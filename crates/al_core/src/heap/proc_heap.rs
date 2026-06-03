//! [`ProcHeap`]: one process's memory, and every *when* decision the
//! collector makes.
//!
//! The mechanism files below this one never decide anything: `space.rs`
//! holds words, `copy.rs` moves live graphs, `dest.rs` says where copies
//! land. This file is the policy that drives them — when a collection runs
//! (`ensure`/`needs_collect`), when a minor escalates to a major, when
//! survivors tenure (the high-water mark), when the young space grows or
//! shrinks — plus the two outward-facing copies (spawn, frozen publish) and
//! the debug enforcement of the rooting rule.

use super::copy::{CopyMode, copy_graph, sweep_off_heap_list};
use super::dest::{FrozenDest, MinorDest, SegDest, SpaceDest};
use super::flags::{gc_stress, heap_stats_enabled};
use super::roots::{Classifier, Roots};
use super::sizing::{
    INITIAL_YOUNG_WORDS, MAJOR_THRESHOLD_MIN_WORDS, OLD_SEGMENT_MIN_WORDS, SHRINK_AFTER_MINORS,
    size_for,
};
use super::space::{SegSpace, Space};
use crate::bytecode::value::{
    Value, binary_drop_backing, binary_off_heap_next, header_is_forwarding,
};
use crate::frozen::FrozenBuilder;

/// Counters for one heap's lifetime, printed on drop under
/// `AL_HEAP_STATS=1`.
#[derive(Debug, Default, Clone)]
pub struct HeapStats {
    pub minor_gcs: u64,
    pub major_gcs: u64,
    pub words_copied: u64,
    pub words_tenured: u64,
    pub binaries_dropped: u64,
    pub young_shrinks: u64,
    pub peak_young_words: usize,
    pub peak_old_words: usize,
}

/// A process-private heap: young from/to semispaces, a segmented old
/// (tenured) space, the off-heap list head, and GC bookkeeping.
///
/// `ProcHeap` is `Send` by construction (static assert in `mod.rs`): every
/// field is either an owned slab or a plain index/address into a slab this
/// heap owns. Slab addresses are stable under moves of the `ProcHeap`
/// itself, so migrating the owning `Process` to another scheduler is a
/// plain move.
pub struct ProcHeap {
    /// Allocation space: new objects bump-allocate here.
    young_from: Space,
    /// Evacuation target of the next minor collection. Empty between
    /// collections; the minor sizes (and if needed replaces) it when it
    /// runs.
    young_to: Space,
    /// Tenured data, grown by appending segments, collected only by major
    /// GC.
    old: SegSpace,
    /// Intrusive off-heap list head: the address of the first arena box
    /// whose payload owns an `Arc` (Binary backings), `0` when the list is
    /// empty. Cheney runs no destructors, so after every collection this
    /// list is swept and rebuilt (`sweep_off_heap_list`): unforwarded
    /// entries in the vacated ranges drop their `Arc`, everything else is
    /// relinked. Stored as an address, not a raw pointer — it only ever
    /// points into a slab this heap owns, which is what keeps the struct
    /// `Send`.
    off_heap_head: usize,
    /// End-of-heap *address* in `young_from` as of the previous minor
    /// collection (the high-water mark). Objects below it have already
    /// survived one minor and tenure into `old` on the next. `0` — below
    /// any real address — right after a heap is created or its young space
    /// is replaced wholesale, so nothing tenures on the first collection.
    high_water: usize,
    /// Minor collections since the last major.
    minor_count: u32,
    /// Old-space occupancy that escalates a minor to a major collection.
    pub(super) major_threshold: usize,
    /// Consecutive minors that ended with low young occupancy; drives the
    /// shrink policy.
    low_occupancy_streak: u32,
    /// Per-heap stress override for tests/tooling; `None` defers to the
    /// process-wide [`gc_stress`] flag.
    stress_override: Option<bool>,
    stats: HeapStats,
    /// Watermark for the debug rooting-rule check: words that may still be
    /// bump-allocated before the next `ensure()`. Set by [`note_ensured`]
    /// (replacing, not accumulating, so stale slack from a previous opcode
    /// cannot mask a missing `ensure`), drained by [`alloc_young`]'s
    /// assert.
    ///
    /// [`note_ensured`]: ProcHeap::note_ensured
    /// [`alloc_young`]: ProcHeap::alloc_young
    #[cfg(debug_assertions)]
    ensured: usize,
}

