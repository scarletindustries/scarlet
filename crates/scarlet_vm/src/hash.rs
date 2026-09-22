//! A value's hash, which places it in a map (`map.rs`).
//!
//! Two values that are `==` always hash the same, so the hash reads a value
//! the way `eq.rs` compares one: an Int by its number, a Float with its two
//! zeros as one, a range as the array of its Ints, a slice as the bits it
//! shows, a map by its entries. Each value goes in as a tag saying what it is,
//! then its parts, with a count before any run of them, so two different
//! values never feed the same words.
//!
//! The hash is SipHash-1-3 (Aumasson & Bernstein, 2012), the one Rust's own
//! `HashMap` uses, with a fixed key. Fixed, so a map's order is the same on
//! every run and every machine. SipHash, so that even knowing the key, making
//! many keys share a whole 64-bit hash is out of reach: a map only slows down
//! for keys whose whole hashes are equal, and those it keeps in a list.
//!
//! Like `==`, the walk is a list of values still to hash rather than
//! recursion, so a list a million long hashes without overflowing the stack.

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
}

/// `v`'s hash.
pub(crate) fn hash(heap: &Heap, v: Value) -> u64 {
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

/// Hash `v` itself, and queue what it holds, first part on top.
fn one(heap: &Heap, v: Value, s: &mut Sip<1, 3>, todo: &mut Vec<Value>) {
    match v.view() {
        View::Int(n) => int(s, i128::from(n)),
        // `0.0 == -0.0`, so both hash as `0.0`.
        View::Float(f) => {
            s.tag(Tag::Float);
            s.word(if f == 0.0 { 0 } else { f.to_bits() });
        }
        View::Nil | View::Bool(_) | View::Func(_) | View::Nullary(_) => {
            s.tag(Tag::Word);
            s.word(v.bits());
        }
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
            s.word(u64::from(v.variant_idx) << 32 | u64::from(v.type_id.0 as u32));
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
        // Equal maps have the same shape (`map.rs`), so their entries come
        // out in the same order. All but the keys a collision holds, which
        // are in the order they went in: those add up their own hashes,
        // which comes out the same in any order.
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

/// An Int, small or big, by its number: its sign, then its magnitude as
/// 64-bit digits, least significant first.
fn int(s: &mut Sip<1, 3>, n: i128) {
    s.tag(Tag::Int);
    s.word(u64::from(n < 0));
    let m = n.unsigned_abs();
    let digits: &[u64] = &[m as u64, (m >> 64) as u64];
    let len = digits.iter().rposition(|&d| d != 0).map_or(0, |i| i + 1);
    s.word(len as u64);
    for &d in &digits[..len] {
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

    fn tag(&mut self, t: Tag) {
        self.word(t as u64);
    }

    fn word(&mut self, w: u64) {
        if self.tail_len == 0 {
            self.len += 8;
            self.compress(w);
        } else {
            self.bytes(&w.to_le_bytes());
        }
    }

    fn bytes(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.tail |= u64::from(b) << (8 * self.tail_len);
            self.tail_len += 1;
            self.len += 1;
            if self.tail_len == 8 {
                let m = self.tail;
                self.tail = 0;
                self.tail_len = 0;
                self.compress(m);
            }
        }
    }

    fn compress(&mut self, m: u64) {
        self.v[3] ^= m;
        for _ in 0..C {
            self.round();
        }
        self.v[0] ^= m;
    }

    fn finish(mut self) -> u64 {
        let m = (self.len & 0xFF) << 56 | self.tail;
        self.compress(m);
        self.v[2] ^= 0xFF;
        for _ in 0..D {
            self.round();
        }
        self.v[0] ^ self.v[1] ^ self.v[2] ^ self.v[3]
    }

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
