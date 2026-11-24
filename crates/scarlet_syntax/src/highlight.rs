//! ANSI syntax highlighting for Scarlet source.
//!
//! Colours come from the scanner's own tokens rather than a second pattern
//! language, so the REPL can't drift from what the compiler reads.
//! The output is the input with escape sequences inserted: every source byte
//! survives, in order, so the highlighted text keeps the original display
//! width (which is what a line editor repainting a line requires).

use std::fmt;

use crate::scanner::new_scanner;
use crate::span::Span;
use crate::term::Palette;
use crate::token::{Kind, Token, is_type_name};

/// `source` coloured for a terminal, or `source` unchanged when the palette is
/// off. Partial input is fine: the scanner's error recovery decides how a
/// half-typed line is coloured. It also bolds the bracket pair that the cursor
/// at byte offset `cursor` sits on (on either side of it), the way an editor
/// shows which `}` closes the `{` under the caret.
///
/// It fails when the scanner's tokens don't map onto `source` in order, and
/// then there is no faithful colouring to give.
pub fn highlight_at(
    source: &str,
    p: &Palette,
    cursor: Option<usize>,
) -> Result<String, HighlightError> {
    if !p.enabled() || source.is_empty() {
        return Ok(source.to_string());
    }
    let (tokens, _) = new_scanner(source).scan_all();
    paint(source, &tokens, p, cursor)
}

/// Why the tokens of a source could not be laid back over it. Offsets are
/// bytes into the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HighlightError {
    /// A token starts at `start`, before the token ahead of it ends at
    /// `previous_end`.
    TokenOverlap { previous_end: usize, start: usize },
    /// `start..end` does not cut the source on character boundaries.
    NotCharRange { start: usize, end: usize },
}

impl fmt::Display for HighlightError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HighlightError::TokenOverlap {
                previous_end,
                start,
            } => write!(
                f,
                "a token starts at byte {start}, before the previous one ends at byte {previous_end}"
            ),
            HighlightError::NotCharRange { start, end } => {
                write!(
                    f,
                    "bytes {start}..{end} do not fall on character boundaries"
                )
            }
        }
    }
}

fn source_range(source: &str, range: std::ops::Range<usize>) -> Result<&str, HighlightError> {
    let (start, end) = (range.start, range.end);
    source
        .get(range)
        .ok_or(HighlightError::NotCharRange { start, end })
}

fn paint(
    source: &str,
    tokens: &[Token],
    p: &Palette,
    cursor: Option<usize>,
) -> Result<String, HighlightError> {
    let offsets = Offsets::new(source);
    let ranges: Vec<(usize, usize)> = tokens
        .iter()
        .map(|t| offsets.range(source, t.span))
        .collect();
    let pair = cursor.and_then(|c| matching_pair(tokens, &ranges, c));

    let mut out = String::with_capacity(source.len().saturating_mul(2));
    let mut cursor_byte = 0usize;
    let mut interp = InterpState::default();

    for (i, (token, &(start, end))) in tokens.iter().zip(&ranges).enumerate() {
        if token.kind == Kind::Eof {
            break;
        }
        // Whatever the scanner skipped between tokens: horizontal whitespace,
        // newlines, comments, and the quote characters of an interpolated
        // string, which are consumed without becoming tokens of their own.
        if start < cursor_byte {
            return Err(HighlightError::TokenOverlap {
                previous_end: cursor_byte,
                start,
            });
        }
        let gap = source_range(source, cursor_byte..start)?;
        paint_gap(&mut out, gap, cursor_byte, interp.in_body(), p)?;

        let color = if pair.is_some_and(|(a, b)| a == i || b == i) {
            p.bold
        } else {
            token_color(token, tokens.get(i.saturating_add(1)), interp.in_body(), p)
        };
        push_colored(&mut out, source_range(source, start..end)?, color, p);

        interp.advance(&token.kind);
        cursor_byte = end;
    }
    let tail = source_range(source, cursor_byte..source.len())?;
    paint_gap(&mut out, tail, cursor_byte, interp.in_body(), p)?;
    Ok(out)
}

/// A copy of the scanner's interpolation state. Inside `'a ${b} c'` the string
/// body and the embedded expression are coloured differently, and only the
/// scanner's own rule (`${` opens, the matching `}` closes) says which is
/// which.
#[derive(Default)]
struct InterpState {
    /// One entry per open interpolated string, holding its brace depth: `0`
    /// means the string body, `> 0` means inside `${ ... }`.
    depths: Vec<u32>,
}

impl InterpState {
    fn in_body(&self) -> bool {
        self.depths.last().is_some_and(|d| *d == 0)
    }

    fn advance(&mut self, kind: &Kind) {
        match kind {
            Kind::InterpStringStart => self.depths.push(0),
            Kind::InterpStringEnd => {
                self.depths.pop();
            }
            Kind::PuncOpenBrace => {
                if let Some(d) = self.depths.last_mut() {
                    *d = d.saturating_add(1);
                }
            }
            Kind::PuncCloseBrace => {
                if let Some(d) = self.depths.last_mut() {
                    *d = d.saturating_sub(1);
                }
            }
            _ => {}
        }
    }
}

