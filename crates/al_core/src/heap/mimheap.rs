//! The reference-counting allocator: mimalloc's per-thread default heap.
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
//! [`MiHeap`] is therefore a zero-sized marker that exists only to give the
//! `Arena` impl a `&mut self` receiver and keep the call sites unchanged.

// Designated unsafe module: the mimalloc FFI lives here, behind a safe API.
#![allow(unsafe_code)]

use std::collections::HashMap;
use std::ptr::NonNull;

use libmimalloc_sys::{mi_free, mi_malloc_aligned};

use crate::bytecode::value::{
    RC_PREFIX_WORDS, Value, binary_clone_backing, for_each_child, header_has_off_heap_link,
    header_total_words, mark_immortal, rc_increment,
};
use crate::frozen::FrozenBuilder;

/// Zero-sized allocator marker over mimalloc's per-thread default heap. See the
/// module docs for why there is no per-process `mi_heap_t`.
#[derive(Default)]
pub struct MiHeap;

impl MiHeap {
    pub fn new() -> MiHeap {
        MiHeap
    }

    /// Allocate `words` 8-byte words, 8-byte aligned, from the calling thread's
    /// default heap. The storage is uninitialized.
    #[inline]
    #[allow(clippy::expect_used)]
    pub fn alloc_words(&self, words: usize) -> NonNull<u64> {
        let bytes = words * size_of::<u64>();
        // SAFETY: `mi_malloc_aligned` returns 8-aligned storage or null on OOM.
        let p = unsafe { mi_malloc_aligned(bytes, align_of::<u64>()) };
        NonNull::new(p.cast::<u64>()).expect("mi_malloc_aligned returned null")
    }

    /// Allocate storage for one reference-counted object of `words` (header +
    /// payload) words, reserving the leading refcount slot — initialized to 1 —
    /// and returning a pointer to the HEADER, so every header-relative offset
    /// is unchanged. Reclaimed by reference counting (`value::release`).
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

    /// Free a single object's allocation. mimalloc recovers the owning heap
    /// from the pointer's segment metadata, so a free on a different thread than
    /// the one that allocated (after a migration) is routed to the owning heap.
    ///
    /// # Safety
    /// `addr` must be a live mimalloc allocation that is not freed again.
    #[inline]
    #[allow(dead_code)]
    pub unsafe fn free(addr: NonNull<u64>) {
        // SAFETY: the caller guarantees `addr` is a live, not-yet-freed block.
        unsafe { mi_free(addr.as_ptr().cast()) };
    }
}

/// Shallow-copy one node of a spawn graph into `dst`, returning the value to
/// store in the parent slot. Immediates and immortal (frozen) values pass
/// through unchanged (frozen objects are shared, never copied). A node already
/// copied (a DAG join) is shared: its existing copy's refcount is bumped. A
/// first-seen node is byte-copied (its child slots still point at the *source*
/// graph; the caller's queue links them later) and queued.
fn copy_node(
    src: &Value,
    dst: &MiHeap,
    map: &mut HashMap<usize, NonNull<u64>>,
    queue: &mut Vec<NonNull<u64>>,
) -> Value {
    if !src.is_heap() || src.is_immortal() {
        return src.clone();
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
/// copied root. A single Clone pass with no native recursion (an explicit
/// queue links children, so a deep graph cannot overflow the stack). It
///
/// - preserves sharing — a DAG stays a DAG — via a `src → dst` address map;
/// - sets each copy's refcount to its in-graph reference count (one per edge);
/// - shares immortal (frozen) objects without copying them;
/// - shares `Binary` byte backings by bumping their `Arc` (no byte copy).
pub fn rc_copy_graph(root: &Value, dst: &MiHeap) -> Value {
    let mut map: HashMap<usize, NonNull<u64>> = HashMap::new();
    let mut queue: Vec<NonNull<u64>> = Vec::new();
    let root_copy = copy_node(root, dst, &mut map, &mut queue);
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
                let copied = copy_node(child, dst, &mut map, &mut queue);
                std::ptr::write(child as *mut Value, copied);
            });
        }
    }
    root_copy
}

/// Publish-side counterpart of [`copy_node`]: copy one node into the frozen
/// `builder`. Frozen copies carry NO refcount prefix and are marked immortal
/// (reference counting never touches them); already-frozen children and
/// immediates pass through shared.
fn publish_node(
    src: &Value,
    builder: &mut FrozenBuilder,
    map: &mut HashMap<usize, NonNull<u64>>,
    queue: &mut Vec<NonNull<u64>>,
) -> Value {
    if !src.is_heap() || src.is_immortal() {
        return src.clone();
    }
    let src_obj = src.heap_obj();
    let src_addr = src_obj as usize;
    if let Some(&d) = map.get(&src_addr) {
        return Value::from_object_ptr(d); // shared; frozen objects have no count
    }
    // SAFETY: `src` is a live heap object; copy its image into the frozen area.
    let words = unsafe { header_total_words(*src_obj) };
    let d = builder.alloc(words);
    unsafe {
        std::ptr::copy_nonoverlapping(src_obj, d.as_ptr(), words);
        mark_immortal(d.as_ptr());
        if header_has_off_heap_link(*d.as_ptr()) {
            binary_clone_backing(d.as_ptr()); // frozen holds the Arc for the program's life
        }
    }
    map.insert(src_addr, d);
    queue.push(d);
    Value::from_object_ptr(d)
}

/// Deep-copy the graph reachable from `root` into the frozen `builder`,
/// returning the frozen root. A single pass with an explicit queue (no native
/// recursion), preserving sharing via a `src → dst` map, sharing already-frozen
/// subgraphs, and bumping `Binary` Arc backings (which the frozen area then
/// holds forever).
pub fn rc_publish_graph(root: &Value, builder: &mut FrozenBuilder) -> Value {
    let mut map: HashMap<usize, NonNull<u64>> = HashMap::new();
    let mut queue: Vec<NonNull<u64>> = Vec::new();
    let root_copy = publish_node(root, builder, &mut map, &mut queue);
    let mut i = 0;
    while i < queue.len() {
        let d = queue[i];
        i += 1;
        // SAFETY: child slots hold verbatim source pointer bits; rewrite each
        // with `ptr::write` (the alias owns no count, so it must not drop).
        unsafe {
            for_each_child(d.as_ptr(), &mut |child: &mut Value| {
                let copied = publish_node(child, builder, &mut map, &mut queue);
                std::ptr::write(child as *mut Value, copied);
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
    fn many_alloc_free_round_trips() {
        // Reference counting frees each object individually; there is no
        // wholesale heap destroy. Many alloc/free pairs must not corrupt the
        // shared default heap.
        let h = MiHeap::new();
        for i in 0..10_000u64 {
            let p = h.alloc_words(8);
            // SAFETY: `p` is a live 8-word allocation owned exclusively here.
            unsafe {
                p.as_ptr().write(i);
                assert_eq!(p.as_ptr().read(), i);
                MiHeap::free(p);
            }
        }
    }

    #[test]
    fn miheap_is_send() {
        fn takes_send<T: Send>() {}
        takes_send::<MiHeap>();
    }
}
