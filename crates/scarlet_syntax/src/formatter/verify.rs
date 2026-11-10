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
    for (&i, &o) in a.iter().zip(&b) {
        if reference[i].kind != output[o].kind {
            return Verdict::Different(Mismatch::TokenKind {
                made: output[o].kind.clone(),
                doc: reference[i].kind.clone(),
                at: position(&output[o]),
            });
        }
    }
    if a.len() != b.len() {
        return Verdict::Different(Mismatch::TokenCount {
            made: b.len(),
            doc: a.len(),
        });
    }

    let reference_ends = line_ends(reference);
    let output_ends = line_ends(output);
    let mut keep_flat = BTreeSet::new();
    for (&i, &o) in a.iter().zip(&b) {
        match (reference_ends[i], output_ends[o]) {
            (true, false) => {
                return Verdict::Different(Mismatch::JoinedLines {
                    at: position(&output[o]),
                });
            }
            (false, true) => {
                let tok = &output[o];
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
                let prev = o.checked_sub(1).map_or(0, |p| output[p].span.end_line);
                let from = usize::try_from(prev).unwrap_or(0);
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

/// The indices of the tokens that must match. A trailing comma, right before
/// a closer, is left out: the formatter writes one exactly when the list
/// breaks, and the parser reads the list the same either way.
fn comparable(tokens: &[Token]) -> Vec<usize> {
    (0..tokens.len())
        .filter(|&i| {
            let kind = &tokens[i].kind;
            let trailing_comma = *kind == Kind::PuncComma
                && tokens.get(i + 1).is_some_and(|next| {
                    matches!(
                        next.kind,
                        Kind::PuncCloseParen
                            | Kind::PuncCloseBracket
                            | Kind::PuncCloseBrace
                            | Kind::BinClose
                    )
                });
            *kind != Kind::Eof && !trailing_comma
        })
        .collect()
}
