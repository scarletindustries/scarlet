use super::flags::{env_flag_truthy, gc_stress};
use super::sizing::{INITIAL_YOUNG_WORDS, size_for};
use super::*;
use crate::bytecode::value::Value;
use crate::frozen::FrozenArea;
use std::ffi::OsStr;
use std::sync::Arc;

/// An empty root set (everything is garbage) — for tests that allocate
/// without a VM stack to root from.
struct NoRoots;

impl Roots for NoRoots {
    fn for_each(&mut self, _f: &mut dyn FnMut(&mut Value)) {}
}

/// Words a `Str` of `byte_len` bytes occupies (header + len word + data).
fn str_words(byte_len: usize) -> usize {
    2 + byte_len.div_ceil(8)
}

/// Words a `Tuple` of `n` elements occupies (header + count + elements).
fn tuple_words(n: usize) -> usize {
    2 + n
}

/// Words a `Binary` box occupies (header + 5 payload words).
const BIN_WORDS: usize = 6;

/// A heap with stress mode forced off, for tests that assert exact GC
/// counts/capacities (stress behavior has its own explicit test).
fn quiet_heap() -> ProcHeap {
    let mut h = ProcHeap::new();
    h.set_stress(false);
    h
}

fn tuple_elem(v: Value, i: usize) -> Value {
    v.as_tuple().expect("expected a tuple")[i]
}

fn read_str(v: Value) -> String {
    v.as_str().expect("expected a str").to_string()
}

#[test]
fn bump_allocation_is_contiguous_and_bounded() {
    let mut s = Space::with_capacity(8);
    let a = s.alloc(3).expect("3 of 8 words");
    let b = s.alloc(5).expect("remaining 5 words");
    // Contiguous bump: the second block starts exactly 3 words after the
    // first, and the space is now exactly full.
    assert_eq!(b as usize, a as usize + 3 * std::mem::size_of::<u64>());
    assert_eq!(s.used(), 8);
    assert_eq!(s.available(), 0);
    assert!(s.alloc(1).is_none(), "a full space must refuse to allocate");
    s.reset();
    assert_eq!(s.used(), 0);
    assert!(s.alloc(8).is_some(), "reset reclaims the whole slab");
}

#[test]
fn classification_is_by_address_range() {
    let mut heap = ProcHeap::new();
    heap.note_ensured(4);
    let p = heap.alloc_young(4).expect("fresh heap has room") as usize;
    assert!(heap.contains(p));
    assert!(heap.contains(p + 3 * std::mem::size_of::<u64>()));

    // A pointer into a different heap is foreign here and mine there.
    let mut other = ProcHeap::new();
    other.note_ensured(1);
    let q = other.alloc_young(1).expect("fresh heap has room") as usize;
    assert!(!heap.contains(q));
    assert!(other.contains(q));

    // Null/zero is never classified as owned.
    assert!(!heap.contains(0));
}

#[test]
fn default_heap_is_empty_until_ensured() {
    let mut h = ProcHeap::default();
    h.set_stress(false);
    assert_eq!(h.capacity_words(), 0);
    assert_eq!(h.used_words(), 0);
    assert_eq!(h.off_heap_head(), 0);
    assert_eq!(h.high_water(), 0);
    assert_eq!(h.minor_count(), 0);
    // The placeholder owns no slab: any allocation need at all demands a
    // collection/growth first…
    assert!(h.needs_collect(1));
    // …and ensure() installs a young space on demand.
    h.ensure(4, &mut NoRoots);
    assert!(h.young_capacity() >= INITIAL_YOUNG_WORDS);
    let t = Value::tuple_in(&mut h, &[Value::small_int(1), Value::small_int(2)]);
    assert!(h.contains(t.object_addr().expect("heap value")));
}

#[test]
fn fresh_heap_has_the_initial_young_capacity() {
    let heap = ProcHeap::new();
    assert_eq!(heap.capacity_words(), INITIAL_YOUNG_WORDS);
    let sized = ProcHeap::with_young_capacity(1024);
    assert_eq!(sized.capacity_words(), 1024);
}

