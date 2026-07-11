//! MSB-first bit-granular primitives over `[u8]`.
//!
//! Bits are addressed big-endian within each byte: bit `i` lives at byte
//! `i / 8`, position `7 - (i % 8)`. This is the single definition of that
//! addressing — the `Binary` value layout, the VM's bit-string ops, and the
//! bit-pattern hasher all read and write bits through here, so a change to the
//! bit order (or a fix to the two-byte splice) has exactly one edit site.

/// Read bit `i` (MSB-first). Caller guarantees `i / 8` is in bounds.
#[inline]
pub fn get_bit(bytes: &[u8], i: u64) -> u8 {
    (bytes[(i / 8) as usize] >> (7 - (i % 8))) & 1
}

/// Write bit `i` (MSB-first) to `bit & 1`. Caller guarantees `i / 8` is in
/// bounds. The target bit is cleared first, so this is a true overwrite (not
/// an OR) and is safe on a non-zero destination.
#[inline]
pub fn set_bit(bytes: &mut [u8], i: u64, bit: u8) {
    let idx = (i / 8) as usize;
    let shift = 7 - (i % 8) as u32;
    bytes[idx] = (bytes[idx] & !(1 << shift)) | ((bit & 1) << shift);
}

/// Read the 8 bits starting at bit `at` as one byte, MSB-first. Bits past the
/// end of `bytes` read as **zero** — the second source byte is fetched with
/// `.get()`, never a panicking index — so a read whose window straddles the
/// buffer end is well-defined. Callers that need strict bounds mask the
/// result through [`tail_mask`].
#[inline]
pub fn read_byte(bytes: &[u8], at: u64) -> u8 {
    let idx = (at / 8) as usize;
    let shift = (at % 8) as u32;
    let hi = bytes.get(idx).copied().unwrap_or(0);
    if shift == 0 {
        hi
    } else {
        let lo = bytes.get(idx + 1).copied().unwrap_or(0);
        (hi << shift) | (lo >> (8 - shift))
    }
}

/// MSB-first mask selecting the logical bits of a partial trailing byte, or
/// `None` when `bit_len` is a whole number of bytes. This is the single
/// definition of "the masked partial tail": aligned-vec materialisation,
/// logical-bit equality, and the value hasher all mask through it, which is
/// what keeps equality and hashing consistent over the same logical bits.
#[inline]
pub fn tail_mask(bit_len: u64) -> Option<u8> {
    let rem = (bit_len % 8) as u32;
    if rem == 0 {
        None
    } else {
        Some(0xFFu8 << (8 - rem))
    }
}

/// Copy `n` logical bits of `src` starting at bit `src_at` into `dst` starting
/// at bit `dst_at` (MSB-first). Caller guarantees `dst` holds bits
/// `[dst_at, dst_at + n)` and `src` holds bits `[src_at, src_at + n)`; unlike
/// [`read_byte`], an out-of-window access panics. The all-byte-aligned span is
/// a `memcpy`; only ragged edges take the per-bit loop, so an N-way concat
/// through this is O(total bits). Writes overwrite (not OR), so `dst` need not
/// be pre-zeroed.
pub fn copy_bits(dst: &mut [u8], dst_at: u64, src: &[u8], src_at: u64, n: u64) {
    let mut done = 0u64;
    if dst_at.is_multiple_of(8) && src_at.is_multiple_of(8) {
        let full = (n / 8) as usize;
        let (d, s) = ((dst_at / 8) as usize, (src_at / 8) as usize);
        dst[d..d + full].copy_from_slice(&src[s..s + full]);
        done = full as u64 * 8;
    }
    for i in done..n {
        set_bit(dst, dst_at + i, get_bit(src, src_at + i));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Extract `n` bits at `at` into a fresh zeroed buffer — the shape callers
    /// use to materialise a bit-string view.
    fn extract(src: &[u8], at: u64, n: u64) -> Vec<u8> {
        let mut out = vec![0u8; n.div_ceil(8) as usize];
        copy_bits(&mut out, 0, src, at, n);
        out
    }

    /// Concatenate two bit-strings, as `bin_concat_n` does into its backing.
    fn concat(a: &[u8], a_bits: u64, b: &[u8], b_bits: u64) -> Vec<u8> {
        let mut out = vec![0u8; (a_bits + b_bits).div_ceil(8) as usize];
        copy_bits(&mut out, 0, a, 0, a_bits);
        copy_bits(&mut out, a_bits, b, 0, b_bits);
        out
    }

    #[test]
    fn copy_bits_byte_aligned_is_memcpy() {
        assert_eq!(extract(&[0xAB, 0xCD, 0xEF], 8, 16), vec![0xCD, 0xEF]);
    }

    #[test]
    fn copy_bits_unaligned_source() {
        // 1010_1011 1100_1101, 8 bits from bit 4 → 1011_1100
        assert_eq!(extract(&[0xAB, 0xCD], 4, 8), vec![0xBC]);
    }

    #[test]
    fn copy_bits_leaves_partial_tail_untouched() {
        // 3 bits from bit 0 of 1111_1111 → 1110_0000; the 5 tail bits of the
        // zeroed destination are not written.
        assert_eq!(extract(&[0xFF], 0, 3), vec![0b1110_0000]);
    }

    #[test]
    fn copy_bits_overwrites_rather_than_ors() {
        // A non-zero destination: the copied span is replaced, and only it.
        let mut dst = [0xFF, 0xFF];
        copy_bits(&mut dst, 4, &[0x00], 0, 8);
        assert_eq!(dst, [0b1111_0000, 0b0000_1111]);
    }

    #[test]
    fn copy_bits_concat_aligned() {
        assert_eq!(concat(&[0xAB], 8, &[0xCD], 8), vec![0xAB, 0xCD]);
    }

    #[test]
    fn copy_bits_concat_unaligned_destination() {
        // 4 bits 1010 ++ 8 bits 1100_1101 = 12 bits 1010_1100_1101
        let out = concat(&[0b1010_0000], 4, &[0xCD], 8);
        assert_eq!(out, vec![0b1010_1100, 0b1101_0000]);
    }

    #[test]
    fn copy_bits_concat_then_extract_roundtrip() {
        let joined = concat(&[0b1011_0000], 4, &[0xFF, 0b1000_0000], 9); // 13 bits
        assert_eq!(extract(&joined, 0, 4), vec![0b1011_0000]);
        assert_eq!(extract(&joined, 4, 9), vec![0xFF, 0b1000_0000]);
    }

    #[test]
    fn copy_bits_zero_length_is_a_noop() {
        let mut dst = [0xAA];
        copy_bits(&mut dst, 3, &[0xFF], 0, 0);
        assert_eq!(dst, [0xAA]);
    }
}
