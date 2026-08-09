//! Frozen shared area: program-wide constants + write-once globals.
//!
//! A [`FrozenArea`] is a set of append-only word segments. Every word is
//! written once, by the allocating [`FrozenBuilder`], before any pointer to it
//! escapes. Frozen objects are immortal, so refcounting skips them and nothing
//! is ever freed. Segment storage is a boxed slice that never moves, so a raw
//! pointer into it stays valid until the program ends.
//!
//! Segments hold raw `u64` words and run no destructors. An *owning* pointer
//! written into one (the `Arc<[u8]>` backing of a frozen binary, say) is never
//! released — its strong count leaks for the program's life. Only store owners
//! you mean to keep alive until exit.
//!
//! # Publication protocol
//!
//! Allocation hands out disjoint word ranges under the area's mutex, but
//! contents are written outside the lock and readers never lock at all. Sound
//! only because:
//!
//! 1. the builder fully initializes an object before its pointer is shared
//!    with another thread;
//! 2. cross-thread publication rides an existing release/acquire edge —
//!    `globals_version` in `crates/al/src/vm`, or the handoff that distributes
//!    the program itself;
//! 3. published words are never written again.
//!
//! So a frozen pointer must never be shared through a channel or table that
//! lacks such an edge.
//!
//! # Why this module keeps `unsafe`
//!
//! Segment writes go through raw pointers and must stay that way. One boxed
//! slice backs many allocations, and earlier ones may already be published and
//! read by other threads with no lock. Forming `&mut seg.words[..]` would
//! assert exclusive access over those published words and alias the readers —
//! UB even if the write only touches its own disjoint range. `UnsafeCell` plus
//! raw pointers is the only sound way to express that, so this is a designated
//! unsafe module behind safe public APIs.

#![allow(unsafe_code)]

use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::ptr::NonNull;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use indexmap::{Equivalent, IndexMap};

use crate::bytecode::{Value, enum_hash_with_payload, enum_name_prefix_hash};

/// Initial segment capacity in words (32 KiB). Capacities double from here so
/// the segment list stays short and `contains` range checks stay cheap.
const DEFAULT_SEGMENT_WORDS: usize = 4096;

/// Ceiling for the doubling growth, in words (8 MiB). A single allocation
/// larger than this still gets its own exactly-sized segment.
const MAX_SEGMENT_WORDS: usize = 1 << 20;

