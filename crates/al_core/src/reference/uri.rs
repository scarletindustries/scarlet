//! `file://` URI translation for the reference graph. Round-trips with the
//! LSP's `uri_to_path`.

use std::fmt;
use std::path::Path;

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

use crate::module::{ModuleSource, ResolveError, resolve_canonical};

use super::{ModuleId, ReferenceGraph};

/// Must match VS Code's `URI.file(path).toString()` byte for byte, or a
/// graph-derived URI will not compare equal to the client's key for that file.
const URI_PATH: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~')
    .remove(b'/');

/// Translate an absolute file path to a `file://` URI.
pub fn path_to_uri(p: &Path) -> String {
    format!(
        "file://{}",
        utf8_percent_encode(&p.to_string_lossy(), URI_PATH)
    )
}

/// Why a [`ModuleId`] has no `file://` URI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModuleUriError {
    /// The precompiled stdlib — embedded source, no file on disk.
    Embedded,
    /// The id was never interned into this graph (no path to resolve).
    NoPath,
    /// No source file found. The bare entry module lands here; its caller
    /// supplies the URI through the `entry` override instead.
    Resolve(ResolveError),
}

impl fmt::Display for ModuleUriError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ModuleUriError::Embedded => write!(f, "precompiled stdlib module"),
            ModuleUriError::NoPath => write!(f, "module is not in the reference graph"),
            ModuleUriError::Resolve(e) => write!(f, "{e}"),
        }
    }
}

/// Map a [`ModuleId`] to a file URI. Graph module paths are always canonical,
/// so no base directory is involved.
pub fn module_uri(graph: &ReferenceGraph, module: ModuleId) -> Result<String, ModuleUriError> {
    let path = graph.module_path(module).ok_or(ModuleUriError::NoPath)?;
    match resolve_canonical(path) {
        Ok(r) => match r.source {
            ModuleSource::File(p) => Ok(path_to_uri(&p)),
            ModuleSource::Embedded(_) => Err(ModuleUriError::Embedded),
        },
        Err(e) => Err(ModuleUriError::Resolve(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reference::{ReferenceGraphBuilder, mp};

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
        assert_eq!(
            path_to_uri(Path::new("/a+b/c:d(e)'!.al")),
            "file:///a%2Bb/c%3Ad%28e%29%27%21.al"
        );
    }

    #[test]
    fn module_uri_stdlib_is_embedded_error() {
        let mut g = ReferenceGraphBuilder::new();
        let std_mod = g.intern_module(&mp(&["al", "array"]));
        let bare = g.intern_module(&mp(&["whatever"]));
        let g = g.finish();
        assert_eq!(module_uri(&g, std_mod), Err(ModuleUriError::Embedded));
        assert!(matches!(
            module_uri(&g, bare),
            Err(ModuleUriError::Resolve(_))
        ));
        assert_eq!(module_uri(&g, ModuleId(99)), Err(ModuleUriError::NoPath));
    }

    #[test]
    fn module_uri_resolves_canonical_file() {
        let dir = std::env::temp_dir().join(format!("al_uri_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("helpers.al");
        std::fs::write(&file, "pub fn x() { 1 }\n").expect("write temp module");

        let mut g = ReferenceGraphBuilder::new();
        let canon = crate::module::file_module_path(&file).unwrap();
        let m = g.intern_module(&canon);
        let g = g.finish();
        let uri = module_uri(&g, m).expect("canonical module resolves");
        assert!(uri.starts_with("file://"));
        assert!(uri.ends_with("helpers.al"));
        assert_eq!(uri, path_to_uri(&std::path::absolute(&file).unwrap()));

        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_dir(&dir);
    }
}