impl ProcHeap {
    /// A fresh heap for a new process: one small young space, nothing else.
    /// No allocation budget is granted — the VM's first `ensure()` stakes
    /// it.
    pub fn new() -> ProcHeap {
        ProcHeap::from_young(Space::with_capacity(INITIAL_YOUNG_WORDS))
    }

    /// A heap whose initial young space holds `words` words, with the whole
    /// space pre-granted as the allocation budget. This is the arena shape
    /// used by paths that fill a heap without a VM ensure() loop (seed
    /// construction, tests).
    pub fn with_young_capacity(words: usize) -> ProcHeap {
        let mut heap = ProcHeap::from_young(Space::with_capacity(words));
        heap.note_ensured(words);
        heap
    }

    /// Adopt a space (possibly already filled — the spawn path) as the
    /// young generation.
    fn from_young(young: Space) -> ProcHeap {
        ProcHeap {
            young_from: young,
            young_to: Space::empty(),
            old: SegSpace::new(OLD_SEGMENT_MIN_WORDS),
            off_heap_head: 0,
            high_water: 0,
            minor_count: 0,
            major_threshold: MAJOR_THRESHOLD_MIN_WORDS,
            low_occupancy_streak: 0,
            stress_override: None,
            stats: HeapStats::default(),
            #[cfg(debug_assertions)]
            ensured: 0,
        }
    }

    // ---- the safepoint protocol -------------------------------------------

    /// Must the caller's `ensure(need)` collect before allocating `need`
    /// more words? True when the young space lacks room — or unconditionally
    /// under `AL_GC_STRESS=1` (see [`gc_stress`]), which turns every
    /// safepoint into a collection so any unrooted temporary in the VM is
    /// flushed out deterministically. Every `ensure()` implementation must
    /// route its collect-or-not decision through here; that is what makes
    /// stress mode a property of the heap rather than of each call site.
    pub fn needs_collect(&self, need: usize) -> bool {
        self.stress() || need > self.young_from.available()
    }

    fn stress(&self) -> bool {
        self.stress_override.unwrap_or_else(gc_stress)
    }

    /// Force (or disable) stress mode for this heap regardless of
    /// `AL_GC_STRESS` — test/tooling hook.
    pub fn set_stress(&mut self, on: bool) {
        self.stress_override = Some(on);
    }

    /// Record that `ensure()` has secured capacity for `need` words: until
    /// the next call, at most `need` words may be bump-allocated. Debug
    /// builds enforce this in [`alloc_young`](ProcHeap::alloc_young) (the
    /// watermark check) and verify here that the capacity really exists —
    /// after a correct `ensure()`, `alloc_young` cannot fail.
    ///
    /// The budget *replaces* any previous one. Each safepoint covers exactly
    /// the allocations up to the next safepoint, so leftover slack from an
    /// earlier opcode cannot hide a missing `ensure`.
    pub fn note_ensured(&mut self, need: usize) {
        debug_assert!(
            need <= self.young_from.available(),
            "ensure() noted a budget of {need} words but the young space only has {} available; \
             ensure must collect/grow before noting",
            self.young_from.available()
        );
        #[cfg(debug_assertions)]
        {
            self.ensured = need;
        }
    }

