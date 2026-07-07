use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

use serde_json::{Value as Json, json};

use crate::ast;
use crate::bytecode;
use crate::diagnostic;
use crate::module::{self, ModulePath};
use crate::parser;
use crate::reference;
use crate::scanner;
use crate::span::Span;

/// One open editor buffer. The reference graph (built by the incremental
/// session and carried on every `CompileResult`) is the single source of
/// truth for hover / goto-def / find-refs / rename / symbols, so the document
/// only needs its text for re-analysis and overlay mirroring.
pub struct DocumentState {
    pub text: String,
}

/// One dependent-file occurrence of a cross-module definition, captured with
/// its true file URI so it survives a later analysis that makes a *different*
/// file the compilation entry.
#[derive(Clone)]
struct Xref {
    uri: String,
    span: Span,
    kind: reference::ReferenceKind,
}

/// Workspace-wide reverse edges that the session's per-entry reference graph
/// cannot retain on its own.
///
/// A position query re-roots compilation to the queried file, so that file
/// becomes the entry module (`main`) and only its own import closure is in the
/// session graph. A file's *importers* are never in its closure (import edges
/// point the other way) and are never cached as modules, so their references
/// into it — e.g. `main.al`'s call of `lib.greet()` when the query is driven
/// from `greet`'s declaration in `lib.al` — would otherwise vanish whenever
/// `lib.al` is the entry.
///
/// After every analysis we record the entry file's cross-module uses here,
/// keyed by the canonical [`reference::DefId`] each one targets (a cached
/// module's `DefId` is stable across re-roots because the session interner is
/// append-only). `by_file` lets a re-analysis of one file replace exactly its
/// own contribution, so a removed import stops resolving — keeping the index
/// coherent with incremental edits.
#[derive(Default)]
struct WorkspaceXrefs {
    by_def: HashMap<reference::DefId, Vec<Xref>>,
    by_file: HashMap<String, Vec<reference::DefId>>,
}

impl WorkspaceXrefs {
    /// Replace `uri`'s entire contribution with `found` (each entry a canonical
    /// target plus the occurrence's span/kind in `uri`).
    fn refresh(
        &mut self,
        uri: &str,
        found: Vec<(reference::DefId, Span, reference::ReferenceKind)>,
    ) {
        if let Some(defs) = self.by_file.remove(uri) {
            for d in defs {
                if let Some(v) = self.by_def.get_mut(&d) {
                    v.retain(|x| x.uri != uri);
                    if v.is_empty() {
                        self.by_def.remove(&d);
                    }
                }
            }
        }
        let mut touched: Vec<reference::DefId> = Vec::new();
        for (target, span, kind) in found {
            self.by_def.entry(target).or_default().push(Xref {
                uri: uri.to_string(),
                span,
                kind,
            });
            touched.push(target);
        }
        if !touched.is_empty() {
            self.by_file.insert(uri.to_string(), touched);
        }
    }

    /// Every persisted dependent-file occurrence of `def`.
    fn callers(&self, def: reference::DefId) -> &[Xref] {
        self.by_def.get(&def).map_or(&[], Vec::as_slice)
    }
}

pub struct LspServer {
    running: bool,
    documents: HashMap<String, DocumentState>,
    reader: BufReader<io::Stdin>,
    /// Roots reported by the client at `initialize` (and updated by
    /// `didChangeWorkspaceFolders`). Used to bucket files into sessions.
    workspace_roots: Vec<PathBuf>,
    /// One persistent compiler per workspace root; reused across `didChange`
    /// so unchanged imports stay cached. Keyed by the matching entry in
    /// `workspace_roots`, or an empty path for files opened outside any root.
    sessions: HashMap<PathBuf, bytecode::IncrementalSession>,
    /// Latched once the one-time recursive workspace scan has run, so cross-
    /// module references resolve workspace-wide rather than only within the
    /// open file's import closure.
    scanned: bool,
    /// URI of the document last analysed as the compilation entry. A query
    /// for a different document re-analyses it first so its module is keyed
    /// as the entry (`main`) in its session's workspace graph before we
    /// query it.
    entry_uri: Option<String>,
    /// URIs the client actually has open in an editor tab. Distinct from
    /// `documents`, which also holds the whole-workspace index produced by the
    /// one-time scan: diagnostics are published only for members of this set,
    /// and a watched-file change re-analyses only these, so opening a single
    /// file can't flood the Problems panel with errors for files (example
    /// demos, stdlib) the user never opened.
    open: HashSet<String>,
    /// Per-workspace-root persistent cross-module reverse edges, keyed by the
    /// same root as `sessions`. Survives re-rooting so find-references / rename
    /// driven from an imported file's declaration still see its dependent-file
    /// callers (see [`WorkspaceXrefs`]).
    xrefs: HashMap<PathBuf, WorkspaceXrefs>,
}

pub fn new_server() -> LspServer {
    LspServer {
        running: true,
        documents: HashMap::new(),
        reader: BufReader::new(io::stdin()),
        workspace_roots: Vec::new(),
        sessions: HashMap::new(),
        scanned: false,
        entry_uri: None,
        open: HashSet::new(),
        xrefs: HashMap::new(),
    }
}

impl LspServer {
    pub fn run(&mut self) {
        while self.running {
            match self.read_message() {
                Ok(content) => {
                    if content.is_empty() {
                        continue;
                    }
                    self.handle_message(&content);
                }
                Err(e) => {
                    self.log(&format!("Failed to read message: {}", e));
                }
            }
        }
    }

