//! A value's hash, which places it in a map (`map.rs`).
//!
//! Two values that are `==` always hash the same, so the hash reads a value
//! the way `eq.rs` compares one. It reads an Int by its number, both Float
//! zeros as one, a range as the array of its Ints, a slice as the bits it
//! shows and a map by its entries. Each value goes in as a tag saying what it
//! is, then its parts, with a count before any run of them. So two different
//! values never feed in the same words.
//!
//! The hash is SipHash-1-3 (Aumasson & Bernstein, 2012), which Rust's own
//! `HashMap` uses, with a fixed key. The key is fixed so a map's order is the
//! same on every run and every machine. We use SipHash because, even if you
//! know the key, you can't make many keys share a whole 64-bit hash. A map
//! only slows down for keys whose whole hashes are equal, and it keeps those
//! in a list.
//!
//! An Int that fits 64 bits is the exception. It hashes through a fixed
//! permutation of the 64-bit words instead (the splitmix64 finalizer), which
//! is a few multiplies. A permutation gives no two such Ints the same hash at
//! all, which is more than SipHash promises. The keys of one map all have one
//! type, so these hashes only ever meet each other.
//!
//! Like `==`, the walk keeps a list of values still to hash instead of
//! recursing, so a very long list hashes without overflowing the stack.

use scarlet_ir::core_ir::VariantRef;

use crate::array::{self, Seq};
use crate::bigint;
use crate::binary;
use crate::heap::{Cell, Heap, Kind};
use crate::map;
use crate::value::{Value, View};

/// The key: "scarlet:" and "map-hash", as little-endian words.
const K0: u64 = u64::from_le_bytes(*b"scarlet:");
const K1: u64 = u64::from_le_bytes(*b"map-hash");

/// What a value is, written before its parts.
#[derive(Clone, Copy)]
enum Tag {
    Int = 1,
    Float,
    /// `Nil`, a Bool, a constructor with no fields or a function with no
    /// captures: one word, the same only for the same value.
    Word,
    String,
    Ctor,
    Closure,
    Tuple,
    Array,
    Binary,
    Map,
    /// Keys whose whole hashes are equal, in a map.
    Collision,
    /// A socket or a listener, by the host's number for it.
    Handle,
}

/// `v`'s hash.
pub(crate) fn hash(heap: &Heap, v: Value) -> u64 {
    // Nearly every Int key is small, so that is asked first.
    let big = || bigint::wide(heap, v).and_then(|n| i64::try_from(n).ok());
    if let Some(n) = v.small_int().or_else(big) {
        return permute(n.cast_unsigned() ^ K0);
    }
    let mut s = Sip::<1, 3>::new(K0, K1);
    // Empty until `v` holds other values, so a key that holds none, like an
    // Int or a string, hashes without allocating.
    let mut todo = Vec::new();
    one(heap, v, &mut s, &mut todo);
    while let Some(v) = todo.pop() {
        one(heap, v, &mut s, &mut todo);
    }
    s.finish()
}

