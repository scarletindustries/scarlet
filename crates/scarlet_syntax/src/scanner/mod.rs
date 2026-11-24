use std::ops::Range;

mod error;

pub use error::{Radix, ScanError};

use crate::diagnostic::Diagnostic;
use crate::span::Span;
use crate::token::{self, Kind, Token, Trivia};

/// Every escape the language accepts, as (source char after `\`, denoted
/// byte). The editor grammars are generated from this table (`cargo xtask
/// gen-editor-syntax`), so adding an escape here updates them too.
pub const ESCAPES: &[(u8, u8)] = &[
    (b'n', b'\n'),
    (b't', b'\t'),
    (b'r', b'\r'),
    (b'0', b'\0'),
    (b'"', b'"'),
    (b'\'', b'\''),
    (b'\\', b'\\'),
    (b'$', b'$'),
];

/// One level of string-interpolation nesting. `brace_depth == 0` means the
/// scanner is in the string body; `> 0` means it is inside `${ ... }` and
/// counts unmatched `{` so a nested brace does not close the interpolation.
#[derive(Clone, Copy)]
struct InterpFrame {
    quote: u8,
    brace_depth: u32,
    // Where the opening quote is, so a string left open at EOF is reported
    // there.
    start_line: i32,
    start_column: i32,
}

/// The largest input the scanner reads. Positions are `i32`, and the scanner
/// looks a few bytes past `pos`, so the input stays far below `i32::MAX`.
const MAX_INPUT: i32 = 1 << 30;

pub struct Scanner {
    input: Vec<u8>,
    /// `input.len()`, which `new_scanner` checked is at most [`MAX_INPUT`].
    len: i32,
    pos: i32,
    column: i32,
    line: i32,
    diagnostics: Vec<Diagnostic<ScanError>>,
    pending_trivia: Vec<Trivia>,
    token_start_column: i32,
    token_start_line: i32,
    interp_stack: Vec<InterpFrame>,
}

#[inline]
pub fn new_scanner(input: impl Into<String>) -> Scanner {
    let input = input.into().into_bytes();
    let (input, len, diagnostics) = match i32::try_from(input.len()) {
        Ok(len) if len <= MAX_INPUT => (input, len, Vec::new()),
        // Too long to scan, so the scanner sees an empty input and says why.
        Ok(_) | Err(_) => (
            Vec::new(),
            0,
            vec![Diagnostic::new(
                Span::point(0, 0),
                ScanError::TooLarge { bytes: input.len() },
            )],
        ),
    };
    Scanner {
        input,
        len,
        pos: 0,
        column: 0,
        line: 0,
        diagnostics,
        pending_trivia: Vec::new(),
        token_start_column: 0,
        token_start_line: 0,
        interp_stack: Vec::new(),
    }
}

impl Scanner {
    fn add_error(&mut self, error: ScanError) {
        self.error_at(Span::point(self.line, self.column), error);
    }

    fn error_at(&mut self, at: Span, error: ScanError) {
        self.diagnostics.push(Diagnostic::new(at, error));
    }

    fn in_interp_string(&self) -> bool {
        !self.interp_stack.is_empty()
    }

    fn enter_interp_string(&mut self, quote: u8, start_line: i32, start_column: i32) {
        self.interp_stack.push(InterpFrame {
            quote,
            brace_depth: 0,
            start_line,
            start_column,
        });
    }

    fn exit_interp_string(&mut self) {
        self.interp_stack.pop();
    }

    fn input_len(&self) -> i32 {
        self.len
    }

    // The only way to read a byte, and it never fails. A cursor past EOF or
    // below zero reads as `0`, which every caller already treats as the end
    // of input.
    fn byte_at(&self, pos: i32) -> u8 {
        usize::try_from(pos)
            .ok()
            .and_then(|p| self.input.get(p))
            .copied()
            .unwrap_or(0)
    }

    // The input from `start` to `end`, clamped to the input the way
    // `byte_at` is, so the part of the range outside it reads as nothing.
    fn bytes(&self, start: i32, end: i32) -> &[u8] {
        let len = self.input.len();
        let clamp = |i: i32| usize::try_from(i).map_or(0, |i| i.min(len));
        let s = clamp(start);
        let e = clamp(end).max(s);
        self.input.get(s..e).unwrap_or_default()
    }

