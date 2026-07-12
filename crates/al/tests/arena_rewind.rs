//! The two boundaries where an index can outlive the arena that minted it.
//!
//! 1. **`IncrementalSession` rewinds.** `Compiler::reset_to` truncates the
//!    inference arena, the code/function/constant pools and the lowered Core
//!    IR back to a `Watermark`. Anything surviving the rewind that still holds
//!    an index into one of them must be truncated, filtered, or cleared. These
//!    tests drive the rewind hard — repeated checks, invalidation cascades,
//!    edit-and-revert — and assert the compiler answers identically every time.
//!
//! 2. **The precompiled stdlib blob.** `seed_static` memcpys the `.rodata`
//!    arenas in as the live engine's prefix; every `Ty` frozen into a static
//!    `Scheme` indexes there. No rewind may cross that prefix, and — because
//!    the blob ships *bytecode*, not IR — no stdlib body is ever re-lowered,
//!    which is why the post-inference resolved-type pool stays compile-local
//!    and out of the blob.

use al::bytecode::IncrementalSession;

mod common;
use common::{Project, module_key, parse};

// ── Seam 2: the frozen blob ────────────────────────────────────────────────

/// The spec's third assumption, confirmed behaviourally: **no stdlib body is
/// lowered at runtime.** The stdlib contributes hundreds of entries to
/// `program.functions` (hydrated straight out of `.rodata`) and exactly zero to
/// `core.fns` — `lower` only ever runs over the code being compiled now. A
/// resolved-type arena is therefore compile-local by construction: there is no
/// stdlib `RTy` for `static_ir::flatten` to serialise, and `build.rs`'s
/// dependency set need not grow a new pool.
///
/// If this ever fails — if some stdlib body reaches `lower` at runtime — the
/// blob must start carrying the resolved-type pool, and the spec's
/// "build.rs dependency set unchanged" non-goal is dead.
#[test]
fn stdlib_bodies_are_never_relowered() {
    // Two user functions, one of which leans on the stdlib (`array.map`) so the
    // stdlib is genuinely reachable rather than merely seeded.
    let src = "\
import al/array

fn double(x Int) Int { x * 2 }

fn apply_all(xs Array(Int)) Array(Int) { array.map(xs, double) }

println(apply_all([1, 2, 3]))
";
    let r = al::bytecode::compile(&parse(src), None, Some(&al::STDLIB));
    assert!(r.success, "compile failed: {:?}", r.diagnostics);

    // The seeded stdlib is large; the entry adds `__main__` on top of the two
    // user fns.
    assert!(
        r.program.functions.len() > 50,
        "expected the hydrated stdlib in program.functions, got {}",
        r.program.functions.len()
    );
    // ...and none of it was lowered. `double` and `apply_all` did, and nothing
    // else: the bound is loose enough to survive a future phase synthesising a
    // handful of extra fns (eta-wrappers become ordinary `TypedFn`s under the
    // typed-IR plan) and still tight enough that a single lowered stdlib module
    // would blow through it.
    let lowered = r.core.fns.len();
    assert!(
        (2..10).contains(&lowered),
        "expected only the 2 user fn bodies to reach `lower`, saw {lowered}; if a \
         stdlib body is being lowered at runtime the resolved-type pool is no \
         longer compile-local and the blob must serialise it"
    );
}

/// The frozen prefix survives every kind of rewind. `array.map` is a stdlib
/// `Scheme` whose `Ty`s index into the `.rodata` node arena; if any `reset_to`
/// truncated below the seed watermark those indices would dangle and this
/// program would stop type-checking (or resolve to garbage). Ten checks with
/// invalidations in between.
#[test]
fn stdlib_prefix_survives_repeated_rewinds() {
    let p = Project::new("arena_rewind_prefix");
    p.write("lib.al", "pub fn one() Int { 1 }\n");

    let entry = "\
import ./lib
import al/array

println(array.map([lib.one()], fn(x Int) x + 1))
";

    let mut s = IncrementalSession::new(&al::STDLIB);
    for i in 0..10 {
        // Alternate the imported module's body so `check` invalidates it and
        // rewinds to that module's watermark, not just the entry's.
        p.write("lib.al", &format!("pub fn one() Int {{ {} }}\n", i + 1));
        let r = s.check(&parse(entry), Some(&p.dir));
        assert!(
            r.success,
            "check {i} failed after rewind: {:?}",
            r.diagnostics
        );
    }
}

// ── Seam 1: rewound arenas ─────────────────────────────────────────────────