/// The splitmix64 finalizer, a bijection on 64-bit words: each step is a
/// shift-xor or a multiply by an odd number, and each of those can be undone.
fn permute(x: u64) -> u64 {
    let x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    let x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// Hash `v` itself, and queue what it holds, first part on top.
#[inline]
fn one(heap: &Heap, v: Value, s: &mut Sip<1, 3>, todo: &mut Vec<Value>) {
    match v.view() {
        View::Int(n) => int(s, i128::from(n)),
        // `0.0 == -0.0`, so both hash as `0.0`.
        View::Float(f) => {
            s.tag(Tag::Float);
            s.word(if f == 0.0 { 0 } else { f.to_bits() });
        }
        View::Nil => plain(s, PLAIN_IMMEDIATE),
        View::Bool(b) => plain(s, PLAIN_IMMEDIATE | if b { 2 } else { 1 }),
        View::Func(f) => plain(s, PLAIN_FUNC | u64::from(f.0)),
        View::Nullary(v) => plain(s, PLAIN_NULLARY | variant_word(v)),
        View::EmptyArray => array(heap, Seq::Empty, s, todo),
        View::Cell(cell) => self::cell(heap, cell, s, todo),
    }
}

/// [`one`] of an array.
fn array(heap: &Heap, seq: Seq, s: &mut Sip<1, 3>, todo: &mut Vec<Value>) {
    s.tag(Tag::Array);
    s.word(seq.len(heap));
    match seq {
        Seq::Empty => {}
        Seq::Range { start, end } => {
            for n in start..end {
                int(s, i128::from(n));
            }
        }
        Seq::Flat(f) => todo.extend(f.items(heap).rev()),
        Seq::Tree(t) => queue(todo, array::elements_of_tree(heap, t)),
    }
}

/// [`one`] of a cell. Only a broken heap has a cell with no kind, a constructor
/// that is none of the program's or a closure of no function. What such a cell
/// lacks counts as 0 here or is left out, and an audit is what finds it.
fn cell(heap: &Heap, cell: Cell, s: &mut Sip<1, 3>, todo: &mut Vec<Value>) {
    if let Some(b) = binary::bits(heap, cell) {
        s.tag(Tag::Binary);
        s.word(b.len);
        s.bytes(&binary::bytes(heap, b));
        return;
    }
    let Some(kind) = heap.kind(cell) else {
        return;
    };
    match kind {
        Kind::String => {
            s.tag(Tag::String);
            let mut text = Vec::with_capacity(heap.string_len(cell));
            heap.read_string(cell, &mut text);
            s.word(text.len() as u64);
            s.bytes(&text);
        }
        Kind::BigInt => big(s, heap.data(cell)),
        Kind::Ctor => {
            s.tag(Tag::Ctor);
            s.word(heap.variant(cell).map_or(0, variant_word));
            counted(s, todo, heap.values(cell).collect());
        }
        Kind::Closure => {
            s.tag(Tag::Closure);
            s.word(heap.closure_func(cell).map_or(0, |f| u64::from(f.0)));
            counted(s, todo, heap.captures(cell).collect());
        }
        Kind::Tuple => {
            s.tag(Tag::Tuple);
            counted(s, todo, heap.values(cell).collect());
        }
        Kind::Handle => {
            s.tag(Tag::Handle);
            s.word(heap.data(cell).first().copied().unwrap_or(0));
        }
        // Equal maps have the same shape (`map.rs`), so their entries come
        // out in the same order. The exception is the keys in a collision,
        // which keep the order they went in. So we add up their hashes
        // instead, because a sum is the same in any order.
        Kind::Map => {
            s.tag(Tag::Map);
            s.word(map::size(heap, cell));
            let mut parts = Vec::new();
            for piece in map::pieces(heap, cell) {
                match piece {
                    map::Piece::Entry(k, v) => parts.extend([k, v]),
                    map::Piece::Collision(entries) => {
                        let sum = entries.iter().fold(0u64, |sum, &(k, v)| {
                            let mut e = Sip::<1, 3>::new(K0, K1);
                            e.word(hash(heap, k));
                            e.word(hash(heap, v));
                            sum.wrapping_add(e.finish())
                        });
                        s.tag(Tag::Collision);
                        s.word(sum);
                    }
                }
            }
            queue(todo, parts);
        }
        Kind::ArrayFlat | Kind::ArrayRoot | Kind::Range => {
            if let Some(seq) = array::seq(heap, Value::cell(cell)) {
                array(heap, seq, s, todo);
            }
        }
        // Handled above, or never a value of its own.
        Kind::ArrayBranch
        | Kind::Binary
        | Kind::BinarySlice
        | Kind::MapNode
        | Kind::MapCollision => {}
    }
}

/// What kind of plain value a [`Tag::Word`] word is, in its top 16 bits.
/// These numbers are the hash's own and not the tags in `value.rs`. They are
/// fixed, because changing one reorders every map keyed by that kind.
const PLAIN_IMMEDIATE: u64 = 0xFFFA << 48;
const PLAIN_FUNC: u64 = 0xFFFB << 48;
const PLAIN_NULLARY: u64 = 0xFFFD << 48;

/// A value that is all in one word and holds nothing: its kind, with what
/// it is below. `Nil` is 0, `False` 1 and `True` 2.
#[inline]
fn plain(s: &mut Sip<1, 3>, word: u64) {
    s.tag(Tag::Word);
    s.word(word);
}

/// A constructor as one word: its variant above its type.
fn variant_word(v: VariantRef) -> u64 {
    u64::from(v.variant_idx) << 32 | u64::from(v.type_id.0.cast_unsigned())
}

/// An Int, small or big, by its number: its sign, then its magnitude as
/// 64-bit digits, least significant first.
#[inline]
fn int(s: &mut Sip<1, 3>, n: i128) {
    s.tag(Tag::Int);
    s.word(u64::from(n < 0));
    let m = n.unsigned_abs();
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the low and the high 64 bits of a u128, on purpose"
    )]
    let digits: &[u64] = &[m as u64, (m >> 64) as u64];
    let len = digits
        .iter()
        .rposition(|&d| d != 0)
        .map_or(0, |i| i.saturating_add(1));
    s.word(len as u64);
    for &d in digits.iter().take(len) {
        s.word(d);
    }
}