    /// Guarantee room for `need` more words, collecting if required (always
    /// collecting under stress mode) and escalating to a major collection
    /// when the old generation has grown past its threshold. `roots` is the
    /// complete root set — every `Value` that must survive and be rewritten.
    /// Returns the words copied, which the VM charges to the running
    /// process's reduction budget so GC-heavy processes still yield fairly.
    pub fn ensure(&mut self, need: usize, roots: &mut dyn Roots) -> usize {
        let mut copied = 0;
        if self.needs_collect(need) {
            copied += self.minor_gc(roots, need);
            if self.old.used() > self.major_threshold {
                copied += self.major_gc(roots, need);
            }
        }
        self.note_ensured(need);
        copied
    }

    /// Bump-allocate `len` words in the young space. Never collects; `None`
    /// means the caller's `ensure` discipline was violated or capacity must
    /// be grown first.
    pub fn alloc_young(&mut self, len: usize) -> Option<*mut u64> {
        // The watermark check: every mutator allocation must fit the budget
        // the last ensure() secured while the operands were still rooted.
        // Tripping this assert means some path allocated without (or
        // beyond) an ensure — a latent moving-GC dangling-pointer bug,
        // surfaced at the faulting site.
        #[cfg(debug_assertions)]
        {
            assert!(
                len <= self.ensured,
                "rooting-rule violation: allocation of {len} words exceeds the ensured budget \
                 of {} (allocation outside an ensure()d budget)",
                self.ensured
            );
            self.ensured -= len;
        }
        self.young_from.alloc(len)
    }

    // ---- collection ---------------------------------------------------------

    /// A [`Classifier`] covering everything this process owns (young + old):
    /// the source whitelist for spawn-copy and frozen publish.
    pub fn classifier(&self) -> Classifier {
        let mut classifier = Classifier::new();
        classifier.push_space(&self.young_from);
        self.old.add_ranges(&mut classifier);
        classifier
    }

    /// Minor collection: evacuate the young from-space. Survivors of a
    /// previous minor (below the high-water mark) tenure into old; the rest
    /// move to the to-space, which then becomes the new from-space. Old
    /// segments are neither touched nor scanned — immutability makes
    /// old→young pointers impossible (see `MinorDest`). Returns words
    /// copied.
    pub fn minor_gc(&mut self, roots: &mut dyn Roots, need: usize) -> usize {
        self.stats.peak_young_words = self.stats.peak_young_words.max(self.young_from.used());
        debug_assert_eq!(self.young_to.used(), 0);
        // Worst case every allocated word survives, plus the caller's need;
        // never below the current capacity (shrinking is a deliberate,
        // separate policy below).
        let required = self.young_from.used() + need;
        let target = size_for(required).max(self.young_from.capacity());
        if self.young_to.capacity() < target {
            self.young_to = Space::with_capacity(target);
        }
        let mut classifier = Classifier::new();
        classifier.push_space(&self.young_from);
        let high_water = self.high_water;
        let (copied_words, tenured_words) = {
            let mut dest = MinorDest {
                to: SpaceDest::new(&mut self.young_to),
                old: SegDest::new(&mut self.old),
                high_water,
                tenured_words: 0,
            };
            let stats = copy_graph(roots, &classifier, &mut dest, CopyMode::Move);
            (stats.words_copied, dest.tenured_words)
        };
        let (head, dropped) = sweep_off_heap_list(self.off_heap_head, &classifier);
        self.off_heap_head = head;
        self.young_from.reset();
        std::mem::swap(&mut self.young_from, &mut self.young_to);
        // Everything alive right now has survived this minor; next minor it
        // tenures. New allocations land above this mark.
        self.high_water =
            self.young_from.base_addr() + self.young_from.used() * std::mem::size_of::<u64>();
        self.minor_count += 1;
        self.stats.minor_gcs += 1;
        self.stats.words_copied += copied_words as u64;
        self.stats.words_tenured += tenured_words as u64;
        self.stats.binaries_dropped += dropped;
        self.stats.peak_old_words = self.stats.peak_old_words.max(self.old.used());
        // Direct callers (tests, forced GC) must re-ensure before
        // allocating.
        #[cfg(debug_assertions)]
        {
            self.ensured = 0;
        }
        copied_words + self.maybe_shrink(roots, need)
    }

