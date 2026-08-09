//! Position-query responders on [`Workspace`]. Each `*_response` computes the
//! JSON result of one LSP request with no I/O; the transport layer wraps it in
//! a `jsonrpc` envelope.

use serde_json::{Value as Json, json};

use crate::module;
use crate::reference;
use crate::span::Span;

use super::wire::{
    clean_doc_comment, completion_item, doc_uri, qualifier_before, query_module, range_json,
    symbol_kind, uri_for, workspace_edit_json,
};
use super::workspace::Workspace;

impl Workspace {
    /// `textDocument/hover`: a markdown block for the symbol under the cursor,
    /// or `Json::Null`.
    pub fn hover_response(&mut self, params: &Json) -> Json {
        let Some((uri, line, col)) = self.resolve_pos(params) else {
            return Json::Null;
        };
        // The reference graph carries no inference, so the type comes from the
        // session's hover table. Absent for in-repo stdlib files, which have no
        // session; hover then shows the graph's identity alone.
        let typed = query_module(&uri).and_then(|m| {
            let key = if module::is_stdlib(&m) {
                module::ModuleKey::for_stdlib(&m)
            } else {
                module::ModuleKey::main()
            };
            self.session_for(&uri)
                .and_then(|s| s.hover(Some(&key), line, col))
        });
        // Must come before the definition lookup: an import path segment has no
        // `Definition`, and a qualifier resolves to the alias binding, whose doc
        // is empty.
        if let Some((graph, mid)) = self.graph_module(&uri)
            && let Some(target) = graph.module_named_at(mid, line, col)
        {
            let path = graph
                .module_path(target)
                .map_or(String::new(), |p| p.join("/"));
            let mut value = format!("```al\nimport {path}\n```\n\n*module*");
            if let Some(d) = graph.module_doc(target) {
                value.push_str("\n\n---\n\n");
                value.push_str(&clean_doc_comment(d));
            }
            return json!({ "contents": { "kind": "markdown", "value": value } });
        }
        if let Some((graph, mid)) = self.graph_module(&uri)
            && let Some(def) = graph.definition_at(mid, line, col)
        {
            let signature = match &typed {
                Some((_, ty, _)) => format!("{} {}", def.name, labelled(ty, def.param_names())),
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

    /// `textDocument/completion`: after `alias.`, the imported module's public
    /// exports; at a bare identifier, the current module's top-level
    /// declarations. Always an array — an unresolvable position yields an
    /// empty list, not null, so a client mid-keystroke sees "no items" rather
    /// than an error.
    pub fn completion_response(&mut self, params: &Json) -> Json {
        let empty = Json::Array(Vec::new());
        let Some((uri, line, col)) = self.resolve_pos(params) else {
            return empty;
        };
        let Some((graph, mid)) = self.graph_module(&uri) else {
            return empty;
        };
        // The completion prefix comes from the buffer, not the graph: a
        // trailing `alias.` is a parse error the recovering parser dropped, so
        // no occurrence exists at the cursor to resolve.
        let qualifier = self.documents.get(&uri).and_then(|text| {
            let line_text = text.lines().nth(line as usize)?;
            qualifier_before(line_text, col as usize)
        });

        let mut items: Vec<Json> = match qualifier {
            Some(q) => {
                // Only a module alias completes as a qualifier. A record value
                // before the dot would need field completion, which needs the
                // expression's inferred type — not wired up yet.
                let Some(target) = graph
                    .module_refs(mid)
                    .map_or(&[][..], |mr| mr.defs_named(&q))
                    .iter()
                    .filter_map(|d| graph.definition(*d))
                    .find_map(reference::Definition::imports_module)
                else {
                    return empty;
                };
                let key = graph.module_path(target).map(module::ModuleKey::of);
                let session = self.session_for(&uri);
                graph
                    .defs_in(target)
                    .filter(|d| d.is_pub && d.entity() != reference::EntityKind::Field)
                    .map(|d| {
                        // The type comes from the session's hover table, keyed
                        // by the export's own declaring span. Absent for
                        // modules with no recorded facts; the item then shows
                        // name and kind alone.
                        let detail = session
                            .zip(key.as_ref())
                            .and_then(|(s, k)| {
                                s.hover(Some(k), d.span().start_line, d.span().start_column)
                            })
                            .map(|(_, ty, _)| labelled(&ty, d.param_names()));
                        completion_item(d, detail)
                    })
                    .collect()
            }
            // Bare position: the module's own declarations, same surface as
            // documentSymbol plus module aliases, minus record fields, which
            // are never bare names.
            None => graph
                .defs_in(mid)
                .filter(|d| d.is_symbol_listable() && d.entity() != reference::EntityKind::Field)
                .map(|d| {
                    let detail = self
                        .session_for(&uri)
                        .and_then(|s| s.hover(None, d.span().start_line, d.span().start_column))
                        .map(|(_, ty, _)| labelled(&ty, d.param_names()));
                    completion_item(d, detail)
                })
                .collect(),
        };
        items.sort_by(|a, b| a["label"].as_str().cmp(&b["label"].as_str()));
        Json::Array(items)
    }

    /// `textDocument/definition`: the `{ uri, range }` of the definition under
    /// the cursor, or `Json::Null`.
    pub fn definition_response(&mut self, params: &Json) -> Json {
        let Some((uri, line, col)) = self.resolve_pos(params) else {
            return Json::Null;
        };
        let Some((graph, mid)) = self.graph_module(&uri) else {
            return Json::Null;
        };
        // A position naming a module jumps to that module's file at 0:0.
        // Embedded `al/*` modules have no file and yield null. Without this the
        // cursor would resolve to the alias binding, which spans the whole
        // `import` declaration — a no-op self-jump.
        if let Some(import_mod) = graph.module_named_at(mid, line, col) {
            return match reference::module_uri(graph, import_mod).ok() {
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
            // Elsewhere in an `import` declaration there is nothing to navigate
            // to, and `ModuleAlias` spans the whole declaration, so jumping
            // there would be a no-op self-jump.
            if !def.entity().is_navigable() {
                return Json::Null;
            }
            if let Some(u) = uri_for(graph, &uri, def.defid.module) {
                return json!({ "uri": u, "range": range_json(&def.span()) });
            }
        }
        Json::Null
    }

    /// `textDocument/references`: deduplicated `{ uri, range }` for the
    /// definition under the cursor, or `Json::Null` when none resolves. A
    /// resolved definition with no references gives an empty array, not null.
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
                // The declaration itself is added only by the `include_decl`
                // branch below, so `includeDeclaration = false` is honored.
                if !rr.kind.is_reference_site() {
                    continue;
                }
                if let Some(u) = uri_for(graph, &uri, rr.module) {
                    push(u, &rr.span, &mut out);
                }
            }
            // An importer that is also the current entry appears both here and
            // in the live edges above, hence the dedup.
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

    /// `textDocument/rename`: a `WorkspaceEdit`, `Json::Null` when nothing
    /// resolves, or `Err` when the rename is refused. The caller turns `Err`
    /// into a JSON-RPC error.
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
            return match graph.rename(defid, &new_name, Some((mid, uri.as_str()))) {
                Ok(mut we) => {
                    // `graph.rename` already validated `new_name` and rejected
                    // stdlib / module-alias targets, so the cross-file edits
                    // need no further checks. Appending breaks each file's
                    // positional edit order, so re-sort.
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

    /// `textDocument/prepareRename`: `{ range, placeholder }`, or `Json::Null`
    /// when the position cannot be renamed.
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

    /// `textDocument/documentSymbol`: the module's top-level declarations, or
    /// `Json::Null` when the document has no analysed module. The graph also
    /// holds locals and parameters; `is_symbol_listable` keeps them out.
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

    /// `workspace/symbol`: every declaration matching the query, as
    /// `{ name, kind, location }`. Always an array, empty rather than null when
    /// nothing has been analysed yet. Lists declarations only, as
    /// `document_symbol_response` does.
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
}

/// A function type rendered with its parameter names: `fn(path String) Result(…)`.
///
/// Names live beside the type, not in it — a parameter's name is not part of a
/// function's identity. Falls back to bare `Display` when the arity disagrees.
fn labelled(ty: &crate::type_def::Type, names: &[String]) -> String {
    let crate::type_def::Type::Function { params, ret } = ty else {
        return ty.to_string();
    };
    if names.len() != params.len() {
        return ty.to_string();
    }
    let ps: Vec<String> = names
        .iter()
        .zip(params)
        .map(|(n, t)| format!("{n} {t}"))
        .collect();
    format!("fn({}) {}", ps.join(", "), ret)
}
