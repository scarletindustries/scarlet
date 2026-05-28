use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriviaKind {
    Whitespace,
    Newline,
    LineComment,
    BlockComment,
    DocComment,
}

#[derive(Debug, Clone)]
pub struct Trivia {
    pub kind: TriviaKind,
    pub text: String,
}

impl fmt::Display for Trivia {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}
