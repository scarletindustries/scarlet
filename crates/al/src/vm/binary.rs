//! Bit-granular operations on `HeapValue::Binary`. Bits are addressed MSB-first:
//! bit `i` lives at byte `i / 8`, position
//! `7 - (i % 8)`. The trailing low bits of the last byte beyond `bit_len` are
//! always zero.

#[inline]
fn get_bit(bytes: &[u8], i: u64) -> u8 {
    let byte = bytes[(i / 8) as usize];
    (byte >> (7 - (i % 8))) & 1
}

#[inline]
fn set_bit(out: &mut [u8], i: u64, v: u8) {
    let idx = (i / 8) as usize;
    let shift = 7 - (i % 8);
    out[idx] |= (v & 1) << shift;
}

/// Extract `take` bits starting at bit offset `at`. Caller has already
/// bounds-checked `at + take <= bit_len`.
pub fn slice(bytes: &[u8], at: u64, take: u64) -> Vec<u8> {
    let out_bytes = take.div_ceil(8) as usize;
    let mut out = vec![0u8; out_bytes];
    if at.is_multiple_of(8) {
        // Byte-aligned source: memcpy then mask the trailing partial byte.
        let start = (at / 8) as usize;
        let copy_len = out_bytes.min(bytes.len() - start);
        out[..copy_len].copy_from_slice(&bytes[start..start + copy_len]);
    } else {
        for i in 0..take {
            set_bit(&mut out, i, get_bit(bytes, at + i));
        }
    }
    mask_tail(&mut out, take);
    out
}

/// Concatenate two bitstrings. `a_bits` is the bit length of `a`; the result
/// has bit length `a_bits + b_bits`.
pub fn append(a: &[u8], a_bits: u64, b: &[u8], b_bits: u64) -> Vec<u8> {
    let total = a_bits + b_bits;
    let out_bytes = total.div_ceil(8) as usize;
    if a_bits.is_multiple_of(8) {
        let mut out = Vec::with_capacity(out_bytes);
        out.extend_from_slice(&a[..(a_bits / 8) as usize]);
        out.extend_from_slice(&b[..b_bits.div_ceil(8) as usize]);
        // `b`'s tail is already masked by invariant; nothing more to do.
        return out;
    }
    let mut out = vec![0u8; out_bytes];
    out[..(a_bits.div_ceil(8) as usize)].copy_from_slice(a);
    // `a`'s last byte already has its low bits zeroed (invariant), so we can
    // OR `b`'s bits in without clearing first.
    for i in 0..b_bits {
        set_bit(&mut out, a_bits + i, get_bit(b, i));
    }
    out
}

#[inline]
fn mask_tail(out: &mut [u8], bit_len: u64) {
    let rem = (bit_len % 8) as u8;
    if rem != 0
        && let Some(last) = out.last_mut()
    {
        *last &= 0xFFu8 << (8 - rem);
    }
}

pub fn inspect(bytes: &[u8], bit_len: u64) -> String {
    let full = (bit_len / 8) as usize;
    let mut parts: Vec<String> = bytes[..full].iter().map(|b| b.to_string()).collect();
    let rem = (bit_len % 8) as u8;
    if rem != 0 {
        let last = bytes[full] >> (8 - rem);
        parts.push(format!("{}:size({})", last, rem));
    }
    format!("<<{}>>", parts.join(", "))
}

/// Encode the low `num_bits` of `value` big-endian (MSB-first) into a fresh
/// bit-string. Bits beyond 64 are zero; `num_bits == 0` yields an empty
/// bit-string. Total: never panics, never errors.
pub fn from_int(value: i64, num_bits: u64) -> Vec<u8> {
    let mut out = vec![0u8; num_bits.div_ceil(8) as usize];
    let v = value as u64;
    for i in 0..num_bits {
        let pos = num_bits - 1 - i;
        let bit = if pos < 64 { ((v >> pos) & 1) as u8 } else { 0 };
        set_bit(&mut out, i, bit);
    }
    out
}

/// Decode `num_bits` starting at bit `at` as an unsigned big-endian integer.
/// Bits outside `[0, bit_len)` read as zero and a width over 64 wraps into
/// `i64` (consistent with the language's wrapping integer arithmetic). Total.
pub fn read_int(bytes: &[u8], bit_len: u64, at: u64, num_bits: u64) -> i64 {
    let mut v: u128 = 0;
    for i in 0..num_bits {
        let idx = at + i;
        let bit = if idx < bit_len {
            get_bit(bytes, idx)
        } else {
            0
        };
        v = (v << 1) | bit as u128;
    }
    v as i64
}

