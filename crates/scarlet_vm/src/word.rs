//! Taking a heap word apart. Each piece comes out of the word's bytes, so no
//! cast can drop a bit the caller meant to keep.

/// Bits 0 to 31.
pub(crate) fn low_u32(w: u64) -> u32 {
    let [a, b, c, d, ..] = w.to_le_bytes();
    u32::from_le_bytes([a, b, c, d])
}

/// Bits 32 to 63.
pub(crate) fn high_u32(w: u64) -> u32 {
    let [.., a, b, c, d] = w.to_le_bytes();
    u32::from_le_bytes([a, b, c, d])
}

/// Bits 0 to 15.
pub(crate) fn low_u16(w: u64) -> u16 {
    let [a, b, ..] = w.to_le_bytes();
    u16::from_le_bytes([a, b])
}

/// Bits 0 to 7.
pub(crate) fn low_byte(w: u64) -> u8 {
    let [a, ..] = w.to_le_bytes();
    a
}

/// A length or index read from a heap word. `lib.rs` refuses to build the VM
/// for a target narrower than 64 bits, so every `u64` fits.
#[expect(
    clippy::cast_possible_truncation,
    reason = "the VM only builds for 64-bit targets, where usize is u64"
)]
pub(crate) fn to_usize(w: u64) -> usize {
    w as usize
}
