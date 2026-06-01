//! End-to-end tests for `IncrementalSession`: a 3-module chain A→B→C where
//! editing one file recompiles only that file and its dependents.

use al::bytecode::IncrementalSession;
use al::reference::EntityKind;

mod common;
use common::{Project, cursor, parse};

const A_SRC: &str = "import ./b\nprintln(b.b())\n";
const B_SRC: &str = "import ./c\npub fn b() Int { c.val() + 1 }\n";
const C_SRC: &str = "pub fn val() Int { 1 }\n";

/// Write the A→B→C chain to a fresh project and run the initial `check`,
/// asserting the baseline: it succeeds and exactly b + c compile fresh.
fn fresh_three_module_session(tag: &str) -> (Project, IncrementalSession) {
    let p = Project::new(tag);
    p.write("c.al", C_SRC);
    p.write("b.al", B_SRC);
    p.write("a.al", A_SRC);

    let mut s = IncrementalSession::new(al::stdlib());
    let r = s.check(&parse(A_SRC), Some(&p.dir));
    assert!(r.success, "initial: {:?}", r.diagnostics);
    assert_eq!(s.compile_count(), 2, "b + c compile on first check");
    (p, s)
}

#[test]
fn three_module_incremental() {
    let (p, mut s) = fresh_three_module_session("3mod");

    // --- no change: b and c are cache hits, only entry recompiles.
    let r = s.check(&parse(A_SRC), Some(&p.dir));
    assert!(r.success, "unchanged: {:?}", r.diagnostics);
    assert_eq!(s.compile_count(), 2, "no module recompile on unchanged");

    // --- change c: c invalidated, b is a dependent → both recompile.
    p.write("c.al", "pub fn val() Int { 2 }\n");
    let r = s.check(&parse(A_SRC), Some(&p.dir));
    assert!(r.success, "c-changed: {:?}", r.diagnostics);
    assert_eq!(
        s.compile_count(),
        4,
        "c-change recompiles c and its dependent b"
    );

    // --- change b only: c is cache-hit, b recompiles.
    p.write("b.al", "import ./c\npub fn b() Int { c.val() + 100 }\n");
    let r = s.check(&parse(A_SRC), Some(&p.dir));
    assert!(r.success, "b-changed: {:?}", r.diagnostics);
    assert_eq!(s.compile_count(), 5, "b-change recompiles b only; c cached");

    // --- type error introduced in c is reported.
    p.write("c.al", "pub fn val() Int { 'nope' }\n");
    let r = s.check(&parse(A_SRC), Some(&p.dir));
    assert!(!r.success, "expected type error in c");
    assert_eq!(s.compile_count(), 7);

    // --- fix c: clean again.
    p.write("c.al", "pub fn val() Int { 3 }\n");
    let r = s.check(&parse(A_SRC), Some(&p.dir));
    assert!(r.success, "fixed: {:?}", r.diagnostics);
    assert_eq!(s.compile_count(), 9);
}

#[test]
fn overlay_preferred_over_disk() {
    let (p, mut s) = fresh_three_module_session("overlay");

    // Unsaved buffer for c.al with a type error; disk is unchanged.
    s.set_overlay(
        p.dir.join("c.al"),
        "pub fn val() Int { 'oops' }\n".to_string(),
    );
    let r = s.check(&parse(A_SRC), Some(&p.dir));
    assert!(
        !r.success,
        "overlay change must be picked up even though disk is unchanged"
    );
    // c+b recompiled from the overlay.
    assert_eq!(s.compile_count(), 4);
}