#[test]
fn growth_table_roughly_doubles_and_covers_need() {
    // Below the floor, sizes snap to the initial size; above it, the table
    // is powers of two from the initial size, always covering the need.
    assert_eq!(size_for(0), INITIAL_YOUNG_WORDS);
    assert_eq!(size_for(257), 512);
    assert_eq!(size_for(5000), 8192);
}

#[test]
fn heap_moves_keep_interior_pointers_valid() {
    // Box contents do not move with the owning struct: an address taken
    // before a move still classifies as "mine" after, which is the
    // property process migration relies on.
    let mut heap = ProcHeap::new();
    heap.note_ensured(2);
    let p = heap.alloc_young(2).expect("fresh heap has room") as usize;
    let moved = heap;
    assert!(moved.contains(p));
}

#[test]
fn stress_flag_parsing() {
    assert!(!env_flag_truthy(None));
    assert!(!env_flag_truthy(Some(OsStr::new(""))));
    assert!(!env_flag_truthy(Some(OsStr::new("0"))));
    assert!(env_flag_truthy(Some(OsStr::new("1"))));
    // Spelled differently still means "on" — only empty/"0" opt out, so
    // a typo'd AL_GC_STRESS=true cannot silently disable the gate.
    assert!(env_flag_truthy(Some(OsStr::new("true"))));
}

#[test]
fn needs_collect_tracks_capacity_and_stress() {
    let heap = ProcHeap::new();
    // More than the whole young space always demands a collection.
    assert!(heap.needs_collect(INITIAL_YOUNG_WORDS + 1));
    // With room to spare the answer depends on the mode: stress collects
    // at every safepoint, normal mode only under pressure. (The suite
    // also runs under AL_GC_STRESS=1, so both branches must hold here.)
    if gc_stress() {
        assert!(heap.needs_collect(0));
        assert!(heap.needs_collect(1));
    } else {
        assert!(!heap.needs_collect(0));
        assert!(!heap.needs_collect(1));
        assert!(!heap.needs_collect(INITIAL_YOUNG_WORDS));
    }
    // The per-heap override wins over the environment in both directions.
    let mut forced = ProcHeap::new();
    forced.set_stress(true);
    assert!(forced.needs_collect(0));
    forced.set_stress(false);
    assert!(!forced.needs_collect(0));
}

#[test]
fn ensured_budget_covers_allocations_until_replaced() {
    let mut heap = ProcHeap::new();
    heap.note_ensured(5);
    assert!(heap.alloc_young(3).is_some());
    assert!(heap.alloc_young(2).is_some());
    // A fresh ensure replaces (not extends) the budget.
    heap.note_ensured(2);
    assert!(heap.alloc_young(2).is_some());
    assert_eq!(heap.used_words(), 7);
}

#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "rooting-rule violation")]
fn alloc_without_ensure_panics_in_debug() {
    let mut heap = ProcHeap::new();
    // Plenty of capacity — the watermark check fires on the missing
    // ensure, not on exhaustion.
    heap.alloc_young(1);
}

#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "rooting-rule violation")]
fn alloc_beyond_ensured_budget_panics_in_debug() {
    let mut heap = ProcHeap::new();
    heap.note_ensured(2);
    heap.alloc_young(2);
    heap.alloc_young(1);
}

#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "ensure() noted a budget")]
fn noting_an_unsecured_budget_panics_in_debug() {
    let mut heap = ProcHeap::default();
    // The placeholder heap has no capacity at all: claiming an ensured
    // word here means ensure() forgot to collect/grow first.
    heap.note_ensured(1);
}

// ---- copy_graph / collections --------------------------------------------

// copy_graph must preserve sharing, asserted via object/word counts —
// a tuple referencing one string twice must copy as 2 objects, not 3.
#[test]
fn copy_graph_preserves_sharing() {
    let mut h = quiet_heap();
    h.ensure(str_words(8) + tuple_words(2), &mut NoRoots);
    let s = Value::str_in(&mut h, "shared!!");
    let t = Value::tuple_in(&mut h, &[s, s]);
    let mut roots = [t];
    let copied = h.minor_gc(&mut roots, 0);
    let expected = str_words(8) + tuple_words(2);
    assert_eq!(copied, expected, "exactly one copy of the shared string");
    assert_eq!(h.used_words(), expected);
    let a = tuple_elem(roots[0], 0);
    let b = tuple_elem(roots[0], 1);
    assert_eq!(
        a.object_addr(),
        b.object_addr(),
        "shared subtree must stay shared after the copy"
    );
    assert_eq!(read_str(a), "shared!!");
}