    fn slice(&self, start: i32, end: i32) -> String {
        String::from_utf8_lossy(self.bytes(start, end)).into_owned()
    }

    fn collect_trivia(&mut self) {
        while self.pos < self.input_len() {
            let ch = self.peek_char();

            if ch == b' ' || ch == b'\t' {
                // No consumer reads whitespace trivia, and recording it would
                // allocate a Vec<Trivia> for nearly every token.
                while self.pos < self.input_len() {
                    let c = self.peek_char();
                    if c != b' ' && c != b'\t' {
                        break;
                    }
                    self.incr_pos();
                }
                continue;
            }

            if ch == b'\n' {
                self.incr_pos();
                self.pending_trivia.push(Trivia::Newline);
                continue;
            }

            // `\r\n` is one logical newline; a lone `\r` is whitespace.
            if ch == b'\r' {
                self.incr_pos();
                if self.peek_char() == b'\n' {
                    self.incr_pos();
                    self.pending_trivia.push(Trivia::Newline);
                }
                continue;
            }

            if ch == b'/' && self.peek_next() == b'/' {
                let start = self.pos;
                while self.pos < self.input_len() {
                    let c = self.peek_char();
                    if c == b'\n' || c == b'\r' {
                        break;
                    }
                    self.incr_pos();
                }
                let text = self.slice(start, self.pos);
                self.pending_trivia.push(Trivia::LineComment(text));
                continue;
            }

            if ch == b'/' && self.peek_next() == b'*' {
                let start = self.pos;
                let start_line = self.line;
                let start_column = self.column;
                self.incr_pos(); // skip /
                self.incr_pos(); // skip *

                // `/**` opens a doc comment, but `/**/` does not.
                let opens_doc = self.pos < self.input_len()
                    && self.peek_char() == b'*'
                    && self.peek_next() != b'/';

                let mut closed = false;
                while self.pos.saturating_add(1) < self.input_len() {
                    if self.peek_char() == b'*' && self.peek_next() == b'/' {
                        self.incr_pos(); // skip *
                        self.incr_pos(); // skip /
                        closed = true;
                        break;
                    }
                    self.incr_pos();
                }
                if !closed {
                    // Consume the last byte, which the loop bound left behind,
                    // so it isn't scanned again as a stray token after the error.
                    while self.pos < self.input_len() {
                        self.incr_pos();
                    }
                    // Anchor at the opening `/*`; the cursor is at EOF.
                    self.error_at(
                        Span::point(start_line, start_column),
                        ScanError::UnclosedBlockComment,
                    );
                }

                let text = self.slice(start, self.pos);
                // Only a closed `/** … */` is a doc comment. An unterminated one
                // has swallowed the rest of the file, and would attach it all to
                // the next declaration.
                self.pending_trivia.push(if opens_doc && closed {
                    Trivia::DocComment(text)
                } else {
                    Trivia::BlockComment(text)
                });
                continue;
            }

            break;
        }
    }

