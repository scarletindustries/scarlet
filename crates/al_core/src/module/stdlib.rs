use include_dir::{Dir, include_dir};

static STD: Dir = include_dir!("$CARGO_MANIFEST_DIR/src/std");

pub fn lookup(path: &str) -> Option<&'static str> {
    STD.get_file(format!("{path}.al"))
        .and_then(|f| f.contents_utf8())
}

/// Every stdlib module path other than the prelude itself, sorted for
/// deterministic precompilation order. Runs only at build time (via
/// `precompile_stdlib` in `build.rs`), so a bad glob surfaces as a loud
/// build failure through `precompile_stdlib`'s `Result` rather than a
/// silently empty stdlib.
pub fn all_modules() -> Result<Vec<crate::module::ModulePath>, String> {
    let mut out: Vec<_> = STD
        .find("al/**/*.al")
        .map_err(|e| format!("stdlib glob pattern is invalid: {e}"))?
        .map(|e| {
            let p = e.path().with_extension("");
            p.components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect()
        })
        .collect();
    out.sort();
    Ok(out)
}
