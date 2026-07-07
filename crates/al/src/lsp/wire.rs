//! LSP wire helpers: JSON shaping, URI/path translation, protocol constants.
//! Nothing here holds state; every function is a pure mapping between the
//! compiler's domain types and the JSON-RPC wire format.

use std::path::{Path, PathBuf};

use serde_json::{Value as Json, json};

use crate::diagnostic;
use crate::module::{self, ModulePath};
use crate::reference;
use crate::span::Span;

/// LSP `FileChangeType` (didChangeWatchedFiles). The wire encodes these as
/// bare integers; parsing them into a closed enum here means the invalidation
/// logic matches on names, not `== 3` sprinkled at call sites.
#[repr(i64)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum FileChangeType {
    Created = 1,
    Changed = 2,
    Deleted = 3,
}

impl FileChangeType {
    pub(super) fn from_wire(n: i64) -> Self {
        match n {
            1 => Self::Created,
            3 => Self::Deleted,
            _ => Self::Changed,
        }
    }
}

pub(super) fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let decoded = percent_encoding::percent_decode_str(rest)
        .decode_utf8()
        .ok()?;
    Some(PathBuf::from(&*decoded))
}

/// Pick the workspace root that owns `path`. With nested roots (rare but
/// permitted by LSP) the deepest one wins so a sub-project gets its own
/// session. Files outside every root share the empty-path session.
pub(super) fn root_for(roots: &[PathBuf], path: &Path) -> PathBuf {
    roots
        .iter()
        .filter(|r| path.starts_with(r))
        .max_by_key(|r| r.as_os_str().len())
        .cloned()
        .unwrap_or_default()
}

pub(super) fn doc_uri(params: &Json) -> Option<String> {
    Some(
        params
            .get("textDocument")?
            .get("uri")?
            .as_str()?
            .to_string(),
    )
}

pub(super) fn extract_position_params(params: &Json) -> Option<(String, i32, i32)> {
    let uri = doc_uri(params)?;
    let pos = params.get("position")?;
    let line = pos.get("line")?.as_i64()? as i32;
    let col = pos.get("character")?.as_i64()? as i32;
    Some((uri, line, col))
}

/// LSP `SymbolKind` wire number for an [`EntityKind`]. Lives here rather than
/// on `EntityKind` because `al_core` is protocol-agnostic.
pub(super) fn symbol_kind(e: reference::EntityKind) -> i32 {
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

/// JSON-RPC error code for a refused rename: `InvalidParams` (-32602) for a
/// bad `newName`, `RequestFailed` (-32803) for a resolvable-but-refused
/// position. Lives here rather than on `RenameError` because `al_core` is
/// protocol-agnostic.
pub(super) fn rename_error_code(e: &reference::rename::RenameError) -> i32 {
    use reference::rename::RenameError;
    match e {
        RenameError::InvalidName(_) => -32602,
        RenameError::NotFound | RenameError::NotRenameable(_) | RenameError::Unresolvable(_) => {
            -32803
        }
    }
}

pub(super) fn diagnostic_to_json(diag: &diagnostic::Diagnostic) -> Json {
    let severity = match diag.severity {
        diagnostic::Severity::Error => 1,
        diagnostic::Severity::Hint => 4,
    };
    json!({
        "range": range_json(&diag.span),
        "severity": severity,
        "message": diag.message,
    })
}

/// The module a queried file's defs/occurrences are keyed under in the graph:
/// its stdlib path when it is an in-repo stdlib file, otherwise the entry
/// module (`main`) it was last analysed as.
pub(super) fn query_module(uri: &str) -> Option<ModulePath> {
    let p = uri_to_path(uri)?;
    Some(module::detect_stdlib_module(&p).unwrap_or_else(module::main_module))
}

/// The id of the module imported by the `import a/b` declaration whose final
/// module-name segment the cursor sits on, or `None` when it is anywhere else.
/// The `Import` occurrence (recorded at that segment) carries the imported
/// module as its target's owning module; the tightest covering occurrence
/// wins, mirroring `ReferenceGraph::resolve_position`.
pub(super) fn import_target_at(
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
/// goes through `module::resolve` via `reference::module_uri`.
pub(super) fn uri_for(
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
            return Some(reference::path_to_uri(&p));
        }
        return None;
    }
    reference::module_uri(graph, module, req_path.parent())
}

/// Shape a pure `WorkspaceEdit` (computed in `al_core`, which has no
/// `serde_json`) into the LSP wire JSON.
pub(super) fn workspace_edit_json(we: &reference::rename::WorkspaceEdit) -> Json {
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
pub(super) fn stdlib_file(m: &ModulePath, near: &Path) -> Option<PathBuf> {
    let mut p = module::find_stdlib_root(near)?;
    for seg in m {
        p.push(seg);
    }
    p.set_extension("al");
    p.is_file().then_some(p)
}

pub(super) fn range_json(span: &Span) -> Json {
    json!({
        "start": { "line": span.start_line, "character": span.start_column },
        "end": { "line": span.end_line, "character": span.end_column },
    })
}

pub(super) fn clean_doc_comment(doc: &str) -> String {
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