    fn read_message(&mut self) -> Result<String, String> {
        let mut content_length: usize = 0;

        loop {
            let mut line = String::new();
            let n = self
                .reader
                .read_line(&mut line)
                .map_err(|e| e.to_string())?;
            if n == 0 {
                self.running = false;
                return Ok(String::new());
            }

            let trimmed = line.trim();
            if trimmed.is_empty() {
                break;
            }

            if let Some(rest) = trimmed.strip_prefix("Content-Length:") {
                content_length = rest.trim().parse::<usize>().unwrap_or(0);
            }
        }

        if content_length == 0 {
            return Ok(String::new());
        }

        let mut content = vec![0u8; content_length];
        self.reader
            .read_exact(&mut content)
            .map_err(|e| e.to_string())?;

        String::from_utf8(content).map_err(|e| e.to_string())
    }

    fn handle_message(&mut self, content: &str) {
        let raw: Json = match serde_json::from_str(content) {
            Ok(v) => v,
            Err(e) => {
                self.log(&format!("Failed to parse JSON: {}", e));
                return;
            }
        };

        let method = raw
            .get("method")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string();
        let id = raw.get("id").cloned().unwrap_or(Json::Null);
        let params = raw.get("params").cloned().unwrap_or(Json::Null);

        self.log(&format!("Received: {}", method));

        match method.as_str() {
            // Response to a server→client request (e.g. registerCapability).
            // No method field; nothing to dispatch.
            "" => {}
            "initialize" => self.handle_initialize(&id, &params),
            "initialized" => self.handle_initialized(),
            "$/setTrace" | "$/cancelRequest" => {
                // Notifications, no response needed
            }
            "shutdown" => self.send_response(&id, Json::Null),
            "exit" => {
                self.running = false;
            }
            "textDocument/didOpen" => self.handle_did_open(&params),
            "textDocument/didChange" => self.handle_did_change(&params),
            "textDocument/didClose" => self.handle_did_close(&params),
            "textDocument/hover" => self.respond(&id, &params, Self::hover_response),
            "textDocument/definition" => self.respond(&id, &params, Self::definition_response),
            "textDocument/references" => self.respond(&id, &params, Self::references_response),
            "textDocument/rename" => self.handle_rename(&id, &params),
            "textDocument/prepareRename" => {
                self.respond(&id, &params, Self::prepare_rename_response)
            }
            "textDocument/documentSymbol" => {
                self.respond(&id, &params, Self::document_symbol_response)
            }
            "workspace/symbol" => self.respond(&id, &params, Self::workspace_symbol_response),
            "workspace/didChangeWorkspaceFolders" => {
                self.handle_did_change_workspace_folders(&params)
            }
            "workspace/didChangeWatchedFiles" => self.handle_did_change_watched_files(&params),
            _ => {
                self.log(&format!("Unknown method: {}", method));
            }
        }
    }

