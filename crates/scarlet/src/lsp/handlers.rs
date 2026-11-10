//! The handlers on [`Workspace`] that answer position queries. Each `*_response` computes the
//! JSON result of one LSP request with no I/O, and the transport layer wraps it in a `jsonrpc`
//! envelope.

use serde_json::{Value as Json, json};

use crate::build;
use crate::module::ModuleKey;
use crate::reference::{self, EntityKind};
use crate::span::Span;
use crate::types::interface::ValueKind;
use crate::types::ty::Type;

use super::wire::{
    DocPosition, clean_doc_comment, completion_item, doc_uri, qualifier_before, range_json,
    symbol_kind, uri_to_path, workspace_edit_json,
};
use super::workspace::Workspace;

/// The type of the narrowest name in `module` that contains the cursor.
fn type_at(module: &build::Module, line: i32, col: i32) -> Option<&Type> {
    module
        .checked
        .index
        .hovers
        .iter()
        .filter(|h| h.span.contains(line, col))
        .min_by_key(|h| h.span.width())
        .map(|h| &h.ty)
}

impl Workspace {
    /// Answers `textDocument/hover` with a markdown block for the symbol under the cursor, or
    /// `Json::Null`.
    pub fn hover_response(&mut self, params: &Json) -> Json {
        let Some(DocPosition { uri, line, col }) = self.resolve_pos(params) else {
            return Json::Null;
        };
        let Some(q) = self.query(&uri) else {
            return Json::Null;
        };
        // This has to come before the definition lookup, because an import path segment has no
        // `Definition`, and a qualifier resolves to the alias binding, whose doc is empty.
        if let Some(target) = q.graph.module_named_at(q.module, line, col) {
            let path = q
                .graph
                .module_path(target)
                .map_or(String::new(), |p| p.join("/"));
            let mut value = format!("```scarlet\nimport {path}\n```\n\n*module*");
            if let Some(d) = q.graph.module_doc(target) {
                value.push_str("\n\n---\n\n");
                value.push_str(&clean_doc_comment(d));
            }
            return json!({ "contents": { "kind": "markdown", "value": value } });
        }
        let Some(def) = q.graph.definition_at(q.module, line, col) else {
            return Json::Null;
        };
        // The graph carries no types, so the type comes from the check's hover table. When the
        // table has no type here, hover shows only the graph's identity.
        let ty = q
            .root
            .build
            .module(q.key)
            .and_then(|m| type_at(m, line, col));
        let signature = match ty {
            Some(ty) => format!("{} {}", def.name, labelled(ty, def.param_names())),
            None => def.name.clone(),
        };
        let mut value = format!(
            "```scarlet\n{}\n```\n\n*{}*",
            signature,
            def.entity().noun()
        );
        if let Some(d) = &def.doc {
            value.push_str("\n\n---\n\n");
            value.push_str(&clean_doc_comment(d));
        }
        json!({ "contents": { "kind": "markdown", "value": value } })
    }

