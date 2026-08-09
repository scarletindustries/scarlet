use super::Kind;

/// The single source of truth for al's keyword set: `keywords!` derives the
/// `Keyword` enum, `ALL`, `text`, and `parse` from one list, so adding a
/// keyword is a one-line edit and the spellings cannot drift apart.
macro_rules! keywords {
    (@unit $variant:ident) => { () };
    ($($variant:ident => $text:literal),+ $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum Keyword {
            $($variant),+
        }

        impl Keyword {
            pub const ALL: [Keyword; [$(keywords!(@unit $variant)),+].len()] =
                [$(Self::$variant),+];

            pub const fn text(self) -> &'static str {
                match self {
                    $(Self::$variant => $text),+
                }
            }

            pub fn parse(s: &str) -> Option<Self> {
                Some(match s {
                    $($text => Self::$variant,)+
                    _ => return None,
                })
            }
        }
    };
}

keywords! {
    Fn => "fn",
    Import => "import",
    Type => "type",
    In => "in",
    Match => "match",
    Const => "const",
    If => "if",
    Else => "else",
    Or => "or",
    Pub => "pub",
    Opaque => "opaque",
    As => "as",
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
