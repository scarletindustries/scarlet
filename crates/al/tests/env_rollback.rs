//! Regression: an incremental `IncrementalSession` must not let a shadowing
//! selective import permanently corrupt the prelude for the rest of the
//! session. A selective import of a shadowable prelude name (e.g.
//! `import ./lib.{println}`) used to overwrite the prelude binding in place in
//! the persistent root scope; the length-based `truncate_to` rollback could
//! never restore it, so a *later* check that relied on the prelude binding saw
//! the imported one instead.

use al::bytecode::IncrementalSession;

mod common;
use common::{Project, parse};

#[test]
fn shadowing_import_does_not_corrupt_prelude_across_checks() {
    let p = Project::new("envrollback");
    // A local module that defines its own `println` with a DIFFERENT, more
    // restrictive signature than the prelude's `fn(a) Nil` — it only accepts an
    // `Int`.
    p.write("lib.al", "pub fn println(_x Int) Nil { Nil }\n");

    let mut s = IncrementalSession::new(al::stdlib());

    // Check 1: entry selectively imports lib's `println`, shadowing the prelude
    // binding. This overwrites the prelude `println` entry in place in the
    // persistent root scope.
    let entry1 = "import ./lib.{println}\nprintln(1)\n";
    let r1 = s.check(&parse(entry1), Some(&p.dir));
    assert!(r1.success, "check 1 should succeed: {:?}", r1.diagnostics);

    // Check 2: entry no longer imports `println`; it uses the *prelude*
    // `println`, which accepts any type. The rollback must have restored the
    // prelude binding, so calling it with a String must type-check.
    let entry2 = "println(\"hello\")\n";
    let r2 = s.check(&parse(entry2), Some(&p.dir));
    assert!(
        r2.success,
        "prelude `println` must accept a String after a prior shadowing import \
         was rolled back; got diagnostics: {:?}",
        r2.diagnostics
    );
}