    fn send_response(&self, id: &Json, result: Json) {
        let response = json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        });
        self.send_message(&response.to_string());
    }

    /// Compute a request's result with `f` and send it as the response.
    fn respond(&mut self, id: &Json, params: &Json, f: fn(&mut Self, &Json) -> Json) {
        let result = f(self, params);
        self.send_response(id, result);
    }

    fn send_error(&self, id: &Json, code: i32, message: &str) {
        let response = json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message },
        });
        self.send_message(&response.to_string());
    }

    fn send_notification(&self, method: &str, params: Json) {
        let notification = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.send_message(&notification.to_string());
    }

    fn send_request(&self, id: Json, method: &str, params: Json) {
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.send_message(&request.to_string());
    }

    fn send_message(&self, content: &str) {
        let stdout = io::stdout();
        let mut out = stdout.lock();
        let _ = write!(out, "Content-Length: {}\r\n\r\n", content.len());
        let _ = out.write_all(content.as_bytes());
        let _ = out.flush();
    }

    fn log(&self, msg: &str) {
        eprintln!("[AL LSP] {}", msg);
    }

    // ========================================================================
    // Handlers
    // ========================================================================

    fn handle_initialize(&mut self, id: &Json, params: &Json) {
        // Multi-root: prefer `workspaceFolders` (array of {uri,name}); fall
        // back to legacy single `rootUri`. A client may send neither when a
        // loose file is opened, in which case every file lands in the
        // empty-root session.
        if let Some(folders) = params.get("workspaceFolders").and_then(|v| v.as_array()) {
            for f in folders {
                if let Some(uri) = f.get("uri").and_then(|v| v.as_str())
                    && let Some(p) = uri_to_path(uri)
                {
                    self.workspace_roots.push(p);
                }
            }
        } else if let Some(uri) = params.get("rootUri").and_then(|v| v.as_str())
            && let Some(p) = uri_to_path(uri)
        {
            self.workspace_roots.push(p);
        }
        self.send_response(
            id,
            json!({
                "capabilities": {
                    "textDocumentSync": 1,
                    "hoverProvider": true,
                    "definitionProvider": true,
                    "referencesProvider": true,
                    "renameProvider": { "prepareProvider": true },
                    "documentSymbolProvider": true,
                    "workspaceSymbolProvider": true,
                    "workspace": {
                        "workspaceFolders": {
                            "supported": true,
                            "changeNotifications": true,
                        }
                    },
                }
            }),
        );
    }

    fn handle_initialized(&self) {
        // File-watch capability can only be registered dynamically (LSP spec
        // forbids static registration for didChangeWatchedFiles). Ask the
        // client to watch every .al file so external edits — git checkout,
        // formatter, another editor — invalidate the incremental session.
        self.send_request(
            json!("al/watchers"),
            "client/registerCapability",
            json!({
                "registrations": [{
                    "id": "al/didChangeWatchedFiles",
                    "method": "workspace/didChangeWatchedFiles",
                    "registerOptions": {
                        "watchers": [{ "globPattern": "**/*.al" }]
                    }
                }]
            }),
        );
    }

    fn handle_did_change_workspace_folders(&mut self, params: &Json) {
        let event = params.get("event");
        if let Some(removed) = event
            .and_then(|e| e.get("removed"))
            .and_then(|v| v.as_array())
        {
            for f in removed {
                if let Some(uri) = f.get("uri").and_then(|v| v.as_str())
                    && let Some(p) = uri_to_path(uri)
                {
                    self.workspace_roots.retain(|r| r != &p);
                    self.sessions.remove(&p);
                }
            }
        }
        if let Some(added) = event
            .and_then(|e| e.get("added"))
            .and_then(|v| v.as_array())
        {
            let mut new_roots = Vec::new();
            for f in added {
                if let Some(uri) = f.get("uri").and_then(|v| v.as_str())
                    && let Some(p) = uri_to_path(uri)
                {
                    self.workspace_roots.push(p.clone());
                    new_roots.push(p);
                }
            }
            // A folder added after the initial scan would never be indexed
            // (the `scanned` latch makes `ensure_workspace_scanned` early
            // return), leaving its modules invisible to cross-module queries.
            // Index it now. Before the first scan, the pending
            // `ensure_workspace_scanned` will cover it from `workspace_roots`.
            if self.scanned {
                for r in &new_roots {
                    self.index_root(r);
                }
            }
        }
    }

    fn handle_did_change_watched_files(&mut self, params: &Json) {
        let Some(changes) = params.get("changes").and_then(|v| v.as_array()) else {
            return;
        };
        if self.sessions.is_empty() {
            return;
        }
        for change in changes {
            let Some(uri) = change.get("uri").and_then(|v| v.as_str()) else {
                continue;
            };
            if let Some(path) = uri_to_path(uri) {
                let root = root_for(&self.workspace_roots, &path);
                if let Some(session) = self.sessions.get_mut(&root) {
                    session.invalidate_path(&path);
                }
            }
        }
        // Re-analyse only the client-open documents (their imports may have
        // changed underneath them); the sessions invalidated above make the
        // re-check pick up the new sources. Driving this off the full
        // `documents` map would re-typecheck the entire workspace index on
        // every external `.al` change (formatter run, git checkout, save from
        // another editor).
        let open: Vec<(String, String)> = self
            .open
            .iter()
            .filter_map(|uri| {
                self.documents
                    .get(uri)
                    .map(|d| (uri.clone(), d.text.clone()))
            })
            .collect();
        for (uri, text) in open {
            self.analyze_document(&uri, &text);
        }
    }

    fn handle_did_open(&mut self, params: &Json) {
        let Some(uri) = doc_uri(params) else { return };
        let Some(text) = params
            .get("textDocument")
            .and_then(|t| t.get("text"))
            .and_then(|v| v.as_str())
        else {
            return;
        };
        self.open_document(&uri, text);
    }

    /// Test seam: register a workspace root, as the client's `initialize`
    /// `workspaceFolders`/`rootUri` would, so files under it bucket into one
    /// session and cross-module queries resolve workspace-wide.
    pub fn add_workspace_root(&mut self, root: PathBuf) {
        self.workspace_roots.push(root);
    }

    /// Open `uri` with `text` as a client tab and analyse it — the body of
    /// `textDocument/didOpen`, also exposed so a handler-layer test can drive
    /// a document into the reference graph / session before querying it.
    pub fn open_document(&mut self, uri: &str, text: &str) {
        self.documents.insert(
            uri.to_string(),
            DocumentState {
                text: text.to_string(),
            },
        );
        self.open.insert(uri.to_string());
        self.analyze_document(uri, text);
    }

    fn handle_did_change(&mut self, params: &Json) {
        let Some(uri) = doc_uri(params) else { return };
        let Some(changes) = params.get("contentChanges").and_then(|v| v.as_array()) else {
            return;
        };

        if let Some(last_change) = changes.last()
            && let Some(text) = last_change.get("text").and_then(|v| v.as_str())
        {
            self.open_document(&uri, text);
        }
    }

    fn handle_did_close(&mut self, params: &Json) {
        let Some(uri) = doc_uri(params) else { return };
        // No longer a client tab: drop it from the open set so we stop
        // publishing diagnostics for it (the file may stay in the workspace
        // index below).
        self.open.remove(&uri);
        // Keep the workspace index complete: a closed tab whose file still
        // exists stays indexed (re-synced to disk) so cross-module references
        // into it remain resolvable. Only forget files that are truly gone.
        if let Some(path) = uri_to_path(&uri)
            && let Ok(text) = std::fs::read_to_string(&path)
        {
            self.documents
                .insert(uri.clone(), DocumentState { text: text.clone() });
            self.analyze_document(&uri, &text);
        } else {
            self.documents.remove(&uri);
        }
    }

    /// Ensure `uri` is the document currently analysed as the compilation
    /// entry, so its module is keyed predictably in the graph, then report
    /// whether a graph is available to query. Re-analysis is cheap: the
    /// session keeps every imported/cached module from the last check.
    fn ensure_entry(&mut self, uri: &str) -> bool {
        if self.entry_uri.as_deref() != Some(uri) {
            let text = self.documents.get(uri).map(|d| d.text.clone());
            if let Some(text) = text {
                self.analyze_document(uri, &text);
            }
        }
        self.entry_uri.as_deref() == Some(uri) && self.graph_for(uri).is_some()
    }

    /// The workspace reference graph held by the session that owns `uri`.
    /// An in-repo stdlib file is checked via `check_as_module` (no session,
    /// because the precompiled blob is stale) so it has no graph and graph
    /// features no-op for it; cross-module goto-def *into* `al/*` from a
    /// normal file still works via the session graph's synthesised stdlib
    /// defs.
    fn graph_for(&self, uri: &str) -> Option<&reference::ReferenceGraph> {
        Some(self.session_for(uri)?.reference_graph())
    }

    /// The persistent compiler session that owns `uri`: it buckets the file
    /// into its workspace root and returns that root's session, with an
    /// in-repo stdlib carve-out (those files are
    /// checked via `check_as_module`, so they have no session and
    /// session-backed features no-op for them). Where `graph_for` exposes
    /// only the identity-level reference graph, this hands back the session
    /// itself so handlers can reach its type-aware queries — e.g.
    /// `IncrementalSession::hover`, which joins the inferred type onto the
    /// graph's name/kind for the hover response.
    pub fn session_for(&self, uri: &str) -> Option<&bytecode::IncrementalSession> {
        let path = uri_to_path(uri)?;
        if module::detect_stdlib_module(&path).is_some() {
            return None;
        }
        let root = root_for(&self.workspace_roots, &path);
        self.sessions.get(&root)
    }

    /// The module path the queried file's defs/occurrences are keyed under for
    /// position queries. An open file that another workspace file *imports*
    /// resolves to the canonical module its callers reference (e.g.
    /// `["." , "lib"]`), so a query driven from its own declaration shares the
    /// `DefId` every caller's reverse edge targets — instead of the bare `main`
    /// entry identity a re-rooted analysis would otherwise assign it. Falls back
    /// to [`query_module`] (the file's stdlib path, or the `main` entry).
    fn query_module_path(&self, uri: &str) -> ModulePath {
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
    /// [`query_module_path`](Self::query_module_path) in one step: the shared
    /// guard pair every position-based responder runs after `resolve_pos`.
    fn graph_module(&self, uri: &str) -> Option<(&reference::ReferenceGraph, reference::ModuleId)> {
        let graph = self.graph_for(uri)?;
        let mid = graph.module_id(&self.query_module_path(uri))?;
        Some((graph, mid))
    }

    /// Shared preamble for the position-based query responders. Re-analyses
    /// the requested document as the compilation entry (mutating) but does
    /// NOT answer the request: it returns `None` and lets the caller fold
    /// that into its own "no result" reply (`Json::Null`, wire-equivalent to
    /// the old `send_null_response`). A success guarantees `graph_for(&uri)`
    /// is `Some` for the caller to re-fetch (kept out of the return to
    /// sidestep the `&mut self`/`&graph` split).
    fn resolve_pos(&mut self, params: &Json) -> Option<(String, i32, i32)> {
        let (uri, line, col) = extract_position_params(params)?;
        self.ensure_entry(&uri).then_some((uri, line, col))
    }

    /// Compute the `textDocument/hover` result: a markdown block describing
    /// the symbol under the cursor, or `Json::Null` when there is nothing to
    /// show.
    pub fn hover_response(&mut self, params: &Json) -> Json {
        let Some((uri, line, col)) = self.resolve_pos(params) else {
            return Json::Null;
        };
        // Inferred type, joined from the owning session's hover-type table:
        // the reference graph is deliberately inference-free, so it supplies
        // name / kind / doc while the *type* comes from the session. Resolved
        // to an owned tuple before `graph_for` re-borrows `self`, and absent
        // for in-repo stdlib files (no session) — the response then falls back
        // to the graph's identity alone.
        let typed = query_module(&uri).and_then(|m| {
            let key = module::path_key(&m);
            self.session_for(&uri)
                .and_then(|s| s.hover(&key, line, col))
        });
        if let Some((graph, mid)) = self.graph_module(&uri)
            && let Some(def) = graph.definition_at(mid, line, col)
        {
            let signature = match &typed {
                Some((_, ty, _)) => format!("{} {}", def.name, ty),
                None => def.name.clone(),
            };
            let mut value = format!("```al\n{}\n```\n\n*{}*", signature, def.entity().noun());
            if let Some(d) = &def.doc {
                value.push_str("\n\n---\n\n");
                value.push_str(&clean_doc_comment(d));
            }
            return json!({ "contents": { "kind": "markdown", "value": value } });
        }
        Json::Null
    }

    /// Compute the `textDocument/definition` result: the `{ uri, range }` of
    /// the definition under the cursor, or `Json::Null` when none resolves.
    pub fn definition_response(&mut self, params: &Json) -> Json {
        let Some((uri, line, col)) = self.resolve_pos(params) else {
            return Json::Null;
        };
        let Some((graph, mid)) = self.graph_module(&uri) else {
            return Json::Null;
        };
        // The final module-name segment of an `import a/b` resolves to the
        // *imported* module's file. Its `Import` occurrence carries that
        // module's id; `module::resolve` maps `./x` to its on-disk file and an
        // embedded `al/*` module to nothing (null) — it never fabricates a
        // location. Without this the cursor would resolve to the importing
        // alias's own binding, a no-op self-jump.
        if let Some(import_mod) = import_target_at(graph, mid, line, col) {
            let req_path = uri_to_path(&uri);
            return match req_path
                .as_deref()
                .and_then(|p| reference::rename::module_uri(graph, import_mod, p.parent()))
            {
                Some(u) => json!({
                    "uri": u,
                    "range": {
                        "start": { "line": 0, "character": 0 },
                        "end": { "line": 0, "character": 0 },
                    },
                }),
                None => Json::Null,
            };
        }
        if let Some(def) = graph.definition_at(mid, line, col) {
            // Elsewhere in an `import ...` declaration — the `import` keyword, a
            // non-final path segment, the `as` alias binding — there is nothing
            // to navigate to (only the final segment resolves, above). The
            // `ModuleAlias` definition spans the whole declaration, so suppress
            // the otherwise no-op self-jump it would yield there.
            if !def.entity().is_navigable() {
                return Json::Null;
            }
            if let Some(u) = uri_for(graph, &uri, def.defid.module) {
                return json!({ "uri": u, "range": range_json(&def.span()) });
            }
        }
        Json::Null
    }

    /// Dependent-file callers of `def`, persisted across re-rooting: an
    /// importer of the queried file is never inside its imports' closure, so
    /// the session graph rooted at that file cannot carry these reverse edges
    /// — their true URIs are stored directly (see [`WorkspaceXrefs`]). Only
    /// real use sites are surfaced; the declaration's own self-occurrence and
    /// import/alias bindings are not.
    fn dependent_callers(&self, uri: &str, def: reference::DefId) -> impl Iterator<Item = &Xref> {
        uri_to_path(uri)
            .and_then(|p| self.xrefs.get(&root_for(&self.workspace_roots, &p)))
            .into_iter()
            .flat_map(move |wx| wx.callers(def))
            .filter(|x| x.kind.is_use_site())
    }

    /// Compute the `textDocument/references` result: an array of
    /// `{ uri, range }` (deduplicated) for the definition under the cursor,
    /// or `Json::Null` when none resolves. An empty result for a resolved
    /// definition stays an empty *array*, never null.
    pub fn references_response(&mut self, params: &Json) -> Json {
        let include_decl = params
            .get("context")
            .and_then(|c| c.get("includeDeclaration"))
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let Some((uri, line, col)) = self.resolve_pos(params) else {
            return Json::Null;
        };
        if let Some((graph, mid)) = self.graph_module(&uri)
            && let Some(def) = graph.definition_at(mid, line, col)
        {
            let defid = def.defid;
            let def_span = def.span();
            let mut seen: std::collections::BTreeSet<(String, i32, i32, i32, i32)> =
                std::collections::BTreeSet::new();
            let mut out: Vec<Json> = Vec::new();
            let mut push = |u: String, s: &Span, out: &mut Vec<Json>| {
                if seen.insert((
                    u.clone(),
                    s.start_line,
                    s.start_column,
                    s.end_line,
                    s.end_column,
                )) {
                    out.push(json!({ "uri": u, "range": range_json(s) }));
                }
            };
            for rr in graph.references_to(defid) {
                // The declaration's own self-occurrence (and import/alias
                // bindings) are excluded here; the declaration is added solely
                // via the `include_decl` branch below so `includeDeclaration =
                // false` is actually honored.
                if !rr.kind.is_use_site() {
                    continue;
                }
                if let Some(u) = uri_for(graph, &uri, rr.module) {
                    push(u, &rr.span, &mut out);
                }
            }
            // Dependent-file callers (see `dependent_callers`): dedup with the
            // live edges above — an importer that happens to be the current
            // entry appears in both.
            for x in self.dependent_callers(&uri, defid) {
                push(x.uri.clone(), &x.span, &mut out);
            }
            if include_decl && let Some(u) = uri_for(graph, &uri, defid.module) {
                push(u, &def_span, &mut out);
            }
            return Json::Array(out);
        }
        Json::Null
    }

    fn handle_rename(&mut self, id: &Json, params: &Json) {
        match self.rename_response(params) {
            Ok(r) => self.send_response(id, r),
            Err(e) => self.send_error(id, e.lsp_code(), &e.message()),
        }
    }

    /// Compute the `textDocument/rename` result: `Ok(WorkspaceEdit json)` for
    /// a renamable definition under the cursor, `Ok(Json::Null)` when nothing
    /// resolves there, or `Err(reason)` when the rename is refused (the
    /// caller maps that onto a JSON-RPC error).
    pub fn rename_response(
        &mut self,
        params: &Json,
    ) -> Result<Json, reference::rename::RenameError> {
        let new_name = params
            .get("newName")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let Some((uri, line, col)) = self.resolve_pos(params) else {
            return Ok(Json::Null);
        };
        if let Some((graph, mid)) = self.graph_module(&uri)
            && let Some(defid) = graph.def_id_at(mid, line, col)
        {
            let base = uri_to_path(&uri);
            let base_dir = base.as_deref().and_then(|p| p.parent());
            return match graph.rename(defid, &new_name, base_dir, Some((mid, uri.as_str()))) {
                Ok(mut we) => {
                    // Fold in dependent-file callers (see `dependent_callers`).
                    // `graph.rename` already validated `new_name` and rejected
                    // stdlib / module-alias targets, so reuse both for the
                    // cross-file edits; dedup against the edits it produced,
                    // then restore each file's positional edit order if
                    // anything was appended.
                    let mut added = false;
                    for x in self.dependent_callers(&uri, defid) {
                        let edits = we.changes.entry(x.uri.clone()).or_default();
                        if !edits.iter().any(|e| e.span == x.span) {
                            edits.push(reference::rename::TextEdit {
                                span: x.span,
                                new_text: new_name.clone(),
                            });
                            added = true;
                        }
                    }
                    if added {
                        for edits in we.changes.values_mut() {
                            edits.sort_by_key(|e| (e.span.start_line, e.span.start_column));
                        }
                    }
                    Ok(workspace_edit_json(&we))
                }
                Err(e) => Err(e),
            };
        }
        Ok(Json::Null)
    }

    /// Compute the `textDocument/prepareRename` result: `{ range, placeholder }`
    /// when the cursor sits on a renamable definition, or `Json::Null` when
    /// the position cannot be renamed.
    pub fn prepare_rename_response(&mut self, params: &Json) -> Json {
        let Some((uri, line, col)) = self.resolve_pos(params) else {
            return Json::Null;
        };
        let Some((graph, mid)) = self.graph_module(&uri) else {
            return Json::Null;
        };
        match graph.prepare_rename(mid, line, col) {
            Ok(p) => json!({ "range": range_json(&p.range), "placeholder": p.placeholder }),
            Err(_) => Json::Null,
        }
    }

    /// Compute the `textDocument/documentSymbol` result: an array of the
    /// module's *structural* declarations (top-level fns / types / consts and
    /// the constructors they introduce), or `Json::Null` when the document has
    /// no analysed module. Local bindings, parameters and pattern binders are
    /// recorded in the graph so goto-def / find-refs / rename resolve on them,
    /// but [`is_symbol_listable`](reference::Definition::is_symbol_listable)
    /// keeps them out of this projection — the editor outline lists a file's
    /// declarations, not every local in every function body.
    pub fn document_symbol_response(&mut self, params: &Json) -> Json {
        let Some(uri) = doc_uri(params) else {
            return Json::Null;
        };
        if !self.ensure_entry(&uri) {
            return Json::Null;
        }
        let Some((graph, mid)) = self.graph_module(&uri) else {
            return Json::Null;
        };
        let syms: Vec<Json> = graph
            .defs_in(mid)
            .filter(|d| d.is_symbol_listable())
            .map(|d| {
                json!({
                    "name": d.name,
                    "kind": symbol_kind(d.entity()),
                    "range": range_json(&d.span()),
                    "selectionRange": range_json(&d.span()),
                })
            })
            .collect();
        Json::Array(syms)
    }

    /// Compute the `workspace/symbol` result: every workspace declaration
    /// whose name matches the query, as `{ name, kind, location }`. Always a
    /// JSON *array* — empty (never null) when no module has been analysed yet.
    /// Like `document_symbol_response` this lists structural declarations only:
    /// `is_symbol_listable` excludes the `EntityKind::Value` locals the graph
    /// records for resolution, so the Cmd-T picker never offers a function's
    /// internal bindings.
    pub fn workspace_symbol_response(&mut self, params: &Json) -> Json {
        let query = params
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_lowercase();
        let Some(entry) = self.entry_uri.clone() else {
            return Json::Array(Vec::new());
        };
        let Some(graph) = self.graph_for(&entry) else {
            return Json::Array(Vec::new());
        };
        let mut out: Vec<Json> = Vec::new();
        for d in graph.all_defs() {
            if !d.is_symbol_listable() {
                continue;
            }
            if !query.is_empty() && !d.name.to_lowercase().contains(&query) {
                continue;
            }
            if let Some(u) = uri_for(graph, &entry, d.defid.module) {
                out.push(json!({
                    "name": d.name,
                    "kind": symbol_kind(d.entity()),
                    "location": { "uri": u, "range": range_json(&d.span()) },
                }));
            }
        }
        Json::Array(out)
    }

    // ========================================================================
    // Analysis
    // ========================================================================

    fn analyze_document(&mut self, uri: &str, text: &str) {
        self.ensure_workspace_scanned();
        self.analyze_text(uri, text);
    }

    /// One-time recursive scan of every workspace root for `.al` files,
    /// analysing each so its module is compiled/cached and its positions are
    /// indexed in `documents`. Without this a file imported by neither the
    /// open file nor its transitive imports is never compiled, so its
    /// references stay invisible to workspace-wide queries.
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

    /// Index every non-stdlib `.al` file under `root` into `documents` and
    /// warm its session, without publishing diagnostics (the file is not in
    /// `open`). Stdlib files are skipped: they are precompiled and seeded into
    /// every session, so re-checking them here via `check_as_module` bypasses
    /// the sessions entirely — pure latency that contributes nothing to
    /// cross-module resolution. Idempotent (a file already in `documents` is
    /// left untouched), so it is safe to call again for a workspace folder
    /// added at runtime after the initial scan latched.
    fn index_root(&mut self, root: &Path) {
        let mut files = Vec::new();
        module::collect_al_files(root, &mut files);
        for path in files {
            if module::detect_stdlib_module(&path).is_some() {
                continue;
            }
            let uri = reference::rename::path_to_uri(&path);
            if self.documents.contains_key(&uri) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            self.documents
                .insert(uri.clone(), DocumentState { text: text.clone() });
            self.analyze_text(&uri, &text);
        }
    }

    fn analyze_text(&mut self, uri: &str, text: &str) {
        self.log(&format!("Analyzing: {}", uri));

        let mut sc = scanner::new_scanner(text.to_string());
        let mut p = parser::new_parser(&mut sc);
        let parser::ParseResult {
            ast: parsed_ast,
            diagnostics: parse_diagnostics,
        } = p.parse_program();

        let mut lsp_diagnostics: Vec<Json> = Vec::new();

        for diag in &parse_diagnostics {
            lsp_diagnostics.push(diagnostic_to_json(diag));
        }

        let has_errors = diagnostic::has_errors(&parse_diagnostics);

        // Check the buffer — even when it has parse errors. The recovering
        // parser (`synchronize()` then resume) hands back a best-effort AST
        // that drops only the malformed statements, so it still spells every
        // well-formed declaration; feeding it through the session keeps the
        // workspace graph populated for the clean parts of a mid-edit buffer.
        // That is what lets hover / goto-def / find-refs / rename keep working
        // on a valid symbol while the user is partway through typing something
        // elsewhere. Skipping the check on any parse error (the previous
        // behaviour) meant a file that had never parsed cleanly had no session
        // at all, so every position query — including a rename of an untouched,
        // perfectly valid def — was refused outright.
        let ast_expr = ast::Expression::BlockExpression(parsed_ast);
        let file_path = uri_to_path(uri);
        let base_dir = file_path.as_deref().and_then(|p| p.parent());
        let stdlib_module = file_path
            .as_deref()
            .and_then(crate::module::detect_stdlib_module);
        let check_result = match &stdlib_module {
            // Editing stdlib: precompiled blob is stale by definition, so
            // bypass the session and check from source.
            Some(m) => bytecode::check_as_module(&ast_expr, base_dir, m.clone()),
            None => {
                let root = file_path
                    .as_deref()
                    .map(|p| root_for(&self.workspace_roots, p))
                    .unwrap_or_default();
                let session = self
                    .sessions
                    .entry(root)
                    .or_insert_with(|| bytecode::IncrementalSession::new(crate::stdlib()));
                // Mirror the in-memory buffer into the module overlay so a
                // dependent file's analyse picks up unsaved edits — but only
                // while it parses cleanly. A half-typed buffer must not become
                // what every importer resolves against; they keep seeing the
                // last good version until this one parses again. (The entry's
                // own graph is built from `ast_expr` below regardless, so the
                // open file stays renamable either way.)
                if !has_errors && let Some(p) = &file_path {
                    session.set_overlay(p.clone(), text.to_string());
                }
                session.check(&ast_expr, base_dir)
            }
        };

        // Publish the type / unused diagnostics only for a buffer that fully
        // parsed: the errors a partial AST yields are noise for the region the
        // user is still typing, and the published set must stay exactly the
        // parse errors in that case (the graph above is still built either way
        // — it just isn't surfaced as diagnostics).
        if !has_errors {
            for diag in &check_result.diagnostics {
                lsp_diagnostics.push(diagnostic_to_json(diag));
            }

            // 4th capability group: unused-import / dead-code diagnostics.
            // Only the session path builds a reference graph (the stdlib
            // `check_as_module` path has none), so resolve the entry module
            // from the session graph and fold its unused diagnostics into the
            // published set.
            if stdlib_module.is_none()
                && let Some(qm) = query_module(uri)
                && let Some(graph) = self.graph_for(uri)
            {
                for diag in graph.unused_diagnostics_for(&qm) {
                    lsp_diagnostics.push(diagnostic_to_json(&diag));
                }
            }
        }

        // The session now holds the workspace graph with this file as the
        // entry module — populated whether or not the buffer fully parsed — so
        // remember it as the analysed entry. Latching this unconditionally
        // (rather than only on a clean parse, the root cause of "rename refused
        // on a clean def") is what stops `ensure_entry` short-circuiting every
        // position query the moment the buffer holds a syntax error.
        self.entry_uri = Some(uri.to_string());

        // Persist this entry file's cross-module references workspace-wide,
        // keyed by the canonical `DefId` each one targets. The next position
        // query may re-root compilation to one of the files this one imports,
        // making *it* the entry and dropping this file (an importer is never in
        // its imports' closure, and is never cached as a module) from the
        // session graph; without this its reverse edges — e.g. `main.al`'s call
        // of `lib.greet()`, needed when find-references is driven from `greet`'s
        // declaration in `lib.al` — would vanish. Refreshed per-file so a
        // removed import drops its now-stale edges. The session-less stdlib edit
        // path builds no graph, so it contributes nothing.
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
                            .filter(|o| o.target.module != entry_id && o.kind.is_use_site())
                            .map(|o| (o.target, o.span, o.kind))
                            .collect()
                    })
                })
                .unwrap_or_default();
            self.xrefs.entry(root).or_default().refresh(uri, found);
        }

        // Only the client actually has these files open. The workspace scan
        // populates `documents` purely for cross-module indexing; publishing
        // its parse/type diagnostics would flood the Problems panel with
        // errors for files the user never opened (example demos churned by the
        // in-flight language rewrite, stdlib checked outside its real import
        // context, intentional syntax-error fixtures).
        if self.open.contains(uri) {
            self.send_notification(
                "textDocument/publishDiagnostics",
                json!({
                    "uri": uri,
                    "diagnostics": lsp_diagnostics,
                }),
            );
        }
    }
}

