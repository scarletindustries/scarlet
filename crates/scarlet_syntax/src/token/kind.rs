use std::fmt;
use std::rc::Rc;

use super::keywords::Keyword;

/// The kind of a token. A token with text carries it as a payload, so an
/// identifier without a name can't be built. Equality and hashing compare the
/// text itself.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Kind {
    Eof,
    /// A line break that ends a statement. The scanner never makes one: the
    /// parser inserts them, by the rule in `line_end`.
    LineEnd,
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
    PuncPipe,
    Keyword(Keyword),
    PuncArrow,
    PuncBackArrow,
    PuncComma,
    PuncColon,
    PuncColonColon,
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

impl Kind {
    /// Every `Kind` whose `Display` output is its fixed source spelling. The
    /// exhaustive match below forces a new variant to be classified here.
    /// The editor grammars are generated from this list (`cargo xtask
    /// gen-editor-syntax`).
    pub fn fixed_spelling_kinds() -> Vec<Kind> {
        let mut kinds = vec![
            Kind::LogicalAnd,
            Kind::LogicalOr,
            Kind::BitwiseOr,
            Kind::PuncPipe,
            Kind::PuncArrow,
            Kind::PuncBackArrow,
            Kind::PuncComma,
            // Before `:`, so a first-match consumer reads `::` whole.
            Kind::PuncColonColon,
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
        kinds.extend(Keyword::ALL.into_iter().map(Kind::Keyword));
        kinds
    }
}

/// How a token reads in a diagnostic, not how to rebuild the source. It has no
/// quotes, so error messages that want them add their own.
impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Kind::Eof => "EOF",
            Kind::LineEnd => "end of line",
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
            Kind::PuncPipe => "|>",
            Kind::Keyword(kw) => kw.text(),
            Kind::PuncArrow => "->",
            Kind::PuncBackArrow => "<-",
            Kind::PuncComma => ",",
            Kind::PuncColon => ":",
            Kind::PuncColonColon => "::",
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
mod tests;
