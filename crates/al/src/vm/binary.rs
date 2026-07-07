//! Bit-granular operations on `HeapValue::Binary`. Bits are addressed MSB-first
//! (see [`al_core::bytecode::bits`]); the trailing low bits of the last byte
//! beyond `bit_len` are always zero.

#[cfg(test)]
use al_core::bytecode::bits::copy_bits;
use al_core::bytecode::bits::{get_bit, read_byte};

/// Extract `take` bits starting at bit offset `at` into a fresh, offset-0
/// buffer with a zeroed tail. Test helper.
#[cfg(test)]
fn slice(bytes: &[u8], at: u64, take: u64) -> Vec<u8> {
    let mut out = vec![0u8; take.div_ceil(8) as usize];
    copy_bits(&mut out, 0, bytes, at, take);
    out
}

/// Concatenate two bitstrings. Test helper — production concat is
/// `Op::BinConcatN` (`vm::text::bin_concat_n`), which builds the result
/// directly in a shared backing without an intermediate `Vec`.
#[cfg(test)]
fn append(a: &[u8], a_bits: u64, b: &[u8], b_bits: u64) -> Vec<u8> {
    let total = a_bits + b_bits;
    let mut out = vec![0u8; total.div_ceil(8) as usize];
    copy_bits(&mut out, 0, a, 0, a_bits);
    copy_bits(&mut out, a_bits, b, 0, b_bits);
    out
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
    let n = num_bits.div_ceil(8) as usize;
    let mut out = vec![0u8; n];
    if num_bits == 0 {
        return out;
    }
    let v = value as u64;
    let low = if num_bits < 64 {
        v & ((1u64 << num_bits) - 1)
    } else {
        v
    };
    // Left-align the low `num_bits` of the value: the segment ends at bit
    // `num_bits`, leaving `tail` zero pad bits in the last byte. The shifted
    // value spans at most 71 bits, so the low 16 bytes of a u128 cover it;
    // any output bytes before that stay zero (bits beyond 64 encode as zero).
    let tail = (8 * n) as u64 - num_bits;
    let be = ((low as u128) << tail).to_be_bytes();
    let take = n.min(16);
    out[n - take..].copy_from_slice(&be[16 - take..]);
    out
}

