//! NaN-boxed runtime value.
//!
//! `Value` is an 8-byte word. A regular IEEE-754 `f64` whose bit pattern is
//! not a quiet-NaN is stored verbatim — `Value::float` clamps every non-finite
//! input to `0.0`, so a real NaN never enters the box and the entire qNaN
//! space is free for tagging. Tagged values set the qNaN bits and use the top
//! 16 bits as a discriminant header; the low 48 bits carry the payload (a
//! sign-extended small integer, a bool, or an `Rc<HeapValue>` pointer — user-
//! space pointers fit in 48 bits on every supported platform). Integers
//! outside the 48-bit signed range spill to `HeapValue::BigInt` so the full
//! `i64` domain is preserved.
//!
//! All `unsafe` is confined to this file: the heap-pointer pack/unpack and the
//! manual `Clone`/`Drop` refcount maintenance.

use std::fmt;
use std::rc::Rc;
use std::sync::Arc;

/// Quiet-NaN signature: exponent all-ones, top mantissa bit set. Any word with
/// these bits set is a tagged value, never a real float.
const QNAN: u64 = 0x7FF8_0000_0000_0000;
const SIGN: u64 = 0x8000_0000_0000_0000;
/// Low 48 bits: small-int value, bool, or heap pointer.
const PAYLOAD: u64 = 0x0000_FFFF_FFFF_FFFF;

/// Top-16-bit headers for non-float values. All include `QNAN` so the
/// is-float check is a single mask-and-compare.
const HDR_MASK: u64 = 0xFFFF_0000_0000_0000;
const HDR_INT: u64 = QNAN | 0x0001_0000_0000_0000;
const HDR_BOOL: u64 = QNAN | 0x0002_0000_0000_0000;
const HDR_NIL: u64 = QNAN | 0x0003_0000_0000_0000;
/// Heap pointer uses the sign bit so `(bits & (SIGN|QNAN)) == (SIGN|QNAN)`
/// is the heap test — disjoint from the sign-clear immediate headers above.
const HDR_PTR: u64 = SIGN | QNAN;

/// Inclusive bounds of the 48-bit signed small-int range.
const SMALL_INT_MIN: i64 = -(1i64 << 47);
const SMALL_INT_MAX: i64 = (1i64 << 47) - 1;

/// The `Array` backing store: a persistent RRB vector. The non-atomic `RcK`
/// pointer kind is mandatory — the crate-default `imbl::Vector` is `ArcK`
/// (atomic), which would turn every clone into an atomic RMW.
pub type Seq = imbl::GenericVector<Value, imbl::shared_ptr::RcK>;

#[repr(transparent)]
pub struct Value(u64);

/// Heap-backed payloads. Stored behind a single `Rc<HeapValue>` whose pointer
/// is packed into the NaN-box. The inner `Rc`s on `Str`/`Array`/`Binary`/
/// `Tuple`/`Closure`/`Enum` are kept so callers can clone the inner handle
/// cheaply (e.g. `pop_str -> Rc<str>`) and so `Rc::unwrap_or_clone` on the
/// inner array gives in-place mutation at refcount 1.
#[derive(Debug, Clone)]
pub enum HeapValue {
    Str(Rc<str>),
    Array(Rc<Seq>),
    Range(i64, i64),
    Binary(Rc<BinaryValue>),
    Tuple(Rc<TupleValue>),
    Closure(Rc<ClosureValue>),
    Enum(Rc<EnumValue>),
    Socket(SocketValue),
    /// `i64` outside the 48-bit small-int range.
    BigInt(i64),
}

/// Borrowed view for many-armed matches. `Int` collapses small-int and
/// `BigInt` so callers see a single integer arm.
#[derive(Debug)]
pub enum ValueView<'a> {
    Int(i64),
    Float(f64),
    Bool(bool),
    Nil,
    Heap(&'a HeapValue),
}

macro_rules! as_heap_variant {
    ($name:ident, $variant:ident, $ty:ty) => {
        #[inline]
        pub fn $name(&self) -> Option<&$ty> {
            match self.as_heap()? {
                HeapValue::$variant(x) => Some(x),
                _ => None,
            }
        }
    };
}

impl Value {
    // ---- classifiers --------------------------------------------------------

