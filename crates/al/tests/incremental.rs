//! End-to-end tests for `IncrementalSession`: a 3-module chain A→B→C where
//! editing one file recompiles only that file and its dependents.

use al::bytecode::IncrementalSession;
use al::module::MODULE_TYPE_ID_RANGE;
use al::reference::EntityKind;

mod common;
use common::{Project, SessionQueryExt, cursor, module_key, parse};

const A_SRC: &str = "import ./b\nprintln(b.b())\n";
const B_SRC: &str = "import ./c\npub fn b() Int { c.val() + 1 }\n";
const C_SRC: &str = "pub fn val() Int { 1 }\n";

/// Write the A→B→C chain to a fresh project and run the initial `check`,
/// asserting the baseline: exactly b + c compile fresh.
fn fresh_three_module_session(tag: &str) -> (Project, IncrementalSession) {
    let p = Project::new(tag);
    p.write("c.al", C_SRC);
    p.write("b.al", B_SRC);
    p.write("a.al", A_SRC);

    let mut s = IncrementalSession::new(&al::STDLIB);
    let r = s.check(&parse(A_SRC), Some(&p.dir));
    assert!(r.success(), "initial: {:?}", r.diagnostics);
    assert_eq!(s.compile_count(), 2, "b + c compile on first check");
    (p, s)
}

#[test]
fn three_module_incremental() {
    let (p, mut s) = fresh_three_module_session("3mod");

    // No change: b and c are cache hits.
    let r = s.check(&parse(A_SRC), Some(&p.dir));
    assert!(r.success(), "unchanged: {:?}", r.diagnostics);
    assert_eq!(s.compile_count(), 2, "no module recompile on unchanged");

    // Change c: b is a dependent, so both recompile.
    p.write("c.al", "pub fn val() Int { 2 }\n");
    let r = s.check(&parse(A_SRC), Some(&p.dir));
    assert!(r.success(), "c-changed: {:?}", r.diagnostics);
    assert_eq!(
        s.compile_count(),
        4,
        "c-change recompiles c and its dependent b"
    );

    // Change b only: c stays cached.
    p.write("b.al", "import ./c\npub fn b() Int { c.val() + 100 }\n");
    let r = s.check(&parse(A_SRC), Some(&p.dir));
    assert!(r.success(), "b-changed: {:?}", r.diagnostics);
    assert_eq!(s.compile_count(), 5, "b-change recompiles b only; c cached");

    // A type error in c is reported.
    p.write("c.al", "pub fn val() Int { 'nope' }\n");
    let r = s.check(&parse(A_SRC), Some(&p.dir));
    assert!(!r.success(), "expected type error in c");
    assert_eq!(s.compile_count(), 7);

    // Fix c.
    p.write("c.al", "pub fn val() Int { 3 }\n");
    let r = s.check(&parse(A_SRC), Some(&p.dir));
    assert!(r.success(), "fixed: {:?}", r.diagnostics);
    assert_eq!(s.compile_count(), 9);
}

#[test]
fn overlay_preferred_over_disk() {
    let (p, mut s) = fresh_three_module_session("overlay");

    // Unsaved buffer with a type error; disk is unchanged.
    s.set_overlay(
        p.dir.join("c.al"),
        "pub fn val() Int { 'oops' }\n".to_string(),
    );
    let r = s.check(&parse(A_SRC), Some(&p.dir));
    assert!(
        !r.success(),
        "overlay change must be picked up even though disk is unchanged"
    );
    assert_eq!(s.compile_count(), 4, "c + b recompiled from the overlay");
}

/// `IncrementalSession::invalidate_path` backs the LSP's
/// `didChangeWatchedFiles`. It must act independently of the `(mtime, len)`
/// stat gate `check` uses: it drops the path's overlay and force-evicts the
/// cached module and its dependents. A no-op control check first proves the
/// gate would otherwise have treated the file as cached.
#[test]
fn invalidate_path_drops_overlay_and_force_evicts() {
    let (p, mut s) = fresh_three_module_session("invalidate");
    let cpath = p.dir.join("c.al");

    // Control: with the disk untouched nothing recompiles, so the next bump
    // can only come from invalidate_path.
    let r = s.check(&parse(A_SRC), Some(&p.dir));
    assert!(r.success(), "unchanged: {:?}", r.diagnostics);
    assert_eq!(
        s.compile_count(),
        2,
        "unchanged disk: the (mtime,len) stat gate keeps c + b cached"
    );

    // The file is byte-identical since the control check, so `source_changed`
    // alone would skip c.
    let before = s.compile_count();
    s.invalidate_path(&cpath);
    let r = s.check(&parse(A_SRC), Some(&p.dir));
    assert!(r.success(), "after force-evict: {:?}", r.diagnostics);
    assert_eq!(
        s.compile_count(),
        before + 2,
        "invalidate_path force-evicts c + its dependent b despite the unchanged stat tuple"
    );

    // An unsaved erroring buffer fails the check over a clean disk.
    s.set_overlay(cpath.clone(), "pub fn val() Int { 'oops' }\n".to_string());
    let r = s.check(&parse(A_SRC), Some(&p.dir));
    assert!(
        !r.success(),
        "overlay type error must be observed over clean disk"
    );

    // Green here proves the overlay was evicted, not merely that the module
    // recompiled from the same buffer.
    s.invalidate_path(&cpath);
    let r = s.check(&parse(A_SRC), Some(&p.dir));
    assert!(
        r.success(),
        "clean disk must be re-read after invalidate_path drops the overlay: {:?}",
        r.diagnostics
    );
}

