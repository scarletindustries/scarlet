//! Regression: an `IncrementalSession` must roll back the entry file's
//! selective-import bindings between checks. `env.type_info` is a flat map (not
//! a scope stack), so a `import m.{Type}` binding written by `process_imports`
//! used to be captured in the `last_entry` watermark; the next check's
//! `reset_to` then preserved it, so removing or renaming the import left the old
//! name still resolving to a stale `TypeInfo` with no diagnostic.

use al::bytecode::IncrementalSession;

mod common;
use common::{Project, parse};

#[test]
fn removed_selective_type_import_stops_resolving() {
    let p = Project::new("importrollback");
    p.write("lib.al", "pub type Color { Color }\n");

    let mut s = IncrementalSession::new(al::stdlib());

    // Check 1: the entry selectively imports `Color` and annotates with it.
    let entry1 = "import ./lib.{Color}\nfn paint(_c Color) Int { 1 }\n_x = paint\n";
    let r1 = s.check(&parse(entry1), Some(&p.dir));
    assert!(r1.success, "check 1 should succeed: {:?}", r1.diagnostics);

    // Check 2: the import is gone but the annotation `Color` remains. The
    // binding must have been rolled back, so this is now an unknown type.
    let entry2 = "fn paint(_c Color) Int { 1 }\n_x = paint\n";
    let r2 = s.check(&parse(entry2), Some(&p.dir));
    assert!(
        !r2.success,
        "removed selective type import `Color` must stop resolving; got no \
         diagnostics: {:?}",
        r2.diagnostics
    );
    assert!(
        r2.diagnostics
            .iter()
            .any(|d| d.message.contains("Unknown type 'Color'")),
        "expected an `Unknown type 'Color'` diagnostic, got: {:?}",
        r2.diagnostics
    );
}

#[test]
fn removed_aliased_type_import_stops_resolving() {
    let p = Project::new("importrollbackalias");
    p.write("lib.al", "pub type Color { Color }\n");

    let mut s = IncrementalSession::new(al::stdlib());

    // The local name `Hue` is the import alias of `Color`.
    let entry1 = "import ./lib.{Color as Hue}\nfn paint(_c Hue) Int { 1 }\n_x = paint\n";
    let r1 = s.check(&parse(entry1), Some(&p.dir));
    assert!(r1.success, "check 1 should succeed: {:?}", r1.diagnostics);

    let entry2 = "fn paint(_c Hue) Int { 1 }\n_x = paint\n";
    let r2 = s.check(&parse(entry2), Some(&p.dir));
    assert!(
        !r2.success,
        "removed aliased type import `Color as Hue` must stop resolving; got no \
         diagnostics: {:?}",
        r2.diagnostics
    );
}

#[test]
fn kept_selective_type_import_keeps_resolving_across_checks() {
    // The rollback drops the entry's imported `Color` between checks, so each
    // check must re-bind it from the persistent cached module interface. A check
    // that keeps the import must keep type-checking — repeatedly.
    let p = Project::new("importrollbackkept");
    p.write("lib.al", "pub type Color { Color }\n");

    let mut s = IncrementalSession::new(al::stdlib());

    let entry = "import ./lib.{Color}\nfn paint(_c Color) Int { 1 }\n_x = paint\n";
    for i in 0..3 {
        let r = s.check(&parse(entry), Some(&p.dir));
        assert!(r.success, "check {i} (kept import): {:?}", r.diagnostics);
    }
}