#[test]
fn minor_collects_garbage_and_rewrites_roots() {
    let mut h = quiet_heap();
    h.ensure(str_words(4) + str_words(27), &mut NoRoots);
    let live = Value::str_in(&mut h, "keep");
    let _garbage = Value::str_in(&mut h, "this string is unreachable");
    let mut roots = [live];
    h.minor_gc(&mut roots, 0);
    assert_ne!(
        roots[0].object_addr(),
        live.object_addr(),
        "the root slot must be rewritten to the moved object"
    );
    assert_eq!(h.used_words(), str_words(4), "garbage must not survive");
    assert_eq!(read_str(roots[0]), "keep");
}

#[test]
fn survivors_tenure_on_their_second_minor() {
    let mut h = quiet_heap();
    h.ensure(str_words(9), &mut NoRoots);
    let mut roots = [Value::str_in(&mut h, "tenure-me")];
    h.minor_gc(&mut roots, 0);
    assert_eq!(h.old_used(), 0, "first minor: still young");
    assert_eq!(h.young_used(), str_words(9));
    h.minor_gc(&mut roots, 0);
    assert_eq!(
        h.old_used(),
        str_words(9),
        "second minor: below the high-water mark, so it tenures"
    );
    assert_eq!(h.young_used(), 0);
    assert_eq!(read_str(roots[0]), "tenure-me");

    // young→old references are fine (the reverse cannot exist): a new
    // young tuple pointing at the tenured string survives a minor with
    // the old object left in place.
    h.ensure(tuple_words(1), &mut roots);
    let t = Value::tuple_in(&mut h, &[roots[0]]);
    let mut roots2 = [t, roots[0]];
    h.minor_gc(&mut roots2, 0);
    assert_eq!(h.old_used(), str_words(9));
    assert_eq!(
        tuple_elem(roots2[0], 0).object_addr(),
        roots2[1].object_addr(),
        "the old object must not move in a minor"
    );
    assert_eq!(read_str(tuple_elem(roots2[0], 0)), "tenure-me");
}

#[test]
fn major_reclaims_tenured_garbage() {
    let mut h = quiet_heap();
    h.ensure(str_words(8), &mut NoRoots);
    let mut live = [Value::str_in(&mut h, "live-old")];
    h.minor_gc(&mut live, 0);
    h.minor_gc(&mut live, 0);
    assert_eq!(h.old_used(), str_words(8));

    // Tenure a pile of garbage: root each string across two minors so it
    // reaches old space, then unroot it.
    for i in 0..100 {
        let mut r = [live[0], Value::nil()];
        h.ensure(str_words(32), &mut r);
        r[1] = Value::str_in(&mut h, &format!("tenured-garbage-{i:03}-aaaaaaaaaaaa"));
        h.minor_gc(&mut r, 0);
        h.minor_gc(&mut r, 0);
        live[0] = r[0];
    }
    let old_before = h.old_used();
    assert!(old_before > str_words(32) * 50, "garbage piled up in old");

    let copied = h.major_gc(&mut live, 0);
    assert_eq!(copied, str_words(8));
    assert_eq!(h.old_used(), 0, "major empties the old generation");
    assert_eq!(h.young_used(), str_words(8));
    assert_eq!(read_str(live[0]), "live-old");
    assert_eq!(h.stats().major_gcs, 1);
}

