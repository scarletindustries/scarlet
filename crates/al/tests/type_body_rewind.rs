//! Regression: a cached file-module's `TypeInfo` bodies (variant field
//! templates, alias targets) outlive an incremental rewind, but the engine's
//! `vars` table does not — `InferEngine::truncate_to` clears it wholesale.
//! Bodies stored in open `Var(param_id)` form therefore dangled: the next
//! check's `substitute_type_vars` chased a cleared (or re-minted) var slot
//! and panicked or resolved to garbage. Bodies are now closed to `Bound(idx)`
//! at registration (`close_body` in `bytecode/analysis.rs`), exactly as the
//! precompiled stdlib's are.

use al::bytecode::IncrementalSession;

mod common;
use common::{Project, parse};

#[test]
fn cached_module_type_bodies_survive_rewinds() {
    let p = Project::new("type_body_rewind");
    p.write(
        "lib.al",
        "pub type Box(a) {\n\tMk(v a)\n}\npub type Pair(a) = (a, a)\npub fn wrap(x Int) Box(Int) { Mk(x) }\n",
    );

    // Exercises every consumer of the cached bodies: the alias target
    // (annotation hydration), the variant field template (field access), and
    // exhaustiveness/pattern elaboration (the single-ctor match).
    let entry = |k: i64| {
        format!(
            "import ./lib.{{Box, Pair, Mk, wrap}}\n\n\
             fn get(b Box(Int)) Int {{\n\tmatch b {{\n\t\tMk(v) -> v\n\t}}\n}}\n\
             fn first(p Pair(Int)) Int {{ p.0 }}\n\
             fn peek(b Box(Int)) Int {{ b.v }}\n\
             println(get(wrap({k})) + first((2, 3)) + peek(wrap({k})))\n"
        )
    };

    let mut s = IncrementalSession::new(&al::STDLIB);
    for i in 0..4 {
        // Edit only the ENTRY: `lib` stays cached below the rewind watermark
        // while the rewind clears the engine's `vars` table, so each re-check
        // substitutes into lib's stored type bodies with no live vars behind
        // an open template.
        let r = s.check(&parse(&entry(i)), Some(&p.dir));
        assert!(
            r.success(),
            "check {i} failed after rewind: {:?}",
            r.diagnostics
        );
    }
}
