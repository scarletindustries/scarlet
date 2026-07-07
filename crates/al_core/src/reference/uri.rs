//! `file://` URI translation for the reference graph. Maps a filesystem
//! [`Path`] to the percent-encoded URI the LSP wire uses (round-trips with the
//! LSP's `uri_to_path`), and a [`ModuleId`] to a URI via `module::resolve`.
//! Separate from `rename` because every LSP handler that returns a location
//! (goto-def, find-references, workspace-index) needs this, not just rename.

use std::path::Path;

use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};

use crate::module::{ModuleSource, resolve};

use super::{ModuleId, ReferenceGraph};

/// Characters that must be percent-encoded in the path component of a
/// `file://` URI (RFC 3986 non-`pchar`). Leaves `/` and the unreserved set
/// alone so ordinary repo paths encode to themselves.
const URI_PATH: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}');

/// Translate an absolute file path to a `file://` URI. Round-trips with the
/// LSP's `uri_to_path`: the path component is percent-encoded so a workspace
/// under `/My Proj/` produces the same key the client sends for it.
pub fn path_to_uri(p: &Path) -> String {
    format!(
        "file://{}",
        utf8_percent_encode(&p.to_string_lossy(), URI_PATH)
    )
}

/// Map a [`ModuleId`] to a file URI via [`crate::module::resolve`] (the
/// spec-mandated translation). Returns `None` for the precompiled stdlib
/// (`Embedded`, not editable) and for paths that don't resolve from
/// `base_dir` (e.g. the bare entry module — the caller supplies that URI
/// directly via the `entry` override).
pub fn module_uri(
    graph: &ReferenceGraph,
    module: ModuleId,
    base_dir: Option<&Path>,
) -> Option<String> {
    let path = graph.module_path(module)?;
    match resolve(path, base_dir) {
        Ok(ModuleSource::File(p)) => Some(path_to_uri(&p)),
        Ok(ModuleSource::Embedded(_)) | Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reference::mp;

    #[test]
    fn path_to_uri_percent_encodes() {
        assert_eq!(
            path_to_uri(Path::new("/My Proj/foo.al")),
            "file:///My%20Proj/foo.al"
        );
        assert_eq!(
            path_to_uri(Path::new("/plain/foo.al")),
            "file:///plain/foo.al"
        );
    }

    #[test]
    fn module_uri_stdlib_is_embedded_none() {
        let mut g = ReferenceGraph::new();
        let std_mod = g.intern_module(&mp(&["al", "array"]));
        // al/array resolves to embedded stdlib source -> not an editable file.
        assert_eq!(module_uri(&g, std_mod, None), None);
        // a bare non-al module is reserved/unresolvable -> None.
        let bare = g.intern_module(&mp(&["whatever"]));
        assert_eq!(module_uri(&g, bare, None), None);
    }

    #[test]
    fn module_uri_resolves_relative_file() {
        let dir = std::env::temp_dir().join(format!("al_uri_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("helpers.al");
        std::fs::write(&file, "pub fn x() { 1 }\n").expect("write temp module");

        let mut g = ReferenceGraph::new();
        // a `./helpers` relative import resolves against base_dir.
        let m = g.intern_module(&mp(&[".", "helpers"]));
        let uri = module_uri(&g, m, Some(&dir)).expect("relative module resolves");
        assert!(uri.starts_with("file://"));
        assert!(uri.ends_with("helpers.al"));
        assert_eq!(uri, path_to_uri(&file));

        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_dir(&dir);
    }
}