    /// Answers `textDocument/completion`. After `alias::` it lists the imported module's public
    /// exports, and at a bare identifier the current module's top-level declarations. It always
    /// returns an array: a position it can't resolve gives an empty list, not null, so a client
    /// mid-keystroke sees "no items" instead of an error.
    pub fn completion_response(&mut self, params: &Json) -> Json {
        let empty = Json::Array(Vec::new());
        let Some(DocPosition { uri, line, col }) = self.resolve_pos(params) else {
            return empty;
        };
        // The completion prefix comes from the buffer, not the graph. A trailing `alias::` is a
        // parse error the recovering parser dropped, so there's no occurrence at the cursor to
        // resolve.
        let qualifier = self.document_text(&uri).and_then(|text| {
            let line_text = text.lines().nth(line as usize)?;
            qualifier_before(line_text, col as usize)
        });
        let Some(q) = self.query(&uri) else {
            return empty;
        };

        let mut items: Vec<Json> = match qualifier {
            Some(name) => {
                // Only a module alias can come before `::`.
                let Some(target) = q
                    .graph
                    .module_refs(q.module)
                    .map_or(&[][..], |mr| mr.defs_named(&name))
                    .iter()
                    .filter_map(|d| q.graph.definition(*d))
                    .find_map(reference::Definition::imports_module)
                else {
                    return empty;
                };
                let Some(iface) = q
                    .graph
                    .module_path(target)
                    .and_then(|p| q.root.build.module(&ModuleKey::of(p)))
                    .map(|m| &m.checked.interface)
                else {
                    return empty;
                };
                let values = iface.values.iter().map(|(name, v)| {
                    let entity = match v.kind {
                        ValueKind::Fn { .. } | ValueKind::Builtin { .. } => EntityKind::Function,
                        ValueKind::Const => EntityKind::Constant,
                        ValueKind::Constructor { .. } => EntityKind::Constructor,
                    };
                    let labels = v.kind.labels().unwrap_or(&[]);
                    let detail = labelled(&v.scheme.ty, labels);
                    completion_item(name, entity, Some(detail), v.doc.as_deref())
                });
                let types = iface
                    .types
                    .iter()
                    .filter(|(_, t)| t.public)
                    .map(|(name, t)| {
                        completion_item(name, EntityKind::Type, None, t.doc.as_deref())
                    });
                values.chain(types).collect()
            }
            // At a bare position, offer the module's own declarations: what documentSymbol
            // lists, plus module aliases, minus record fields, which are never bare names.
            None => {
                let module = q.root.build.module(q.key);
                q.graph
                    .defs_in(q.module)
                    .filter(|d| d.is_symbol_listable() && d.entity() != EntityKind::Field)
                    .map(|d| {
                        let s = d.span();
                        let detail = module
                            .and_then(|m| type_at(m, s.start_line, s.start_column))
                            .map(|ty| labelled(ty, d.param_names()));
                        completion_item(&d.name, d.entity(), detail, d.doc.as_deref())
                    })
                    .collect()
            }
        };
        items.sort_by(|a, b| a["label"].as_str().cmp(&b["label"].as_str()));
        Json::Array(items)
    }

