//! A per-process mimalloc heap: the reference-counting allocator's storage.
//!
//! Each al process owns one [`MiHeap`]. Under reference counting an object is
//! allocated from it while the process runs, freed individually as its count
//! hits zero, and the *whole* heap is destroyed in one shot when the process
//! dies (`mi_heap_destroy` — O(1) teardown, no per-object frees). That last
//! property is what preserves today's "drop the whole arena at death" win.
//!
//! # The `Send` invariant (the one piece of `unsafe` reasoning)
//!
//! A `mi_heap_t` is owned by one thread at a time. al upholds this by
//! construction: a process — and therefore its heap — is run by exactly one
//! scheduler core at any instant, and migration moves the whole process across
//! a synchronized handoff (the scheduler's run-queue / inbox), so the heap is
//! never touched by two cores concurrently. That serialized-single-owner
//! discipline is precisely what makes moving the heap between threads sound,
//! which is why [`MiHeap`] is `Send` even though it wraps a raw pointer. The
//! day a process could run on two cores at once, this `unsafe impl` breaks.

// Designated unsafe module: the mimalloc FFI and the Send assertion live here
// and nowhere else, behind a safe API.
#![allow(unsafe_code)]

use std::collections::HashMap;
use std::ptr::NonNull;

use libmimalloc_sys::{mi_free, mi_heap_destroy, mi_heap_malloc_aligned, mi_heap_new, mi_heap_t};

use crate::bytecode::value::{
    RC_PREFIX_WORDS, Value, binary_clone_backing, for_each_child, header_has_off_heap_link,
    header_total_words, rc_increment,
};

/// A per-process mimalloc heap. See the module docs for the `Send` invariant
/// and the wholesale-destroy teardown.
pub struct MiHeap {
    heap: NonNull<mi_heap_t>,
}

// SAFETY: a process and its heap are owned by exactly one scheduler core at a
// time; migration hands the process across a synchronized edge, so no two cores
// ever touch the heap concurrently. See the module docs.
unsafe impl Send for MiHeap {}

const fn assert_send<T: Send>() {}
const _: () = assert_send::<MiHeap>();

// Allocation/free are exercised by this module's tests but not yet wired into
// `ProcHeap`'s hot path; that happens at Stage 4 (the flip), when this `allow`
// is removed.
#[allow(dead_code)]
impl MiHeap {
    /// Create a fresh, empty heap.
    #[allow(clippy::expect_used)]
    pub fn new() -> MiHeap {
        // SAFETY: `mi_heap_new` is always safe to call; it returns a fresh
        // non-null heap (or aborts internally on OOM).
        let heap = unsafe { mi_heap_new() };
        MiHeap {
            heap: NonNull::new(heap).expect("mi_heap_new returned null"),
        }
    }

    /// Allocate `words` 8-byte words, 8-byte aligned, from this heap. The
    /// storage is uninitialized; the caller writes the object image into it.
    #[inline]
    #[allow(clippy::expect_used)]
    pub fn alloc_words(&self, words: usize) -> NonNull<u64> {
        let bytes = words * size_of::<u64>();
        // SAFETY: `self.heap` is a live heap owned by this thread; mimalloc
        // returns 8-aligned storage of `bytes` bytes or null on OOM.
        let p = unsafe { mi_heap_malloc_aligned(self.heap.as_ptr(), bytes, align_of::<u64>()) };
        NonNull::new(p.cast::<u64>()).expect("mi_heap_malloc_aligned returned null")
    }

    /// Allocate storage for one reference-counted object of `words` (header +
    /// payload) words, reserving the leading refcount slot — initialized to 1 —
    /// and returning a pointer to the HEADER, so every header-relative offset
    /// is unchanged. The object is reclaimed by reference counting
    /// (`bytecode::value::release`), which frees the allocation at `rc_slot`.
    #[inline]
    pub fn alloc_object(&self, words: usize) -> NonNull<u64> {
        let raw = self.alloc_words(RC_PREFIX_WORDS + words);
        // SAFETY: fresh allocation of `RC_PREFIX_WORDS + words` (>= 2) words;
        // the first word is the refcount, the object begins after it.
        unsafe {
            raw.as_ptr().write(1); // refcount = 1 (the constructing handle)
            NonNull::new_unchecked(raw.as_ptr().add(RC_PREFIX_WORDS))
        }
    }

