//! `file://` URI translation for the reference graph. Round-trips with the
//! LSP's `uri_to_path`.

use std::fmt;
use std::path::Path;

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

use crate::module::{ModuleSource, ResolveError, resolve_canonical};

use super::{ModuleId, ReferenceGraph};

/// This must match VS Code's `URI.file(path).toString()` byte for byte, or a
/// URI built from the graph will not equal the client's key for that file.
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
    /// A stdlib module. Its source is embedded, with no file on disk.
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

/// Map a [`ModuleId`] to a file URI. Module paths in the graph are always
/// canonical, so this needs no base directory.
pub(crate) fn module_uri(
    graph: &ReferenceGraph,
    module: ModuleId,
) -> Result<String, ModuleUriError> {
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
mod tests;