/// `IncrementalSession::invalidate_path` is the sole behavior behind the LSP
/// `didChangeWatchedFiles` notification (a file edited on disk *outside* the
/// editor). Unlike the per-keystroke staleness scan, it must act
/// unconditionally — independent of the `(mtime, len)` stat gate that `check`
/// uses to skip unchanged files: it (1) drops any in-memory overlay for the
/// path so the next check re-reads disk, and (2) force-evicts the cached
/// module *and its dependents* so they recompile. This pins both, using a
/// no-op "control" check to prove the stat gate really would otherwise have
/// treated the file as cached — so the forced recompile is attributable to
/// `invalidate_path` and nothing else. No other test in this file calls it.
#[test]
fn invalidate_path_drops_overlay_and_force_evicts() {
    let (p, mut s) = fresh_three_module_session("invalidate");
    let cpath = p.dir.join("c.al");

    // Control: with the disk untouched, the (mtime,len) stat gate treats c
    // (and b) as cached, so a second check recompiles nothing. This baseline
    // is what makes the forced eviction below provable — the next bump cannot
    // come from a detected source change, only from invalidate_path.
    let r = s.check(&parse(A_SRC), Some(&p.dir));
    assert!(r.success, "unchanged: {:?}", r.diagnostics);
    assert_eq!(
        s.compile_count(),
        2,
        "unchanged disk: the (mtime,len) stat gate keeps c + b cached"
    );

    // Force-evict past the stat gate: the on-disk file is byte-identical and
    // its (mtime,len) is unchanged since the control check, so `source_changed`
    // alone would skip c — yet invalidate_path evicts c and its dependent b,
    // and the following check recompiles both.
    let before = s.compile_count();
    s.invalidate_path(&cpath);
    let r = s.check(&parse(A_SRC), Some(&p.dir));
    assert!(r.success, "after force-evict: {:?}", r.diagnostics);
    assert_eq!(
        s.compile_count(),
        before + 2,
        "invalidate_path force-evicts c + its dependent b despite the unchanged stat tuple"
    );

    // Overlay observed: an unsaved buffer for c.al with a type error makes the
    // check fail, even though the on-disk source is still clean.
    s.set_overlay(cpath.clone(), "pub fn val() Int { 'oops' }\n".to_string());
    let r = s.check(&parse(A_SRC), Some(&p.dir));
    assert!(
        !r.success,
        "overlay type error must be observed over clean disk"
    );

    // didChangeWatchedFiles arrives: invalidate_path drops the overlay, so the
    // next check re-reads the clean disk source and goes green again. Were the
    // overlay *not* dropped, c would reload from the erroring buffer and the
    // check would still fail — so a green result here proves the overlay was
    // evicted, not merely that the module was recompiled from the same buffer.
    s.invalidate_path(&cpath);
    let r = s.check(&parse(A_SRC), Some(&p.dir));
    assert!(
        r.success,
        "clean disk must be re-read after invalidate_path drops the overlay: {:?}",
        r.diagnostics
    );
}

#[test]
fn unrelated_module_keeps_type_id_base() {
    // x and y are independent; the entry imports both. Editing x must not
    // shift y's type-id range.
    let p = Project::new("idbase");
    p.write("x.al", "pub type X { X }\npub fn f() X { X }\n");
    p.write("y.al", "pub type Y { Y }\npub fn g() Y { Y }\n");
    let entry = "import ./x\nimport ./y\n_a = x.f()\n_b = y.g()\n";

    let mut s = IncrementalSession::new(al::stdlib());
    let r = s.check(&parse(entry), Some(&p.dir));
    assert!(r.success, "initial: {:?}", r.diagnostics);

    let x0 = s.module_id_base("./x").expect("x has an id_base");
    let y0 = s.module_id_base("./y").expect("y has an id_base");
    assert_eq!(x0 % 256, 0, "id_base is 256-aligned");
    assert_ne!(x0, y0, "distinct modules get distinct ranges");

    // Change x: add a second type. y is unrelated.
    p.write(
        "x.al",
        "pub type X { X }\npub type X2 { X2 }\npub fn f() X { X }\n",
    );
    let r = s.check(&parse(entry), Some(&p.dir));
    assert!(r.success, "after x edit: {:?}", r.diagnostics);

    assert_eq!(
        s.module_id_base("./x"),
        Some(x0),
        "x reuses its original id_base on recompile"
    );
    assert_eq!(
        s.module_id_base("./y"),
        Some(y0),
        "y keeps its id_base even though it was compiled after x"
    );
}

