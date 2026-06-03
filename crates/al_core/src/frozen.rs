//! Frozen shared area: program-wide constants + write-once globals.
//!
//! A [`FrozenArea`] is a set of append-only word segments that are:
//!
//! - **immutable once written** — every word is written exactly once, by the
//!   allocating [`FrozenBuilder`], before any pointer to it escapes;
//! - **never collected** — the GC does not trace or move frozen objects.
//!   Pointer classification in the heap is by address-range check against
//!   the process's *own* spaces; anything outside them (frozen or foreign)
//!   is left untouched, so the collector never needs to consult this module;
//! - **stable for the program lifetime** — segment storage is a separately
//!   boxed slice whose heap allocation never moves or reallocates, and the
//!   area itself is `Arc`-held by the runtime, so a raw pointer into a
//!   segment stays valid until the program ends;
//! - **never runs destructors** — segments store raw `u64` words, so when
//!   the area is finally dropped at process exit the words drop as plain
//!   integers. Any *owning* pointer written into a segment (e.g. the
//!   `Arc<[u8]>` backing of a frozen binary, whose strong count is bumped at
//!   freeze time) is therefore never released: the count it holds leaks for
//!   the program's life. That leak is the price of pointers that must stay
//!   valid forever, and it is why writers must only ever store owners they
//!   intend to keep alive until exit.
//!
//! Writers:
//!
//! - hydration builds program literals/constants through an explicit
//!   `&mut FrozenBuilder` while the program is being loaded;
//! - publishing a top-level binding deep-copies the global's value graph
//!   into the area through the `al` crate's `vm::freeze::freeze_global`
//!   (the heap's `copy_graph` with a builder as destination). The
//!   subsequent `Runtime::publish_global` does no copying: it only stores
//!   the already-frozen word into the shared table and release-bumps
//!   `globals_version`.
//!
//! # Publication protocol (the frozen invariant)
//!
//! Allocation hands out disjoint word ranges under the area's mutex, but the
//! contents are written through the returned raw pointer *outside* the lock,
//! and readers never lock at all — they just dereference. That is sound
//! because of a write-once + happens-before discipline:
//!
//! 1. the builder fully initializes every word of an object **before** the
//!    pointer is shared with any other thread;
//! 2. cross-thread publication piggybacks on an existing release/acquire
//!    edge: runtime globals are stored into the shared table and then
//!    `globals_version` is bumped with `Release`; a scheduler `Acquire`-loads
//!    the version before reading the table (`Runtime::publish_global` /
//!    `Vm::sync_globals` in `crates/al/src/vm`). Observing the version bump
//!    therefore makes the segment contents visible. Hydration-time constants
//!    are published by the thread/channel handoff that distributes the
//!    program itself, which synchronizes the same way;
//! 3. after publication the words are never written again, so unlocked reads
//!    can never race a write.
//!
//! A frozen pointer must consequently never be shared through a channel or
//! table that lacks such an edge — `globals_version` (or an equivalent
//! synchronizing handoff) is the only publication door.

use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::fmt;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::bytecode::{Value, enum_hash_with_payload, enum_name_prefix_hash};

/// Initial segment capacity in words (32 KiB). Small programs stay in one
/// segment; segment capacities double from here so the segment list stays
/// short (O(log total) segments) and `contains` range checks stay cheap.
const DEFAULT_SEGMENT_WORDS: usize = 4096;

/// Ceiling for the doubling growth, in words (8 MiB). A single allocation
/// larger than this still gets its own exactly-sized segment.
const MAX_SEGMENT_WORDS: usize = 1 << 20;