#[test]
fn unrelated_module_keeps_type_id_base() {
    // x and y are independent; editing x must not shift y's type-id range.
    let p = Project::new("idbase");
    p.write("x.al", "pub type X { X }\npub fn f() X { X }\n");
    p.write("y.al", "pub type Y { Y }\npub fn g() Y { Y }\n");
    let entry = "import ./x\nimport ./y\n_a = x.f()\n_b = y.g()\n";

    let mut s = IncrementalSession::new(&al::STDLIB);
    let r = s.check(&parse(entry), Some(&p.dir));
    assert!(r.success(), "initial: {:?}", r.diagnostics);

    let x0 = s
        .module_id_base(&module_key(&p.dir, "x.al"))
        .expect("x has an id_base");
    let y0 = s
        .module_id_base(&module_key(&p.dir, "y.al"))
        .expect("y has an id_base");
    assert_eq!(x0.0 % MODULE_TYPE_ID_RANGE, 0, "id_base is range-aligned");
    assert_ne!(x0, y0, "distinct modules get distinct ranges");

    // Add a second type to x.
    p.write(
        "x.al",
        "pub type X { X }\npub type X2 { X2 }\npub fn f() X { X }\n",
    );
    let r = s.check(&parse(entry), Some(&p.dir));
    assert!(r.success(), "after x edit: {:?}", r.diagnostics);

    assert_eq!(
        s.module_id_base(&module_key(&p.dir, "x.al")),
        Some(x0),
        "x reuses its original id_base on recompile"
    );
    assert_eq!(
        s.module_id_base(&module_key(&p.dir, "y.al")),
        Some(y0),
        "y keeps its id_base even though it was compiled after x"
    );
}

#[test]
fn query_api_resolves_across_the_module_chain() {
    let (_p, s) = fresh_three_module_session("queryapi");

    let (l, c) = cursor(A_SRC, "b()", 1, 0);
    let (m, _) = s.definition("main", l, c).expect("b.b() resolves");
    assert_eq!(m.last().map(String::as_str), Some("b"));

    let (hn, _, _) = s
        .hover(Some(&al::module::ModuleKey::main()), l, c)
        .expect("hover at b.b()");
    assert_eq!(hn, "b");

    assert!(
        s.document_symbols("./c")
            .iter()
            .any(|x| x.name == "val" && x.kind == EntityKind::Function)
    );
    assert!(s.workspace_symbols("val").iter().any(|x| x.name == "val"));

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

    // Edit B's body only, keeping the signature.
    p.write("b.al", "import ./c\npub fn b() Int { c.val() + 41 }\n");
    let r = s.check(&parse(A_SRC), Some(&p.dir));
    assert!(r.success(), "after B edit: {:?}", r.diagnostics);
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

    // B no longer declares `b`: the rebuilt graph must retain no stale edge.
    let entry2 = "import ./b\nprintln(b.gone())\n";
    p.write("b.al", "import ./c\npub fn gone() Int { c.val() }\n");
    p.write("a.al", entry2);
    let r = s.check(&parse(entry2), Some(&p.dir));
    assert!(r.success(), "after invalidation: {:?}", r.diagnostics);
    assert!(
        s.reference_graph().references_to(bdef0).is_empty()
            && s.reference_graph().references_to(bdef1).is_empty(),
        "stale reverse edges must vanish when B is invalidated"
    );
}

/// Type heads `big.al` declares once grown. One id each, deliberately well
/// above `MODULE_TYPE_ID_RANGE` so a reused range spills past its reservation
/// into the sibling module's block.
const BIG_TYPE_COUNT: i32 = MODULE_TYPE_ID_RANGE + 44;

/// Type heads `big.al` declares initially. Under `MODULE_TYPE_ID_RANGE`, so
/// the first compile fits and `y` lands in the very next block — which is what
/// the later spill collides with.
const SMALL_TYPE_COUNT: i32 = 10;

