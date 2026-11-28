//! Binaries, which are strings of bits. They work as the old VM's did (that
//! was `vm/binary.rs`, before the rewrite).
//!
//! Bits are counted from the most significant bit of the first byte, and the
//! bits past a binary's end in its last byte are zero.
//!
//! A binary is one of two kinds of cell:
//!
//! - An **owner** holds the bits. It has a word with its length in bits, then
//!   its bytes, eight to a word, with the first byte in the lowest bits.
//! - A **slice** holds a reference to an owner, a bit offset and a length in
//!   bits. `binary.slice_bits` and a pattern's `rest:binary` make one, so they
//!   copy nothing, as their docs promise. A slice of a slice points at the
//!   owner, so there is never a chain of them.
//!
//! [`Bits`] stands for either, meaning "these bits of this owner". Everything
//! that reads a binary reads it through [`Bits`], so which kind it is doesn't
//! matter.

use num_bigint::{BigInt, Sign};

use crate::heap::{Cell, Fault, Heap, Kind};
use crate::value::Value;
use crate::word::{low_byte, to_usize};

/// The most bits one binary can hold, because a cell is at most
/// [`crate::heap::MAX_CELL_WORDS`] words and a word holds 64 bits. A longer
/// one makes the heap full instead.
pub(crate) const MAX_BITS: u64 = crate::heap::MAX_CELL_WORDS as u64 * 64;

/// `len` bits of `owner`'s bytes, from bit `at`.
#[derive(Clone, Copy)]
pub(crate) struct Bits {
    owner: Cell,
    at: u64,
    pub(crate) len: u64,
}

/// The most bytes a binary can hold. That is a cell's worth of words, less
/// its header and the word holding its length.
pub(crate) const MAX_BYTES: usize = (crate::heap::MAX_CELL_WORDS - 2) * 8;

/// The bits `cell` holds, or `None` when it is not a binary.
pub(crate) fn bits(heap: &Heap, cell: Cell) -> Option<Bits> {
    let d = heap.binary_data(cell);
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
        Kind::Handle
        | Kind::String
        | Kind::BigInt
        | Kind::Ctor
        | Kind::Closure
        | Kind::Tuple
        | Kind::ArrayFlat
        | Kind::ArrayRoot
        | Kind::ArrayBranch
        | Kind::Range
        | Kind::Map
        | Kind::MapNode
        | Kind::MapCollision => None,
    }
}

/// A new owner holding the first `len` bits of `bytes`. The bits past `len`
/// in the last byte are cleared.
pub(crate) fn make(heap: &mut Heap, bytes: &[u8], len: u64) -> Result<Cell, Fault> {
    if len > MAX_BITS {
        return Err(Fault::Full);
    }
    let n = to_usize(len.div_ceil(8));
    let mut buf: Vec<u8> = bytes.iter().copied().take(n).collect();
    buf.resize(n, 0);
    // Keep the top `len % 8` bits of a last byte that is not whole.
    let rem = len % 8;
    if rem != 0
        && let Some(last) = buf.last_mut()
    {
        *last &= !(0xFFu8 >> rem);
    }
    let words = buf.chunks(8).map(|chunk| {
        let mut word = [0u8; 8];
        for (w, b) in word.iter_mut().zip(chunk) {
            *w = *b;
        }
        u64::from_le_bytes(word)
    });
    let data: Vec<u64> = std::iter::once(len).chain(words).collect();
    heap.make(Kind::Binary, &data)
}

/// How many bits `n` whole bytes are, or `None` past what a word counts.
pub(crate) fn bit_len(n: usize) -> Option<u64> {
    u64::try_from(n).ok()?.checked_mul(8)
}

/// A new owner holding all of `bytes`.
pub(crate) fn from_bytes(heap: &mut Heap, bytes: &[u8]) -> Result<Cell, Fault> {
    make(heap, bytes, bit_len(bytes.len()).ok_or(Fault::Full)?)
}