/// Decode one UTF-8 codepoint starting at bit `at`. Returns `(codepoint,
/// bits_consumed)` or `None` if there are too few bits or the bytes are not
/// valid UTF-8. Total.
pub fn read_utf8(bytes: &[u8], bit_len: u64, at: u64) -> Option<(u32, u64)> {
    if at + 8 > bit_len {
        return None;
    }
    let b0 = slice(bytes, at, 8)[0];
    let len: u64 = if b0 < 0x80 {
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
    let nbits = len * 8;
    if at + nbits > bit_len {
        return None;
    }
    let buf = slice(bytes, at, nbits);
    let s = std::str::from_utf8(&buf).ok()?;
    let mut chars = s.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    Some((c as u32, nbits))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slice_byte_aligned() {
        let s = slice(&[0xAB, 0xCD, 0xEF], 8, 16);
        assert_eq!(s, vec![0xCD, 0xEF]);
    }

    #[test]
    fn slice_unaligned() {
        // bytes = 1010_1011 1100_1101, take 8 bits from bit 4 → 1011_1100
        let s = slice(&[0xAB, 0xCD], 4, 8);
        assert_eq!(s, vec![0xBC]);
    }

    #[test]
    fn slice_partial_tail_masked() {
        // take 3 bits from bit 0 of 1111_1111 → 1110_0000
        let s = slice(&[0xFF], 0, 3);
        assert_eq!(s, vec![0b1110_0000]);
    }

    #[test]
    fn append_aligned() {
        let out = append(&[0xAB], 8, &[0xCD], 8);
        assert_eq!(out, vec![0xAB, 0xCD]);
    }

    #[test]
    fn append_unaligned() {
        // 4 bits 1010 ++ 8 bits 1100_1101 = 12 bits 1010_1100_1101
        let out = append(&[0b1010_0000], 4, &[0xCD], 8);
        assert_eq!(out, vec![0b1010_1100, 0b1101_0000]);
    }

    #[test]
    fn append_then_slice_roundtrip() {
        let a = [0b1011_0000];
        let b = [0xFF, 0b1000_0000];
        let joined = append(&a, 4, &b, 9); // 13 bits
        // slice back the original pieces
        assert_eq!(slice(&joined, 0, 4), vec![0b1011_0000]);
        assert_eq!(slice(&joined, 4, 9), vec![0xFF, 0b1000_0000]);
    }

    #[test]
    fn inspect_aligned_and_partial() {
        assert_eq!(inspect(&[1, 2, 3], 24), "<<1, 2, 3>>");
        assert_eq!(inspect(&[0xFF, 0b1010_0000], 11), "<<255, 5:size(3)>>");
        assert_eq!(inspect(&[], 0), "<<>>");
    }

    #[test]
    fn from_int_widths() {
        assert_eq!(from_int(1, 4), vec![0b0001_0000]);
        assert_eq!(from_int(2, 4), vec![0b0010_0000]);
        assert_eq!(from_int(65, 8), vec![65]);
        assert_eq!(from_int(0x1234, 16), vec![0x12, 0x34]);
        assert_eq!(from_int(7, 0), Vec::<u8>::new());
        // `<<1:4, 2:4>>` packs to a single 18 byte.
        let packed = append(&from_int(1, 4), 4, &from_int(2, 4), 4);
        assert_eq!(packed, vec![18]);
    }

    #[test]
    fn read_int_roundtrip() {
        // "AB" = [0x41, 0x42]; two default 8-bit segments.
        assert_eq!(read_int(&[0x41, 0x42], 16, 0, 8), 65);
        assert_eq!(read_int(&[0x41, 0x42], 16, 8, 8), 66);
        assert_eq!(read_int(&[0b0001_0010], 8, 0, 4), 1);
        assert_eq!(read_int(&[0b0001_0010], 8, 4, 4), 2);
        // Past the end reads as zero (total).
        assert_eq!(read_int(&[0xFF], 8, 4, 8), 0b1111_0000);
    }

    #[test]
    fn read_utf8_codepoints() {
        let s = "aé€".as_bytes();
        let bits = (s.len() as u64) * 8;
        let (c0, n0) = read_utf8(s, bits, 0).unwrap();
        assert_eq!((c0, n0), ('a' as u32, 8));
        let (c1, n1) = read_utf8(s, bits, n0).unwrap();
        assert_eq!((c1, n1), ('é' as u32, 16));
        let (c2, n2) = read_utf8(s, bits, n0 + n1).unwrap();
        assert_eq!((c2, n2), ('€' as u32, 24));
        assert_eq!(read_utf8(&[0xFF], 8, 0), None);
        assert_eq!(read_utf8(&[], 0, 0), None);
    }
}