    /// Major (full-sweep) collection: evacuate young *and* old together into
    /// one fresh, compacted young space; the old generation empties and its
    /// segments are freed. Triggered from `ensure` when old occupancy passes
    /// the threshold. Returns words copied.
    pub fn major_gc(&mut self, roots: &mut dyn Roots, need: usize) -> usize {
        let required = self.young_from.used() + self.old.used() + need;
        let mut to = Space::with_capacity(size_for(required));
        let mut classifier = Classifier::new();
        classifier.push_space(&self.young_from);
        self.old.add_ranges(&mut classifier);
        let stats = {
            let mut dest = SpaceDest::new(&mut to);
            copy_graph(roots, &classifier, &mut dest, CopyMode::Move)
        };
        let (head, dropped) = sweep_off_heap_list(self.off_heap_head, &classifier);
        self.off_heap_head = head;
        self.old.clear();
        self.young_from = to;
        self.young_to = Space::empty();
        self.high_water =
            self.young_from.base_addr() + self.young_from.used() * std::mem::size_of::<u64>();
        self.minor_count = 0;
        self.major_threshold = (self.young_from.used() * 2).max(MAJOR_THRESHOLD_MIN_WORDS);
        self.stats.major_gcs += 1;
        self.stats.words_copied += stats.words_copied as u64;
        self.stats.binaries_dropped += dropped;
        #[cfg(debug_assertions)]
        {
            self.ensured = 0;
        }
        stats.words_copied
    }

    /// Shrink policy: after [`SHRINK_AFTER_MINORS`] consecutive minors that
    /// end below 25% occupancy, re-copy the (small) live set into a
    /// table-sized space and drop the oversized slabs.
    fn maybe_shrink(&mut self, roots: &mut dyn Roots, need: usize) -> usize {
        let live = self.young_from.used();
        let capacity = self.young_from.capacity();
        if capacity <= INITIAL_YOUNG_WORDS || live * 4 >= capacity {
            self.low_occupancy_streak = 0;
            return 0;
        }
        self.low_occupancy_streak += 1;
        if self.low_occupancy_streak < SHRINK_AFTER_MINORS {
            return 0;
        }
        self.low_occupancy_streak = 0;
        let target = size_for(live * 2 + need);
        if target >= capacity {
            return 0;
        }
        let mut small = Space::with_capacity(target);
        let mut classifier = Classifier::new();
        classifier.push_space(&self.young_from);
        let stats = {
            let mut dest = SpaceDest::new(&mut small);
            copy_graph(roots, &classifier, &mut dest, CopyMode::Move)
        };
        // Everything in young was live (it just survived a minor), so this
        // sweep drops nothing — it relinks the moved boxes.
        let (head, dropped) = sweep_off_heap_list(self.off_heap_head, &classifier);
        debug_assert_eq!(dropped, 0, "shrink source must be fully evacuated");
        let _ = dropped;
        self.off_heap_head = head;
        self.young_from = small;
        self.young_to = Space::empty();
        self.high_water =
            self.young_from.base_addr() + self.young_from.used() * std::mem::size_of::<u64>();
        self.stats.young_shrinks += 1;
        stats.words_copied
    }

    // ---- spawn and frozen publish -------------------------------------------

    /// Spawn-side graph copy: clone the closure graph reachable from `root`
    /// into a fresh mini-arena sized to exactly that graph, and return it as
    /// the child's heap together with the child's own root. The pair is the
    /// payload of a scheduler `Seed`: the child adopts the heap as its
    /// initial young space and starts from the copied closure, sharing no
    /// heap state with the spawner.
    ///
    /// Contract (what the seed path relies on):
    ///
    /// - **Sharing is preserved.** A node referenced n times in the source
    ///   graph is copied exactly once and referenced n times in the copy
    ///   (forwarding pointers, as in every `copy_graph` use).
    /// - **`Binary` backings are zero-copy.** The bytes live off-heap behind
    ///   an `Arc<[u8]>`; the copy clones the `Arc` (a refcount bump), never
    ///   the bytes, so arbitrarily large binaries spawn in O(1).
    /// - **Frozen references are shared, not copied.** Enum/variant names
    ///   and program constants point into the frozen area, which the
    ///   classifier does not cover.
    /// - **The parent graph is untouched.** Clone mode restores every
    ///   forwarded header before returning; `root` and everything it
    ///   references stay valid, which is why the spawner keeps running with
    ///   no repair pass.
    pub fn spawn_copy(&mut self, root: &Value) -> (ProcHeap, Value) {
        let (heap, child_root, _words) = self.spawn_seed(*root);
        (heap, child_root)
    }