    /// Free a single object. mimalloc recovers the owning heap from the
    /// pointer's segment metadata, so no heap handle is needed — and a free on
    /// a different thread than the one that allocated (after a migration) is
    /// routed to the owning heap's deferred-free list.
    ///
    /// # Safety
    /// `addr` must be a live allocation handed out by some [`MiHeap`] (this one
    /// or one this process adopted via migration) and must not be freed again.
    #[inline]
    pub unsafe fn free(addr: NonNull<u64>) {
        // SAFETY: the caller guarantees `addr` is a live, not-yet-freed
        // mimalloc allocation.
        unsafe { mi_free(addr.as_ptr().cast()) };
    }
}

impl Drop for MiHeap {
    fn drop(&mut self) {
        // Wholesale teardown: frees every block this heap owns in one shot,
        // running no per-object frees. Any off-heap resource an object owns
        // (e.g. a Binary's `Arc<[u8]>` backing) must already have been released
        // by the caller before the heap is dropped — `mi_heap_destroy` will not
        // run those destructors.
        // SAFETY: the heap is live and, by the module's Send invariant, owned
        // solely by this thread at drop time.
        unsafe { mi_heap_destroy(self.heap.as_ptr()) };
    }
}

impl Default for MiHeap {
    fn default() -> MiHeap {
        MiHeap::new()
    }
}

// `copy_node`/`rc_copy_graph` are wired into `ProcHeap::spawn` at Stage 4.
/// Shallow-copy one node of a spawn graph into `dst`, returning the value to
/// store in the parent slot. Immediates and immortal (frozen) values pass
/// through unchanged (frozen objects are shared, never copied). A node already
/// copied (a DAG join) is shared: its existing copy's refcount is bumped. A
/// first-seen node is byte-copied (its child slots still point at the *source*
/// graph; the caller's queue links them later) and queued.
#[allow(dead_code)]
fn copy_node(
    src: Value,
    dst: &MiHeap,
    map: &mut HashMap<usize, NonNull<u64>>,
    queue: &mut Vec<NonNull<u64>>,
) -> Value {
    if !src.is_heap() || src.is_immortal() {
        return src;
    }
    let src_obj = src.heap_obj();
    let src_addr = src_obj as usize;
    if let Some(&d) = map.get(&src_addr) {
        // SAFETY: `d` is a live mortal object this copy already created.
        unsafe { rc_increment(d.as_ptr()) };
        return Value::from_object_ptr(d);
    }
    // SAFETY: `src` is a live heap object; its header gives its total size.
    let words = unsafe { header_total_words(*src_obj) };
    let d = dst.alloc_object(words); // refcount = 1 (this first reference)
    // SAFETY: `d` has `words` header+payload words; copy the image verbatim,
    // then share the off-heap Arc backing if this is a Binary box.
    unsafe {
        std::ptr::copy_nonoverlapping(src_obj, d.as_ptr(), words);
        if header_has_off_heap_link(*d.as_ptr()) {
            binary_clone_backing(d.as_ptr());
        }
    }
    map.insert(src_addr, d);
    queue.push(d);
    Value::from_object_ptr(d)
}

