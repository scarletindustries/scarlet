use std::fmt;
use std::rc::Rc;

use super::keywords::Keyword;

/// Token kind. Text-bearing tokens carry their text as a payload, so an
/// identifier without a name (or a name without an identifier) cannot be
/// constructed. `Rc<str>` keeps `Kind` cheap to clone; equality and hashing
/// compare the text itself.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Kind {
    Eof,
    Error(Rc<str>),
    Identifier(Rc<str>),
    LiteralNumber(Rc<str>),
    LiteralString(Rc<str>),
    InterpStringStart,
    InterpStringPart(Rc<str>),
    InterpStringEnd,
    LogicalAnd,
    LogicalOr,
    BitwiseOr,
    Keyword(Keyword),
    PuncArrow,
    PuncComma,
    PuncColon,
    PuncSemicolon,
    PuncDot,
    PuncDotdot,
    PuncOpenParen,
    PuncCloseParen,
    PuncOpenBrace,
    PuncCloseBrace,
    PuncOpenBracket,
    PuncCloseBracket,
    BinOpen,
    BinClose,
    PuncQuestionMark,
    PuncExclamationMark,
    PuncAt,
    PuncEquals,
    PuncEqualsComparator,
    PuncNotEqual,
    PuncGt,
    PuncLt,
    PuncGte,
    PuncLte,
    PuncPlus,
    PuncPlusplus,
    PuncMinus,
    PuncMinusminus,
    PuncMul,
    PuncDiv,
    PuncMod,
}

/// Display for diagnostics, not source reconstruction: the payload for
/// text-bearing kinds, the fixed spelling for punctuation/operators/keywords,
/// and a human-readable description for `Eof` and the interpolation
/// delimiters (which have no single spelling). No quoting — error sites that
/// want quotes add their own.
impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Kind::Eof => "EOF",
            Kind::Error(s)
            | Kind::Identifier(s)
            | Kind::LiteralNumber(s)
            | Kind::LiteralString(s)
            | Kind::InterpStringPart(s) => s.as_ref(),
            Kind::InterpStringStart => "interpolated string start",
            Kind::InterpStringEnd => "interpolated string end",
            Kind::LogicalAnd => "&&",
            Kind::LogicalOr => "||",
            Kind::BitwiseOr => "|",
            Kind::Keyword(kw) => kw.text(),
            Kind::PuncArrow => "->",
            Kind::PuncComma => ",",
            Kind::PuncColon => ":",
            Kind::PuncSemicolon => ";",
            Kind::PuncDot => ".",
            Kind::PuncDotdot => "..",
            Kind::PuncOpenParen => "(",
            Kind::PuncCloseParen => ")",
            Kind::PuncOpenBrace => "{",
            Kind::PuncCloseBrace => "}",
            Kind::PuncOpenBracket => "[",
            Kind::PuncCloseBracket => "]",
            Kind::BinOpen => "<<",
            Kind::BinClose => ">>",
            Kind::PuncQuestionMark => "?",
            Kind::PuncExclamationMark => "!",
            Kind::PuncAt => "@",
            Kind::PuncEquals => "=",
            Kind::PuncEqualsComparator => "==",
            Kind::PuncNotEqual => "!=",
            Kind::PuncGt => ">",
            Kind::PuncLt => "<",
            Kind::PuncGte => ">=",
            Kind::PuncLte => "<=",
            Kind::PuncPlus => "+",
            Kind::PuncPlusplus => "++",
            Kind::PuncMinus => "-",
            Kind::PuncMinusminus => "--",
            Kind::PuncMul => "*",
            Kind::PuncDiv => "/",
            Kind::PuncMod => "%",
        };
        f.write_str(s)
    }
}

#[cfg(test)]
mod tests {
    use super::Kind;

