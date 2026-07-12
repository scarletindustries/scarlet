//! [`ProcHeap`]: one process's allocator handle over mimalloc's per-thread
//! default heap, under non-atomic reference counting.
//!
//! Objects are allocated with `mi_malloc_aligned` from the *calling thread's*
//! default heap and freed individually with `mi_free` as their refcount hits
//! zero. This is the thread-safe usage of mimalloc: an object may be allocated
//! on one scheduler core and freed on another (after a process migrates), and
//! `mi_free` routes the block back to its owning heap's atomic thread-free list.
//!
//! We deliberately do NOT use per-process `mi_heap_t`s: a `mi_heap_t` is bound
//! to its creating thread, so a process's heap built on the spawner and then
//! run on a worker would be allocated from off-thread — undefined behaviour.
//! With the default-heap model, a process never owns heap state that has to
//! cross threads; only its `Value`s (plain words) do, and reference counting
//! reclaims its graph when the process's roots drop at death.
//!
//! [`ProcHeap`] is therefore a zero-sized handle: it exists to give the `Arena`
//! impl a receiver and to name the two graph-copy entry points ([`spawn`] and
//! [`publish_frozen`]). There is no collector: no rooting rule, no generations,
//! no safepoints.
//!
//! [`spawn`]: ProcHeap::spawn
//! [`publish_frozen`]: ProcHeap::publish_frozen

// Designated unsafe module: the mimalloc FFI lives here, behind a safe API.
#![allow(unsafe_code)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::ptr::NonNull;

#[cfg(test)]
use libmimalloc_sys::mi_free;
use libmimalloc_sys::mi_malloc_aligned;

use crate::bytecode::value::{
    Arena, RC_PREFIX_WORDS, Value, binary_clone_backing, for_each_child, freeze_enum_hash,
    header_has_off_heap_link, header_total_words, mark_immortal, rc_increment,
};
use crate::frozen::{FrozenBuilder, FrozenConst};