#[test]
fn query_api_resolves_across_the_module_chain() {
    let (_p, s) = fresh_three_module_session("queryapi");

    // goto-def: `b.b()` in the entry resolves into module B.
    let (l, c) = cursor(A_SRC, "b()", 1, 0);
    let (m, _) = s.definition("main", l, c).expect("b.b() resolves");
    assert_eq!(m.last().map(String::as_str), Some("b"));

    // hover surfaces the name + an inferred type at the same position.
    let (hn, _, _) = s.hover("main", l, c).expect("hover at b.b()");
    assert_eq!(hn, "b");

    // documentSymbol / workspace symbol see the chain's declarations.
    assert!(
        s.document_symbols("./c")
            .iter()
            .any(|x| x.name == "val" && x.kind == EntityKind::Function)
    );
    assert!(s.workspace_symbols("val").iter().any(|x| x.name == "val"));

    // find-references on C.val: declaration in C + the qualified use in B.
    let (cl, cc) = cursor(B_SRC, "val", 1, 0);
    let (vdef, _) = s.prepare_rename("./b", cl, cc).expect("val DefId via B");
    let rmods: Vec<String> = s
        .references(vdef)
        .iter()
        .map(|(mp, _)| mp.join("/"))
        .collect();
    assert!(
        rmods.iter().any(|m| m.ends_with("c")) && rmods.iter().any(|m| m.ends_with("b")),
        "val references must span C (decl) and B (use): {rmods:?}"
    );
    assert_eq!(s.rename(vdef), s.references(vdef), "rename == references");
}

#[test]
fn edit_b_keeps_refs_then_invalidation_drops_reverse_edges() {
    let (p, mut s) = fresh_three_module_session("revedges");

    let (l, c) = cursor(A_SRC, "b()", 1, 0);
    let (bdef0, _) = s.prepare_rename("main", l, c).expect("b DefId");
    assert!(
        !s.reference_graph().references_to(bdef0).is_empty(),
        "entry use creates a reverse edge into B"
    );
    let n0 = s.compile_count();

    // Edit B's body only (signature kept): a reference into B from A must
    // still resolve after B is recompiled.
    p.write("b.al", "import ./c\npub fn b() Int { c.val() + 41 }\n");
    let r = s.check(&parse(A_SRC), Some(&p.dir));
    assert!(r.success, "after B edit: {:?}", r.diagnostics);
    assert!(s.compile_count() > n0, "B recompiled");
    let (m1, _) = s
        .definition("main", l, c)
        .expect("b.b() still resolves after editing B");
    assert_eq!(m1.last().map(String::as_str), Some("b"));
    let (bdef1, _) = s.prepare_rename("main", l, c).expect("b DefId after edit");
    assert!(
        s.references(bdef1)
            .iter()
            .any(|(mp, _)| mp.join("/") == "main"),
        "entry's use of B must survive B's recompile"
    );

    // Invalidate: B no longer declares `b` and the entry no longer uses it.
    // The rebuilt graph must not retain a stale reverse edge for the old def.
    let entry2 = "import ./b\nprintln(b.gone())\n";
    p.write("b.al", "import ./c\npub fn gone() Int { c.val() }\n");
    p.write("a.al", entry2);
    let r = s.check(&parse(entry2), Some(&p.dir));
    assert!(r.success, "after invalidation: {:?}", r.diagnostics);
    assert!(
        s.reference_graph().references_to(bdef0).is_empty()
            && s.reference_graph().references_to(bdef1).is_empty(),
        "stale reverse edges must vanish when B is invalidated"
    );
}

/// Number of nominal type heads `big.al` declares in its *grown* form.
/// Each `type Tn { Tn }` consumes exactly one type id
/// (`register_type_head`, pass 1), so this is the module's per-compile
/// type-id count after the overflow edit. It is deliberately well above
/// `MODULE_TYPE_ID_RANGE` (256) so a *reused* range spills past its
/// reservation and collides with the sibling module's assigned block.
const BIG_TYPE_COUNT: i32 = 300;