    #[inline(always)]
    pub fn is_float(&self) -> bool {
        (self.0 & QNAN) != QNAN
    }
    #[inline(always)]
    fn is_int(&self) -> bool {
        self.is_small_int() || (self.is_heap() && matches!(self.heap_ref(), HeapValue::BigInt(_)))
    }
    #[inline(always)]
    fn is_small_int(&self) -> bool {
        (self.0 & HDR_MASK) == HDR_INT
    }
    /// Sign-extend the low 48 bits; only meaningful when `is_small_int()` holds.
    #[inline(always)]
    fn small_int(&self) -> i64 {
        ((self.0 & PAYLOAD) as i64) << 16 >> 16
    }
    #[inline(always)]
    pub fn is_bool(&self) -> bool {
        (self.0 & HDR_MASK) == HDR_BOOL
    }
    #[inline(always)]
    pub fn is_heap(&self) -> bool {
        (self.0 & (SIGN | QNAN)) == (SIGN | QNAN)
    }
    // ---- raw heap pointer (private; all unsafe goes through these) ---------

    #[inline(always)]
    fn heap_ptr(&self) -> *const HeapValue {
        debug_assert!(self.is_heap());
        (self.0 & PAYLOAD) as usize as *const HeapValue
    }
    #[inline(always)]
    fn heap_ref(&self) -> &HeapValue {
        debug_assert!(self.is_heap());
        // SAFETY: `is_heap()` is checked by every public path; the pointer was
        // produced by `Rc::into_raw` in `heap()` and the `Clone`/`Drop` impls
        // keep the strong count > 0 for as long as a `Value` holding it lives.
        unsafe { &*self.heap_ptr() }
    }

    #[inline(always)]
    fn pack(rc: Rc<HeapValue>) -> Value {
        let ptr = Rc::into_raw(rc) as usize as u64;
        debug_assert!(ptr & !PAYLOAD == 0, "heap pointer exceeds 48 bits");
        Value(HDR_PTR | ptr)
    }

    #[inline(always)]
    fn heap(h: HeapValue) -> Value {
        Self::pack(Rc::new(h))
    }

    // ---- constructors ------------------------------------------------------