/// Bits `from .. from + len` of `b`, sharing its owner. The caller has
/// checked that they are inside it.
pub(crate) fn slice(heap: &mut Heap, b: Bits, from: u64, len: u64) -> Result<Cell, Fault> {
    let at = b.at.checked_add(from).ok_or(Fault::Full)?;
    heap.share(b.owner)?;
    heap.make(Kind::BinarySlice, &[Value::cell(b.owner).bits(), at, len])
}

/// Byte `i` of an owner's data words, or 0 past its end. The data's first
/// word is the owner's length, and its bytes start after it.
fn data_byte(data: &[u64], i: u64) -> u8 {
    match data.get(1..).and_then(|words| words.get(to_usize(i / 8))) {
        Some(w) => low_byte(w >> ((i % 8) << 3)),
        None => 0,
    }
}

/// Some [`Bits`] with their owner's words in hand. Whatever reads more than
/// one byte reads through this, so that it looks the owner up once and not
/// once for every byte.
#[derive(Clone, Copy)]
struct Opened<'h> {
    data: &'h [u64],
    at: u64,
    len: u64,
}

fn open(heap: &Heap, b: Bits) -> Opened<'_> {
    Opened {
        data: heap.binary_data(b.owner),
        at: b.at,
        len: b.len,
    }
}

impl Opened<'_> {
    /// The 8 bits from bit `i`. Bits past the end read as 0.
    fn byte(self, i: u64) -> u8 {
        let Some(left) = self.len.checked_sub(i).filter(|left| *left > 0) else {
            return 0;
        };
        let Some(p) = self.at.checked_add(i) else {
            return 0;
        };
        let s = p % 8;
        let hi = data_byte(self.data, p / 8);
        let raw = if s == 0 {
            hi
        } else {
            // The byte straddles two of the owner's, so it is the middle 8
            // bits of the pair.
            let lo = (p / 8)
                .checked_add(1)
                .map_or(0, |i| data_byte(self.data, i));
            let [top, _] = (u16::from_be_bytes([hi, lo]) << s).to_be_bytes();
            top
        };
        if left >= 8 {
            raw
        } else {
            raw & !(0xFFu8 >> left)
        }
    }
}

/// Word `k` of `b`'s bytes, little-endian. That is bytes `8k` to `8k + 7`,
/// with the first in the lowest bits. It is `None` when it runs past `b`'s
/// end. When `b` starts on a word of its owner, as a whole binary does, this
/// is that word unchanged, so a JSON tape (`json.rs`) reads a node without
/// copying the tape.
pub(crate) fn word(heap: &Heap, b: Bits, k: u64) -> Option<u64> {
    let end = k.checked_add(1)?.checked_mul(64)?;
    if end > b.len {
        return None;
    }
    if b.at.is_multiple_of(64) {
        let at = usize::try_from((b.at / 64).checked_add(k)?).ok()?;
        return heap.binary_data(b.owner).get(1..)?.get(at).copied();
    }
    let start = k.checked_mul(64)?;
    let b = open(heap, b);
    let mut bytes = [0u8; 8];
    for (out, i) in bytes.iter_mut().zip((start..end).step_by(8)) {
        *out = b.byte(i);
    }
    Some(u64::from_le_bytes(bytes))
}

/// Bytes `from` to `from + n` of `b`, or `None` when they run past its end.
pub(crate) fn byte_range(heap: &Heap, b: Bits, from: u64, n: u64) -> Option<Vec<u8>> {
    let end = from.checked_add(n)?.checked_mul(8)?;
    if end > b.len {
        return None;
    }
    let start = from.checked_mul(8)?;
    let b = open(heap, b);
    Some((start..end).step_by(8).map(|i| b.byte(i)).collect())
}

