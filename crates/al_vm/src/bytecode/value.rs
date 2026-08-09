//! NaN-boxed runtime value over per-process arena heaps.
//!
//! `Value` is an 8-byte word. A non-quiet-NaN bit pattern is a plain `f64`;
//! `Value::float` clamps every non-finite input to `0.0`, so a real NaN never
//! enters the box and the whole qNaN space is free for tagging. A tagged value
//! sets the qNaN bits, discriminates on the top 16 bits, and carries a
//! sign-extended small int, a bool, a socket id, or a raw arena pointer in the
//! low 48 bits. Integers outside that range spill to an arena `BigInt`.
//!
//! # Arena object layout
//!
//! A mortal object is `[rc word][header word][payload words…]` and a `Value`
//! points at the *header*, so every header-relative offset is layout-stable.
//! Frozen (immortal) objects have no rc word.
//!
//! Header bits, low first:
//!
//! ```text
//! bit 0      header marker, always 1 (objects are 8-byte aligned, so a read
//!            of a freed or zeroed slot trips the accessors' debug guard)
//! bits 1-5   HeapTag
//! bit 6      off-heap: the payload owns an `Arc<[u8]>` backing (Binary)
//! bit 7      immortal: lives in the frozen area, never reference counted
//! bits 8-63  payload length in words
//! ```
//!
//! Payload layouts:
//!
//! - `BigInt`:    `[i64]`
//! - `Range`:     `[start][end]`
//! - `Str`:       `[byte_len][UTF-8 bytes inline, zero-padded to a word]`
//! - `Binary`:    `[arc_data_ptr][arc_len][bit_offset][bit_len]` — the bytes
//!   stay off-heap in an `Arc<[u8]>` shared across views/spawn/migration
//! - `Tuple`:     `[count][elements…]`
//! - `Enum`:      `[type_id | variant_idx<<32][hash][enum_name][variant_name][labels][count][payload…]`
//! - `Closure`:   `[func_idx][count][captures…]` — no name; printers resolve
//!   it through `program.functions[func_idx]`
//! - `Seq`:       `[len][shift][head][tree][tail]` — persistent-vector root
//! - `SeqLeaf`:   `[count][elements…]`, 1..=32 elements
//! - `SeqBranch`: `[count][shift][cumulative sizes × count][children × count]`
//!
//! # Reference counting
//!
//! `Value` is not `Copy`: cloning a heap value increments its count, dropping
//! decrements and frees at zero. Freeing walks an explicit work list, so a
//! deep list cannot overflow the native stack. The graph is acyclic by
//! construction (immutable values, capture by value, frame-based
//! self-reference), so there is no cycle collector. [`Arena::alloc_words`] is
//! infallible and non-moving, so a `Value` in a Rust local is never
//! invalidated by a later allocation.
//!
//! All `unsafe` in the value representation is confined to this file behind
//! typed accessors; `seq` and `hamt` go through the views and builders here.
//! The escape hatches for `heap::proc_heap` are `for_each_child_slot` (the one
//! layout table for tracing; its `&mut` face demands exclusive ownership, so
//! only the shared [`Value::for_each_child_ref`] may walk immortal objects)
//! and `binary_clone_backing`/`binary_drop_backing`.

#![allow(unsafe_code)]

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::fmt;
use std::marker::PhantomData;
use std::mem::ManuallyDrop;
use std::ptr::NonNull;
use std::sync::Arc;

use smallvec::SmallVec;

use crate::TypeId;
use crate::frozen::FrozenBuilder;
use crate::heap::ProcHeap;

use super::bits::{copy_bits, get_bit, read_byte, tail_mask};
pub use super::seq;
pub use super::seq::SeqIter;

/// Quiet-NaN signature. A word carrying it is a tagged value, never a float.
const QNAN: u64 = 0x7FF8_0000_0000_0000;
const SIGN: u64 = 0x8000_0000_0000_0000;
/// Low 48 bits: small-int value, bool, socket id, or heap pointer.
const PAYLOAD: u64 = 0x0000_FFFF_FFFF_FFFF;

/// Top-16-bit headers for non-float values, all including `QNAN`.
const HDR_MASK: u64 = 0xFFFF_0000_0000_0000;
const HDR_INT: u64 = QNAN | 0x0001_0000_0000_0000;
const HDR_BOOL: u64 = QNAN | 0x0002_0000_0000_0000;
const HDR_NIL: u64 = QNAN | 0x0003_0000_0000_0000;
const HDR_SOCKET: u64 = QNAN | 0x0004_0000_0000_0000;
/// The sign bit makes `(bits & (SIGN|QNAN)) == (SIGN|QNAN)` the heap test,
/// disjoint from the sign-clear immediate headers above.
const HDR_PTR: u64 = SIGN | QNAN;

/// Immortality marker in the `Value` word itself: bit 0 of the payload, free
/// because arena objects are 8-byte aligned. Reference counting can therefore
/// skip frozen values without reading frozen memory, so there is no drop-order
/// constraint against the frozen area. [`Value::from_object_ptr`] sets it once
/// from the object's header bit; it then rides along with the word.
const VALUE_IMMORTAL: u64 = 1;
/// Payload mask for heap pointers only: [`PAYLOAD`] minus the immortality
/// marker, recovering the aligned object address.
const PTR_PAYLOAD: u64 = PAYLOAD & !VALUE_IMMORTAL;

/// Inclusive bounds of the 48-bit signed small-int range.
const SMALL_INT_MIN: i64 = -(1i64 << 47);
const SMALL_INT_MAX: i64 = (1i64 << 47) - 1;

/// Socket immediate payload: low 32 bits = id, bit 32 = is_listener.
const SOCKET_LISTENER_BIT: u64 = 1 << 32;

/// Decode the socket immediate payload; caller must have checked `is_socket`.
#[inline]
fn decode_socket(bits: u64) -> SocketValue {
    SocketValue {
        id: (bits & 0xFFFF_FFFF) as u32 as i32,
        is_listener: bits & SOCKET_LISTENER_BIT != 0,
    }
}

/// Object type stored in the arena header word (bits 1-5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum HeapTag {
    BigInt = 0,
    Range = 1,
    Str = 2,
    Binary = 3,
    Tuple = 4,
    Enum = 5,
    Closure = 6,
    /// Persistent-vector root (`ValueView::Array`).
    Seq = 7,
    /// Vector leaf node — interior to a `Seq`, never observed via `kind()`.
    SeqLeaf = 8,
    /// Vector branch node — interior to a `Seq`, never observed via `kind()`.
    SeqBranch = 9,
    /// A `Map(k, v)`. Payload word 0 is a [`MapBacking`] discriminant; the
    /// rest of the layout is backing-specific.
    Map = 10,
    /// HAMT branch — `[bitmap, child…]`, one child per set bit. Interior to a
    /// `Map`, never observed via `kind()`. See [`super::hamt`].
    HamtBranch = 11,
    /// HAMT leaf — `[key, value]`. Interior to a `Map`.
    HamtEntry = 12,
    /// HAMT collision bucket — `[hash, count, key, value, …]` for distinct
    /// keys sharing one 64-bit hash. Interior to a `Map`.
    HamtCollision = 13,
}

/// How a [`HeapTag::Map`] sources its entries. The set is open: a new backing
/// gets a new discriminant, and the observable type stays `Map(k, v)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u64)]
pub enum MapBacking {
    /// Live view of the host process environment, typed `Map(String, String)`.
    /// Reads go straight to `std::env`; the object holds no `Value` words.
    Env = 0,
    /// A persistent hash array mapped trie ([`super::hamt`]). The map object
    /// carries `[backing, size, root]`.
    Hamt = 1,
}

/// Decode a [`MapBacking`] discriminant word. Aborts on an unknown one.
fn map_backing(word: u64) -> MapBacking {
    match word {
        0 => MapBacking::Env,
        1 => MapBacking::Hamt,
        _ => view_mismatch("map-backing"),
    }
}

const HEADER_BIT: u64 = 1;
const HEADER_TAG_SHIFT: u32 = 1;
const HEADER_TAG_MASK: u64 = 0x1F << HEADER_TAG_SHIFT;
const HEADER_OFF_HEAP_BIT: u64 = 1 << 6;
/// Immortal (frozen): reference counting must never touch this object. Set at
/// allocation time via [`Arena::marks_immortal`].
const HEADER_IMMORTAL_BIT: u64 = 1 << 7;
const HEADER_LEN_SHIFT: u32 = 8;

/// Pack an object header word.
#[inline]
fn pack_header(tag: HeapTag, payload_words: usize, off_heap: bool) -> u64 {
    HEADER_BIT
        | ((tag as u64) << HEADER_TAG_SHIFT)
        | if off_heap { HEADER_OFF_HEAP_BIT } else { 0 }
        | ((payload_words as u64) << HEADER_LEN_SHIFT)
}

/// Whether a word is a live object header. A debug guard: reading a freed or
/// uninitialized slot trips it.
#[inline]
fn header_marks_object(word: u64) -> bool {
    word & HEADER_BIT != 0
}

#[inline]
fn header_tag(word: u64) -> HeapTag {
    debug_assert!(header_marks_object(word));
    match (word & HEADER_TAG_MASK) >> HEADER_TAG_SHIFT {
        0 => HeapTag::BigInt,
        1 => HeapTag::Range,
        2 => HeapTag::Str,
        3 => HeapTag::Binary,
        4 => HeapTag::Tuple,
        5 => HeapTag::Enum,
        6 => HeapTag::Closure,
        7 => HeapTag::Seq,
        8 => HeapTag::SeqLeaf,
        9 => HeapTag::SeqBranch,
        10 => HeapTag::Map,
        11 => HeapTag::HamtBranch,
        12 => HeapTag::HamtEntry,
        13 => HeapTag::HamtCollision,
        _ => view_mismatch("heap-tag"),
    }
}

/// Payload length in words encoded in a header.
#[inline]
fn header_payload_words(word: u64) -> usize {
    debug_assert!(header_marks_object(word));
    (word >> HEADER_LEN_SHIFT) as usize
}

/// Total object size (header + payload) in words.
#[inline]
pub(crate) fn header_total_words(word: u64) -> usize {
    1 + header_payload_words(word)
}

/// Whether the object owns an `Arc` and sits on its space's off-heap list.
#[inline]
pub(crate) fn header_has_off_heap_link(word: u64) -> bool {
    debug_assert!(header_marks_object(word));
    word & HEADER_OFF_HEAP_BIT != 0
}

/// Whether the object is immortal. See [`HEADER_IMMORTAL_BIT`].
#[inline]
fn header_is_immortal(word: u64) -> bool {
    debug_assert!(header_marks_object(word));
    word & HEADER_IMMORTAL_BIT != 0
}

/// Mark an object's header immortal, for a mortal graph copied into the
/// frozen area.
///
/// # Safety
/// `obj` must point at a live, non-forwarded object header.
#[inline]
pub(crate) unsafe fn mark_immortal(obj: *mut u64) {
    unsafe { *obj |= HEADER_IMMORTAL_BIT };
}

/// Allocation interface the value constructors build through: a process heap
/// or the frozen builder. `alloc_words` is infallible by contract and never
/// moves existing objects.
pub trait Arena {
    /// Allocate `words` words (header + payload), 8-byte aligned, at an
    /// address stable for the object's lifetime.
    fn alloc_words(&mut self, words: usize) -> NonNull<u64>;

    /// Whether objects from this arena are born immortal.
    fn marks_immortal(&self) -> bool {
        false
    }
}

impl Arena for ProcHeap {
    #[inline]
    fn alloc_words(&mut self, words: usize) -> NonNull<u64> {
        self.alloc_object(words)
    }
}

impl Arena for FrozenBuilder {
    #[inline]
    fn alloc_words(&mut self, words: usize) -> NonNull<u64> {
        self.alloc(words)
    }

    /// Frozen objects are never reference counted: mark them immortal at birth.
    #[inline]
    fn marks_immortal(&self) -> bool {
        true
    }
}

/// Allocate an object and write its header; the payload starts one word on.
#[inline]
fn alloc_obj<A: Arena + ?Sized>(
    a: &mut A,
    tag: HeapTag,
    payload_words: usize,
    off_heap: bool,
) -> NonNull<u64> {
    let obj = a.alloc_words(1 + payload_words);
    let mut header = pack_header(tag, payload_words, off_heap);
    if a.marks_immortal() {
        header |= HEADER_IMMORTAL_BIT;
    }
    // SAFETY: freshly allocated, in bounds.
    unsafe { obj.as_ptr().write(header) };
    obj
}

/// A Perceus reuse address: a uniquely-owned mortal cell (rc==1) a following
/// constructor will overwrite in place, or `None` to allocate fresh. Opaque,
/// so only [`Value::into_reuse_addr`] can mint a `Some` — which is what makes
/// the `*_reuse_in` constructors safe to call from safe code.
///
/// Holds the cell's rc==1 count, and `Drop` frees it, so a token that never
/// reaches a constructor releases its allocation instead of leaking.
#[derive(Debug)]
pub struct ReuseAddr(Option<NonNull<u64>>);

impl ReuseAddr {
    /// The fresh-allocate fallback (no cell to reuse).
    #[inline(always)]
    pub const fn none() -> ReuseAddr {
        ReuseAddr(None)
    }
}

impl Drop for ReuseAddr {
    fn drop(&mut self) {
        if let Some(p) = self.0.take() {
            // SAFETY: a `Some` address names a live rc==1 cell that
            // `hollow_for_reuse` already stripped of children, so the free is
            // the whole release.
            unsafe { free_object(p.as_ptr()) };
        }
    }
}

/// Allocate a fresh object, or — Perceus reuse — overwrite `reuse` in place.
///
/// A reused cell arrives at rc==1 and keeps that count, so the caller writes
/// the payload exactly as for a fresh `alloc_obj`. Its old children need no
/// release: [`Value::hollow_for_reuse`] already released them and wrote
/// immediates into every child slot, so a second walk would only visit
/// sentinels. Debug builds assert that.
#[inline]
fn reuse_or_alloc<A: Arena + ?Sized>(
    a: &mut A,
    mut reuse: ReuseAddr,
    tag: HeapTag,
    payload_words: usize,
) -> NonNull<u64> {
    let Some(obj) = reuse.0.take() else {
        return alloc_obj(a, tag, payload_words, false);
    };
    // SAFETY: `obj` is a live mortal header at rc==1 whose children are
    // already released. The compiler pairs a reuse only with a same-shape
    // constructor; the size check below keeps a mispairing memory-safe in
    // release by falling back to a fresh allocation.
    unsafe {
        debug_assert!(!a.marks_immortal(), "Perceus reuse into a frozen arena");
        if header_total_words(*obj.as_ptr()) != 1 + payload_words {
            debug_assert!(false, "Perceus reuse shape mismatch");
            // Children are already hollowed to immediates, so the cell frees
            // without a walk.
            free_object(obj.as_ptr());
            return alloc_obj(a, tag, payload_words, false);
        }
        debug_assert_eq!(*rc_slot(obj.as_ptr()), 1, "Perceus reuse of a shared cell");
        #[cfg(debug_assertions)]
        for_each_child(obj.as_ptr(), &mut |child: &mut Value| {
            debug_assert!(
                !child.is_heap() || child.is_immortal(),
                "Perceus reuse of a cell holding a live mortal child"
            );
        });
        obj.as_ptr().write(pack_header(tag, payload_words, false));
    }
    obj
}

/// SAFETY for all of these: `obj` must point at a live, non-forwarded arena
/// object of the expected tag, and indices must stay within the payload length
/// its header declares.
#[inline]
unsafe fn payload_word(obj: *const u64, i: usize) -> u64 {
    unsafe { *obj.add(1 + i) }
}

#[inline]
unsafe fn payload_value(obj: *const u64, i: usize) -> Value {
    // Returns an owned (counted) reference to the child, so callers can drop it.
    unsafe { owned_from_bits(payload_word(obj, i)) }
}

/// Borrow `n` payload words at `at` as a `Value` slice. The lifetime is
/// unbounded; callers constrain it to the `Value` that produced `obj`.
#[inline]
unsafe fn payload_values<'a>(obj: *const u64, at: usize, n: usize) -> &'a [Value] {
    // SAFETY: `Value` is `repr(transparent)` over `u64`.
    unsafe { std::slice::from_raw_parts(obj.add(1 + at) as *const Value, n) }
}

/// The UTF-8 contents of a `Str`. Unbounded lifetime, as [`payload_values`].
#[inline]
unsafe fn str_contents<'a>(obj: *const u64) -> &'a str {
    unsafe {
        debug_assert_eq!(header_tag(*obj), HeapTag::Str);
        let len = payload_word(obj, 0) as usize;
        let bytes = std::slice::from_raw_parts(obj.add(2) as *const u8, len);
        std::str::from_utf8_unchecked(bytes)
    }
}

// Safe windows over the three `Seq` object layouts, so the `seq` module needs
// no unsafe code. They are sound because heap values point at live objects and
// because the count word of every `SeqLeaf`/`SeqBranch` agrees with its header
// — only the builders below write those words, and the GC copies objects
// verbatim. That is what keeps the slices in bounds with no per-access clamp.