#[test]
fn ensure_escalates_to_major_past_the_threshold() {
    let mut h = quiet_heap();
    h.major_threshold = 32;
    // Tenure ~40 garbage words (each str is 10 words).
    for _ in 0..4 {
        let mut r = [Value::nil()];
        h.ensure(str_words(64), &mut r);
        r[0] = Value::str_in(&mut h, &"x".repeat(64));
        h.minor_gc(&mut r, 0);
        h.minor_gc(&mut r, 0);
    }
    assert!(h.old_used() > 32);
    // Force the next ensure to collect; the minor then escalates.
    h.set_stress(true);
    h.ensure(1, &mut NoRoots);
    assert!(h.stats().major_gcs >= 1, "ensure must escalate to a major");
    assert_eq!(h.old_used(), 0);
}

#[test]
fn ensure_grows_young_through_the_size_table() {
    let mut h = quiet_heap();
    assert_eq!(h.young_capacity(), 256);
    let copied = h.ensure(5000, &mut NoRoots);
    assert_eq!(copied, 0, "nothing was live to copy");
    assert_eq!(h.young_capacity(), 8192);
    assert_eq!(h.stats().minor_gcs, 1);
    // The ensured budget is real: a 4000-byte string fits.
    let s = Value::str_in(&mut h, &"y".repeat(4000));
    assert_eq!(s.as_str().map(str::len), Some(4000));
}

#[test]
fn sustained_low_occupancy_shrinks_young() {
    let mut h = quiet_heap();
    h.ensure(6000, &mut NoRoots);
    assert_eq!(h.young_capacity(), 8192);
    let mut roots: Vec<Value> = Vec::new();
    for _ in 0..8 {
        h.minor_gc(&mut roots, 0);
    }
    assert_eq!(
        h.young_capacity(),
        INITIAL_YOUNG_WORDS,
        "an empty heap shrinks back to the initial size"
    );
    assert!(h.stats().young_shrinks >= 1);
}

// Arc refcounts must balance across a collection: the live binary keeps
// its backing (relinked, no refcount churn), the dead binary releases it
// through the off-heap sweep, and heap teardown releases whatever
// survives.
#[test]
fn off_heap_sweep_drops_dead_and_keeps_live_backings() {
    let live_backing: Arc<[u8]> = vec![1, 2, 3].into();
    let dead_backing: Arc<[u8]> = vec![9, 9].into();
    let mut h = quiet_heap();
    h.ensure(2 * BIN_WORDS, &mut NoRoots);
    let live = Value::binary_from_arc_in(&mut h, Arc::clone(&live_backing), 24);
    let _dead = Value::binary_from_arc_in(&mut h, Arc::clone(&dead_backing), 16);
    assert_eq!(Arc::strong_count(&live_backing), 2);
    assert_eq!(Arc::strong_count(&dead_backing), 2);

    let mut roots = [live];
    h.minor_gc(&mut roots, 0);
    assert_eq!(
        Arc::strong_count(&live_backing),
        2,
        "a live binary's backing moves with it (no refcount churn)"
    );
    assert_eq!(
        Arc::strong_count(&dead_backing),
        1,
        "a dead binary's backing is released by the off-heap sweep"
    );
    assert_eq!(h.stats().binaries_dropped, 1);
    let moved = roots[0].as_binary().expect("expected a binary");
    assert_eq!(moved.bit_len(), 24);
    assert_eq!(
        moved.backing().as_ptr(),
        Arc::as_ptr(&live_backing) as *const u8,
        "zero-copy: the moved box still points at the same backing"
    );
    // The rebuilt off-heap list heads at the moved box.
    assert_eq!(h.off_heap_head(), roots[0].object_addr().expect("heap"));

    drop(h);
    assert_eq!(
        Arc::strong_count(&live_backing),
        1,
        "heap teardown releases surviving binaries"
    );
}

#[test]
fn binaries_tenure_and_release_on_heap_drop() {
    let backing: Arc<[u8]> = vec![5u8; 32].into();
    let mut h = quiet_heap();
    h.ensure(BIN_WORDS, &mut NoRoots);
    let mut roots = [Value::binary_from_arc_in(&mut h, Arc::clone(&backing), 256)];
    h.minor_gc(&mut roots, 0);
    h.minor_gc(&mut roots, 0);
    assert_eq!(h.young_used(), 0);
    assert_eq!(h.old_used(), BIN_WORDS);
    assert_eq!(
        Arc::strong_count(&backing),
        2,
        "tenuring relinks the box, it does not duplicate the backing"
    );
    // Major moves it again; still exactly one heap-owned count.
    h.major_gc(&mut roots, 0);
    assert_eq!(Arc::strong_count(&backing), 2);
    assert_eq!(roots[0].as_binary().map(|b| b.bit_len()), Some(256));
    drop(h);
    assert_eq!(Arc::strong_count(&backing), 1);
}

