//! Checks that layout didn't change what the formatter's doc means.
//!
//! The parser reads only tokens and line ends (`token::line_ends`). So two
//! texts with the same tokens and the same line ends are the same program,
//! whatever their spacing. The reference is the doc laid out with every soft
//! break flat: just its tokens and its hard newlines. A soft break can only
//! add a newline or a trailing comma (`doc::DocInner::Break`), so the real
//! layout has the same tokens, and the one thing it can get wrong is a line
//! end. A line end it adds is repaired: the soft breaks that made it are kept
//! flat and layout runs again.

use std::collections::BTreeSet;
use std::fmt;

use super::doc::{BreakId, Layout};
use crate::token::{Kind, Token, Trivia, line_ends};

pub(super) enum Verdict {
    Same,
    /// The output has a line end the reference doesn't. Keeping these breaks
    /// flat removes it.
    KeepFlat(BTreeSet<BreakId>),
    /// The output is a different program, and layout can't fix it.
    Different(Mismatch),
}

/// How the laid-out tokens differ from the doc's. Positions are the output's,
/// counted from 1.
#[derive(Debug)]
pub enum Mismatch {
    TokenKind {
        made: Kind,
        doc: Kind,
        at: (i32, i32),
    },
    TokenCount {
        made: usize,
        doc: usize,
    },
    JoinedLines {
        at: (i32, i32),
    },
    LineCommentEndsLine {
        at: (i32, i32),
    },
}

impl fmt::Display for Mismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Mismatch::TokenKind {
                made,
                doc,
                at: (line, col),
            } => write!(
                f,
                "layout made '{made}' where the doc has '{doc}', at {line}:{col}"
            ),
            Mismatch::TokenCount { made, doc } => {
                write!(f, "layout made {made} tokens, the doc has {doc}")
            }
            Mismatch::JoinedLines { at: (line, col) } => {
                write!(f, "layout joined two lines at {line}:{col}")
            }
            Mismatch::LineCommentEndsLine { at: (line, col) } => {
                write!(f, "a line comment ends a line at {line}:{col}")
            }
        }
    }
}

/// Where a token starts, counted from 1.
fn position(tok: &Token) -> (i32, i32) {
    (tok.span.start_line + 1, tok.span.start_column + 1)
}

pub(super) fn compare(reference: &[Token], output: &[Token], layout: &Layout) -> Verdict {
    let a = comparable(reference);
    let b = comparable(output);
    for (r, o) in a.iter().zip(&b) {
        if r.tok.kind != o.tok.kind {
            return Verdict::Different(Mismatch::TokenKind {
                made: o.tok.kind.clone(),
                doc: r.tok.kind.clone(),
                at: position(o.tok),
            });
        }
    }
    if a.len() != b.len() {
        return Verdict::Different(Mismatch::TokenCount {
            made: b.len(),
            doc: a.len(),
        });
    }

    let mut keep_flat = BTreeSet::new();
    for (r, o) in a.iter().zip(&b) {
        match (r.ends_line, o.ends_line) {
            (true, false) => {
                return Verdict::Different(Mismatch::JoinedLines {
                    at: position(o.tok),
                });
            }
            (false, true) => {
                let tok = o.tok;
                if tok
                    .leading_trivia
                    .iter()
                    .any(|t| matches!(t, Trivia::LineComment(_)))
                {
                    return Verdict::Different(Mismatch::LineCommentEndsLine { at: position(tok) });
                }
                // Newline `k` ends line `k`, so the ones between the two
                // tokens are those from the previous token's line up to this
                // one's.
                let from = usize::try_from(o.prev_end_line).unwrap_or(0);
                let to = usize::try_from(tok.span.start_line).unwrap_or(0);
                keep_flat.extend(
                    layout
                        .newlines
                        .get(from..to)
                        .into_iter()
                        .flatten()
                        .flatten(),
                );
            }
            (true, true) | (false, false) => {}
        }
    }
    if keep_flat.is_empty() {
        Verdict::Same
    } else {
        Verdict::KeepFlat(keep_flat)
    }
}

/// A token that must match, with what the comparison needs from its
/// neighbours in the full stream.
struct Comparable<'a> {
    tok: &'a Token,
    ends_line: bool,
    /// The line the token right before this one ends on, or 0 for the first.
    prev_end_line: i32,
}

/// The tokens that must match. A trailing comma, right before a closer, is
/// left out: the formatter writes one exactly when the list breaks, and the
/// parser reads the list the same either way.
fn comparable(tokens: &[Token]) -> Vec<Comparable<'_>> {
    let mut out = Vec::new();
    let mut prev_end_line = 0;
    let mut rest = tokens.iter().zip(line_ends(tokens)).peekable();
    while let Some((tok, ends_line)) = rest.next() {
        let trailing_comma = tok.kind == Kind::PuncComma
            && rest.peek().is_some_and(|(next, _)| {
                matches!(
                    next.kind,
                    Kind::PuncCloseParen
                        | Kind::PuncCloseBracket
                        | Kind::PuncCloseBrace
                        | Kind::BinClose
                )
            });
        if tok.kind != Kind::Eof && !trailing_comma {
            out.push(Comparable {
                tok,
                ends_line,
                prev_end_line,
            });
        }
        prev_end_line = tok.span.end_line;
    }
    out
}
