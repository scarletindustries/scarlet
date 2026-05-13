use super::keywords::is_keyword;

#[inline]
pub fn is_name_char(c: u8) -> bool {
    c.is_ascii_lowercase() || is_uppercase_ascii(c) || c == b'_' || c.is_ascii_digit()
}

#[inline]
pub fn is_uppercase_ascii(c: u8) -> bool {
    c.is_ascii_uppercase()
}

#[inline]
pub fn is_type_identifier(identifier: &str) -> bool {
    is_uppercase_ascii(identifier.as_bytes()[0])
}

#[inline]
pub fn is_valid_identifier(identifier: &str, is_fully_qualified: bool) -> bool {
    let bytes = identifier.as_bytes();
    if bytes.is_empty() {
        return false;
    }

    if is_fully_qualified && is_keyword(identifier) {
        return false;
    }

    if !bytes[0].is_ascii_alphabetic() && bytes[0] != b'_' {
        return false;
    }

    for &b in &bytes[1..] {
        if !is_name_char(b) {
            return false;
        }
    }

    true
}

#[inline]
pub fn is_quote(c: u8) -> bool {
    c == b'\'' || c == b'`'
}