fn token_color(
    token: &Token,
    next: Option<&Token>,
    in_string_body: bool,
    p: &Palette,
) -> &'static str {
    match &token.kind {
        Kind::Error(_) => p.red,
        Kind::Keyword(_) => p.magenta,
        Kind::LiteralNumber(_) => p.yellow,
        Kind::LiteralString(_) | Kind::InterpStringPart(_) => p.green,
        // A type name is capitalised, a name before `(` is being called, and
        // everything else is a plain binding. Those stay uncoloured so the eye
        // lands on the structure instead of on every word.
        Kind::Identifier(name) if is_type_name(name) => p.cyan,
        Kind::Identifier(_) if next.is_some_and(|t| t.kind == Kind::PuncOpenParen) => p.blue,
        // The `${` and `}` bracketing an interpolation belong to the string.
        _ if in_string_body => p.green,
        _ => "",
    }
}

/// Source between two tokens: whitespace, comments, or (inside an
/// interpolated string) the literal text the scanner folded into the token
/// payload, which keeps the string colour.
fn paint_gap(
    out: &mut String,
    gap: &str,
    gap_start: usize,
    in_string_body: bool,
    p: &Palette,
) -> Result<(), HighlightError> {
    if gap.is_empty() {
        return Ok(());
    }
    if in_string_body {
        push_colored(out, gap, p.green, p);
        return Ok(());
    }
    let mut rest = gap;
    let mut at = gap_start;
    while !rest.is_empty() {
        let (len, color) = if rest.starts_with("//") {
            (rest.find('\n').unwrap_or(rest.len()), p.dim)
        } else if rest.starts_with("/*") {
            (
                rest.find("*/").map_or(rest.len(), |e| e.saturating_add(2)),
                p.dim,
            )
        } else {
            // Up to the next `/`, which is the only byte that can open a
            // comment.
            let next = rest
                .bytes()
                .skip(1)
                .position(|b| b == b'/')
                .map_or(rest.len(), |n| n.saturating_add(1));
            (next, "")
        };
        let (piece, tail) = rest
            .split_at_checked(len)
            .ok_or(HighlightError::NotCharRange {
                start: at,
                end: at.saturating_add(len),
            })?;
        push_colored(out, piece, color, p);
        rest = tail;
        at = at.saturating_add(len);
    }
    Ok(())
}

fn push_colored(out: &mut String, text: &str, color: &str, p: &Palette) {
    if color.is_empty() {
        out.push_str(text);
    } else {
        out.push_str(color);
        out.push_str(text);
        out.push_str(p.reset);
    }
}

/// The token indices of the bracket pair the cursor rests on, if any. Both
/// sides of the cursor count, so `{|` and `|}` each light up their partner.
fn matching_pair(
    tokens: &[Token],
    ranges: &[(usize, usize)],
    cursor: usize,
) -> Option<(usize, usize)> {
    // Each bracket is its token index and its byte range.
    let mut stack: Vec<(usize, (usize, usize))> = Vec::new();
    let mut pairs = Vec::new();
    for (i, (t, &range)) in tokens.iter().zip(ranges).enumerate() {
        match bracket(&t.kind) {
            Some(Bracket::Open) => stack.push((i, range)),
            Some(Bracket::Close) => {
                if let Some(open) = stack.pop() {
                    pairs.push((open, (i, range)));
                }
            }
            None => {}
        }
    }
    pairs
        .into_iter()
        .find(|(open, close)| {
            [open, close]
                .iter()
                .any(|(_, (start, end))| *start == cursor || *end == cursor)
        })
        .map(|((open, _), (close, _))| (open, close))
}

enum Bracket {
    Open,
    Close,
}

fn bracket(kind: &Kind) -> Option<Bracket> {
    match kind {
        Kind::PuncOpenParen | Kind::PuncOpenBrace | Kind::PuncOpenBracket | Kind::BinOpen => {
            Some(Bracket::Open)
        }
        Kind::PuncCloseParen | Kind::PuncCloseBrace | Kind::PuncCloseBracket | Kind::BinClose => {
            Some(Bracket::Close)
        }
        _ => None,
    }
}

/// Byte offset of the start of each line, so a token's `(line, column)` span
/// can address the source text. The scanner counts columns in bytes, so the
/// two coordinate systems agree within a line.
struct Offsets {
    line_starts: Vec<usize>,
}

impl Offsets {
    fn new(source: &str) -> Self {
        let mut line_starts = vec![0];
        // Pair each byte with the offset just after it.
        line_starts.extend(
            (1..=source.len())
                .zip(source.bytes())
                .filter(|&(_, b)| b == b'\n')
                .map(|(after, _)| after),
        );
        Offsets { line_starts }
    }

    /// `span` as a byte range, clamped into `source`. A span from a
    /// half-scanned token can point past the end of a line or of the text; the
    /// clamp keeps slicing total rather than making every caller check.
    fn range(&self, source: &str, span: Span) -> (usize, usize) {
        let start = self.offset(source, span.start_line, span.start_column);
        let end = self
            .offset(source, span.end_line, span.end_column)
            .max(start);
        (start, end)
    }

    fn offset(&self, source: &str, line: i32, column: i32) -> usize {
        let base = usize::try_from(line)
            .ok()
            .and_then(|l| self.line_starts.get(l).copied())
            .unwrap_or(source.len());
        let col = usize::try_from(column).unwrap_or(0);
        source.floor_char_boundary(base.saturating_add(col))
    }
}

#[cfg(test)]
mod tests;
