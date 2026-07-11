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
    /// The comment's source text, or `None` for a `Newline`.
    #[inline]
    pub fn comment_text(&self) -> Option<&str> {
        match self {
            Trivia::Newline => None,
            Trivia::LineComment(s) | Trivia::BlockComment(s) | Trivia::DocComment(s) => Some(s),
        }
    }
}
