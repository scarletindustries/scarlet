//! The LSP server's workspace state and analysis: every open/indexed buffer,
//! the per-root incremental sessions, and the cross-module reverse-edge index.
//! Answers position queries and computes diagnostics but never touches stdio;
//! the transport layer in `mod.rs` does that.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::Value as Json;

use crate::ast;
use crate::bytecode;
use crate::diagnostic;
use crate::module::{self, ModulePath};
use crate::parser;
use crate::reference;
use crate::scanner;

use super::wire::{
    FileChangeType, diagnostic_to_json, extract_position_params, query_module, root_for,
    uri_to_path,
};
use super::xrefs::{RootState, Xref};

pub struct Workspace {
    /// Every known `.al` buffer's text, keyed by URI. Text only: the session's
    /// reference graph answers hover / goto-def / find-refs / rename.
    pub(super) documents: HashMap<String, String>,
    /// Roots reported by the client, used to bucket files into sessions.
    pub(super) workspace_roots: Vec<PathBuf>,
    /// One [`RootState`] per workspace root, keyed by the matching entry in
    /// `workspace_roots`, or an empty path for files outside any root.
    pub(super) roots: HashMap<PathBuf, RootState>,
    /// Latched once the one-time recursive workspace scan has run.
    pub(super) scanned: bool,
    /// URI of the document last analysed as the compilation entry. A query for
    /// a different document re-analyses it first, so its module is keyed as the
    /// entry (`main`) in its session's graph.
    pub(super) entry_uri: Option<String>,
    /// URIs the client has open in a tab, a subset of `documents` (which also
    /// holds the whole-workspace scan). Only these get published diagnostics or
    /// get re-analysed on a watched-file change.
    pub(super) open: HashSet<String>,
    /// Diagnostics produced when a position query re-roots the entry, staged
    /// for the transport layer to publish. Without them an importer's Problems
    /// panel goes stale after an in-memory edit to one of its imports.
    pub(super) pending_diagnostics: Vec<(String, Vec<Json>)>,
}

impl Workspace {
    pub fn new() -> Self {
        Self {
            documents: HashMap::new(),
            workspace_roots: Vec::new(),
            roots: HashMap::new(),
            scanned: false,
            entry_uri: None,
            open: HashSet::new(),
            pending_diagnostics: Vec::new(),
        }
    }

    /// Test seam: register a workspace root, as the client's `initialize`
    /// would.
    pub fn add_workspace_root(&mut self, root: PathBuf) {
        self.workspace_roots.push(root);
    }

    /// Body of `textDocument/didOpen`: open `uri` as a client tab, analyse it,
    /// and return the diagnostics to publish.
    pub fn open_document(&mut self, uri: &str, text: &str) -> Vec<Json> {
        self.documents.insert(uri.to_string(), text.to_string());
        self.open.insert(uri.to_string());
        self.analyze_document(uri, text)
    }

    /// Body of `textDocument/didClose`. Re-syncs the index to disk, or forgets
    /// a truly-gone file and purges its persisted reverse edges.
    pub(super) fn close_document(&mut self, uri: &str) {
        self.open.remove(uri);
        let path = uri_to_path(uri);
        if let Some(p) = &path
            && let Ok(text) = std::fs::read_to_string(p)
        {
            self.documents.insert(uri.to_string(), text.clone());
            self.analyze_document(uri, &text);
        } else {
            self.documents.remove(uri);
            if let Some(p) = &path
                && let Some(r) = self.roots.get_mut(&root_for(&self.workspace_roots, p))
            {
                r.xrefs.refresh(uri, Vec::new());
            }
        }
    }

    /// Apply one `workspace/didChangeWatchedFiles` change: invalidate the
    /// owning session's cache, and on deletion drop the file's reverse edges so
    /// find-references stops reporting occurrences in a file that is gone.
    pub(super) fn invalidate_watched(&mut self, uri: &str, ty: FileChangeType) {
        let Some(path) = uri_to_path(uri) else {
            return;
        };
        let root = root_for(&self.workspace_roots, &path);
        if let Some(r) = self.roots.get_mut(&root) {
            r.session.invalidate_path(&path);
            if ty == FileChangeType::Deleted {
                r.xrefs.refresh(uri, Vec::new());
            }
        }
        if ty == FileChangeType::Deleted {
            self.documents.remove(uri);
        }
    }

    /// Re-analyse every client-open document and return its fresh diagnostics.
    pub(super) fn reanalyze_open(&mut self) -> Vec<(String, Vec<Json>)> {
        let open: Vec<(String, String)> = self
            .open
            .iter()
            .filter_map(|uri| self.documents.get(uri).map(|t| (uri.clone(), t.clone())))
            .collect();
        open.into_iter()
            .map(|(uri, text)| {
                let diags = self.analyze_document(&uri, &text);
                (uri, diags)
            })
            .collect()
    }

