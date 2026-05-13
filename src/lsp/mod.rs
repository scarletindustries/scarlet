use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};

use serde_json::{Value as Json, json};

use crate::ast;
use crate::bytecode;
use crate::diagnostic;
use crate::flags::Flags;
use crate::parser;
use crate::scanner;
use crate::type_def;

#[derive(Debug, Clone)]
pub struct TypeAtPosition {
    pub line: i32,
    pub col_start: i32,
    pub col_end: i32,
    pub type_str: String,
    pub name: String,
    pub def_line: i32,
    pub def_col: i32,
    pub def_end: i32,
    pub doc: Option<String>,
}

pub struct DocumentState {
    pub text: String,
    pub type_info: Vec<TypeAtPosition>,
}

pub struct LspServer {
    running: bool,
    documents: HashMap<String, DocumentState>,
    reader: BufReader<io::Stdin>,
}

pub fn new_server() -> LspServer {
    LspServer {
        running: true,
        documents: HashMap::new(),
        reader: BufReader::new(io::stdin()),
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
            "initialize" => self.handle_initialize(&id),
            "initialized" | "$/setTrace" | "$/cancelRequest" => {
                // Notifications, no response needed
            }
            "shutdown" => self.handle_shutdown(&id),
            "exit" => {
                self.running = false;
            }
            "textDocument/didOpen" => self.handle_did_open(&params),
            "textDocument/didChange" => self.handle_did_change(&params),
            "textDocument/didClose" => self.handle_did_close(&params),
            "textDocument/hover" => self.handle_hover(&id, &params),
            "textDocument/definition" => self.handle_definition(&id, &params),
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

    fn send_null_response(&self, id: &Json) {
        let response = json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": Json::Null,
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

    fn handle_initialize(&self, id: &Json) {
        self.send_response(
            id,
            json!({
                "capabilities": {
                    "textDocumentSync": 1,
                    "hoverProvider": true,
                    "definitionProvider": true,
                }
            }),
        );
    }

    fn handle_shutdown(&self, id: &Json) {
        self.send_null_response(id);
    }

    fn handle_did_open(&mut self, params: &Json) {
        let text_doc = match params.get("textDocument") {
            Some(v) => v,
            None => return,
        };
        let uri = match text_doc.get("uri").and_then(|v| v.as_str()) {
            Some(v) => v.to_string(),
            None => return,
        };
        let text = match text_doc.get("text").and_then(|v| v.as_str()) {
            Some(v) => v.to_string(),
            None => return,
        };

        self.documents.insert(
            uri.clone(),
            DocumentState {
                text: text.clone(),
                type_info: Vec::new(),
            },
        );
        self.analyze_document(&uri, &text);
    }

    fn handle_did_change(&mut self, params: &Json) {
        let text_doc = match params.get("textDocument") {
            Some(v) => v,
            None => return,
        };
        let uri = match text_doc.get("uri").and_then(|v| v.as_str()) {
            Some(v) => v.to_string(),
            None => return,
        };
        let changes = match params.get("contentChanges").and_then(|v| v.as_array()) {
            Some(v) => v,
            None => return,
        };

        if let Some(last_change) = changes.last()
            && let Some(text) = last_change.get("text").and_then(|v| v.as_str())
        {
            let text = text.to_string();
            if let Some(doc) = self.documents.get_mut(&uri) {
                doc.text = text.clone();
            } else {
                self.documents.insert(
                    uri.clone(),
                    DocumentState {
                        text: text.clone(),
                        type_info: Vec::new(),
                    },
                );
            }
            self.analyze_document(&uri, &text);
        }
    }

    fn handle_did_close(&mut self, params: &Json) {
        let text_doc = match params.get("textDocument") {
            Some(v) => v,
            None => return,
        };
        let uri = match text_doc.get("uri").and_then(|v| v.as_str()) {
            Some(v) => v.to_string(),
            None => return,
        };

        self.documents.remove(&uri);
    }

    fn handle_hover(&self, id: &Json, params: &Json) {
        let (uri, line, col) = match extract_position_params(params) {
            Some(v) => v,
            None => {
                self.send_null_response(id);
                return;
            }
        };

        if let Some(doc) = self.documents.get(&uri) {
            for t in &doc.type_info {
                if t.line == line && col >= t.col_start && col < t.col_end {
                    let type_sig = format!("{}: {}", t.name, t.type_str);
                    let mut value = format!("```al\n{}\n```", type_sig);
                    if let Some(d) = &t.doc {
                        let clean_doc = clean_doc_comment(d);
                        value.push_str("\n\n---\n\n");
                        value.push_str(&clean_doc);
                    }
                    self.send_response(
                        id,
                        json!({
                            "contents": {
                                "kind": "markdown",
                                "value": value,
                            }
                        }),
                    );
                    return;
                }
            }
        }

        self.send_null_response(id);
    }

    fn handle_definition(&self, id: &Json, params: &Json) {
        let (uri, line, col) = match extract_position_params(params) {
            Some(v) => v,
            None => {
                self.send_null_response(id);
                return;
            }
        };

        if let Some(doc) = self.documents.get(&uri) {
            for t in &doc.type_info {
                if t.line == line
                    && col >= t.col_start
                    && col < t.col_end
                    && t.def_line >= 0
                    && t.def_col >= 0
                {
                    self.send_response(
                        id,
                        json!({
                            "uri": uri,
                            "range": {
                                "start": { "line": t.def_line, "character": t.def_col },
                                "end": { "line": t.def_line, "character": t.def_end },
                            }
                        }),
                    );
                    return;
                }
            }
        }

        self.send_null_response(id);
    }

    // ========================================================================
    // Analysis
    // ========================================================================

    fn analyze_document(&mut self, uri: &str, text: &str) {
        self.log(&format!("Analyzing: {}", uri));

        let mut sc = scanner::new_scanner(text.to_string());
        let mut p = parser::new_parser(&mut sc);
        let parse_result = p.parse_program();

        let mut lsp_diagnostics: Vec<Json> = Vec::new();

        for diag in &parse_result.diagnostics {
            lsp_diagnostics.push(diagnostic_to_json(diag));
        }

        let has_errors = diagnostic::has_errors(&parse_result.diagnostics);
        if !has_errors {
            let ast_expr = ast::Expression::BlockExpression(parse_result.ast.clone());
            let check_result = bytecode::check(&ast_expr, Flags::default());

            for diag in &check_result.diagnostics {
                lsp_diagnostics.push(diagnostic_to_json(diag));
            }

            let types = extract_types(&check_result);
            if let Some(doc) = self.documents.get_mut(uri) {
                doc.type_info = types;
            } else {
                self.documents.insert(
                    uri.to_string(),
                    DocumentState {
                        text: text.to_string(),
                        type_info: types,
                    },
                );
            }
        }

        self.send_notification(
            "textDocument/publishDiagnostics",
            json!({
                "uri": uri,
                "diagnostics": lsp_diagnostics,
            }),
        );
    }
}

fn extract_position_params(params: &Json) -> Option<(String, i32, i32)> {
    let text_doc = params.get("textDocument")?;
    let pos = params.get("position")?;
    let uri = text_doc.get("uri")?.as_str()?.to_string();
    let line = pos.get("line")?.as_i64()? as i32;
    let col = pos.get("character")?.as_i64()? as i32;
    Some((uri, line, col))
}

fn diagnostic_to_json(diag: &diagnostic::Diagnostic) -> Json {
    let severity = if diag.severity == diagnostic::Severity::Error {
        1
    } else {
        2
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

fn extract_types(check_result: &bytecode::CompileResult) -> Vec<TypeAtPosition> {
    let mut result = Vec::new();
    for tp in &check_result.type_positions {
        result.push(TypeAtPosition {
            line: tp.line,
            col_start: tp.column,
            col_end: tp.end_col,
            name: tp.name.clone(),
            type_str: type_def::type_to_string(&tp.type_info),
            def_line: tp.def_line,
            def_col: tp.def_col,
            def_end: tp.def_end,
            doc: tp.doc.clone(),
        });
    }
    result
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