#[cfg(feature = "alloc-counter")]
thread_local! {
    /// Test hook: running count of [`ProcHeap::alloc_object`] calls on THIS
    /// thread. Thread-local (not a process-global atomic) so each Rust test —
    /// which the default libtest harness runs on its own thread — reads and
    /// resets an isolated counter; a global would race with the many
    /// `value`/`hamt` unit tests that allocate in parallel and make exact
    /// net-allocation assertions (the point of the Perceus reuse tests)
    /// impossible. The reuse tests drive the VM in-process without spawning
    /// worker schedulers, so all counted allocations happen on the test thread.
    static ALLOC_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// A process's allocator handle: a zero-sized marker over mimalloc's per-thread
/// default heap. See the module docs for why there is no per-process
/// `mi_heap_t`.
///
/// `ProcHeap` is trivially `Send` (static assert in `mod.rs`) — it holds no
/// thread-bound state. Objects are allocated from mimalloc's per-thread default
/// heap and freed with `mi_free`, which is cross-thread safe, so migrating the
/// owning `Process` to another scheduler is a plain move.
#[derive(Default)]
pub struct ProcHeap;

/// OOM abort for [`ProcHeap::alloc_raw`], keeping the `Arena::alloc_words`
/// contract infallible: report the failed layout through the global
/// allocation-error hook (which aborts) instead of unwinding.
#[cold]
#[inline(never)]
fn alloc_failed(bytes: usize) -> ! {
    let layout = std::alloc::Layout::from_size_align(bytes, align_of::<u64>())
        .unwrap_or_else(|_| std::alloc::Layout::new::<u64>());
    std::alloc::handle_alloc_error(layout)
}

impl ProcHeap {
    /// A fresh allocator handle for a new process.
    pub fn new() -> ProcHeap {
        ProcHeap
    }

    // ---- allocation -------------------------------------------------------

    /// Allocate `words` 8-byte words, 8-byte aligned, from the calling thread's
    /// default heap. The storage is uninitialized. Infallible by the `Arena`
    /// contract: OOM aborts via [`std::alloc::handle_alloc_error`].
    #[inline]
    fn alloc_raw(&self, words: usize) -> NonNull<u64> {
        let bytes = words * size_of::<u64>();
        // SAFETY: `mi_malloc_aligned` returns 8-aligned storage or null on OOM.
        let p = unsafe { mi_malloc_aligned(bytes, align_of::<u64>()) };
        match NonNull::new(p.cast::<u64>()) {
            Some(p) => p,
            None => alloc_failed(bytes),
        }
    }

    /// Allocate storage for one reference-counted object of `words` (header +
    /// payload) words, reserving the leading refcount slot — initialized to 1 —
    /// and returning a pointer to the HEADER, so every header-relative offset
    /// is unchanged. Reclaimed by reference counting (`value::release`).
    #[inline]
    pub fn alloc_object(&self, words: usize) -> NonNull<u64> {
        #[cfg(feature = "alloc-counter")]
        ALLOC_COUNT.set(ALLOC_COUNT.get().wrapping_add(1));
        let raw = self.alloc_raw(RC_PREFIX_WORDS + words);
        // SAFETY: fresh allocation of `RC_PREFIX_WORDS + words` (>= 2) words;
        // the first word is the refcount, the object begins after it.
        unsafe {
            raw.write(1); // refcount = 1 (the constructing handle)
            raw.add(RC_PREFIX_WORDS)
        }
    }

    // ---- test instrumentation --------------------------------------------

    /// Number of [`alloc_object`](Self::alloc_object) calls on the current
    /// thread since its last [`reset_alloc_count`](Self::reset_alloc_count).
    /// Used by reuse tests to assert net allocations (e.g. `list.map` on a
    /// uniquely-owned list must allocate zero fresh cells).
    #[cfg(feature = "alloc-counter")]
    pub fn alloc_count() -> usize {
        ALLOC_COUNT.get()
    }

    /// Reset this thread's [`alloc_count`](Self::alloc_count) to zero. Call
    /// immediately before the code span whose allocations a test is measuring.
    #[cfg(feature = "alloc-counter")]
    pub fn reset_alloc_count() {
        ALLOC_COUNT.set(0);
    }

    /// Free a single object's allocation. mimalloc recovers the owning heap
    /// from the pointer's segment metadata, so a free on a different thread than
    /// the one that allocated (after a migration) is routed to the owning heap.
    ///
    /// # Safety
    /// `addr` must be a live mimalloc allocation that is not freed again.
    #[cfg(test)]
    unsafe fn free_raw(addr: NonNull<u64>) {
        // SAFETY: the caller guarantees `addr` is a live, not-yet-freed block.
        unsafe { mi_free(addr.as_ptr().cast()) };
    }

    // ---- spawn and frozen publish -----------------------------------------

    /// Spawn-side graph copy: deep-copy the closure graph reachable from
    /// `root` into a fresh child heap, returning the child heap and its own
    /// root. Sharing is preserved, `Binary` backings are shared by `Arc` bump
    /// (no byte copy), frozen references are shared untouched, and the parent
    /// graph is left intact. See [`rc_copy_graph`].
    ///
    /// Associated (no `&self`): the copy allocates from the *child's* handle;
    /// the parent heap is neither read nor mutated.
    pub fn spawn(root: &Value) -> (ProcHeap, Value) {
        (ProcHeap, rc_copy_graph(root))
    }

    /// Publish the graph reachable from `root` into the frozen area, returning
    /// the frozen root — the crate's only mint of a [`FrozenConst`] from an
    /// arbitrary runtime value. References already in the frozen area are
    /// shared as-is (a pure passthrough); mortal graphs are deep-copied. See
    /// [`rc_publish_graph`].
    ///
    /// Associated (no `&self`): the copy writes only into `builder`; the source
    /// process heap is neither read nor mutated.
    pub fn publish_frozen(builder: &mut FrozenBuilder, root: &Value) -> FrozenConst {
        FrozenConst::from_publish(rc_publish_graph(root, builder))
    }
}

// `ProcHeap` needs no `Drop`: a process's objects are reference-counted, so
// they are freed as its `Value`s (stack, frames, result) drop at death — there
// is no per-process heap to tear down.

/// `src address → dst object` map for [`walk_graph`]'s DAG-preserving copy.
type WalkMap = HashMap<usize, NonNull<u64>>;
/// BFS queue of freshly copied objects whose child slots still need rewriting.
type WalkQueue = Vec<NonNull<u64>>;

/// Shallow-copy one node into `arena`, returning the value to store in the
/// parent slot. Immediates and immortal (frozen) values pass through unchanged
/// (frozen objects are shared, never copied). A node already copied (a DAG
/// join) is shared — bumping the existing copy's refcount unless the copy is
/// frozen, as frozen objects carry no count. A first-seen node is byte-copied
/// (its child slots still point at the *source* graph; the caller's queue links
/// them later) and queued.
///
/// The destination discipline is the arena's own: [`Arena::marks_immortal`]
/// says whether its allocations are frozen (no refcount prefix, never counted)
/// or mortal (refcount prefix, born at 1). Taking the allocator as an `Arena`
/// rather than a bare closure plus a flag makes the two impossible to mismatch,
/// and each caller still monomorphizes to exactly the code it needs.
#[inline]
fn copy_node_into<A: Arena>(
    src: &Value,
    map: &mut WalkMap,
    queue: &mut WalkQueue,
    arena: &mut A,
) -> Value {
    if !src.is_heap() || src.is_immortal() {
        return src.clone();
    }
    let immortal = arena.marks_immortal();
    let src_obj = src.heap_obj();
    let src_addr = src_obj as usize;
    if let Some(&d) = map.get(&src_addr) {
        // SAFETY: `d` is a live object this walk already created.
        return unsafe {
            if !immortal {
                rc_increment(d.as_ptr());
            }
            Value::from_object_ptr(d)
        };
    }
    // SAFETY: `src` is a live heap object; its header gives its total size.
    let words = unsafe { header_total_words(*src_obj) };
    let d = arena.alloc_words(words); // mortal: refcount = 1 (this first reference)
    // SAFETY: `d` has `words` header+payload words; copy the image verbatim,
    // then share the off-heap Arc backing if this is a Binary box (which the
    // frozen area, if that is the destination, then holds for the program's life).
    unsafe {
        if immortal {
            // A frozen cell is shared across threads and can never be lazily
            // hashed, so hash the (single-owner) source now: the copy below
            // carries the cached word into the frozen image.
            freeze_enum_hash(src_obj);
        }
        std::ptr::copy_nonoverlapping(src_obj, d.as_ptr(), words);
        if immortal {
            mark_immortal(d.as_ptr());
        }
        if header_has_off_heap_link(*d.as_ptr()) {
            binary_clone_backing(d.as_ptr());
        }
    }
    map.insert(src_addr, d);
    queue.push(d);
    // SAFETY: `d` is a fully written object header (image copied above).
    unsafe { Value::from_object_ptr(d) }
}

/// Breadth-first graph-copy driver shared by [`rc_copy_graph`] and
/// [`rc_publish_graph`]. `copy_one` shallow-copies one node — passing through
/// immediates/immortals, sharing already-seen nodes via `map`, and pushing each
/// first-seen copy onto `queue` with its child slots still holding verbatim
/// *source* pointer bits. The driver then walks the queue, rewriting each
/// copy's child slots in place. No native recursion, so a deep graph cannot
/// overflow the stack.
fn walk_graph(
    root: &Value,
    mut copy_one: impl FnMut(&Value, &mut WalkMap, &mut WalkQueue) -> Value,
) -> Value {
    thread_local! {
        /// Per-thread scratch for [`walk_graph`]: the `src → dst` address map
        /// and BFS queue, cleared and reused per call so a spawn/publish does
        /// not pay two fresh allocations plus O(nodes) rehash growth.
        /// `walk_graph` is non-reentrant (its `copy_one` callbacks never
        /// spawn/publish), so one per-thread pair is sound.
        static WALK_SCRATCH: RefCell<(WalkMap, WalkQueue)>
            = RefCell::new((WalkMap::new(), WalkQueue::new()));
    }
    WALK_SCRATCH.with(|cell| {
        let (map, queue) = &mut *cell.borrow_mut();
        map.clear();
        queue.clear();
        let root_copy = copy_one(root, map, queue);
        let mut i = 0;
        while i < queue.len() {
            let d = queue[i];
            i += 1;
            // SAFETY: `d` is a freshly copied object whose child slots still hold
            // verbatim *source* pointer bits (un-counted aliases). Build each
            // copied child from those bits, then overwrite the slot with
            // `ptr::write` so the alias is NOT dropped (it owns no count).
            unsafe {
                for_each_child(d.as_ptr(), &mut |child: &mut Value| {
                    let copied = copy_one(child, map, queue);
                    std::ptr::write(child as *mut Value, copied);
                });
            }
        }
        root_copy
    })
}

/// Deep-copy the value graph reachable from `root` into a fresh process heap,
/// returning the copied root. A single Clone pass with no native recursion (an
/// explicit queue links children, so a deep graph cannot overflow the stack).
/// It
///
/// - preserves sharing — a DAG stays a DAG — via a `src → dst` address map;
/// - sets each copy's refcount to its in-graph reference count (one per edge);
/// - shares immortal (frozen) objects without copying them;
/// - shares `Binary` byte backings by bumping their `Arc` (no byte copy).
fn rc_copy_graph(root: &Value) -> Value {
    walk_graph(root, |v, map, queue| {
        copy_node_into(v, map, queue, &mut ProcHeap)
    })
}

/// Deep-copy the graph reachable from `root` into the frozen `builder`,
/// returning the frozen root. A single pass with an explicit queue (no native
/// recursion), preserving sharing via a `src → dst` map, sharing already-frozen
/// subgraphs, and bumping `Binary` Arc backings (which the frozen area then
/// holds forever). Frozen copies carry NO refcount prefix and are marked
/// immortal, so reference counting never touches them.
fn rc_publish_graph(root: &Value, builder: &mut FrozenBuilder) -> Value {
    walk_graph(root, |v, map, queue| copy_node_into(v, map, queue, builder))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_write_read_then_free() {
        let h = ProcHeap::new();
        let p = h.alloc_raw(4);
        assert_eq!(p.as_ptr() as usize % 8, 0, "8-byte aligned");
        // SAFETY: `p` is a live 4-word allocation we own exclusively here.
        unsafe {
            for i in 0..4 {
                p.as_ptr().add(i).write(0xABCD_0000 + i as u64);
            }
            for i in 0..4 {
                assert_eq!(p.as_ptr().add(i).read(), 0xABCD_0000 + i as u64);
            }
            ProcHeap::free_raw(p);
        }
    }

    #[cfg(feature = "alloc-counter")]
    #[test]
    fn alloc_counter_tracks_alloc_object() {
        ProcHeap::reset_alloc_count();
        let base = ProcHeap::alloc_count();
        assert_eq!(base, 0);
        let h = ProcHeap::new();
        let a = h.alloc_object(3);
        let b = h.alloc_object(5);
        assert_eq!(ProcHeap::alloc_count(), 2);
        ProcHeap::reset_alloc_count();
        assert_eq!(ProcHeap::alloc_count(), 0);
        // SAFETY: `a`/`b` are live allocations; back up over the RC prefix to
        // the block start before freeing.
        unsafe {
            ProcHeap::free_raw(a.sub(RC_PREFIX_WORDS));
            ProcHeap::free_raw(b.sub(RC_PREFIX_WORDS));
        }
    }

    #[test]
    fn many_alloc_free_round_trips() {
        // Reference counting frees each object individually; there is no
        // wholesale heap destroy. Many alloc/free pairs must not corrupt the
        // shared default heap.
        let h = ProcHeap::new();
        for i in 0..10_000u64 {
            let p = h.alloc_raw(8);
            // SAFETY: `p` is a live 8-word allocation owned exclusively here.
            unsafe {
                p.as_ptr().write(i);
                assert_eq!(p.as_ptr().read(), i);
                ProcHeap::free_raw(p);
            }
        }
    }
}