    /// As [`spawn_copy`](ProcHeap::spawn_copy), also returning the words
    /// copied (for reduction charging).
    ///
    /// Two passes: the graph's live size is unknowable up front without a
    /// marking pass (sharing makes a naive size walk overcount), so pass 1
    /// clones into a growable scratch — which measures the graph exactly —
    /// and pass 2 moves the scratch into a right-sized young space.
    pub fn spawn_seed(&mut self, root: Value) -> (ProcHeap, Value, usize) {
        let mut slot = [root];
        let mut scratch = SegSpace::new(INITIAL_YOUNG_WORDS);
        let classifier = self.classifier();
        let (pass1, scratch_head) = {
            let mut dest = SegDest::new(&mut scratch);
            let stats = copy_graph(&mut slot, &classifier, &mut dest, CopyMode::Clone);
            (stats, dest.off_heap_head)
        };
        let live = scratch.used();
        let mut young = Space::with_capacity(size_for(live));
        let mut scratch_classifier = Classifier::new();
        scratch.add_ranges(&mut scratch_classifier);
        let pass2 = {
            let mut dest = SpaceDest::new(&mut young);
            copy_graph(&mut slot, &scratch_classifier, &mut dest, CopyMode::Move)
        };
        // Relink the moved boxes into the child's list. Every scratch object
        // was just evacuated, so nothing is dropped.
        let (child_head, dropped) = sweep_off_heap_list(scratch_head, &scratch_classifier);
        debug_assert_eq!(dropped, 0, "spawn scratch must be fully evacuated");
        let _ = dropped;
        let mut child = ProcHeap::from_young(young);
        child.off_heap_head = child_head;
        (child, slot[0], pass1.words_copied + pass2.words_copied)
    }

    /// Publish the graph reachable from `root` into the frozen area
    /// (`publish_global`): a Clone-mode [`copy_graph`] with the builder as
    /// the destination. Returns the frozen root and the words copied. The
    /// source value stays valid; references that already point into the
    /// frozen area are reused as-is (the classifier does not cover them),
    /// so re-publishing shared structure never duplicates it.
    ///
    /// The returned pointer must only be shared through a release/acquire
    /// publication (the `globals_version` gate) — see the `frozen` module
    /// docs for the protocol.
    pub fn publish_frozen(&mut self, builder: &mut FrozenBuilder, root: Value) -> (Value, usize) {
        let mut slot = [root];
        let classifier = self.classifier();
        let stats = {
            let mut dest = FrozenDest {
                builder,
                copied: Vec::new(),
                next: 0,
            };
            copy_graph(&mut slot, &classifier, &mut dest, CopyMode::Clone)
        };
        (slot[0], stats.words_copied)
    }

    // ---- introspection --------------------------------------------------------

    /// "Mine" classification: does `addr` point into this process's own
    /// young or old spaces? `false` means frozen-or-foreign — not traced,
    /// not moved.
    pub fn contains(&self, addr: usize) -> bool {
        self.young_from.contains(addr) || self.young_to.contains(addr) || self.old.contains(addr)
    }

    /// Words currently allocated across young and old spaces.
    pub fn used_words(&self) -> usize {
        self.young_from.used() + self.old.used()
    }

