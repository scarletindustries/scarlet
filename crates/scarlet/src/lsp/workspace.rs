//! The LSP server's workspace state and analysis: every open or indexed buffer, and one
//! [`Build`] per root. Each file is checked as its own module under its real key, so a file's
//! definitions have the same identity whether it is open or only imported. It answers position
//! queries and computes diagnostics, but never touches stdio. The transport layer in `mod.rs`
//! does that.

use std::cell::OnceCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::Value as Json;

use crate::ast;
use crate::build::{self, Build, Entry, Origin};
use crate::check::{Role, UnusedBindings};
use crate::diagnostic::{self, Diagnostic};
use crate::module::{self, ModuleKey, ModulePath};
use crate::reference::{self, ModuleId, ReferenceGraph};

use super::wire::{
    DocPosition, WatchedChange, diagnostic_to_json, extract_position_params, root_for, stdlib_file,
    uri_to_path,
};

/// The [`Root`] key that owns `path`, and the stdlib source directory when that root is the
/// in-repo stdlib itself. Stdlib sources go under their own root (the `src/std` directory),
/// whose build reads `scarlet/...` modules from disk. Everything else goes under the longest
/// matching workspace root.
fn owning_root(workspace_roots: &[PathBuf], path: &Path) -> (PathBuf, Option<PathBuf>) {
    if module::detect_stdlib_module(path).is_some()
        && let Some(std_root) = module::find_stdlib_root(path)
    {
        return (std_root.clone(), Some(std_root));
    }
    (root_for(workspace_roots, path), None)
}

/// A buffer's text and what the parser said about it.
struct Document {
    text: String,
    syntax: Vec<Diagnostic>,
}

impl Document {
    fn new(text: String) -> Self {
        let syntax = build::parse(&text).diagnostics;
        Document { text, syntax }
    }
}

/// One workspace root: a build that lives as long as the session, and the reference graph of
/// everything in it, made again only after a check.
pub(super) struct Root {
    pub(super) build: Build,
    /// The module each document was checked as.
    keys: HashMap<String, ModuleKey>,
    /// The client's URI for each module checked from a document.
    uris: HashMap<ModuleKey, String>,
    graph: OnceCell<ReferenceGraph>,
}

impl Root {
    fn new(stdlib_root: Option<PathBuf>) -> Self {
        Root {
            build: Build::new(stdlib_root),
            keys: HashMap::new(),
            uris: HashMap::new(),
            graph: OnceCell::new(),
        }
    }

    pub(super) fn graph(&self) -> &ReferenceGraph {
        self.graph
            .get_or_init(|| ReferenceGraph::of_build(&self.build))
    }

    fn key_of(&self, uri: &str) -> Option<&ModuleKey> {
        self.keys.get(uri)
    }

    /// The file URI of module `key`, when it has a file.
    fn uri_of(&self, key: &ModuleKey) -> Option<String> {
        if let Some(u) = self.uris.get(key) {
            return Some(u.clone());
        }
        match &self.build.module(key)?.origin {
            Origin::File(p) => Some(reference::path_to_uri(p)),
            Origin::Embedded | Origin::Given => None,
        }
    }

    /// The file URI of graph module `id`. An embedded stdlib module opens its source when the
    /// stdlib tree is on disk near `near`, and has no URI otherwise.
    pub(super) fn uri_for(&self, id: ModuleId, near: Option<&Path>) -> Option<String> {
        let path = self.graph().module_path(id)?;
        if let Some(u) = self.uri_of(&ModuleKey::of(path)) {
            return Some(u);
        }
        if module::is_stdlib(path) {
            let p = stdlib_file(path, near?)?;
            return Some(reference::path_to_uri(&p));
        }
        None
    }
}

/// What a position query works on: the document's root and its module in the root's graph.
pub(super) struct Query<'a> {
    pub(super) root: &'a Root,
    pub(super) graph: &'a ReferenceGraph,
    pub(super) module: ModuleId,
    pub(super) key: &'a ModuleKey,
}

pub struct Workspace {
    /// Every known `.sca` buffer, keyed by URI: the open tabs and the whole-workspace scan.
    documents: HashMap<String, Document>,
    /// Roots reported by the client, used to bucket files into builds.
    pub(super) workspace_roots: Vec<PathBuf>,
    /// One [`Root`] per workspace root, keyed by the matching entry in `workspace_roots`, or an
    /// empty path for files outside any root.
    pub(super) roots: HashMap<PathBuf, Root>,
    /// Set once the one-time recursive workspace scan has run.
    pub(super) scanned: bool,
    /// URIs the client has open in a tab, a subset of `documents`. Their text wins over the file
    /// on disk.
    open: HashSet<String>,
}