/// Release-mode backstop for a typed view on the wrong tag, or a decoder
/// hitting a corrupt word. A VM bug or heap corruption, never user input.
#[cold]
#[inline(never)]
pub(crate) fn view_mismatch(kind: &'static str) -> ! {
    eprintln!("al: internal error: {kind} view on wrong heap tag");
    std::process::abort()
}

/// Every child stored into a frozen object must be an immediate or immortal:
/// A compiler-proven invariant failed at runtime. Aborting beats silently
/// substituting a wrong value: the program answer would be garbage either way,
/// and the abort names the bug. Cold by construction — the check guarding each
/// call is on the proven-correct path.
#[cold]
#[inline(never)]
pub(crate) fn proof_violation(what: &str) -> ! {
    eprintln!("al_vm: internal invariant violated: {what} (compiler bug)");
    std::process::abort()
}

#[inline(never)]
fn assert_frozen_children<'a>(children: impl IntoIterator<Item = &'a Value>) {
    for child in children {
        if child.is_heap() && !child.is_immortal() {
            // Release too: a frozen pointer to a mortal value dangles the
            // moment its owning process dies, and the freeze path is cold.
            proof_violation("mortal value frozen into a constant");
        }
    }
}

/// Decoded `Seq` root: payload `[len | shift | head | tree | tail]`.
pub(crate) struct SeqRootRef {
    pub(crate) len: usize,
    pub(crate) shift: usize,
    pub(crate) head: Value,
    pub(crate) tree: Value,
    pub(crate) tail: Value,
}

impl SeqRootRef {
    #[inline(always)]
    pub(crate) fn new(root: &Value) -> SeqRootRef {
        if !root.is_heap() {
            view_mismatch("seq");
        }
        let obj = root.heap_obj();
        // SAFETY: heap values point at live, non-forwarded arena objects.
        let header = unsafe { *obj };
        if header_tag(header) != HeapTag::Seq || header_payload_words(header) < 5 {
            view_mismatch("seq");
        }
        // SAFETY: tag checked above; the header declares at least 5 payload
        // words.
        unsafe {
            SeqRootRef {
                len: payload_word(obj, 0) as usize,
                shift: payload_word(obj, 1) as usize,
                head: payload_value(obj, 2),
                tree: payload_value(obj, 3),
                tail: payload_value(obj, 4),
            }
        }
    }
}

/// Decoded `SeqLeaf` / `SeqBranch` node, borrowed from the input `Value`.
pub(crate) enum SeqNodeRef<'a> {
    Leaf(&'a [Value]),
    Branch {
        shift: usize,
        sizes: &'a [u64],
        children: &'a [Value],
    },
}

impl<'a> SeqNodeRef<'a> {
    #[inline(always)]
    pub(crate) fn of(node: &'a Value) -> SeqNodeRef<'a> {
        if !node.is_heap() {
            view_mismatch("seq");
        }
        let obj = node.heap_obj();
        // SAFETY: heap values point at live, non-forwarded arena objects.
        let header = unsafe { *obj };
        match header_tag(header) {
            HeapTag::SeqLeaf => {
                // SAFETY: tag checked; the count word bounds the slice.
                unsafe {
                    let n = payload_word(obj, 0) as usize;
                    debug_assert_eq!(1 + n, header_payload_words(header));
                    SeqNodeRef::Leaf(payload_values(obj, 1, n))
                }
            }
            HeapTag::SeqBranch => {
                // Payload: [count | shift | sizes[count] | children[count]].
                // SAFETY: tag checked; the count word bounds both slices.
                unsafe {
                    let n = payload_word(obj, 0) as usize;
                    debug_assert_eq!(2 + 2 * n, header_payload_words(header));
                    let shift = payload_word(obj, 1) as usize;
                    let sizes = std::slice::from_raw_parts(obj.add(3), n);
                    let children = payload_values(obj, 2 + n, n);
                    SeqNodeRef::Branch {
                        shift,
                        sizes,
                        children,
                    }
                }
            }
            _ => view_mismatch("seq"),
        }
    }
}

/// Allocate a `Seq` root over the given parts.
#[inline]
pub(crate) fn seq_root_in<A: Arena + ?Sized>(
    a: &mut A,
    len: usize,
    shift: usize,
    head: Value,
    tree: Value,
    tail: Value,
) -> Value {
    if a.marks_immortal() {
        assert_frozen_children([&head, &tree, &tail]);
    }
    let obj = alloc_obj(a, HeapTag::Seq, 5, false);
    // SAFETY: freshly allocated 5-word payload; header written by `alloc_obj`.
    unsafe {
        let p = obj.as_ptr().add(1);
        p.write(len as u64);
        p.add(1).write(shift as u64);
        move_child(p.add(2), head);
        move_child(p.add(3), tree);
        move_child(p.add(4), tail);
        Value::from_object_ptr(obj)
    }
}

/// Allocate a `SeqLeaf` holding `items`.
#[inline]
pub(crate) fn seq_leaf_in<A: Arena + ?Sized>(a: &mut A, items: &[Value]) -> Value {
    if a.marks_immortal() {
        assert_frozen_children(items);
    }
    let obj = alloc_obj(a, HeapTag::SeqLeaf, 1 + items.len(), false);
    // SAFETY: freshly allocated payload of exactly 1 + len words; header
    // written by `alloc_obj`.
    unsafe {
        let p = obj.as_ptr().add(1);
        p.write(items.len() as u64);
        for (i, v) in items.iter().enumerate() {
            store_child(p.add(1 + i), v);
        }
        Value::from_object_ptr(obj)
    }
}

/// Element count under a seq node: leaf count, or a branch's last cumulative
/// size. Reads are unclamped — the builder-written count word keeps them in
/// bounds even for a degenerate empty node.
#[inline]
fn seq_node_total(node: &Value) -> u64 {
    if !node.is_heap() {
        view_mismatch("seq");
    }
    let obj = node.heap_obj();
    // SAFETY: live arena object; both arms read within the payload length the
    // count word implies.
    unsafe {
        let header = *obj;
        match header_tag(header) {
            HeapTag::SeqLeaf => payload_word(obj, 0),
            HeapTag::SeqBranch => {
                let n = payload_word(obj, 0) as usize;
                debug_assert_eq!(2 + 2 * n, header_payload_words(header));
                payload_word(obj, 1 + n)
            }
            _ => view_mismatch("seq"),
        }
    }
}

/// Allocate a `SeqBranch` at height `shift` over `children` (each a leaf or
/// branch), computing the cumulative size table.
#[inline]
pub(crate) fn seq_branch_in<A: Arena + ?Sized>(
    a: &mut A,
    shift: usize,
    children: &[Value],
) -> Value {
    if a.marks_immortal() {
        assert_frozen_children(children);
    }
    let n = children.len();
    let obj = alloc_obj(a, HeapTag::SeqBranch, 2 + 2 * n, false);
    // SAFETY: freshly allocated payload of exactly 2 + 2n words; header
    // written by `alloc_obj`.
    unsafe {
        let p = obj.as_ptr().add(1);
        p.write(n as u64);
        p.add(1).write(shift as u64);
        let mut total = 0u64;
        for (i, c) in children.iter().enumerate() {
            total += seq_node_total(c);
            p.add(2 + i).write(total);
            store_child(p.add(2 + n + i), c);
        }
        Value::from_object_ptr(obj)
    }
}

// The arena layer for [`super::hamt`]. Three node tags, all interior to a
// `Map` and never observed via `kind()`:
//
// - `HamtEntry`     `[key, value]`
// - `HamtCollision` `[hash, count, key, value, …]` (count ≥ 2 distinct keys)
// - `HamtBranch`    `[bitmap, child…]` (one child per set bit of `bitmap`)

/// Decoded HAMT interior node.
pub(crate) enum HamtNodeRef<'a> {
    Entry {
        key: Value,
        value: Value,
    },
    Collision {
        hash: u64,
        /// Interleaved `key, value, …`; length is `2 * count`.
        pairs: &'a [Value],
    },
    Branch {
        bitmap: u32,
        children: &'a [Value],
    },
}

impl<'a> HamtNodeRef<'a> {
    #[inline]
    pub(crate) fn of(node: &'a Value) -> HamtNodeRef<'a> {
        if !node.is_heap() {
            view_mismatch("hamt");
        }
        let obj = node.heap_obj();
        // SAFETY: live arena object; each arm reads within its builder's
        // payload length.
        unsafe {
            let header = *obj;
            match header_tag(header) {
                HeapTag::HamtEntry => HamtNodeRef::Entry {
                    key: payload_value(obj, 0),
                    value: payload_value(obj, 1),
                },
                HeapTag::HamtCollision => {
                    let count = payload_word(obj, 1) as usize;
                    debug_assert_eq!(2 + 2 * count, header_payload_words(header));
                    HamtNodeRef::Collision {
                        hash: payload_word(obj, 0),
                        pairs: payload_values(obj, 2, 2 * count),
                    }
                }
                HeapTag::HamtBranch => {
                    let bitmap = payload_word(obj, 0) as u32;
                    let n = bitmap.count_ones() as usize;
                    debug_assert_eq!(1 + n, header_payload_words(header));
                    HamtNodeRef::Branch {
                        bitmap,
                        children: payload_values(obj, 1, n),
                    }
                }
                _ => view_mismatch("hamt"),
            }
        }
    }
}

/// Allocate a `HamtEntry` `[key, value]`.
#[inline]
pub(crate) fn hamt_entry_in<A: Arena + ?Sized>(a: &mut A, key: Value, value: Value) -> Value {
    if a.marks_immortal() {
        assert_frozen_children([&key, &value]);
    }
    let obj = alloc_obj(a, HeapTag::HamtEntry, 2, false);
    // SAFETY: freshly allocated 2-word payload; header written by `alloc_obj`.
    unsafe {
        let p = obj.as_ptr().add(1);
        move_child(p, key);
        move_child(p.add(1), value);
        Value::from_object_ptr(obj)
    }
}

/// Allocate a `HamtCollision` over `pairs` (interleaved `key, value, …`, so
/// `pairs.len()` is even), all sharing `hash`.
#[inline]
pub(crate) fn hamt_collision_in<A: Arena + ?Sized>(a: &mut A, hash: u64, pairs: &[Value]) -> Value {
    debug_assert!(pairs.len() >= 4 && pairs.len().is_multiple_of(2));
    if a.marks_immortal() {
        assert_frozen_children(pairs);
    }
    let count = pairs.len() / 2;
    let obj = alloc_obj(a, HeapTag::HamtCollision, 2 + pairs.len(), false);
    // SAFETY: freshly allocated payload of exactly 2 + 2*count words; header
    // written by `alloc_obj`.
    unsafe {
        let p = obj.as_ptr().add(1);
        p.write(hash);
        p.add(1).write(count as u64);
        for (i, v) in pairs.iter().enumerate() {
            store_child(p.add(2 + i), v);
        }
        Value::from_object_ptr(obj)
    }
}

/// Allocate a `HamtBranch` whose occupied slots are `bitmap` and whose
/// `children` are in ascending slot order (`children.len() == bitmap.count_ones()`).
#[inline]
pub(crate) fn hamt_branch_in<A: Arena + ?Sized>(
    a: &mut A,
    bitmap: u32,
    children: &[Value],
) -> Value {
    debug_assert_eq!(bitmap.count_ones() as usize, children.len());
    if a.marks_immortal() {
        assert_frozen_children(children);
    }
    let obj = alloc_obj(a, HeapTag::HamtBranch, 1 + children.len(), false);
    // SAFETY: freshly allocated payload of exactly 1 + n words; header written
    // by `alloc_obj`.
    unsafe {
        let p = obj.as_ptr().add(1);
        p.write(bitmap as u64);
        for (i, c) in children.iter().enumerate() {
            store_child(p.add(1 + i), c);
        }
        Value::from_object_ptr(obj)
    }
}

/// Decoded `Map` root with the `Hamt` backing: `[backing, size, root]`.
pub(crate) struct HamtMapRef {
    pub(crate) size: usize,
    /// The top trie node, or `Nil` when the map is empty.
    pub(crate) root: Value,
}

impl HamtMapRef {
    /// The one decoder for the `[backing, size, root]` layout. The backing
    /// discriminant is release-checked: an `Env` map holds no such words.
    ///
    /// # Safety
    /// `obj` must point at a live `Map` object header.
    #[inline]
    unsafe fn from_obj(obj: *const u64) -> HamtMapRef {
        // SAFETY: word 0 is the backing discriminant for every Map layout.
        unsafe {
            if map_backing(payload_word(obj, 0)) != MapBacking::Hamt {
                view_mismatch("hamt");
            }
            HamtMapRef {
                size: payload_word(obj, 1) as usize,
                root: payload_value(obj, 2),
            }
        }
    }

