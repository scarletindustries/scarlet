//! LSP stdio transport. [`LspServer`] owns a [`Workspace`] and a stdin
//! reader; it decodes JSON-RPC frames, dispatches each method to the matching
//! `Workspace` query/mutation, and writes responses/notifications to stdout.
//! All language-server *state* — buffers, sessions, the reference graph, the
//! reverse-edge index — lives on `Workspace`, so tests construct a `Workspace`
//! directly and never touch stdin/stdout.

use std::io::{self, BufRead, BufReader, Read as _, Write};

use serde_json::{Value as Json, json};

mod handlers;
mod wire;
mod workspace;
mod xrefs;

pub use workspace::Workspace;

use wire::{FileChangeType, doc_uri, rename_error_code, uri_to_path};

/// Outcome of one framed stdin read: a full message body, or clean EOF (the
/// client closed the pipe). Distinguishing EOF in the type — instead of the
/// old `running: bool` side-field flipped from inside `read_message` — makes
/// `run()`'s exit condition local to its own loop.
enum Incoming {
    Message(String),
    Eof,
}

pub struct LspServer {
    ws: Workspace,
    reader: BufReader<io::Stdin>,
}

pub fn new_server() -> LspServer {
    LspServer {
        ws: Workspace::new(),
        reader: BufReader::new(io::stdin()),
    }
}

impl LspServer {
    pub fn run(&mut self) {
        loop {
            match self.read_message() {
                Ok(Incoming::Eof) => break,
                Ok(Incoming::Message(c)) if c.is_empty() => continue,
                Ok(Incoming::Message(c)) => {
                    if self.handle_message(&c).is_break() {
                        break;
                    }
                }
                Err(e) => self.log(&format!("Failed to read message: {e}")),
            }
        }
    }