    fn scan_next(&mut self) -> Token {
        if let Some(&InterpFrame {
            quote,
            brace_depth: 0,
            ..
        }) = self.interp_stack.last()
        {
            return self.scan_interp_string_content(quote);
        }

        self.collect_trivia();

        self.token_start_column = self.column;
        self.token_start_line = self.line;

        if self.pos >= self.input_len() {
            // At EOF inside `${ ... }`, every frame still on the stack is an
            // unclosed string, reported at its own opening quote.
            while let Some(frame) = self.interp_stack.pop() {
                self.error_at(
                    Span::point(frame.start_line, frame.start_column),
                    ScanError::UnclosedInterpolation,
                );
            }
            return self.new_token(Kind::Eof);
        }

        let ch = self.peek_char();
        self.incr_pos();

        if self.in_interp_string() {
            if ch == b'{' {
                if let Some(f) = self.interp_stack.last_mut() {
                    f.brace_depth = f.brace_depth.saturating_add(1);
                }
                return self.new_token(Kind::PuncOpenBrace);
            }
            if ch == b'}' {
                if let Some(f) = self.interp_stack.last_mut() {
                    f.brace_depth = f.brace_depth.saturating_sub(1);
                }
                return self.new_token(Kind::PuncCloseBrace);
            }
        }

        if token::is_name_start(ch) {
            let Range { start, end } = self.scan_name();

            // Look keywords up on a borrowed slice, so they allocate no String.
            // Names are ASCII, so `from_utf8` can't fail here.
            if let Some(keyword_kind) = std::str::from_utf8(self.bytes(start, end))
                .ok()
                .and_then(token::match_keyword)
            {
                return self.new_token(keyword_kind);
            }

            let text = self.slice(start, end);
            return self.new_token(Kind::Identifier(text.into()));
        }

        if ch == b'-' && self.peek_char() == b'>' {
            self.incr_pos();
            return self.new_token(Kind::PuncArrow);
        }

        // Must come before the number check.
        if ch == b'.' && self.peek_char() == b'.' {
            self.incr_pos();
            return self.new_token(Kind::PuncDotdot);
        }

        if ch.is_ascii_digit() {
            return self.scan_number();
        }

        if is_quote(ch) {
            // Build the literal body while watching for `$`. On `${` or
            // `$ident`, rewind to the opening quote and hand off to the interp
            // scanner, so a plain string is scanned once instead of twice.
            let start_pos = self.pos;
            let start_col = self.column;
            let start_line = self.line;
            let diag_len = self.diagnostics.len();

            let mut result: Vec<u8> = Vec::new();
            loop {
                let next = self.peek_char();

                if next == 0 || next == b'\n' || next == b'\r' {
                    // Anchor at the opening quote, one byte before the body:
                    // the cursor sits at the line end or EOF.
                    self.error_at(
                        Span::point(start_line, start_col.saturating_sub(1)),
                        ScanError::UnclosedString,
                    );
                    return self.new_token(Kind::Error(utf8(result).into()));
                }

                if next == b'$' {
                    let follow = self.peek_next();
                    if follow == b'{' || token::is_name_start(follow) {
                        self.pos = start_pos;
                        self.column = start_col;
                        self.line = start_line;
                        self.diagnostics.truncate(diag_len);
                        self.enter_interp_string(ch, start_line, start_col.saturating_sub(1));
                        return self.new_token(Kind::InterpStringStart);
                    }
                }

                self.incr_pos();

                if next == ch {
                    break;
                }

                if next == b'\\' {
                    // Pushes nothing when the terminator is unconsumed, so the
                    // loop top reports the unterminated string.
                    self.scan_escape_sequence(&mut result);
                } else {
                    result.push(next);
                }
            }
            return self.new_token(Kind::LiteralString(utf8(result).into()));
        }

        if ch == b'&' && self.peek_char() == b'&' {
            self.incr_pos();
            return self.new_token(Kind::LogicalAnd);
        }

        match ch {
            b',' => self.new_token(Kind::PuncComma),
            b'(' => self.new_token(Kind::PuncOpenParen),
            b')' => self.new_token(Kind::PuncCloseParen),
            b'{' => self.new_token(Kind::PuncOpenBrace),
            b'}' => self.new_token(Kind::PuncCloseBrace),
            b'[' => self.new_token(Kind::PuncOpenBracket),
            b']' => self.new_token(Kind::PuncCloseBracket),
            b';' => self.new_token(Kind::PuncSemicolon),
            b'.' => self.new_token(Kind::PuncDot),
            b'+' => self.punc2(b'+', Kind::PuncPlusplus, Kind::PuncPlus),
            b'-' => self.punc2(b'-', Kind::PuncMinusminus, Kind::PuncMinus),
            b'*' => self.new_token(Kind::PuncMul),
            b'%' => self.new_token(Kind::PuncMod),
            b'!' => self.punc2(b'=', Kind::PuncNotEqual, Kind::PuncExclamationMark),
            b'?' => self.new_token(Kind::PuncQuestionMark),
            b'@' => self.new_token(Kind::PuncAt),
            b':' => self.punc2(b':', Kind::PuncColonColon, Kind::PuncColon),
            b'>' => match self.peek_char() {
                b'=' => {
                    self.incr_pos();
                    self.new_token(Kind::PuncGte)
                }
                b'>' => {
                    self.incr_pos();
                    self.new_token(Kind::BinClose)
                }
                _ => self.new_token(Kind::PuncGt),
            },
            b'<' => match self.peek_char() {
                b'=' => {
                    self.incr_pos();
                    self.new_token(Kind::PuncLte)
                }
                b'<' => {
                    self.incr_pos();
                    self.new_token(Kind::BinOpen)
                }
                b'-' => {
                    self.incr_pos();
                    self.new_token(Kind::PuncBackArrow)
                }
                _ => self.new_token(Kind::PuncLt),
            },
            b'/' => self.new_token(Kind::PuncDiv),
            b'|' => match self.peek_char() {
                b'|' => {
                    self.incr_pos();
                    self.new_token(Kind::LogicalOr)
                }
                b'>' => {
                    self.incr_pos();
                    self.new_token(Kind::PuncPipe)
                }
                _ => self.new_token(Kind::BitwiseOr),
            },
            b'=' => self.punc2(b'=', Kind::PuncEqualsComparator, Kind::PuncEquals),
            _ => {
                // `ch` is a raw byte; `as char` would reinterpret it as
                // Latin-1. A byte >= 0x80 starts a UTF-8 sequence, so consume
                // the whole sequence and decode it, or each byte of one
                // character gets its own error with a garbled glyph.
                if ch >= 0x80 {
                    let start = self.pos.saturating_sub(1);
                    while (0x80..=0xBF).contains(&self.peek_char()) {
                        self.incr_pos();
                    }
                    let text = self.slice(start, self.pos);
                    self.add_error(ScanError::UnexpectedChar(text.to_string()));
                    self.new_token(Kind::Error(text.into()))
                } else {
                    self.add_error(ScanError::UnexpectedChar((ch as char).to_string()));
                    self.new_token(Kind::Error((ch as char).to_string().into()))
                }
            }
        }
    }