/// `reset_to` must leave the compiler *exactly* as it was at the watermark.
/// The sharpest observable of that is hover: it joins a resolved `Type` onto a
/// span through the engine arena that the rewind just truncated. Re-checking
/// the same source must produce the same type every time — a stale index into
/// a rewound arena would resolve to whatever re-minted at that slot.
#[test]
fn hover_is_stable_across_many_rewinds() {
    let p = Project::new("arena_rewind_hover");
    p.write("lib.al", "pub fn one() Int { 1 }\n");
    let entry = "import ./lib\nconst v = lib.one()\nprintln(v)\n";

    let mut s = IncrementalSession::new(&al::STDLIB);
    let mut seen: Option<String> = None;
    for i in 0..8 {
        let r = s.check(&parse(entry), Some(&p.dir));
        assert!(r.success, "check {i}: {:?}", r.diagnostics);
        // `v` on line 2 (0-based), inside `const v`.
        let (name, ty, _) = s
            .hover(Some(&al::module::ModuleKey::main()), 1, 6)
            .unwrap_or_else(|| panic!("no hover fact on check {i}"));
        let rendered = format!("{name}: {ty}");
        match &seen {
            None => seen = Some(rendered),
            Some(prev) => assert_eq!(prev, &rendered, "hover drifted on check {i}"),
        }
    }
}

/// A closure site holds a `func_idx` into `program.functions` and `StrId`s into
/// the engine's string arena, found by the `Span` of the lambda that minted it
/// — and a `Span` carries no module id. A site owned by the compiler rather
/// than by the frame that wrote it would survive an arena rewind not merely
/// dangling but *aliasable* by an unrelated body landing on the same span next
/// compile. Here the entry's closure and the imported module's closure sit at
/// spans that overlap, and the module is invalidated between checks so the
/// arenas move under them.
#[test]
fn closures_survive_an_invalidation_cascade() {
    let p = Project::new("arena_rewind_closures");
    // Same shape, same spans, different capture set than the entry's closure.
    p.write(
        "lib.al",
        "pub fn go(n Int) Int {\n  f = fn(x Int) x + n\n  f(1)\n}\n",
    );
    let entry = "\
import ./lib
const k = 10
g = fn(y Int) y * k
println(g(lib.go(2)))
";

    let mut s = IncrementalSession::new(&al::STDLIB);
    let first = s.check(&parse(entry), Some(&p.dir));
    assert!(first.success, "initial: {:?}", first.diagnostics);

    for i in 0..5 {
        // Touch the module so it (and the entry) recompile against a rewound
        // arena; the closure body moves under the same span.
        p.write(
            "lib.al",
            &format!("pub fn go(n Int) Int {{\n  f = fn(x Int) x + n + {i}\n  f(1)\n}}\n"),
        );
        let r = s.check(&parse(entry), Some(&p.dir));
        assert!(
            r.success,
            "check {i} after closure rewind: {:?}",
            r.diagnostics
        );
    }
}

/// Edit a module, then revert it. The session must land back on exactly the
/// state it started in: same diagnostics, same success, and — the arena
/// property — the same reserved type-id block for the module, which is only
/// reusable if the rewind was exact.
#[test]
fn edit_and_revert_returns_to_the_same_arena_state() {
    let p = Project::new("arena_rewind_revert");
    p.write(
        "lib.al",
        "pub type Pair = (Int, Int)\npub fn mk() Pair { (1, 2) }\n",
    );
    let entry = "import ./lib\nprintln(lib.mk())\n";

    let mut s = IncrementalSession::new(&al::STDLIB);
    assert!(s.check(&parse(entry), Some(&p.dir)).success);
    // The id-base table is keyed by the module's canonical identity (the
    // resolved file), never the `./lib` spelling — a written-path lookup
    // here would always be `None` and the assertion below vacuous.
    let lib = module_key(&p.dir, "lib.al");
    let base_before = s.module_id_base(&lib);
    assert!(
        base_before.is_some(),
        "lib.al was compiled, so it has a range"
    );

    p.write(
        "lib.al",
        "pub type Pair = (Int, Int)\npub fn mk() Pair { (3, 4) }\n",
    );
    assert!(s.check(&parse(entry), Some(&p.dir)).success);

    p.write(
        "lib.al",
        "pub type Pair = (Int, Int)\npub fn mk() Pair { (1, 2) }\n",
    );
    let r = s.check(&parse(entry), Some(&p.dir));
    assert!(r.success, "after revert: {:?}", r.diagnostics);
    assert_eq!(
        base_before,
        s.module_id_base(&lib),
        "type-id block was not reused; the rewind was not exact"
    );
}