    #[inline]
    pub(crate) fn of(map: &Value) -> HamtMapRef {
        if !map.is_heap() {
            view_mismatch("hamt");
        }
        let obj = map.heap_obj();
        // SAFETY: `obj` is a live object header; the tag check proves Map.
        unsafe {
            if header_tag(*obj) != HeapTag::Map {
                view_mismatch("hamt");
            }
            HamtMapRef::from_obj(obj)
        }
    }
}

/// Allocate a `Map` with the `Hamt` backing: `[backing, size, root]`. `root`
/// is `Nil` for an empty map, else a trie node.
#[inline]
pub(crate) fn hamt_map_in<A: Arena + ?Sized>(a: &mut A, size: usize, root: Value) -> Value {
    if a.marks_immortal() {
        assert_frozen_children([&root]);
    }
    let obj = alloc_obj(a, HeapTag::Map, 3, false);
    // SAFETY: freshly allocated 3-word payload; header written by `alloc_obj`.
    unsafe {
        let p = obj.as_ptr().add(1);
        p.write(MapBacking::Hamt as u64);
        p.add(1).write(size as u64);
        move_child(p.add(2), root);
        Value::from_object_ptr(obj)
    }
}

/// The universal NaN-boxed value: one machine word. Not `Copy` — a mortal heap
/// value owns a reference count. Immediates and frozen values carry no count,
/// so their `Clone`/`Drop` are no-ops.
#[repr(transparent)]
pub struct Value(u64);

impl Clone for Value {
    #[inline]
    fn clone(&self) -> Value {
        if self.is_heap() && !self.is_immortal() {
            // SAFETY: a mortal heap value points at a live object with a count.
            unsafe { rc_increment(self.heap_obj()) };
        }
        Value(self.0)
    }
}

impl Drop for Value {
    #[inline]
    fn drop(&mut self) {
        // Raw bits, so this never builds another droppable `Value`.
        release_bits(self.0);
    }
}

const _: () = assert!(std::mem::size_of::<Value>() == 8);

/// Borrowed typed view for many-armed matches. `Int` collapses small ints and
/// arena `BigInt`s. Interior nodes are unreachable here — user values only
/// ever point at roots.
pub enum ValueView<'a> {
    Int(i64),
    Float(f64),
    Bool(bool),
    Nil,
    Socket(SocketValue),
    Str(&'a str),
    Array(SeqRef<'a>),
    Range(i64, i64),
    Binary(BinaryRef<'a>),
    Tuple(&'a [Value]),
    Closure(ClosureRef<'a>),
    Enum(EnumRef<'a>),
    Map(MapRef<'a>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SocketValue {
    pub id: i32,
    pub is_listener: bool,
}

impl Value {
    /// The raw NaN-box bits, by borrow. Pairs with `from_bits`.
    #[inline(always)]
    pub fn to_bits(&self) -> u64 {
        self.0
    }

    /// Reconstitute a value from raw bits without taking a reference: the
    /// result is an un-counted alias.
    ///
    /// # Safety
    /// `bits` must be an immediate, or the address of a live object header
    /// whose count the caller is transferring. Storing the result or letting
    /// it drop mis-counts unless the caller balances it. The only sound bit
    /// sources are `to_bits` and `from_object_ptr`.
    #[inline(always)]
    pub unsafe fn from_bits(bits: u64) -> Value {
        Value(bits)
    }

    /// Box a pointer to an object header as a heap value. The only place
    /// immortality is read from memory; afterwards it lives in the value word
    /// (see [`VALUE_IMMORTAL`]).
    ///
    /// # Safety
    /// `obj` must point at a live header whose header word is written. The
    /// result takes ownership of one reference count.
    #[inline]
    pub unsafe fn from_object_ptr(obj: NonNull<u64>) -> Value {
        let addr = obj.as_ptr() as usize as u64;
        debug_assert!(addr & !PAYLOAD == 0, "arena pointer exceeds 48 bits");
        debug_assert!(addr.is_multiple_of(8), "unaligned arena pointer");
        // SAFETY: `obj` is a freshly written, live object header.
        let marker = if header_is_immortal(unsafe { *obj.as_ptr() }) {
            VALUE_IMMORTAL
        } else {
            0
        };
        Value(HDR_PTR | addr | marker)
    }

    /// The object header address of a heap value, marker masked off.
    #[inline(always)]
    pub fn object_addr(&self) -> Option<usize> {
        if self.is_heap() {
            Some((self.0 & PTR_PAYLOAD) as usize)
        } else {
            None
        }
    }

    /// Visit every immediate child `Value` of `self` — the read-only face of
    /// [`for_each_child_slot`]. Interior nodes that [`Value::kind`] hides
    /// behind a root are visited too, so recursing here reaches every `Value`
    /// in the graph. The callback gets a shared reference, so this is the one
    /// walk that is sound on frozen objects shared across threads.
    #[inline]
    pub fn for_each_child_ref(&self, mut f: impl FnMut(&Value)) {
        if !self.is_heap() {
            return;
        }
        // SAFETY: live object header, and the slots are only ever reborrowed
        // as `&Value`, so this cannot conflict with another shared borrow.
        unsafe { for_each_child_slot(self.heap_obj(), &mut |p: *mut Value| f(&*p)) }
    }

    /// The heap object header this value points at. Public for the native
    /// backend's tests, which pair it with [`rc_slot`].
    #[inline(always)]
    pub fn heap_obj(&self) -> *const u64 {
        debug_assert!(self.is_heap());
        (self.0 & PTR_PAYLOAD) as usize as *const u64
    }

    /// The object tag of a heap-backed value.
    #[inline]
    pub fn heap_tag(&self) -> Option<HeapTag> {
        if self.is_heap() {
            // SAFETY: heap values point at live arena objects.
            Some(header_tag(unsafe { *self.heap_obj() }))
        } else {
            None
        }
    }

    #[inline(always)]
    pub(crate) fn is_tag(&self, tag: HeapTag) -> bool {
        self.heap_tag() == Some(tag)
    }

    /// Whether this is an immortal (frozen) heap object. Immediates are
    /// `false`. A pure bit test that never dereferences the object, so
    /// `Clone`/`Drop` of a frozen value does not need the area still mapped.
    #[inline(always)]
    pub fn is_immortal(&self) -> bool {
        self.is_heap() && (self.0 & VALUE_IMMORTAL != 0)
    }

    /// Whether this heap value's refcount is exactly 1, so a Perceus reuse may
    /// overwrite it in place. Immediates and frozen values are `false`.
    #[inline(always)]
    pub fn is_unique(&self) -> bool {
        if !self.is_heap() || self.is_immortal() {
            return false;
        }
        // SAFETY: a mortal heap object has an initialized refcount slot.
        unsafe { *rc_slot(self.heap_obj()) == 1 }
    }

    /// Perceus `Op::Drop` on a uniquely-owned cell: release every child and
    /// write a sentinel into its slot, leaving the allocation hollow — header
    /// intact, rc still 1. A same-shape constructor then overwrites it via
    /// [`reuse_or_alloc`]. Hollowing here rather than at the constructor is
    /// what makes reuse propagate down a recursive chain: the callee sees its
    /// argument at rc==1 only because the parent released its ref first.
    /// No-op on shared, immortal, and immediate values.
    pub fn hollow_for_reuse(&mut self) {
        if !self.is_unique() {
            return;
        }
        // SAFETY: rc==1 makes this the sole owner.
        unsafe { hollow_children(self.heap_obj() as *mut u64) }
    }

    /// Consume a Perceus reuse token pushed by `Op::Reuse`: either an rc==1
    /// mortal heap value, or `nil` to allocate fresh. The reference count
    /// transfers to the returned [`ReuseAddr`], and from there to whatever
    /// `*_reuse_in` constructor consumes it.
    #[inline(always)]
    pub fn into_reuse_addr(self) -> ReuseAddr {
        if !self.is_heap() || self.is_immortal() {
            return ReuseAddr::none();
        }
        debug_assert!(self.is_unique(), "reuse token must be uniquely owned");
        let addr = NonNull::new(self.heap_obj() as *mut u64);
        // The count now lives in the raw address.
        std::mem::forget(self);
        ReuseAddr(addr)
    }

    #[inline(always)]
    pub fn is_float(&self) -> bool {
        (self.0 & QNAN) != QNAN
    }
    #[inline(always)]
    fn is_small_int(&self) -> bool {
        (self.0 & HDR_MASK) == HDR_INT
    }
    /// Sign-extend the low 48 bits; only meaningful when `is_small_int()`.
    #[inline(always)]
    fn small_int_value(&self) -> i64 {
        ((self.0 & PAYLOAD) as i64) << 16 >> 16
    }
    #[inline(always)]
    pub fn is_bool(&self) -> bool {
        (self.0 & HDR_MASK) == HDR_BOOL
    }
    #[inline(always)]
    pub fn is_nil(&self) -> bool {
        self.0 == HDR_NIL
    }
    #[inline(always)]
    pub fn is_socket(&self) -> bool {
        (self.0 & HDR_MASK) == HDR_SOCKET
    }
    #[inline(always)]
    pub fn is_heap(&self) -> bool {
        (self.0 & (SIGN | QNAN)) == (SIGN | QNAN)
    }
    #[inline(always)]
    pub fn is_int(&self) -> bool {
        self.is_small_int() || self.is_tag(HeapTag::BigInt)
    }
    /// Whether `i` fits the 48-bit immediate integer range (no arena spill).
    #[inline(always)]
    pub fn fits_small_int(i: i64) -> bool {
        (SMALL_INT_MIN..=SMALL_INT_MAX).contains(&i)
    }

    /// An integer known to be in the 48-bit immediate range: lengths, counts,
    /// codepoints. Arithmetic results that may exceed it must use `int_in`.
    #[inline(always)]
    pub fn small_int(i: i64) -> Value {
        debug_assert!(
            Value::fits_small_int(i),
            "small_int out of range: {i} (use int_in)"
        );
        Value(HDR_INT | (i as u64 & PAYLOAD))
    }
    #[inline(always)]
    pub fn float(f: f64) -> Value {
        // AL has no NaN/Inf, and a real NaN would collide with the tag space.
        let f = if f.is_finite() { f } else { 0.0 };
        Value(f.to_bits())
    }
    #[inline(always)]
    pub fn bool(b: bool) -> Value {
        Value(HDR_BOOL | b as u64)
    }
    #[inline(always)]
    pub fn nil() -> Value {
        Value(HDR_NIL)
    }
    #[inline]
    pub fn socket(s: SocketValue) -> Value {
        let listener = if s.is_listener {
            SOCKET_LISTENER_BIT
        } else {
            0
        };
        Value(HDR_SOCKET | listener | s.id as u32 as u64)
    }

    // These allocate but never collect, so process-heap callers must have
    // ensured capacity. Worst-case sizes are documented for `ensure`.

    /// Full-range integer; spills to a 2-word `BigInt` box outside the
    /// immediate range. Worst-case allocation: 2 words.
    #[inline]
    pub fn int_in<A: Arena + ?Sized>(a: &mut A, i: i64) -> Value {
        if Value::fits_small_int(i) {
            Value::small_int(i)
        } else {
            let obj = alloc_obj(a, HeapTag::BigInt, 1, false);
            // SAFETY: freshly allocated 1-word payload; header written by
            // `alloc_obj`.
            unsafe {
                obj.as_ptr().add(1).write(i as u64);
                Value::from_object_ptr(obj)
            }
        }
    }

    /// Allocation: `2 + len.div_ceil(8)` words.
    pub fn str_in<A: Arena + ?Sized>(a: &mut A, s: &str) -> Value {
        let blen = s.len();
        let payload = 1 + blen.div_ceil(8);
        let obj = alloc_obj(a, HeapTag::Str, payload, false);
        // SAFETY: payload sized for the length word plus the padded bytes;
        // header written by `alloc_obj`.
        unsafe {
            let p = obj.as_ptr().add(1);
            p.write(blen as u64);
            if blen > 0 {
                // Zero the last word first, so padding bytes are
                // deterministic.
                p.add(payload - 1).write(0);
                std::ptr::copy_nonoverlapping(s.as_ptr(), p.add(1) as *mut u8, blen);
            }
            Value::from_object_ptr(obj)
        }
    }

    /// Concatenate `parts` into a fresh arena Str with no host `String` in
    /// between. Allocation: `2 + total_len.div_ceil(8)` words.
    pub fn str_from_parts_in<A: Arena + ?Sized>(a: &mut A, parts: &[&str]) -> Value {
        let blen: usize = parts.iter().map(|s| s.len()).sum();
        let payload = 1 + blen.div_ceil(8);
        let obj = alloc_obj(a, HeapTag::Str, payload, false);
        // SAFETY: payload sized for the length word plus the padded bytes.
        // `alloc_words` never collects, so `parts` borrowed from existing
        // arena Strs stay valid, and the new object never overlaps them.
        unsafe {
            let p = obj.as_ptr().add(1);
            p.write(blen as u64);
            if blen > 0 {
                p.add(payload - 1).write(0);
                let mut dst = p.add(1) as *mut u8;
                for s in parts {
                    let n = s.len();
                    if n != 0 {
                        std::ptr::copy_nonoverlapping(s.as_ptr(), dst, n);
                        dst = dst.add(n);
                    }
                }
            }
            Value::from_object_ptr(obj)
        }
    }

    /// Allocation: 3 words.
    pub fn range_in<A: Arena + ?Sized>(a: &mut A, start: i64, end: i64) -> Value {
        let obj = alloc_obj(a, HeapTag::Range, 2, false);
        // SAFETY: freshly allocated 2-word payload; header written by
        // `alloc_obj`.
        unsafe {
            obj.as_ptr().add(1).write(start as u64);
            obj.as_ptr().add(2).write(end as u64);
            Value::from_object_ptr(obj)
        }
    }

    /// Allocate a fresh `Tuple` over `elements`.
    /// Allocation: `2 + elements.len()` words.
    pub fn tuple_in<A: Arena + ?Sized>(a: &mut A, elements: &[Value]) -> Value {
        if a.marks_immortal() {
            assert_frozen_children(elements);
        }
        let obj = alloc_obj(a, HeapTag::Tuple, 1 + elements.len(), false);
        // SAFETY: payload sized for the count word plus the elements; header
        // written by `alloc_obj`.
        unsafe {
            let p = obj.as_ptr().add(1);
            p.write(elements.len() as u64);
            for (i, v) in elements.iter().enumerate() {
                store_child(p.add(1 + i), v);
            }
            Value::from_object_ptr(obj)
        }
    }

    /// A `Map(String, String)` reading through to the host environment.
    /// Allocation: 2 words; entries are served live from `std::env`.
    pub fn env_map_in<A: Arena + ?Sized>(a: &mut A) -> Value {
        let obj = alloc_obj(a, HeapTag::Map, 1, false);
        // SAFETY: freshly allocated 1-word payload for the backing tag; header
        // written by `alloc_obj`.
        unsafe {
            obj.as_ptr().add(1).write(MapBacking::Env as u64);
            Value::from_object_ptr(obj)
        }
    }

    /// Allocate a fresh `Closure` capturing `captures`.
    /// Allocation: `3 + captures.len()` words.
    pub fn closure_in<A: Arena + ?Sized>(a: &mut A, func_idx: i32, captures: &[Value]) -> Value {
        if a.marks_immortal() {
            assert_frozen_children(captures);
        }
        let obj = alloc_obj(a, HeapTag::Closure, 2 + captures.len(), false);
        // SAFETY: payload sized for func_idx + count + captures; header written
        // by `alloc_obj`.
        unsafe {
            let p = obj.as_ptr().add(1);
            p.write(func_idx as u32 as u64);
            p.add(1).write(captures.len() as u64);
            for (i, v) in captures.iter().enumerate() {
                store_child(p.add(2 + i), v);
            }
            Value::from_object_ptr(obj)
        }
    }

    /// Construct an enum from prebuilt name/label values: `enum_name` and
    /// `variant_name` must be `Str`s and `labels` a `Tuple` of `Str`s, normally
    /// frozen. Allocation: `7 + payload.len()` words.
    ///
    /// `hash` MUST equal
    /// `enum_hash_with_payload(enum_name_prefix_hash(enum_name, variant_name), payload)`.
    /// Nothing checks it after construction, and equality fast-rejects on it,
    /// so a wrong hash makes equal enums silently compare unequal. Use
    /// [`Value::enum_with_names_in`] if you do not already have the hash.
    #[allow(clippy::too_many_arguments)]
    #[inline]
    pub fn enum_in<A: Arena + ?Sized>(
        a: &mut A,
        type_id: TypeId,
        variant_idx: u16,
        hash: u64,
        enum_name: Value,
        variant_name: Value,
        labels: Value,
        payload: &[Value],
    ) -> Value {
        Value::enum_reuse_in(
            a,
            ReuseAddr::none(),
            type_id,
            variant_idx,
            hash,
            enum_name,
            variant_name,
            labels,
            payload,
        )
    }

    /// As [`Value::enum_in`], overwriting `reuse` in place when it names a
    /// cell (see [`reuse_or_alloc`] for the Perceus contract).
    #[allow(clippy::too_many_arguments)]
    pub fn enum_reuse_in<A: Arena + ?Sized>(
        a: &mut A,
        reuse: ReuseAddr,
        type_id: TypeId,
        variant_idx: u16,
        hash: u64,
        enum_name: Value,
        variant_name: Value,
        labels: Value,
        payload: &[Value],
    ) -> Value {
        debug_assert!(enum_name.is_tag(HeapTag::Str) && variant_name.is_tag(HeapTag::Str));
        debug_assert!(labels.is_tag(HeapTag::Tuple));
        if a.marks_immortal() {
            assert_frozen_children(
                [&enum_name, &variant_name, &labels]
                    .into_iter()
                    .chain(payload),
            );
        }
        #[cfg(debug_assertions)]
        if hash != 0 {
            // 0 means "lazy"; anything else must be the real hash, or equal
            // enums silently compare unequal via the fast-reject.
            let prefix = enum_name_prefix_hash(
                enum_name.as_str().unwrap_or(""),
                variant_name.as_str().unwrap_or(""),
            );
            debug_assert_eq!(
                hash,
                enum_hash_with_payload(prefix, payload),
                "enum_in called with a wrong precomputed hash"
            );
        }
        let obj = reuse_or_alloc(a, reuse, HeapTag::Enum, 6 + payload.len());
        // SAFETY: payload sized for the 6 fixed words plus the payload values;
        // header written by `reuse_or_alloc`.
        unsafe {
            let p = obj.as_ptr().add(1);
            p.write((type_id.0 as u32 as u64) | ((variant_idx as u64) << 32));
            p.add(1).write(hash);
            move_child(p.add(2), enum_name);
            move_child(p.add(3), variant_name);
            move_child(p.add(4), labels);
            p.add(5).write(payload.len() as u64);
            for (i, v) in payload.iter().enumerate() {
                store_child(p.add(6 + i), v);
            }
            Value::from_object_ptr(obj)
        }
    }

    /// Also allocates the name strings and label tuple and computes the hash.
    /// A test/hydration helper; VM paths reuse frozen names via
    /// [`Value::enum_in`].
    pub fn enum_with_names_in<A: Arena + ?Sized>(
        a: &mut A,
        type_id: TypeId,
        variant_idx: u16,
        enum_name: &str,
        variant_name: &str,
        labels: &[&str],
        payload: &[Value],
    ) -> Value {
        let en = Value::str_in(a, enum_name);
        let vn = Value::str_in(a, variant_name);
        let label_vals: Vec<Value> = labels.iter().map(|l| Value::str_in(a, l)).collect();
        let labels_tuple = Value::tuple_in(a, &label_vals);
        let hash = enum_hash_with_payload(enum_name_prefix_hash(enum_name, variant_name), payload);
        Value::enum_in(a, type_id, variant_idx, hash, en, vn, labels_tuple, payload)
    }

    /// Whole-buffer binary from owned bytes. Allocation: 6 words (the box).
    #[inline]
    pub fn binary_in<A: Arena + ?Sized>(a: &mut A, bytes: Vec<u8>) -> Value {
        let bit_len = (bytes.len() as u64) * 8;
        Value::binary_bits_in(a, bytes, bit_len)
    }

    #[inline]
    pub fn binary_bits_in<A: Arena + ?Sized>(a: &mut A, bytes: Vec<u8>, bit_len: u64) -> Value {
        debug_assert!(bit_len.div_ceil(8) as usize == bytes.len());
        Value::binary_from_arc_in(a, Arc::from(bytes), bit_len)
    }

    /// Whole-buffer binary copied from a slice: one allocation and one copy,
    /// where going through a `Vec<u8>` would copy twice.
    #[inline]
    pub fn binary_from_slice_in<A: Arena + ?Sized>(a: &mut A, bytes: &[u8]) -> Value {
        let bit_len = (bytes.len() as u64) * 8;
        Value::binary_from_arc_in(a, Arc::from(bytes), bit_len)
    }

    /// Whole-buffer binary concatenating N byte windows: one allocation, each
    /// source byte copied once. Every window but the last must be whole bytes.
    /// The last window's final byte may carry a neighbouring view's bits past
    /// `bit_len`; those are masked to zero here.
    pub fn binary_concat_parts_in<A: Arena + ?Sized>(
        a: &mut A,
        parts: &[&[u8]],
        bit_len: u64,
    ) -> Value {
        let n: usize = parts.iter().map(|p| p.len()).sum();
        debug_assert_eq!(n, bit_len.div_ceil(8) as usize);
        let mut uninit = Arc::new_uninit_slice(n);
        #[allow(clippy::expect_used)]
        let dst = Arc::get_mut(&mut uninit).expect("freshly allocated Arc is unique");
        // SAFETY: `dst` is exactly `n` bytes; the copies lie back-to-back, so
        // together they initialise every byte and stay in bounds.
        unsafe {
            let base = dst.as_mut_ptr() as *mut u8;
            let mut p = base;
            for part in parts {
                std::ptr::copy_nonoverlapping(part.as_ptr(), p, part.len());
                p = p.add(part.len());
            }
            if let Some(mask) = tail_mask(bit_len)
                && n > 0
            {
                *base.add(n - 1) &= mask;
            }
        }
        // SAFETY: every byte was initialised above.
        let backing = unsafe { uninit.assume_init() };
        Value::binary_from_arc_in(a, backing, bit_len)
    }

    /// Whole-buffer binary over an already-shared backing, no byte copy.
    #[inline]
    pub fn binary_from_arc_in<A: Arena + ?Sized>(
        a: &mut A,
        backing: Arc<[u8]>,
        bit_len: u64,
    ) -> Value {
        debug_assert!(bit_len.div_ceil(8) as usize == backing.len());
        Value::binary_view_in(a, backing, 0, bit_len)
    }

    /// A zero-copy sub-view `[bit_offset, bit_offset + bit_len)` into a shared
    /// backing. Only the 6-word box is allocated, so slicing is O(1).
    pub fn binary_view_in<A: Arena + ?Sized>(
        a: &mut A,
        backing: Arc<[u8]>,
        bit_offset: u64,
        bit_len: u64,
    ) -> Value {
        debug_assert!((bit_offset + bit_len).div_ceil(8) as usize <= backing.len());
        let obj = alloc_obj(a, HeapTag::Binary, 4, true);
        let arc_len = backing.len();
        let data = Arc::into_raw(backing) as *const u8 as usize;
        // SAFETY: freshly allocated 4-word payload; header written by
        // `alloc_obj`.
        unsafe {
            let p = obj.as_ptr().add(1);
            p.write(data as u64);
            p.add(1).write(arc_len as u64);
            p.add(2).write(bit_offset);
            p.add(3).write(bit_len);
            Value::from_object_ptr(obj)
        }
    }

    /// Array from a slice; see [`seq::from_slice`] for the cost model.
    #[inline]
    pub fn array_in<A: Arena + ?Sized>(a: &mut A, items: &[Value]) -> Value {
        seq::from_slice(a, items)
    }

    #[inline(always)]
    pub fn as_int(&self) -> Option<i64> {
        if self.is_small_int() {
            Some(self.small_int_value())
        } else if self.is_tag(HeapTag::BigInt) {
            // SAFETY: tag-checked BigInt has a 1-word i64 payload.
            Some(unsafe { payload_word(self.heap_obj(), 0) } as i64)
        } else {
            None
        }
    }
    /// Int payload of a value the compiler has proven to be an int (the typed
    /// `*Int` opcodes). "Typed", not "unchecked": misuse aborts rather than
    /// silently computing with a substituted value.
    #[inline(always)]
    pub fn as_int_typed(&self) -> i64 {
        if self.is_small_int() {
            self.small_int_value()
        } else {
            match self.as_int() {
                Some(i) => i,
                None => proof_violation("*Int opcode on a non-int"),
            }
        }
    }
    #[inline(always)]
    pub fn as_float(&self) -> Option<f64> {
        if self.is_float() {
            Some(f64::from_bits(self.0))
        } else {
            None
        }
    }
    /// Float payload under [`Value::as_int_typed`]'s contract: misuse aborts.
    #[inline(always)]
    pub fn as_float_typed(&self) -> f64 {
        if self.is_float() {
            f64::from_bits(self.0)
        } else {
            proof_violation("*Float opcode on a non-float")
        }
    }
    #[inline(always)]
    pub fn as_bool(&self) -> Option<bool> {
        if self.is_bool() {
            Some(self.0 & 1 == 1)
        } else {
            None
        }
    }
    #[inline]
    pub fn as_socket(&self) -> Option<SocketValue> {
        if self.is_socket() {
            Some(decode_socket(self.0))
        } else {
            None
        }
    }
    #[inline]
    pub fn as_str(&self) -> Option<&str> {
        if self.is_tag(HeapTag::Str) {
            // SAFETY: tag-checked; lifetime constrained to `&self`.
            Some(unsafe { str_contents(self.heap_obj()) })
        } else {
            None
        }
    }
    #[inline]
    pub fn as_range(&self) -> Option<(i64, i64)> {
        if self.is_tag(HeapTag::Range) {
            let obj = self.heap_obj();
            // SAFETY: tag-checked; Range payload is two i64 words.
            unsafe { Some((payload_word(obj, 0) as i64, payload_word(obj, 1) as i64)) }
        } else {
            None
        }
    }
    #[inline]
    pub fn as_tuple(&self) -> Option<&[Value]> {
        if self.is_tag(HeapTag::Tuple) {
            let obj = self.heap_obj();
            // SAFETY: tag-checked; lifetime constrained to `&self`.
            unsafe {
                let n = payload_word(obj, 0) as usize;
                Some(payload_values(obj, 1, n))
            }
        } else {
            None
        }
    }
    #[inline]
    pub fn as_array(&self) -> Option<SeqRef<'_>> {
        if self.is_tag(HeapTag::Seq) {
            Some(SeqRef { root: self })
        } else {
            None
        }
    }
    #[inline]
    pub fn as_binary(&self) -> Option<BinaryRef<'_>> {
        if self.is_tag(HeapTag::Binary) {
            Some(BinaryRef {
                obj: self.heap_obj(),
                _life: PhantomData,
            })
        } else {
            None
        }
    }
    #[inline]
    pub fn as_closure(&self) -> Option<ClosureRef<'_>> {
        if self.is_tag(HeapTag::Closure) {
            Some(ClosureRef {
                obj: self.heap_obj(),
                _life: PhantomData,
            })
        } else {
            None
        }
    }
    #[inline]
    pub fn as_enum(&self) -> Option<EnumRef<'_>> {
        if self.is_tag(HeapTag::Enum) {
            Some(EnumRef {
                obj: self.heap_obj(),
                _life: PhantomData,
            })
        } else {
            None
        }
    }
    /// Payload field `idx` of a value the compiler has proven to be an enum
    /// with more than `idx` fields (`GetFieldUnchecked`). The proof is
    /// re-checked against the header's payload count — one compare on a word
    /// already in cache — so a compiler bug aborts instead of reading
    /// arbitrary heap words.
    #[inline(always)]
    pub fn enum_field_typed(&self, idx: usize) -> Value {
        match self.as_enum() {
            Some(e) if idx < e.payload().len() => {
                // SAFETY: tag-checked Enum with `idx` bounded by the payload
                // count; payload fields start at word 6 (see `EnumRef::payload`).
                unsafe { payload_value(self.heap_obj(), 6 + idx) }
            }
            _ => proof_violation("GetFieldUnchecked outside its proof"),
        }
    }

    /// Borrowed many-armed view, folding `BigInt` into `Int`.
    #[inline]
    pub fn kind(&self) -> ValueView<'_> {
        if self.is_small_int() {
            ValueView::Int(self.small_int_value())
        } else if self.is_float() {
            ValueView::Float(f64::from_bits(self.0))
        } else if self.is_bool() {
            ValueView::Bool(self.0 & 1 == 1)
        } else if self.is_socket() {
            ValueView::Socket(decode_socket(self.0))
        } else if self.is_heap() {
            let obj = self.heap_obj();
            // SAFETY: heap values point at live arena objects; each arm is
            // tag-checked by the match.
            unsafe {
                match header_tag(*obj) {
                    HeapTag::BigInt => ValueView::Int(payload_word(obj, 0) as i64),
                    HeapTag::Range => {
                        ValueView::Range(payload_word(obj, 0) as i64, payload_word(obj, 1) as i64)
                    }
                    HeapTag::Str => ValueView::Str(str_contents(obj)),
                    HeapTag::Binary => ValueView::Binary(BinaryRef {
                        obj,
                        _life: PhantomData,
                    }),
                    HeapTag::Tuple => {
                        let n = payload_word(obj, 0) as usize;
                        ValueView::Tuple(payload_values(obj, 1, n))
                    }
                    HeapTag::Enum => ValueView::Enum(EnumRef {
                        obj,
                        _life: PhantomData,
                    }),
                    HeapTag::Closure => ValueView::Closure(ClosureRef {
                        obj,
                        _life: PhantomData,
                    }),
                    HeapTag::Seq => ValueView::Array(SeqRef { root: self }),
                    HeapTag::SeqLeaf | HeapTag::SeqBranch => view_mismatch("kind"),
                    HeapTag::Map => ValueView::Map(MapRef {
                        obj,
                        _life: PhantomData,
                    }),
                    HeapTag::HamtBranch | HeapTag::HamtEntry | HeapTag::HamtCollision => {
                        view_mismatch("kind")
                    }
                }
            }
        } else {
            debug_assert!(self.0 == HDR_NIL);
            ValueView::Nil
        }
    }
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind() {
            ValueView::Int(i) => write!(f, "Int({i})"),
            ValueView::Float(x) => write!(f, "Float({x})"),
            ValueView::Bool(b) => write!(f, "Bool({b})"),
            ValueView::Nil => f.write_str("Nil"),
            ValueView::Socket(s) => write!(f, "Socket({}, listener={})", s.id, s.is_listener),
            ValueView::Str(s) => write!(f, "Str({s:?})"),
            ValueView::Range(a, b) => write!(f, "Range({a}, {b})"),
            ValueView::Binary(b) => write!(f, "Binary({} bits)", b.bit_len()),
            ValueView::Tuple(t) => f.debug_tuple("Tuple").field(&t).finish(),
            ValueView::Closure(c) => write!(f, "Closure(fn#{})", c.func_idx()),
            ValueView::Enum(e) => write!(
                f,
                "Enum({}.{} {:?})",
                e.enum_name(),
                e.variant_name(),
                e.payload()
            ),
            ValueView::Array(s) => f.debug_list().entries(s.iter()).finish(),
            ValueView::Map(m) => write!(f, "Map({:?})", m.backing()),
        }
    }
}

/// Borrowed view of a `Binary` box: a `bit_len`-bit MSB-first window at
/// `bit_offset` in a shared `Arc<[u8]>` backing.
///
/// Since the backing is shared, the trailing partial byte may carry a
/// neighbouring view's bits and is NOT guaranteed zero. Equality
/// ([`BinaryRef::bits_eq`]) and hashing ([`hash_value`]) are therefore defined
/// over the logical bits only.
#[derive(Clone, Copy)]
pub struct BinaryRef<'a> {
    obj: *const u64,
    _life: PhantomData<&'a u64>,
}