/// A big-int cell's words, hashed as [`int`] would hash its number.
fn big(s: &mut Sip<1, 3>, data: &[u64]) {
    let head = data.first().copied().unwrap_or(0);
    let len = (head & 0xFFFF_FFFF) as usize;
    s.tag(Tag::Int);
    s.word(head >> 63);
    s.word(len as u64);
    for &d in data.iter().skip(1).take(len) {
        s.word(d);
    }
}

fn counted(s: &mut Sip<1, 3>, todo: &mut Vec<Value>, parts: Vec<Value>) {
    s.word(parts.len() as u64);
    queue(todo, parts);
}

/// Queue `parts` so the first is hashed first.
fn queue(todo: &mut Vec<Value>, parts: Vec<Value>) {
    todo.extend(parts.into_iter().rev());
}

/// SipHash-`C`-`D`, fed a stream of bytes.
struct Sip<const C: usize, const D: usize> {
    v: [u64; 4],
    /// Bytes not yet a whole word, the first in the low byte.
    tail: u64,
    tail_len: usize,
    /// Bytes fed so far.
    len: u64,
}

impl<const C: usize, const D: usize> Sip<C, D> {
    #[inline]
    fn new(k0: u64, k1: u64) -> Self {
        Sip {
            v: [
                k0 ^ 0x736f_6d65_7073_6575,
                k1 ^ 0x646f_7261_6e64_6f6d,
                k0 ^ 0x6c79_6765_6e65_7261,
                k1 ^ 0x7465_6462_7974_6573,
            ],
            tail: 0,
            tail_len: 0,
            len: 0,
        }
    }

    #[inline]
    fn tag(&mut self, t: Tag) {
        self.word(t as u64);
    }

    #[inline]
    fn word(&mut self, w: u64) {
        if self.tail_len == 0 {
            self.len = self.len.wrapping_add(8);
            self.compress(w);
        } else {
            self.bytes(&w.to_le_bytes());
        }
    }

    fn bytes(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.tail |= u64::from(b) << (self.tail_len << 3);
            self.tail_len = self.tail_len.saturating_add(1);
            self.len = self.len.wrapping_add(1);
            if self.tail_len == 8 {
                let m = self.tail;
                self.tail = 0;
                self.tail_len = 0;
                self.compress(m);
            }
        }
    }

    #[inline]
    fn compress(&mut self, m: u64) {
        self.v[3] ^= m;
        for _ in 0..C {
            self.round();
        }
        self.v[0] ^= m;
    }

    #[inline]
    fn finish(mut self) -> u64 {
        let m = (self.len & 0xFF) << 56 | self.tail;
        self.compress(m);
        self.v[2] ^= 0xFF;
        for _ in 0..D {
            self.round();
        }
        self.v[0] ^ self.v[1] ^ self.v[2] ^ self.v[3]
    }

    #[inline]
    fn round(&mut self) {
        let [v0, v1, v2, v3] = &mut self.v;
        *v0 = v0.wrapping_add(*v1);
        *v1 = v1.rotate_left(13) ^ *v0;
        *v0 = v0.rotate_left(32);
        *v2 = v2.wrapping_add(*v3);
        *v3 = v3.rotate_left(16) ^ *v2;
        *v0 = v0.wrapping_add(*v3);
        *v3 = v3.rotate_left(21) ^ *v0;
        *v2 = v2.wrapping_add(*v1);
        *v1 = v1.rotate_left(17) ^ *v2;
        *v2 = v2.rotate_left(32);
    }
}

#[cfg(test)]
mod tests;