#[test]
fn spawn_copy_clones_the_graph_and_shares_backings() {
    let backing: Arc<[u8]> = vec![7u8; 16].into();
    let mut parent = quiet_heap();
    let graph_words = str_words(6) + BIN_WORDS + tuple_words(3);
    parent.ensure(graph_words + str_words(8), &mut NoRoots);
    let s = Value::str_in(&mut parent, "shared");
    let b = Value::binary_from_arc_in(&mut parent, Arc::clone(&backing), 128);
    let t = Value::tuple_in(&mut parent, &[s, s, b]);
    // Unreachable from t; must not be copied into the child.
    let _noise = Value::str_in(&mut parent, "not-mine");

    let (child, child_root, copied) = parent.spawn_seed(t);
    // Pass 1 measures, pass 2 right-sizes: the graph is copied twice.
    assert_eq!(copied, graph_words * 2);
    assert_eq!(
        child.used_words(),
        graph_words,
        "the child arena holds exactly the closure graph"
    );
    assert_eq!(
        Arc::strong_count(&backing),
        3,
        "external + parent box + child box (bytes shared zero-copy)"
    );
    // The child graph is intact, with sharing preserved, in child memory.
    assert!(child.contains(child_root.object_addr().expect("heap")));
    assert_eq!(
        tuple_elem(child_root, 0).object_addr(),
        tuple_elem(child_root, 1).object_addr()
    );
    assert_eq!(read_str(tuple_elem(child_root, 0)), "shared");
    // The child's off-heap list holds its binary box copy.
    assert_eq!(
        child.off_heap_head(),
        tuple_elem(child_root, 2).object_addr().expect("heap")
    );
    // The parent graph survived the clone fully readable (headers were
    // restored), and the parent still collects normally afterwards.
    assert_eq!(read_str(tuple_elem(t, 0)), "shared");
    assert_eq!(tuple_elem(t, 0).object_addr(), s.object_addr());
    assert_eq!(parent.used_words(), graph_words + str_words(8));
    let mut roots = [t];
    parent.minor_gc(&mut roots, 0);
    assert_eq!(read_str(tuple_elem(roots[0], 0)), "shared");

    drop(parent);
    assert_eq!(Arc::strong_count(&backing), 2);
    drop(child);
    assert_eq!(Arc::strong_count(&backing), 1);
}

#[test]
fn spawn_copy_of_an_immediate_root() {
    let mut parent = quiet_heap();
    let (child, root, copied) = parent.spawn_seed(Value::small_int(42));
    assert_eq!(root.as_int(), Some(42));
    assert_eq!(copied, 0);
    assert_eq!(child.used_words(), 0);
}

// Spawn-copy must preserve sharing on a DAG, asserted via object counts.
// The DAG
//
//        root
//        /  \
//       a    b
//        \  /
//       shared
//        /  \
//      leaf  leaf      (one node, referenced twice)
//
// is 5 objects / 17 words; a copy that loses sharing anywhere
// tree-expands toward 9 objects / 24 words, which the exact word count
// rules out.
#[test]
fn spawn_copy_preserves_dag_sharing_by_object_count() {
    let mut parent = quiet_heap();
    let dag_words = str_words(4) + tuple_words(2) * 2 + tuple_words(1) * 2;
    parent.ensure(dag_words, &mut NoRoots);
    let leaf = Value::str_in(&mut parent, "leaf");
    let shared = Value::tuple_in(&mut parent, &[leaf, leaf]);
    let a = Value::tuple_in(&mut parent, &[shared]);
    let b = Value::tuple_in(&mut parent, &[shared]);
    let root = Value::tuple_in(&mut parent, &[a, b]);

    let (child, copy, _copied) = parent.spawn_seed(root);
    assert_eq!(
        child.used_words(),
        dag_words,
        "the copy must hold 5 distinct objects, not a 9-node tree"
    );
    // And the diamond really is a diamond in the copy.
    let ca = tuple_elem(copy, 0);
    let cb = tuple_elem(copy, 1);
    assert_ne!(ca.object_addr(), cb.object_addr());
    assert_eq!(
        tuple_elem(ca, 0).object_addr(),
        tuple_elem(cb, 0).object_addr(),
        "both arms must reference the same shared node"
    );
    let cshared = tuple_elem(ca, 0);
    assert_eq!(
        tuple_elem(cshared, 0).object_addr(),
        tuple_elem(cshared, 1).object_addr()
    );
    assert_eq!(read_str(tuple_elem(cshared, 0)), "leaf");
}