/// Reconstruct the fat pointer to a `Binary` box's `Arc<[u8]>` backing from
/// its two payload words. The single place that knows that encoding.
///
/// # Safety
///
/// `obj` must point at a live `Binary` box whose Arc words are intact.
#[inline]
unsafe fn binary_backing_raw(obj: *const u64) -> *const [u8] {
    unsafe {
        let data = payload_word(obj, 0) as usize as *const u8;
        let len = payload_word(obj, 1) as usize;
        std::ptr::slice_from_raw_parts(data, len)
    }
}

/// Reborrow the backing `Arc` without consuming the count the box owns.
/// Dropping the guard as a plain `Arc` would double-release it.
///
/// # Safety
///
/// As [`binary_backing_raw`].
#[inline]
unsafe fn binary_backing_reborrow(obj: *const u64) -> ManuallyDrop<Arc<[u8]>> {
    unsafe { ManuallyDrop::new(Arc::from_raw(binary_backing_raw(obj))) }
}

impl<'a> BinaryRef<'a> {
    #[inline]
    pub fn bit_offset(&self) -> u64 {
        // SAFETY: constructed from a tag-checked Binary value.
        unsafe { payload_word(self.obj, 2) }
    }
    #[inline]
    pub fn bit_len(&self) -> u64 {
        // SAFETY: as above.
        unsafe { payload_word(self.obj, 3) }
    }
    /// The full shared backing buffer, not just this view's window.
    #[inline]
    pub fn backing(&self) -> &'a [u8] {
        // SAFETY: the box holds a strong count released only when it is
        // freed, so the bytes outlive any borrow of it.
        unsafe { &*binary_backing_raw(self.obj) }
    }
    /// Clone the backing `Arc` — a count bump, no byte copy.
    pub fn backing_arc(&self) -> Arc<[u8]> {
        // SAFETY: tag-checked Binary, so the Arc words are intact.
        unsafe { Arc::clone(&binary_backing_reborrow(self.obj)) }
    }

    /// The whole logical bytes, excluding any partial trailing byte. Borrows
    /// the backing when the view is byte-aligned, else re-aligns into a copy.
    #[inline]
    pub fn full_bytes(&self) -> Cow<'a, [u8]> {
        let full = (self.bit_len() / 8) as usize;
        if self.bit_offset().is_multiple_of(8) {
            let start = (self.bit_offset() / 8) as usize;
            Cow::Borrowed(&self.backing()[start..start + full])
        } else {
            let mut v = self.to_aligned_vec();
            v.truncate(full);
            Cow::Owned(v)
        }
    }

    /// Materialise the logical bits into a fresh offset-0 buffer with the
    /// partial trailing byte masked to zero. Cold path.
    pub fn to_aligned_vec(&self) -> Vec<u8> {
        let (bit_offset, bit_len) = (self.bit_offset(), self.bit_len());
        let mut out = vec![0u8; bit_len.div_ceil(8) as usize];
        // `copy_bits` writes exactly `bit_len` bits into a zeroed buffer, so
        // no explicit tail mask is needed.
        copy_bits(&mut out, 0, self.backing(), bit_offset, bit_len);
        out
    }

    /// Whether the logical bits at `at` begin with all of `prefix`'s. Out of
    /// range is `false`, never an error.
    pub fn starts_with_at(&self, at: u64, prefix: &BinaryRef<'_>) -> bool {
        if at + prefix.bit_len() > self.bit_len() {
            return false;
        }
        let abs = self.bit_offset() + at;
        if abs.is_multiple_of(8)
            && prefix.bit_offset().is_multiple_of(8)
            && prefix.bit_len().is_multiple_of(8)
        {
            let s = (abs / 8) as usize;
            let p = (prefix.bit_offset() / 8) as usize;
            let n = (prefix.bit_len() / 8) as usize;
            return self.backing()[s..s + n] == prefix.backing()[p..p + n];
        }
        let (sb, pb) = (self.backing(), prefix.backing());
        (0..prefix.bit_len()).all(|i| get_bit(sb, abs + i) == get_bit(pb, prefix.bit_offset() + i))
    }

    /// Logical-bit equality, regardless of backing identity or offsets.
    pub fn bits_eq(&self, other: &BinaryRef<'_>) -> bool {
        if self.bit_len() != other.bit_len() {
            return false;
        }
        let bit_len = self.bit_len();
        // Both byte-aligned: compare full bytes, then the masked tail.
        if self.bit_offset().is_multiple_of(8) && other.bit_offset().is_multiple_of(8) {
            let full = (bit_len / 8) as usize;
            let s = (self.bit_offset() / 8) as usize;
            let o = (other.bit_offset() / 8) as usize;
            let (sb, ob) = (self.backing(), other.backing());
            if sb[s..s + full] != ob[o..o + full] {
                return false;
            }
            return match tail_mask(bit_len) {
                None => true,
                Some(mask) => (sb[s + full] & mask) == (ob[o + full] & mask),
            };
        }
        let (sb, ob) = (self.backing(), other.backing());
        (0..bit_len)
            .all(|i| get_bit(sb, self.bit_offset() + i) == get_bit(ob, other.bit_offset() + i))
    }
}

/// Borrowed view of a `Closure` arena object.
#[derive(Clone, Copy)]
pub struct ClosureRef<'a> {
    obj: *const u64,
    _life: PhantomData<&'a u64>,
}

impl<'a> ClosureRef<'a> {
    #[inline]
    pub fn func_idx(&self) -> i32 {
        // SAFETY: constructed from a tag-checked Closure value.
        unsafe { payload_word(self.obj, 0) as u32 as i32 }
    }
    #[inline]
    pub fn captures(&self) -> &'a [Value] {
        // SAFETY: as above; count word bounds the slice.
        unsafe {
            let n = payload_word(self.obj, 1) as usize;
            payload_values(self.obj, 2, n)
        }
    }
}

/// Borrowed view of a `Map` object. Entries are not exposed here; the VM
/// dispatches reads on [`MapRef::backing`].
#[derive(Clone, Copy)]
pub struct MapRef<'a> {
    obj: *const u64,
    _life: PhantomData<&'a u64>,
}

impl MapRef<'_> {
    #[inline]
    pub fn backing(&self) -> MapBacking {
        // SAFETY: constructed from a tag-checked Map value; word 0 is the
        // backing discriminant for every Map layout.
        map_backing(unsafe { payload_word(self.obj, 0) })
    }

    /// Decoded `[size, root]` of a `Hamt`-backed map. The backing is
    /// release-checked, since an `Env` map holds no such words.
    #[inline]
    pub(crate) fn as_hamt(&self) -> HamtMapRef {
        // SAFETY: a MapRef comes only from a tag-checked Map value.
        unsafe { HamtMapRef::from_obj(self.obj) }
    }
}

/// Borrowed view of an `Enum` object. Names and labels are `Str`/`Tuple`
/// values, normally frozen.
#[derive(Clone, Copy)]
pub struct EnumRef<'a> {
    obj: *const u64,
    _life: PhantomData<&'a u64>,
}

