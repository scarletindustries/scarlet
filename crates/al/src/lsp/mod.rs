//! LSP stdio transport: decode JSON-RPC frames, dispatch to [`Workspace`],
//! write responses back. All server state lives on `Workspace`, so tests build
//! one directly and never touch stdin/stdout.

use std::io::{self, BufRead, BufReader, Read as _, Write};

use serde_json::{Value as Json, json};

mod handlers;
mod wire;
mod workspace;
mod xrefs;

pub use workspace::Workspace;

use wire::{FileChangeType, doc_uri, folder_paths, rename_error_code, uri_to_path};

/// Outcome of one framed stdin read: a message body, or the client closed the
/// pipe.
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
            // A response to a server→client request has no method field.
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
            "textDocument/completion" => self.respond(&id, &params, Workspace::completion_response),
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
                // An unknown request must be answered or the client waits
                // forever; JSON-RPC forbids replying to a notification.
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

    /// Run `f` on the workspace and send its result. Diagnostics the query's
    /// re-root staged (see [`Workspace::ensure_entry`]) go out first, so an
    /// open importer's problem list refreshes on an edited dependency.
    fn respond(&mut self, id: &Json, params: &Json, f: fn(&mut Workspace, &Json) -> Json) {
        let result = f(&mut self.ws, params);
        for (uri, diags) in self.ws.take_pending_diagnostics() {
            self.publish_diagnostics(&uri, diags);
        }
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

    fn handle_initialize(&mut self, id: &Json, params: &Json) {
        // Prefer `workspaceFolders`, fall back to legacy `rootUri`. A client
        // opening a loose file sends neither; that file gets the empty root.
        let folders = params.get("workspaceFolders");
        if folders.is_some_and(Json::is_array) {
            self.ws.workspace_roots.extend(folder_paths(folders));
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
                    "completionProvider": { "triggerCharacters": ["."] },
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
        // didChangeWatchedFiles has no static registration in the LSP spec, so
        // ask for the .al watch dynamically. External edits (git checkout,
        // formatter) must invalidate the incremental session.
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
        for p in folder_paths(event.and_then(|e| e.get("removed"))) {
            self.ws.workspace_roots.retain(|r| r != &p);
            self.ws.roots.remove(&p);
        }
        let added = folder_paths(event.and_then(|e| e.get("added")));
        self.ws.workspace_roots.extend(added.iter().cloned());
        // The `scanned` latch makes `ensure_workspace_scanned` early-return, so
        // a folder added after the first scan must be indexed here or its
        // modules stay invisible to cross-module queries.
        if self.ws.scanned {
            for r in &added {
                self.ws.index_root(r);
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
        // Only the client-open documents. Driving this off the full `documents`
        // map would re-typecheck the whole workspace on every external edit.
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
        let result = self.ws.rename_response(params);
        for (uri, diags) in self.ws.take_pending_diagnostics() {
            self.publish_diagnostics(&uri, diags);
        }
        match result {
            Ok(r) => self.send_response(id, r),
            Err(e) => self.send_error(id, rename_error_code(&e), &e.message()),
        }
    }
}
