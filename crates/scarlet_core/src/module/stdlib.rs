use include_dir::{Dir, include_dir};

static STD: Dir = include_dir!("$CARGO_MANIFEST_DIR/src/std");

pub(crate) fn lookup(path: &str) -> Option<&'static str> {
    STD.get_file(format!("{path}.scrl"))
        .and_then(|f| f.contents_utf8())
}

/// Every module the embedded stdlib holds, by the path an import names it
/// with: `scarlet`, `scarlet/string`, `scarlet/net/tls`, ...
pub(crate) fn module_paths() -> Vec<super::ModulePath> {
    let mut out: Vec<super::ModulePath> = STD
        .find("**/*.scrl")
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.as_file())
        .filter_map(|file| {
            let path = file.path().to_str()?.strip_suffix(".scrl")?;
            Some(path.split('/').map(str::to_string).collect())
        })
        .collect();
    out.sort();
    out
}