/// One append-only segment. The words live in a separately boxed slice, so
/// growing the segment *list* (a `Vec` re-allocation) moves this struct but
/// never the words — raw pointers into `words` are stable for the life of
/// the segment, and segments are never dropped while the area lives.
///
/// `UnsafeCell` is what makes the one-time initializing write through a
/// shared `FrozenArea` defined behavior; the absence of a *data race* comes
/// from the publication protocol in the module docs (disjoint ranges handed
/// out under the mutex, write-once before publication, release/acquire on
/// publication).
struct FrozenSegment {
    words: Box<[UnsafeCell<u64>]>,
    /// Bump index: words below `used` are allocated (their contents may
    /// still be in flight on the allocating thread until publication).
    used: usize,
}

impl FrozenSegment {
    fn with_capacity(words: usize) -> FrozenSegment {
        FrozenSegment {
            words: (0..words).map(|_| UnsafeCell::new(0)).collect(),
            used: 0,
        }
    }

    fn base(&self) -> *mut u64 {
        UnsafeCell::raw_get(self.words.as_ptr())
    }
}

/// The program-wide frozen area. Created once at program load, shared as
/// `Arc<FrozenArea>` by the runtime, every scheduler, and the hydration
/// path. See the module docs for the immutability/publication invariants.
pub struct FrozenArea {
    segments: Mutex<Vec<FrozenSegment>>,
}

// Shared across every scheduler thread by construction.
const fn assert_send_sync<T: Send + Sync>() {}
const _: () = assert_send_sync::<FrozenArea>();

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl FrozenArea {
    pub fn new() -> FrozenArea {
        FrozenArea {
            segments: Mutex::new(Vec::new()),
        }
    }

    /// An append handle. Multiple builders may exist: one for hydration, one
    /// long-lived builder per `Vm` (used for every runtime freeze), plus
    /// ad-hoc ones for one-off freezes. The area's mutex serializes their
    /// bumps, and the ranges they receive are disjoint, so their content
    /// writes can proceed in parallel without synchronization.
    pub fn builder(self: &Arc<Self>) -> FrozenBuilder {
        FrozenBuilder {
            area: Arc::clone(self),
            strs: HashMap::new(),
            str_arrays: HashMap::new(),
            label_tuples: HashMap::new(),
        }
    }

    /// Whether `ptr` points into allocated frozen storage. The GC does not
    /// need this (it skips everything outside the process's own spaces); it
    /// exists for debug assertions and tests.
    pub fn contains(&self, ptr: *const u64) -> bool {
        let addr = ptr as usize;
        let segments = lock(&self.segments);
        segments.iter().any(|seg| {
            let base = seg.base() as usize;
            addr >= base && addr < base + seg.used * size_of::<u64>()
        })
    }

    pub fn segment_count(&self) -> usize {
        lock(&self.segments).len()
    }

    /// Total allocated words across all segments (for stats/tests).
    pub fn words_used(&self) -> usize {
        lock(&self.segments).iter().map(|s| s.used).sum()
    }

    /// Total reserved words across all segments (for stats/tests).
    pub fn words_capacity(&self) -> usize {
        lock(&self.segments).iter().map(|s| s.words.len()).sum()
    }

    /// Bump-allocate `words` words. Appends a new segment when the open
    /// (last) one is full; earlier segments are never touched again, which
    /// is what makes the area append-only.
    fn alloc(&self, words: usize) -> NonNull<u64> {
        assert!(words > 0, "frozen allocation must be at least one word");
        let mut segments = lock(&self.segments);
        if let Some(seg) = segments.last_mut()
            && seg.words.len() - seg.used >= words
        {
            let at = seg.used;
            seg.used += words;
            // SAFETY: `at + words <= capacity`, so the offset stays in
            // bounds of the boxed slice; the slice allocation is non-null
            // and outlives the area.
            return unsafe { NonNull::new_unchecked(seg.base().add(at)) };
        }
        let cap = Self::next_capacity(segments.last().map(|s| s.words.len()), words);
        let mut seg = FrozenSegment::with_capacity(cap);
        seg.used = words;
        // SAFETY: a fresh boxed slice's base pointer is non-null.
        let ptr = unsafe { NonNull::new_unchecked(seg.base()) };
        segments.push(seg);
        ptr
    }

    fn next_capacity(prev: Option<usize>, words: usize) -> usize {
        let doubled = prev.map_or(DEFAULT_SEGMENT_WORDS, |c| c.saturating_mul(2));
        doubled
            .clamp(DEFAULT_SEGMENT_WORDS, MAX_SEGMENT_WORDS)
            .max(words)
    }
}