#[test]
fn publish_frozen_deep_copies_and_reuses_frozen_subgraphs() {
    let area = Arc::new(FrozenArea::new());
    let mut builder = area.builder();
    let mut h = quiet_heap();
    h.ensure(str_words(4) + tuple_words(2), &mut NoRoots);
    let s = Value::str_in(&mut h, "glob");
    let t = Value::tuple_in(&mut h, &[s, s]);

    let (frozen_root, words) = h.publish_frozen(&mut builder, t);
    assert_eq!(
        words,
        str_words(4) + tuple_words(2),
        "sharing preserved in the copy"
    );
    let frozen_addr = frozen_root.object_addr().expect("heap value");
    assert!(area.contains(frozen_addr as *const u64));
    assert!(!h.contains(frozen_addr));
    assert_eq!(
        tuple_elem(frozen_root, 0).object_addr(),
        tuple_elem(frozen_root, 1).object_addr()
    );
    assert_eq!(read_str(tuple_elem(frozen_root, 0)), "glob");
    // The source graph is untouched.
    assert_eq!(read_str(tuple_elem(t, 0)), "glob");

    // The process GC leaves frozen pointers alone (range check fails).
    let mut roots = [frozen_root];
    h.minor_gc(&mut roots, 0);
    assert_eq!(roots[0].object_addr(), Some(frozen_addr));

    // Publishing a graph that already references frozen data does not
    // re-copy the frozen part.
    h.ensure(tuple_words(1), &mut NoRoots);
    let t2 = Value::tuple_in(&mut h, &[frozen_root]);
    let (frozen2, words2) = h.publish_frozen(&mut builder, t2);
    assert_eq!(words2, tuple_words(1), "only the new tuple is copied");
    assert_eq!(tuple_elem(frozen2, 0).object_addr(), Some(frozen_addr));

    // Frozen data outlives the publishing process.
    drop(h);
    assert_eq!(read_str(tuple_elem(frozen_root, 1)), "glob");
}

#[test]
fn frozen_binaries_keep_their_backing_alive() {
    let backing: Arc<[u8]> = vec![3u8; 8].into();
    let area = Arc::new(FrozenArea::new());
    let mut builder = area.builder();
    let mut h = quiet_heap();
    h.ensure(BIN_WORDS, &mut NoRoots);
    let b = Value::binary_from_arc_in(&mut h, Arc::clone(&backing), 64);
    let (frozen_b, _) = h.publish_frozen(&mut builder, b);
    assert_eq!(
        Arc::strong_count(&backing),
        3,
        "external + heap box + frozen box"
    );
    drop(h);
    assert_eq!(
        Arc::strong_count(&backing),
        2,
        "the frozen box keeps its count for the program lifetime"
    );
    let fr = frozen_b.as_binary().expect("expected a binary");
    assert_eq!(fr.bit_len(), 64);
    // Frozen keeps no off-heap list: the link word is cleared.
    let addr = frozen_b.object_addr().expect("heap value");
    assert_eq!(super::copy::off_heap_next(addr), 0);
}

