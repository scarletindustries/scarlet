//! ANSI syntax highlighting for Scarlet source.
//!
//! Colours come from the scanner's own tokens rather than a second pattern
//! language, so the REPL can't drift from what the compiler reads.
//! The output is the input with escape sequences inserted: every source byte
//! survives, in order, so the highlighted text keeps the original display
//! width (which is what a line editor repainting a line requires).

use crate::scanner::new_scanner;
use crate::span::Span;
use crate::term::Palette;
use crate::token::{Kind, Token, is_type_name};

/// `source` coloured for a terminal, or `source` unchanged when the palette is
/// off. Partial input is fine: the scanner's error recovery decides how a
/// half-typed line is coloured. It also bolds the bracket pair that the cursor
/// at byte offset `cursor` sits on (on either side of it), the way an editor
/// shows which `}` closes the `{` under the caret.
pub fn highlight_at(source: &str, p: &Palette, cursor: Option<usize>) -> String {
    if !p.enabled() || source.is_empty() {
        return source.to_string();
    }

    let (tokens, _) = new_scanner(source).scan_all();
    let offsets = Offsets::new(source);
    let ranges: Vec<(usize, usize)> = tokens
        .iter()
        .map(|t| offsets.range(source, t.span))
        .collect();
    let pair = cursor.and_then(|c| matching_pair(&tokens, &ranges, c));

    let mut out = String::with_capacity(source.len() * 2);
    let mut cursor_byte = 0usize;
    let mut interp = InterpState::default();

    for (i, token) in tokens.iter().enumerate() {
        if token.kind == Kind::Eof {
            break;
        }
        let (start, end) = ranges[i];
        // Whatever the scanner skipped between tokens: horizontal whitespace,
        // newlines, comments, and the quote characters of an interpolated
        // string, which are consumed without becoming tokens of their own.
        paint_gap(&mut out, &source[cursor_byte..start], interp.in_body(), p);

        let color = if pair.is_some_and(|(a, b)| a == i || b == i) {
            p.bold
        } else {
            token_color(token, tokens.get(i + 1), interp.in_body(), p)
        };
        push_colored(&mut out, &source[start..end], color, p);

        interp.advance(&token.kind);
        cursor_byte = end;
    }
    paint_gap(&mut out, &source[cursor_byte..], interp.in_body(), p);
    out
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
                    *d += 1;
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
fn paint_gap(out: &mut String, gap: &str, in_string_body: bool, p: &Palette) {
    if gap.is_empty() {
        return;
    }
    if in_string_body {
        push_colored(out, gap, p.green, p);
        return;
    }
    let bytes = gap.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let rest = &gap[i..];
        let comment_len = if rest.starts_with("//") {
            Some(rest.find('\n').unwrap_or(rest.len()))
        } else if rest.starts_with("/*") {
            Some(rest.find("*/").map_or(rest.len(), |e| e + 2))
        } else {
            None
        };
        match comment_len {
            Some(len) => {
                push_colored(out, &rest[..len], p.dim, p);
                i += len;
            }
            None => {
                // Up to the next `/`, which is the only byte that can open a
                // comment. `/` is ASCII, so the split is on a char boundary.
                let next = rest
                    .bytes()
                    .skip(1)
                    .position(|b| b == b'/')
                    .map_or(rest.len(), |n| n + 1);
                out.push_str(&rest[..next]);
                i += next;
            }
        }
    }
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
    let mut stack: Vec<usize> = Vec::new();
    let mut pairs: Vec<(usize, usize)> = Vec::new();
    for (i, t) in tokens.iter().enumerate() {
        match bracket(&t.kind) {
            Some(Bracket::Open) => stack.push(i),
            Some(Bracket::Close) => {
                if let Some(open) = stack.pop() {
                    pairs.push((open, i));
                }
            }
            None => {}
        }
    }
    pairs.into_iter().find(|&(open, close)| {
        [open, close]
            .iter()
            .any(|&i| ranges[i].0 == cursor || ranges[i].1 == cursor)
    })
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
        line_starts.extend(
            source
                .bytes()
                .enumerate()
                .filter(|&(_, b)| b == b'\n')
                .map(|(i, _)| i + 1),
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
        let mut at = base.saturating_add(col).min(source.len());
        while at > 0 && !source.is_char_boundary(at) {
            at -= 1;
        }
        at
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: Palette = crate::term::Palette::ansi_for_test();

    /// Whatever the colouring, the visible text must survive byte for byte: a
    /// line editor lays the line out by the width of what it printed.
    fn strip(s: &str) -> String {
        let mut out = String::new();
        let mut rest = s;
        while let Some(i) = rest.find('\x1b') {
            out.push_str(&rest[..i]);
            let after = rest[i..].find('m').map_or(rest.len(), |m| i + m + 1);
            rest = &rest[after..];
        }
        out.push_str(rest);
        out
    }

    #[track_caller]
    fn assert_transparent(src: &str) {
        assert_eq!(
            strip(&highlight_at(src, &P, None)),
            src,
            "source text changed"
        );
    }

    #[test]
    fn coloring_preserves_the_source_text() {
        for src in [
            "fn add(a Int, b Int) Int { a + b }",
            "// just a comment",
            "const x = 1 // trailing\nconst y = 2",
            "/* block */ const z = 'hi'",
            "println('hello ${name}, you are ${age + 1}')",
            "const s = 'unterminated",
            "match x { Ok(v) -> v, Err(_) -> 0 }",
            "const emoji = 'héllo 🌶'",
            "",
            "   ",
            "§",
        ] {
            assert_transparent(src);
        }
    }

    #[test]
    fn keywords_strings_and_numbers_get_distinct_colors() {
        let out = highlight_at("const x = 'hi'", &P, None);
        assert!(
            out.contains("\x1b[35mconst"),
            "keyword not magenta: {out:?}"
        );
        assert!(out.contains("\x1b[32m'hi'"), "string not green: {out:?}");
        let out = highlight_at("const x = 42", &P, None);
        assert!(out.contains("\x1b[33m42"), "number not yellow: {out:?}");
    }

    #[test]
    fn a_comment_between_tokens_is_dimmed() {
        let out = highlight_at("const x = 1 // note\n", &P, None);
        assert!(out.contains("\x1b[2m// note"), "comment not dim: {out:?}");
    }

    #[test]
    fn a_type_name_and_a_callee_differ_from_a_binding() {
        let out = highlight_at("println(Int)", &P, None);
        assert!(out.contains("\x1b[34mprintln"), "callee not blue: {out:?}");
        assert!(out.contains("\x1b[36mInt"), "type not cyan: {out:?}");
        assert!(
            !highlight_at("x", &P, None).contains('\x1b'),
            "plain name coloured"
        );
    }

    #[test]
    fn the_bracket_pair_under_the_cursor_is_bolded() {
        let src = "f(a, b)";
        let out = highlight_at(src, &P, Some(1));
        assert_eq!(out.matches("\x1b[1m").count(), 2, "want both ends: {out:?}");
        // Away from any bracket, nothing is bolded.
        let out = highlight_at(src, &P, Some(4));
        assert_eq!(out.matches("\x1b[1m").count(), 0, "{out:?}");
    }

    #[test]
    fn an_unmatched_bracket_has_no_partner_to_bold() {
        let out = highlight_at("f(a", &P, Some(1));
        assert!(!out.contains("\x1b[1m"), "{out:?}");
    }

    #[test]
    fn an_interpolation_hole_is_not_string_colored() {
        let out = highlight_at("'a ${b} c'", &P, None);
        // The name inside the hole keeps its own (absent) colour, so the green
        // run must be broken by the reset that ends the `${`.
        assert!(strip(&out) == "'a ${b} c'");
        assert!(out.contains("\x1b[32m"), "string parts not green: {out:?}");
        assert!(
            !out.contains("\x1b[32mb"),
            "hole coloured as string: {out:?}"
        );
    }

    #[test]
    fn a_qualified_member_being_typed_is_colored() {
        let out = highlight_at("http.Del", &P, None);
        assert!(
            out.contains("\x1b[36mDel"),
            "type-cased member not cyan: {out:?}"
        );
    }

    #[test]
    fn a_disabled_palette_adds_nothing() {
        let plain = crate::term::Palette::plain_for_test();
        assert_eq!(
            highlight_at("const x = 'hi'", &plain, None),
            "const x = 'hi'"
        );
    }
}