    /// Make `uri` the document analysed as the compilation entry, then report
    /// whether a graph is available to query.
    pub(super) fn ensure_entry(&mut self, uri: &str) -> bool {
        if self.entry_uri.as_deref() != Some(uri) {
            let text = self.documents.get(uri).cloned();
            if let Some(text) = text {
                let diags = self.analyze_document(uri, &text);
                if self.open.contains(uri) {
                    self.pending_diagnostics.push((uri.to_string(), diags));
                }
            }
        }
        self.entry_uri.as_deref() == Some(uri) && self.graph_for(uri).is_some()
    }

    /// Drain the diagnostics staged by [`ensure_entry`](Self::ensure_entry), to
    /// publish alongside the query response.
    pub fn take_pending_diagnostics(&mut self) -> Vec<(String, Vec<Json>)> {
        std::mem::take(&mut self.pending_diagnostics)
    }

    /// The workspace reference graph held by the session that owns `uri`.
    /// `None` for an in-repo stdlib file: it has no session, so graph features
    /// no-op on it. Goto-def *into* `al/*` from a normal file still works.
    pub(super) fn graph_for(&self, uri: &str) -> Option<&reference::ReferenceGraph> {
        Some(self.session_for(uri)?.reference_graph())
    }

    /// The persistent compiler session owning `uri`, for handlers that need its
    /// type-aware queries rather than just the reference graph. `None` for
    /// in-repo stdlib files, which have no session.
    pub fn session_for(&self, uri: &str) -> Option<&bytecode::IncrementalSession> {
        let path = uri_to_path(uri)?;
        if module::detect_stdlib_module(&path).is_some() {
            return None;
        }
        let root = root_for(&self.workspace_roots, &path);
        self.roots.get(&root).map(|r| &r.session)
    }

    /// The module path the queried file's defs are keyed under. An imported
    /// file resolves to the canonical module its callers reference, so a query
    /// from its own declaration shares the `DefId` their reverse edges target
    /// rather than the `main` entry identity a re-rooted analysis would assign.
    pub(super) fn query_module_path(&self, uri: &str) -> ModulePath {
        if let Some(path) = uri_to_path(uri)
            && module::detect_stdlib_module(&path).is_none()
            && let Some(session) = self.session_for(uri)
            && let Some(mpath) = session.module_path_for_source(&path)
        {
            return mpath.clone();
        }
        query_module(uri).unwrap_or_else(module::main_module)
    }

    /// [`graph_for`](Self::graph_for) plus the interned module id of
    /// [`query_module_path`](Self::query_module_path) in one step.
    pub(super) fn graph_module(
        &self,
        uri: &str,
    ) -> Option<(&reference::ReferenceGraph, reference::ModuleId)> {
        let graph = self.graph_for(uri)?;
        let mid = graph.module_id(&self.query_module_path(uri))?;
        Some((graph, mid))
    }

    /// Shared preamble for the position-based query responders: re-analyses the
    /// document as the entry. `Some` guarantees `graph_for(&uri)` is `Some`,
    /// which the caller re-fetches to avoid a `&mut self` / `&graph` overlap.
    pub(super) fn resolve_pos(&mut self, params: &Json) -> Option<(String, i32, i32)> {
        let (uri, line, col) = extract_position_params(params)?;
        self.ensure_entry(&uri).then_some((uri, line, col))
    }

    /// Dependent-file callers of `def`, persisted across re-rooting because an
    /// importer is never inside its imports' closure. Only reference sites:
    /// real uses and selective-import `{item}` tokens, which rename rewrites.
    pub(super) fn dependent_callers(
        &self,
        uri: &str,
        def: reference::DefId,
    ) -> impl Iterator<Item = &Xref> {
        uri_to_path(uri)
            .and_then(|p| self.roots.get(&root_for(&self.workspace_roots, &p)))
            .into_iter()
            .flat_map(move |r| r.xrefs.callers(def))
            .filter(|x| x.kind.is_reference_site())
    }

    pub(super) fn analyze_document(&mut self, uri: &str, text: &str) -> Vec<Json> {
        self.ensure_workspace_scanned();
        self.analyze_text(uri, text)
    }

    /// One-time recursive scan of every workspace root for `.al` files.
    /// Without it, a file outside the open file's import closure is never
    /// compiled and its references stay invisible to workspace-wide queries.
    pub(super) fn ensure_workspace_scanned(&mut self) {
        if self.scanned {
            return;
        }
        self.scanned = true;
        let roots = self.workspace_roots.clone();
        for root in &roots {
            self.index_root(root);
        }
    }

