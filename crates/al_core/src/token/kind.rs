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
    BitwiseAnd,
    BitwiseOr,
    BitwiseXor,
    BitwiseNot,
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
            Kind::BitwiseAnd => "&",
            Kind::BitwiseOr => "|",
            Kind::BitwiseXor => "^",
            Kind::BitwiseNot => "~",
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
