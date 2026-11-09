/// The identifier grammar's leading-character predicate: `[A-Za-z_]`.
#[inline]
pub fn is_name_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}

/// The identifier grammar's continuation-character predicate: `[A-Za-z0-9_]`.
#[inline]
pub fn is_name_continue(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// A leading uppercase ASCII letter marks a type or constructor name rather
/// than a value name. This is the language's only case rule, and both the
/// parser and the type hydrator use it.
#[inline]
pub fn is_type_name(s: &str) -> bool {
    s.as_bytes().first().is_some_and(|b| b.is_ascii_uppercase())
}