/// One append-only segment. The words live in a separately boxed slice, so
/// growing the segment *list* moves this struct but never the words: raw
/// pointers into `words` stay valid for the life of the area.
struct FrozenSegment {
    words: Box<[UnsafeCell<u64>]>,
    /// Bump index. Words below it are allocated, but their contents may still
    /// be in flight on the allocating thread until publication.
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
/// `Arc<FrozenArea>` by the runtime, every scheduler, and the hydration path.
/// See the module docs for the immutability/publication invariants.
pub struct FrozenArea {
    segments: Mutex<Vec<FrozenSegment>>,
}

const _: () = crate::assert_send_sync::<FrozenArea>();

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl FrozenArea {
    pub fn new() -> FrozenArea {
        FrozenArea {
            segments: Mutex::new(Vec::new()),
        }
    }

    /// An append handle. Several may coexist over one area: the mutex
    /// serializes their bumps and the ranges they get are disjoint, so their
    /// content writes proceed in parallel without synchronization.
    pub fn builder(self: &Arc<Self>) -> FrozenBuilder {
        FrozenBuilder {
            area: Arc::clone(self),
            strs: HashMap::new(),
            ints: HashMap::new(),
            str_arrays: IndexMap::new(),
            label_tuples: IndexMap::new(),
        }
    }

    /// Whether `ptr` points into allocated frozen storage. For debug
    /// assertions and tests only; refcounting skips immortals via a header bit.
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

    /// Bump-allocate `words` words. Appends a new segment when the last one is
    /// full; earlier segments are never touched again.
    #[allow(clippy::expect_used)]
    fn alloc(&self, words: usize) -> NonNull<u64> {
        assert!(words > 0, "frozen allocation must be at least one word");
        let mut segments = lock(&self.segments);
        if let Some(seg) = segments.last_mut()
            && seg.words.len() - seg.used >= words
        {
            let at = seg.used;
            seg.used += words;
            // SAFETY: `at + words <= capacity`, so the offset stays in bounds.
            let ptr = unsafe { seg.base().add(at) };
            return NonNull::new(ptr).expect("frozen segment base is non-null");
        }
        let cap = Self::next_capacity(segments.last().map(|s| s.words.len()), words);
        let mut seg = FrozenSegment::with_capacity(cap);
        seg.used = words;
        let ptr = NonNull::new(seg.base()).expect("frozen segment base is non-null");
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

/// A constant `Value` with frozen provenance: an immediate, or a pointer into
/// a frozen area — never a mortal process-heap object. Minted only by
/// [`FrozenBuilder`]'s constant methods and
/// [`crate::heap::ProcHeap::publish_frozen`], so the aggregate constructors can
/// require `FrozenConst` children. Storing a mortal value into a frozen
/// constant — a dangling pointer once the owning process heap dies — is then a
/// type error rather than a debug-only assertion.
#[derive(Clone, Debug)]
#[repr(transparent)]
pub struct FrozenConst(Value);

impl FrozenConst {
    /// Borrow the underlying value.
    pub fn value(&self) -> &Value {
        &self.0
    }

    /// Unwrap into the underlying value (e.g. to store in a constant pool).
    pub fn into_value(self) -> Value {
        self.0
    }

    /// Crate-internal mint for [`crate::heap::ProcHeap::publish_frozen`], whose
    /// roots are frozen by construction. Not public, so outside this crate a
    /// `FrozenConst` can only come from something that built into an area.
    pub(crate) fn from_publish(v: Value) -> FrozenConst {
        debug_assert!(
            !v.is_heap() || v.is_immortal(),
            "publish produced a mortal value"
        );
        FrozenConst(v)
    }
}

/// View `FrozenConst` children as the raw `Value` slice the object
/// constructors take.
fn frozen_values(items: &[FrozenConst]) -> &[Value] {
    // SAFETY: `FrozenConst` is `repr(transparent)` over `Value`.
    unsafe { std::slice::from_raw_parts(items.as_ptr().cast::<Value>(), items.len()) }
}

/// Intern table for string-aggregate constants, keyed by contents. An
/// `IndexMap`, not a `HashMap`, so lookups can probe with a borrowed
/// `&[&str]` via [`Equivalent`] and only build the owned key on a miss.
type StrAggregateMap = IndexMap<Box<[Box<str>]>, Value>;

/// Borrowed lookup key for [`StrAggregateMap`]. Must hash identically to the
/// stored `Box<[Box<str>]>` and compare by contents.
struct BorrowedStrs<'a>(&'a [&'a str]);

impl Hash for BorrowedStrs<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

impl Equivalent<Box<[Box<str>]>> for BorrowedStrs<'_> {
    fn equivalent(&self, key: &Box<[Box<str>]>) -> bool {
        key.iter().map(Box::as_ref).eq(self.0.iter().copied())
    }
}

/// Append handle to a [`FrozenArea`], and the construction door for every
/// program constant. Hydration threads one through the compiler as
/// `&mut FrozenBuilder`; `rc_publish_graph` takes one as its destination when
/// publishing a global. Several coexist over one area (see
/// [`FrozenArea::builder`]).
///
/// String contents are interned, so each distinct name/label gets one
/// canonical frozen allocation *per builder*. Per-builder, not per-area:
/// the same string frozen through two builders yields two allocations. That is
/// fine — interning is only a space optimization, and equality, hashing and
/// pattern matching all compare contents, never pointers.
pub struct FrozenBuilder {
    /// The frozen area these interned `Value`s point into. Field order is not
    /// load-bearing: an immortal `Value`'s `Clone`/`Drop` is pure bit math and
    /// never dereferences the object, so they may drop in any order relative
    /// to the area.
    area: Arc<FrozenArea>,
    /// Canonical frozen `Str` constant per distinct contents.
    strs: HashMap<Box<str>, Value>,
    /// Boxed-Int interning. A big int (outside ±2^47) is a frozen allocation,
    /// and the constant pool's dedup keys on `to_bits()` — a *pointer* for
    /// boxed values — so without this map every use pooled a fresh copy.
    ints: HashMap<i64, Value>,
    /// Canonical frozen all-string array constant per distinct contents.
    str_arrays: StrAggregateMap,
    /// Canonical frozen `Tuple`-of-`Str` label list per distinct contents.
    label_tuples: StrAggregateMap,
}

impl FrozenBuilder {
    /// Allocate `words` zero-initialized words and return a pointer to the
    /// first, 8-byte aligned and stable for the program lifetime. The caller
    /// must fully initialize the words before letting the pointer escape this
    /// thread (module docs, "Publication protocol").
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

// Program literals/constants are built exclusively through these methods,
// which is what makes `Program` `Send + Sync`: every constant `Value` is an
// immediate or points into the program's frozen area, never into a process
// heap.
impl FrozenBuilder {
    /// A frozen Int constant. Small ints are immediates with no backing
    /// allocation; the method exists so construction uniformly goes through
    /// the builder.
    pub fn int(&mut self, i: i64) -> FrozenConst {
        if let Some(v) = self.ints.get(&i) {
            return FrozenConst(v.clone());
        }
        let v = Value::int_in(self, i);
        // An immediate already dedups by bits in the pool.
        if !Value::fits_small_int(i) {
            self.ints.insert(i, v.clone());
        }
        FrozenConst(v)
    }

    /// A frozen Float constant (immediate; see [`FrozenBuilder::int`]).
    pub fn float(&mut self, f: f64) -> FrozenConst {
        FrozenConst(Value::float(f))
    }

    /// A frozen Bool constant (immediate; see [`FrozenBuilder::int`]).
    pub fn bool(&mut self, b: bool) -> FrozenConst {
        FrozenConst(Value::bool(b))
    }

    /// A frozen string constant: one canonical `Value` per distinct contents
    /// per builder. Enum/variant names and field labels all resolve here.
    pub fn str(&mut self, s: &str) -> FrozenConst {
        if let Some(v) = self.strs.get(s) {
            return FrozenConst(v.clone());
        }
        let v = Value::str_in(self, s);
        self.strs.insert(Box::from(s), v.clone());
        FrozenConst(v)
    }

    /// A frozen all-string array constant (enum field-label lists), interned
    /// as a unit.
    pub fn str_array(&mut self, items: &[&str]) -> FrozenConst {
        FrozenConst(self.intern_str_aggregate(items, |b| &mut b.str_arrays, Value::array_in))
    }

    /// A frozen tuple constant. Children carry frozen provenance by type, so a
    /// mortal heap element cannot be passed here.
    pub fn tuple(&mut self, items: Vec<FrozenConst>) -> FrozenConst {
        FrozenConst(Value::tuple_in(self, frozen_values(&items)))
    }

    /// A frozen binary constant of `bit_len` bits.
    pub fn binary_bits(&mut self, bytes: Vec<u8>, bit_len: u64) -> FrozenConst {
        FrozenConst(Value::binary_bits_in(self, bytes, bit_len))
    }

    /// A frozen enum constant. The hash must be computed exactly the way the
    /// VM computes it at construction, or equality breaks.
    pub fn enum_(
        &mut self,
        type_id: crate::TypeId,
        variant_idx: u16,
        enum_name: &str,
        variant_name: &str,
        field_labels: &[&str],
        payload: Vec<FrozenConst>,
    ) -> FrozenConst {
        let payload = frozen_values(&payload);
        let hash = enum_hash_with_payload(enum_name_prefix_hash(enum_name, variant_name), payload);
        let en = self.str(enum_name).into_value();
        let vn = self.str(variant_name).into_value();
        let labels_tuple = self.label_tuple(field_labels);
        FrozenConst(Value::enum_in(
            self,
            type_id,
            variant_idx,
            hash,
            en,
            vn,
            labels_tuple,
            payload,
        ))
    }

    /// The canonical frozen labels reference for enum objects: a `Tuple` of
    /// `Str`, the shape [`Value::enum_in`] requires for its `labels` argument.
    fn label_tuple(&mut self, labels: &[&str]) -> Value {
        self.intern_str_aggregate(labels, |b| &mut b.label_tuples, Value::tuple_in)
    }

    /// Shared interning loop for the string-aggregate constants. `map` selects
    /// the table by callback rather than borrowing it up front so `self` stays
    /// free for the element interning in between.
    fn intern_str_aggregate(
        &mut self,
        items: &[&str],
        map: fn(&mut Self) -> &mut StrAggregateMap,
        construct: fn(&mut Self, &[Value]) -> Value,
    ) -> Value {
        if let Some(v) = map(self).get(&BorrowedStrs(items)) {
            return v.clone();
        }
        let elems: Vec<Value> = items.iter().map(|s| self.str(s).into_value()).collect();
        let v = construct(self, &elems);
        let key: Box<[Box<str>]> = items.iter().map(|&s| Box::from(s)).collect();
        map(self).insert(key, v.clone());
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
        // Every earlier pointer still reads back its exact contents.
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
        assert!(area.contains(unsafe { p.as_ptr().add(3) }));
        // One past the used range: segment slack is not "frozen" yet.
        assert!(!area.contains(unsafe { p.as_ptr().add(4) }));
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

    /// The publication protocol in the shape the runtime uses for globals:
    /// writer fills an object, stores its pointer, bumps a version with
    /// `Release`; reader `Acquire`-loads the version and dereferences without
    /// locks.
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

    /// The frozen object address of a heap-backed constant: interning must hand
    /// out the *same allocation*, not just equal contents.
    fn addr(v: &Value) -> usize {
        v.object_addr().expect("constant should be heap-backed")
    }

    #[test]
    fn str_constants_intern_to_one_allocation() {
        let area = Arc::new(FrozenArea::new());
        let mut b = area.builder();
        let a1 = b.str("Some");
        let a2 = b.str("Some");
        let other = b.str("None");
        assert_eq!(a1.value().as_str(), Some("Some"));
        assert_eq!(
            addr(a1.value()),
            addr(a2.value()),
            "same contents, same allocation"
        );
        assert_ne!(addr(a1.value()), addr(other.value()));
        assert!(area.contains(addr(a1.value()) as *const u64));
        // A repeat allocates nothing.
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
        assert_eq!(
            addr(l1.value()),
            addr(l2.value()),
            "same contents, same array"
        );
        assert_ne!(addr(l1.value()), addr(l3.value()));
        assert!(area.contains(addr(l1.value()) as *const u64));
        let elem = l1.value().as_array().unwrap().get(0).unwrap();
        assert_eq!(elem.as_str(), Some("host"));
        assert_eq!(addr(&elem), addr(b.str("host").value()));
    }

    /// Enum constants carry interned names/labels: two constants of the same
    /// variant share the canonical string and label-tuple allocations.
    #[test]
    fn enum_names_and_labels_point_into_the_frozen_area() {
        let area = Arc::new(FrozenArea::new());
        let mut b = area.builder();
        let v1 = b.enum_(
            crate::TypeId(7),
            0,
            "Credentials",
            "Basic",
            &["user", "pass"],
            vec![],
        );
        let v2 = b.enum_(
            crate::TypeId(7),
            0,
            "Credentials",
            "Basic",
            &["user", "pass"],
            vec![],
        );
        assert_ne!(addr(v1.value()), addr(v2.value()), "distinct enum objects");
        assert!(area.contains(addr(v1.value()) as *const u64));
        let (e1, e2) = (v1.value().as_enum().unwrap(), v2.value().as_enum().unwrap());
        assert_eq!(e1.enum_name(), "Credentials");
        assert_eq!(e1.variant_name(), "Basic");
        assert_eq!(e1.field_labels()[1].as_str(), Some("pass"));
        assert_eq!(e1.hash(), e2.hash());
        assert_eq!(addr(&e1.enum_name_value()), addr(&e2.enum_name_value()));
        assert_eq!(
            addr(&e1.variant_name_value()),
            addr(&e2.variant_name_value())
        );
        assert_eq!(addr(&e1.labels_value()), addr(&e2.labels_value()));
        assert_eq!(
            addr(&e1.enum_name_value()),
            addr(b.str("Credentials").value())
        );
        assert!(area.contains(addr(&e1.labels_value()) as *const u64));
    }
}
