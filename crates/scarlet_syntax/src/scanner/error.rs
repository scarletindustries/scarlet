//! What the scanner can find wrong with the text.

use std::fmt;

use crate::diagnostic::{DiagnosticCode, Problem};

/// The base of a prefixed number literal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Radix {
    Hex,
    Binary,
}

impl fmt::Display for Radix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Radix::Hex => "hex",
            Radix::Binary => "binary",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanError {
    /// A character no token starts with, as the source spells it.
    UnexpectedChar(String),
    /// `0x` or `0b` with no digit after it.
    NoDigits(Radix),
    /// A letter or digit after a prefixed number that isn't a digit of it.
    BadDigit(Radix),
    /// The input ends inside a `/* */` comment.
    UnclosedBlockComment,
    /// A string runs into the end of its line.
    UnclosedString,
    /// The input ends inside an interpolated string's `${ }`.
    UnclosedInterpolation,
    /// A backslash before something that isn't an escape: that something.
    UnknownEscape(String),
    /// `\u` with no `{` after it.
    UnicodeEscapeWithoutBrace,
    /// `\u{` with no `}` after its digits.
    UnclosedUnicodeEscape { digits: String },
    /// `\u{}`.
    EmptyUnicodeEscape,
    /// `\u{..}` whose digits name no Unicode scalar value.
    NotAScalarValue { digits: String },
}

impl Problem for ScanError {
    fn code(&self) -> DiagnosticCode {
        match self {
            // The input stopped while a construct was still open, so a
            // reader that can ask for more input should.
            ScanError::UnclosedBlockComment | ScanError::UnclosedInterpolation => {
                DiagnosticCode::UnexpectedEof
            }
            ScanError::UnexpectedChar(_)
            | ScanError::NoDigits(_)
            | ScanError::BadDigit(_)
            | ScanError::UnclosedString
            | ScanError::UnknownEscape(_)
            | ScanError::UnicodeEscapeWithoutBrace
            | ScanError::UnclosedUnicodeEscape { .. }
            | ScanError::EmptyUnicodeEscape
            | ScanError::NotAScalarValue { .. } => DiagnosticCode::ParseError,
        }
    }
}

impl fmt::Display for ScanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScanError::UnexpectedChar(c) => write!(f, "Unexpected character '{c}'"),
            ScanError::NoDigits(r) => write!(f, "{r} literal has no digits"),
            ScanError::BadDigit(r) => write!(f, "invalid digit in {r} literal"),
            ScanError::UnclosedBlockComment => {
                write!(f, "This block comment is never closed. End it with `*/`")
            }
            ScanError::UnclosedString | ScanError::UnclosedInterpolation => {
                write!(
                    f,
                    "This string is never closed. End it with a matching quote"
                )
            }
            ScanError::UnknownEscape(e) => write!(
                f,
                "Unknown escape sequence '\\{e}'. Write '\\\\' for a backslash"
            ),
            ScanError::UnicodeEscapeWithoutBrace => {
                write!(f, "Expected '{{' after '\\u', like '\\u{{E9}}'")
            }
            ScanError::UnclosedUnicodeEscape { digits } => write!(
                f,
                "The '\\u{{{digits}' escape is never closed. End it with '}}'"
            ),
            ScanError::EmptyUnicodeEscape => write!(
                f,
                "The '\\u{{}}' escape is empty. Put hex digits inside, like '\\u{{E9}}'"
            ),
            ScanError::NotAScalarValue { digits } => write!(
                f,
                "'\\u{{{digits}}}' is not a Unicode scalar value, because it is above \
                 U+10FFFF or in the surrogate range U+D800 to U+DFFF"
            ),
        }
    }
}