fn uri_to_path(uri: &str) -> Option<PathBuf> {
    // VSCode sends file:///abs/path. Percent-decoding is overkill for our
    // own repo paths (no spaces/unicode in src/std/), so a strip suffices.
    uri.strip_prefix("file://").map(PathBuf::from)
}

/// Pick the workspace root that owns `path`. With nested roots (rare but
/// permitted by LSP) the deepest one wins so a sub-project gets its own
/// session. Files outside every root share the empty-path session.
fn root_for(roots: &[PathBuf], path: &Path) -> PathBuf {
    roots
        .iter()
        .filter(|r| path.starts_with(r))
        .max_by_key(|r| r.as_os_str().len())
        .cloned()
        .unwrap_or_default()
}

fn doc_uri(params: &Json) -> Option<String> {
    Some(
        params
            .get("textDocument")?
            .get("uri")?
            .as_str()?
            .to_string(),
    )
}

fn extract_position_params(params: &Json) -> Option<(String, i32, i32)> {
    let uri = doc_uri(params)?;
    let pos = params.get("position")?;
    let line = pos.get("line")?.as_i64()? as i32;
    let col = pos.get("character")?.as_i64()? as i32;
    Some((uri, line, col))
}

/// LSP `SymbolKind` wire number for an [`EntityKind`]. Lives here rather than
/// on `EntityKind` because `al_core` is protocol-agnostic.
fn symbol_kind(e: reference::EntityKind) -> i32 {
    use reference::EntityKind;
    match e {
        EntityKind::Function => 12,
        EntityKind::Value => 13,
        EntityKind::Constant => 14,
        EntityKind::Constructor => 9,
        EntityKind::Type => 23,
        EntityKind::ModuleAlias => 2,
        EntityKind::Field => 8,
    }
}

