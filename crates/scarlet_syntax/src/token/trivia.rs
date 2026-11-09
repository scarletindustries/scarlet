/// Source text between tokens that isn't a token itself. The scanner drops
/// spaces and tabs, so these four kinds are all there is.
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
    pub(crate) fn comment_text(&self) -> Option<&str> {
        match self {
            Trivia::Newline => None,
            Trivia::LineComment(s) | Trivia::BlockComment(s) | Trivia::DocComment(s) => Some(s),
        }
    }
}
