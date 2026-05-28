#[inline]
pub fn is_name_char(c: u8) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_uppercase() || c == b'_' || c.is_ascii_digit()
}

#[inline]
pub fn is_quote(c: u8) -> bool {
    c == b'\'' || c == b'"'
}
