//! Where words live: [`Space`], a bump-allocated slab, and [`SegSpace`], an
//! append-only list of them.
//!
//! Everything above this file is policy; this file is the only place memory
//! is actually owned. Two properties carry the whole design:
//!
//! - **Slab addresses are stable.** A `Space` never reallocates in place —
//!   growth always means "allocate a *new* space and let the collector
//!   evacuate into it" — and a `Box`'s contents do not move when the box
//!   (or a struct owning it) moves. Raw pointers into a live space
//!   therefore stay valid across `Process` migration between OS threads.
//! - **Allocation never collects.** [`Space::alloc`] is a pure bump that
//!   fails when full; deciding *what to do* about a full space (collect,
//!   grow, panic) belongs to `ProcHeap`, never here.

use std::ptr::NonNull;

use super::roots::Classifier;

/// A bump-allocation space: one owned slab of words and a fill index.
pub struct Space {
    /// The slab. Object images are word runs inside it.
    pub(super) words: Box<[u64]>,
    /// Index of the next free word (the bump pointer).
    pub(super) top: usize,
}

impl Space {
    /// A zero-capacity space. Owns no slab; every allocation fails until it
    /// is replaced by a sized space.
    pub fn empty() -> Space {
        Space {
            words: Box::default(),
            top: 0,
        }
    }

    /// A space with room for `words` words.
    pub fn with_capacity(words: usize) -> Space {
        Space {
            words: vec![0u64; words].into_boxed_slice(),
            top: 0,
        }
    }

    pub fn capacity(&self) -> usize {
        self.words.len()
    }

    pub fn used(&self) -> usize {
        self.top
    }

    pub fn available(&self) -> usize {
        self.words.len() - self.top
    }

    /// Base address of the slab.
    pub(super) fn base_addr(&self) -> usize {
        self.words.as_ptr() as usize
    }

    /// One-past-the-end address of the used prefix (the bump pointer).
    pub(super) fn top_addr(&self) -> usize {
        self.base_addr() + self.top * std::mem::size_of::<u64>()
    }

    /// Bump-allocate `len` words, or `None` when the space is full. `alloc`
    /// itself never collects and never grows — the caller must have ensured
    /// capacity first.
    pub fn alloc(&mut self, len: usize) -> Option<*mut u64> {
        if len > self.available() {
            return None;
        }
        let ptr = self.words[self.top..].as_mut_ptr();
        self.top += len;
        Some(ptr)
    }

    /// Infallible bump for copy destinations, which are pre-sized (the minor
    /// to-space, the major target) or grown by appending segments (old,
    /// scratch) before this is reached. Overflow here is a collector bug,
    /// not an out-of-memory condition.
    pub(super) fn bump(&mut self, len: usize) -> NonNull<u64> {
        assert!(
            len <= self.available(),
            "copy destination overflow: destination spaces must be pre-sized"
        );
        let ptr = NonNull::from(&mut self.words[self.top]);
        self.top += len;
        ptr
    }

    /// Address-range membership: does `addr` point into this space's slab?
    /// Spans the *full capacity* (used and slack alike), so it answers
    /// "is this address owned by this space" — surfaced via
    /// `ProcHeap::contains` for introspection (and aggregated per-segment by
    /// `SegSpace::contains`). It is *not* the evacuation boundary: the
    /// collector decides what to copy via [`Classifier`], which whitelists
    /// only the *used* ranges of source spaces (see [`Classifier::push_space`]).
    pub fn contains(&self, addr: usize) -> bool {
        let start = self.base_addr();
        let end = start + self.words.len() * std::mem::size_of::<u64>();
        addr >= start && addr < end
    }

    /// Forget everything in the space (the collector evacuates live data and
    /// sweeps the off-heap list before resetting the source semispace).
    pub fn reset(&mut self) {
        self.top = 0;
    }
}

/// An append-only list of spaces. Used for the old (tenured) generation and
/// as the scratch destination of a spawn copy.
///
/// Why segments instead of one growable slab: a copy destination must never
/// reallocate mid-collection — forwarding pointers already point into it.
/// Growth that *appends* a segment leaves every already-copied object at
/// its address.
pub(super) struct SegSpace {
    pub(super) segments: Vec<Space>,
    /// Capacity for the next appended segment (doubles from `initial`,
    /// clamped at `max`).
    next_words: usize,
    initial: usize,
    max: usize,
}

impl SegSpace {
    pub(super) fn new(initial: usize, max: usize) -> SegSpace {
        SegSpace {
            segments: Vec::new(),
            next_words: initial,
            initial,
            max,
        }
    }

    pub(super) fn used(&self) -> usize {
        self.segments.iter().map(Space::used).sum()
    }

    pub(super) fn capacity(&self) -> usize {
        self.segments.iter().map(Space::capacity).sum()
    }

    pub(super) fn contains(&self, addr: usize) -> bool {
        self.segments.iter().any(|s| s.contains(addr))
    }

    /// Whitelist every segment's used range in `classifier`.
    pub(super) fn add_ranges(&self, classifier: &mut Classifier) {
        for seg in &self.segments {
            classifier.push_space(seg);
        }
    }

    /// Bump `words` in the open (last) segment, appending a new segment when
    /// it does not fit. Only the last segment ever grows, which the copy
    /// scan cursor relies on.
    pub(super) fn bump(&mut self, words: usize) -> NonNull<u64> {
        let need_new = self.segments.last().is_none_or(|s| s.available() < words);
        if need_new {
            let cap = self.next_words.max(words);
            self.segments.push(Space::with_capacity(cap));
            self.next_words = self.next_words.saturating_mul(2).min(self.max);
        }
        let last = self.segments.len() - 1;
        self.segments[last].bump(words)
    }

    /// Drop all segments (used by major GC after the off-heap sweep — the
    /// whole generation has been evacuated).
    pub(super) fn clear(&mut self) {
        self.segments.clear();
        self.next_words = self.initial;
    }
}