/// Up to `n` whole bytes of `b` from byte `from`, fewer where it ends first.
pub(crate) fn bytes_from(heap: &Heap, b: Bits, from: u64, n: u64) -> Vec<u8> {
    // Bit positions, clamped to the last whole byte's end.
    let whole = whole_bits(b);
    let start = from.saturating_mul(8).min(whole);
    let end = from.saturating_add(n).saturating_mul(8).min(whole);
    if !b.at.is_multiple_of(8) {
        let b = open(heap, b);
        return (start..end).step_by(8).map(|i| b.byte(i)).collect();
    }
    // `b` starts on a byte boundary of the owner, so each byte is one of the
    // owner's bytes unchanged.
    let d = heap.binary_data(b.owner);
    let base = b.at / 8;
    (start / 8..end / 8)
        .map(|k| base.checked_add(k).map_or(0, |i| data_byte(d, i)))
        .collect()
}

/// Whether bytes `from` to `from + text.len()` of `b` are `text`, read in
/// place.
pub(crate) fn bytes_are(heap: &Heap, b: Bits, from: u64, text: &[u8]) -> bool {
    let Some(end) = (text.len() as u64)
        .checked_add(from)
        .and_then(|end| end.checked_mul(8))
        .filter(|end| *end <= b.len)
    else {
        return false;
    };
    let Some(start) = from.checked_mul(8) else {
        return false;
    };
    let b = open(heap, b);
    text.iter()
        .zip((start..end).step_by(8))
        .all(|(&t, i)| b.byte(i) == t)
}

/// `b` as whole bytes, the last one padded with zero bits.
pub(crate) fn bytes(heap: &Heap, b: Bits) -> Vec<u8> {
    let b = open(heap, b);
    (0..b.len).step_by(8).map(|i| b.byte(i)).collect()
}

/// Where `b`'s last whole byte ends, in bits.
fn whole_bits(b: Bits) -> u64 {
    b.len & !7
}

/// `b`'s whole bytes. A last byte that is not whole is left out, which is how
/// the ASCII built-ins read it.
pub(crate) fn whole_bytes(heap: &Heap, b: Bits) -> Vec<u8> {
    let whole = whole_bits(b);
    let b = open(heap, b);
    (0..whole).step_by(8).map(|i| b.byte(i)).collect()
}

/// Whole byte `i` of `b`, or `None` past the last whole one.
pub(crate) fn whole_byte(heap: &Heap, b: Bits, i: u64) -> Option<u8> {
    (i < b.len / 8).then(|| i.checked_mul(8).map(|bit| open(heap, b).byte(bit)))?
}

/// Whether `a` and `b` hold the same bits.
pub(crate) fn equal(heap: &Heap, a: Bits, b: Bits) -> bool {
    let (a, b) = (open(heap, a), open(heap, b));
    a.len == b.len && (0..a.len).step_by(8).all(|i| a.byte(i) == b.byte(i))
}

/// Whether `b` holds `prefix` from bit `at`.
pub(crate) fn has_at(heap: &Heap, b: Bits, at: u64, prefix: Bits) -> bool {
    let Some(end) = at.checked_add(prefix.len) else {
        return false;
    };
    if end > b.len {
        return false;
    }
    let Some(at) = b.at.checked_add(at) else {
        return false;
    };
    let window = Bits {
        owner: b.owner,
        at,
        len: prefix.len,
    };
    equal(heap, window, prefix)
}

/// All of `parts`, one after another, as a new owner.
pub(crate) fn join(heap: &mut Heap, parts: &[Bits]) -> Result<Cell, Fault> {
    let len = parts
        .iter()
        .try_fold(0u64, |len, p| len.checked_add(p.len))
        .filter(|len| *len <= MAX_BITS)
        .ok_or(Fault::Full)?;
    let mut out = vec![0u8; to_usize(len.div_ceil(8))];
    // Every position below is inside `len`, which fits, so the sums are
    // exact.
    let mut at = 0u64;
    for p in parts {
        let p = open(heap, *p);
        for i in (0..p.len).step_by(8) {
            let chunk = p.byte(i);
            let n = p.len.saturating_sub(i).min(8);
            put(&mut out, at.saturating_add(i), chunk, n);
        }
        at = at.saturating_add(p.len);
    }
    make(heap, &out, len)
}

