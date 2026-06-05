//! The Cheney copying engine: [`copy_graph`], the one primitive every copy
//! in the runtime goes through.
//!
//! Minor GC, major GC, spawn-copy, and frozen publish are all *the same
//! loop* — evacuate everything reachable from the roots into a destination
//! — differing only in which ranges are evacuated (the [`Classifier`]),
//! where copies land (the [`CopyDest`]), and whether the source survives
//! (the [`CopyMode`]). Understanding this file is understanding the
//! collector; everything else is plumbing around it.
//!
//! # Cheney's algorithm in one diagram
//!
//! The destination doubles as the work queue. Two cursors chase each other:
//! `alloc` (where the next copy lands) and `scan` (the next copy whose
//! children have not been visited). Copying an object advances `alloc`;
//! visiting one advances `scan`; the copy is complete when `scan` catches
//! `alloc`. No recursion, no separate queue, arbitrarily deep graphs.
//!
//! ```text
//!   destination: [ A' | B' | C' |--------- free ----------]
//!                       ^scan         ^alloc
//!
//!   1. evacuate the roots            → A' copied,  alloc advances
//!   2. scan A': its children B, C    → B', C' copied, alloc advances
//!   3. scan B': children already
//!      copied (forwarding pointers)  → scan advances, nothing new
//!   4. scan == alloc                 → done; everything live has moved
//! ```
//!
//! # Forwarding pointers (how sharing survives)
//!
//! The first time an object is copied, its *source header* is overwritten
//! with a forwarding pointer to the copy. Any later reference to the same
//! object finds the forward and reuses the copy — an object referenced n
//! times is copied once and referenced n times in the copy. DAGs stay DAGs;
//! a value shared between two array slots is still shared after every
//! collection, spawn, and publish.
//!
//! # What "dead" means here
//!
//! Nothing visits dead objects — they are simply never copied, and the
//! source space is reset wholesale afterwards. That is why the collector
//! runs no destructors, and why arena boxes that own a real Rust resource
//! (a `Binary`'s `Arc<[u8]>` backing) need the off-heap list:
//! [`sweep_off_heap_list`] walks it after every Move-mode copy and releases
//! the backings of the boxes that did not get forwarded — the one
//! destructor-like duty Cheney cannot perform.
//!
//! # Encapsulation contract
//!
//! This file and `dest.rs` are the *only* modules under `heap/` that contain
//! `unsafe` code, and `heap/` exports no unsafe API: every `pub(super)` item
//! here ([`copy_graph`], [`sweep_off_heap_list`], [`release_off_heap_list`],
//! the [`CopyDest`] trait) is a safe fn or trait whose raw-pointer traffic
//! stays between this file and `dest.rs`. Policy code (`proc_heap.rs`) and
//! the tests drive the engine through these safe entry points only.

// Designated unsafe module: the Cheney evacuation engine's raw-pointer copy
// loop, behind safe `pub(super)` entry points.
#![allow(unsafe_code)]

use super::roots::{Classifier, Roots};
use crate::bytecode::value::{
    Value, binary_clone_backing, binary_drop_backing, binary_off_heap_next, for_each_child,
    forwarding_target, header_has_off_heap_link, header_is_forwarding, header_total_words,
    make_forwarding, set_binary_off_heap_next,
};

/// What a copy means for `Arc`-owning boxes and for the source objects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CopyMode {
    /// GC semantics: the source space dies after the copy. Box `Arc` counts
    /// transfer to the copies; the caller sweeps the off-heap list
    /// afterwards ([`sweep_off_heap_list`]) to relink survivors and release
    /// dead entries.
    Move,
    /// Spawn / frozen-publish semantics: the source stays alive. Copied
    /// boxes bump their backing's strong count and are linked onto the
    /// destination's off-heap list at copy time, and every forwarded source
    /// header is restored before `copy_graph` returns, leaving the source
    /// graph exactly as it was.
    Clone,
}

#[derive(Debug, Default, Clone, Copy)]
pub(super) struct CopyStats {
    pub(super) words_copied: usize,
}

