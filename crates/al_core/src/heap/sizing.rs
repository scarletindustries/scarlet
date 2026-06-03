//! Sizing policy: every "how big should this space be?" decision, in one
//! place.
//!
//! The policy: start tiny (a server spawning
//! thousands of short-lived processes should pay almost nothing per
//! process), grow by doubling when collections find the heap crowded, and
//! shrink again after a sustained quiet period (the shrink itself lives in
//! `ProcHeap::maybe_shrink`; the constant governing it lives here).

/// Initial young-space size in words.
pub const INITIAL_YOUNG_WORDS: usize = 256;

/// First old-space segment size in words; later segments double up to
/// [`MAX_OLD_SEGMENT_WORDS`].
pub(super) const OLD_SEGMENT_MIN_WORDS: usize = 1024;

/// Ceiling for old-segment doubling (1 Mi words = 8 MiB). A single allocation
/// larger than this still gets an exactly-sized segment.
pub(super) const MAX_OLD_SEGMENT_WORDS: usize = 1 << 20;

/// Old-space occupancy (words) above which a minor collection escalates to
/// a major one. Re-derived after every major as `2 * live`, floored here.
pub(super) const MAJOR_THRESHOLD_MIN_WORDS: usize = 8 * 1024;

/// Consecutive low-occupancy minors before the young space shrinks.
pub(super) const SHRINK_AFTER_MINORS: u32 = 4;

/// The absolute growth table: powers of two starting at
/// [`INITIAL_YOUNG_WORDS`]. Returns the smallest table size that holds
/// `min_words`.
pub(super) fn size_for(min_words: usize) -> usize {
    let mut size = INITIAL_YOUNG_WORDS;
    while size < min_words {
        size = size.saturating_mul(2);
    }
    size
}
