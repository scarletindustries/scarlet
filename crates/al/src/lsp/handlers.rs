//! Position-query responders on [`Workspace`]. Each `*_response` computes the
//! JSON *result* of one LSP request (hover, definition, references, rename,
//! prepareRename, documentSymbol, workspace/symbol) with no I/O; the transport
//! layer wraps it in a `jsonrpc` envelope and writes it to stdout. Tests call
//! these directly on a `Workspace` and assert on the returned JSON.

use serde_json::{Value as Json, json};

use crate::module;
use crate::reference;
use crate::span::Span;

use super::wire::{
    clean_doc_comment, doc_uri, query_module, range_json, symbol_kind, uri_for, workspace_edit_json,
};
use super::workspace::Workspace;

impl Workspace {
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
            // Mint the query module's canonical key — its stdlib written
            // form, or the entry module — rather than hand-joining segments.
            let key = if module::is_stdlib(&m) {
                module::ModuleKey::for_stdlib(&m)
            } else {
                module::ModuleKey::main()
            };
            self.session_for(&uri)
                .and_then(|s| s.hover(Some(&key), line, col))
        });
        // A position naming a *module* — the final segment of `import a/b`, or
        // the `b` of `b.add(..)` — hovers as that module plus its doc comment.
        // Checked before the definition lookup: an import path segment resolves
        // to no `Definition` at all, and a qualifier resolves to the alias
        // binding, whose own doc is empty.
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

    /// Compute the `textDocument/definition` result: the `{ uri, range }` of
    /// the definition under the cursor, or `Json::Null` when none resolves.
    pub fn definition_response(&mut self, params: &Json) -> Json {
        let Some((uri, line, col)) = self.resolve_pos(params) else {
            return Json::Null;
        };
        let Some((graph, mid)) = self.graph_module(&uri) else {
            return Json::Null;
        };
        // A position naming a module — the final segment of `import a/b`, or
        // the `b` of `b.add(..)` — resolves to that module's file at 0:0.
        // `module::resolve_canonical` maps the module's canonical identity to
        // its on-disk file and an embedded `al/*` module to nothing (null);
        // it never fabricates a location. Without this the cursor would
        // resolve to the importing alias's own binding, whose span is the
        // whole `import` declaration — a no-op self-jump.
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
                // false` is actually honored. `Qualifier` occurrences — the `b`
                // of `b.add(..)` — are references to the alias and do belong.
                if !rr.kind.is_reference_site() {
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
            return match graph.rename(defid, &new_name, Some((mid, uri.as_str()))) {
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
}

/// A function type rendered with its parameter names: `fn(path String) Result(…)`.
///
/// The names are documentation carried alongside the type (`Definition::
/// param_names`), never inside it — al rejects labelled arguments outside a
/// constructor call, so a parameter's name is not part of a function's identity.
/// Falls back to the bare `Display` when the arity does not match, which is the
/// case for every non-function and for a value whose names were never recorded.
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