impl<'a> EnumRef<'a> {
    #[inline]
    pub fn type_id(&self) -> TypeId {
        // SAFETY: constructed from a tag-checked Enum value.
        TypeId(unsafe { payload_word(self.obj, 0) as u32 as i32 })
    }
    /// Declaration-order index of this value's constructor within its type.
    /// `Op::SwitchTag` reads it to turn an exhaustive match into one jump.
    #[inline]
    pub fn variant_idx(&self) -> u16 {
        // SAFETY: constructed from a tag-checked Enum value.
        unsafe { (payload_word(self.obj, 0) >> 32) as u16 }
    }
    /// The raw stored hash word; `0` means "not computed yet". Heap cells
    /// defer hashing to first use. Frozen cells always carry a build-time
    /// hash, because [`freeze_enum_hash`] runs before they are marked
    /// immortal.
    #[inline]
    fn stored_hash(&self) -> u64 {
        // SAFETY: as above.
        unsafe { payload_word(self.obj, 1) }
    }

    /// The value hash, computed on first use and cached in the cell. Hashing
    /// eagerly at construction measured ~5% of a keep-alive request, since
    /// almost no constructed enum is ever a map key. The in-place write is
    /// sound because a process heap has one owner thread, and a frozen cell —
    /// shared across threads — always has a nonzero hash already. A true hash
    /// of `0` is recomputed per read.
    pub fn hash(&self) -> u64 {
        let stored = self.stored_hash();
        if stored != 0 {
            return stored;
        }
        let prefix = enum_name_prefix_hash(self.enum_name(), self.variant_name());
        let h = enum_hash_with_payload(prefix, self.payload());
        // SAFETY: tag-checked Enum cell; word 1 is the hash slot.
        unsafe {
            if h != 0 && !header_is_immortal(*self.obj) {
                (self.obj as *mut u64).add(2).write(h);
            }
        }
        h
    }
    /// The `Str` value holding the enum type name (for re-construction).
    #[inline]
    pub fn enum_name_value(&self) -> Value {
        // SAFETY: as above.
        unsafe { payload_value(self.obj, 2) }
    }
    #[inline]
    pub fn variant_name_value(&self) -> Value {
        // SAFETY: as above.
        unsafe { payload_value(self.obj, 3) }
    }
    /// The `Tuple`-of-`Str` value holding the field labels.
    #[inline]
    pub fn labels_value(&self) -> Value {
        // SAFETY: as above.
        unsafe { payload_value(self.obj, 4) }
    }
    #[inline]
    pub fn enum_name(&self) -> &'a str {
        // SAFETY: enum_name is a Str by construction; lifetime tied to 'a.
        unsafe { str_contents(self.enum_name_value().heap_obj()) }
    }
    #[inline]
    pub fn variant_name(&self) -> &'a str {
        // SAFETY: as above.
        unsafe { str_contents(self.variant_name_value().heap_obj()) }
    }
    /// Field labels parallel to `payload()`; empty for nullary constructors.
    #[inline]
    pub fn field_labels(&self) -> &'a [Value] {
        let labels = self.labels_value();
        // SAFETY: labels is a Tuple by construction.
        unsafe {
            let obj = labels.heap_obj();
            let n = payload_word(obj, 0) as usize;
            payload_values(obj, 1, n)
        }
    }
    #[inline]
    pub fn payload(&self) -> &'a [Value] {
        // SAFETY: count word bounds the slice.
        unsafe {
            let n = payload_word(self.obj, 5) as usize;
            payload_values(self.obj, 6, n)
        }
    }
}

/// Borrowed view of a `Seq` (array) root.
#[derive(Clone, Copy)]
pub struct SeqRef<'a> {
    root: &'a Value,
}

impl<'a> SeqRef<'a> {
    #[inline]
    pub fn len(&self) -> usize {
        seq::len(self.root)
    }
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    #[inline]
    pub fn get(&self, i: usize) -> Option<Value> {
        seq::get(self.root, i)
    }
    /// Iterate front to back. The iterator owns the root's section nodes.
    pub fn iter(&self) -> SeqIter {
        SeqIter::new(self.root)
    }
}

/// The sole layout table for object tracing: yield a raw pointer to every
/// payload slot of `obj` that holds a `Value`, skipping raw words. Callers
/// reborrow as they are entitled to — [`for_each_child`] as `&mut`,
/// [`Value::for_each_child_ref`] as `&`. Generic rather than `dyn` because an
/// indirect call per visited word cost ~9% of `bench_typed`.
///
/// # Safety
/// `obj` must point at a live arena object header. The slot pointers live only
/// for the walk, and a `&mut Value` also requires no other live reference to
/// that slot.
#[inline]
pub(crate) unsafe fn for_each_child_slot<F: FnMut(*mut Value)>(obj: *const u64, f: &mut F) {
    // SAFETY (whole body): every slot index stays within the payload length
    // the header declares, per the layouts in the module docs.
    unsafe {
        let h = *obj;
        debug_assert!(header_marks_object(h));
        let p = obj.add(1);
        let visit = |start: usize, n: usize, f: &mut F| {
            for i in start..start + n {
                f(p.add(i) as *mut Value);
            }
        };
        match header_tag(h) {
            HeapTag::BigInt | HeapTag::Range | HeapTag::Str | HeapTag::Binary => {}
            // `Env` holds no children; `Hamt` points at one root node.
            HeapTag::Map => match map_backing(*p) {
                MapBacking::Env => {}
                MapBacking::Hamt => visit(2, 1, f),
            },
            // `[bitmap, child…]` — one child per set bit.
            HeapTag::HamtBranch => visit(1, (*p as u32).count_ones() as usize, f),
            // `[key, value]`.
            HeapTag::HamtEntry => visit(0, 2, f),
            // `[hash, count, key, value, …]` — `2 * count` value words.
            HeapTag::HamtCollision => {
                let count = *p.add(1) as usize;
                visit(2, 2 * count, f);
            }
            HeapTag::Tuple => {
                let n = *p as usize;
                visit(1, n, f);
            }
            HeapTag::Enum => {
                visit(2, 3, f);
                let n = *p.add(5) as usize;
                visit(6, n, f);
            }
            HeapTag::Closure => {
                let n = *p.add(1) as usize;
                visit(2, n, f);
            }
            HeapTag::Seq => visit(2, 3, f),
            HeapTag::SeqLeaf => {
                let n = *p as usize;
                visit(1, n, f);
            }
            HeapTag::SeqBranch => {
                let n = *p as usize;
                visit(2 + n, n, f);
            }
        }
    }
}

/// The mutating face of [`for_each_child_slot`]: visit every child as `&mut`.
///
/// # Safety
///
/// `obj` must be a live object header the caller owns exclusively — a private
/// mortal object, never a shared immortal one — and the callback must only
/// rewrite the visited slot, never read other arena state mid-walk.
#[inline]
pub(crate) unsafe fn for_each_child<F: FnMut(&mut Value)>(obj: *mut u64, f: &mut F) {
    // SAFETY: forwarded from this function's own contract; exclusive
    // ownership is what makes the `&mut` unique.
    unsafe { for_each_child_slot(obj as *const u64, &mut |p: *mut Value| f(&mut *p)) }
}

/// Bump the backing `Arc` of the `Binary` box at `obj`, so a graph copy and
/// its live source each own a count.
///
/// # Safety
///
/// `obj` must point at a live `Binary` box whose Arc words are intact.
pub(crate) unsafe fn binary_clone_backing(obj: *const u64) {
    // SAFETY (forget): the clone's count is handed to the copied box.
    unsafe {
        let h = *obj;
        debug_assert!(header_marks_object(h));
        debug_assert!(header_tag(h) == HeapTag::Binary);
        std::mem::forget(Arc::clone(&binary_backing_reborrow(obj)))
    }
}

/// Release the backing `Arc` owned by the `Binary` box at `obj`, exactly once
/// per box.
///
/// # Safety
///
/// `obj` must point at a `Binary` box whose Arc words are intact and whose
/// count has not already been released.
pub(crate) unsafe fn binary_drop_backing(obj: *const u64) {
    // No reborrow guard: this is the one place that consumes the box's
    // count, so the reconstructed Arc is dropped for real.
    unsafe {
        let h = *obj;
        debug_assert!(header_marks_object(h));
        debug_assert!(header_tag(h) == HeapTag::Binary);
        drop(Arc::from_raw(binary_backing_raw(obj)))
    }
}

// A mortal object's refcount word sits immediately BEFORE its header, and
// every `Value` points at the header, so header-relative offsets are the same
// for counted and immortal objects. Immortal objects have no such word, and
// every counting path gates on `is_immortal()` first.
//
// There is no cycle collector, which is complete only because the heap is
// acyclic by construction: immutable values, capture by value with no
// backpatch, self-reference through the live call frame, mutual recursion
// through the immortal global table. A construct that could tie a heap cycle
// would leak.

/// Words reserved before a mortal object's header for its refcount.
pub(crate) const RC_PREFIX_WORDS: usize = 1;

/// The refcount slot, also the allocation start: the word before the header.
/// Public for the native backend's tests.
///
/// # Safety
/// `obj` must be a mortal (non-immortal) heap object pointer.
#[inline]
pub unsafe fn rc_slot(obj: *const u64) -> *mut u64 {
    unsafe { (obj as *mut u64).sub(RC_PREFIX_WORDS) }
}

/// Increment a mortal object's refcount. A saturated count is permanently
/// live and never freed.
///
/// # Safety
/// `obj` must be a mortal heap object with an initialized refcount slot.
#[inline]
pub(crate) unsafe fn rc_increment(obj: *const u64) {
    unsafe {
        let p = rc_slot(obj);
        *p = (*p).saturating_add(1);
    }
}

/// Decrement a mortal object's refcount; `true` means the caller must now free
/// it. A saturated count never decrements.
///
/// # Safety
/// `obj` must be a mortal heap object with an initialized refcount slot.
#[inline]
pub(crate) unsafe fn rc_decrement_is_zero(obj: *const u64) -> bool {
    unsafe {
        let p = rc_slot(obj);
        if *p == u64::MAX {
            return false;
        }
        *p -= 1;
        *p == 0
    }
}

/// Free one mortal object, releasing any off-heap `Arc` it owns. Does NOT
/// touch its `Value` children — the caller has already decremented them.
///
/// # Safety
/// `obj` must be a live, unreferenced, not-yet-freed mortal heap object
/// allocated through a `ProcHeap`.
#[inline]
unsafe fn free_object(obj: *mut u64) {
    unsafe {
        if header_has_off_heap_link(*obj) {
            binary_drop_backing(obj);
        }
        // Poison the block so a use-after-free is loud: a stale header read
        // trips `header_marks_object` and a stale decref underflows.
        #[cfg(debug_assertions)]
        {
            let words = RC_PREFIX_WORDS + header_total_words(*obj);
            let base = rc_slot(obj);
            for i in 0..words {
                base.add(i).write(0);
            }
        }
        libmimalloc_sys::mi_free(rc_slot(obj).cast());
    }
    FREED_OBJECTS.with(|c| c.set(c.get() + 1));
}

thread_local! {
    /// Reusable scratch for the free-at-zero traversal, so a `Drop` never
    /// allocates. Sound as one buffer per thread because the traversal is
    /// non-reentrant and empty between releases.
    static DROP_STACK: RefCell<Vec<*mut u64>> = const { RefCell::new(Vec::new()) };

    /// Objects freed on this thread since the last [`take_freed_objects`]. The
    /// VM drains it at call checkpoints to charge a process for reclamation, so
    /// a large cascading free preempts instead of stalling the scheduler.
    static FREED_OBJECTS: Cell<u64> = const { Cell::new(0) };

    /// Running total of drained frees since the last
    /// [`reset_freed_objects_total`]. Parity tests assert every allocation was
    /// freed exactly once, whichever backend did the freeing.
    static FREED_OBJECTS_TOTAL: Cell<u64> = const { Cell::new(0) };
}

/// Objects freed on this thread since the last call, reset to zero. Also
/// feeds [`freed_objects_total`].
#[inline]
pub fn take_freed_objects() -> u64 {
    let n = FREED_OBJECTS.with(|c| c.replace(0));
    if n != 0 {
        FREED_OBJECTS_TOTAL.with(|c| c.set(c.get() + n));
    }
    n
}

/// Objects freed on this thread since the last [`take_freed_objects`], without
/// resetting. The call checkpoint peeks this before deciding to drain.
#[inline]
pub fn freed_objects_pending() -> u64 {
    FREED_OBJECTS.with(|c| c.get())
}

/// Every object freed on this thread since the last
/// [`reset_freed_objects_total`], drained or not. Test instrumentation: it
/// must equal `ProcHeap::alloc_count` once the VM is dropped.
pub fn freed_objects_total() -> u64 {
    FREED_OBJECTS_TOTAL.with(|c| c.get()) + FREED_OBJECTS.with(|c| c.get())
}

/// Zero this thread's [`freed_objects_total`]. Call alongside
/// `ProcHeap::reset_alloc_count`, right before the span being measured.
pub fn reset_freed_objects_total() {
    FREED_OBJECTS_TOTAL.with(|c| c.set(0));
    FREED_OBJECTS.with(|c| c.set(0));
}

/// Store a *borrowed* `child` into an object slot, taking a new reference. The
/// caller keeps its own and drops it later, so the object ends up owning
/// exactly the references it holds. Use [`move_child`] for owned arguments.
///
/// # Safety
/// `slot` must be a writable object payload word; `child` a valid value.
#[inline]
pub(crate) unsafe fn store_child(slot: *mut u64, child: &Value) {
    if child.is_heap() && !child.is_immortal() {
        // SAFETY: mortal heap child has a refcount slot.
        unsafe { rc_increment(child.heap_obj()) };
    }
    unsafe { slot.write(child.0) };
}

/// Move an *owned* value into an object slot, transferring its reference with
/// no count change.
///
/// # Safety
/// `slot` must be a writable object payload word.
#[inline]
pub(crate) unsafe fn move_child(slot: *mut u64, child: Value) {
    unsafe { slot.write(child.0) };
    std::mem::forget(child); // ownership now lives in the slot
}

/// Build an owned (counted) value from raw bits. Unlike [`Value::from_bits`],
/// which yields a bare alias, the result is safe to drop.
///
/// # Safety
/// `bits` must come from a live value (`to_bits`/`from_object_ptr`).
#[inline]
pub(crate) unsafe fn owned_from_bits(bits: u64) -> Value {
    // Pure bit math: immortal and immediate values need no count, and their
    // object is never read.
    if bits & (SIGN | QNAN) == (SIGN | QNAN) && bits & VALUE_IMMORTAL == 0 {
        // SAFETY: mortal heap bits point at a live object with a refcount slot.
        unsafe { rc_increment((bits & PTR_PAYLOAD) as *const u64) };
    }
    Value(bits)
}

/// Release one reference held as raw value bits, freeing at zero and
/// transitively releasing everything the object uniquely owns. Iterative, so a
/// long cons list cannot overflow the native stack. Takes bits, not a `Value`,
/// so it never builds another droppable value. The hottest function in the VM,
/// and nearly every call is an immediate or a frozen constant — hence the
/// `#[cold]` split.
#[inline]
pub(crate) fn release_bits(bits: u64) {
    // Pure bit math, and crucially it never reads the object: a frozen value
    // can be released after its frozen area is gone.
    if bits & (SIGN | QNAN) != (SIGN | QNAN) || bits & VALUE_IMMORTAL != 0 {
        return;
    }
    let obj = (bits & PTR_PAYLOAD) as *mut u64;
    // SAFETY: a mortal heap object has an initialized refcount slot.
    if !unsafe { rc_decrement_is_zero(obj) } {
        return;
    }
    // SAFETY: the count just reached zero, so this frame is the sole owner.
    unsafe { release_at_zero(obj) };
}

// The Cranelift backend bakes these into generated code. Derived from the same
// private constants the interpreter uses, so the two backends cannot drift.

/// Mask for the mortal-heap drop gate the native backend emits inline:
/// `bits & NATIVE_MORTAL_GATE_MASK == NATIVE_MORTAL_HEAP_BITS`. This is
/// [`release_bits`]' fast-path test, exported as bit constants.
pub const NATIVE_MORTAL_GATE_MASK: u64 = SIGN | QNAN | VALUE_IMMORTAL;
/// Expected gate result for a mortal heap value; see [`NATIVE_MORTAL_GATE_MASK`].
pub const NATIVE_MORTAL_HEAP_BITS: u64 = SIGN | QNAN;
/// Mask recovering the object header pointer from a heap value word.
pub const NATIVE_PTR_MASK: u64 = PTR_PAYLOAD;
/// Byte offset from the object header pointer to its refcount slot.
pub const NATIVE_RC_BYTE_OFFSET: i32 = -((RC_PREFIX_WORDS as i32) * 8);

/// Symbol name JIT modules register [`native_release_at_zero`] under.
pub const NATIVE_RELEASE_AT_ZERO_SYMBOL: &str = "al_native_release_at_zero";

/// [`release_at_zero`] behind an `extern "C"` ABI for JIT-compiled code. The
/// native drop sequence inlines everything up to the zero test and calls this
/// only at zero, so `FREED_OBJECTS` accounting stays identical across backends.
///
/// # Safety
/// `obj` must be a live mortal heap object whose refcount just reached zero,
/// not yet freed, allocated through a `ProcHeap`.
#[cold]
pub unsafe extern "C" fn native_release_at_zero(obj: *mut u64) {
    unsafe { release_at_zero(obj) }
}

