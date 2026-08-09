//! The two boundaries where an index can outlive the arena that minted it.
//!
//! 1. `IncrementalSession` rewinds. `Compiler::reset_to` truncates the
//!    inference arena, the code/function/constant pools and the lowered Core
//!    IR back to a `Watermark`. Anything surviving that still holds an index
//!    into one of them must be truncated, filtered, or cleared.
//!
//! 2. The precompiled stdlib blob. `seed_static` memcpys the `.rodata` arenas
//!    in as the live engine's prefix, and every `Ty` frozen into a static
//!    `Scheme` indexes there, so no rewind may cross it.

use al::bytecode::IncrementalSession;

mod common;
use common::{Project, module_key, parse};

/// No stdlib body is lowered at runtime on the seeded path: the stdlib
/// contributes hundreds of entries to `program.functions` and zero to
/// `core.fns`. That is what keeps the resolved-type pool compile-local and out
/// of the blob. If this fails the blob must start carrying that pool.
#[test]
fn stdlib_bodies_are_never_relowered() {
    // `array.map` makes the stdlib genuinely reachable, not merely seeded.
    let src = "\
import al/array

fn double(x Int) Int { x * 2 }

fn apply_all(xs Array(Int)) Array(Int) { array.map(xs, double) }

println(apply_all([1, 2, 3]))
";
    let r = al::bytecode::compile(&parse(src), None, Some(&al::STDLIB));
    assert!(r.success(), "compile failed: {:?}", r.diagnostics);
    let r = r.emitted.expect("a successful compile emits");

    assert!(
        r.program.functions.len() > 50,
        "expected the hydrated stdlib in program.functions, got {}",
        r.program.functions.len()
    );
    // Loose enough to survive a phase synthesising a few extra fns, tight
    // enough that one lowered stdlib module would blow through it.
    let lowered = r.core.fns.len();
    assert!(
        (2..10).contains(&lowered),
        "expected only the 2 user fn bodies to reach `lower`, saw {lowered}; if a \
         stdlib body is being lowered at runtime the resolved-type pool is no \
         longer compile-local and the blob must serialise it"
    );
}

/// The frozen prefix survives every kind of rewind. `array.map`'s `Ty`s index
/// into the `.rodata` node arena, so a `reset_to` below the seed watermark
/// would dangle them and this program would stop type-checking.
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
        // Vary the imported module so `check` rewinds to its watermark, not
        // just the entry's.
        p.write("lib.al", &format!("pub fn one() Int {{ {} }}\n", i + 1));
        let r = s.check(&parse(entry), Some(&p.dir));
        assert!(
            r.success(),
            "check {i} failed after rewind: {:?}",
            r.diagnostics
        );
    }
}

/// `reset_to` must leave the compiler exactly as it was at the watermark.
/// Hover is the sharpest observable: it joins a resolved `Type` onto a span
/// through the arena the rewind just truncated, so a stale index would resolve
/// to whatever re-minted at that slot.
#[test]
fn hover_is_stable_across_many_rewinds() {
    let p = Project::new("arena_rewind_hover");
    p.write("lib.al", "pub fn one() Int { 1 }\n");
    let entry = "import ./lib\nconst v = lib.one()\nprintln(v)\n";

    let mut s = IncrementalSession::new(&al::STDLIB);
    let mut seen: Option<String> = None;
    for i in 0..8 {
        let r = s.check(&parse(entry), Some(&p.dir));
        assert!(r.success(), "check {i}: {:?}", r.diagnostics);
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

/// A closure site is keyed by the `Span` of the lambda that minted it, and a
/// `Span` carries no module id. A site outliving a rewind would be not merely
/// dangling but *aliasable* by an unrelated body landing on the same span. Here
/// the entry's and the module's closures sit at overlapping spans, and the
/// module is invalidated between checks so the arenas move under them.
#[test]
fn closures_survive_an_invalidation_cascade() {
    let p = Project::new("arena_rewind_closures");
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
    assert!(first.success(), "initial: {:?}", first.diagnostics);

    for i in 0..5 {
        // Touch the module so it and the entry recompile against a rewound
        // arena; the closure body moves under the same span.
        p.write(
            "lib.al",
            &format!("pub fn go(n Int) Int {{\n  f = fn(x Int) x + n + {i}\n  f(1)\n}}\n"),
        );
        let r = s.check(&parse(entry), Some(&p.dir));
        assert!(
            r.success(),
            "check {i} after closure rewind: {:?}",
            r.diagnostics
        );
    }
}

/// Edit a module, then revert it. The session must land back on the state it
/// started in, down to the same reserved type-id block, which is reusable only
/// if the rewind was exact.
#[test]
fn edit_and_revert_returns_to_the_same_arena_state() {
    let p = Project::new("arena_rewind_revert");
    p.write(
        "lib.al",
        "pub type Pair = (Int, Int)\npub fn mk() Pair { (1, 2) }\n",
    );
    let entry = "import ./lib\nprintln(lib.mk())\n";

    let mut s = IncrementalSession::new(&al::STDLIB);
    assert!(s.check(&parse(entry), Some(&p.dir)).success());
    // Keyed by canonical identity, never the `./lib` spelling: a written-path
    // lookup would always be `None` and the assertion below vacuous.
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
    assert!(s.check(&parse(entry), Some(&p.dir)).success());

    p.write(
        "lib.al",
        "pub type Pair = (Int, Int)\npub fn mk() Pair { (1, 2) }\n",
    );
    let r = s.check(&parse(entry), Some(&p.dir));
    assert!(r.success(), "after revert: {:?}", r.diagnostics);
    assert_eq!(
        base_before,
        s.module_id_base(&lib),
        "type-id block was not reused; the rewind was not exact"
    );
}
