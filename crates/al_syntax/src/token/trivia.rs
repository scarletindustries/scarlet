/// Non-token source between tokens. The scanner drops horizontal whitespace,
/// so only these four shapes exist.
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