/// Deep-copy the value graph reachable from `root` into `dst`, returning the
/// copied root. This is the reference-counting replacement for the Cheney
/// spawn copy: a single Clone pass with no forwarding pointers and no native
/// recursion (an explicit queue links children, so a deep graph cannot
/// overflow the stack). It
///
/// - preserves sharing — a DAG stays a DAG — via a `src → dst` address map;
/// - sets each copy's refcount to its in-graph reference count (one per edge);
/// - shares immortal (frozen) objects without copying them;
/// - shares `Binary` byte backings by bumping their `Arc` (no byte copy).
#[allow(dead_code)]
pub fn rc_copy_graph(root: Value, dst: &MiHeap) -> Value {
    let mut map: HashMap<usize, NonNull<u64>> = HashMap::new();
    let mut queue: Vec<NonNull<u64>> = Vec::new();
    let root_copy = copy_node(root, dst, &mut map, &mut queue);
    let mut i = 0;
    while i < queue.len() {
        let d = queue[i];
        i += 1;
        // SAFETY: `d` is a freshly copied object whose child slots still hold
        // source pointers; rewrite each to point into `dst`.
        unsafe {
            for_each_child(d.as_ptr(), &mut |child: &mut Value| {
                *child = copy_node(*child, dst, &mut map, &mut queue);
            });
        }
    }
    root_copy
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_write_read_then_free() {
        let h = MiHeap::new();
        let p = h.alloc_words(4);
        assert_eq!(p.as_ptr() as usize % 8, 0, "8-byte aligned");
        // SAFETY: `p` is a live 4-word allocation we own exclusively here.
        unsafe {
            for i in 0..4 {
                p.as_ptr().add(i).write(0xABCD_0000 + i as u64);
            }
            for i in 0..4 {
                assert_eq!(p.as_ptr().add(i).read(), 0xABCD_0000 + i as u64);
            }
            MiHeap::free(p);
        }
    }

    #[test]
    fn destroy_reclaims_everything_without_per_object_free() {
        let h = MiHeap::new();
        // Many allocations, none individually freed: `mi_heap_destroy` on drop
        // reclaims them all in one shot.
        for _ in 0..10_000 {
            let _ = h.alloc_words(8);
        }
        drop(h);
    }

    #[test]
    fn miheap_is_send() {
        fn takes_send<T: Send>() {}
        takes_send::<MiHeap>();
    }
}

/// Exercises the reference-counting free-at-zero work list (`value::release`)
/// against graphs built directly in a `MiHeap`, before the VM is wired to RC.
#[cfg(test)]
mod rc_free_tests {
    use super::MiHeap;
    use crate::bytecode::value::{HeapTag, Value, pack_header, rc_slot, release};

    /// Build an `n`-element tuple object in `mi` with refcount 1 over the given
    /// element bit patterns. Counts of heap elements are left untouched, so the
    /// caller controls the reference graph exactly.
    unsafe fn make_tuple(mi: &MiHeap, elems: &[Value]) -> Value {
        let n = elems.len();
        let payload = 1 + n; // [count][elements…]
        let obj = mi.alloc_object(1 + payload); // header + payload
        let p = obj.as_ptr();
        // SAFETY: `obj` is a fresh object with header + payload words reserved.
        unsafe {
            p.write(pack_header(HeapTag::Tuple, payload, false));
            p.add(1).write(n as u64);
            for (i, e) in elems.iter().enumerate() {
                p.add(2 + i).write(e.to_bits());
            }
        }
        Value::from_object_ptr(obj)
    }

    /// The refcount currently stored for a heap value.
    unsafe fn count(v: Value) -> u64 {
        // SAFETY: `v` is a mortal heap object built by `make_tuple`.
        unsafe { *rc_slot(v.heap_obj()) }
    }

    #[test]
    fn releasing_a_leaf_frees_it() {
        let mi = MiHeap::new();
        let leaf = unsafe { make_tuple(&mi, &[Value::small_int(7)]) };
        assert_eq!(unsafe { count(leaf) }, 1);
        release(leaf); // count 1 -> 0 -> freed
        drop(mi); // wholesale destroy; no double free of the already-freed block
    }

    #[test]
    fn shared_child_freed_only_at_last_reference() {
        let mi = MiHeap::new();
        let child = unsafe { make_tuple(&mi, &[Value::small_int(1)]) };
        // Model the child as referenced by two parents (and nothing else).
        unsafe {
            *rc_slot(child.heap_obj()) = 2;
        }
        let p1 = unsafe { make_tuple(&mi, &[child]) };
        let p2 = unsafe { make_tuple(&mi, &[child]) };

        release(p1); // p1 freed; child 2 -> 1, still live
        assert_eq!(unsafe { count(child) }, 1, "child survives first release");

        release(p2); // p2 freed; child 1 -> 0 -> freed
        drop(mi);
    }

