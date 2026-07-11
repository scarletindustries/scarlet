use include_dir::{Dir, include_dir};

static STD: Dir = include_dir!("$CARGO_MANIFEST_DIR/src/std");

pub fn lookup(path: &str) -> Option<&'static str> {
    STD.get_file(format!("{path}.al"))
        .and_then(|f| f.contents_utf8())
}

/// Every stdlib module path other than the prelude itself, sorted for
/// deterministic precompilation order. Runs only at build time (via
/// `precompile_stdlib` in `build.rs`), so a bad glob is a loud build failure
/// rather than a silently empty stdlib.
#[allow(clippy::expect_used)] // build-time only: a bad glob must fail the build
pub fn all_modules() -> Vec<crate::module::ModulePath> {
    let mut out: Vec<_> = STD
        .find("al/**/*.al")
        .expect("stdlib glob pattern is valid")
        .map(|e| {
            let p = e.path().with_extension("");
            p.components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect()
        })
        .collect();
    out.sort();
    out
}