fn diagnostic_to_json(diag: &diagnostic::Diagnostic) -> Json {
    let severity = match diag.severity {
        diagnostic::Severity::Error => 1,
        diagnostic::Severity::Hint => 4,
    };
    json!({
        "range": {
            "start": {
                "line": diag.span.start_line,
                "character": diag.span.start_column,
            },
            "end": {
                "line": diag.span.end_line,
                "character": diag.span.end_column,
            },
        },
        "severity": severity,
        "message": diag.message,
    })
}

/// The module a queried file's defs/occurrences are keyed under in the graph:
/// its stdlib path when it is an in-repo stdlib file, otherwise the entry
/// module (`main`) it was last analysed as.
fn query_module(uri: &str) -> Option<ModulePath> {
    let p = uri_to_path(uri)?;
    Some(module::detect_stdlib_module(&p).unwrap_or_else(module::main_module))
}

/// The id of the module imported by the `import a/b` declaration whose final
/// module-name segment the cursor sits on, or `None` when it is anywhere else.
/// The `Import` occurrence (recorded at that segment) carries the imported
/// module as its target's owning module; the tightest covering occurrence
/// wins, mirroring `ReferenceGraph::resolve_position`.
fn import_target_at(
    graph: &reference::ReferenceGraph,
    module: reference::ModuleId,
    line: i32,
    col: i32,
) -> Option<reference::ModuleId> {
    let mr = graph.module_refs(module)?;
    mr.occurrences()
        .iter()
        .filter(|o| o.kind == reference::ReferenceKind::Import && o.span.contains(line, col))
        .min_by_key(|o| o.span.width())
        .map(|o| o.target.module)
}

