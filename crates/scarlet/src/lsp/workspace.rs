//! The LSP server's workspace state and analysis: every open or indexed buffer, the incremental
//! session for each root, and the index of cross-module reverse edges. It answers position
//! queries and computes diagnostics, but never touches stdio. The transport layer in `mod.rs`
//! does that.

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
    DocPosition, WatchedChange, diagnostic_to_json, extract_position_params, query_module,
    root_for, uri_to_path,
};
use super::xrefs::{RootState, Xref};

/// The [`RootState`] key that owns `path`, and whether that root is the in-repo stdlib source
/// tree. Stdlib sources go under their own root (the `src/std` directory itself) with a
/// from-source session, and everything else goes under the longest matching workspace root.
fn owning_root(workspace_roots: &[PathBuf], path: &Path) -> (PathBuf, bool) {
    if module::detect_stdlib_module(path).is_some()
        && let Some(std_root) = module::find_stdlib_root(path)
    {
        return (std_root, true);
    }
    (root_for(workspace_roots, path), false)
}

pub struct Workspace {
    /// The text of every known `.sca` buffer, keyed by URI. It holds only text, because the
    /// session's reference graph answers hover, go-to-definition, find-references and rename.
    pub(super) documents: HashMap<String, String>,
    /// Roots reported by the client, used to bucket files into sessions.
    pub(super) workspace_roots: Vec<PathBuf>,
    /// One [`RootState`] per workspace root, keyed by the matching entry in
    /// `workspace_roots`, or an empty path for files outside any root.
    pub(super) roots: HashMap<PathBuf, RootState>,
    /// Set once the one-time recursive workspace scan has run.
    pub(super) scanned: bool,
    /// The URI of the document last analysed as the compilation entry. A query for a different
    /// document re-analyses that one first, so its module is keyed as the entry (`main`) in its
    /// session's graph.
    pub(super) entry_uri: Option<String>,
    /// URIs the client has open in a tab, a subset of `documents` (which also
    /// holds the whole-workspace scan). Only these get published diagnostics or
    /// get re-analysed on a watched-file change.
    open: HashSet<String>,
    /// Diagnostics from a position query re-rooting the entry, waiting for the transport layer to
    /// publish them. Without them, an importer's Problems panel goes stale after an in-memory edit
    /// to one of its imports.
    pending_diagnostics: Vec<(String, Vec<Json>)>,
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

    /// Registers a workspace root, as the client's `initialize` would. Tests use this.
    pub fn add_workspace_root(&mut self, root: PathBuf) {
        self.workspace_roots.push(root);
    }

    /// Handles `textDocument/didOpen` and `didChange`. It opens `uri` as a client tab, analyses it,
    /// and returns the diagnostics to publish.
    pub fn open_document(&mut self, uri: &str, text: &str) -> Vec<Json> {
        self.documents.insert(uri.to_string(), text.to_string());
        self.open.insert(uri.to_string());
        self.analyze_document(uri, text)
    }

