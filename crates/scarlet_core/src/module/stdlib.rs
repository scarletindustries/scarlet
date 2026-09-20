use include_dir::{Dir, include_dir};

static STD: Dir = include_dir!("$CARGO_MANIFEST_DIR/src/std");

pub(crate) fn lookup(path: &str) -> Option<&'static str> {
    STD.get_file(format!("{path}.scrl"))
        .and_then(|f| f.contents_utf8())
}