/// Symbol name JIT modules register [`native_hollow_for_reuse`] under.
pub const NATIVE_HOLLOW_FOR_REUSE_SYMBOL: &str = "al_native_hollow_for_reuse";

/// [`Value::hollow_for_reuse`]'s child-release walk behind an `extern "C"` ABI
/// for JIT-compiled code. The native sequence inlines the gate and the rc==1
/// test and calls this only on a uniquely-owned cell; the hollowed allocation
/// stays parked in its frame slot for the paired reuse constructor.
///
/// # Safety
/// `obj` must be a live, uniquely-owned (rc == 1) mortal heap object
/// allocated through a `ProcHeap`.
pub unsafe extern "C" fn native_hollow_for_reuse(obj: *mut u64) {
    unsafe { hollow_children(obj) }
}

/// Release every mortal heap child of `obj`, overwriting each slot with an
/// immediate sentinel. Immediate and frozen children are skipped: they own no
/// reference to give back, and the paired constructor rewrites every child
/// word anyway.
///
/// # Safety
/// `obj` must be a live, uniquely-owned (rc == 1) mortal heap object.
unsafe fn hollow_children(obj: *mut u64) {
    unsafe {
        for_each_child(obj, &mut |c: &mut Value| {
            if c.is_heap() && !c.is_immortal() {
                *c = Value::small_int(0);
            }
        });
    }
}

/// Free `obj`, whose refcount just hit zero, and everything it transitively
/// owns. Out of line so [`release_bits`]' fast path can inline without the
/// thread-local access. The common cases orphan at most one child, so the loop
/// below just walks the chain; only a second orphan falls back to
/// [`release_pending`] and its work list.
///
/// # Safety
/// `obj` must be a live mortal heap object at count 0, not yet freed.
#[cold]
#[inline(never)]
unsafe fn release_at_zero(mut obj: *mut u64) {
    debug_assert!(
        DROP_STACK.with(|cell| cell.borrow().is_empty()),
        "drop stack must be empty between releases"
    );
    loop {
        let mut next: *mut u64 = std::ptr::null_mut();
        let mut spilled = false;
        // SAFETY: `obj` is at count 0 awaiting free; its child slots stay
        // live until `free_object`.
        unsafe {
            for_each_child(obj, &mut |child: &mut Value| {
                if !child.is_heap() || child.is_immortal() {
                    return;
                }
                let c = child.heap_obj() as *mut u64;
                if !rc_decrement_is_zero(c) {
                    return;
                }
                if next.is_null() {
                    next = c;
                } else {
                    spilled = true;
                    DROP_STACK.with(|cell| cell.borrow_mut().push(c));
                }
            });
            free_object(obj);
        }
        if spilled {
            // SAFETY: `spilled` implies `next` is set, and it and every
            // stacked pointer name a mortal object at count 0.
            return unsafe { release_pending(next) };
        }
        if next.is_null() {
            return;
        }
        obj = next;
    }
}

/// Drain the thread-local work list, starting from `seed`.
///
/// # Safety
/// `seed` and every pointer on `DROP_STACK` must be a live mortal heap object
/// at count 0, not yet freed.
#[cold]
#[inline(never)]
unsafe fn release_pending(seed: *mut u64) {
    DROP_STACK.with(|cell| {
        let mut stack = cell.borrow_mut();
        stack.push(seed);
        while let Some(obj) = stack.pop() {
            // SAFETY: every queued pointer is at count 0 awaiting free.
            unsafe {
                for_each_child(obj, &mut |child: &mut Value| {
                    if child.is_heap() && !child.is_immortal() {
                        let c = child.heap_obj() as *mut u64;
                        if rc_decrement_is_zero(c) {
                            stack.push(c);
                        }
                    }
                });
                free_object(obj);
            }
        }
    });
}

const HASH_BASIS: u64 = 0xcbf29ce484222325;

#[inline]
fn fnv1a_combine(h: u64, val: u64) -> u64 {
    (h ^ val).wrapping_mul(0x100000001b3)
}

/// Fold each byte into the hash. [`hash_value`] and [`enum_name_prefix_hash`]
/// must agree byte-for-byte, so both fold through this one helper.
#[inline]
fn fnv1a_bytes(mut h: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        h = fnv1a_combine(h, b as u64);
    }
    h
}

/// Leading elements folded into a sequence hash. The hash is only an equality
/// fast-reject, so a bounded prefix plus the length keeps hashing O(1) —
/// `Some(0..n)` must not walk the whole range.
const SEQ_HASH_SAMPLE: usize = 32;

/// Leading and trailing bytes folded into a `Str`/`Binary` hash, for the same
/// reason as [`SEQ_HASH_SAMPLE`]: wrapping a multi-megabyte buffer in `Ok(..)`
/// must not re-walk it. Payloads up to twice the sample hash in full.
const BYTES_HASH_SAMPLE: usize = 64;

/// Hash `len` logical bytes via `byte_at`, sampling per [`BYTES_HASH_SAMPLE`].
/// Callers fold the length in separately, so differing lengths fast-reject.
#[inline]
fn fnv1a_bytes_sampled(mut h: u64, len: usize, mut byte_at: impl FnMut(usize) -> u8) -> u64 {
    if len <= 2 * BYTES_HASH_SAMPLE {
        for i in 0..len {
            h = fnv1a_combine(h, byte_at(i) as u64);
        }
    } else {
        for i in 0..BYTES_HASH_SAMPLE {
            h = fnv1a_combine(h, byte_at(i) as u64);
        }
        for i in len - BYTES_HASH_SAMPLE..len {
            h = fnv1a_combine(h, byte_at(i) as u64);
        }
    }
    h
}

/// The `i`-th logical byte of a bit-unaligned binary view, MSB-first. Bits
/// past the end of `backing` read as zero; callers mask a partial tail through
/// [`tail_mask`], matching [`BinaryRef::to_aligned_vec`].
#[inline]
fn logical_byte(backing: &[u8], bit_offset: u64, i: usize) -> u8 {
    read_byte(backing, bit_offset + 8 * i as u64)
}

#[inline]
fn hash_int(i: i64) -> u64 {
    fnv1a_combine(HASH_BASIS, i as u64)
}

/// The [`hash_value`] of a `Str` holding `s`, computable without a `Value`.
/// The `Str` arm delegates here, so host strings and arena `Str`s hash the
/// same way — which is what the `Env` map fold relies on.
#[inline]
fn hash_str(s: &str) -> u64 {
    let bytes = s.as_bytes();
    let h = fnv1a_combine(HASH_BASIS, bytes.len() as u64);
    fnv1a_bytes_sampled(h, bytes.len(), |i| bytes[i])
}

/// Per-entry combine for a map's order-independent fold. Both backings fold
/// every entry through this with `wrapping_add`, so equal maps hash equally
/// regardless of backing or insertion order.
#[inline]
pub(crate) fn map_entry_hash(key_hash: u64, value_hash: u64) -> u64 {
    key_hash.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ value_hash
}

/// Entry fold of the `Env` map view. Non-UTF-8 entries are invisible to a
/// `Map(String, String)`, so they are skipped, matching the VM's `Env` reads.
/// Nothing in the runtime mutates the environment, so the fold is stable.
fn env_map_hash() -> u64 {
    let mut acc = 0u64;
    for (k, v) in std::env::vars_os() {
        if let (Some(k), Some(v)) = (k.to_str(), v.to_str()) {
            acc = acc.wrapping_add(map_entry_hash(hash_str(k), hash_str(v)));
        }
    }
    acc
}

/// Structural equality of the `Env` view against a HAMT map: equal counts
/// plus containment, which is a bijection because HAMT keys are distinct.
/// The environment is snapshotted rather than probed per key: `env::var` is an
/// O(env) scan and is case-insensitive on Windows, where a HAMT with
/// case-variant keys could then compare equal.
fn env_equals_hamt(m: MapRef<'_>) -> bool {
    let env: std::collections::HashMap<String, String> = std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .collect();
    super::hamt::hamt_matches(m, env.len(), |k, v| match (k.as_str(), v.as_str()) {
        (Some(k), Some(v)) => env.get(k).is_some_and(|ev| ev == v),
        _ => false,
    })
}

/// Hash a sequence from its length and a bounded prefix of element hashes. A
/// `Range` and the array it materialises to agree on both, so they hash
/// identically without ever iterating a huge range.
#[inline]
fn hash_sequence(len: usize, elem_hashes: impl Iterator<Item = u64>) -> u64 {
    let mut h = fnv1a_combine(HASH_BASIS, len as u64);
    for eh in elem_hashes.take(SEQ_HASH_SAMPLE) {
        h = fnv1a_combine(h, eh);
    }
    h
}

/// Equality worklist. The inline capacity covers ordinary nesting, so the
/// common comparison never mallocs — `values_equal` runs on every map probe.
pub(super) type EqPending = SmallVec<[(Value, Value); 16]>;

/// Decide a pair without descending. `None` means it needs the worklist:
/// heap composites, and cross-kind pairs like Range vs Array.
#[inline]
fn decide_flat(x: &Value, y: &Value) -> Option<bool> {
    // Bit-identical words are always equal, since a real NaN never enters
    // the box.
    if x.0 == y.0 {
        return Some(true);
    }
    match (x.kind(), y.kind()) {
        (ValueView::Int(a), ValueView::Int(b)) => Some(a == b),
        (ValueView::Float(a), ValueView::Float(b)) => Some(a == b),
        (ValueView::Bool(a), ValueView::Bool(b)) => Some(a == b),
        (ValueView::Str(a), ValueView::Str(b)) => Some(a == b),
        (ValueView::Nil, ValueView::Nil) => Some(true),
        _ => None,
    }
}

/// Decide one child pair in place if it is flat, else defer it to the
/// worklist. Shared with [`super::hamt::hamts_equal`], so map entry values
/// join the same worklist instead of recursing.
#[inline]
pub(super) fn eq_defer(pending: &mut EqPending, x: &Value, y: &Value) -> bool {
    match decide_flat(x, y) {
        Some(eq) => eq,
        None => {
            pending.push((x.clone(), y.clone()));
            true
        }
    }
}

/// Stream two equal-length slices pairwise: scalars decide in place, and a
/// mismatch stops without visiting the rest. Deferred pairs are reversed so
/// the driver's `pop` compares them left-to-right, which keeps `pending` at
/// O(depth) for chain shapes instead of one deferred head per level.
fn push_pairs(pending: &mut EqPending, a: &[Value], b: &[Value]) -> bool {
    let start = pending.len();
    let all = a.len() == b.len() && a.iter().zip(b).all(|(x, y)| eq_defer(pending, x, y));
    all && {
        pending[start..].reverse();
        true
    }
}

/// Element count of the half-open range `s..e`, 0 for `e <= s` and saturating
/// at `i64::MAX`. Shared by equality, hashing and the VM sequence ops so every
/// Range/Array cross-path agrees on one length.
#[inline]
pub fn range_len(s: i64, e: i64) -> i64 {
    e.saturating_sub(s).max(0)
}

/// AL structural equality — the semantics of `==`. Lives here as the partner
/// of [`hash_value`]; [`super::hamt`] needs both to key the persistent map.
/// Ranges and arrays compare element-wise, and maps compare structurally
/// regardless of order or backing, so an `Env` map equals a HAMT holding
/// exactly the environment's entries.
///
/// Iterative: child pairs go onto an explicit worklist, so a 100k-deep cons
/// list cannot overflow the native stack. Map keys are compared by fresh
/// `values_equal` calls, each itself iterative.
pub fn values_equal(a: &Value, b: &Value) -> bool {
    let mut pending = EqPending::new();
    if !pair_equal(a, b, &mut pending) {
        return false;
    }
    while let Some((x, y)) = pending.pop() {
        if !pair_equal(&x, &y, &mut pending) {
            return false;
        }
    }
    true
}

/// One step of [`values_equal`]: decide the pair, or push its children.
fn pair_equal(a: &Value, b: &Value, pending: &mut EqPending) -> bool {
    if let Some(eq) = decide_flat(a, b) {
        return eq;
    }
    match (a.kind(), b.kind()) {
        (ValueView::Enum(ae), ValueView::Enum(be)) => {
            // A cheap "not equal"; an uncomputed side just skips it.
            let (ha, hb) = (ae.stored_hash(), be.stored_hash());
            (ha == 0 || hb == 0 || ha == hb)
                && ae.type_id() == be.type_id()
                && ae.variant_name() == be.variant_name()
                && push_pairs(pending, ae.payload(), be.payload())
        }
        (ValueView::Closure(x), ValueView::Closure(y)) => {
            x.func_idx() == y.func_idx() && push_pairs(pending, x.captures(), y.captures())
        }
        (ValueView::Array(aa), ValueView::Array(ba)) => {
            // As push_pairs, including the reverse.
            let start = pending.len();
            let all = aa.len() == ba.len()
                && aa
                    .iter()
                    .zip(ba.iter())
                    .all(|(x, y)| eq_defer(pending, &x, &y));
            all && {
                pending[start..].reverse();
                true
            }
        }
        (ValueView::Range(as_, ae), ValueView::Range(bs, be)) => {
            let alen = range_len(as_, ae);
            let blen = range_len(bs, be);
            (alen == 0 && blen == 0) || (as_ == bs && ae == be)
        }
        (ValueView::Range(s, e), ValueView::Array(arr))
        | (ValueView::Array(arr), ValueView::Range(s, e)) => {
            let len = range_len(s, e) as usize;
            if arr.len() != len {
                return false;
            }
            for (i, av) in arr.iter().enumerate() {
                let n = match av.as_int() {
                    Some(n) => n,
                    None => return false,
                };
                if n != s + i as i64 {
                    return false;
                }
            }
            true
        }
        (ValueView::Binary(a), ValueView::Binary(b)) => a.bits_eq(&b),
        (ValueView::Tuple(at), ValueView::Tuple(bt)) => push_pairs(pending, at, bt),
        (ValueView::Socket(asv), ValueView::Socket(bsv)) => {
            asv.id == bsv.id && asv.is_listener == bsv.is_listener
        }
        (ValueView::Map(am), ValueView::Map(bm)) => match (am.backing(), bm.backing()) {
            (MapBacking::Hamt, MapBacking::Hamt) => super::hamt::hamts_equal(am, bm, pending),
            (MapBacking::Env, MapBacking::Env) => true,
            (MapBacking::Env, MapBacking::Hamt) => env_equals_hamt(bm),
            (MapBacking::Hamt, MapBacking::Env) => env_equals_hamt(am),
        },
        _ => false,
    }
}

/// Equality fast-reject hash: `values_equal` values must hash identically,
/// unequal ones may collide. Every arm folds exactly what equality inspects;
/// omitting a component is sound but forfeits the fast-reject for it.
pub fn hash_value(v: &Value) -> u64 {
    let mut h = HASH_BASIS;
    match v.kind() {
        ValueView::Int(i) => {
            h = fnv1a_combine(h, i as u64);
        }
        ValueView::Float(f) => {
            // `+0.0` and `-0.0` are `values_equal` but differ in bits.
            let bits = if f == 0.0 { 0 } else { f.to_bits() };
            h = fnv1a_combine(h, bits);
        }
        ValueView::Bool(b) => {
            h = fnv1a_combine(h, if b { 1 } else { 0 });
        }
        ValueView::Str(s) => {
            h = hash_str(s);
        }
        ValueView::Enum(e) => {
            h = e.hash();
        }
        ValueView::Array(a) => {
            h = hash_sequence(a.len(), a.iter().map(|e| hash_value(&e)));
        }
        ValueView::Range(start, end) => {
            let len = range_len(start, end) as usize;
            h = hash_sequence(len, (start..end).map(hash_int));
        }
        ValueView::Binary(bin) => {
            // The logical bits only, to stay consistent with `bits_eq`:
            // aligned views hash straight off the backing, unaligned ones
            // extract logical bytes, and both fold the same values.
            let (bit_offset, bit_len) = (bin.bit_offset(), bin.bit_len());
            let backing = bin.backing();
            let full = (bit_len / 8) as usize;
            h = fnv1a_combine(h, bit_len);
            if bit_offset.is_multiple_of(8) {
                let start = (bit_offset / 8) as usize;
                let window = &backing[start..start + full];
                h = fnv1a_bytes_sampled(h, full, |i| window[i]);
                if let Some(mask) = tail_mask(bit_len) {
                    h = fnv1a_combine(h, (backing[start + full] & mask) as u64);
                }
            } else {
                h = fnv1a_bytes_sampled(h, full, |i| logical_byte(backing, bit_offset, i));
                if let Some(mask) = tail_mask(bit_len) {
                    h = fnv1a_combine(h, (logical_byte(backing, bit_offset, full) & mask) as u64);
                }
            }
        }
        ValueView::Tuple(t) => {
            // Like arrays, since they compare element-wise. A tuple is never
            // `values_equal` to an array, so the shared shape only collides.
            h = hash_sequence(t.len(), t.iter().map(hash_value));
        }
        ValueView::Closure(c) => {
            // Both halves of closure equality, so different captures
            // fast-reject.
            h = fnv1a_combine(h, c.func_idx() as u64);
            for cap in c.captures() {
                h = fnv1a_combine(h, hash_value(cap));
            }
        }
        ValueView::Socket(s) => {
            // Socket equality is identity: descriptor id plus role.
            h = fnv1a_combine(h, s.id as u64);
            h = fnv1a_combine(h, s.is_listener as u64);
        }
        ValueView::Nil => {
            // `Nil` is equal only to itself; any constant respects equality.
            h = fnv1a_combine(h, 0);
        }
        ValueView::Map(m) => {
            // The backing tag is deliberately NOT folded: an `Env` view and
            // a HAMT with the same entries must hash identically.
            h = h.wrapping_add(match m.backing() {
                MapBacking::Hamt => super::hamt::hamt_hash(m),
                MapBacking::Env => env_map_hash(),
            });
        }
    }
    h
}