/// A copy destination: hands out space for evacuated objects, adopts their
/// off-heap list entries (Clone mode), and replays copied-but-unscanned
/// objects to the Cheney scan loop. The four implementations live in
/// `dest.rs`. `pub(super)` on purpose: `next_unscanned` hands out raw object
/// pointers, so the trait must stay inside the `heap` module.
pub(super) trait CopyDest {
    /// Allocate `words` for the object whose source header is at `src_addr`
    /// (minor GC uses the source address for its tenuring decision).
    /// Infallible: destinations are pre-sized or grow by appending segments.
    fn alloc_for(&mut self, src_addr: usize, words: usize) -> *mut u64;

    /// Thread a just-copied `Arc`-owning box at `addr` onto the
    /// destination's off-heap list; returns the previous head for the box's
    /// link word. Only called in Clone mode — Move mode relinks through the
    /// post-collection sweep instead.
    fn link_off_heap(&mut self, addr: usize) -> usize;

    /// The next copied object that has not been scanned yet, or `None` at
    /// the fixpoint. This is the Cheney scan pointer, generalized over
    /// multi-space destinations.
    fn next_unscanned(&mut self) -> Option<*mut u64>;
}

struct CopyCtx<'a> {
    classifier: &'a Classifier,
    mode: CopyMode,
    /// Clone mode: source headers to restore (their pristine image lives in
    /// the destination copy the forwarding pointer names).
    forwarded: Vec<usize>,
    stats: CopyStats,
}

/// Evacuate one value slot: copy its object if it lives in a collected range
/// and was not copied yet, and re-point the slot at the copy.
fn evacuate(slot: &mut Value, ctx: &mut CopyCtx<'_>, dst: &mut dyn CopyDest) {
    let Some(src_addr) = slot.object_addr() else {
        return; // immediates carry no heap pointer
    };
    if !ctx.classifier.covers(src_addr) {
        return; // frozen, foreign, or an uncollected generation
    }
    let src = src_addr as *mut u64;
    // SAFETY: the classifier whitelists only the used ranges of live spaces,
    // so `src` is a real object header slot.
    let header = unsafe { *src };
    let copy_addr = if header_is_forwarding(header) {
        forwarding_target(header)
    } else {
        let words = header_total_words(header);
        // Header sanity: a real object describes at least its header word; a
        // garbage word here means a stale pointer or a missed root rewrite.
        debug_assert!(words >= 1, "evacuate: header describes zero words");
        let copy = dst.alloc_for(src_addr, words);
        // SAFETY: source and destination are distinct live slabs; `copy` has
        // room for `words` words by the `alloc_for` contract.
        unsafe { std::ptr::copy_nonoverlapping(src, copy, words) };
        if header_has_off_heap_link(header) && ctx.mode == CopyMode::Clone {
            // The source box stays alive: source and copy must each own one
            // strong count, and the copy threads onto the destination's own
            // off-heap list. (Move mode leaves both duties to the sweep.)
            // SAFETY: the copy is a fully initialized Binary box image.
            unsafe {
                binary_clone_backing(copy);
                let prev = dst.link_off_heap(copy as usize);
                set_binary_off_heap_next(copy, prev);
            }
        }
        // SAFETY: overwriting the source header with the forwarding pointer;
        // the payload stays intact for the off-heap sweep / header restore.
        unsafe { *src = make_forwarding(copy as usize) };
        if ctx.mode == CopyMode::Clone {
            ctx.forwarded.push(src_addr);
        }
        ctx.stats.words_copied += words;
        copy as usize
    };
    // SAFETY: `copy_addr` is the 8-aligned, non-null header address of the
    // evacuated object.
    *slot =
        Value::from_object_ptr(unsafe { std::ptr::NonNull::new_unchecked(copy_addr as *mut u64) });
}