    /// Handles `textDocument/didClose`. It re-syncs the index with the file on disk, or, if the
    /// file is really gone, forgets it and drops its saved reverse edges.
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
                && let Some(r) = self.roots.get_mut(&owning_root(&self.workspace_roots, p).0)
            {
                r.xrefs.refresh(uri, Vec::new());
            }
        }
    }

    /// Applies one `workspace/didChangeWatchedFiles` change. It invalidates the owning session's
    /// cache, and on deletion drops the file's reverse edges so find-references stops reporting
    /// occurrences in a file that's gone.
    pub(super) fn invalidate_watched(&mut self, uri: &str, ty: WatchedChange) {
        let Some(path) = uri_to_path(uri) else {
            return;
        };
        let (root, is_stdlib_root) = owning_root(&self.workspace_roots, &path);
        // The prelude seeds the stdlib session and sits below its rewind floor. Drop the whole
        // root so the next analysis re-seeds from the new source instead of serving stale
        // prelude facts.
        if is_stdlib_root
            && module::detect_stdlib_module(&path).as_deref()
                == Some(module::scarlet_prelude().as_slice())
        {
            self.roots.remove(&root);
            return;
        }
        if let Some(r) = self.roots.get_mut(&root) {
            r.session.invalidate_path(&path);
            if ty == WatchedChange::Deleted {
                r.xrefs.refresh(uri, Vec::new());
            }
        }
        if ty == WatchedChange::Deleted {
            self.documents.remove(uri);
        }
    }

    /// Re-analyses every document the client has open and returns their fresh diagnostics.
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

    /// Makes `uri` the document analysed as the compilation entry, then reports whether there's a
    /// graph to query.
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

    /// Drains the diagnostics staged by [`ensure_entry`](Self::ensure_entry), so they're published
    /// alongside the query response.
    pub fn take_pending_diagnostics(&mut self) -> Vec<(String, Vec<Json>)> {
        std::mem::take(&mut self.pending_diagnostics)
    }

    /// The workspace reference graph held by the session that owns `uri`.
    pub(super) fn graph_for(&self, uri: &str) -> Option<&reference::ReferenceGraph> {
        Some(self.session_for(uri)?.reference_graph())
    }

    /// The persistent compiler session that owns `uri`, for handlers that need its type-aware
    /// queries and not just the reference graph. The stdlib root's from-source session owns
    /// in-repo stdlib files.
    pub fn session_for(&self, uri: &str) -> Option<&bytecode::IncrementalSession> {
        let path = uri_to_path(uri)?;
        let (root, _) = owning_root(&self.workspace_roots, &path);
        self.roots.get(&root).map(|r| &r.session)
    }

    /// The module path the queried file's definitions are keyed under. An imported file resolves
    /// to the canonical module its callers reference, so a query from its own declaration shares
    /// the `DefId` their reverse edges target, not the `main` entry identity a re-rooted analysis
    /// would give it.
    fn query_module_path(&self, uri: &str) -> ModulePath {
        if let Some(path) = uri_to_path(uri)
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

    /// The first step every position query handler shares. It re-analyses the document as the
    /// entry. `Some` guarantees `graph_for(&uri)` is `Some`, and the caller fetches the graph again
    /// to avoid a `&mut self` and `&graph` overlap.
    pub(super) fn resolve_pos(&mut self, params: &Json) -> Option<DocPosition> {
        let pos = extract_position_params(params)?;
        self.ensure_entry(&pos.uri).then_some(pos)
    }

    /// Callers of `def` in dependent files. We keep them across re-rooting because an importer is
    /// never inside its imports' closure. It returns only reference sites, because those are what
    /// rename rewrites.
    pub(super) fn dependent_callers<'a>(
        &'a self,
        uri: &str,
        def: &'a super::xrefs::StableDefId,
    ) -> impl Iterator<Item = &'a Xref> {
        uri_to_path(uri)
            .and_then(|p| self.roots.get(&owning_root(&self.workspace_roots, &p).0))
            .into_iter()
            .flat_map(move |r| r.xrefs.callers(def))
            .filter(|x| x.kind.is_reference_site())
    }

    fn analyze_document(&mut self, uri: &str, text: &str) -> Vec<Json> {
        self.ensure_workspace_scanned();
        self.analyze_text(uri, text)
    }

    /// Scans every workspace root for `.sca` files, once. Without this, a file outside the open
    /// file's import closure is never compiled and workspace-wide queries never see its
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

    /// Indexes every non-stdlib `.sca` file under `root` into `documents` and warms its session.
    /// Every session already has the stdlib files seeded, so checking them again here would only
    /// add latency. It's idempotent, so it's safe to call again for a folder added after the
    /// first scan.
    pub(super) fn index_root(&mut self, root: &Path) {
        let mut files = Vec::new();
        module::collect_source_files(root, &mut files);
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

    /// Parses and type-checks `text` as `uri`, updates the session, the reverse-edge index and
    /// `entry_uri`, and returns the LSP diagnostics. Callers decide whether to publish them.
    fn analyze_text(&mut self, uri: &str, text: &str) -> Vec<Json> {
        eprintln!("[Scarlet LSP] Analysing: {uri}");

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

        // Check the buffer even when it has parse errors. The recovering parser drops only the
        // malformed statements, so the graph keeps the clean parts and position queries keep
        // working mid-edit.
        let ast_expr = ast::Expression::BlockExpression(parsed_ast);
        let file_path = uri_to_path(uri);
        let base_dir = file_path.as_deref().and_then(|p| p.parent());
        let stdlib_module = file_path
            .as_deref()
            .and_then(crate::module::detect_stdlib_module);
        let (root, is_stdlib_root) = file_path
            .as_deref()
            .map(|p| owning_root(&self.workspace_roots, p))
            .unwrap_or_default();
        let session = &mut self
            .roots
            .entry(root.clone())
            .or_insert_with(|| {
                if is_stdlib_root {
                    RootState::new_stdlib(root.clone())
                } else {
                    RootState::new()
                }
            })
            .session;
        // Mirror the buffer into the module overlay so dependents pick up unsaved edits, but only
        // while it parses cleanly. Until then, importers keep resolving against the last good
        // version.
        if !has_errors && let Some(p) = &file_path {
            session.set_overlay(p.clone(), text.to_string());
        }
        let check_result = match &stdlib_module {
            // For a stdlib file, analyse the buffer *as* its stdlib module in the stdlib root's
            // from-source session, so it gets the same graph, hover and incremental machinery as
            // any other file.
            Some(m) => session.check_as(&ast_expr, base_dir, m.clone()),
            None => session.check(&ast_expr, base_dir),
        };

        // Only report type and unused diagnostics for a buffer that fully parsed, because a
        // partial AST gives noise for the region still being typed.
        if !has_errors {
            for diag in &check_result.diagnostics {
                lsp_diagnostics.push(diagnostic_to_json(diag));
            }

            // Unused-import / dead-code diagnostics.
            if let Some(qm) = query_module(uri)
                && let Some(graph) = self.graph_for(uri)
            {
                for diag in graph.unused_diagnostics_for(&qm) {
                    lsp_diagnostics.push(diagnostic_to_json(&diag));
                }
            }
        }

        // Set the entry every time, parse errors included, or `ensure_entry` refuses every
        // position query on a buffer with a syntax error anywhere in it.
        self.entry_uri = Some(uri.to_string());

        // Save this file's cross-module references, keyed by a stable id for the definition each
        // one targets. A later query may re-root compilation at a file this one imports, which
        // drops this file from the session graph, and with it the reverse edges find-references
        // needs. We refresh them per file so a removed import drops its stale edges.
        if let Some(p) = &file_path {
            let root = owning_root(&self.workspace_roots, p).0;
            let entry_module = stdlib_module.clone().unwrap_or_else(module::main_module);
            let found = self
                .graph_for(uri)
                .and_then(|g| g.module_id(&entry_module).map(|id| (g, id)))
                .map(|(g, entry_id)| {
                    g.module_refs(entry_id).map_or_else(Vec::new, |mr| {
                        mr.occurrences()
                            .iter()
                            .map(|o| o.reference)
                            .filter(|r| r.target.module != entry_id && r.kind.is_reference_site())
                            .filter_map(|r| {
                                // Use the stable key, not the DefId. The target file's next
                                // edit shifts every span in it, and edges filed under a key
                                // that carries a span would be lost to rename and
                                // find-references.
                                let d = g.definition(r.target)?;
                                let key = super::xrefs::StableDefId::of(g, uri, d)?;
                                Some((key, r.span, r.kind))
                            })
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
