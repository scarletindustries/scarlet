use super::Kind;

/// The single source of truth for al's keyword set. Adding a keyword means
/// adding one variant here: `text` is an exhaustive match so the compiler
/// forces a display spelling; `parse` is not (it matches on `&str`), so the
/// `keyword_roundtrip` test below enforces that every variant in `ALL` has a
/// `parse` arm that agrees with `text`.
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
    pub const ALL: [Keyword; 12] = [
        Self::Fn,
        Self::Import,
        Self::Type,
        Self::In,
        Self::Match,
        Self::Const,
        Self::If,
        Self::Else,
        Self::Or,
        Self::Pub,
        Self::Opaque,
        Self::As,
    ];

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

#[cfg(test)]
mod tests {
    use super::Keyword;

    #[test]
    fn keyword_roundtrip() {
        for kw in Keyword::ALL {
            assert_eq!(Keyword::parse(kw.text()), Some(kw), "{kw:?}");
        }
    }
}
