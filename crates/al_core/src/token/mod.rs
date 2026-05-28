mod keywords;
mod kind;
mod trivia;
mod util;

pub use keywords::match_keyword;
pub use kind::Kind;
pub use trivia::{Trivia, TriviaKind};
pub use util::*;

use std::fmt;

#[derive(Debug, Clone)]
pub struct Token {
    pub kind: Kind,
    pub literal: Option<String>,
    pub line: i32,
    pub column: i32,
    pub length: i32,
    pub leading_trivia: Vec<Trivia>,
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(lit) = &self.literal {
            if self.kind == Kind::LiteralString {
                return write!(f, "'{}'", lit);
            }
            return write!(f, "{}", lit);
        }
        write!(f, "{}", self.kind)
    }
}
