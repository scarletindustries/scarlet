//! Which line breaks end a statement. This is the language's one newline
//! rule. The parser turns each of these breaks into a `Kind::LineEnd` token
//! and never looks at line breaks itself, and the formatter checks its output
//! against the same rule, so the two can't disagree.
//!
//! A line break ends a statement only when all three hold:
//!
//! - it sits directly in a `{ }` of statements, arms or variants, or at the
//!   top of the file. Inside `( )`, `[ ]`, `<< >>`, an import's `.{ }` and a
//!   string's `${ }`, line breaks are just spaces.
//! - the token before it can end an expression, like a name or a `)`. A line
//!   that ends in `+`, `,` or `=` goes on.
//! - the token after it can start a statement. A line that starts with `|>`,
//!   `.`, `else` or `||` continues the one before.

use super::{Keyword, Kind, Token, Trivia};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Frame {
    /// Inside a `{ }` of statements, arms or variants, or the file itself.
    Lines,
    /// Inside brackets that hold one expression or a comma list.
    Items,
    /// Inside an interpolated string, between its start and end tokens.
    Interp,
}

/// For each token, whether a line end comes right before it.
pub(crate) fn line_ends(tokens: &[Token]) -> Vec<bool> {
    let mut frames = vec![Frame::Lines];
    let mut prev: Option<&Kind> = None;
    let mut out = Vec::with_capacity(tokens.len());
    for tok in tokens {
        let top = *frames.last().unwrap_or(&Frame::Lines);
        out.push(
            top == Frame::Lines
                && prev.is_some_and(can_end)
                && can_begin(&tok.kind)
                && breaks_line_before(tok),
        );
        match &tok.kind {
            Kind::PuncOpenParen | Kind::PuncOpenBracket | Kind::BinOpen => {
                frames.push(Frame::Items)
            }
            Kind::PuncOpenBrace => {
                let list = top == Frame::Interp || prev == Some(&Kind::PuncDot);
                frames.push(if list { Frame::Items } else { Frame::Lines });
            }
            // A stray closer in broken source must not pop the file's frame or
            // the string it sits in.
            Kind::PuncCloseParen
            | Kind::PuncCloseBracket
            | Kind::BinClose
            | Kind::PuncCloseBrace
                if frames.len() > 1 && top != Frame::Interp =>
            {
                frames.pop();
            }
            Kind::InterpStringStart => frames.push(Frame::Interp),
            Kind::InterpStringEnd if top == Frame::Interp => {
                frames.pop();
            }
            _ => {}
        }
        prev = Some(&tok.kind);
    }
    out
}

/// Inserts a `Kind::LineEnd` token before every token that `line_ends` marks.
/// It sits at the end of the token before it, so an error about it points at
/// the end of that line.
pub(crate) fn insert_line_ends(tokens: Vec<Token>) -> Vec<Token> {
    let ends = line_ends(&tokens);
    let mut out = Vec::with_capacity(tokens.len() + tokens.len() / 4);
    for (tok, ends) in tokens.into_iter().zip(ends) {
        if ends && let Some(prev) = out.last() {
            let prev: &Token = prev;
            out.push(Token {
                kind: Kind::LineEnd,
                span: crate::span::Span::point(prev.span.end_line, prev.span.end_column),
                leading_trivia: Vec::new(),
            });
        }
        out.push(tok);
    }
    out
}

fn breaks_line_before(tok: &Token) -> bool {
    tok.leading_trivia
        .iter()
        .any(|t| matches!(t, Trivia::Newline))
}

fn can_end(kind: &Kind) -> bool {
    match kind {
        Kind::Identifier(_)
        | Kind::LiteralNumber(_)
        | Kind::LiteralString(_)
        | Kind::InterpStringEnd
        | Kind::PuncCloseParen
        | Kind::PuncCloseBracket
        | Kind::PuncCloseBrace
        | Kind::BinClose
        | Kind::Error(_) => true,
        Kind::Eof
        | Kind::LineEnd
        | Kind::InterpStringStart
        | Kind::InterpStringPart(_)
        | Kind::LogicalAnd
        | Kind::LogicalOr
        | Kind::BitwiseOr
        | Kind::PuncPipe
        | Kind::Keyword(_)
        | Kind::PuncArrow
        | Kind::PuncBackArrow
        | Kind::PuncComma
        | Kind::PuncColon
        | Kind::PuncColonColon
        | Kind::PuncSemicolon
        | Kind::PuncDot
        | Kind::PuncDotdot
        | Kind::PuncOpenParen
        | Kind::PuncOpenBrace
        | Kind::PuncOpenBracket
        | Kind::BinOpen
        | Kind::PuncQuestionMark
        | Kind::PuncExclamationMark
        | Kind::PuncAt
        | Kind::PuncEquals
        | Kind::PuncEqualsComparator
        | Kind::PuncNotEqual
        | Kind::PuncGt
        | Kind::PuncLt
        | Kind::PuncGte
        | Kind::PuncLte
        | Kind::PuncPlus
        | Kind::PuncPlusplus
        | Kind::PuncMinus
        | Kind::PuncMinusminus
        | Kind::PuncMul
        | Kind::PuncDiv
        | Kind::PuncMod => false,
    }
}