impl Default for FrozenArea {
    fn default() -> FrozenArea {
        FrozenArea::new()
    }
}

impl fmt::Debug for FrozenArea {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrozenArea")
            .field("segments", &self.segment_count())
            .field("words_used", &self.words_used())
            .finish()
    }
}

/// Intern table for string-aggregate constants, keyed by the aggregate's
/// string contents.
type StrAggregateMap = HashMap<Box<[Box<str>]>, Value>;

/// Append handle to a [`FrozenArea`]. Hydration threads one through the
/// compiler as `&mut FrozenBuilder` so write access to the frozen area is
/// visible in signatures; `copy_graph` takes one as its destination when
/// publishing a global. Several builders coexist over one area (see
/// [`FrozenArea::builder`]).
///
/// Beyond the raw word allocator, the builder is the construction door for
/// every program constant (the constant methods below): it interns string
/// contents so each distinct name/label gets one canonical frozen
/// allocation *per builder*, shared by every constant-pool entry built
/// through it — and through them by every runtime enum/closure cloned out
/// of the pool. The intern tables map contents to the canonical frozen
/// `Value` (the allocation's frozen pointer).
///
/// Interning is per-builder, not per-area: the same contents frozen
/// through two different builders (say a hydration constant and a runtime
/// `publish_global` of the same string) yield two distinct frozen
/// allocations. That is by design — interning is purely a space
/// optimization, and no correctness property depends on interned strings
/// sharing an address. Equality, hashing, and pattern matching all compare
/// string contents, never pointers, so cross-builder duplicates are
/// invisible to the program.
pub struct FrozenBuilder {
    area: Arc<FrozenArea>,
    /// Canonical frozen `Str` constant per distinct contents.
    strs: HashMap<Box<str>, Value>,
    /// Canonical frozen all-string array constant (enum field-label list
    /// pool entries) per distinct contents.
    str_arrays: StrAggregateMap,
    /// Canonical frozen `Tuple`-of-`Str` label list per distinct contents
    /// (the labels reference stored inside enum objects).
    label_tuples: StrAggregateMap,
}

impl FrozenBuilder {
    /// Allocate `words` zero-initialized words and return a pointer to the
    /// first. The pointer is 8-byte aligned and stable for the program
    /// lifetime. The caller must fully initialize the words before letting
    /// the pointer escape this thread (module docs, "Publication protocol").
    pub fn alloc(&mut self, words: usize) -> NonNull<u64> {
        self.area.alloc(words)
    }

    /// Allocate and copy a fully formed object image (header + payload
    /// words) into the area.
    pub fn alloc_from(&mut self, image: &[u64]) -> NonNull<u64> {
        let dst = self.area.alloc(image.len());
        // SAFETY: `dst` is a fresh, disjoint allocation of exactly
        // `image.len()` words.
        unsafe { std::ptr::copy_nonoverlapping(image.as_ptr(), dst.as_ptr(), image.len()) };
        dst
    }

    pub fn area(&self) -> &FrozenArea {
        &self.area
    }
}

