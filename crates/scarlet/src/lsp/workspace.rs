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
use crate::build::{self, Analysis, Build, Entry, Origin};
use crate::check::{Role, UnusedBindings};
use crate::diagnostic::{self, Diagnostic};
use crate::module::{self, ModuleKey, ModulePath};
use crate::reference::{self, ModuleId, ReferenceGraph};
use crate::source::SourceFile;
use crate::sources::{DiskSources, Sources, Text};

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
    text: Text,
    syntax: Vec<Diagnostic>,
}

impl Document {
    fn new(text: String) -> Self {
        let syntax = build::parse(&text).diagnostics;
        Document {
            text: Text::new(text),
            syntax,
        }
    }
}

/// Where a check reads module text: an open tab's buffer, then the disk.
struct Editor<'w> {
    documents: &'w HashMap<String, Document>,
    open: &'w HashSet<String>,
    disk: &'w mut DiskSources,
}

impl Sources for Editor<'_> {
    fn read(&mut self, path: &Path) -> std::io::Result<Text> {
        let uri = reference::path_to_uri(path);
        match self.documents.get(&uri) {
            Some(d) if self.open.contains(&uri) => Ok(d.text.clone()),
            _ => self.disk.read(path),
        }
    }
}

/// One workspace root: a build that lives as long as the session, and the reference graph of
/// everything in it, made again only after a check.
pub(super) struct Root {
    build: Build,
    /// Each document's latest analysis. Its entry is the document's module.
    analyses: HashMap<String, Analysis>,
    /// The client's URI for each module checked from a document.
    uris: HashMap<ModuleKey, String>,
    graph: OnceCell<ReferenceGraph>,
}

impl Root {
    fn new(stdlib_root: Option<PathBuf>) -> Self {
        Root {
            build: Build::new(stdlib_root),
            analyses: HashMap::new(),
            uris: HashMap::new(),
            graph: OnceCell::new(),
        }
    }

    /// The analyses in URI order, so what is made from them doesn't depend
    /// on a map's order.
    fn analyses(&self) -> Vec<&Analysis> {
        let mut uris: Vec<&String> = self.analyses.keys().collect();
        uris.sort();
        uris.into_iter().map(|u| &self.analyses[u]).collect()
    }

    /// The graph of every document's module, and of what they import. A
    /// document's own module wins over its copy checked as another's import.
    pub(super) fn graph(&self) -> &ReferenceGraph {
        self.graph.get_or_init(|| {
            let analyses = self.analyses();
            let entries = analyses.iter().map(|a| a.entry());
            let imports = analyses.iter().flat_map(|a| a.modules());
            ReferenceGraph::of_modules(entries.chain(imports))
        })
    }

    fn analysis(&self, uri: &str) -> Option<&Analysis> {
        self.analyses.get(uri)
    }

