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
/// at bit `dst_at` (MSB-first). The all-byte-aligned span is a `memcpy`; only
/// ragged edges take the per-bit loop, so an N-way concat through this is
/// O(total bits). Writes overwrite (not OR), so `dst` need not be pre-zeroed.
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