// ---------------------------------------------------------------------------
// Constant `Value` construction (compiler + hydration)
// ---------------------------------------------------------------------------
//
// Program literals/constants are built exclusively through these methods:
// the compiler owns a builder for the program it is emitting, and the stdlib
// hydration path (`StaticStdlib::hydrate_program`) receives one as an
// explicit `&mut FrozenBuilder`. This is what makes `Program` `Send + Sync`:
// every constant `Value` is an immediate or points into the program's frozen
// area, never into a process heap.
//
// Each method writes the constant's `[header][payload…]` object image into
// the area through the builder's `Arena` impl (the `Value::*_in`
// constructors in `bytecode::value`) and returns a `Value` holding the
// frozen pointer; immediates (int/float/bool/nil) have no backing words and
// the methods exist so constant construction uniformly goes through the
// builder. String contents — and label lists — are interned, so every
// constant-pool entry naming the same string (enum names, variant names,
// field labels) shares one canonical frozen allocation, and runtime values
// cloned out of the pool keep pointing at it.
impl FrozenBuilder {
    /// A frozen Int constant. Small ints are immediates (no backing
    /// allocation); the method exists so constant construction uniformly
    /// goes through the builder.
    pub fn int(&mut self, i: i64) -> Value {
        Value::int_in(self, i)
    }

    /// A frozen Float constant (immediate; see [`FrozenBuilder::int`]).
    pub fn float(&mut self, f: f64) -> Value {
        Value::float(f)
    }

    /// A frozen Bool constant (immediate; see [`FrozenBuilder::int`]).
    pub fn bool(&mut self, b: bool) -> Value {
        Value::bool(b)
    }

    /// The frozen Nil constant (immediate; see [`FrozenBuilder::int`]).
    pub fn nil(&mut self) -> Value {
        Value::nil()
    }

    /// A frozen Range constant.
    pub fn range(&mut self, start: i64, end: i64) -> Value {
        Value::range_in(self, start, end)
    }

    /// A frozen string constant: one canonical `Value` per distinct
    /// contents per program. Enum/variant names and field labels all resolve
    /// through here, so every compile-time occurrence of the same name
    /// points at the same frozen allocation.
    pub fn str(&mut self, s: &str) -> Value {
        if let Some(v) = self.strs.get(s) {
            return *v;
        }
        let v = Value::str_in(self, s);
        self.strs.insert(Box::from(s), v);
        v
    }

    /// A frozen all-string array constant (enum field-label lists). Interned
    /// as a unit: every construction site of the same variant shares one
    /// array allocation, and each element shares the canonical interned
    /// string.
    pub fn str_array(&mut self, items: &[&str]) -> Value {
        self.intern_str_aggregate(items, |b| &mut b.str_arrays, Value::array_in)
    }

    /// A frozen array constant over already-built (frozen) elements.
    pub fn array(&mut self, items: Vec<Value>) -> Value {
        Value::array_in(self, &items)
    }

    /// A frozen tuple constant over already-built (frozen) elements.
    pub fn tuple(&mut self, items: Vec<Value>) -> Value {
        Value::tuple_in(self, &items)
    }

    /// A frozen binary constant over whole bytes.
    pub fn binary(&mut self, bytes: Vec<u8>) -> Value {
        Value::binary_in(self, bytes)
    }

    /// A frozen binary constant of `bit_len` bits.
    pub fn binary_bits(&mut self, bytes: Vec<u8>, bit_len: u64) -> Value {
        Value::binary_bits_in(self, bytes, bit_len)
    }

    /// A frozen closure constant over already-built captures.
    pub fn closure(&mut self, func_idx: i32, captures: Vec<Value>) -> Value {
        Value::closure_in(self, func_idx, &captures)
    }

    /// A frozen enum constant. The names and field labels are interned so
    /// they point at the area's canonical allocations; the hash is computed
    /// exactly the way the VM computes it at construction so equality keeps
    /// working.
    pub fn enum_(
        &mut self,
        type_id: i32,
        enum_name: &str,
        variant_name: &str,
        field_labels: &[&str],
        payload: Vec<Value>,
    ) -> Value {
        let hash = enum_hash_with_payload(enum_name_prefix_hash(enum_name, variant_name), &payload);
        let en = self.str(enum_name);
        let vn = self.str(variant_name);
        let labels_tuple = self.label_tuple(field_labels);
        Value::enum_in(self, type_id, hash, en, vn, labels_tuple, &payload)
    }

