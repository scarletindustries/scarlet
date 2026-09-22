//! Binaries: strings of bits, as the old VM had them (`vm/binary.rs` on
//! master before the rewrite).
//!
//! Bits are counted from the most significant bit of the first byte, and the
//! bits past a binary's end in its last byte are zero.
//!
//! A binary is one of two cells:
//!
//! - an **owner** holds the bits: a word with its length in bits, then its
//!   bytes, eight to a word, the first byte in the lowest bits of a word;
//! - a **slice** holds a reference to an owner, a bit offset and a length in
//!   bits. `binary.slice_bits` and a pattern's `rest:binary` make one, so they
//!   copy nothing, as their docs promise. A slice of a slice points at the
//!   owner, so there is never a chain of them.
//!
//! [`Bits`] is either, as "these bits of this owner". Everything that reads a
//! binary reads it through [`Bits`], so it does not matter which one it is.

use num_bigint::{BigInt, Sign};

use crate::heap::{Cell, Full, Heap, Kind};
use crate::value::Value;

/// The most bits one binary can hold: a cell is at most 2^28 words, and a
/// word holds 64 bits. A longer one is a full heap, not a binary.
pub(crate) const MAX_BITS: u64 = (1 << 28) * 64;

/// `len` bits of `owner`'s bytes, from bit `at`.
#[derive(Clone, Copy)]
pub(crate) struct Bits {
    owner: Cell,
    at: u64,
    pub(crate) len: u64,
}

/// The bits `cell` holds, or `None` when it is not a binary.
pub(crate) fn bits(heap: &Heap, cell: Cell) -> Option<Bits> {
    let d = heap.data(cell);
    match heap.kind(cell)? {
        Kind::Binary => Some(Bits {
            owner: cell,
            at: 0,
            len: d.first().copied().unwrap_or(0),
        }),
        Kind::BinarySlice => Some(Bits {
            owner: Value::from_bits(d.first().copied()?).as_cell()?,
            at: d.get(1).copied()?,
            len: d.get(2).copied()?,
        }),
        Kind::String
        | Kind::BigInt
        | Kind::Ctor
        | Kind::Closure
        | Kind::Tuple
        | Kind::ArrayRoot
        | Kind::ArrayLeaf
        | Kind::ArrayBranch
        | Kind::Range => None,
    }
}

/// A new owner holding the first `len` bits of `bytes`. The bits past `len`
/// in the last byte are cleared.
pub(crate) fn make(heap: &mut Heap, bytes: &[u8], len: u64) -> Result<Cell, Full> {
    let n = len.div_ceil(8) as usize;
    let mut buf: Vec<u8> = bytes.iter().copied().take(n).collect();
    buf.resize(n, 0);
    let pad = n as u64 * 8 - len;
    if let Some(last) = buf.last_mut() {
        *last &= 0xFFu8 << pad;
    }
    let mut data = Vec::with_capacity(1 + n.div_ceil(8));
    data.push(len);
    for chunk in buf.chunks(8) {
        let mut word = [0u8; 8];
        for (w, b) in word.iter_mut().zip(chunk) {
            *w = *b;
        }
        data.push(u64::from_le_bytes(word));
    }
    heap.make(Kind::Binary, &data)
}

/// Bits `from .. from + len` of `b`, sharing its owner. The caller has
/// checked that they are inside it.
pub(crate) fn slice(heap: &mut Heap, b: Bits, from: u64, len: u64) -> Result<Cell, Full> {
    heap.share(b.owner);
    heap.make(
        Kind::BinarySlice,
        &[Value::cell(b.owner).bits(), b.at + from, len],
    )
}

/// Byte `i` of `b`'s owner, or 0 past its end.
fn owner_byte(heap: &Heap, owner: Cell, i: u64) -> u8 {
    let d = heap.data(owner);
    match d.get(1 + (i / 8) as usize) {
        Some(w) => (w >> ((i % 8) * 8)) as u8,
        None => 0,
    }
}

/// The 8 bits of `b` from bit `i`, the ones past its end read as 0.
fn byte(heap: &Heap, b: Bits, i: u64) -> u8 {
    if i >= b.len {
        return 0;
    }
    let p = b.at + i;
    let s = (p % 8) as u32;
    let hi = owner_byte(heap, b.owner, p / 8);
    let raw = if s == 0 {
        hi
    } else {
        let lo = owner_byte(heap, b.owner, p / 8 + 1);
        (hi << s) | (lo >> (8 - s))
    };
    let left = b.len - i;
    if left >= 8 {
        raw
    } else {
        raw & (0xFFu8 << (8 - left))
    }
}