    fn read_message(&mut self) -> io::Result<Incoming> {
        let mut content_length: usize = 0;

        loop {
            let mut line = String::new();
            let n = self.reader.read_line(&mut line)?;
            if n == 0 {
                return Ok(Incoming::Eof);
            }

            let trimmed = line.trim();
            if trimmed.is_empty() {
                break;
            }

            if let Some(rest) = trimmed.strip_prefix("Content-Length:") {
                content_length = rest.trim().parse().map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "bad Content-Length header")
                })?;
            }
        }

        if content_length == 0 {
            return Ok(Incoming::Message(String::new()));
        }

        let mut content = vec![0u8; content_length];
        self.reader.read_exact(&mut content)?;

        String::from_utf8(content)
            .map(Incoming::Message)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    fn handle_message(&mut self, content: &str) -> std::ops::ControlFlow<()> {
        use std::ops::ControlFlow;
        let raw: Json = match serde_json::from_str(content) {
            Ok(v) => v,
            Err(e) => {
                self.log(&format!("Failed to parse JSON: {e}"));
                return ControlFlow::Continue(());
            }
        };

        let method = raw
            .get("method")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string();
        let id = raw.get("id").cloned().unwrap_or(Json::Null);
        let params = raw.get("params").cloned().unwrap_or(Json::Null);

        self.log(&format!("Received: {method}"));

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
            "exit" => return ControlFlow::Break(()),
            "textDocument/didOpen" => self.handle_did_open(&params),
            "textDocument/didChange" => self.handle_did_change(&params),
            "textDocument/didClose" => self.handle_did_close(&params),
            "textDocument/hover" => self.respond(&id, &params, Workspace::hover_response),
            "textDocument/definition" => self.respond(&id, &params, Workspace::definition_response),
            "textDocument/references" => self.respond(&id, &params, Workspace::references_response),
            "textDocument/rename" => self.handle_rename(&id, &params),
            "textDocument/prepareRename" => {
                self.respond(&id, &params, Workspace::prepare_rename_response)
            }
            "textDocument/documentSymbol" => {
                self.respond(&id, &params, Workspace::document_symbol_response)
            }
            "workspace/symbol" => self.respond(&id, &params, Workspace::workspace_symbol_response),
            "workspace/didChangeWorkspaceFolders" => {
                self.handle_did_change_workspace_folders(&params)
            }
            "workspace/didChangeWatchedFiles" => self.handle_did_change_watched_files(&params),
            _ => {
                // An unknown *request* (id present) must be answered or the
                // client waits forever; an unknown *notification* is just
                // logged (JSON-RPC forbids replying to a notification).
                if !matches!(id, Json::Null) {
                    self.send_error(&id, -32601, &format!("method not found: {method}"));
                } else {
                    self.log(&format!("Unknown method: {method}"));
                }
            }
        }
        ControlFlow::Continue(())
    }

    fn send_response(&self, id: &Json, result: Json) {
        let response = json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        });
        self.send_message(&response.to_string());
    }

    /// Compute a request's result on the workspace with `f` and send it as the
    /// response.
    fn respond(&mut self, id: &Json, params: &Json, f: fn(&mut Workspace, &Json) -> Json) {
        let result = f(&mut self.ws, params);
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
        if let Err(e) = write!(out, "Content-Length: {}\r\n\r\n", content.len())
            .and_then(|()| out.write_all(content.as_bytes()))
            .and_then(|()| out.flush())
        {
            self.log(&format!("stdout write failed: {e} — client likely gone"));
        }
    }

    fn publish_diagnostics(&self, uri: &str, diagnostics: Vec<Json>) {
        self.send_notification(
            "textDocument/publishDiagnostics",
            json!({ "uri": uri, "diagnostics": diagnostics }),
        );
    }

    fn log(&self, msg: &str) {
        eprintln!("[AL LSP] {msg}");
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
                    self.ws.workspace_roots.push(p);
                }
            }
        } else if let Some(uri) = params.get("rootUri").and_then(|v| v.as_str())
            && let Some(p) = uri_to_path(uri)
        {
            self.ws.workspace_roots.push(p);
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
                    self.ws.workspace_roots.retain(|r| r != &p);
                    self.ws.roots.remove(&p);
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
                    self.ws.workspace_roots.push(p.clone());
                    new_roots.push(p);
                }
            }
            // A folder added after the initial scan would never be indexed
            // (the `scanned` latch makes `ensure_workspace_scanned` early
            // return), leaving its modules invisible to cross-module queries.
            // Index it now. Before the first scan, the pending
            // `ensure_workspace_scanned` will cover it from `workspace_roots`.
            if self.ws.scanned {
                for r in &new_roots {
                    self.ws.index_root(r);
                }
            }
        }
    }

    fn handle_did_change_watched_files(&mut self, params: &Json) {
        let Some(changes) = params.get("changes").and_then(|v| v.as_array()) else {
            return;
        };
        if self.ws.roots.is_empty() {
            return;
        }
        for change in changes {
            let Some(uri) = change.get("uri").and_then(|v| v.as_str()) else {
                continue;
            };
            let ty =
                FileChangeType::from_wire(change.get("type").and_then(|v| v.as_i64()).unwrap_or(2));
            self.ws.invalidate_watched(uri, ty);
        }
        // Re-analyse only the client-open documents (their imports may have
        // changed underneath them); the sessions invalidated above make the
        // re-check pick up the new sources. Driving this off the full
        // `documents` map would re-typecheck the entire workspace index on
        // every external `.al` change (formatter run, git checkout, save from
        // another editor).
        for (uri, diags) in self.ws.reanalyze_open() {
            self.publish_diagnostics(&uri, diags);
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
        let diags = self.ws.open_document(&uri, text);
        self.publish_diagnostics(&uri, diags);
    }

    fn handle_did_change(&mut self, params: &Json) {
        let Some(uri) = doc_uri(params) else { return };
        let Some(changes) = params.get("contentChanges").and_then(|v| v.as_array()) else {
            return;
        };

        if let Some(last_change) = changes.last()
            && let Some(text) = last_change.get("text").and_then(|v| v.as_str())
        {
            let diags = self.ws.open_document(&uri, text);
            self.publish_diagnostics(&uri, diags);
        }
    }

    fn handle_did_close(&mut self, params: &Json) {
        let Some(uri) = doc_uri(params) else { return };
        self.ws.close_document(&uri);
    }

    fn handle_rename(&mut self, id: &Json, params: &Json) {
        match self.ws.rename_response(params) {
            Ok(r) => self.send_response(id, r),
            Err(e) => self.send_error(id, rename_error_code(&e), &e.message()),
        }
    }
}