    /// Answers `textDocument/definition` with the `{ uri, range }` of the definition under the
    /// cursor, or `Json::Null`.
    pub fn definition_response(&mut self, params: &Json) -> Json {
        let Some(DocPosition { uri, line, col }) = self.resolve_pos(params) else {
            return Json::Null;
        };
        let Some(q) = self.query(&uri) else {
            return Json::Null;
        };
        let near = uri_to_path(&uri);
        // A position naming a module jumps to that module's file at 0:0. A `scarlet/*` module
        // opens its source when the stdlib tree is on disk next to the open file, and gives null
        // when it's only embedded. Without this, the cursor would resolve to the alias binding,
        // which spans the whole `import` declaration, so the jump would go nowhere.
        if let Some(import_mod) = q.graph.module_named_at(q.module, line, col) {
            return match q.root.uri_for(import_mod, near.as_deref()) {
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
        if let Some(def) = q.graph.definition_at(q.module, line, col) {
            // Elsewhere in an `import` declaration there's nothing to go to, and `ModuleAlias`
            // spans the whole declaration, so jumping there would go nowhere.
            if !def.entity().is_navigable() {
                return Json::Null;
            }
            if let Some(u) = q.root.uri_for(def.defid.module, near.as_deref()) {
                return json!({ "uri": u, "range": range_json(&def.span()) });
            }
        }
        Json::Null
    }

    /// Answers `textDocument/references` with deduplicated `{ uri, range }`s for the definition
    /// under the cursor, or `Json::Null` when none resolves. A resolved definition with no
    /// references gives an empty array, not null.
    pub fn references_response(&mut self, params: &Json) -> Json {
        let include_decl = params
            .get("context")
            .and_then(|c| c.get("includeDeclaration"))
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let Some(DocPosition { uri, line, col }) = self.resolve_pos(params) else {
            return Json::Null;
        };
        let Some(q) = self.query(&uri) else {
            return Json::Null;
        };
        // The module name inside an `import` declaration resolves to a target that pairs the
        // imported module's id with a span in the *importing* file's coordinates. Looking that
        // up in the imported module's definitions uses the wrong key. It usually misses, and
        // when the spans happen to line up it returns another module's data. A qualifier use
        // (`b` of `b::add`) resolves through the importer's own alias definition, so it still
        // works.
        if q.graph.import_declaration_at(q.module, line, col) {
            return Json::Null;
        }
        let Some(def) = q.graph.definition_at(q.module, line, col) else {
            return Json::Null;
        };
        let near = uri_to_path(&uri);
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
        for rr in q.graph.references_to(def.defid) {
            // Only the `include_decl` branch below adds the declaration itself, so
            // `includeDeclaration = false` is honoured.
            if !rr.kind.is_reference_site() {
                continue;
            }
            if let Some(u) = q.root.uri_for(rr.module, near.as_deref()) {
                push(u, &rr.span, &mut out);
            }
        }
        if include_decl && let Some(u) = q.root.uri_for(def.defid.module, near.as_deref()) {
            push(u, &def.span(), &mut out);
        }
        Json::Array(out)
    }

    /// Answers `textDocument/rename` with a `WorkspaceEdit`, `Json::Null` when nothing resolves,
    /// or `Err` when the rename is refused. The caller turns `Err` into a JSON-RPC error.
    pub fn rename_response(
        &mut self,
        params: &Json,
    ) -> Result<Json, reference::rename::RenameError> {
        let new_name = params
            .get("newName")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let Some(DocPosition { uri, line, col }) = self.resolve_pos(params) else {
            return Ok(Json::Null);
        };
        let Some(q) = self.query(&uri) else {
            return Ok(Json::Null);
        };
        let Some(defid) = q.graph.def_id_at(q.module, line, col) else {
            return Ok(Json::Null);
        };
        let we = q
            .graph
            .rename(defid, &new_name, Some((q.module, uri.as_str())))?;
        Ok(workspace_edit_json(&we))
    }

    /// Answers `textDocument/prepareRename` with `{ range, placeholder }`, or `Json::Null` when
    /// the position can't be renamed.
    pub fn prepare_rename_response(&mut self, params: &Json) -> Json {
        let Some(DocPosition { uri, line, col }) = self.resolve_pos(params) else {
            return Json::Null;
        };
        let Some(q) = self.query(&uri) else {
            return Json::Null;
        };
        match q.graph.prepare_rename(q.module, line, col) {
            Ok(p) => json!({ "range": range_json(&p.range), "placeholder": p.placeholder }),
            Err(_) => Json::Null,
        }
    }

    /// Answers `textDocument/documentSymbol` with the module's top-level declarations, or
    /// `Json::Null` when the document has no checked module. The graph also holds locals and
    /// parameters, and `is_symbol_listable` keeps them out.
    pub fn document_symbol_response(&mut self, params: &Json) -> Json {
        let Some(uri) = doc_uri(params) else {
            return Json::Null;
        };
        let Some(q) = self.query(&uri) else {
            return Json::Null;
        };
        let syms: Vec<Json> = q
            .graph
            .defs_in(q.module)
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

    /// Answers `workspace/symbol` with every declaration matching the query, as
    /// `{ name, kind, location }`. It's always an array, and empty instead of null when nothing
    /// has been checked yet. Like `document_symbol_response`, it lists only declarations, and
    /// only those in a file.
    pub fn workspace_symbol_response(&mut self, params: &Json) -> Json {
        let query = params
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_lowercase();
        let mut seen: std::collections::BTreeSet<(String, i32, i32)> =
            std::collections::BTreeSet::new();
        let mut out: Vec<Json> = Vec::new();
        for root in self.all_roots() {
            for d in root.graph().all_defs() {
                if !d.is_symbol_listable() {
                    continue;
                }
                if !query.is_empty() && !d.name.to_lowercase().contains(&query) {
                    continue;
                }
                let Some(u) = root.uri_for(d.defid.module, None) else {
                    continue;
                };
                let s = d.span();
                if seen.insert((u.clone(), s.start_line, s.start_column)) {
                    out.push(json!({
                        "name": d.name,
                        "kind": symbol_kind(d.entity()),
                        "location": { "uri": u, "range": range_json(&s) },
                    }));
                }
            }
        }
        Json::Array(out)
    }
}

/// A function type rendered with its parameter names: `fn(path String) Result(…)`.
///
/// Names live beside the type, not in it, because a parameter's name isn't part of a function's
/// identity. When the arity disagrees, this falls back to plain `Display`.
fn labelled<S: AsRef<str>>(ty: &Type, names: &[S]) -> String {
    let Type::Fn { params, ret } = ty else {
        return ty.to_string();
    };
    if names.len() != params.len() {
        return ty.to_string();
    }
    let ps: Vec<String> = names
        .iter()
        .zip(params.iter())
        .map(|(n, t)| format!("{} {t}", n.as_ref()))
        .collect();
    format!("fn({}) {}", ps.join(", "), ret)
}
