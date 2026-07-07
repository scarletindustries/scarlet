use std::fmt;

/// Non-token source between tokens. The scanner drops horizontal whitespace
/// entirely (no consumer reads it), so only these four shapes exist. A
/// `Newline` carries no text — it is counted, never printed — so making it a
/// bare unit variant means the common case allocates nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trivia {
    Newline,
    LineComment(String),
    BlockComment(String),
    DocComment(String),
}

impl Trivia {
    #[inline]
    pub fn is_comment(&self) -> bool {
        !matches!(self, Trivia::Newline)
    }

    #[inline]
    pub fn text(&self) -> &str {
        match self {
            Trivia::Newline => "",
            Trivia::LineComment(s) | Trivia::BlockComment(s) | Trivia::DocComment(s) => s,
        }
    }
}

impl fmt::Display for Trivia {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.text())
    }
}
