use super::Kind;

/// The single source of truth for al's keyword set. Adding a keyword means
/// adding one variant here: `text` and `parse` are exhaustive matches, so the
/// compiler enforces that the scanner spelling and the display spelling stay in
/// lockstep with the variant list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Keyword {
    Fn,
    Import,
    Type,
    In,
    Match,
    Const,
    If,
    Else,
    Or,
    Pub,
    Opaque,
    As,
}

impl Keyword {
    pub const fn text(self) -> &'static str {
        match self {
            Self::Fn => "fn",
            Self::Import => "import",
            Self::Type => "type",
            Self::In => "in",
            Self::Match => "match",
            Self::Const => "const",
            Self::If => "if",
            Self::Else => "else",
            Self::Or => "or",
            Self::Pub => "pub",
            Self::Opaque => "opaque",
            Self::As => "as",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        use Keyword::*;
        Some(match s {
            "fn" => Fn,
            "import" => Import,
            "type" => Type,
            "in" => In,
            "match" => Match,
            "const" => Const,
            "if" => If,
            "else" => Else,
            "or" => Or,
            "pub" => Pub,
            "opaque" => Opaque,
            "as" => As,
            _ => return None,
        })
    }
}

#[inline]
pub fn match_keyword(s: &str) -> Option<Kind> {
    Keyword::parse(s).map(Kind::Keyword)
}