/// Diagnostics to publish, as `(uri, diagnostics)` pairs.
pub type Publications = Vec<(String, Vec<Json>)>;

impl Workspace {
    pub fn new() -> Self {
        Self {
            documents: HashMap::new(),
            workspace_roots: Vec::new(),
            roots: HashMap::new(),
            scanned: false,
            open: HashSet::new(),
        }
    }

    /// Registers a workspace root, as the client's `initialize` would. Tests use this.
    pub fn add_workspace_root(&mut self, root: PathBuf) {
        self.workspace_roots.push(root);
    }

    /// The build that owns `uri`.
    pub fn build_for(&self, uri: &str) -> Option<&Build> {
        Some(&self.roots.get(&self.root_dir_of(uri))?.build)
    }

    /// Handles `textDocument/didOpen` and `didChange`. It opens `uri` as a client tab, checks it
    /// and every document that imports it, and returns the diagnostics to publish.
    pub fn open_document(&mut self, uri: &str, text: &str) -> Publications {
        self.documents
            .insert(uri.to_string(), Document::new(text.to_string()));
        self.open.insert(uri.to_string());
        self.root_mut(uri);
        // Every root gets the buffer, because a file in one root can import a file in another.
        if let Some(p) = uri_to_path(uri) {
            for root in self.roots.values_mut() {
                root.build.set_overlay(p.clone(), text.to_string());
            }
        }
        self.ensure_workspace_scanned();
        self.check_with_dependents(uri)
    }

    /// Handles `textDocument/didClose`. The file on disk takes over from the buffer. If the
    /// file is gone, the document is forgotten and its root is built again without it.
    pub fn close_document(&mut self, uri: &str) -> Publications {
        self.open.remove(uri);
        let path = uri_to_path(uri);
        if let Some(p) = &path {
            for root in self.roots.values_mut() {
                root.build.clear_overlay(p);
            }
        }
        match path.as_deref().map(std::fs::read_to_string) {
            Some(Ok(text)) => {
                self.documents.insert(uri.to_string(), Document::new(text));
                self.check_with_dependents(uri)
            }
            _ => {
                self.documents.remove(uri);
                let dir = self.root_dir_of(uri);
                let mut out = Merged::default();
                out.add(vec![(uri.to_string(), Vec::new())]);
                out.add(self.rebuild_root(&dir));
                out.add(self.recheck_roots_except(&HashSet::from([dir])));
                out.into_vec()
            }
        }
    }

    /// Handles `workspace/didChangeWatchedFiles`. An open buffer wins over its file, so only
    /// closed files are read again. A deleted file's root is built again without it, and every
    /// other root re-checks its documents, since any of them may import the file. The build
    /// compares each source with what it last checked, so only what changed is checked again.
    pub fn files_changed(&mut self, changes: &[(String, WatchedChange)]) -> Publications {
        let mut rebuild: HashSet<PathBuf> = HashSet::new();
        let mut any = false;
        for (uri, ty) in changes {
            let Some(path) = uri_to_path(uri) else {
                continue;
            };
            if self.open.contains(uri) {
                continue;
            }
            any = true;
            match ty {
                WatchedChange::Deleted => {
                    self.documents.remove(uri);
                    rebuild.insert(owning_root(&self.workspace_roots, &path).0);
                }
                WatchedChange::Touched => {
                    let known = self.documents.contains_key(uri) || self.is_scanned_file(&path);
                    if known && let Ok(text) = std::fs::read_to_string(&path) {
                        self.documents.insert(uri.clone(), Document::new(text));
                    }
                }
            }
        }
        let mut out = Merged::default();
        for dir in &rebuild {
            out.add(self.rebuild_root(dir));
        }
        if any {
            out.add(self.recheck_roots_except(&rebuild));
        }
        out.into_vec()
    }

    /// Whether the workspace scan would index `path`: a non-stdlib file inside a workspace root.
    fn is_scanned_file(&self, path: &Path) -> bool {
        self.workspace_roots.iter().any(|r| path.starts_with(r))
            && module::detect_stdlib_module(path).is_none()
    }

    /// Throws away the build of root `dir` and checks every document in it again. A build never
    /// forgets a module, so this is how a deleted file leaves the graph.
    fn rebuild_root(&mut self, dir: &Path) -> Publications {
        self.roots.remove(dir);
        let uris = self.documents_in(dir);
        for u in &uris {
            self.check_document(u);
        }
        self.publications(dir, &uris)
    }