/// Number of type heads `big.al` declares in its *initial* form. Kept well
/// under `MODULE_TYPE_ID_RANGE` (256) so the first compile fits `big`
/// inside its reservation and the sibling `y` is assigned the immediately
/// following 256-aligned block (`big0 + 256`) — *before* `big` grows. That
/// ordering is what makes the later reused-range spill actually overlap
/// y's ids, so the recovery machinery is required for correctness, not
/// merely exercised.
const SMALL_TYPE_COUNT: i32 = 10;

/// `big.al`: `type_count` single-constructor types plus `f` (so the entry
/// can keep it referenced) and `tweak` (an Int body so the source still
/// changes — forcing a recompile — even between two equal type counts).
/// Growing `type_count` from `SMALL_TYPE_COUNT` to `BIG_TYPE_COUNT` is the
/// edit that makes a *reused* id range overflow into the sibling's block.
fn big_module_src(type_count: i32, tweak: i32) -> String {
    let mut s = String::with_capacity(type_count as usize * 24);
    for i in 0..type_count {
        s.push_str(&format!("pub type T{i} {{ T{i} }}\n"));
    }
    s.push_str("pub fn f() T0 { T0 }\n");
    s.push_str(&format!("pub fn tweak() Int {{ {tweak} }}\n"));
    s
}

