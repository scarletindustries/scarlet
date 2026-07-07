//! The four places a copy can land. Each [`CopyDest`] pairs an allocation
//! target with a Cheney scan cursor over what it has accepted so far:
//!
//! | destination   | used by                          | lands in              |
//! |---------------|----------------------------------|-----------------------|
//! | [`SpaceDest`] | major GC, spawn pass 2, shrink   | one pre-sized space   |
//! | [`SegDest`]   | spawn pass 1 (scratch)           | appended segments     |
//! | [`MinorDest`] | minor GC                         | to-space *or* old, by |
//! |               |                                  | the tenuring rule     |
//! | [`FrozenDest`]| `publish_global`                 | the frozen area       |
//!
//! The `link_off_heap` impls here are the collector-side half of a twin seam:
//! they thread the *copies* that Clone-mode evacuation makes of `Arc`-owning
//! binaries. The mutator-side half — `Arena::link_off_heap` in
//! `bytecode/value.rs` — links each binary once at first allocation. Same
//! push-front protocol on both sides; only the trigger differs.
//!
//! # Encapsulation contract
//!
//! Together with `copy.rs` this file is the designated home of all `unsafe`
//! under `heap/`: the raw scan-cursor reads here never leave the module —
//! `next_unscanned`'s `*mut u64` flows only into `copy.rs`'s scan loop, and
//! the [`CopyDest`] trait itself is `pub(super)`, so no unsafe-adjacent API
//! escapes `heap/*`.

// Designated unsafe module: Cheney destination scan cursors hand raw object
// pointers to `copy.rs`; nothing unsafe escapes `heap/*`.
#![allow(unsafe_code)]

use super::copy::CopyDest;
use super::space::{SegSpace, Space};
use crate::bytecode::value::header_total_words;
use crate::frozen::FrozenBuilder;

/// Destination over a single pre-sized space.
pub(super) struct SpaceDest<'a> {
    pub(super) space: &'a mut Space,
    /// Cheney scan pointer: word index of the next unscanned object.
    scan: usize,
    /// Off-heap list head for Clone-mode copies into this space.
    pub(super) off_heap_head: usize,
}

impl<'a> SpaceDest<'a> {
    pub(super) fn new(space: &'a mut Space) -> SpaceDest<'a> {
        let scan = space.top;
        SpaceDest {
            space,
            scan,
            off_heap_head: 0,
        }
    }
}

impl CopyDest for SpaceDest<'_> {
    fn alloc_for(&mut self, _src_addr: usize, words: usize) -> *mut u64 {
        self.space.bump(words)
    }

    fn link_off_heap(&mut self, addr: usize) -> usize {
        std::mem::replace(&mut self.off_heap_head, addr)
    }

    fn next_unscanned(&mut self) -> Option<*mut u64> {
        if self.scan >= self.space.top {
            return None;
        }
        // SAFETY: `scan < top` (checked above) and every word below `top` is
        // part of a copied object image, so `p` is a valid header slot.
        let p = unsafe { self.space.words.as_mut_ptr().add(self.scan) };
        // SAFETY: `p` points at an initialized header word (see above).
        let header = unsafe { *p };
        self.scan += header_total_words(header);
        // cfg-gated (not debug_assert!) so the hot scan loop carries zero
        // extra MIR in release builds.
        #[cfg(debug_assertions)]
        assert!(
            self.scan <= self.space.top,
            "scan cursor overshot the copied region"
        );
        Some(p)
    }
}

/// Destination over a segmented space. The cursor starts at the end of the
/// pre-existing content and follows segment appends.
pub(super) struct SegDest<'a> {
    pub(super) seg: &'a mut SegSpace,
    seg_idx: usize,
    scan: usize,
    /// Off-heap list head for Clone-mode copies into this space.
    pub(super) off_heap_head: usize,
}

impl<'a> SegDest<'a> {
    pub(super) fn new(seg: &'a mut SegSpace) -> SegDest<'a> {
        let seg_idx = seg.segments.len().saturating_sub(1);
        let scan = seg.segments.last().map_or(0, |s| s.top);
        SegDest {
            seg,
            seg_idx,
            scan,
            off_heap_head: 0,
        }
    }
}

impl CopyDest for SegDest<'_> {
    fn alloc_for(&mut self, _src_addr: usize, words: usize) -> *mut u64 {
        self.seg.bump(words)
    }

