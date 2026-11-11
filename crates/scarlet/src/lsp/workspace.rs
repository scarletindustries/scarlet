//! The LSP server's workspace state and analysis: every open or indexed buffer, and one
//! [`Build`] per root. Each file is checked as its own module under its real key, so a file's
//! definitions have the same identity whether it is open or only imported. It answers position
//! queries and computes diagnostics, but never touches stdio. The transport layer in `mod.rs`
//! does that.
//!
//! A document's root and module key are worked out from its URI whenever they are needed, never
//! stored, so they can't fall out of step with the workspace folders.

use std::cell::OnceCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::Value as Json;

use crate::ast;
use crate::build::{self, Analysis, Build, Entry, Origin};
use crate::check::{Role, UnusedBindings};
use crate::diagnostic::{self, Diagnostic};
use crate::issue::Issue;
use crate::module::{self, ModuleKey, ModulePath};
use crate::parser::SyntaxError;
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
    syntax: Vec<Diagnostic<SyntaxError>>,
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

/// A `.sca` file the workspace knows.
enum Doc {
    /// Found by the workspace scan. Its file on disk is the truth.
    Indexed(Document),
    /// Open in a tab. Its buffer wins over the file on disk.
    Open(Document),
}

impl Doc {
    fn document(&self) -> &Document {
        match self {
            Doc::Indexed(d) | Doc::Open(d) => d,
        }
    }
}

/// Where a check reads module text: an open tab's buffer, then the disk.
struct Editor<'w> {
    documents: &'w HashMap<String, Doc>,
    disk: &'w mut DiskSources,
}

impl Sources for Editor<'_> {
    fn read(&mut self, path: &Path) -> std::io::Result<Text> {
        match self.documents.get(&reference::path_to_uri(path)) {
            Some(Doc::Open(d)) => Ok(d.text.clone()),
            Some(Doc::Indexed(_)) | None => self.disk.read(path),
        }
    }
}

/// Each document's latest analysis, and the reference graph made from them. The graph is made
/// when first asked for, and every change to the analyses goes through here and drops it, so it
/// is never older than they are.
#[derive(Default)]
struct Analyses {
    by_uri: HashMap<String, Analysis>,
    graph: OnceCell<ReferenceGraph>,
}

impl Analyses {
    fn get(&self, uri: &str) -> Option<&Analysis> {
        self.by_uri.get(uri)
    }

    fn insert(&mut self, uri: &str, analysis: Analysis) -> Option<Analysis> {
        self.graph.take();
        self.by_uri.insert(uri.to_string(), analysis)
    }

    fn remove(&mut self, uri: &str) -> Option<Analysis> {
        self.graph.take();
        self.by_uri.remove(uri)
    }

    /// Every analysis with its URI, in URI order, so what is made from them doesn't depend on a
    /// map's order.
    fn sorted(&self) -> Vec<(&String, &Analysis)> {
        let mut all: Vec<(&String, &Analysis)> = self.by_uri.iter().collect();
        all.sort_by(|a, b| a.0.cmp(b.0));
        all
    }

    /// The graph of every document's module, and of what they import. A document's own module
    /// wins over its copy checked as another's import.
    fn graph(&self) -> &ReferenceGraph {
        self.graph.get_or_init(|| {
            let all = self.sorted();
            let entries = all.iter().map(|(_, a)| a.entry());
            let imports = all.iter().flat_map(|(_, a)| a.modules());
            ReferenceGraph::of_modules(entries.chain(imports))
        })
    }
}

/// One workspace root: a build that lives as long as the session, and the analyses of the
/// documents it owns.
pub(super) struct Root {
    build: Build,
    analyses: Analyses,
}

impl Root {
    fn new(stdlib_root: Option<PathBuf>) -> Self {
        Root {
            build: Build::new(stdlib_root),
            analyses: Analyses::default(),
        }
    }

    pub(super) fn graph(&self) -> &ReferenceGraph {
        self.analyses.graph()
    }

    fn analysis(&self, uri: &str) -> Option<&Analysis> {
        self.analyses.get(uri)
    }

    /// Check the document `uri` as `entry`.
    fn check(&mut self, uri: &str, sources: &mut dyn Sources, entry: Entry<'_>) {
        let spec = entry.spec();
        let analysis = self.build.check(sources, entry);
        // A document checked as something else before leaves that check behind.
        if let Some(old) = self.analyses.insert(uri, analysis)
            && old.entry().spec != spec
        {
            self.build.forget(&old.entry().spec);
        }
    }

    /// Forget the document `uri`, and every module only it reached.
    fn remove(&mut self, uri: &str) {
        if let Some(old) = self.analyses.remove(uri) {
            self.build.forget(&old.entry().spec);
        }
    }

    /// The documents whose analysis reaches module `key`.
    fn dependents(&self, key: &ModuleKey) -> Vec<String> {
        self.analyses
            .sorted()
            .into_iter()
            .filter(|(_, a)| a.module(key).is_some())
            .map(|(u, _)| u.clone())
            .collect()
    }