fn can_begin(kind: &Kind) -> bool {
    match kind {
        Kind::Identifier(_)
        | Kind::LiteralNumber(_)
        | Kind::LiteralString(_)
        | Kind::InterpStringStart
        | Kind::PuncOpenParen
        | Kind::PuncOpenBrace
        | Kind::PuncOpenBracket
        | Kind::BinOpen
        | Kind::PuncMinus
        | Kind::PuncMinusminus
        | Kind::PuncExclamationMark
        | Kind::PuncAt
        | Kind::Error(_) => true,
        Kind::Keyword(kw) => match kw {
            Keyword::Fn
            | Keyword::Import
            | Keyword::Type
            | Keyword::Match
            | Keyword::Const
            | Keyword::If
            | Keyword::Pub
            | Keyword::Opaque => true,
            Keyword::In | Keyword::Else | Keyword::Or | Keyword::As => false,
        },
        Kind::Eof
        | Kind::LineEnd
        | Kind::InterpStringPart(_)
        | Kind::InterpStringEnd
        | Kind::LogicalAnd
        | Kind::LogicalOr
        | Kind::BitwiseOr
        | Kind::PuncPipe
        | Kind::PuncArrow
        | Kind::PuncBackArrow
        | Kind::PuncComma
        | Kind::PuncColon
        | Kind::PuncColonColon
        | Kind::PuncSemicolon
        | Kind::PuncDot
        | Kind::PuncDotdot
        | Kind::PuncCloseParen
        | Kind::PuncCloseBrace
        | Kind::PuncCloseBracket
        | Kind::BinClose
        | Kind::PuncQuestionMark
        | Kind::PuncEquals
        | Kind::PuncEqualsComparator
        | Kind::PuncNotEqual
        | Kind::PuncGt
        | Kind::PuncLt
        | Kind::PuncGte
        | Kind::PuncLte
        | Kind::PuncPlus
        | Kind::PuncPlusplus
        | Kind::PuncMul
        | Kind::PuncDiv
        | Kind::PuncMod => false,
    }
}

#[cfg(test)]
mod tests {
    use super::line_ends;
    use crate::scanner;

    // The source with a `;` where each line end goes.
    fn marked(src: &str) -> String {
        let (tokens, _) = scanner::new_scanner(src.to_string()).scan_all();
        let ends = line_ends(&tokens);
        let mut out = String::new();
        for (tok, end) in tokens.iter().zip(ends) {
            if tok.kind == crate::token::Kind::Eof {
                break;
            }
            if end {
                out.push(';');
            }
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(&tok.kind.to_string());
        }
        out
    }

    #[test]
    fn a_break_between_statements_ends_the_line() {
        assert_eq!(marked("{\na\nb\n}"), "{ a; b }");
    }

    #[test]
    fn breaks_in_brackets_are_spaces() {
        assert_eq!(
            marked("{\nf(\na,\nb\n)\n[\nx\n]\n}"),
            "{ f ( a , b ); [ x ] }"
        );
    }

    #[test]
    fn an_operator_at_either_end_continues_the_line() {
        assert_eq!(
            marked("{\na +\nb\nc\n|> d\ne\n.f\n}"),
            "{ a + b; c |> d; e . f }"
        );
    }

    #[test]
    fn a_block_inside_brackets_has_lines_again() {
        assert_eq!(marked("f(fn() {\na\nb\n})"), "f ( fn ( ) { a; b } )");
    }

    #[test]
    fn minus_and_paren_start_a_new_line() {
        assert_eq!(marked("{\na\n-b\nc\n(d)\n}"), "{ a; - b; c; ( d ) }");
    }

    #[test]
    fn else_continues_an_if() {
        assert_eq!(
            marked("{\nif a { b }\nelse { c }\n}"),
            "{ if a { b } else { c } }"
        );
    }
}
