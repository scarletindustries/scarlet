mod keywords;
mod kind;
mod trivia;
mod util;

pub use keywords::{keyword_text, match_keyword};
pub use kind::Kind;
pub use trivia::Trivia;
pub use util::{is_name_continue, is_name_start, is_type_name};

use crate::span::Span;
use std::fmt;

#[derive(Debug, Clone)]
pub struct Token {
    pub kind: Kind,
    pub literal: Option<String>,
    pub span: Span,
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