    /// Index every non-stdlib `.al` file under `root` into `documents` and warm
    /// its session. Stdlib files are already seeded into every session, so
    /// re-checking them here would be pure latency. Idempotent, so it is safe
    /// to call again for a folder added after the initial scan latched.
    pub(super) fn index_root(&mut self, root: &Path) {
        let mut files = Vec::new();
        module::collect_al_files(root, &mut files);
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
            self.documents.insert(uri.clone(), text.clone());
            self.analyze_text(&uri, &text);
        }
    }

    /// Parse and type-check `text` as `uri`, updating the session, reverse-edge
    /// index and `entry_uri`, and return the LSP diagnostics. Callers decide
    /// whether to publish them.
    fn analyze_text(&mut self, uri: &str, text: &str) -> Vec<Json> {
        eprintln!("[AL LSP] Analyzing: {uri}");

        let mut sc = scanner::new_scanner(text.to_string());
        let p = parser::new_parser(&mut sc);
        let parser::ParseResult {
            ast: parsed_ast,
            diagnostics: parse_diagnostics,
            ..
        } = p.parse_program();

        let mut lsp_diagnostics: Vec<Json> = Vec::new();

        for diag in &parse_diagnostics {
            lsp_diagnostics.push(diagnostic_to_json(diag));
        }

        let has_errors = diagnostic::has_errors(&parse_diagnostics);

        // Check the buffer even when it has parse errors: the recovering parser
        // drops only the malformed statements, so the graph stays populated for
        // the clean parts and position queries keep working mid-edit.
        let ast_expr = ast::Expression::BlockExpression(parsed_ast);
        let file_path = uri_to_path(uri);
        let base_dir = file_path.as_deref().and_then(|p| p.parent());
        let stdlib_module = file_path
            .as_deref()
            .and_then(crate::module::detect_stdlib_module);
        let check_result = match &stdlib_module {
            // Editing stdlib: the precompiled blob is stale, so bypass the
            // session and check from source.
            Some(m) => bytecode::check_as_module(&ast_expr, base_dir, m.clone()),
            None => {
                let root = file_path
                    .as_deref()
                    .map(|p| root_for(&self.workspace_roots, p))
                    .unwrap_or_default();
                let session = &mut self
                    .roots
                    .entry(root)
                    .or_insert_with(RootState::new)
                    .session;
                // Mirror the buffer into the module overlay so dependents pick
                // up unsaved edits, but only while it parses cleanly: importers
                // keep resolving against the last good version.
                if !has_errors && let Some(p) = &file_path {
                    session.set_overlay(p.clone(), text.to_string());
                }
                session.check(&ast_expr, base_dir)
            }
        };

        // Type / unused diagnostics only for a buffer that fully parsed: a
        // partial AST yields noise for the region still being typed.
        if !has_errors {
            for diag in &check_result.diagnostics {
                lsp_diagnostics.push(diagnostic_to_json(diag));
            }

            // Unused-import / dead-code diagnostics. Only the session path
            // builds a reference graph, so the stdlib path has none.
            if stdlib_module.is_none()
                && let Some(qm) = query_module(uri)
                && let Some(graph) = self.graph_for(uri)
            {
                for diag in graph.unused_diagnostics_for(&qm) {
                    lsp_diagnostics.push(diagnostic_to_json(&diag));
                }
            }
        }

        // Latch the entry unconditionally, parse errors included, or
        // `ensure_entry` refuses every position query on a buffer with a syntax
        // error somewhere.
        self.entry_uri = Some(uri.to_string());

        // Persist this file's cross-module references, keyed by the `DefId`
        // each targets. A later query may re-root compilation to a file this
        // one imports, dropping this file from the session graph and with it
        // the reverse edges find-references needs. Refreshed per file so a
        // removed import drops its stale edges.
        if stdlib_module.is_none()
            && let Some(p) = &file_path
        {
            let root = root_for(&self.workspace_roots, p);
            let found = self
                .graph_for(uri)
                .and_then(|g| g.module_id(&module::main_module()).map(|id| (g, id)))
                .map(|(g, entry_id)| {
                    g.module_refs(entry_id).map_or_else(Vec::new, |mr| {
                        mr.occurrences()
                            .iter()
                            .map(|o| o.reference)
                            .filter(|r| r.target.module != entry_id && r.kind.is_reference_site())
                            .map(|r| (r.target, r.span, r.kind))
                            .collect()
                    })
                })
                .unwrap_or_default();
            self.roots
                .entry(root)
                .or_insert_with(RootState::new)
                .xrefs
                .refresh(uri, found);
        }

        lsp_diagnostics
    }
}

impl Default for Workspace {
    fn default() -> Self {
        Self::new()
    }
}