/// Sixth incremental test — the end-to-end regression for the type-id
/// overflow machinery (`note_id_usage`'s `reused && used > 256` branch,
/// the `id_range_overflow` flag, and the `invalidate_all` +
/// `reset_id_bases` recovery inside `IncrementalSession::check`). The other
/// five tests never allocate more than 256 ids in a reused range, so none
/// of that machinery is otherwise exercised.
///
/// DELIVERABLE COUNT (resolved): the spec's "incremental 5/5" target was
/// written before this regression unit was carved out. This unit's whole
/// purpose is a *sixth* test that actually raises the overflow flag, which
/// `unrelated_module_keeps_type_id_base` (<= 256 ids) provably cannot.
/// incremental.rs is therefore 6/6 and `verify-green-and-bench` expects 6.
///
/// DISCRIMINATING BY CONSTRUCTION: `big` is compiled *small* first, so the
/// sibling `y` is assigned the immediately-following 256-block
/// (`big0 + 256`). The edit then *grows* `big` past 256 ids: on the reuse
/// path that spill runs `[big0, big0 + BIG_TYPE_COUNT)` and overlaps y's
/// `big0 + 256` block — a real id collision. Recovery (Placement B:
/// `invalidate_all` + `reset_id_bases` + one re-run, inside the same
/// `check`) re-allocates every module fresh, so `big` keeps `big0` while
/// `y` is pushed past big's grown span. Every post-edit signal below is
/// false if that recovery is removed or unwired: `y` would stay at
/// `big0 + 256` (so both `y1 != y0` and `y1 >= big1 + BIG_TYPE_COUNT`
/// fail), and `compile_count` would be 4, not 6 — recovery re-runs
/// `process_imports`, recompiling big + y a second time within the one
/// `check`. FLAG-CONSUMER PLACEMENT is single (Placement B), so success +
/// stable ranges hold after *one* `check`; this never assumes a second
/// pass (it does not encode the rejected Step-D-in-note_id_usage variant).
#[test]
fn recompile_id_overflow_recovers_with_stable_ranges() {
    let p = Project::new("idoverflow");
    p.write("big.al", &big_module_src(SMALL_TYPE_COUNT, 1));
    p.write("y.al", "pub type Y { Y }\npub fn g() Y { Y }\n");
    let entry = "import ./big\nimport ./y\n_a = big.f()\n_b = y.g()\n";

    let mut s = IncrementalSession::new(al::stdlib());

    // --- initial compile: big is small, so it fits inside its 256-id
    // reservation (fresh, reused = false, no overflow flag) and y is
    // assigned the very next 256-aligned block — big0 + 256 — *before*
    // big grows. That places y exactly where big's later spill will land.
    let r = s.check(&parse(entry), Some(&p.dir));
    assert!(r.success, "initial: {:?}", r.diagnostics);
    assert_eq!(s.compile_count(), 2, "big + y compile on first check");

    let big0 = s.module_id_base("./big").expect("big has an id_base");
    let y0 = s.module_id_base("./y").expect("y has an id_base");
    assert_eq!(big0 % 256, 0, "big id_base is 256-aligned");
    assert_eq!(y0 % 256, 0, "y id_base is 256-aligned");
    assert_ne!(big0, y0, "distinct modules get distinct ranges");
    assert_eq!(
        y0,
        big0 + 256,
        "while big is small, y sits in the block right after big's \
         reservation (the block big's spill will collide with): \
         big0={big0} y0={y0}"
    );

    // --- grow big past 256 type heads: the forced recompile *reuses*
    // big's id_base (reused = true) and allocates BIG_TYPE_COUNT > 256
    // ids, so the spill [big0, big0 + BIG_TYPE_COUNT) overlaps y's
    // already-assigned big0 + 256 block — `id_range_overflow` is raised.
    // Under Placement B the same `check` invalidates all, resets the id
    // bases and re-runs once, re-placing y past big's grown span so the
    // call returns successfully with non-colliding ranges.
    p.write("big.al", &big_module_src(BIG_TYPE_COUNT, 2));
    let r = s.check(&parse(entry), Some(&p.dir));
    assert!(
        r.success,
        "overflow recovery must keep the check green within one pass: {:?}",
        r.diagnostics
    );
    // Recovery re-runs process_imports, so big + y are each compiled twice
    // this check: 2 (normal pass) + 2 (recovery re-run) on top of the 2
    // from the initial check = 6. Without recovery this is 4 — pinning
    // this is what makes deleting/unwiring the recovery block fail here.
    assert_eq!(
        s.compile_count(),
        6,
        "overflow recovery re-runs process_imports: big + y compile twice"
    );

    let big1 = s
        .module_id_base("./big")
        .expect("big id_base after recovery");
    let y1 = s.module_id_base("./y").expect("y id_base after recovery");
    assert_eq!(big1 % 256, 0, "big id_base still 256-aligned post-recovery");
    assert_eq!(y1 % 256, 0, "y id_base still 256-aligned post-recovery");
    assert_eq!(
        big1, big0,
        "big keeps a stable id_base across overflow recovery"
    );
    assert_ne!(
        y1, y0,
        "recovery must move y off its now-colliding big0 + 256 block; \
         y1 == y0 means recovery did not run"
    );
    assert!(
        y1 >= big1 + BIG_TYPE_COUNT,
        "post-recovery ranges must clear big's grown {BIG_TYPE_COUNT}-id \
         span: big1={big1} y1={y1} (false if recovery is unwired)"
    );

    // --- an unrelated edit after recovery stays stable and green: big is
    // a cache hit (its usage is never re-noted, so no second overflow),
    // and y recompiles inside its own post-recovery reused range.
    p.write(
        "y.al",
        "pub type Y { Y }\npub fn g() Y { Y }\npub fn h() Int { 7 }\n",
    );
    let r = s.check(&parse(entry), Some(&p.dir));
    assert!(
        r.success,
        "post-recovery unrelated edit: {:?}",
        r.diagnostics
    );
    assert_eq!(
        s.compile_count(),
        7,
        "only y recompiles on the unrelated edit; big is a cache hit"
    );
    assert_eq!(
        s.module_id_base("./big"),
        Some(big0),
        "big id_base remains stable after a later unrelated edit"
    );
    assert_eq!(
        s.module_id_base("./y"),
        Some(y1),
        "y keeps its post-recovery id_base after a later unrelated edit"
    );
}
