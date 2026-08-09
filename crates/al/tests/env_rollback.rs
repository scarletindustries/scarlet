//! Regression: in an `IncrementalSession`, a selective import that shadows a
//! prelude name must not survive the rollback into a later check.

mod common;
use common::{Project, checked_with, recheck};

#[test]
fn shadowing_import_does_not_corrupt_prelude_across_checks() {
    let p = Project::new("envrollback");
    // Narrower than the prelude's `fn(a) Nil`: Int only.
    p.write("lib.al", "pub fn println(_x Int) Nil { Nil }\n");

    let mut s = checked_with(&p, "import ./lib.{println}\nprintln(1)\n");

    // No import this time, so this must resolve to the prelude `println`.
    let r2 = recheck(&mut s, &p, "println(\"hello\")\n");
    assert!(
        r2.success(),
        "prelude `println` must accept a String after a prior shadowing import \
         was rolled back; got diagnostics: {:?}",
        r2.diagnostics
    );
}