    fn link_off_heap(&mut self, addr: usize) -> usize {
        std::mem::replace(&mut self.off_heap_head, addr)
    }

    fn next_unscanned(&mut self) -> Option<*mut u64> {
        loop {
            let segment_count = self.seg.segments.len();
            if self.seg_idx >= segment_count {
                return None;
            }
            let s = &mut self.seg.segments[self.seg_idx];
            if self.scan < s.top {
                // SAFETY: `scan < top` (checked above) and every word below
                // `top` is a copied object image, so `p` is a valid header
                // slot.
                let p = unsafe { s.words.as_mut_ptr().add(self.scan) };
                // SAFETY: `p` points at an initialized header word.
                let header = unsafe { *p };
                self.scan += header_total_words(header);
                // cfg-gated (not debug_assert!) so the hot scan loop carries
                // zero extra MIR in release builds.
                #[cfg(debug_assertions)]
                assert!(
                    self.scan <= s.top,
                    "scan cursor overshot the segment's copied region"
                );
                return Some(p);
            }
            // Only the last segment grows, so a fully scanned non-last
            // segment is final and the cursor can advance.
            if self.seg_idx + 1 < segment_count {
                self.seg_idx += 1;
                self.scan = 0;
                continue;
            }
            return None;
        }
    }
}

/// Minor-GC destination: survivors split between the to-space and the old
/// generation by the high-water rule — an object whose *source* address
/// is below the previous minor's end-of-heap mark has already survived one
/// collection and tenures.
///
/// Tenuring is closed under reachability: everything below the mark was live
/// at the previous collection, and immutability means objects only reference
/// values older than themselves, so a tenured object's children sit below
/// the mark too and tenure in the same pass. That is what keeps old→young
/// pointers impossible and minor collections barrier-free.
pub(super) struct MinorDest<'a> {
    pub(super) to: SpaceDest<'a>,
    pub(super) old: SegDest<'a>,
    pub(super) high_water: usize,
    pub(super) tenured_words: usize,
}

impl CopyDest for MinorDest<'_> {
    fn alloc_for(&mut self, src_addr: usize, words: usize) -> *mut u64 {
        if src_addr < self.high_water {
            self.tenured_words += words;
            self.old.alloc_for(src_addr, words)
        } else {
            self.to.alloc_for(src_addr, words)
        }
    }

    #[allow(clippy::unreachable)]
    fn link_off_heap(&mut self, _addr: usize) -> usize {
        // Only Clone-mode copies build per-destination off-heap lists, and
        // minor GC always copies in Move mode (the swept process-wide list
        // keeps tracking survivors).
        unreachable!("minor GC runs in Move mode")
    }

    fn next_unscanned(&mut self) -> Option<*mut u64> {
        self.to
            .next_unscanned()
            .or_else(|| self.old.next_unscanned())
    }
}

/// Frozen-publish destination: objects are appended to the program-wide
/// frozen area through a [`FrozenBuilder`]. The builder's segments are not
/// contiguous and not ours to cursor over, so the scan queue is an explicit
/// list of the objects this copy allocated.
pub(super) struct FrozenDest<'a> {
    pub(super) builder: &'a mut FrozenBuilder,
    pub(super) copied: Vec<*mut u64>,
    pub(super) next: usize,
}

impl CopyDest for FrozenDest<'_> {
    fn alloc_for(&mut self, _src_addr: usize, words: usize) -> *mut u64 {
        let p = self.builder.alloc(words).as_ptr();
        self.copied.push(p);
        p
    }

    fn link_off_heap(&mut self, _addr: usize) -> usize {
        // No-list rule for the never-collected frozen area; the rationale
        // lives on its mutator-side twin, `impl Arena for FrozenBuilder` in
        // bytecode/value.rs. The Clone-mode copy bumped the box's backing
        // `Arc`, which is now held for the rest of the program.
        0
    }

    fn next_unscanned(&mut self) -> Option<*mut u64> {
        let p = *self.copied.get(self.next)?;
        self.next += 1;
        Some(p)
    }
}