    /// Every `Kind` whose `Display` output is its fixed source spelling.
    /// Kept in step with an exhaustive match over `Kind`, so adding a
    /// variant fails to compile until it is classified below — and
    /// classifying it as fixed-spelling means adding it to this list.
    fn fixed_spelling_kinds() -> Vec<Kind> {
        let mut kinds = vec![
            Kind::LogicalAnd,
            Kind::LogicalOr,
            Kind::BitwiseOr,
            Kind::PuncArrow,
            Kind::PuncComma,
            Kind::PuncColon,
            Kind::PuncSemicolon,
            Kind::PuncDot,
            Kind::PuncDotdot,
            Kind::PuncOpenParen,
            Kind::PuncCloseParen,
            Kind::PuncOpenBrace,
            Kind::PuncCloseBrace,
            Kind::PuncOpenBracket,
            Kind::PuncCloseBracket,
            Kind::BinOpen,
            Kind::BinClose,
            Kind::PuncQuestionMark,
            Kind::PuncExclamationMark,
            Kind::PuncAt,
            Kind::PuncEquals,
            Kind::PuncEqualsComparator,
            Kind::PuncNotEqual,
            Kind::PuncGt,
            Kind::PuncLt,
            Kind::PuncGte,
            Kind::PuncLte,
            Kind::PuncPlus,
            Kind::PuncPlusplus,
            Kind::PuncMinus,
            Kind::PuncMinusminus,
            Kind::PuncMul,
            Kind::PuncDiv,
            Kind::PuncMod,
        ];
        kinds.extend(super::Keyword::ALL.into_iter().map(Kind::Keyword));

        for kind in &kinds {
            match kind {
                // No fixed source spelling: payload-bearing or positional.
                Kind::Eof
                | Kind::Error(_)
                | Kind::Identifier(_)
                | Kind::LiteralNumber(_)
                | Kind::LiteralString(_)
                | Kind::InterpStringStart
                | Kind::InterpStringPart(_)
                | Kind::InterpStringEnd => {
                    unreachable!("{kind:?} must not be in the fixed-spelling list")
                }
                // Fixed spelling: must appear in the list above.
                Kind::LogicalAnd
                | Kind::LogicalOr
                | Kind::BitwiseOr
                | Kind::Keyword(_)
                | Kind::PuncArrow
                | Kind::PuncComma
                | Kind::PuncColon
                | Kind::PuncSemicolon
                | Kind::PuncDot
                | Kind::PuncDotdot
                | Kind::PuncOpenParen
                | Kind::PuncCloseParen
                | Kind::PuncOpenBrace
                | Kind::PuncCloseBrace
                | Kind::PuncOpenBracket
                | Kind::PuncCloseBracket
                | Kind::BinOpen
                | Kind::BinClose
                | Kind::PuncQuestionMark
                | Kind::PuncExclamationMark
                | Kind::PuncAt
                | Kind::PuncEquals
                | Kind::PuncEqualsComparator
                | Kind::PuncNotEqual
                | Kind::PuncGt
                | Kind::PuncLt
                | Kind::PuncGte
                | Kind::PuncLte
                | Kind::PuncPlus
                | Kind::PuncPlusplus
                | Kind::PuncMinus
                | Kind::PuncMinusminus
                | Kind::PuncMul
                | Kind::PuncDiv
                | Kind::PuncMod => {}
            }
        }
        kinds
    }

    /// Mirrors `keyword_roundtrip` in `keywords.rs`: the fixed spellings in
    /// `Kind`'s `Display` are hand-maintained separately from the scanner's
    /// dispatch, so scan each spelling with the real scanner and require the
    /// exact same kind back.
    #[test]
    fn fixed_spelling_roundtrip() {
        for kind in fixed_spelling_kinds() {
            let src = kind.to_string();
            let mut s = crate::scanner::new_scanner(src.clone());
            let (tokens, diagnostics) = s.scan_all();
            assert!(
                diagnostics.is_empty(),
                "scanning {src:?} diagnosed: {diagnostics:?}"
            );
            let scanned: Vec<Kind> = tokens.into_iter().map(|t| t.kind).collect();
            assert_eq!(scanned, [kind, Kind::Eof], "scanning {src:?}");
        }
    }
}