    #[test]
    fn deep_chain_frees_without_native_recursion() {
        // A long single-owner chain: releasing the head must free every node
        // iteratively. Recursive `Drop` would overflow the stack here.
        let mi = MiHeap::new();
        let mut head = Value::small_int(0); // bottom sentinel (immediate)
        for _ in 0..200_000 {
            head = unsafe { make_tuple(&mi, &[head]) }; // count 1, child = prev
        }
        release(head); // frees 200k nodes via the work list
        drop(mi);
    }

    #[test]
    fn rc_copy_preserves_sharing_counts_and_independence() {
        use super::rc_copy_graph;
        use crate::bytecode::value::ValueView;

        // root = ( (leaf), (leaf) ) — two parents share one leaf.
        let src = MiHeap::new();
        let leaf = unsafe { make_tuple(&src, &[Value::small_int(9)]) };
        let p1 = unsafe { make_tuple(&src, &[leaf]) };
        let p2 = unsafe { make_tuple(&src, &[leaf]) };
        let root = unsafe { make_tuple(&src, &[p1, p2]) };

        let dst = MiHeap::new();
        let copy = rc_copy_graph(root, &dst);

        let (c1, c2) = match copy.kind() {
            ValueView::Tuple(es) => (es[0], es[1]),
            _ => panic!("root copy should be a tuple"),
        };
        let leaf1 = match c1.kind() {
            ValueView::Tuple(es) => es[0],
            _ => panic!("p1 copy should be a tuple"),
        };
        let leaf2 = match c2.kind() {
            ValueView::Tuple(es) => es[0],
            _ => panic!("p2 copy should be a tuple"),
        };

        // Sharing preserved: the leaf is copied once, both parents point at it.
        assert_eq!(
            leaf1.object_addr(),
            leaf2.object_addr(),
            "shared leaf copied once"
        );
        // Independent of the source graph.
        assert_ne!(leaf1.object_addr(), leaf.object_addr());
        assert_eq!(
            match leaf1.kind() {
                ValueView::Tuple(es) => es[0].as_int(),
                _ => None,
            },
            Some(9),
        );

        // Counts are the in-graph reference counts.
        assert_eq!(unsafe { count(copy) }, 1, "root held by us");
        assert_eq!(unsafe { count(c1) }, 1);
        assert_eq!(unsafe { count(c2) }, 1);
        assert_eq!(unsafe { count(leaf1) }, 2, "leaf has two parents");

        // Releasing the copy frees it whole; the source is untouched.
        release(copy);
        assert_eq!(unsafe { count(root) }, 1, "source root intact");
        drop(src);
        drop(dst);
    }

    #[test]
    fn rc_copy_shares_frozen_children() {
        use super::rc_copy_graph;
        use crate::bytecode::value::ValueView;
        use crate::frozen::FrozenArea;
        use std::sync::Arc;

        let area = Arc::new(FrozenArea::new());
        let mut b = area.builder();
        let frozen = b.str("hello");
        assert!(frozen.is_immortal());

        let src = MiHeap::new();
        let root = unsafe { make_tuple(&src, &[frozen]) };

        let dst = MiHeap::new();
        let copy = rc_copy_graph(root, &dst);
        let child = match copy.kind() {
            ValueView::Tuple(es) => es[0],
            _ => panic!("expected tuple"),
        };
        // Frozen child is shared, not copied — same address, still immortal.
        assert_eq!(child.object_addr(), frozen.object_addr());
        assert!(child.is_immortal());

        release(copy); // frees the copied tuple; must not touch the frozen child
        drop(src);
        drop(dst);
        assert_eq!(frozen.as_str(), Some("hello"), "frozen survives");
    }
}