    /// Checks every document of every root but those in `except` again.
    fn recheck_roots_except(&mut self, except: &HashSet<PathBuf>) -> Publications {
        let mut dirs: Vec<PathBuf> = self
            .roots
            .keys()
            .filter(|d| !except.contains(*d))
            .cloned()
            .collect();
        dirs.sort();
        let mut out = Merged::default();
        for dir in dirs {
            let uris = self.documents_in(&dir);
            for u in &uris {
                self.check_document(u);
            }
            out.add(self.publications(&dir, &uris));
        }
        out.into_vec()
    }

    fn documents_in(&self, dir: &Path) -> Vec<String> {
        let mut uris: Vec<String> = self
            .documents
            .keys()
            .filter(|u| self.root_dir_of(u) == dir)
            .cloned()
            .collect();
        uris.sort();
        uris
    }

    fn root_dir_of(&self, uri: &str) -> PathBuf {
        uri_to_path(uri)
            .map(|p| owning_root(&self.workspace_roots, &p).0)
            .unwrap_or_default()
    }

    /// The root that owns `uri`. A new root starts with every open buffer as an overlay.
    fn root_mut(&mut self, uri: &str) -> &mut Root {
        let (dir, stdlib_root) = uri_to_path(uri)
            .map(|p| owning_root(&self.workspace_roots, &p))
            .unwrap_or_default();
        match self.roots.entry(dir) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                let mut root = Root::new(stdlib_root);
                for u in &self.open {
                    if let (Some(p), Some(d)) = (uri_to_path(u), self.documents.get(u)) {
                        root.build.set_overlay(p, d.text.clone());
                    }
                }
                e.insert(root)
            }
        }
    }

    /// The root and module a position query on `uri` works on, or `None` when the document
    /// hasn't been checked.
    pub(super) fn query(&self, uri: &str) -> Option<Query<'_>> {
        let root = self.roots.get(&self.root_dir_of(uri))?;
        let key = root.key_of(uri)?;
        let graph = root.graph();
        let module = graph.module_id_by_key(key)?;
        Some(Query {
            root,
            graph,
            module,
            key,
        })
    }

    /// Every root, for queries over the whole workspace.
    pub(super) fn all_roots(&self) -> impl Iterator<Item = &Root> {
        self.roots.values()
    }

    pub(super) fn document_text(&self, uri: &str) -> Option<&str> {
        self.documents.get(uri).map(|d| d.text.as_str())
    }

    /// The first step every position query handler shares.
    pub(super) fn resolve_pos(&self, params: &Json) -> Option<DocPosition> {
        let pos = extract_position_params(params)?;
        self.query(&pos.uri).is_some().then_some(pos)
    }

    /// Scans every workspace root for `.sca` files, once. Without this, a file outside the open
    /// file's import closure is never checked and workspace-wide queries never see its
    /// references.
    fn ensure_workspace_scanned(&mut self) {
        if self.scanned {
            return;
        }
        self.scanned = true;
        let roots = self.workspace_roots.clone();
        for root in &roots {
            self.index_root(root);
        }
    }

    /// Indexes every non-stdlib `.sca` file under `root` into `documents` and checks it. It's
    /// idempotent, so it's safe to call again for a folder added after the first scan.
    pub(super) fn index_root(&mut self, root: &Path) {
        let mut files = Vec::new();
        module::collect_source_files(root, &mut files);
        files.sort();
        for path in files {
            if module::detect_stdlib_module(&path).is_some() {
                continue;
            }
            let uri = reference::path_to_uri(&path);
            if self.documents.contains_key(&uri) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            self.documents.insert(uri.clone(), Document::new(text));
            self.check_document(&uri);
        }
    }

    /// Checks `uri`, then every other document that imports it, directly or not and in any
    /// root, and returns the diagnostics of every document those checks reach.
    fn check_with_dependents(&mut self, uri: &str) -> Publications {
        let Some(key) = self.check_document(uri) else {
            return Vec::new();
        };
        let own = self.root_dir_of(uri);
        let mut dirs: Vec<PathBuf> = self.roots.keys().filter(|d| **d != own).cloned().collect();
        dirs.sort();
        dirs.insert(0, own.clone());
        let mut out = Merged::default();
        for dir in dirs {
            let mut checked: Vec<String> = match self.roots.get(&dir) {
                Some(root) => root
                    .keys
                    .iter()
                    .filter(|(u, k)| u.as_str() != uri && root.build.reach(k).contains(&key))
                    .map(|(u, _)| u.clone())
                    .collect(),
                None => Vec::new(),
            };
            checked.sort();
            for d in &checked {
                self.check_document(d);
            }
            if dir == own {
                checked.insert(0, uri.to_string());
            }
            out.add(self.publications(&dir, &checked));
        }
        out.into_vec()
    }

    /// Checks the document `uri` as its own module and returns that module's key.
    fn check_document(&mut self, uri: &str) -> Option<ModuleKey> {
        eprintln!("[Scarlet LSP] Checking: {uri}");
        let text = self.documents.get(uri)?.text.clone();
        let path = uri_to_path(uri);
        let stdlib_module = path.as_deref().and_then(module::detect_stdlib_module);
        let key = match (&stdlib_module, &path) {
            (Some(m), _) => ModuleKey::for_stdlib(m),
            (None, Some(p)) => module::file_module_path(p)
                .map(|mp| ModuleKey::of(&mp))
                .unwrap_or_else(|_| ModuleKey::main()),
            (None, None) => ModuleKey::main(),
        };
        // The recovering parser drops only the malformed statements, so a buffer with a syntax
        // error is still checked, and position queries keep working mid-edit.
        let parsed = build::parse(&text);
        let role = match stdlib_module {
            Some(_) => Role::Library,
            None => role_of(&parsed.ast),
        };
        let root = self.root_mut(uri);
        root.build.check_entry(Entry {
            key: key.clone(),
            ast: &parsed.ast,
            doc: parsed.doc,
            parse: parsed.diagnostics,
            hash: build::source_hash(&text),
            base_dir: path.as_deref().and_then(Path::parent),
            role,
            unused: UnusedBindings::Report,
        });
        root.keys.insert(uri.to_string(), key.clone());
        root.uris.insert(key.clone(), uri.to_string());
        root.graph = OnceCell::new();
        Some(key)
    }

    /// The diagnostics of every document the modules `checked` reach, each on the URI of the
    /// module it is about. A document with a syntax error shows only its syntax errors, because
    /// a partial tree gives noise for the region still being typed.
    fn publications(&self, dir: &Path, checked: &[String]) -> Publications {
        let Some(root) = self.roots.get(dir) else {
            return Vec::new();
        };
        let mut reach: Vec<ModuleKey> = Vec::new();
        let mut seen: HashSet<ModuleKey> = HashSet::new();
        for k in checked.iter().filter_map(|u| root.key_of(u)) {
            for r in root.build.reach(k).into_iter().rev() {
                if seen.insert(r.clone()) {
                    reach.push(r);
                }
            }
        }
        let mut by_module: HashMap<&ModuleKey, Vec<&Diagnostic>> = HashMap::new();
        for k in &reach {
            let Some(m) = root.build.module(k) else {
                continue;
            };
            for d in m.diagnostics() {
                by_module
                    .entry(d.source.as_ref().unwrap_or(k))
                    .or_default()
                    .push(d);
            }
        }
        let mut out = Vec::new();
        for k in &reach {
            let Some(uri) = root.uri_of(k) else {
                continue;
            };
            let Some(doc) = self.documents.get(&uri) else {
                continue;
            };
            let diags: Vec<Json> = if diagnostic::has_errors(&doc.syntax) {
                doc.syntax.iter().map(diagnostic_to_json).collect()
            } else {
                let unused = root.graph().unused_diagnostics_for(&path_of(k));
                by_module
                    .get(k)
                    .into_iter()
                    .flatten()
                    .copied()
                    .chain(&unused)
                    .map(diagnostic_to_json)
                    .collect()
            };
            out.push((uri, diags));
        }
        out
    }
}