/// Translate a graph `ModuleId` back to a file URI, relative to the
/// requesting file (mirror of `uri_to_path`). The requesting file's own
/// module maps to its request URI (the entry module is bare and `resolve`
/// deliberately can't locate it); in-repo stdlib targets resolve against the
/// stdlib root so goto-def lands on the real declaration; everything else
/// goes through `module::resolve` via `reference::rename::module_uri`.
fn uri_for(
    graph: &reference::ReferenceGraph,
    request_uri: &str,
    module: reference::ModuleId,
) -> Option<String> {
    let path = graph.module_path(module)?;
    let req_path = uri_to_path(request_uri)?;
    let req_mod = module::detect_stdlib_module(&req_path).unwrap_or_else(module::main_module);
    if *path == req_mod {
        return Some(request_uri.to_string());
    }
    if module::is_stdlib(path) {
        if let Some(p) = stdlib_file(path, &req_path) {
            return Some(reference::rename::path_to_uri(&p));
        }
        return None;
    }
    reference::rename::module_uri(graph, module, req_path.parent())
}

/// Shape a pure `WorkspaceEdit` (computed in `al_core`, which has no
/// `serde_json`) into the LSP wire JSON.
fn workspace_edit_json(we: &reference::rename::WorkspaceEdit) -> Json {
    let mut changes = serde_json::Map::new();
    for (uri, edits) in &we.changes {
        let arr: Vec<Json> = edits
            .iter()
            .map(|e| json!({ "range": range_json(&e.span), "newText": e.new_text }))
            .collect();
        changes.insert(uri.clone(), Json::Array(arr));
    }
    json!({ "changes": Json::Object(changes) })
}