    /// The file URI of module `key`, when it has a file.
    fn uri_of(&self, key: &ModuleKey) -> Option<String> {
        if let Some(u) = self.uris.get(key) {
            return Some(u.clone());
        }
        self.analyses()
            .into_iter()
            .find_map(|a| match &a.module(key)?.origin {
                Origin::File(p) => Some(reference::path_to_uri(p)),
                Origin::Embedded | Origin::Given => None,
            })
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
    /// The document's latest analysis.
    pub(super) analysis: &'a Analysis,
    pub(super) graph: &'a ReferenceGraph,
    pub(super) module: ModuleId,
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
    disk: DiskSources,
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
            disk: DiskSources::new(),
        }
    }

    /// Registers a workspace root, as the client's `initialize` would. Tests use this.
    pub fn add_workspace_root(&mut self, root: PathBuf) {
        self.workspace_roots.push(root);
    }

    /// The latest analysis of `uri`.
    pub fn analysis_of(&self, uri: &str) -> Option<&Analysis> {
        self.roots.get(&self.root_dir_of(uri))?.analysis(uri)
    }

    /// Handles `textDocument/didOpen` and `didChange`. It opens `uri` as a client tab, checks it
    /// and every document that imports it, and returns the diagnostics to publish.
    pub fn open_document(&mut self, uri: &str, text: &str) -> Publications {
        self.documents
            .insert(uri.to_string(), Document::new(text.to_string()));
        self.open.insert(uri.to_string());
        self.ensure_workspace_scanned();
        self.check_with_dependents(uri)
    }

    /// Handles `textDocument/didClose`. The file on disk takes over from the buffer. If the
    /// file is gone, the document is forgotten and its root is built again without it.
    pub fn close_document(&mut self, uri: &str) -> Publications {
        self.open.remove(uri);
        match uri_to_path(uri).as_deref().map(std::fs::read_to_string) {
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
            self.disk.changed(&path);
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

    /// Throws away the build of root `dir` and checks every document in it again, so a deleted
    /// file leaves the graph.
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

    /// The directory of the root that owns `uri`, and that root, made if it isn't yet.
    fn root_entry(&mut self, uri: &str) -> PathBuf {
        let (dir, stdlib_root) = uri_to_path(uri)
            .map(|p| owning_root(&self.workspace_roots, &p))
            .unwrap_or_default();
        self.roots
            .entry(dir.clone())
            .or_insert_with(|| Root::new(stdlib_root));
        dir
    }

    /// The root and module a position query on `uri` works on, or `None` when the document
    /// hasn't been checked.
    pub(super) fn query(&self, uri: &str) -> Option<Query<'_>> {
        let root = self.roots.get(&self.root_dir_of(uri))?;
        let analysis = root.analysis(uri)?;
        let key = analysis.entry().key();
        let graph = root.graph();
        let module = graph.module_id_by_key(key)?;
        Some(Query {
            root,
            analysis,
            graph,
            module,
        })
    }

    /// Every root, for queries over the whole workspace.
    pub(super) fn all_roots(&self) -> impl Iterator<Item = &Root> {
        self.roots.values()
    }

    pub(super) fn document_text(&self, uri: &str) -> Option<&str> {
        self.documents.get(uri).map(|d| &*d.text.text)
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
                    .analyses
                    .iter()
                    .filter(|(u, a)| u.as_str() != uri && a.module(&key).is_some())
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
        let text = self.documents.get(uri)?.text.text.clone();
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
        let entry = Entry {
            source: SourceFile::new(key.clone(), path.clone(), text),
            ast: &parsed.ast,
            doc: parsed.doc,
            parse: parsed.diagnostics,
            base_dir: path.as_deref().and_then(Path::parent),
            role,
            unused: UnusedBindings::Report,
        };
        let spec = entry.spec();
        let dir = self.root_entry(uri);
        let mut sources = Editor {
            documents: &self.documents,
            open: &self.open,
            disk: &mut self.disk,
        };
        let root = self.roots.get_mut(&dir)?;
        // A document checked as something else before leaves that check behind.
        if let Some(old) = root.analyses.get(uri).map(|a| a.entry().spec.clone())
            && old != spec
        {
            root.build.forget(&old);
        }
        let analysis = root.build.check(&mut sources, entry);
        root.analyses.insert(uri.to_string(), analysis);
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
        let mut reach: Vec<&build::Module> = Vec::new();
        let mut seen: HashSet<&ModuleKey> = HashSet::new();
        for a in checked.iter().filter_map(|u| root.analysis(u)) {
            for m in a.modules().rev() {
                if seen.insert(m.key()) {
                    reach.push(m);
                }
            }
        }
        let mut by_module: HashMap<&ModuleKey, Vec<&Diagnostic>> = HashMap::new();
        for m in &reach {
            for d in m.diagnostics() {
                by_module
                    .entry(d.source.as_ref().map_or(m.key(), |s| &s.key))
                    .or_default()
                    .push(d);
            }
        }
        let mut out = Vec::new();
        for m in &reach {
            let k = m.key();
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