/// Publications from several roots, each URI once. The first root to publish a URI wins, and
/// callers ask the root that owns a file first.
#[derive(Default)]
struct Merged {
    seen: HashSet<String>,
    out: Publications,
}

impl Merged {
    fn add(&mut self, publications: Publications) {
        for (uri, diags) in publications {
            if self.seen.insert(uri.clone()) {
                self.out.push((uri, diags));
            }
        }
    }

    fn into_vec(self) -> Publications {
        self.out
    }
}

impl Default for Workspace {
    fn default() -> Self {
        Self::new()
    }
}

/// The module path a graph interns for `key`.
fn path_of(key: &ModuleKey) -> ModulePath {
    key.as_str().split('/').map(str::to_string).collect()
}

/// What an open file is checked as. A file that declares `fn main` or runs code at module scope
/// is a program, and anything else is a module some other file imports. A program gets the
/// error that says to put its code inside `pub fn main()`.
fn role_of(block: &ast::BlockExpression) -> Role {
    let program = block.body.iter().any(|n| match n {
        ast::Node::Expression(_) => true,
        ast::Node::Statement(s) => match s.as_ref() {
            ast::Statement::Declaration { decl, .. } => {
                matches!(decl.as_ref(), ast::Declaration::Function(f) if f.identifier.name == "main")
            }
            ast::Statement::ImportDeclaration(_) => false,
            ast::Statement::Binding(_) | ast::Statement::Backpass(_) => true,
        },
    });
    if program { Role::Entry } else { Role::Library }
}