/// The on-disk path of stdlib module `m`, found by walking up from `near` for
/// the stdlib-root marker (inverse of `module::detect_stdlib_module`).
fn stdlib_file(m: &ModulePath, near: &Path) -> Option<PathBuf> {
    let near = near.canonicalize().ok()?;
    let mut dir = near.parent()?.to_path_buf();
    loop {
        if dir.join("src/std/.al-stdlib-root").is_file() {
            let mut p = dir.join("src/std");
            for seg in m {
                p.push(seg);
            }
            p.set_extension("al");
            return p.is_file().then_some(p);
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn range_json(span: &Span) -> Json {
    json!({
        "start": { "line": span.start_line, "character": span.start_column },
        "end": { "line": span.end_line, "character": span.end_column },
    })
}

fn clean_doc_comment(doc: &str) -> String {
    let content = doc
        .trim()
        .trim_start_matches("/*")
        .trim_end_matches("*/")
        .trim();

    let lines: Vec<&str> = content.split('\n').collect();
    let mut cleaned: Vec<String> = Vec::new();

    for line in lines {
        let trimmed = line.trim_start_matches([' ', '\t']);
        if let Some(rest) = trimmed.strip_prefix("* ") {
            cleaned.push(rest.to_string());
        } else if let Some(rest) = trimmed.strip_prefix('*') {
            cleaned.push(rest.trim_start_matches(' ').to_string());
        } else {
            cleaned.push(trimmed.to_string());
        }
    }

    cleaned.join("\n\n")
}
