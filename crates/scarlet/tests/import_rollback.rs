//! Regression: an `IncrementalSession` must roll back the types the entry
//! file's imports register (`lib.Color` for `import ./lib`) between checks.
//! `env.type_info` is a flat map, so a binding left above the `last_entry`
//! watermark keeps resolving after the import is removed or renamed.

mod common;
use common::{Project, checked_with, recheck};

/// `entry1` must check clean; `entry2` must then fail with `expected_diag`.
fn assert_import_rolled_back(tag: &str, entry1: &str, entry2: &str, expected_diag: &str) {
    let p = Project::new(tag);
    p.write("lib.scrl", "pub type Color { Color }\n");

    let mut s = checked_with(&p, entry1);

    let r2 = recheck(&mut s, &p, entry2);
    assert!(!r2.success(), "import still resolves: {:?}", r2.diagnostics);
    let found = r2
        .diagnostics
        .iter()
        .any(|d| d.message.contains(expected_diag));
    assert!(found, "missing {expected_diag:?}: {:?}", r2.diagnostics);
}

#[test]
fn removed_import_stops_its_types_resolving() {
    assert_import_rolled_back(
        "importrollback",
        "import ./lib\npub fn paint(_c lib.Color) Int { 1 }\n",
        "pub fn paint(_c lib.Color) Int { 1 }\n",
        "Unknown type 'lib.Color'",
    );
}

#[test]
fn renamed_import_stops_its_old_types_resolving() {
    assert_import_rolled_back(
        "importrollbackalias",
        "import ./lib as hue\npub fn paint(_c hue.Color) Int { 1 }\n",
        "import ./lib as tint\npub fn paint(_c hue.Color) Int { 1 }\n",
        "Unknown type 'hue.Color'",
    );
}

#[test]
fn kept_import_keeps_its_types_resolving_across_checks() {
    // The rollback drops `lib.Color` between checks, so each check must
    // re-bind it from the cached module interface.
    let p = Project::new("importrollbackkept");
    p.write("lib.scrl", "pub type Color { Color }\n");

    let entry = "import ./lib\npub fn paint(_c lib.Color) Int { 1 }\n";
    let mut s = checked_with(&p, entry);
    for i in 1..3 {
        let r = recheck(&mut s, &p, entry);
        assert!(r.success(), "check {i} (kept import): {:?}", r.diagnostics);
    }
}
