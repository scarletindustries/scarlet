//! LSP wire helpers: JSON shaping, URI/path translation, protocol constants.

use std::path::{Path, PathBuf};

use serde_json::{Value as Json, json};

use crate::diagnostic;
use crate::module::{self, ModulePath};
use crate::reference;
use crate::span::Span;

/// LSP `FileChangeType` (didChangeWatchedFiles), sent on the wire as an int.
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

/// Decode an LSP `WorkspaceFolder[]` into filesystem paths, skipping malformed
/// or non-`file://` entries.
pub(super) fn folder_paths(v: Option<&Json>) -> Vec<PathBuf> {
    v.and_then(Json::as_array)
        .into_iter()
        .flatten()
        .filter_map(|f| f.get("uri")?.as_str().and_then(uri_to_path))
        .collect()
}

/// Pick the workspace root that owns `path`. LSP permits nested roots; the
/// deepest wins. Files outside every root share the empty-path session.
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

/// LSP `SymbolKind` wire number for an [`EntityKind`]. Here, not on
/// `EntityKind`, because `al_core` is protocol-agnostic.
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

/// JSON-RPC error code for a refused rename. Here, not on `RenameError`,
/// because `al_core` is protocol-agnostic.
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
/// its stdlib path for an in-repo stdlib file, else the entry module `main`.
pub(super) fn query_module(uri: &str) -> Option<ModulePath> {
    let p = uri_to_path(uri)?;
    Some(module::detect_stdlib_module(&p).unwrap_or_else(module::main_module))
}

/// Translate a graph `ModuleId` back to a file URI, relative to the requesting
/// file. The requesting file's own module must short-circuit to the request
/// URI: the entry module is bare, so `resolve` cannot locate it.
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
    reference::module_uri(graph, module).ok()
}

/// Shape a `WorkspaceEdit` into the LSP wire JSON.
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

/// Strip a `/** … */` block's delimiters and leading `*` gutter, keeping the
/// author's line structure. Lines join with a single `\n`, not a blank line:
/// markdown already folds soft-wrapped lines into one paragraph, and blank
/// lines would shred indented code blocks.
pub(super) fn clean_doc_comment(doc: &str) -> String {
    let doc = doc.trim();
    let mut content = doc.strip_prefix("/*").unwrap_or(doc);
    if let Some(rest) = content.strip_suffix("*/") {
        // A `**/` closer leaves its gutter `*` behind. Strip only the run
        // abutting the delimiter, so a markdown `***` line survives.
        content = rest.trim_end_matches('*');
    }
    let content = content.trim();

    let mut cleaned: Vec<String> = Vec::new();
    for line in content.split('\n') {
        // Trim only the gutter; indentation after `* ` is the author's code block.
        let gutter = line.trim_start_matches([' ', '\t']);
        let rest = match gutter.strip_prefix('*') {
            Some(r) => r.strip_prefix(' ').unwrap_or(r),
            None => gutter,
        };
        cleaned.push(rest.trim_end().to_string());
    }
    while cleaned.last().is_some_and(String::is_empty) {
        cleaned.pop();
    }
    let start = cleaned.iter().position(|l| !l.is_empty()).unwrap_or(0);
    cleaned[start..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::clean_doc_comment;

    #[test]
    fn soft_wrapped_lines_stay_one_paragraph() {
        let got = clean_doc_comment("/**\n * One line\n * and its continuation.\n */");
        assert_eq!(got, "One line\nand its continuation.");
    }

    #[test]
    fn a_blank_line_separates_paragraphs() {
        let got = clean_doc_comment("/**\n * Title.\n *\n * Body.\n */");
        assert_eq!(got, "Title.\n\nBody.");
    }

    #[test]
    fn indented_code_block_survives() {
        let got = clean_doc_comment("/**\n * Example:\n *\n *     new(15, 1)\n */");
        assert_eq!(got, "Example:\n\n    new(15, 1)");
    }

    #[test]
    fn single_line_doc_is_unchanged() {
        assert_eq!(
            clean_doc_comment("/** Sum of `a` and `b`. */"),
            "Sum of `a` and `b`."
        );
    }

    #[test]
    fn double_star_closer_leaves_no_stray_gutter() {
        assert_eq!(clean_doc_comment("/** Doc. **/"), "Doc.");
        assert_eq!(clean_doc_comment("/**\n * Doc.\n **/"), "Doc.");
    }

    #[test]
    fn trailing_markdown_rule_survives() {
        assert_eq!(clean_doc_comment("/**\n * a\n * ***\n*/"), "a\n***");
    }
}
