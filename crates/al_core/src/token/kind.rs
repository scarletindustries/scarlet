use std::fmt;

use super::keywords::keyword_text;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Eof,
    Error,
    Identifier,
    LiteralNumber,
    LiteralString,
    InterpStringStart,
    InterpStringPart,
    InterpStringEnd,
    LogicalAnd,
    LogicalOr,
    BitwiseAnd,
    BitwiseOr,
    BitwiseXor,
    BitwiseNot,
    KwConst,
    KwIf,
    KwElse,
    KwFunction,
    KwImport,
    KwType,
    KwIn,
    KwMatch,
    KwOr,
    KwPub,
    KwOpaque,
    KwAs,
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

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Kind::Eof => "EOF",
            Kind::Error => "<error>",
            Kind::Identifier => "identifier",
            Kind::LiteralNumber => "number",
            Kind::LiteralString => "string",
            Kind::InterpStringStart => "interpolated string start",
            Kind::InterpStringPart => "interpolated string part",
            Kind::InterpStringEnd => "interpolated string end",
            Kind::LogicalAnd => "&&",
            Kind::LogicalOr => "||",
            Kind::BitwiseAnd => "&",
            Kind::BitwiseOr => "|",
            Kind::BitwiseXor => "^",
            Kind::BitwiseNot => "~",
            // The keyword or-pattern keeps this match exhaustive; the display
            // text itself comes from `KEYWORDS`, so it cannot drift from what
            // the scanner accepts.
            #[allow(clippy::expect_used)]
            k @ (Kind::KwConst
            | Kind::KwIf
            | Kind::KwElse
            | Kind::KwFunction
            | Kind::KwImport
            | Kind::KwType
            | Kind::KwIn
            | Kind::KwMatch
            | Kind::KwOr
            | Kind::KwPub
            | Kind::KwOpaque
            | Kind::KwAs) => keyword_text(*k).expect("every Kw* variant is in KEYWORDS"),
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