    pub fn scan_all(&mut self) -> (Vec<Token>, Vec<Diagnostic<ScanError>>) {
        let mut tokens = Vec::new();

        loop {
            let t = self.scan_next();
            let is_eof = t.kind == Kind::Eof;
            tokens.push(t);

            if is_eof {
                break;
            }
        }

        (tokens, std::mem::take(&mut self.diagnostics))
    }

    fn new_token(&mut self, kind: Kind) -> Token {
        Token {
            kind,
            span: Span {
                start_line: self.token_start_line,
                start_column: self.token_start_column,
                end_line: self.line,
                end_column: self.column,
            },
            leading_trivia: std::mem::take(&mut self.pending_trivia),
        }
    }

    // Advances past the rest of a name and returns its `[start, end)` byte range.
    // The caller has already consumed the first byte, so the name starts at
    // `pos - 1`. Names are ASCII, so byte offsets are column offsets.
    fn scan_name(&mut self) -> Range<i32> {
        let start = self.pos.saturating_sub(1);
        while token::is_name_continue(self.peek_char()) {
            self.incr_pos();
        }
        start..self.pos
    }

    fn scan_identifier(&mut self) -> Token {
        let Range { start, end } = self.scan_name();
        let text = self.slice(start, end);
        self.new_token(Kind::Identifier(text.into()))
    }

    fn scan_number(&mut self) -> Token {
        let start = self.pos.saturating_sub(1);

        // `0x` / `0b` (either case) starts a radix literal. Commit on the
        // prefix: a following non-digit is a lexical error, not `0` plus an
        // identifier (`0xFF` used to lex as `0` `xFF`).
        if self.byte_at(start) == b'0' {
            match self.peek_char() {
                b'x' | b'X' => return self.scan_radix_number(start, 16),
                b'b' | b'B' => return self.scan_radix_number(start, 2),
                _ => {}
            }
        }

        let mut has_dot = false;

        loop {
            let next = self.peek_char();

            if next.is_ascii_digit() {
                self.incr_pos();
            } else if next == b'_' && self.peek_next().is_ascii_digit() {
                // `1_000_000`: a separator between digits, kept in the token
                // text so the formatter preserves it; `NumberLiteral::digits`
                // drops it for everything that reads the number.
                self.incr_pos();
            } else if next == b'.' && !has_dot {
                if !self.peek_next().is_ascii_digit() {
                    break;
                }
                has_dot = true;
                self.incr_pos();
            } else {
                break;
            }
        }

        let text = self.slice(start, self.pos);
        self.new_token(Kind::LiteralNumber(text.into()))
    }