    /// Total words of capacity across young and old spaces.
    pub fn capacity_words(&self) -> usize {
        self.young_from.capacity() + self.old.capacity()
    }

    /// Head of the intrusive off-heap (Arc-holding box) list; `0` when
    /// empty.
    pub fn off_heap_head(&self) -> usize {
        self.off_heap_head
    }

    /// Mutator-side push-front onto the off-heap list (the field doc covers
    /// the collector-side sweep). Sole production caller is the `Arena`
    /// impl's `link_off_heap` in `bytecode::value`, which threads each
    /// freshly built Binary box: `addr` must be a live, non-forwarded,
    /// fully written `Arc`-owning Binary box inside this heap whose link
    /// word already holds the previous head. Anything else — a foreign or
    /// stale address, a half-written box, an unset link word — corrupts the
    /// unsafe list walks in [`sweep_off_heap_list`] and this heap's `Drop`.
    /// Tests may also reset the head to `0` after condemning boxes by hand,
    /// so the teardown sweep does not double-drop their backings.
    pub fn set_off_heap_head(&mut self, addr: usize) {
        self.off_heap_head = addr;
    }

    /// The minor-GC tenuring boundary (see field doc).
    pub fn high_water(&self) -> usize {
        self.high_water
    }

    /// Minor collections since the last major collection.
    pub fn minor_count(&self) -> u32 {
        self.minor_count
    }

    /// Capacity of the young allocation space alone (excludes old
    /// segments).
    pub fn young_capacity(&self) -> usize {
        self.young_from.capacity()
    }

    /// Words currently allocated in the young space alone.
    pub fn young_used(&self) -> usize {
        self.young_from.used()
    }

    /// Words currently tenured in the old generation.
    pub fn old_used(&self) -> usize {
        self.old.used()
    }

    pub fn stats(&self) -> &HeapStats {
        &self.stats
    }

    /// Mark `words` of the young space as occupied (saturating at
    /// capacity), bypassing the mutator watermark. Collector-side/test
    /// utility: it models survivor occupancy, which is the collector's to
    /// record, not an allocation under an `ensure` budget — tests use it to
    /// fill the young space so the next `ensure` must collect.
    pub fn occupy_young(&mut self, words: usize) {
        let _ = self
            .young_from
            .alloc(words.min(self.young_from.available()));
    }
}

/// The empty heap: owns no slabs, allocates nothing (until an `ensure`
/// installs a young space). This is the scheduler's "no process is current"
/// placeholder — `suspend_current` `mem::take`s the live heap out of the VM
/// and leaves this behind.
impl Default for ProcHeap {
    fn default() -> ProcHeap {
        ProcHeap::from_young(Space::empty())
    }
}

impl Drop for ProcHeap {
    fn drop(&mut self) {
        // Process death releases every backing the heap's boxes still own —
        // the off-heap list is exactly the set of live Arc-owning boxes.
        let mut cur = self.off_heap_head;
        while cur != 0 {
            let obj = cur as *const u64;
            // SAFETY: at rest (no collection in flight) the list holds live,
            // non-forwarded Binary boxes in slabs this heap still owns; each
            // owns exactly one strong count.
            unsafe {
                debug_assert!(!header_is_forwarding(*obj));
                let next = binary_off_heap_next(obj);
                binary_drop_backing(obj);
                cur = next;
            }
        }
        self.off_heap_head = 0;

        if !heap_stats_enabled() {
            return;
        }
        let s = &self.stats;
        if s.minor_gcs == 0 && s.major_gcs == 0 && self.used_words() == 0 {
            return;
        }
        eprintln!(
            "[al-heap] minors={} majors={} copied={}w tenured={}w shrinks={} \
             binaries_dropped={} peak_young={}w peak_old={}w live_at_drop={}w",
            s.minor_gcs,
            s.major_gcs,
            s.words_copied,
            s.words_tenured,
            s.young_shrinks,
            s.binaries_dropped,
            s.peak_young_words,
            s.peak_old_words,
            self.used_words(),
        );
    }
}