    /// The canonical frozen labels reference for enum objects. An enum
    /// object's labels word holds a `Tuple` whose elements are all `Str`
    /// values — the shape [`Value::enum_in`] requires for its `labels`
    /// argument. The tuple is interned as a unit so every constant of the
    /// same variant shares one allocation.
    fn label_tuple(&mut self, labels: &[&str]) -> Value {
        self.intern_str_aggregate(labels, |b| &mut b.label_tuples, Value::tuple_in)
    }

    /// Shared interning loop for the string-aggregate constants
    /// ([`FrozenBuilder::str_array`] and label tuples): look up the contents
    /// in the chosen intern table, and on a miss intern each element string,
    /// build the aggregate with `construct`, and cache it. `map` selects the
    /// table rather than borrowing it up front so `self` stays free for the
    /// element interning in between.
    fn intern_str_aggregate(
        &mut self,
        items: &[&str],
        map: fn(&mut Self) -> &mut StrAggregateMap,
        construct: fn(&mut Self, &[Value]) -> Value,
    ) -> Value {
        let key: Box<[Box<str>]> = items.iter().map(|&s| Box::from(s)).collect();
        if let Some(v) = map(self).get(&key) {
            return *v;
        }
        let elems: Vec<Value> = items.iter().map(|s| self.str(s)).collect();
        let v = construct(self, &elems);
        map(self).insert(key, v);
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Write `len` recognizable words at `ptr` (tagged by `tag`).
    unsafe fn fill(ptr: NonNull<u64>, tag: u64, len: usize) {
        for i in 0..len {
            unsafe {
                ptr.as_ptr()
                    .add(i)
                    .write(tag.wrapping_mul(1_000_003) + i as u64)
            };
        }
    }

    /// Assert the words written by `fill(ptr, tag, len)` are intact.
    unsafe fn check(ptr: NonNull<u64>, tag: u64, len: usize) {
        for i in 0..len {
            let got = unsafe { ptr.as_ptr().add(i).read() };
            assert_eq!(got, tag.wrapping_mul(1_000_003) + i as u64);
        }
    }

    #[test]
    fn single_alloc_round_trip() {
        let area = Arc::new(FrozenArea::new());
        let mut b = area.builder();
        let p = b.alloc(3);
        assert_eq!(p.as_ptr() as usize % 8, 0, "word alignment");
        unsafe { fill(p, 7, 3) };
        unsafe { check(p, 7, 3) };
        assert!(area.contains(p.as_ptr()));
        assert_eq!(area.words_used(), 3);
        assert_eq!(area.segment_count(), 1);
    }

    #[test]
    fn pointers_stable_across_segment_growth() {
        let area = Arc::new(FrozenArea::new());
        let mut b = area.builder();
        let mut allocs = Vec::new();
        // Enough variable-size objects to force several segment appends.
        for tag in 0..20_000u64 {
            let len = 1 + (tag as usize % 17);
            let p = b.alloc(len);
            unsafe { fill(p, tag, len) };
            allocs.push((p, tag, len));
        }
        assert!(area.segment_count() > 1, "growth should append segments");
        // Every earlier pointer still reads back its exact contents: nothing
        // moved when later segments were appended.
        for (p, tag, len) in allocs {
            unsafe { check(p, tag, len) };
            assert!(area.contains(p.as_ptr()));
        }
    }

    #[test]
    fn oversized_alloc_gets_its_own_segment() {
        let area = Arc::new(FrozenArea::new());
        let mut b = area.builder();
        b.alloc(1);
        let big = MAX_SEGMENT_WORDS + 9;
        let p = b.alloc(big);
        unsafe { fill(p, 42, 8) };
        unsafe { check(p, 42, 8) };
        assert_eq!(area.segment_count(), 2);
        assert_eq!(area.words_used(), 1 + big);
        assert!(area.words_capacity() > big);
    }

    #[test]
    fn contains_is_exact_to_the_used_range() {
        let area = Arc::new(FrozenArea::new());
        let mut b = area.builder();
        let p = b.alloc(4);
        // Interior word: in.
        assert!(area.contains(unsafe { p.as_ptr().add(3) }));
        // One past the used range: out (segment slack is not "frozen" yet).
        assert!(!area.contains(unsafe { p.as_ptr().add(4) }));
        // Unrelated pointers: out.
        let stack_word = 0u64;
        assert!(!area.contains(&raw const stack_word));
        assert!(!area.contains(std::ptr::null()));
    }

    #[test]
    fn alloc_from_copies_image() {
        let area = Arc::new(FrozenArea::new());
        let mut b = area.builder();
        let image = [0xDEAD_BEEFu64, 1, 2, 3, u64::MAX];
        let p = b.alloc_from(&image);
        let got: Vec<u64> = (0..image.len())
            .map(|i| unsafe { p.as_ptr().add(i).read() })
            .collect();
        assert_eq!(got, image);
        assert!(b.area().contains(p.as_ptr()));
    }

    #[test]
    fn concurrent_builders_get_disjoint_ranges() {
        let area = Arc::new(FrozenArea::new());
        let threads = 8;
        let per_thread = 2_000usize;
        let mut handles = Vec::new();
        for t in 0..threads as u64 {
            let area = Arc::clone(&area);
            handles.push(std::thread::spawn(move || {
                let mut b = area.builder();
                let mut out = Vec::new();
                for i in 0..per_thread as u64 {
                    let tag = t * per_thread as u64 + i;
                    let len = 1 + (tag as usize % 9);
                    let p = b.alloc(len);
                    unsafe { fill(p, tag, len) };
                    out.push((p.as_ptr() as usize, tag, len));
                }
                out
            }));
        }
        let mut all: Vec<(usize, u64, usize)> = Vec::new();
        for h in handles {
            all.extend(h.join().unwrap());
        }
        // Nothing was overwritten by a racing builder…
        for &(addr, tag, len) in &all {
            let p = NonNull::new(addr as *mut u64).unwrap();
            unsafe { check(p, tag, len) };
        }
        // …because every handed-out range is disjoint.
        all.sort_unstable_by_key(|&(addr, ..)| addr);
        for pair in all.windows(2) {
            let (addr, _, len) = pair[0];
            assert!(addr + len * size_of::<u64>() <= pair[1].0, "ranges overlap");
        }
        assert_eq!(
            area.words_used(),
            all.iter().map(|&(_, _, len)| len).sum::<usize>()
        );
    }

    /// The cross-thread publication contract from the module docs, in the
    /// exact shape the runtime uses for globals: writer fills an object,
    /// stores its pointer in a slot, then bumps a version with `Release`;
    /// reader `Acquire`-loads the version and may then dereference every
    /// pointer published below it without locks.
    #[test]
    fn publication_via_version_release_acquire() {
        let area = Arc::new(FrozenArea::new());
        let total = 4_000usize;
        let slots: Arc<Vec<AtomicU64>> = Arc::new((0..total).map(|_| AtomicU64::new(0)).collect());
        let version = Arc::new(AtomicU64::new(0));

        let writer = {
            let (area, slots, version) =
                (Arc::clone(&area), Arc::clone(&slots), Arc::clone(&version));
            std::thread::spawn(move || {
                let mut b = area.builder();
                for i in 0..total {
                    let len = 1 + i % 5;
                    let p = b.alloc(len);
                    // Contents fully written BEFORE the pointer escapes.
                    unsafe { fill(p, i as u64, len) };
                    slots[i].store(p.as_ptr() as u64, Ordering::Relaxed);
                    version.fetch_add(1, Ordering::Release);
                }
            })
        };

        let reader = {
            let (slots, version) = (Arc::clone(&slots), Arc::clone(&version));
            std::thread::spawn(move || {
                let mut seen = 0usize;
                while seen < total {
                    let v = version.load(Ordering::Acquire) as usize;
                    while seen < v {
                        let addr = slots[seen].load(Ordering::Relaxed);
                        let p = NonNull::new(addr as *mut u64).unwrap();
                        // The Acquire above makes the writer's segment
                        // contents visible — no locks on the read side.
                        unsafe { check(p, seen as u64, 1 + seen % 5) };
                        seen += 1;
                    }
                    std::hint::spin_loop();
                }
            })
        };

        writer.join().unwrap();
        reader.join().unwrap();
    }

    /// The frozen object address of a heap-backed constant, for identity
    /// assertions: interning must hand out the *same allocation*, not just
    /// equal contents.
    fn addr(v: Value) -> usize {
        v.object_addr().expect("constant should be heap-backed")
    }

    /// Distinct constant-pool entries naming the same string must share one
    /// canonical allocation in the frozen area — the interning foundation
    /// that [`enum_names_and_labels_point_into_the_frozen_area`] builds on.
    #[test]
    fn str_constants_intern_to_one_allocation() {
        let area = Arc::new(FrozenArea::new());
        let mut b = area.builder();
        let a1 = b.str("Some");
        let a2 = b.str("Some");
        let other = b.str("None");
        assert_eq!(a1.as_str(), Some("Some"));
        assert_eq!(addr(a1), addr(a2), "same contents, same allocation");
        assert_ne!(addr(a1), addr(other));
        assert!(area.contains(addr(a1) as *const u64));
        // Interning means no second allocation: the area holds exactly the
        // words of "Some" and "None".
        let words = area.words_used();
        b.str("Some");
        assert_eq!(area.words_used(), words);
    }

    #[test]
    fn str_arrays_intern_as_a_unit() {
        let area = Arc::new(FrozenArea::new());
        let mut b = area.builder();
        let l1 = b.str_array(&["host", "port"]);
        let l2 = b.str_array(&["host", "port"]);
        let l3 = b.str_array(&["host"]);
        assert_eq!(addr(l1), addr(l2), "same contents, same array");
        assert_ne!(addr(l1), addr(l3));
        assert!(area.contains(addr(l1) as *const u64));
        // Elements share the canonical interned strings.
        let elem = l1.as_array().unwrap().get(0).unwrap();
        assert_eq!(elem.as_str(), Some("host"));
        assert_eq!(addr(elem), addr(b.str("host")));
    }

    /// Enum constants built through the builder carry interned names/labels:
    /// every variant constructed from compile-time constants shares the
    /// area's canonical string and label-tuple allocations.
    #[test]
    fn enum_names_and_labels_point_into_the_frozen_area() {
        let area = Arc::new(FrozenArea::new());
        let mut b = area.builder();
        let v1 = b.enum_(7, "Credentials", "Basic", &["user", "pass"], vec![]);
        let v2 = b.enum_(7, "Credentials", "Basic", &["user", "pass"], vec![]);
        assert_ne!(addr(v1), addr(v2), "distinct enum objects");
        assert!(area.contains(addr(v1) as *const u64));
        let (e1, e2) = (v1.as_enum().unwrap(), v2.as_enum().unwrap());
        assert_eq!(e1.enum_name(), "Credentials");
        assert_eq!(e1.variant_name(), "Basic");
        assert_eq!(e1.field_labels()[1].as_str(), Some("pass"));
        assert_eq!(e1.hash(), e2.hash());
        // Names and the labels tuple are canonical frozen allocations.
        assert_eq!(addr(e1.enum_name_value()), addr(e2.enum_name_value()));
        assert_eq!(addr(e1.variant_name_value()), addr(e2.variant_name_value()));
        assert_eq!(addr(e1.labels_value()), addr(e2.labels_value()));
        assert_eq!(addr(e1.enum_name_value()), addr(b.str("Credentials")));
        assert!(area.contains(addr(e1.labels_value()) as *const u64));
    }
}
