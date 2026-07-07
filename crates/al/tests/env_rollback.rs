//! Regression: an incremental `IncrementalSession` must not let a shadowing
//! selective import permanently corrupt the prelude for the rest of the
//! session. A selective import of a shadowable prelude name (e.g.
//! `import ./lib.{println}`) used to overwrite the prelude binding in place in
//! the persistent root scope; the length-based `truncate_to` rollback could
//! never restore it, so a *later* check that relied on the prelude binding saw
//! the imported one instead.

mod common;
use common::{Project, checked_with, recheck};

#[test]
fn shadowing_import_does_not_corrupt_prelude_across_checks() {
    let p = Project::new("envrollback");
    // A local module that defines its own `println` with a DIFFERENT, more
    // restrictive signature than the prelude's `fn(a) Nil` — it only accepts an
    // `Int`.
    p.write("lib.al", "pub fn println(_x Int) Nil { Nil }\n");

    // Check 1: entry selectively imports lib's `println`, shadowing the prelude
    // binding. This overwrites the prelude `println` entry in place in the
    // persistent root scope.
    let mut s = checked_with(&p, "import ./lib.{println}\nprintln(1)\n");

    // Check 2: entry no longer imports `println`; it uses the *prelude*
    // `println`, which accepts any type. The rollback must have restored the
    // prelude binding, so calling it with a String must type-check.
    let r2 = recheck(&mut s, &p, "println(\"hello\")\n");
    assert!(
        r2.success,
        "prelude `println` must accept a String after a prior shadowing import \
         was rolled back; got diagnostics: {:?}",
        r2.diagnostics
    );
}