/// Compute and cache an Enum cell's hash if still unset. The publish path
/// calls this on the still-mortal source cell before freezing its image: a
/// frozen cell is shared across threads and must never be written lazily.
/// Non-enum tags and already-hashed cells are no-ops.
///
/// # Safety
/// `obj` must point at a live heap object the calling thread owns exclusively
/// — the cell may be written.
pub unsafe fn freeze_enum_hash(obj: *const u64) {
    unsafe {
        if header_tag(*obj) == HeapTag::Enum {
            let r = EnumRef {
                obj,
                _life: std::marker::PhantomData::<&u64>,
            };
            let _ = r.hash();
        }
    }
}

/// Hash of the name prefix alone. The names are compile-time constants, so
/// the compiler computes this once per constructor site and the VM folds
/// payloads into it via [`enum_hash_with_payload`].
pub fn enum_name_prefix_hash(enum_name: &str, variant_name: &str) -> u64 {
    let h = fnv1a_bytes(HASH_BASIS, enum_name.as_bytes());
    fnv1a_bytes(h, variant_name.as_bytes())
}

/// Fold payload value hashes into a precomputed [`enum_name_prefix_hash`].
#[inline]
pub fn enum_hash_with_payload(name_prefix_hash: u64, payload: &[Value]) -> u64 {
    let mut h = name_prefix_hash;
    for p in payload {
        h = fnv1a_combine(h, hash_value(p));
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heap::ProcHeap;

    fn test_heap() -> ProcHeap {
        ProcHeap::new()
    }

    #[test]
    fn immortal_flag_marks_only_frozen_objects() {
        use crate::frozen::FrozenArea;
        use std::sync::Arc;

        let mut h = test_heap();
        let mortal = Value::int_in(&mut h, i64::MAX); // BigInt box
        assert!(mortal.is_heap());
        assert!(!mortal.is_immortal());

        let area = Arc::new(FrozenArea::new());
        let mut b = area.builder();
        let frozen = b.int(i64::MAX).into_value();
        assert!(frozen.is_heap());
        assert!(frozen.is_immortal());
        // Tag/length still decode correctly with bit 7 set.
        assert_eq!(frozen.heap_tag(), Some(HeapTag::BigInt));
        assert_eq!(frozen.as_int(), Some(i64::MAX));

        assert!(!Value::small_int(7).is_immortal());
        assert!(!Value::nil().is_immortal());
    }

    #[test]
    fn roundtrip_small_int() {
        for i in [0, 1, -1, 42, -42, SMALL_INT_MAX, SMALL_INT_MIN] {
            let v = Value::small_int(i);
            assert_eq!(v.as_int(), Some(i));
            assert!(!v.is_heap());
            assert!(!v.is_float());
            assert_eq!(v.as_int_typed(), i);
        }
    }

    #[test]
    fn roundtrip_bigint_spills_to_arena() {
        let mut h = test_heap();
        for i in [SMALL_INT_MAX + 1, SMALL_INT_MIN - 1, i64::MAX, i64::MIN] {
            let v = Value::int_in(&mut h, i);
            assert_eq!(v.as_int(), Some(i));
            assert!(v.is_heap());
            assert!(v.is_int());
            assert_eq!(v.heap_tag(), Some(HeapTag::BigInt));
            match v.kind() {
                ValueView::Int(j) => assert_eq!(j, i),
                _ => panic!("expected Int view"),
            }
        }
        assert!(!Value::int_in(&mut h, 7).is_heap());
    }

    #[test]
    fn roundtrip_float() {
        for f in [0.0, 1.0, -1.5, 1e308, f64::MIN_POSITIVE] {
            let v = Value::float(f);
            assert_eq!(v.as_float(), Some(f));
            assert!(v.is_float());
        }
        assert_eq!(Value::float(f64::NAN).as_float(), Some(0.0));
        assert_eq!(Value::float(f64::INFINITY).as_float(), Some(0.0));
        assert_eq!(Value::float(f64::NEG_INFINITY).as_float(), Some(0.0));
    }

    #[test]
    fn roundtrip_bool_nil_socket() {
        assert_eq!(Value::bool(true).as_bool(), Some(true));
        assert_eq!(Value::bool(false).as_bool(), Some(false));
        assert!(!Value::bool(true).is_heap());
        assert!(Value::nil().is_nil());
        assert!(matches!(Value::nil().kind(), ValueView::Nil));

        let s = SocketValue {
            id: 7,
            is_listener: true,
        };
        let v = Value::socket(s);
        assert!(!v.is_heap(), "sockets are immediates");
        assert_eq!(v.as_socket(), Some(s));
        let c = SocketValue {
            id: -3,
            is_listener: false,
        };
        assert_eq!(Value::socket(c).as_socket(), Some(c));
        assert!(matches!(v.kind(), ValueView::Socket(x) if x == s));
    }

    #[test]
    fn roundtrip_str() {
        let mut h = test_heap();
        for s in ["", "a", "hello", "12345678", "123456789", "héllo wörld ☃"] {
            let v = Value::str_in(&mut h, s);
            assert_eq!(v.as_str(), Some(s));
            assert!(v.is_heap());
            assert!(v.as_int().is_none());
            assert!(matches!(v.kind(), ValueView::Str(x) if x == s));
        }
    }

    #[test]
    fn roundtrip_range_tuple_closure_enum() {
        let mut h = test_heap();
        let r = Value::range_in(&mut h, 2, 9);
        assert_eq!(r.as_range(), Some((2, 9)));

        let t = Value::tuple_in(&mut h, &[Value::small_int(7), Value::bool(true)]);
        let elems = t.as_tuple().unwrap();
        assert_eq!(elems.len(), 2);
        assert_eq!(elems[0].as_int(), Some(7));
        assert_eq!(elems[1].as_bool(), Some(true));
        assert!(Value::tuple_in(&mut h, &[]).as_tuple().unwrap().is_empty());

        let c = Value::closure_in(&mut h, 3, &[Value::small_int(1)]);
        let cr = c.as_closure().unwrap();
        assert_eq!(cr.func_idx(), 3);
        assert_eq!(cr.captures().len(), 1);
        assert_eq!(cr.captures()[0].as_int(), Some(1));

        let e = Value::enum_with_names_in(
            &mut h,
            TypeId(1),
            1,
            "Option",
            "Some",
            &["value"],
            &[Value::small_int(5)],
        );
        let er = e.as_enum().unwrap();
        assert_eq!(er.type_id(), TypeId(1));
        assert_eq!(er.variant_idx(), 1);
        assert_eq!(er.enum_name(), "Option");
        assert_eq!(er.variant_name(), "Some");
        assert_eq!(er.payload().len(), 1);
        assert_eq!(er.payload()[0].as_int(), Some(5));
        assert_eq!(er.field_labels().len(), 1);
        assert_eq!(er.field_labels()[0].as_str(), Some("value"));
        let expect = enum_hash_with_payload(
            enum_name_prefix_hash("Option", "Some"),
            &[Value::small_int(5)],
        );
        assert_eq!(er.hash(), expect);
    }

    #[test]
    fn roundtrip_binary() {
        let mut h = test_heap();
        let v = Value::binary_in(&mut h, vec![1u8, 2, 3]);
        let b = v.as_binary().unwrap();
        assert_eq!(&*b.full_bytes(), &[1u8, 2, 3][..]);
        assert_eq!(b.bit_offset(), 0);
        assert_eq!(b.bit_len(), 24);

        let v2 = Value::binary_bits_in(&mut h, vec![0xFF], 5);
        assert_eq!(v2.as_binary().unwrap().bit_len(), 5);
        assert_eq!(v2.as_binary().unwrap().to_aligned_vec(), vec![0xF8]);
    }

    #[test]
    fn binary_concat_windows() {
        let mut h = test_heap();
        let v = Value::binary_concat_parts_in(&mut h, &[&[0xAB, 0xCD], &[0xEF]], 24);
        let b = v.as_binary().unwrap();
        assert_eq!(&*b.full_bytes(), &[0xAB, 0xCD, 0xEF][..]);
        assert_eq!(b.bit_offset(), 0);
        let v2 = Value::binary_concat_parts_in(&mut h, &[&[0xAB], &[0xFF]], 11);
        let b2 = v2.as_binary().unwrap();
        assert_eq!(b2.bit_len(), 11);
        assert_eq!(b2.to_aligned_vec(), vec![0xAB, 0b1110_0000]);
        let v3 = Value::binary_concat_parts_in(&mut h, &[&[], &[]], 0);
        assert_eq!(v3.as_binary().unwrap().bit_len(), 0);
    }

    #[test]
    fn binary_views_share_backing_zero_copy() {
        let mut h = test_heap();
        let backing: Arc<[u8]> = Arc::from(vec![0xAB, 0xCD, 0xEF, 0x01]);
        let whole = Value::binary_from_arc_in(&mut h, Arc::clone(&backing), 32);
        assert_eq!(Arc::strong_count(&backing), 2);
        let slice = Value::binary_view_in(&mut h, whole.as_binary().unwrap().backing_arc(), 8, 16);
        assert_eq!(Arc::strong_count(&backing), 3);
        let s = slice.as_binary().unwrap();
        assert_eq!(&*s.full_bytes(), &[0xCD, 0xEF][..]);
        let same = Value::binary_in(&mut h, vec![0xCD, 0xEF]);
        assert!(s.bits_eq(&same.as_binary().unwrap()));
        assert!(!s.bits_eq(&whole.as_binary().unwrap()));
        assert_eq!(
            hash_value(&slice),
            hash_value(&same),
            "equal logical bits must hash identically"
        );
        assert!(whole.as_binary().unwrap().starts_with_at(8, &s));
        assert!(!whole.as_binary().unwrap().starts_with_at(32, &s));
    }

    #[test]
    fn binary_arc_backing_is_shared_and_released() {
        let mut h = test_heap();
        let backing: Arc<[u8]> = Arc::from(vec![9u8; 16]);
        let b1 = Value::binary_from_arc_in(&mut h, Arc::clone(&backing), 128);
        assert_eq!(Arc::strong_count(&backing), 2);
        let a1 = b1.object_addr().unwrap();
        // SAFETY: `a1` is a live Binary box.
        unsafe {
            assert!(header_has_off_heap_link(*(a1 as *const u64)));
            binary_clone_backing(a1 as *const u64);
            assert_eq!(Arc::strong_count(&backing), 3);
            binary_drop_backing(a1 as *const u64);
            assert_eq!(Arc::strong_count(&backing), 2);
        }
        // `free_object` runs `binary_drop_backing` at zero.
        drop(b1);
        assert_eq!(Arc::strong_count(&backing), 1);
    }

    #[test]
    fn header_roundtrip() {
        let h = pack_header(HeapTag::Enum, 9, false);
        assert_eq!(header_tag(h), HeapTag::Enum);
        assert_eq!(header_payload_words(h), 9);
        assert_eq!(header_total_words(h), 10);
        assert!(!header_has_off_heap_link(h));

        let hb = pack_header(HeapTag::Binary, 5, true);
        assert!(header_has_off_heap_link(hb));
        assert_eq!(header_tag(hb), HeapTag::Binary);
    }

    /// The Perceus in-place contract. Exercised through the enum constructor
    /// because that is the only reachable reuse path: `lower` pairs
    /// `Drop`/`Reuse` only for user-declared constructors.
    #[test]
    fn perceus_reuse_overwrites_in_place_and_releases_old_children() {
        let mut h = test_heap();
        let a = Value::str_in(&mut h, "old-a");
        let b = Value::str_in(&mut h, "old-b");
        let mut old =
            Value::enum_with_names_in(&mut h, TypeId(0), 0, "E", "V", &["x", "y"], &[a, b]);
        let addr = old.object_addr().unwrap();
        assert!(old.is_unique());

        let _ = take_freed_objects();
        old.hollow_for_reuse();
        let freed = take_freed_objects();
        assert!(
            freed >= 2,
            "hollowing must release the payload children, freed {freed}"
        );

        let en = Value::str_in(&mut h, "E");
        let vn = Value::str_in(&mut h, "V");
        let labels = Value::tuple_in(&mut h, &[]);
        let payload = [Value::small_int(7), Value::small_int(8)];
        let reuse = old.into_reuse_addr();
        let new = Value::enum_reuse_in(&mut h, reuse, TypeId(0), 0, 0, en, vn, labels, &payload);
        assert_eq!(new.object_addr().unwrap(), addr, "same allocation reused");
        assert!(new.is_unique(), "rc stays 1 across reuse");

        let en2 = Value::str_in(&mut h, "E");
        let vn2 = Value::str_in(&mut h, "V");
        let labels2 = Value::tuple_in(&mut h, &[]);
        let fresh = Value::enum_reuse_in(
            &mut h,
            Value::nil().into_reuse_addr(),
            TypeId(0),
            0,
            0,
            en2,
            vn2,
            labels2,
            &payload,
        );
        assert_ne!(fresh.object_addr().unwrap(), addr);
    }

    /// The `Env` view equals a HAMT holding the same entries, and hashes
    /// the same.
    #[test]
    fn env_map_equals_hamt_with_same_entries() {
        use crate::bytecode::hamt;

        let mut h = test_heap();
        let env = Value::env_map_in(&mut h);
        let mut m = hamt::empty(&mut h);
        for (k, v) in std::env::vars_os() {
            let (Ok(k), Ok(v)) = (k.into_string(), v.into_string()) else {
                continue;
            };
            let kv = Value::str_in(&mut h, &k);
            let vv = Value::str_in(&mut h, &v);
            let hash = hash_value(&kv);
            m = hamt::insert(&mut h, &m, kv, vv, hash);
        }
        assert!(values_equal(&env, &m), "env view == same-entry HAMT");
        assert!(values_equal(&m, &env), "and symmetrically");
        assert_eq!(
            hash_value(&env),
            hash_value(&m),
            "equal values hash identically"
        );

        let kv = Value::str_in(&mut h, "__al_env_eq_test_key__");
        let vv = Value::str_in(&mut h, "x");
        let hash = hash_value(&kv);
        let m2 = hamt::insert(&mut h, &m, kv, vv, hash);
        assert!(!values_equal(&env, &m2));
        assert!(!values_equal(&m2, &env));
    }

    /// Guards the worklist in [`values_equal`]: recursing through enum
    /// payloads overflowed the native stack at this depth.
    #[test]
    fn values_equal_deep_enum_chain_is_iterative() {
        let mut h = test_heap();
        let deep = |h: &mut ProcHeap, last: i64| {
            let mut v = Value::nil();
            for i in 0..100_000i64 {
                let head = if i == 99_999 { last } else { i };
                v = Value::enum_with_names_in(
                    h,
                    TypeId(7),
                    0,
                    "List",
                    "Cons",
                    &["head", "tail"],
                    &[Value::small_int(head), v],
                );
            }
            v
        };
        let a = deep(&mut h, 99_999);
        let b = deep(&mut h, 99_999);
        assert!(values_equal(&a, &b), "equal deep chains compare equal");
        assert_eq!(hash_value(&a), hash_value(&b));
        let c = deep(&mut h, -1);
        assert!(!values_equal(&a, &c));
    }

    /// Guards the map arm of the worklist: `{k: {k: …}}` nested this deep
    /// must compare without a native frame per level.
    #[test]
    fn values_equal_deep_map_nesting_is_iterative() {
        use crate::bytecode::hamt;

        let mut h = test_heap();
        let deep = |h: &mut ProcHeap, innermost: i64| {
            let mut v = Value::small_int(innermost);
            for _ in 0..100_000 {
                let k = Value::str_in(h, "k");
                let kh = hash_value(&k);
                let empty = hamt::empty(h);
                v = hamt::insert(h, &empty, k, v, kh);
            }
            v
        };
        let a = deep(&mut h, 1);
        let b = deep(&mut h, 1);
        assert!(values_equal(&a, &b), "equal deep map nests compare equal");
        let c = deep(&mut h, 2);
        assert!(!values_equal(&a, &c));
    }

    /// A reuse token that never reaches a constructor frees its hollow cell
    /// instead of leaking it.
    #[test]
    fn unconsumed_reuse_addr_frees_its_cell_on_drop() {
        let mut h = test_heap();
        let a = Value::str_in(&mut h, "payload");
        let mut old = Value::enum_with_names_in(&mut h, TypeId(0), 0, "E", "V", &["x"], &[a]);
        old.hollow_for_reuse();
        let reuse = old.into_reuse_addr();
        let _ = take_freed_objects();
        drop(reuse);
        assert_eq!(
            take_freed_objects(),
            1,
            "dropping an unconsumed token frees the hollow cell"
        );
        drop(ReuseAddr::none());
    }

    #[test]
    fn for_each_child_visits_exactly_the_value_slots() {
        let mut h = test_heap();
        let s = Value::str_in(&mut h, "elem");
        let t = Value::tuple_in(&mut h, &[s.clone(), Value::small_int(1), Value::nil()]);
        let mut seen = Vec::new();
        unsafe {
            for_each_child(t.object_addr().unwrap() as *mut u64, &mut |v| {
                seen.push(v.to_bits())
            });
        }
        assert_eq!(
            seen,
            vec![
                s.to_bits(),
                Value::small_int(1).to_bits(),
                Value::nil().to_bits()
            ]
        );

        // Names, labels and payload are traced; type_id/hash/count are not.
        let e = Value::enum_with_names_in(
            &mut h,
            TypeId(2),
            0,
            "E",
            "V",
            &["a"],
            std::slice::from_ref(&s),
        );
        let mut count = 0;
        unsafe {
            for_each_child(e.object_addr().unwrap() as *mut u64, &mut |_| count += 1);
        }
        assert_eq!(count, 3 + 1, "enum_name, variant_name, labels, payload[0]");

        // No traced children: the Arc words must never be visited.
        let b = Value::binary_in(&mut h, vec![1, 2, 3]);
        let mut none = 0;
        unsafe {
            for_each_child(b.object_addr().unwrap() as *mut u64, &mut |_| none += 1);
        }
        assert_eq!(none, 0);

        let mut none = 0;
        unsafe {
            for_each_child(s.object_addr().unwrap() as *mut u64, &mut |_| none += 1);
        }
        assert_eq!(none, 0);
    }

    #[test]
    fn size_is_one_word() {
        assert_eq!(std::mem::size_of::<Value>(), 8);
        let mut h = test_heap();
        let v = Value::str_in(&mut h, "copy");
        let w = v.clone();
        assert_eq!(v.as_str(), Some("copy"));
        assert_eq!(w.as_str(), Some("copy"));
    }

    #[test]
    fn values_into_frozen_area_read_back() {
        let area = Arc::new(crate::frozen::FrozenArea::new());
        let mut b = area.builder();
        let s = Value::str_in(&mut b, "frozen constant");
        let t = Value::tuple_in(&mut b, &[s.clone(), Value::small_int(3)]);
        let arr = Value::array_in(&mut b, &[s.clone(), t.clone()]);
        assert!(area.contains(s.object_addr().unwrap() as *const u64));
        assert_eq!(s.as_str(), Some("frozen constant"));
        assert_eq!(t.as_tuple().unwrap()[1].as_int(), Some(3));
        let a = arr.as_array().unwrap();
        assert_eq!(a.len(), 2);
        assert_eq!(a.get(1).unwrap().as_tuple().unwrap().len(), 2);
    }

    fn ints(n: usize) -> Vec<Value> {
        (0..n as i64).map(Value::small_int).collect()
    }

    fn assert_matches_model(root: &Value, model: &[Value]) {
        seq::check_invariants(root);
        let r = root.as_array().unwrap();
        assert_eq!(r.len(), model.len());
        for (i, m) in model.iter().enumerate() {
            assert_eq!(
                r.get(i).unwrap().to_bits(),
                m.to_bits(),
                "element {i} of {} disagrees",
                model.len()
            );
        }
        let collected: Vec<u64> = r.iter().map(|v| v.to_bits()).collect();
        let expect: Vec<u64> = model.iter().map(|v| v.to_bits()).collect();
        assert_eq!(collected, expect, "iterator disagrees with get()");
        assert!(r.get(model.len()).is_none());
    }

    #[test]
    fn seq_from_slice_various_sizes() {
        let mut h = ProcHeap::new();
        for n in [0usize, 1, 31, 32, 33, 63, 64, 65, 1000, 1024, 1025, 4097] {
            let items = ints(n);
            let root = seq::from_slice(&mut h, &items);
            assert_matches_model(&root, &items);
        }
    }

    #[test]
    fn seq_push_back_and_front_grow_correctly() {
        let mut h = ProcHeap::new();
        let mut root = seq::empty_in(&mut h);
        let mut model: Vec<Value> = Vec::new();
        for i in 0..1200i64 {
            root = seq::push_back(&mut h, &root, Value::small_int(i));
            model.push(Value::small_int(i));
        }
        assert_matches_model(&root, &model);

        for i in 0..300i64 {
            root = seq::push_front(&mut h, &root, Value::small_int(-i));
            model.insert(0, Value::small_int(-i));
        }
        assert_matches_model(&root, &model);
    }

    #[test]
    fn seq_pop_front_drains_everything() {
        let mut h = ProcHeap::new();
        let items = ints(1100);
        let mut root = seq::from_slice(&mut h, &items);
        let mut model: Vec<Value> = items;
        while !model.is_empty() {
            let (e, rest) = seq::pop_front(&mut h, &root).unwrap();
            let m = model.remove(0);
            assert_eq!(e.to_bits(), m.to_bits());
            root = rest;
            if model.len().is_multiple_of(97) {
                assert_matches_model(&root, &model);
            }
        }
        assert!(seq::pop_front(&mut h, &root).is_none());
    }

    #[test]
    fn seq_update_replaces_in_place_persistently() {
        let mut h = ProcHeap::new();
        let items = ints(1100);
        let root = seq::from_slice(&mut h, &items);
        let marker = Value::small_int(-777);
        for i in [0usize, 1, 31, 32, 500, 1063, 1064, 1099] {
            let updated = seq::update(&mut h, &root, i, marker.clone()).unwrap();
            let mut model: Vec<Value> = items.clone();
            model[i] = marker.clone();
            assert_matches_model(&updated, &model);
            assert_eq!(seq::get(&root, i).unwrap().as_int(), Some(i as i64));
        }
        assert!(seq::update(&mut h, &root, 1100, marker.clone()).is_none());
    }

    #[test]
    fn seq_take_skip_match_slices() {
        let mut h = ProcHeap::new();
        let items = ints(1100);
        let root = seq::from_slice(&mut h, &items);
        for n in [0usize, 1, 15, 32, 33, 64, 500, 1063, 1064, 1099, 1100, 2000] {
            let t = seq::take(&mut h, &root, n);
            let s = seq::skip(&mut h, &root, n);
            let cut = n.min(items.len());
            assert_matches_model(&t, &items[..cut]);
            assert_matches_model(&s, &items[cut..]);
        }
        let mut pushed = root;
        for i in 0..40i64 {
            pushed = seq::push_front(&mut h, &pushed, Value::small_int(-1 - i));
        }
        let mut model: Vec<Value> = (0..40i64).map(|i| Value::small_int(-40 + i)).collect();
        model.extend_from_slice(&items);
        for n in [5usize, 39, 40, 41, 600] {
            assert_matches_model(&seq::take(&mut h, &pushed, n), &model[..n]);
            assert_matches_model(&seq::skip(&mut h, &pushed, n), &model[n..]);
        }
    }

    #[test]
    fn seq_concat_matches_model_and_stays_shallow() {
        let mut h = ProcHeap::new();
        for (la, lb) in [
            (0usize, 5usize),
            (5, 0),
            (3, 4),
            (32, 32),
            (33, 100),
            (1000, 1),
            (1, 1000),
            (500, 500),
        ] {
            let xs = ints(la);
            let ys: Vec<Value> = (0..lb as i64)
                .map(|i| Value::small_int(10_000 + i))
                .collect();
            let l = seq::from_slice(&mut h, &xs);
            let r = seq::from_slice(&mut h, &ys);
            let joined = seq::concat(&mut h, &l, &r);
            let mut model = xs.clone();
            model.extend_from_slice(&ys);
            assert_matches_model(&joined, &model);
        }

        // Repeated concatenation must keep the tree shallow (the RRB
        // rebalancing invariant).
        let mut h = ProcHeap::new();
        let piece_items = ints(7);
        let mut root = seq::empty_in(&mut h);
        let mut model: Vec<Value> = Vec::new();
        for _ in 0..300 {
            let piece = seq::from_slice(&mut h, &piece_items);
            root = seq::concat(&mut h, &root, &piece);
            model.extend_from_slice(&piece_items);
        }
        assert_matches_model(&root, &model);
        // 2100 elements: a balanced 32-ary tree is depth 3, and E_MAX slack
        // buys a couple of levels, no more.
        let root_obj = root.object_addr().unwrap() as *const u64;
        let shift = unsafe { *root_obj.add(2) } as usize;
        assert!(
            shift <= 25,
            "repeated concat degraded the tree: shift {shift}"
        );
    }

    #[test]
    fn seq_randomized_ops_match_vec_model() {
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut rng = move || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as usize
        };
        let mut h = ProcHeap::new();
        let mut model: Vec<Value> = Vec::new();
        let mut root = seq::empty_in(&mut h);
        for step in 0..600 {
            // Rebuild into a fresh arena periodically. The old root must be
            // dropped BEFORE its heap: a heap has to outlive the values
            // pointing into it. `model` holds only immediates.
            if step % 64 == 63 {
                drop(std::mem::replace(&mut root, Value::nil()));
                h = ProcHeap::new();
                root = seq::from_slice(&mut h, &model);
            }
            match rng() % 8 {
                0 | 1 => {
                    let x = Value::small_int(step as i64);
                    root = seq::push_back(&mut h, &root, x.clone());
                    model.push(x);
                }
                2 => {
                    let x = Value::small_int(-(step as i64));
                    root = seq::push_front(&mut h, &root, x.clone());
                    model.insert(0, x);
                }
                3 => {
                    let popped = seq::pop_front(&mut h, &root);
                    if model.is_empty() {
                        assert!(popped.is_none());
                    } else {
                        let (e, rest) = popped.unwrap();
                        assert_eq!(e.to_bits(), model.remove(0).to_bits());
                        root = rest;
                    }
                }
                4 => {
                    if !model.is_empty() {
                        let i = rng() % model.len();
                        let x = Value::small_int(9000 + step as i64);
                        root = seq::update(&mut h, &root, i, x.clone()).unwrap();
                        model[i] = x;
                    }
                }
                5 => {
                    let n = rng() % (model.len() + 2);
                    root = seq::take(&mut h, &root, n);
                    model.truncate(n.min(model.len()));
                }
                6 => {
                    let n = rng() % (model.len() + 2);
                    root = seq::skip(&mut h, &root, n);
                    let cut = n.min(model.len());
                    model.drain(..cut);
                }
                _ => {
                    let n = rng() % 60;
                    let extra: Vec<Value> = (0..n as i64)
                        .map(|i| Value::small_int(100_000 + i))
                        .collect();
                    let other = seq::from_slice(&mut h, &extra);
                    if rng() % 2 == 0 {
                        root = seq::concat(&mut h, &root, &other);
                        model.extend_from_slice(&extra);
                    } else {
                        root = seq::concat(&mut h, &other, &root);
                        let mut m = extra;
                        m.extend_from_slice(&model);
                        model = m;
                    }
                }
            }
            seq::check_invariants(&root);
            if step % 13 == 0 {
                assert_matches_model(&root, &model);
            }
        }
        assert_matches_model(&root, &model);
    }

    #[test]
    fn seq_structural_sharing_on_push() {
        let mut h = ProcHeap::new();
        let items = ints(100_000);
        let root = seq::from_slice(&mut h, &items);
        let v2 = seq::push_back(&mut h, &root, Value::small_int(-1));
        assert_eq!(seq::len(&root), 100_000, "original version unchanged");
        assert_eq!(seq::len(&v2), 100_001);
    }

    #[test]
    fn hash_stable_across_int_repr() {
        let mut h = test_heap();
        let small = hash_value(&Value::small_int(5));
        assert_eq!(small, fnv1a_combine(HASH_BASIS, 5u64));
        let big = Value::int_in(&mut h, i64::MAX);
        assert!(big.is_heap());
        assert_eq!(hash_value(&big), fnv1a_combine(HASH_BASIS, i64::MAX as u64));
    }

    // Otherwise the precomputed enum hash fast-rejects equal values like
    // `Some(0..3) == Some([0, 1, 2])`.
    #[test]
    fn range_hashes_like_its_materialized_array() {
        let mut h = test_heap();
        for (s, e) in [(0i64, 0i64), (5, 5), (0, 1), (0, 3), (-3, 4), (10, 100)] {
            let items: Vec<Value> = (s..e).map(Value::small_int).collect();
            let arr = Value::array_in(&mut h, &items);
            let range = Value::range_in(&mut h, s, e);
            assert_eq!(
                hash_value(&range),
                hash_value(&arr),
                "range {s}..{e} must hash like its materialised array"
            );
        }
        let r = Value::range_in(&mut h, 0, 4);
        let nested_range = Value::array_in(&mut h, &[r]);
        let inner: Vec<Value> = (0..4).map(Value::small_int).collect();
        let inner_arr = Value::array_in(&mut h, &inner);
        let nested_arr = Value::array_in(&mut h, &[inner_arr]);
        assert_eq!(hash_value(&nested_range), hash_value(&nested_arr));
    }

    // Regression: `+0.0` and `-0.0` are `values_equal` but differ in bits.
    #[test]
    fn signed_zero_hashes_equal() {
        assert_eq!(
            hash_value(&Value::float(0.0)),
            hash_value(&Value::float(-0.0)),
            "+0.0 and -0.0 are values_equal, so they must hash identically"
        );
        let prefix = enum_name_prefix_hash("Option", "Some");
        assert_eq!(
            enum_hash_with_payload(prefix, &[Value::float(0.0)]),
            enum_hash_with_payload(prefix, &[Value::float(-0.0)]),
            "Some(0.0) and Some(-0.0) must share a payload hash"
        );
        assert_ne!(
            hash_value(&Value::float(0.0)),
            hash_value(&Value::float(1.0))
        );
    }

    // Regression: hashing a range must not iterate it. Reaching the asserts
    // at all proves it.
    #[test]
    fn hashing_a_huge_range_is_constant_time() {
        let mut h = test_heap();
        let huge = hash_value(&Value::range_in(&mut h, 0, i64::MAX));
        assert_ne!(huge, hash_value(&Value::range_in(&mut h, 0, i64::MAX - 1)));
        let empty_range = Value::range_in(&mut h, i64::MAX, i64::MAX);
        let empty_arr = Value::array_in(&mut h, &[]);
        assert_eq!(hash_value(&empty_range), hash_value(&empty_arr));
    }

    // The sampled Str/Binary hash must still reject payloads differing only
    // outside the prefix sample, since length and tail are folded in too.
    #[test]
    fn sampled_hash_rejects_tail_and_length_differences() {
        let mut h = test_heap();
        let long = "x".repeat(10 * BYTES_HASH_SAMPLE);
        let sa = Value::str_in(&mut h, &(long.clone() + "a"));
        let sb = Value::str_in(&mut h, &(long.clone() + "b"));
        let sc = Value::str_in(&mut h, &(long.clone() + "aa"));
        assert_ne!(hash_value(&sa), hash_value(&sb));
        assert_ne!(hash_value(&sa), hash_value(&sc));

        let bytes: Vec<u8> = (0..10 * BYTES_HASH_SAMPLE).map(|i| i as u8).collect();
        let mut flipped = bytes.clone();
        *flipped.last_mut().unwrap() ^= 0xFF;
        let ba = Value::binary_in(&mut h, bytes.clone());
        let bb = Value::binary_in(&mut h, flipped);
        let mut extended = bytes.clone();
        extended.push(0);
        let bc = Value::binary_in(&mut h, extended);
        assert_ne!(hash_value(&ba), hash_value(&bb));
        assert_ne!(hash_value(&ba), hash_value(&bc));
    }

    // Aligned and bit-unaligned views of the same logical bits are `bits_eq`,
    // so they must hash identically — sampled or not, partial tail or not.
    #[test]
    fn unaligned_binary_hashes_like_aligned() {
        let mut h = test_heap();
        for (len, dropped_bits) in [(3usize, 0u64), (16, 3), (300, 0), (300, 5)] {
            let bytes: Vec<u8> = (0..len).map(|i| (i * 37 + 11) as u8).collect();
            let bit_len = (len as u64) * 8 - dropped_bits;
            let aligned = Value::binary_bits_in(&mut h, bytes.clone(), bit_len);
            for shift in 1u32..8 {
                // The same bit stream shifted right, viewed at bit_offset
                // = shift: identical logical bits.
                let mut backing = vec![0u8; len + 1];
                for (i, &b) in bytes.iter().enumerate() {
                    backing[i] |= b >> shift;
                    backing[i + 1] |= b << (8 - shift);
                }
                let unaligned =
                    Value::binary_view_in(&mut h, Arc::from(backing), shift as u64, bit_len);
                let (a, u) = (aligned.as_binary().unwrap(), unaligned.as_binary().unwrap());
                assert!(
                    a.bits_eq(&u),
                    "len {len} -{dropped_bits} bits, shift {shift}"
                );
                assert_eq!(
                    hash_value(&aligned),
                    hash_value(&unaligned),
                    "bits_eq views must hash identically (len {len}, -{dropped_bits} bits, shift {shift})"
                );
            }
        }
    }
}