/// Decode `num_bits` starting at bit `at` as an unsigned big-endian integer.
/// Bits outside `[0, bit_len)` read as zero and a width over 64 wraps into
/// `i64` (consistent with the language's wrapping integer arithmetic). Total.
pub fn read_int(bytes: &[u8], bit_len: u64, at: u64, num_bits: u64) -> i64 {
    // Fast path: byte-aligned start and a width that fits one word — covers
    // the dominant 8/16/32/64-bit segments of byte-aligned binaries with a
    // clamped byte copy instead of a per-bit loop. `bytes` may be a shared
    // backing extending past `bit_len` (the view's logical end), so bytes and
    // bits past `bit_len` must read as zero, never copied through.
    if at.is_multiple_of(8) && (1..=64).contains(&num_bits) {
        let mut buf = [0u8; 8];
        if at < bit_len {
            let start = (at / 8) as usize;
            let want = num_bits.div_ceil(8) as usize;
            let avail =
                ((bit_len - at).div_ceil(8) as usize).min(bytes.len().saturating_sub(start));
            let copy = want.min(avail);
            buf[..copy].copy_from_slice(&bytes[start..start + copy]);
            // Zero the bits of the last copied byte at or past `bit_len`.
            let rem = ((bit_len - at) % 8) as u8;
            if rem != 0 && at + 8 * copy as u64 > bit_len {
                buf[copy - 1] &= 0xFFu8 << (8 - rem);
            }
        }
        return (u64::from_be_bytes(buf) >> (64 - num_bits)) as i64;
    }
    // Misaligned (or zero/over-64-bit width) fallback: one bit per iteration.
    // Only the low 64 bits of the accumulator survive the `as i64`, so a width
    // over 64 reads only the last 64 bits — the loop is capped and a hostile
    // width cannot spin.
    let skip = num_bits.saturating_sub(64);
    let mut v: u64 = 0;
    for i in skip..num_bits {
        let idx = at + i;
        let bit = if idx < bit_len {
            get_bit(bytes, idx)
        } else {
            0
        };
        v = (v << 1) | bit as u64;
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
    let b0 = read_byte(bytes, at);
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
    let mut buf = [0u8; 4];
    let n = len as usize;
    if at.is_multiple_of(8) {
        let start = (at / 8) as usize;
        buf[..n].copy_from_slice(&bytes[start..start + n]);
    } else {
        for i in 0..len {
            buf[i as usize] = read_byte(bytes, at + i * 8);
        }
    }
    // The lead byte fixes the sequence length, so a valid `buf[..n]`
    // contains exactly one codepoint.
    let s = std::str::from_utf8(&buf[..n]).ok()?;
    let c = s.chars().next()?;
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
    fn read_int_aligned_word_widths() {
        let b = [0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0, 0x11];
        assert_eq!(read_int(&b, 72, 0, 16), 0x1234);
        assert_eq!(read_int(&b, 72, 8, 32), 0x3456789A);
        assert_eq!(read_int(&b, 72, 0, 64), 0x123456789ABCDEF0u64 as i64);
        assert_eq!(read_int(&b, 72, 8, 64), 0x3456789ABCDEF011u64 as i64);
        // Aligned start, sub-byte width.
        assert_eq!(read_int(&b, 72, 8, 4), 0x3);
        // Aligned start reading past `bit_len` zero-fills, even though the
        // backing slice has live bytes there (view semantics).
        assert_eq!(read_int(&b, 16, 8, 16), 0x3400);
        assert_eq!(read_int(&b, 12, 0, 16), 0x1230);
        assert_eq!(read_int(&b, 12, 8, 8), 0x30);
        assert_eq!(read_int(&b, 72, 72, 8), 0);
        assert_eq!(read_int(&b, 72, 0, 0), 0);
    }

    #[test]
    fn from_int_word_widths() {
        assert_eq!(
            from_int(0x123456789ABCDEF0u64 as i64, 64),
            vec![0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0]
        );
        assert_eq!(from_int(-1, 64), vec![0xFF; 8]);
        // Width over 64: leading bits are zero.
        assert_eq!(from_int(-1, 72), [vec![0x00], vec![0xFF; 8]].concat());
        assert_eq!(from_int(-1, 68), {
            let mut v = vec![0x0F];
            v.extend(vec![0xFF; 7]);
            v.push(0xF0);
            v
        });
        // Sub-byte tail: low bits masked into the top of the last byte.
        assert_eq!(from_int(0x1FF, 9), vec![0xFF, 0b1000_0000]);
        // Value wider than the segment is truncated to the low bits.
        assert_eq!(from_int(0x1AB, 8), vec![0xAB]);
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
        // Truncated multi-byte sequence.
        assert_eq!(read_utf8(&"€".as_bytes()[..2], 16, 0), None);
    }

    #[test]
    fn read_utf8_unaligned() {
        let s = "aé€".as_bytes();
        let s_bits = (s.len() as u64) * 8;
        let shifted = append(&[0u8], 4, s, s_bits);
        let bits = 4 + s_bits;
        let (c0, n0) = read_utf8(&shifted, bits, 4).unwrap();
        assert_eq!((c0, n0), ('a' as u32, 8));
        let (c1, n1) = read_utf8(&shifted, bits, 4 + n0).unwrap();
        assert_eq!((c1, n1), ('é' as u32, 16));
        let (c2, n2) = read_utf8(&shifted, bits, 4 + n0 + n1).unwrap();
        assert_eq!((c2, n2), ('€' as u32, 24));
    }
}