/// `big.al`: `type_count` single-constructor types, `f` to keep the entry
/// referencing it, and `tweak` so the source changes even between two equal
/// type counts.
fn big_module_src(type_count: i32, tweak: i32) -> String {
    let mut s = String::with_capacity(type_count as usize * 24);
    for i in 0..type_count {
        s.push_str(&format!("pub type T{i} {{ T{i} }}\n"));
    }
    s.push_str("pub fn f() T0 { T0 }\n");
    s.push_str(&format!("pub fn tweak() Int {{ {tweak} }}\n"));
    s
}

/// The only exercise of the type-id overflow machinery:
/// `IdRangeReservation::note_usage`'s `reused && used > 256` branch, the
/// `id_range_overflow` flag, and the `invalidate_all` + `reset_id_bases`
/// recovery inside `IncrementalSession::check`.
///
/// `big` compiles small first, so `y` lands at `big0 + 256`. Growing `big`
/// past 256 ids makes the reused range overlap that block. Recovery
/// re-allocates every module inside the same `check`, so `big` keeps `big0`
/// and `y` moves past big's grown span. Without it `y1 == y0` and
/// `compile_count` is 4, not 6.
#[test]
fn recompile_id_overflow_recovers_with_stable_ranges() {
    let p = Project::new("idoverflow");
    p.write("big.al", &big_module_src(SMALL_TYPE_COUNT, 1));
    p.write("y.al", "pub type Y { Y }\npub fn g() Y { Y }\n");
    let entry = "import ./big\nimport ./y\n_a = big.f()\n_b = y.g()\n";

    let mut s = IncrementalSession::new(&al::STDLIB);

    // big fits its reservation, so y is assigned the very next block — where
    // big's later spill will land.
    let r = s.check(&parse(entry), Some(&p.dir));
    assert!(r.success(), "initial: {:?}", r.diagnostics);
    assert_eq!(s.compile_count(), 2, "big + y compile on first check");

    let big0 = s
        .module_id_base(&module_key(&p.dir, "big.al"))
        .expect("big has an id_base")
        .0;
    let y0 = s
        .module_id_base(&module_key(&p.dir, "y.al"))
        .expect("y has an id_base")
        .0;
    assert_eq!(
        big0 % MODULE_TYPE_ID_RANGE,
        0,
        "big id_base is range-aligned"
    );
    assert_eq!(y0 % MODULE_TYPE_ID_RANGE, 0, "y id_base is range-aligned");
    assert_ne!(big0, y0, "distinct modules get distinct ranges");
    assert_eq!(
        y0,
        big0 + MODULE_TYPE_ID_RANGE,
        "while big is small, y sits in the block right after big's \
         reservation (the block big's spill will collide with): \
         big0={big0} y0={y0}"
    );

    // Grow big past 256 heads: the recompile reuses big's id_base, so the
    // spill overlaps y's block and raises `id_range_overflow`. The same
    // `check` then invalidates all, resets the bases and re-runs once.
    p.write("big.al", &big_module_src(BIG_TYPE_COUNT, 2));
    let r = s.check(&parse(entry), Some(&p.dir));
    assert!(
        r.success(),
        "overflow recovery must keep the check green within one pass: {:?}",
        r.diagnostics
    );
    // 2 initial + 2 normal pass + 2 recovery re-run. Without recovery: 4.
    assert_eq!(
        s.compile_count(),
        6,
        "overflow recovery re-runs process_imports: big + y compile twice"
    );

    let big1 = s
        .module_id_base(&module_key(&p.dir, "big.al"))
        .expect("big id_base after recovery")
        .0;
    let y1 = s
        .module_id_base(&module_key(&p.dir, "y.al"))
        .expect("y id_base after recovery")
        .0;
    assert_eq!(
        big1 % MODULE_TYPE_ID_RANGE,
        0,
        "big id_base still range-aligned post-recovery"
    );
    assert_eq!(
        y1 % MODULE_TYPE_ID_RANGE,
        0,
        "y id_base still range-aligned post-recovery"
    );
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

    // An unrelated edit after recovery: big is a cache hit, so its usage is
    // never re-noted and there is no second overflow.
    p.write(
        "y.al",
        "pub type Y { Y }\npub fn g() Y { Y }\npub fn h() Int { 7 }\n",
    );
    let r = s.check(&parse(entry), Some(&p.dir));
    assert!(
        r.success(),
        "post-recovery unrelated edit: {:?}",
        r.diagnostics
    );
    assert_eq!(
        s.compile_count(),
        7,
        "only y recompiles on the unrelated edit; big is a cache hit"
    );
    assert_eq!(
        s.module_id_base(&module_key(&p.dir, "big.al")).map(|t| t.0),
        Some(big0),
        "big id_base remains stable after a later unrelated edit"
    );
    assert_eq!(
        s.module_id_base(&module_key(&p.dir, "y.al")).map(|t| t.0),
        Some(y1),
        "y keeps its post-recovery id_base after a later unrelated edit"
    );
}
