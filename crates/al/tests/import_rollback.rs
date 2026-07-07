//! Regression: an `IncrementalSession` must roll back the entry file's
//! selective-import bindings between checks. `env.type_info` is a flat map (not
//! a scope stack), so a `import m.{Type}` binding written by `process_imports`
//! used to be captured in the `last_entry` watermark; the next check's
//! `reset_to` then preserved it, so removing or renaming the import left the old
//! name still resolving to a stale `TypeInfo` with no diagnostic.

mod common;
use common::{Project, checked_with, recheck};

/// Shared rollback check: `lib.al` exports `Color`, `entry1` imports and uses
/// it (must type-check), then `entry2` drops the import but keeps the use
/// (must fail, with an `expected_diag` diagnostic when one is given).
fn assert_import_rolled_back(tag: &str, entry1: &str, entry2: &str, expected_diag: Option<&str>) {
    let p = Project::new(tag);
    p.write("lib.al", "pub type Color { Color }\n");

    let mut s = checked_with(&p, entry1);

    let r2 = recheck(&mut s, &p, entry2);
    assert!(!r2.success, "import still resolves: {:?}", r2.diagnostics);
    if let Some(diag) = expected_diag {
        let found = r2.diagnostics.iter().any(|d| d.message.contains(diag));
        assert!(found, "missing {diag:?}: {:?}", r2.diagnostics);
    }
}

#[test]
fn removed_selective_type_import_stops_resolving() {
    assert_import_rolled_back(
        "importrollback",
        "import ./lib.{Color}\nfn paint(_c Color) Int { 1 }\n_x = paint\n",
        "fn paint(_c Color) Int { 1 }\n_x = paint\n",
        Some("Unknown type 'Color'"),
    );
}

#[test]
fn removed_aliased_type_import_stops_resolving() {
    // The local name `Hue` is the import alias of `Color`.
    assert_import_rolled_back(
        "importrollbackalias",
        "import ./lib.{Color as Hue}\nfn paint(_c Hue) Int { 1 }\n_x = paint\n",
        "fn paint(_c Hue) Int { 1 }\n_x = paint\n",
        None,
    );
}

#[test]
fn kept_selective_type_import_keeps_resolving_across_checks() {
    // The rollback drops the entry's imported `Color` between checks, so each
    // check must re-bind it from the persistent cached module interface. A check
    // that keeps the import must keep type-checking — repeatedly.
    let p = Project::new("importrollbackkept");
    p.write("lib.al", "pub type Color { Color }\n");

    let entry = "import ./lib.{Color}\nfn paint(_c Color) Int { 1 }\n_x = paint\n";
    let mut s = checked_with(&p, entry);
    for i in 1..3 {
        let r = recheck(&mut s, &p, entry);
        assert!(r.success, "check {i} (kept import): {:?}", r.diagnostics);
    }
}