/// Write the top `n` bits of `chunk` into `out` from bit `at`.
fn put(out: &mut [u8], at: u64, chunk: u8, n: u64) {
    for j in 0..n {
        if chunk & (0x80 >> j) != 0 {
            let Some(p) = at.checked_add(j) else {
                return;
            };
            if let Some(b) = out.get_mut(to_usize(p / 8)) {
                *b |= 0x80 >> (p % 8);
            }
        }
    }
}

/// The low `width` bits of `n`, most significant first. A negative `n` is
/// written as its two's complement, extended as far as `width` asks, so
/// `<<-1:size(12)>>` is twelve ones.
#[expect(
    clippy::arithmetic_side_effects,
    reason = "Int arithmetic grows instead of overflowing, and the modulus is at least 1"
)]
pub(crate) fn from_int(n: &BigInt, width: u64) -> Vec<u8> {
    let bytes = to_usize(width.div_ceil(8));
    if bytes == 0 {
        return Vec::new();
    }
    let modulus = BigInt::from(1) << width;
    // `%` keeps the dividend's sign, so add the modulus back to land in
    // `0 .. 2^width`.
    let low = ((n % &modulus) + &modulus) % &modulus;
    // The zero bits after the value that fill its last byte.
    let pad = 8u64.abs_diff(width % 8) % 8;
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
        at: b.at.saturating_add(at.min(b.len)),
        len: b.len.saturating_sub(at).min(width),
    };
    let n = BigInt::from_bytes_be(Sign::Plus, &bytes(heap, window));
    // `bytes` pads the window to whole bytes. Drop the padding, then add the
    // zeros past `b`'s end that `width` still asks for. The window is no
    // wider than `width`.
    let pad = 8u64.abs_diff(window.len % 8) % 8;
    shift(n, pad, width.abs_diff(window.len))
}

/// `n` shifted down by `down` bits and then up by `up`.
#[expect(
    clippy::arithmetic_side_effects,
    reason = "an Int grows instead of overflowing when shifted"
)]
fn shift(n: BigInt, down: u64, up: u64) -> BigInt {
    (n >> down) << up
}

/// The UTF-8 code point at bit `at` of `b`, and how many bits it took, or
/// `None` when no valid one starts there.
pub(crate) fn read_utf8(heap: &Heap, b: Bits, at: u64) -> Option<(u32, u64)> {
    if at.checked_add(8)? > b.len {
        return None;
    }
    let b = open(heap, b);
    let b0 = b.byte(at);
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
    let bits = n.checked_mul(8)?;
    let end = at.checked_add(bits)?;
    if end > b.len {
        return None;
    }
    let buf: Vec<u8> = (at..end).step_by(8).map(|i| b.byte(i)).collect();
    let c = std::str::from_utf8(&buf).ok()?.chars().next()?;
    Some((c as u32, bits))
}

/// How a binary shows, like `<<1, 2, 3>>`. A last byte that is not whole
/// shows as `5:size(3)`.
pub(crate) fn text(heap: &Heap, b: Bits) -> String {
    let whole = whole_bits(b);
    let b = open(heap, b);
    let mut parts: Vec<String> = (0..whole)
        .step_by(8)
        .map(|i| b.byte(i).to_string())
        .collect();
    let rem = b.len % 8;
    if rem != 0 {
        let last = b.byte(whole) >> 8u64.abs_diff(rem);
        parts.push(format!("{last}:size({rem})"));
    }
    format!("<<{}>>", parts.join(", "))
}

#[cfg(test)]
mod tests;
