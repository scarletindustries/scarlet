use include_dir::{Dir, include_dir};

static STD: Dir = include_dir!("$CARGO_MANIFEST_DIR/src/std");

pub fn lookup(path: &str) -> Option<&'static str> {
    STD.get_file(format!("{path}.al"))
        .and_then(|f| f.contents_utf8())
}

/// Every stdlib module path other than the prelude itself, sorted for
/// deterministic precompilation order.
pub fn all_modules() -> Vec<crate::module::ModulePath> {
    let mut out: Vec<_> = STD
        .find("al/**/*.al")
        .into_iter()
        .flatten()
        .filter_map(|e| {
            let p = e.path().with_extension("");
            let segs: Vec<String> = p
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            (!segs.is_empty()).then_some(segs)
        })
        .collect();
    out.sort();
    out
}
