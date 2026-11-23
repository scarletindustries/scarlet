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
    if let Some(n) = crate::bigint::wide(heap, v).and_then(|n| i64::try_from(n).ok()) {
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
        View::Cell(cell) => self::cell(heap, cell, s, todo),
    }
}

fn cell(heap: &Heap, cell: Cell, s: &mut Sip<1, 3>, todo: &mut Vec<Value>) {
    if let Some(seq) = array::seq(heap, cell) {
        s.tag(Tag::Array);
        s.word(seq.len(heap));
        match seq {
            Seq::Range { start, end } => {
                for n in start..end {
                    int(s, i128::from(n));
                }
            }
            Seq::Tree(t) => queue(todo, array::elements(heap, t)),
        }
        return;
    }
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
            let v = heap.variant(cell);
            s.word(variant_word(v));
            counted(s, todo, heap.fields(cell).collect());
        }
        Kind::Closure => {
            s.tag(Tag::Closure);
            s.word(u64::from(heap.closure_func(cell).0));
            counted(
                s,
                todo,
                (0..).map_while(|i| heap.capture(cell, i)).collect(),
            );
        }
        Kind::Tuple => {
            s.tag(Tag::Tuple);
            counted(s, todo, heap.elements(cell).collect());
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
        // Handled above, or never a value of its own.
        Kind::ArrayRoot
        | Kind::ArrayLeaf
        | Kind::ArrayBranch
        | Kind::Range
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
mod tests {
    use super::*;

    /// The rounds are right: SipHash-2-4 gives the paper's answers
    /// (Appendix A), with the key `00 01 .. 0f` and the message `00 01 ..`.
    #[test]
    fn siphash_2_4_matches_the_papers_test_vectors() {
        let k0 = u64::from_le_bytes([0, 1, 2, 3, 4, 5, 6, 7]);
        let k1 = u64::from_le_bytes([8, 9, 10, 11, 12, 13, 14, 15]);
        let run = |n: u8| {
            let mut s = Sip::<2, 4>::new(k0, k1);
            s.bytes(&(0..n).collect::<Vec<u8>>());
            s.finish()
        };
        assert_eq!(run(0), 0x726f_db47_dd0e_0e31);
        assert_eq!(run(1), 0x74f8_39c5_93dc_67fd);
        assert_eq!(run(15), 0xa129_ca61_49be_45e5);
    }

    /// A map's order comes from these hashes, and a program can see it. So
    /// each is pinned, and no change to how a value is stored may move one.
    #[test]
    fn the_hash_of_a_plain_value_does_not_depend_on_its_word() {
        let mut heap = Heap::default();
        let nullary = |t, v| {
            Value::nullary(VariantRef {
                type_id: scarlet_ir::TypeId(t),
                variant_idx: v,
            })
        };
        let func = |f| Value::func(scarlet_ir::core_ir::FuncIdx(f));
        let int = |n| Value::int(n).expect("small");
        let pinned = [
            (Value::NIL, 0xd85b_9296_886d_def0),
            (Value::bool(false), 0x6994_423c_a56f_4da0),
            (Value::bool(true), 0x40d9_1fce_6a74_595d),
            (func(0), 0xb1a9_9d9e_d09c_0185),
            (func(7), 0x86bf_fefa_4fbe_9b58),
            (func(u32::MAX), 0x72a7_391d_e862_ae42),
            (nullary(0, 0), 0x5b0a_b266_62e7_7842),
            (nullary(263, 1), 0xa235_c7dd_2d07_de50),
            (nullary(-1, 7), 0x0cdd_652c_2674_8da6),
            (nullary(i32::MAX, u16::MAX), 0x78b0_7bfb_c87a_b9b6),
            (int(0), 0x6c85_978d_e7cc_c2e3),
            (int(-1), 0xd551_7ece_616f_fee6),
            (int(42), 0x45c9_0af3_9aaa_7d08),
            (Value::float(1.5), 0xe92d_7e62_b635_5bcb),
        ];
        for (v, want) in pinned {
            assert_eq!(hash(&heap, v), want, "{v:?}");
        }
        let pair = heap
            .tuple(&[Value::bool(true), nullary(263, 1)])
            .expect("room");
        assert_eq!(hash(&heap, Value::cell(pair)), 0x80b5_8fd6_9cae_65c5);
    }

    /// An Int hashes by its number and not by its form, so the width of the
    /// small Int moves no Int key. The numbers are well inside the small
    /// range, at both of its ends, a step past each, and far past it.
    #[test]
    fn an_int_hashes_the_same_small_or_big() {
        let mut heap = Heap::default();
        let pinned: [(i64, u64); 7] = [
            (1 << 47, 0x3628_f273_d695_c247),
            ((1 << 47) - 1, 0xf1a5_6322_1915_85dc),
            (1 << 50, 0xb8e5_c728_bdba_d507),
            ((1 << 50) - 1, 0x8385_3df3_d9c9_7b44),
            (-(1 << 50), 0x4b27_7203_7923_0f26),
            (-(1 << 50) - 1, 0x69a9_7e22_4284_8c91),
            (1 << 62, 0xedde_a07f_b473_9d65),
        ];
        for (n, want) in pinned {
            let v = crate::bigint::int(&mut heap, n).expect("room");
            assert_eq!(hash(&heap, v), want, "{n}");
            // Inside another value an Int is hashed by `int` when it is small
            // and by `big` when it is a cell. This cell holds a number that
            // may be small, which no Int operation makes.
            let as_cell = heap.big_int(&num_bigint::BigInt::from(n)).expect("room");
            assert_eq!(hash(&heap, Value::cell(as_cell)), want, "{n}");
            let held = heap.tuple(&[v]).expect("room");
            let held_as_cell = heap.tuple(&[Value::cell(as_cell)]).expect("room");
            assert_eq!(
                hash(&heap, Value::cell(held)),
                hash(&heap, Value::cell(held_as_cell)),
                "{n}"
            );
        }
    }

    /// A word fed whole hashes as its eight bytes fed one at a time.
    #[test]
    fn a_word_is_its_bytes() {
        let mut a = Sip::<1, 3>::new(K0, K1);
        a.bytes(&[9]);
        a.word(0x0102_0304_0506_0708);
        let mut b = Sip::<1, 3>::new(K0, K1);
        b.bytes(&[9]);
        b.bytes(&0x0102_0304_0506_0708u64.to_le_bytes());
        assert_eq!(a.finish(), b.finish());
    }
}