// Deep recursion + allocation churn: a deep linked list of
// nested tuples is built under the ensure discipline, collected, and
// walked back: the Cheney scan loop must not recurse, or this overflows
// the Rust stack long before 30k. Stress mode collects on every ensure,
// which is O(depth) work per node — keep the chain shorter there.
#[test]
fn deep_graphs_copy_iteratively() {
    let depth: i64 = if gc_stress() { 1_500 } else { 30_000 };
    let mut h = ProcHeap::new();
    let mut chain = Value::small_int(0);
    for i in 1..=depth {
        let mut roots = [chain];
        h.ensure(tuple_words(2), &mut roots);
        chain = Value::tuple_in(&mut h, &[Value::small_int(i), roots[0]]);
    }
    let mut roots = [chain];
    h.minor_gc(&mut roots, 0);
    h.major_gc(&mut roots, 0);
    assert_eq!(h.used_words(), depth as usize * tuple_words(2));
    // Walk the chain back down, verifying order and length.
    let mut cur = roots[0];
    let mut expect = depth;
    while let Some(elems) = cur.as_tuple() {
        let head = elems[0];
        let next = elems[1];
        assert_eq!(head.as_int(), Some(expect));
        expect -= 1;
        cur = next;
    }
    assert_eq!(cur.as_int(), Some(0), "chain must end at the seed value");
    assert_eq!(expect, 0, "chain length must be exactly the build depth");
}

#[test]
fn stress_mode_collects_on_every_ensure() {
    let mut h = ProcHeap::new();
    h.set_stress(true);
    let mut roots: Vec<Value> = Vec::new();
    for i in 0..500 {
        h.ensure(str_words(22), &mut roots);
        roots.push(Value::str_in(&mut h, &format!("value-{i:04}-padding-pad")));
    }
    assert!(h.stats().minor_gcs >= 500, "every ensure must collect");
    for (i, v) in roots.iter().enumerate() {
        assert_eq!(read_str(*v), format!("value-{i:04}-padding-pad"));
    }
}

#[test]
fn ensure_reports_copied_words_for_reduction_charging() {
    let mut h = quiet_heap();
    h.ensure(str_words(40), &mut NoRoots);
    let mut roots = [Value::str_in(&mut h, &"z".repeat(40))];
    // Force the next ensure to collect: the live graph is copied and the
    // word count comes back for the VM to charge as reductions.
    h.set_stress(true);
    let copied = h.ensure(0, &mut roots);
    assert_eq!(copied, str_words(40));
    assert_eq!(roots[0].as_str().map(str::len), Some(40));
}

// ---- Safepoint churn through ensure(need, roots) --------------------------

// Tens of thousands of safepoints with transient garbage and a slowly
// growing live set, driven through the real protocol (`ensure` with the
// live set as roots): every post-ensure allocation must succeed, occupancy
// must never exceed capacity, the young space must grow to hold the live
// set without running away, and every collection must be recorded.
// (Under AL_GC_STRESS=1 every iteration collects.)
#[test]
fn allocation_churn_keeps_heap_accounting_consistent() {
    let mut heap = ProcHeap::new();
    let mut live: Vec<Value> = Vec::new();
    let mut collections = 0u64;
    for i in 0..20_000usize {
        // One survivor every 64 iterations, transient garbage otherwise.
        let need = str_words(5) + (i % 13);
        if heap.needs_collect(need) {
            collections += 1;
        }
        heap.ensure(need, &mut live);
        let s = Value::str_in(&mut heap, "churn");
        if i % 64 == 0 {
            live.push(s); // rooted: must survive every later collection
        }
        assert!(heap.young_used() <= heap.young_capacity());
    }
    assert!(collections > 0, "churn at this volume must collect");
    let s = heap.stats();
    assert_eq!(
        s.minor_gcs, collections,
        "every ensure-triggered collection runs exactly one minor"
    );
    // Every survivor is still intact and still rooted.
    assert_eq!(live.len(), 20_000 / 64 + 1);
    for v in &live {
        assert_eq!(read_str(*v), "churn");
    }
    // The live set tops out around a thousand words: the heap must hold it
    // (across young + old) without running away.
    assert!(heap.used_words() >= live.len() * str_words(5));
    assert!(heap.capacity_words() <= 64 * 1024);
}