/// Evacuate every object reachable from `roots` whose address the
/// `classifier` covers into `dst`, installing forwarding pointers so sharing
/// (DAGs) is preserved, and fix up every traversed reference — including the
/// root slots themselves.
pub(super) fn copy_graph(
    roots: &mut dyn Roots,
    classifier: &Classifier,
    dst: &mut dyn CopyDest,
    mode: CopyMode,
) -> CopyStats {
    let mut ctx = CopyCtx {
        classifier,
        mode,
        forwarded: Vec::new(),
        stats: CopyStats::default(),
    };
    roots.for_each(&mut |slot| evacuate(slot, &mut ctx, dst));
    while let Some(obj) = dst.next_unscanned() {
        // SAFETY: `obj` came from `next_unscanned`, so it is a fully copied,
        // non-forwarded object image in the destination; `for_each_child`
        // visits exactly its value slots.
        unsafe { for_each_child(obj, &mut |child| evacuate(child, &mut ctx, dst)) };
    }
    if mode == CopyMode::Clone {
        // Restore every forwarded source header from its copy, leaving the
        // source graph untouched (the copy's header word is the pristine
        // image the forwarding pointer replaced).
        for src_addr in &ctx.forwarded {
            let src = *src_addr as *mut u64;
            // SAFETY: each entry was forwarded by this very copy; its target
            // holds the original header word.
            unsafe {
                let target = forwarding_target(*src) as *const u64;
                *src = *target;
            }
        }
    }
    ctx.stats
}

/// Sweep the intrusive off-heap list headed at `head` after a Move-mode copy
/// vacated the ranges in `collected`. Rebuilds the list: forwarded entries
/// are relinked at their new address, entries outside the collected ranges
/// (e.g. tenured boxes during a minor) are kept, and unforwarded entries in
/// a collected range are dead — their backing `Arc` is released. Returns the
/// new list head and how many backings were dropped.
pub(super) fn sweep_off_heap_list(head: usize, collected: &Classifier) -> (usize, u64) {
    let mut new_head = 0usize;
    let mut dropped = 0u64;
    let mut cur = head;
    while cur != 0 {
        let obj = cur as *mut u64;
        // SAFETY: the list only contains Binary boxes in still-allocated
        // slabs; a forwarded entry's payload — including the link word — is
        // intact, only its header was overwritten.
        let next = unsafe { binary_off_heap_next(obj) };
        let header = unsafe { *obj };
        if header_is_forwarding(header) {
            let moved = forwarding_target(header) as *mut u64;
            // SAFETY: the copy is exclusively ours until the collection ends.
            unsafe { set_binary_off_heap_next(moved, new_head) };
            new_head = moved as usize;
        } else if collected.covers(cur) {
            // Dead: unreachable in a vacated range. Release the strong count
            // the box owns — Cheney ran no destructor for it.
            // SAFETY: dead boxes are swept exactly once (the list is consumed
            // here and the slab is reset/freed by the caller).
            unsafe { binary_drop_backing(obj) };
            dropped += 1;
        } else {
            // Live in an uncollected space (old generation during a minor):
            // keep, relinked onto the rebuilt list.
            // SAFETY: the box is live and exclusively owned by this heap.
            unsafe { set_binary_off_heap_next(obj, new_head) };
            new_head = cur;
        }
        cur = next;
    }
    (new_head, dropped)
}

/// Release every backing on the off-heap list headed at `head` without
/// rebuilding it: the heap is dying (process death), so every box on the
/// list is dead and its strong count must be dropped. The cold counterpart
/// of [`sweep_off_heap_list`]; sole caller is `ProcHeap::drop`.
pub(super) fn release_off_heap_list(head: usize) {
    let mut cur = head;
    while cur != 0 {
        let obj = cur as *const u64;
        // SAFETY: at rest (no collection in flight) the list holds live,
        // non-forwarded Binary boxes in slabs the dropping heap still owns;
        // each owns exactly one strong count, and the list is consumed
        // exactly once because the caller is the heap's destructor.
        unsafe {
            debug_assert!(!header_is_forwarding(*obj));
            let next = binary_off_heap_next(obj);
            binary_drop_backing(obj);
            cur = next;
        }
    }
}

/// Test-only safe view of a Binary box's off-heap link word. `addr` must be
/// the header address of a live Binary box (tests obtain it from
/// `Value::object_addr` on a binary they just built).
#[cfg(test)]
pub(super) fn off_heap_next(addr: usize) -> usize {
    // SAFETY: confined to heap tests, which pass header addresses of live,
    // fully initialized Binary boxes.
    unsafe { binary_off_heap_next(addr as *const u64) }
}