/// `b` as whole bytes, the last one padded with zero bits.
pub(crate) fn bytes(heap: &Heap, b: Bits) -> Vec<u8> {
    (0..b.len.div_ceil(8))
        .map(|k| byte(heap, b, k * 8))
        .collect()
}

/// `b`'s whole bytes: a last byte that is not whole is left out, as the
/// ASCII built-ins read it.
pub(crate) fn whole_bytes(heap: &Heap, b: Bits) -> Vec<u8> {
    (0..b.len / 8).map(|k| byte(heap, b, k * 8)).collect()
}

/// Whole byte `i` of `b`, or `None` past the last whole one.
pub(crate) fn whole_byte(heap: &Heap, b: Bits, i: u64) -> Option<u8> {
    (i < b.len / 8).then(|| byte(heap, b, i * 8))
}

/// Whether `a` and `b` hold the same bits.
pub(crate) fn equal(heap: &Heap, a: Bits, b: Bits) -> bool {
    a.len == b.len && (0..a.len.div_ceil(8)).all(|k| byte(heap, a, k * 8) == byte(heap, b, k * 8))
}

/// Whether `b` holds `prefix` from bit `at`.
pub(crate) fn has_at(heap: &Heap, b: Bits, at: u64, prefix: Bits) -> bool {
    let Some(end) = at.checked_add(prefix.len) else {
        return false;
    };
    if end > b.len {
        return false;
    }
    let window = Bits {
        owner: b.owner,
        at: b.at + at,
        len: prefix.len,
    };
    equal(heap, window, prefix)
}

/// All of `parts`, one after another, as a new owner.
pub(crate) fn join(heap: &mut Heap, parts: &[Bits]) -> Result<Cell, Full> {
    let len: u64 = parts.iter().map(|p| p.len).sum();
    let mut out = vec![0u8; len.div_ceil(8) as usize];
    let mut at = 0u64;
    for p in parts {
        for k in 0..p.len.div_ceil(8) {
            let chunk = byte(heap, *p, k * 8);
            let n = (p.len - k * 8).min(8);
            put(&mut out, at + k * 8, chunk, n);
        }
        at += p.len;
    }
    make(heap, &out, len)
}

/// Write the top `n` bits of `chunk` into `out` from bit `at`.
fn put(out: &mut [u8], at: u64, chunk: u8, n: u64) {
    for j in 0..n {
        if chunk & (0x80 >> j) != 0 {
            let p = at + j;
            if let Some(b) = out.get_mut((p / 8) as usize) {
                *b |= 0x80 >> (p % 8);
            }
        }
    }
}

/// The low `width` bits of `n`, most significant first. A negative `n` is
/// its two's complement, extended as far as `width` asks, so `<<-1:size(12)>>`
/// is twelve ones.
pub(crate) fn from_int(n: &BigInt, width: u64) -> Vec<u8> {
    let bytes = width.div_ceil(8) as usize;
    if bytes == 0 {
        return Vec::new();
    }
    let modulus = BigInt::from(1) << width;
    // `%` keeps the dividend's sign, so add the modulus back to land in
    // `0 .. 2^width`.
    let low = ((n % &modulus) + &modulus) % &modulus;
    let pad = bytes as u64 * 8 - width;
    let (_, be) = (low << pad).to_bytes_be();
    let mut out = vec![0u8; bytes];
    let start = bytes.saturating_sub(be.len());
    for (o, b) in out.iter_mut().skip(start).zip(be) {
        *o = b;
    }
    out
}

/// `width` bits of `b` from bit `at`, as an unsigned Int, most significant
/// bit first. Bits past `b`'s end read as 0.
pub(crate) fn read_uint(heap: &Heap, b: Bits, at: u64, width: u64) -> BigInt {
    let window = Bits {
        owner: b.owner,
        at: b.at + at.min(b.len),
        len: b.len.saturating_sub(at).min(width),
    };
    let n = BigInt::from_bytes_be(Sign::Plus, &bytes(heap, window));
    // `bytes` pads the window to whole bytes; drop the padding, then add the
    // zeros past `b`'s end that `width` still asks for.
    let n = n >> (window.len.div_ceil(8) * 8 - window.len);
    n << (width - window.len)
}

/// The UTF-8 code point at bit `at` of `b`, and how many bits it took, or
/// `None` when no valid one starts there.
pub(crate) fn read_utf8(heap: &Heap, b: Bits, at: u64) -> Option<(u32, u64)> {
    if at.checked_add(8)? > b.len {
        return None;
    }
    let b0 = byte(heap, b, at);
    let n: u64 = if b0 < 0x80 {
        1
    } else if b0 >> 5 == 0b110 {
        2
    } else if b0 >> 4 == 0b1110 {
        3
    } else if b0 >> 3 == 0b11110 {
        4
    } else {
        return None;
    };
    if at + n * 8 > b.len {
        return None;
    }
    let buf: Vec<u8> = (0..n).map(|k| byte(heap, b, at + k * 8)).collect();
    let c = std::str::from_utf8(&buf).ok()?.chars().next()?;
    Some((c as u32, n * 8))
}