    fn scan_radix_number(&mut self, start: i32, radix: u32) -> Token {
        self.incr_pos(); // consume x/X or b/B
        let mut any_digits = false;

        loop {
            let next = self.peek_char();
            if is_radix_digit(next, radix) {
                any_digits = true;
                self.incr_pos();
            } else if next == b'_' && is_radix_digit(self.peek_next(), radix) {
                self.incr_pos();
            } else {
                break;
            }
        }

        let name = if radix == 16 {
            Radix::Hex
        } else {
            Radix::Binary
        };
        if !any_digits {
            // Swallow a following name or digit run so `0xZ` / `0b2` is one
            // error, not `0x` plus a stray identifier.
            if token::is_name_start(self.peek_char()) || self.peek_char().is_ascii_digit() {
                while token::is_name_continue(self.peek_char()) {
                    self.incr_pos();
                }
            }
            self.add_error(ScanError::NoDigits(name));
            let text = self.slice(start, self.pos);
            return self.new_token(Kind::Error(text.into()));
        }

        // `0xFFG` / `0b102`: an alphanumeric after the digits is an invalid
        // digit, not a new token.
        if self.peek_char().is_ascii_alphanumeric() {
            while self.peek_char().is_ascii_alphanumeric() || self.peek_char() == b'_' {
                self.incr_pos();
            }
            self.add_error(ScanError::BadDigit(name));
            let text = self.slice(start, self.pos);
            return self.new_token(Kind::Error(text.into()));
        }

        let text = self.slice(start, self.pos);
        self.new_token(Kind::LiteralNumber(text.into()))
    }

    fn punc2(&mut self, follow: u8, two: Kind, one: Kind) -> Token {
        if self.peek_char() == follow {
            self.incr_pos();
            self.new_token(two)
        } else {
            self.new_token(one)
        }
    }

    fn peek_char(&self) -> u8 {
        self.byte_at(self.pos)
    }

    // Positions stay below `MAX_INPUT`, so adding one never saturates.
    fn peek_next(&self) -> u8 {
        self.byte_at(self.pos.saturating_add(1))
    }

    fn incr_pos(&mut self) {
        if self.byte_at(self.pos) == b'\n' {
            self.line = self.line.saturating_add(1);
            self.column = 0;
        } else {
            self.column = self.column.saturating_add(1);
        }

        self.pos = self.pos.saturating_add(1);
    }

    // Most escapes stand for one ASCII byte, but `\u{...}` stands for the UTF-8
    // encoding of a code point, which can be up to four bytes. Pushes those
    // bytes onto `out`. An unknown escape reports an error and yields the
    // escaped byte itself.
    //
    // Consumes nothing when the backslash sits at EOF or a line ending, so
    // the caller still sees the terminator.
    fn scan_escape_sequence(&mut self, out: &mut Vec<u8>) {
        let peeked = self.peek_char();
        if peeked == 0 || peeked == b'\n' || peeked == b'\r' {
            return;
        }
        self.incr_pos();

        if peeked == b'u' {
            self.scan_unicode_escape(out);
            return;
        }

        match ESCAPES.iter().find(|(source, _)| *source == peeked) {
            Some(&(_, denoted)) => out.push(denoted),
            None => {
                if peeked >= 0x80 {
                    // Decode the full UTF-8 sequence for the message. Only the
                    // lead byte is consumed here; the continuation bytes flow
                    // into the string as ordinary content behind it, so the
                    // buffer stays valid UTF-8.
                    let start = self.pos.saturating_sub(1);
                    let mut end = self.pos;
                    while (0x80..=0xBF).contains(&self.byte_at(end)) {
                        end = end.saturating_add(1);
                    }
                    let escape = self.slice(start, end).to_string();
                    self.add_error(ScanError::UnknownEscape(escape));
                } else {
                    self.add_error(ScanError::UnknownEscape((peeked as char).to_string()));
                }
                out.push(peeked);
            }
        }
    }

