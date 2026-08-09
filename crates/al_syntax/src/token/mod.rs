mod ident;
mod keywords;
mod kind;
mod trivia;

pub use ident::{is_name_continue, is_name_start, is_type_name};
pub use keywords::{Keyword, match_keyword};
pub use kind::Kind;
pub use trivia::Trivia;

use crate::span::Span;
use std::fmt;

#[derive(Debug, Clone)]
pub struct Token {
    pub kind: Kind,
    pub span: Span,
    pub leading_trivia: Vec<Trivia>,
}

/// The token's source text, unquoted. Error sites add their own quotes.
impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.kind)
    }
}