/// How a binary shows: `<<1, 2, 3>>`, with a last byte that is not whole as
/// `5:size(3)`.
pub(crate) fn text(heap: &Heap, b: Bits) -> String {
    let mut parts: Vec<String> = (0..b.len / 8)
        .map(|k| byte(heap, b, k * 8).to_string())
        .collect();
    let rem = b.len % 8;
    if rem != 0 {
        let last = byte(heap, b, b.len - rem) >> (8 - rem);
        parts.push(format!("{last}:size({rem})"));
    }
    format!("<<{}>>", parts.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner(heap: &mut Heap, bytes: &[u8], len: u64) -> Bits {
        let cell = make(heap, bytes, len).expect("room");
        bits(heap, cell).expect("a binary")
    }

    #[test]
    fn a_binary_shows_its_bytes_and_a_partial_last_one() {
        let mut heap = Heap::default();
        let b = owner(&mut heap, &[1, 2, 3], 24);
        assert_eq!(text(&heap, b), "<<1, 2, 3>>");
        let b = owner(&mut heap, &[0xFF, 0b1011_1111], 11);
        assert_eq!(text(&heap, b), "<<255, 5:size(3)>>");
        let b = owner(&mut heap, &[], 0);
        assert_eq!(text(&heap, b), "<<>>");
    }

    #[test]
    fn from_int_writes_the_low_bits_most_significant_first() {
        assert_eq!(from_int(&1.into(), 4), vec![0b0001_0000]);
        assert_eq!(from_int(&65.into(), 8), vec![65]);
        assert_eq!(from_int(&0x1234.into(), 16), vec![0x12, 0x34]);
        assert_eq!(from_int(&0x1FF.into(), 8), vec![0xFF]);
        assert_eq!(from_int(&(-1).into(), 12), vec![0xFF, 0xF0]);
        assert_eq!(from_int(&7.into(), 0), Vec::<u8>::new());
        let big: BigInt = BigInt::from(1) << 100;
        let wide = from_int(&big, 104);
        assert_eq!(wide.len(), 13);
        assert_eq!(wide[0], 0x10);
    }

    /// A slice of a slice reads the right bits of the owner, at any offset.
    #[test]
    fn slices_read_through_to_the_owner() {
        let mut heap = Heap::default();
        let b = owner(&mut heap, &[0b1010_1100, 0b0011_0101, 0xFF], 24);
        let s1 = slice(&mut heap, b, 3, 17).expect("room");
        let s1 = bits(&heap, s1).expect("a slice");
        let s2 = slice(&mut heap, s1, 2, 9).expect("room");
        let s2 = bits(&heap, s2).expect("a slice");
        // Bits 5..14 of the owner: 1 0 0 0 0 1 1 0 1.
        assert_eq!(bytes(&heap, s2), vec![0b1000_0110, 0b1000_0000]);
        assert_eq!(read_uint(&heap, s2, 0, 9), BigInt::from(0b1_0000_1101));
        assert_eq!(read_uint(&heap, s2, 5, 8), BigInt::from(0b1101_0000));
        let want = owner(&mut heap, &[0b1000_0110, 0b1000_0000], 9);
        assert!(equal(&heap, s2, want));
    }

    #[test]
    fn join_packs_at_any_bit() {
        let mut heap = Heap::default();
        let a = owner(&mut heap, &[0b1010_0000], 3);
        let b = owner(&mut heap, &[0xFF, 0b1000_0000], 9);
        let j = join(&mut heap, &[a, b, a]).expect("room");
        let j = bits(&heap, j).expect("a binary");
        assert_eq!(j.len, 15);
        // 101, then nine 1s, then 101.
        assert_eq!(bytes(&heap, j), vec![0b1011_1111, 0b1111_1010]);
    }

    #[test]
    fn utf8_reads_one_code_point_or_none() {
        let mut heap = Heap::default();
        let b = owner(&mut heap, "é!".as_bytes(), 24);
        assert_eq!(read_utf8(&heap, b, 0), Some(('é' as u32, 16)));
        assert_eq!(read_utf8(&heap, b, 16), Some(('!' as u32, 8)));
        assert_eq!(read_utf8(&heap, b, 8), None);
        assert_eq!(read_utf8(&heap, b, 24), None);
    }
}