    #[inline(always)]
    pub fn int(i: i64) -> Value {
        if (SMALL_INT_MIN..=SMALL_INT_MAX).contains(&i) {
            Value(HDR_INT | (i as u64 & PAYLOAD))
        } else {
            Value::heap(HeapValue::BigInt(i))
        }
    }
    #[inline(always)]
    pub fn float(f: f64) -> Value {
        // AL has no NaN/Inf in its value space; collapse to 0.0 so a real NaN
        // never collides with the tag space.
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
    pub fn str(s: impl Into<Rc<str>>) -> Value {
        Value::heap(HeapValue::Str(s.into()))
    }
    #[inline]
    pub fn array(v: Vec<Value>) -> Value {
        Value::heap(HeapValue::Array(Rc::new(Seq::from(v))))
    }
    #[inline]
    pub fn array_seq(s: Seq) -> Value {
        Value::heap(HeapValue::Array(Rc::new(s)))
    }
    #[inline]
    pub fn range(start: i64, end: i64) -> Value {
        Value::heap(HeapValue::Range(start, end))
    }
    #[inline]
    pub fn binary(bytes: Vec<u8>) -> Value {
        let bit_len = (bytes.len() as u64) * 8;
        Value::binary_bits(bytes, bit_len)
    }
    #[inline]
    pub fn binary_bits(bytes: Vec<u8>, bit_len: u64) -> Value {
        debug_assert!(bit_len.div_ceil(8) as usize == bytes.len());
        Value::binary_from_arc(Arc::from(bytes), bit_len)
    }
    /// Build a whole-buffer binary (bit offset 0) from an already-shared backing
    /// with no byte copy. Used by `bytecode::transfer::value_in` so a binary
    /// handed off from another thread keeps sharing the same `Arc` allocation.
    #[inline]
    pub fn binary_from_arc(backing: Arc<[u8]>, bit_len: u64) -> Value {
        debug_assert!(bit_len.div_ceil(8) as usize == backing.len());
        Value::binary_view(backing, 0, bit_len)
    }
    /// Build a binary as a zero-copy sub-view `[bit_offset, bit_offset +
    /// bit_len)` into a shared backing buffer. `Op::BinSlice`/`Op::BinTake`
    /// produce slices in O(1) through this: the backing `Arc` is shared and only
    /// the offset and length change — no bytes are copied.
    #[inline]
    pub fn binary_view(backing: Arc<[u8]>, bit_offset: u64, bit_len: u64) -> Value {
        Value::heap(HeapValue::Binary(Rc::new(BinaryValue::view(
            backing, bit_offset, bit_len,
        ))))
    }
    #[inline]
    pub fn tuple(elements: Vec<Value>) -> Value {
        Value::heap(HeapValue::Tuple(Rc::new(TupleValue { elements })))
    }
    #[inline]
    pub fn closure(c: ClosureValue) -> Value {
        Value::heap(HeapValue::Closure(Rc::new(c)))
    }
    #[inline]
    pub fn closure_rc(c: Rc<ClosureValue>) -> Value {
        Value::heap(HeapValue::Closure(c))
    }
    #[inline]
    pub fn enum_(e: EnumValue) -> Value {
        Value::heap(HeapValue::Enum(Rc::new(e)))
    }
    #[inline]
    pub fn enum_rc(e: Rc<EnumValue>) -> Value {
        Value::heap(HeapValue::Enum(e))
    }
    #[inline]
    pub fn socket(s: SocketValue) -> Value {
        Value::heap(HeapValue::Socket(s))
    }

    // ---- accessors ---------------------------------------------------------

    #[inline(always)]
    pub fn as_int(&self) -> Option<i64> {
        if self.is_small_int() {
            Some(self.small_int())
        } else if let Some(HeapValue::BigInt(i)) = self.as_heap() {
            Some(*i)
        } else {
            None
        }
    }
    #[inline(always)]
    pub fn as_int_unchecked(&self) -> i64 {
        debug_assert!(self.is_int());
        if self.is_small_int() {
            self.small_int()
        } else {
            match self.heap_ref() {
                HeapValue::BigInt(i) => *i,
                _ => 0,
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
    #[inline(always)]
    pub fn as_float_unchecked(&self) -> f64 {
        debug_assert!(self.is_float());
        f64::from_bits(self.0)
    }
    #[inline(always)]
    pub fn as_bool(&self) -> Option<bool> {
        if self.is_bool() {
            Some(self.0 & 1 == 1)
        } else {
            None
        }
    }
    #[inline(always)]
    pub fn as_heap(&self) -> Option<&HeapValue> {
        if self.is_heap() {
            Some(self.heap_ref())
        } else {
            None
        }
    }

    as_heap_variant!(as_str, Str, Rc<str>);
    as_heap_variant!(as_array, Array, Rc<Seq>);
    as_heap_variant!(as_enum, Enum, Rc<EnumValue>);
    as_heap_variant!(as_closure, Closure, Rc<ClosureValue>);

    /// Recover the inner `Rc<HeapValue>` (consuming `self`) so callers can
    /// `Rc::try_unwrap` / `unwrap_or_clone` for in-place mutation.
    #[inline]
    pub fn into_heap(self) -> Option<Rc<HeapValue>> {
        if !self.is_heap() {
            return None;
        }
        let ptr = self.heap_ptr();
        std::mem::forget(self);
        // SAFETY: paired with the `Rc::into_raw` in `heap()`; `forget` skips
        // our `Drop` so this is the sole owner of that strong count.
        Some(unsafe { Rc::from_raw(ptr) })
    }

    /// Borrowed many-armed view. `BigInt` is folded into `Int` so callers see
    /// a single integer arm; every other heap variant is exposed as-is.
    #[inline]
    pub fn kind(&self) -> ValueView<'_> {
        if self.is_small_int() {
            ValueView::Int(self.small_int())
        } else if self.is_float() {
            ValueView::Float(f64::from_bits(self.0))
        } else if self.is_bool() {
            ValueView::Bool(self.0 & 1 == 1)
        } else if self.is_heap() {
            let h = self.heap_ref();
            match h {
                HeapValue::BigInt(i) => ValueView::Int(*i),
                _ => ValueView::Heap(h),
            }
        } else {
            debug_assert!(self.0 == HDR_NIL);
            ValueView::Nil
        }
    }
}

impl Clone for Value {
    #[inline(always)]
    fn clone(&self) -> Self {
        if self.is_heap() {
            // SAFETY: pointer originates from `Rc::into_raw`; bumping the
            // strong count here is balanced by the new `Value`'s `Drop`.
            unsafe { Rc::increment_strong_count(self.heap_ptr()) };
        }
        Value(self.0)
    }
}

impl Drop for Value {
    #[inline(always)]
    fn drop(&mut self) {
        if self.is_heap() {
            // SAFETY: reconstitute the `Rc` to drop one strong count;
            // balanced with `into_raw` in `heap()` / `increment` in `clone()`.
            unsafe { drop(Rc::from_raw(self.heap_ptr())) };
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
            ValueView::Heap(h) => fmt::Debug::fmt(h, f),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TupleValue {
    pub elements: Vec<Value>,
}

/// Bit-addressable binary data, stored as a zero-copy sub-view into a shared
/// backing buffer. The logical value is the `bit_len` bits beginning at
/// `bit_offset` within `backing`; bits outside `[bit_offset, bit_offset +
/// bit_len)` belong to other views over the same buffer and are NOT part of
/// this value. Addressing is MSB-first (Erlang/Gleam convention) — see
/// `vm::binary` for the bit-level operations.
///
/// `backing` is atomically reference-counted for two reasons: a binary can
/// cross the `scheduler.spawn` thread boundary by an `Arc` bump instead of a
/// byte copy (see `bytecode::transfer`), and `Op::BinSlice`/`Op::BinTake`
/// return sub-slices in O(1) by sharing the `Arc` and only adjusting
/// `bit_offset`/`bit_len`.
///
/// Because the buffer is shared, the trailing partial byte may carry bits that
/// belong to a neighbouring view, so — unlike a freshly materialised buffer —
/// those bits are NOT guaranteed zero. Equality ([`PartialEq`]) and hashing
/// ([`hash_value`]) are therefore defined over the LOGICAL bits only (full
/// bytes plus the masked partial tail): two views with different backings or
/// offsets but equal logical contents compare `==` and hash identically.
#[derive(Debug, Clone)]
pub struct BinaryValue {
    pub backing: Arc<[u8]>,
    pub bit_offset: u64,
    pub bit_len: u64,
}

/// Read bit `i` (MSB-first) from a byte buffer. Caller guarantees `i / 8` is in
/// bounds.
#[inline]
fn bit_at(backing: &[u8], i: u64) -> u8 {
    (backing[(i / 8) as usize] >> (7 - (i % 8))) & 1
}

impl BinaryValue {
    /// A zero-copy sub-view `[bit_offset, bit_offset + bit_len)` into `backing`.
    #[inline]
    pub fn view(backing: Arc<[u8]>, bit_offset: u64, bit_len: u64) -> BinaryValue {
        debug_assert!((bit_offset + bit_len).div_ceil(8) as usize <= backing.len());
        BinaryValue {
            backing,
            bit_offset,
            bit_len,
        }
    }

    /// The complete logical bytes as a borrowed slice. HOT path: valid only when
    /// the view is fully byte-aligned (`bit_offset` and `bit_len` both multiples
    /// of 8), which every socket/file write guarantees by rejecting non-aligned
    /// binaries first. Returns exactly `bit_len / 8` bytes with no copy.
    #[inline]
    pub fn byte_window(&self) -> &[u8] {
        debug_assert!(
            self.bit_offset.is_multiple_of(8) && self.bit_len.is_multiple_of(8),
            "byte_window on a non-byte-aligned binary view"
        );
        let start = (self.bit_offset / 8) as usize;
        let len = (self.bit_len / 8) as usize;
        &self.backing[start..start + len]
    }

    /// The complete logical bytes (the first `bit_len / 8` bytes; any partial
    /// trailing byte is excluded). Borrows the backing with no copy when the
    /// view starts on a byte boundary — the common case — and otherwise
    /// re-aligns the bits into a fresh buffer. Entry point for every host-side
    /// byte consumer (socket/file writes and the ASCII `binary.*` builtins).
    #[inline]
    pub fn full_bytes(&self) -> std::borrow::Cow<'_, [u8]> {
        let full = (self.bit_len / 8) as usize;
        if self.bit_offset.is_multiple_of(8) {
            let start = (self.bit_offset / 8) as usize;
            std::borrow::Cow::Borrowed(&self.backing[start..start + full])
        } else {
            let mut v = self.to_aligned_vec();
            v.truncate(full);
            std::borrow::Cow::Owned(v)
        }
    }

    /// Materialise the logical bits into a fresh, bit-offset-0 buffer of
    /// `bit_len.div_ceil(8)` bytes with the trailing partial byte masked to zero
    /// (re-establishing the owned-buffer invariant). COLD path — used for
    /// bit-unaligned views and by `binary.append`/`binary.to_string`/`inspect`.
    pub fn to_aligned_vec(&self) -> Vec<u8> {
        let out_bytes = self.bit_len.div_ceil(8) as usize;
        let mut out = vec![0u8; out_bytes];
        if self.bit_offset.is_multiple_of(8) {
            let start = (self.bit_offset / 8) as usize;
            let copy = out_bytes.min(self.backing.len() - start);
            out[..copy].copy_from_slice(&self.backing[start..start + copy]);
        } else {
            for i in 0..self.bit_len {
                let bit = bit_at(&self.backing, self.bit_offset + i);
                out[(i / 8) as usize] |= bit << (7 - (i % 8));
            }
        }
        let rem = (self.bit_len % 8) as u32;
        if rem != 0
            && let Some(last) = out.last_mut()
        {
            *last &= 0xFFu8 << (8 - rem);
        }
        out
    }

    /// An owned, bit-offset-0 copy of the logical bits. Alias for
    /// [`to_aligned_vec`](Self::to_aligned_vec).
    #[inline]
    pub fn materialize(&self) -> Vec<u8> {
        self.to_aligned_vec()
    }
}

impl PartialEq for BinaryValue {
    fn eq(&self, other: &Self) -> bool {
        if self.bit_len != other.bit_len {
            return false;
        }
        // Both views start on a byte boundary (the overwhelmingly common case):
        // compare full bytes directly, then the masked partial tail — no alloc.
        if self.bit_offset.is_multiple_of(8) && other.bit_offset.is_multiple_of(8) {
            let full = (self.bit_len / 8) as usize;
            let s = (self.bit_offset / 8) as usize;
            let o = (other.bit_offset / 8) as usize;
            if self.backing[s..s + full] != other.backing[o..o + full] {
                return false;
            }
            let rem = (self.bit_len % 8) as u32;
            if rem == 0 {
                return true;
            }
            let mask = 0xFFu8 << (8 - rem);
            return (self.backing[s + full] & mask) == (other.backing[o + full] & mask);
        }
        // At least one view is bit-unaligned: compare bit by bit.
        (0..self.bit_len).all(|i| {
            bit_at(&self.backing, self.bit_offset + i)
                == bit_at(&other.backing, other.bit_offset + i)
        })
    }
}

impl Eq for BinaryValue {}

#[derive(Debug, Clone, Copy)]
pub struct SocketValue {
    pub id: i32,
    pub is_listener: bool,
}

/// The universal tagged runtime value. Covers Nil/Option/Result and all
/// user-defined custom types. `field_labels` is parallel to `payload` and
/// allows position-independent `.field` access; it is empty for nullary
/// constructors.
///
/// `enum_name`/`variant_name`/`field_labels` are statically constant per
/// variant, so they are stored behind `Rc` (shared with the bytecode constant
/// pool). Cloning an `EnumValue` — done on every `Some`/`Ok`/`Err`
/// construction via the prelude templates, including every array index — is
/// then a handful of refcount bumps instead of re-allocating the invariant
/// name/label strings.
#[derive(Debug, Clone)]
pub struct EnumValue {
    pub type_id: i32,
    pub enum_name: Rc<str>,
    pub variant_name: Rc<str>,
    pub field_labels: Rc<[Rc<str>]>,
    pub payload: Vec<Value>,
    pub hash: u64,
}

#[derive(Debug, Clone)]
pub struct ClosureValue {
    pub func_idx: i32,
    // `Rc<[Value]>` (not `Vec<Value>`): a closure is created once but invoked
    // many times (once per element when passed to a HOF like `list.map`), and
    // every `Op::Call`/`Op::TailCall`/`Op::CallSelf` clones the captures into
    // the new call frame. An `Rc` clone is a refcount bump; a `Vec` clone is an
    // O(k) heap alloc+copy. The single allocation happens once at `MakeClosure`.
    pub captures: Rc<[Value]>,
    pub name: Rc<str>,
}

const HASH_BASIS: u64 = 0xcbf29ce484222325;

#[inline]
fn fnv1a_combine(h: u64, val: u64) -> u64 {
    (h ^ val).wrapping_mul(0x100000001b3)
}

/// Number of leading elements folded into a sequence hash. The stored hash is a
/// fast-reject filter for value equality, not a cryptographic digest, so
/// sampling a bounded prefix (plus the length) keeps hashing O(1) for a compact
/// `HeapValue::Range` — `Some(0..n)` must not walk the whole range at build
/// time — while staying collision-resistant enough to reject mismatched
/// payloads cheaply.
const SEQ_HASH_SAMPLE: usize = 32;

/// Hash a sequence from its length and a bounded prefix of its element hashes.
/// A `Range(s, e)` and the array it materialises to (`[s, s+1, …, e-1]`) have
/// the same length and the same leading elements, so they hash identically.
/// That keeps the `Range == Array` equivalence in `values_equal` consistent
/// with the precomputed enum hash without ever iterating the full (possibly
/// enormous) range.
#[inline]
fn hash_sequence(len: usize, elems: impl Iterator<Item = Value>) -> u64 {
    let mut h = fnv1a_combine(HASH_BASIS, len as u64);
    for item in elems.take(SEQ_HASH_SAMPLE) {
        h = fnv1a_combine(h, hash_value(&item));
    }
    h
}

pub fn hash_value(v: &Value) -> u64 {
    let mut h = HASH_BASIS;
    match v.kind() {
        ValueView::Int(i) => {
            h = fnv1a_combine(h, i as u64);
        }
        ValueView::Float(f) => {
            // `+0.0` and `-0.0` are `values_equal` but have distinct bit
            // patterns. The enum-equality arm gates on the cached payload hash
            // (`ae.hash == be.hash` as a necessary precondition), so hashing a
            // signed zero by its raw bits would fast-reject e.g.
            // `Some(0.0) == Some(-0.0)` even though they are structurally equal.
            // Normalise signed zero so the hash respects `values_equal`.
            let bits = if f == 0.0 { 0 } else { f.to_bits() };
            h = fnv1a_combine(h, bits);
        }
        ValueView::Bool(b) => {
            h = fnv1a_combine(h, if b { 1 } else { 0 });
        }
        ValueView::Heap(HeapValue::Str(s)) => {
            for c in s.bytes() {
                h = fnv1a_combine(h, c as u64);
            }
        }
        ValueView::Heap(HeapValue::Enum(e)) => {
            h = e.hash;
        }
        ValueView::Heap(HeapValue::Array(a)) => {
            h = hash_sequence(a.len(), a.iter().cloned());
        }
        ValueView::Heap(HeapValue::Range(start, end)) => {
            let len = (*end - *start).max(0) as usize;
            h = hash_sequence(len, (*start..*end).map(Value::int));
        }
        ValueView::Heap(HeapValue::Binary(bin)) => {
            // Hash the LOGICAL bits so it stays consistent with the hand-written
            // `BinaryValue` equality: the bit length, the full bytes, then the
            // masked partial tail. Byte-aligned views hash straight off the
            // backing; bit-unaligned views re-align first.
            h = fnv1a_combine(h, bin.bit_len);
            if bin.bit_offset.is_multiple_of(8) {
                let start = (bin.bit_offset / 8) as usize;
                let full = (bin.bit_len / 8) as usize;
                for &b in &bin.backing[start..start + full] {
                    h = fnv1a_combine(h, b as u64);
                }
                let rem = (bin.bit_len % 8) as u32;
                if rem != 0 {
                    let mask = 0xFFu8 << (8 - rem);
                    h = fnv1a_combine(h, (bin.backing[start + full] & mask) as u64);
                }
            } else {
                for b in bin.to_aligned_vec() {
                    h = fnv1a_combine(h, b as u64);
                }
            }
        }
        _ => {
            h = fnv1a_combine(h, 0);
        }
    }
    h
}

/// Hash of the constant name prefix (`enum_name` then `variant_name`). These
/// bytes are compile-time constants, so the compiler computes this once per
/// constructor site and the VM folds payloads into it via
/// [`enum_hash_with_payload`] instead of re-walking the name bytes on every
/// construction.
pub fn enum_name_prefix_hash(enum_name: &str, variant_name: &str) -> u64 {
    let mut h = HASH_BASIS;
    for c in enum_name.bytes() {
        h = fnv1a_combine(h, c as u64);
    }
    for c in variant_name.bytes() {
        h = fnv1a_combine(h, c as u64);
    }
    h
}

/// Fold payload value hashes into a precomputed [`enum_name_prefix_hash`].
/// Equivalent to hashing the name bytes then the payload in one pass, but
/// skips re-hashing the compile-time-constant name bytes.
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

    #[test]
    fn roundtrip_int() {
        for i in [0, 1, -1, 42, -42, SMALL_INT_MAX, SMALL_INT_MIN] {
            let v = Value::int(i);
            assert_eq!(v.as_int(), Some(i));
            assert!(!v.is_heap());
            assert!(!v.is_float());
        }
    }

    #[test]
    fn roundtrip_bigint() {
        for i in [SMALL_INT_MAX + 1, SMALL_INT_MIN - 1, i64::MAX, i64::MIN] {
            let v = Value::int(i);
            assert_eq!(v.as_int(), Some(i));
            assert!(v.is_heap());
            assert!(v.is_int());
            match v.kind() {
                ValueView::Int(j) => assert_eq!(j, i),
                _ => panic!("expected Int view"),
            }
        }
    }

    #[test]
    fn roundtrip_float() {
        for f in [0.0, 1.0, -1.5, 1e308, f64::MIN_POSITIVE] {
            let v = Value::float(f);
            assert_eq!(v.as_float(), Some(f));
            assert!(v.is_float());
        }
        // Non-finite clamps to 0.0.
        assert_eq!(Value::float(f64::NAN).as_float(), Some(0.0));
        assert_eq!(Value::float(f64::INFINITY).as_float(), Some(0.0));
        assert_eq!(Value::float(f64::NEG_INFINITY).as_float(), Some(0.0));
    }

    #[test]
    fn roundtrip_bool() {
        assert_eq!(Value::bool(true).as_bool(), Some(true));
        assert_eq!(Value::bool(false).as_bool(), Some(false));
        assert!(!Value::bool(true).is_heap());
    }

    #[test]
    fn roundtrip_str() {
        let v = Value::str("hello");
        assert_eq!(v.as_str().map(|s| s.as_ref()), Some("hello"));
        assert!(v.is_heap());
        assert!(v.as_int().is_none());
    }

    #[test]
    fn clone_drop_refcount() {
        let inner: Rc<str> = Rc::from("shared");
        let weak = Rc::downgrade(&inner);
        let v = Value::heap(HeapValue::Str(inner));
        {
            let v2 = v.clone();
            let v3 = v.clone();
            assert!(weak.upgrade().is_some());
            drop(v2);
            drop(v3);
        }
        assert!(weak.upgrade().is_some());
        drop(v);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn into_heap_roundtrip() {
        let v = Value::str("x");
        let rc = v.into_heap().unwrap();
        assert!(matches!(&*rc, HeapValue::Str(s) if s.as_ref() == "x"));
        let v2 = Value::pack(rc);
        assert_eq!(v2.as_str().map(|s| s.as_ref()), Some("x"));
    }

    #[test]
    fn tags_disjoint() {
        assert!(Value::int(0).as_float().is_none());
        assert!(Value::int(0).as_bool().is_none());
        assert!(Value::float(0.0).as_int().is_none());
        assert!(Value::bool(false).as_int().is_none());
        assert!(Value::str("").as_int().is_none());
    }

    #[test]
    fn size_is_word() {
        assert_eq!(std::mem::size_of::<Value>(), 8);
    }

    #[test]
    fn roundtrip_nil() {
        let v = Value(HDR_NIL);
        assert!(v.0 == HDR_NIL);
        assert!(!v.is_int() && !v.is_bool() && !v.is_float() && !v.is_heap());
        assert!(matches!(v.kind(), ValueView::Nil));
    }

    #[test]
    fn roundtrip_array() {
        let v = Value::array(vec![Value::int(1), Value::int(2), Value::int(3)]);
        match v.as_heap() {
            Some(HeapValue::Array(a)) => {
                assert_eq!(a.len(), 3);
                assert_eq!(a.get(0).and_then(|x| x.as_int()), Some(1));
                assert_eq!(a.get(2).and_then(|x| x.as_int()), Some(3));
            }
            _ => panic!("expected Array"),
        }
    }

    #[test]
    fn roundtrip_range() {
        let v = Value::range(2, 9);
        match v.as_heap() {
            Some(HeapValue::Range(a, b)) => assert_eq!((*a, *b), (2, 9)),
            _ => panic!("expected Range"),
        }
    }

    #[test]
    fn roundtrip_binary() {
        let v = Value::binary(vec![1u8, 2, 3]);
        match v.as_heap() {
            Some(HeapValue::Binary(b)) => {
                assert_eq!(b.byte_window(), &[1u8, 2, 3][..]);
                assert_eq!(b.bit_offset, 0);
                assert_eq!(b.bit_len, 24);
            }
            _ => panic!("expected Binary"),
        }
        let v2 = Value::binary_bits(vec![0xFF], 5);
        match v2.as_heap() {
            Some(HeapValue::Binary(b)) => assert_eq!(b.bit_len, 5),
            _ => panic!("expected Binary"),
        }
    }

    #[test]
    fn roundtrip_tuple() {
        let v = Value::tuple(vec![Value::int(7), Value::bool(true)]);
        match v.as_heap() {
            Some(HeapValue::Tuple(t)) => {
                assert_eq!(t.elements.len(), 2);
                assert_eq!(t.elements[0].as_int(), Some(7));
                assert_eq!(t.elements[1].as_bool(), Some(true));
            }
            _ => panic!("expected Tuple"),
        }
    }

    #[test]
    fn roundtrip_closure() {
        let v = Value::closure(ClosureValue {
            func_idx: 3,
            captures: vec![Value::int(1)].into(),
            name: Rc::from("f"),
        });
        match v.as_heap() {
            Some(HeapValue::Closure(c)) => {
                assert_eq!(c.func_idx, 3);
                assert_eq!(c.captures[0].as_int(), Some(1));
            }
            _ => panic!("expected Closure"),
        }
    }

    #[test]
    fn roundtrip_enum() {
        let v = Value::enum_(EnumValue {
            type_id: 1,
            enum_name: "Option".into(),
            variant_name: "Some".into(),
            field_labels: Rc::from(Vec::<Rc<str>>::new()),
            payload: vec![Value::int(5)],
            hash: 0,
        });
        match v.as_heap() {
            Some(HeapValue::Enum(e)) => {
                assert_eq!(&*e.variant_name, "Some");
                assert_eq!(e.payload[0].as_int(), Some(5));
            }
            _ => panic!("expected Enum"),
        }
    }

    #[test]
    fn roundtrip_socket() {
        let v = Value::socket(SocketValue {
            id: 7,
            is_listener: true,
        });
        match v.as_heap() {
            Some(HeapValue::Socket(s)) => {
                assert_eq!(s.id, 7);
                assert!(s.is_listener);
            }
            _ => panic!("expected Socket"),
        }
    }

    /// Probe the outer `Rc<HeapValue>` strong count without disturbing it.
    fn heap_strong_count(v: &Value) -> usize {
        assert!(v.is_heap());
        let ptr = v.heap_ptr();
        let rc = unsafe { Rc::from_raw(ptr) };
        let n = Rc::strong_count(&rc);
        std::mem::forget(rc);
        n
    }

    #[test]
    fn clone_bumps_heap_strong_count() {
        let v = Value::str("abc");
        assert_eq!(heap_strong_count(&v), 1);
        let w = v.clone();
        assert_eq!(heap_strong_count(&v), 2);
        assert_eq!(heap_strong_count(&w), 2);
        drop(w);
        assert_eq!(heap_strong_count(&v), 1);
    }

    #[test]
    fn drop_frees_nested() {
        let inner: Rc<str> = Rc::from("inner");
        let v = Value::array(vec![Value::heap(HeapValue::Str(inner.clone()))]);
        assert_eq!(Rc::strong_count(&inner), 2);
        drop(v);
        assert_eq!(Rc::strong_count(&inner), 1);
    }

    #[test]
    fn hash_stable_across_int_repr() {
        let small = hash_value(&Value::int(5));
        assert_eq!(small, fnv1a_combine(HASH_BASIS, 5u64));
        let big = Value::int(i64::MAX);
        assert!(big.is_heap());
        assert_eq!(hash_value(&big), fnv1a_combine(HASH_BASIS, i64::MAX as u64));
    }

    // A range and the array it materialises to are `values_equal`, so they must
    // hash identically — otherwise the precomputed enum hash fast-rejects equal
    // values like `Some(0..3) == Some([0, 1, 2])`.
    #[test]
    fn range_hashes_like_its_materialized_array() {
        for (s, e) in [(0, 0), (5, 5), (0, 1), (0, 3), (-3, 4), (10, 100)] {
            let arr = Value::array((s..e).map(Value::int).collect());
            assert_eq!(
                hash_value(&Value::range(s, e)),
                hash_value(&arr),
                "range {s}..{e} must hash like its materialised array"
            );
        }
        // Equivalence holds when nested inside another sequence, too.
        let nested_range = Value::array(vec![Value::range(0, 4)]);
        let nested_arr = Value::array(vec![Value::array((0..4).map(Value::int).collect())]);
        assert_eq!(hash_value(&nested_range), hash_value(&nested_arr));
    }

    // Regression: `+0.0` and `-0.0` are `values_equal` (`0.0 == -0.0` in IEEE
    // 754) but have distinct bit patterns. The enum-equality arm gates on the
    // cached payload hash, so hashing a signed zero by raw bits fast-rejected
    // equal values like `Some(0.0) == Some(-0.0)`. The hash must respect
    // equality: both signed zeros — including when folded into an enum payload
    // hash — must hash identically.
    #[test]
    fn signed_zero_hashes_equal() {
        assert_eq!(
            hash_value(&Value::float(0.0)),
            hash_value(&Value::float(-0.0)),
            "+0.0 and -0.0 are values_equal, so they must hash identically"
        );
        // The same must hold once folded into a constructor's payload hash,
        // which is the gate `values_equal` uses for enums.
        let prefix = enum_name_prefix_hash("Option", "Some");
        assert_eq!(
            enum_hash_with_payload(prefix, &[Value::float(0.0)]),
            enum_hash_with_payload(prefix, &[Value::float(-0.0)]),
            "Some(0.0) and Some(-0.0) must share a payload hash"
        );
        // Sanity: a genuinely different float still hashes differently.
        assert_ne!(
            hash_value(&Value::float(0.0)),
            hash_value(&Value::float(1.0))
        );
    }

    // Regression: hashing a range must not iterate it. Before the fix this arm
    // looped `for i in start..end`, so hashing `0..i64::MAX` (≈9.2e18 elements)
    // hung — reached eagerly whenever a range was wrapped in a constructor such
    // as `Some(0..n)`. Reaching the asserts at all proves the work is bounded.
    #[test]
    fn hashing_a_huge_range_is_constant_time() {
        let huge = hash_value(&Value::range(0, i64::MAX));
        // Huge ranges differing only in length still hash differently (the
        // length is folded in even though their sampled prefixes match).
        assert_ne!(huge, hash_value(&Value::range(0, i64::MAX - 1)));
        // An empty range matches an empty array regardless of its endpoints.
        assert_eq!(
            hash_value(&Value::range(i64::MAX, i64::MAX)),
            hash_value(&Value::array(vec![]))
        );
    }
}