    // `\u{HEX+}`: hex digits naming a Unicode scalar value, encoded as UTF-8.
    // The caller has already consumed the `\u`. There's no cap on the digit
    // count: a scalar value never needs more than 6, and extra digits are
    // either leading zeros or push the value out of range, which gets the same
    // error as any too-large value. Every failure recovers by pushing U+FFFD
    // (the replacement character), so the buffer stays valid UTF-8 and
    // scanning goes on, the way `scan_radix_number` recovers from a malformed
    // number.
    fn scan_unicode_escape(&mut self, out: &mut Vec<u8>) {
        const REPLACEMENT_CHARACTER: char = '\u{FFFD}';

        if self.peek_char() != b'{' {
            self.add_error(ScanError::UnicodeEscapeWithoutBrace);
            out.push(b'u');
            return;
        }
        self.incr_pos();

        let digits_start = self.pos;
        while self.peek_char().is_ascii_hexdigit() {
            self.incr_pos();
        }
        let digits = self.slice(digits_start, self.pos);

        if self.peek_char() != b'}' {
            self.add_error(ScanError::UnclosedUnicodeEscape {
                digits: digits.to_string(),
            });
            let mut buf = [0u8; 4];
            out.extend_from_slice(REPLACEMENT_CHARACTER.encode_utf8(&mut buf).as_bytes());
            return;
        }
        self.incr_pos();

        if digits.is_empty() {
            self.add_error(ScanError::EmptyUnicodeEscape);
            let mut buf = [0u8; 4];
            out.extend_from_slice(REPLACEMENT_CHARACTER.encode_utf8(&mut buf).as_bytes());
            return;
        }

        match u32::from_str_radix(&digits, 16)
            .ok()
            .and_then(char::from_u32)
        {
            Some(c) => {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
            None => {
                self.add_error(ScanError::NotAScalarValue {
                    digits: digits.to_string(),
                });
                let mut buf = [0u8; 4];
                out.extend_from_slice(REPLACEMENT_CHARACTER.encode_utf8(&mut buf).as_bytes());
            }
        }
    }

    fn scan_interp_string_content(&mut self, quote: u8) -> Token {
        self.token_start_column = self.column;
        self.token_start_line = self.line;

        let mut result: Vec<u8> = Vec::new();

        loop {
            let ch = self.peek_char();

            if ch == 0 || ch == b'\n' || ch == b'\r' {
                // Anchor at the opening quote the frame carries; the cursor is
                // at the line end or EOF.
                let anchor = self
                    .interp_stack
                    .last()
                    .map(|f| Span::point(f.start_line, f.start_column))
                    .unwrap_or_else(|| Span::point(self.line, self.column));
                self.error_at(anchor, ScanError::UnclosedString);
                self.exit_interp_string();
                return self.new_token(Kind::Error(utf8(result).into()));
            }

            if ch == quote {
                if !result.is_empty() {
                    return self.new_token(Kind::InterpStringPart(utf8(result).into()));
                }
                self.incr_pos();
                self.exit_interp_string();
                return self.new_token(Kind::InterpStringEnd);
            }

            if ch == b'$' {
                if !result.is_empty() {
                    return self.new_token(Kind::InterpStringPart(utf8(result).into()));
                }

                self.incr_pos();
                let next = self.peek_char();

                if next == b'{' {
                    self.incr_pos();
                    if let Some(f) = self.interp_stack.last_mut() {
                        f.brace_depth = 1;
                    }
                    return self.new_token(Kind::PuncOpenBrace);
                } else if token::is_name_start(next) {
                    self.incr_pos();
                    return self.scan_identifier();
                } else {
                    result.push(b'$');
                    continue;
                }
            }

            self.incr_pos();

            if ch == b'\\' {
                // Pushes nothing when the terminator is unconsumed, so the
                // loop top reports the unterminated string.
                self.scan_escape_sequence(&mut result);
            } else {
                result.push(ch);
            }
        }
    }
}

#[inline]
fn is_quote(c: u8) -> bool {
    c == b'\'' || c == b'"'
}

fn is_radix_digit(ch: u8, radix: u32) -> bool {
    match radix {
        16 => ch.is_ascii_hexdigit(),
        2 => ch == b'0' || ch == b'1',
        _ => false,
    }
}

fn utf8(bytes: Vec<u8>) -> String {
    // Source is UTF-8, but degrade with replacement chars rather than abort if
    // a slice ever lands off a char boundary.
    String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

#[cfg(test)]
mod tests;