    /// The file URI of module `key`: the document checked as it, or else the file it was read
    /// from. This is the one place a module becomes a URI.
    fn uri_of(&self, key: &ModuleKey) -> Option<String> {
        let all = self.analyses.sorted();
        if let Some((u, _)) = all.iter().find(|(_, a)| a.entry().key() == key) {
            return Some((*u).clone());
        }
        all.iter().find_map(|(_, a)| match &a.module(key)?.origin {
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
    /// Every known `.sca` file, keyed by URI: the open tabs and the whole-workspace scan.
    documents: HashMap<String, Doc>,
    /// Roots reported by the client, used to bucket files into builds.
    workspace_roots: Vec<PathBuf>,
    /// One [`Root`] per workspace root, keyed by the matching entry in `workspace_roots`, or an
    /// empty path for files outside any root.
    roots: HashMap<PathBuf, Root>,
    /// Set once the one-time recursive workspace scan has run.
    scanned: bool,
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
            disk: DiskSources::new(),
        }
    }

    /// Registers a workspace root, as the client's `initialize` would.
    pub fn add_workspace_root(&mut self, root: PathBuf) {
        self.workspace_roots.push(root);
    }

    /// The latest analysis of `uri`.
    pub fn analysis_of(&self, uri: &str) -> Option<&Analysis> {
        self.roots.get(&self.root_dir_of(uri))?.analysis(uri)
    }

    /// Whether anything has been checked yet.
    pub(super) fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    /// Handles `textDocument/didOpen` and `didChange`. It opens `uri` as a client tab, checks it
    /// and every document that imports it, and returns the diagnostics to publish.
    pub fn open_document(&mut self, uri: &str, text: &str) -> Publications {
        self.documents
            .insert(uri.to_string(), Doc::Open(Document::new(text.to_string())));
        self.ensure_workspace_scanned();
        self.check_with_dependents(uri)
    }

    /// Handles `textDocument/didClose`. The file on disk takes over from the buffer. If the
    /// file is gone, so is the document.
    pub fn close_document(&mut self, uri: &str) -> Publications {
        match uri_to_path(uri).as_deref().map(std::fs::read_to_string) {
            Some(Ok(text)) => {
                self.documents
                    .insert(uri.to_string(), Doc::Indexed(Document::new(text)));
                self.check_with_dependents(uri)
            }
            _ => self.remove_document(uri),
        }
    }

    /// Handles `workspace/didChangeWatchedFiles`. An open buffer wins over its file, so only
    /// closed files are read again. Then every document is checked again, since any of them may
    /// import a changed file. The build compares each source with what it last checked, so only
    /// what changed is checked again.
    pub fn files_changed(&mut self, changes: &[(String, WatchedChange)]) -> Publications {
        let mut out = Merged::default();
        let mut any = false;
        for (uri, ty) in changes {
            let Some(path) = uri_to_path(uri) else {
                continue;
            };
            if matches!(self.documents.get(uri), Some(Doc::Open(_))) {
                continue;
            }
            any = true;
            self.disk.changed(&path);
            match ty {
                WatchedChange::Deleted => {
                    if self.documents.remove(uri).is_some() {
                        let dir = self.root_dir_of(uri);
                        if let Some(root) = self.roots.get_mut(&dir) {
                            root.remove(uri);
                        }
                    }
                    out.add(vec![(uri.clone(), Vec::new())]);
                }
                WatchedChange::Touched => {
                    let known = self.documents.contains_key(uri) || self.is_scanned_file(&path);
                    if known && let Ok(text) = std::fs::read_to_string(&path) {
                        self.documents
                            .insert(uri.clone(), Doc::Indexed(Document::new(text)));
                    }
                }
            }
        }
        if any {
            out.add(self.recheck_all());
        }
        out.into_vec()
    }

    /// Handles `workspace/didChangeWorkspaceFolders`. Every document then belongs to the root
    /// its path says, so a document whose owner changed moves: the old root forgets it and the
    /// new one checks it. A scanned file that is in no folder any more is dropped.
    pub fn change_folders(&mut self, added: &[PathBuf], removed: &[PathBuf]) -> Publications {
        let before: HashMap<String, PathBuf> = self
            .documents
            .keys()
            .map(|u| (u.clone(), self.root_dir_of(u)))
            .collect();
        self.workspace_roots.retain(|r| !removed.contains(r));
        self.workspace_roots.extend(added.iter().cloned());
        for r in removed {
            self.roots.remove(r);
        }
        let mut out = Merged::default();
        let mut moved = Vec::new();
        let mut uris: Vec<&String> = before.keys().collect();
        uris.sort();
        for uri in uris {
            let old = &before[uri];
            if self.root_dir_of(uri) == *old && self.roots.contains_key(old) {
                continue;
            }
            if let Some(root) = self.roots.get_mut(old) {
                root.remove(uri);
            }
            let dropped = match (self.documents.get(uri), uri_to_path(uri)) {
                (Some(Doc::Indexed(_)), Some(p)) => !self.is_scanned_file(&p),
                (Some(Doc::Indexed(_)), None) => true,
                (Some(Doc::Open(_)) | None, _) => false,
            };
            if dropped {
                self.documents.remove(uri);
                out.add(vec![(uri.clone(), Vec::new())]);
            } else {
                moved.push(uri.clone());
            }
        }
        if self.scanned {
            for r in added {
                self.index_root(r);
            }
        }
        for uri in &moved {
            out.add(self.check_with_dependents(uri));
        }
        out.into_vec()
    }

    /// Whether the workspace scan would index `path`: a non-stdlib file inside a workspace root.
    fn is_scanned_file(&self, path: &Path) -> bool {
        self.workspace_roots.iter().any(|r| path.starts_with(r))
            && module::detect_stdlib_module(path).is_none()
    }

    /// Forgets the document `uri`, whose file is gone, and checks every document that imported
    /// it again.
    fn remove_document(&mut self, uri: &str) -> Publications {
        self.documents.remove(uri);
        let dir = self.root_dir_of(uri);
        let key = self.roots.get_mut(&dir).and_then(|root| {
            let key = root.analysis(uri).map(|a| a.entry().key().clone());
            root.remove(uri);
            key
        });
        let mut out = Merged::default();
        out.add(vec![(uri.to_string(), Vec::new())]);
        if let Some(key) = key {
            out.add(self.check_dependents(&key, uri));
        }
        out.into_vec()
    }

    /// Checks every document again.
    fn recheck_all(&mut self) -> Publications {
        let mut dirs: Vec<PathBuf> = self.roots.keys().cloned().collect();
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

    /// The root and module a position query on `uri` works on, or `None` when the document
    /// hasn't been checked.
    pub(super) fn query(&self, uri: &str) -> Option<Query<'_>> {
        let root = self.roots.get(&self.root_dir_of(uri))?;
        let analysis = root.analysis(uri)?;
        let graph = root.graph();
        let module = graph.module_id_by_key(analysis.entry().key())?;
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
        self.documents.get(uri).map(|d| &*d.document().text.text)
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
    fn index_root(&mut self, root: &Path) {
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
            self.documents
                .insert(uri.clone(), Doc::Indexed(Document::new(text)));
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
        let mut out = Merged::default();
        out.add(self.publications(&own, &[uri.to_string()]));
        out.add(self.check_dependents(&key, uri));
        out.into_vec()
    }

    /// Checks every document but `except` whose analysis reaches module `key`, in any root.
    fn check_dependents(&mut self, key: &ModuleKey, except: &str) -> Publications {
        let mut dirs: Vec<PathBuf> = self.roots.keys().cloned().collect();
        dirs.sort();
        let mut out = Merged::default();
        for dir in dirs {
            let checked: Vec<String> = match self.roots.get(&dir) {
                Some(root) => root
                    .dependents(key)
                    .into_iter()
                    .filter(|u| u != except)
                    .collect(),
                None => Vec::new(),
            };
            for d in &checked {
                self.check_document(d);
            }
            out.add(self.publications(&dir, &checked));
        }
        out.into_vec()
    }

    /// Checks the document `uri` as its own module and returns that module's key.
    fn check_document(&mut self, uri: &str) -> Option<ModuleKey> {
        eprintln!("[Scarlet LSP] Checking: {uri}");
        let text = self.documents.get(uri)?.document().text.text.clone();
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
        let (dir, stdlib_root) = path
            .as_deref()
            .map(|p| owning_root(&self.workspace_roots, p))
            .unwrap_or_default();
        let root = self
            .roots
            .entry(dir)
            .or_insert_with(|| Root::new(stdlib_root));
        let mut sources = Editor {
            documents: &self.documents,
            disk: &mut self.disk,
        };
        root.check(uri, &mut sources, entry);
        Some(key)
    }

    /// The diagnostics of every document the modules `checked` reach, each on the URI of the
    /// module it is about. Only the root that owns a URI publishes it. A document with a syntax
    /// error shows only its syntax errors, because a partial tree gives noise for the region
    /// still being typed.
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
        let mut by_module: HashMap<ModuleKey, Vec<Diagnostic<Issue>>> = HashMap::new();
        for m in &reach {
            for d in m.diagnostics() {
                let key = d.source.as_ref().map_or(m.key(), |s| &s.key).clone();
                by_module.entry(key).or_default().push(d);
            }
        }
        let mut out = Vec::new();
        for m in &reach {
            let k = m.key();
            let Some(uri) = root.uri_of(k) else {
                continue;
            };
            if self.root_dir_of(&uri) != dir {
                continue;
            }
            let Some(doc) = self.documents.get(&uri).map(Doc::document) else {
                continue;
            };
            let diags: Vec<Json> = if diagnostic::has_errors(&doc.syntax) {
                doc.syntax.iter().map(diagnostic_to_json).collect()
            } else {
                let unused = root
                    .graph()
                    .unused_diagnostics_for(&path_of(k))
                    .into_iter()
                    .map(|d| d.map(Issue::Unused));
                let own = by_module.get(k).into_iter().flatten().cloned();
                own.chain(unused).map(|d| diagnostic_to_json(&d)).collect()
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
