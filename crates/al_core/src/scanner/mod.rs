use crate::diagnostic::{self, Diagnostic, DiagnosticCode};
use crate::span::Span;
use crate::token::{self, Kind, Token, Trivia};

pub struct Scanner {
    input: Vec<u8>,
    pos: i32,
    column: i32,
    line: i32,
    diagnostics: Vec<Diagnostic>,
    pending_trivia: Vec<Trivia>,
    token_start_column: i32,
    token_start_line: i32,
    interp_stack: Vec<(u8, i32)>,
}

#[inline]
pub fn new_scanner(input: impl Into<String>) -> Scanner {
    Scanner {
        input: input.into().into_bytes(),
        pos: 0,
        column: 0,
        line: 0,
        diagnostics: Vec::new(),
        pending_trivia: Vec::new(),
        token_start_column: 0,
        token_start_line: 0,
        interp_stack: Vec::new(),
    }
}

impl Scanner {
    fn add_error(&mut self, message: String) {
        self.diagnostics
            .push(diagnostic::error_at(self.line, self.column, message));
    }

    pub fn take_diagnostics(&mut self) -> Vec<Diagnostic> {
        std::mem::take(&mut self.diagnostics)
    }

    fn in_interp_string(&self) -> bool {
        !self.interp_stack.is_empty()
    }

    fn enter_interp_string(&mut self, quote: u8) {
        self.interp_stack.push((quote, 0));
    }

    fn exit_interp_string(&mut self) {
        self.interp_stack.pop();
    }

    fn input_len(&self) -> i32 {
        self.input.len() as i32
    }

    // The single, total byte accessor. `pos as usize` of a negative i32 wraps
    // to a huge value, so `get` returns None for both past-EOF and negative
    // cursors; the universal `0` EOF sentinel every consumer already treats as
    // end-of-input is returned in those cases. This makes out-of-bounds reads
    // unrepresentable rather than a panic.
    fn byte_at(&self, pos: i32) -> u8 {
        self.input.get(pos as usize).copied().unwrap_or(0)
    }

    fn slice(&self, start: i32, end: i32) -> String {
        let len = self.input.len();
        let s = (start.max(0) as usize).min(len);
        let e = (end.max(0) as usize).min(len).max(s);
        String::from_utf8_lossy(&self.input[s..e]).into_owned()
    }

    fn collect_trivia(&mut self) {
        while self.pos < self.input_len() {
            let ch = self.peek_char();

            if ch == b' ' || ch == b'\t' {
                // Whitespace trivia is never read by any consumer (parser and
                // formatter match only Newline/comment kinds), so don't record
                // it at all — recording it would force a fresh Vec<Trivia>
                // allocation for nearly every token, since most tokens follow
                // a space.
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

            if ch == b'/' && self.byte_at(self.pos + 1) == b'/' {
                let start = self.pos;
                while self.pos < self.input_len() && self.peek_char() != b'\n' {
                    self.incr_pos();
                }
                let text = self.slice(start, self.pos);
                self.pending_trivia.push(Trivia::LineComment(text));
                continue;
            }

            if ch == b'/' && self.byte_at(self.pos + 1) == b'*' {
                let start = self.pos;
                self.incr_pos(); // skip /
                self.incr_pos(); // skip *

                // Doc comment if next char is * but NOT followed by /
                // i.e., /** but not /**/
                let is_doc = self.pos < self.input_len()
                    && self.peek_char() == b'*'
                    && self.byte_at(self.pos + 1) != b'/';

                let mut closed = false;
                while self.pos + 1 < self.input_len() {
                    if self.peek_char() == b'*' && self.byte_at(self.pos + 1) == b'/' {
                        self.incr_pos(); // skip *
                        self.incr_pos(); // skip /
                        closed = true;
                        break;
                    }
                    self.incr_pos();
                }
                if !closed {
                    // Swallow whatever tail byte the loop bound left unconsumed
                    // so it isn't re-lexed as a spurious token after the error.
                    while self.pos < self.input_len() {
                        self.incr_pos();
                    }
                    self.diagnostics.push(
                        diagnostic::error_at(
                            self.line,
                            self.column,
                            "Unterminated block comment".to_string(),
                        )
                        .with_code(DiagnosticCode::UnexpectedEof),
                    );
                }

                let text = self.slice(start, self.pos);
                self.pending_trivia.push(if is_doc {
                    Trivia::DocComment(text)
                } else {
                    Trivia::BlockComment(text)
                });
                continue;
            }

            break;
        }
    }

    pub fn scan_next(&mut self) -> Token {
        if let Some(&(quote, 0)) = self.interp_stack.last() {
            return self.scan_interp_string_content(quote);
        }

        self.collect_trivia();

        self.token_start_column = self.column;
        self.token_start_line = self.line;

        if self.pos >= self.input_len() {
            return self.new_token(Kind::Eof, None);
        }

        let ch = self.peek_char();
        self.incr_pos();

        if self.in_interp_string() {
            if ch == b'{' {
                if let Some((_, d)) = self.interp_stack.last_mut() {
                    *d += 1;
                }
                return self.new_token(Kind::PuncOpenBrace, None);
            }
            if ch == b'}' {
                if let Some((_, d)) = self.interp_stack.last_mut() {
                    *d -= 1;
                }
                return self.new_token(Kind::PuncCloseBrace, None);
            }
        }

        if token::is_name_start(ch) {
            let (start, end) = self.scan_name();

            // Keyword lookup runs on a borrowed slice of the source, so the
            // common keyword tokens (`if`/`fn`/`match`/...) allocate no String
            // at all. The name is ASCII by construction (is_name_start /
            // is_name_continue), so from_utf8 never fails; the `ok()` merely
            // avoids a deny(expect_used) — a non-UTF-8 slice cannot occur.
            if let Some(keyword_kind) =
                std::str::from_utf8(&self.input[start as usize..end as usize])
                    .ok()
                    .and_then(token::match_keyword)
            {
                return self.new_token(keyword_kind, None);
            }

            let text = self.slice(start, end);
            return self.new_token(Kind::Identifier, Some(text));
        }

        if ch == b'-' && self.peek_char() == b'>' {
            self.incr_pos();
            return self.new_token(Kind::PuncArrow, None);
        }

        // Must do this check before checking for numbers
        if ch == b'.' && self.peek_char() == b'.' {
            self.incr_pos();
            return self.new_token(Kind::PuncDotdot, None);
        }

        if ch.is_ascii_digit() {
            return self.scan_number();
        }

        if is_quote(ch) {
            if self.has_interpolation(ch) {
                self.enter_interp_string(ch);
                return self.new_token(Kind::InterpStringStart, None);
            }

            let mut result: Vec<u8> = Vec::new();
            loop {
                let next = self.peek_char();

                if next == 0 || next == b'\n' {
                    self.add_error("Unterminated string literal".to_string());
                    return self.new_token(Kind::Error, Some(utf8(result)));
                }

                self.incr_pos();

                if next == ch {
                    break;
                }

                if next == b'\\' {
                    let b = self.scan_escape_sequence();
                    result.push(b);
                } else {
                    result.push(next);
                }
            }
            return self.new_token(Kind::LiteralString, Some(utf8(result)));
        }

        if ch == b'&' && self.peek_char() == b'&' {
            self.incr_pos();
            return self.new_token(Kind::LogicalAnd, None);
        }

        match ch {
            b',' => self.new_token(Kind::PuncComma, None),
            b'(' => self.new_token(Kind::PuncOpenParen, None),
            b')' => self.new_token(Kind::PuncCloseParen, None),
            b'{' => self.new_token(Kind::PuncOpenBrace, None),
            b'}' => self.new_token(Kind::PuncCloseBrace, None),
            b'[' => self.new_token(Kind::PuncOpenBracket, None),
            b']' => self.new_token(Kind::PuncCloseBracket, None),
            b';' => self.new_token(Kind::PuncSemicolon, None),
            b'.' => self.new_token(Kind::PuncDot, None),
            b'+' => self.punc2(b'+', Kind::PuncPlusplus, Kind::PuncPlus),
            b'-' => self.punc2(b'-', Kind::PuncMinusminus, Kind::PuncMinus),
            b'*' => self.new_token(Kind::PuncMul, None),
            b'%' => self.new_token(Kind::PuncMod, None),
            b'!' => self.punc2(b'=', Kind::PuncNotEqual, Kind::PuncExclamationMark),
            b'?' => self.new_token(Kind::PuncQuestionMark, None),
            b'@' => self.new_token(Kind::PuncAt, None),
            b':' => self.new_token(Kind::PuncColon, None),
            b'>' => match self.peek_char() {
                b'=' => {
                    self.incr_pos();
                    self.new_token(Kind::PuncGte, None)
                }
                b'>' => {
                    self.incr_pos();
                    self.new_token(Kind::BinClose, None)
                }
                _ => self.new_token(Kind::PuncGt, None),
            },
            b'<' => match self.peek_char() {
                b'=' => {
                    self.incr_pos();
                    self.new_token(Kind::PuncLte, None)
                }
                b'<' => {
                    self.incr_pos();
                    self.new_token(Kind::BinOpen, None)
                }
                _ => self.new_token(Kind::PuncLt, None),
            },
            b'/' => self.new_token(Kind::PuncDiv, None),
            b'|' => self.punc2(b'|', Kind::LogicalOr, Kind::BitwiseOr),
            b'=' => self.punc2(b'=', Kind::PuncEqualsComparator, Kind::PuncEquals),
            _ => {
                self.add_error(format!("Unexpected character '{}'", ch as char));
                self.new_token(Kind::Error, Some((ch as char).to_string()))
            }
        }
    }

    pub fn scan_all(&mut self) -> Vec<Token> {
        let mut result = Vec::new();

        loop {
            let t = self.scan_next();
            let is_eof = t.kind == Kind::Eof;
            result.push(t);

            if is_eof {
                break;
            }
        }

        result
    }

    fn new_token(&mut self, kind: Kind, literal: Option<String>) -> Token {
        Token {
            kind,
            literal,
            span: Span {
                start_line: self.token_start_line,
                start_column: self.token_start_column,
                end_line: self.line,
                end_column: self.column,
            },
            leading_trivia: std::mem::take(&mut self.pending_trivia),
        }
    }

    // Advance past the rest of a name and return its `[start, end)` byte range.
    // The first byte is already consumed by the caller, so the name begins one
    // byte back. Names are ASCII (is_name_continue), so byte offsets are char
    // offsets and `pos - 1` is always >= 0 here.
    fn scan_name(&mut self) -> (i32, i32) {
        let start = self.pos - 1;
        while token::is_name_continue(self.peek_char()) {
            self.incr_pos();
        }
        (start, self.pos)
    }

    fn scan_identifier(&mut self) -> Token {
        let (start, end) = self.scan_name();
        let text = self.slice(start, end);
        self.new_token(Kind::Identifier, Some(text))
    }

    fn scan_number(&mut self) -> Token {
        let start = self.pos - 1;
        let mut has_dot = false;
        let mut chars_after_dot = 0;

        loop {
            let next = self.peek_char();

            if next == b'.' && has_dot {
                // Back the cursor out of the second `.` and its preceding
                // digits; `slice(start, pos)` then excludes them naturally.
                for _ in 0..chars_after_dot + 1 {
                    self.decr_pos();
                }
                break;
            }

            if next.is_ascii_digit() {
                self.incr_pos();
                if has_dot {
                    chars_after_dot += 1;
                }
            } else if next == b'.' && !has_dot {
                if !self.byte_at(self.pos + 1).is_ascii_digit() {
                    break;
                }
                has_dot = true;
                self.incr_pos();
            } else {
                break;
            }
        }

        let text = self.slice(start, self.pos);
        self.new_token(Kind::LiteralNumber, Some(text))
    }

    fn punc2(&mut self, follow: u8, two: Kind, one: Kind) -> Token {
        if self.peek_char() == follow {
            self.incr_pos();
            self.new_token(two, None)
        } else {
            self.new_token(one, None)
        }
    }

    fn peek_char(&self) -> u8 {
        self.byte_at(self.pos)
    }

    fn incr_pos(&mut self) {
        if self.byte_at(self.pos) == b'\n' {
            self.line += 1;
            self.column = 0;
        } else {
            self.column += 1;
        }

        self.pos += 1;
    }

    fn decr_pos(&mut self) {
        self.pos -= 1;
        // The only caller (`scan_number`) backs out over ASCII digits and `.`,
        // never a newline. The old newline branch decremented `line` but could
        // not restore `column` (previous line's length is unknown), silently
        // corrupting spans; assert the invariant instead.
        debug_assert_ne!(
            self.byte_at(self.pos),
            b'\n',
            "decr_pos must not cross a newline"
        );
        self.column -= 1;
    }

    fn has_interpolation(&self, quote: u8) -> bool {
        let mut pos = self.pos;
        while pos < self.input_len() {
            let ch = self.byte_at(pos);
            if ch == quote {
                return false;
            }
            if ch == b'\n' {
                return false;
            }
            if ch == b'\\' {
                pos += 2;
                continue;
            }
            if ch == b'$' {
                return true;
            }
            pos += 1;
        }
        false
    }

    // Every recognized escape denotes a single ASCII byte, so this returns
    // the byte directly instead of allocating a String per escape. An unknown
    // escape recovers by yielding the escaped byte itself (preserving any
    // following raw UTF-8 continuation bytes) after reporting a diagnostic.
    fn scan_escape_sequence(&mut self) -> u8 {
        let peeked = self.peek_char();
        self.incr_pos();

        match peeked {
            b'n' => b'\n',
            b't' => b'\t',
            b'r' => b'\r',
            b'0' => b'\0',
            b'"' => b'"',
            b'\'' => b'\'',
            b'\\' => b'\\',
            b'$' => b'$',
            _ => {
                self.add_error(format!("Unknown escape sequence '\\{}'", peeked as char));
                peeked
            }
        }
    }

    fn scan_interp_string_content(&mut self, quote: u8) -> Token {
        self.token_start_column = self.column;
        self.token_start_line = self.line;

        let mut result: Vec<u8> = Vec::new();

        loop {
            let ch = self.peek_char();

            if ch == 0 || ch == b'\n' {
                self.add_error("Unterminated string literal".to_string());
                self.exit_interp_string();
                return self.new_token(Kind::Error, Some(utf8(result)));
            }

            if ch == quote {
                if !result.is_empty() {
                    return self.new_token(Kind::InterpStringPart, Some(utf8(result)));
                }
                self.incr_pos();
                self.exit_interp_string();
                return self.new_token(Kind::InterpStringEnd, None);
            }

            if ch == b'$' {
                if !result.is_empty() {
                    return self.new_token(Kind::InterpStringPart, Some(utf8(result)));
                }

                self.incr_pos();
                let next = self.peek_char();

                if next == b'{' {
                    self.incr_pos();
                    if let Some((_, d)) = self.interp_stack.last_mut() {
                        *d = 1;
                    }
                    return self.new_token(Kind::PuncOpenBrace, None);
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
                let b = self.scan_escape_sequence();
                result.push(b);
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

fn utf8(bytes: Vec<u8>) -> String {
    // Source is UTF-8 by construction; degrade with replacement chars rather
    // than aborting the compiler if a slice ever lands off a char boundary.
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use Kind::*;

    fn kinds(input: &str) -> Vec<Kind> {
        new_scanner(input.to_string())
            .scan_all()
            .into_iter()
            .map(|t| t.kind)
            .collect()
    }

    /// Like `kinds`, but asserts the scan produced no diagnostics or Error tokens.
    fn kinds_clean(input: &str) -> Vec<Kind> {
        let mut s = new_scanner(input.to_string());
        let kinds: Vec<Kind> = s.scan_all().into_iter().map(|t| t.kind).collect();
        let diags = s.take_diagnostics();
        assert!(diags.is_empty(), "scanner produced diagnostics: {diags:?}");
        assert!(!kinds.contains(&Error), "found error token");
        kinds
    }

    #[test]
    fn test_hello_al() {
        let src = include_str!("../../../../examples/hello.al");
        let kinds = kinds_clean(src);

        #[rustfmt::skip]
        let expected = vec![
            // println('hello, world')
            Identifier, PuncOpenParen, LiteralString, PuncCloseParen,
            // name = 'AL'
            Identifier, PuncEquals, LiteralString,
            // println('hello from ${name}')
            Identifier, PuncOpenParen,
            InterpStringStart, InterpStringPart, PuncOpenBrace, Identifier, PuncCloseBrace, InterpStringEnd,
            PuncCloseParen,
            // println('2 + 2 = ${2 + 2}')
            Identifier, PuncOpenParen,
            InterpStringStart, InterpStringPart, PuncOpenBrace, LiteralNumber, PuncPlus, LiteralNumber, PuncCloseBrace, InterpStringEnd,
            PuncCloseParen,
            Eof,
        ];

        assert_eq!(kinds, expected);
    }

    #[test]
    fn test_fizzbuzz_al() {
        let src = include_str!("../../../../examples/fizzbuzz.al");
        let kinds = kinds_clean(src);

        #[rustfmt::skip]
        let expected = vec![
            // fn fizzbuzz(n Int) String {
            KwFunction, Identifier, PuncOpenParen, Identifier, Identifier, PuncCloseParen, Identifier, PuncOpenBrace,
            // match (n % 3, n % 5) {
            KwMatch, PuncOpenParen, Identifier, PuncMod, LiteralNumber, PuncComma, Identifier, PuncMod, LiteralNumber, PuncCloseParen, PuncOpenBrace,
            // (0, 0) -> 'FizzBuzz'
            PuncOpenParen, LiteralNumber, PuncComma, LiteralNumber, PuncCloseParen, PuncArrow, LiteralString,
            // (0, _) -> 'Fizz'
            PuncOpenParen, LiteralNumber, PuncComma, Identifier, PuncCloseParen, PuncArrow, LiteralString,
            // (_, 0) -> 'Buzz'
            PuncOpenParen, Identifier, PuncComma, LiteralNumber, PuncCloseParen, PuncArrow, LiteralString,
            // else -> '${n}'
            KwElse, PuncArrow, InterpStringStart, PuncOpenBrace, Identifier, PuncCloseBrace, InterpStringEnd,
            // } }
            PuncCloseBrace, PuncCloseBrace,
            // fn run(n Int, last Int) Nil {
            KwFunction, Identifier, PuncOpenParen, Identifier, Identifier, PuncComma, Identifier, Identifier, PuncCloseParen, Identifier, PuncOpenBrace,
            // if n > last {
            KwIf, Identifier, PuncGt, Identifier, PuncOpenBrace,
            // Nil
            Identifier,
            // } else {
            PuncCloseBrace, KwElse, PuncOpenBrace,
            // println(fizzbuzz(n))
            Identifier, PuncOpenParen, Identifier, PuncOpenParen, Identifier, PuncCloseParen, PuncCloseParen,
            // run(n + 1, last)
            Identifier, PuncOpenParen, Identifier, PuncPlus, LiteralNumber, PuncComma, Identifier, PuncCloseParen,
            // } }
            PuncCloseBrace, PuncCloseBrace,
            // run(1, 20)
            Identifier, PuncOpenParen, LiteralNumber, PuncComma, LiteralNumber, PuncCloseParen,
            Eof,
        ];

        assert_eq!(kinds, expected);
    }

    #[test]
    fn test_interp_nesting() {
        // '${a}${b}' - two adjacent interpolations
        let ks = kinds("'${a}${b}'");
        assert_eq!(
            ks,
            vec![
                InterpStringStart,
                PuncOpenBrace,
                Identifier,
                PuncCloseBrace,
                PuncOpenBrace,
                Identifier,
                PuncCloseBrace,
                InterpStringEnd,
                Eof,
            ]
        );

        // nested braces inside ${...}
        let ks = kinds("'${ {a} }'");
        assert_eq!(
            ks,
            vec![
                InterpStringStart,
                PuncOpenBrace,
                PuncOpenBrace,
                Identifier,
                PuncCloseBrace,
                PuncCloseBrace,
                InterpStringEnd,
                Eof,
            ]
        );

        // nested interpolated string inside ${...}
        let ks = kinds("'a${'b${c}d'}e'");
        assert_eq!(
            ks,
            vec![
                InterpStringStart,
                InterpStringPart,
                PuncOpenBrace,
                InterpStringStart,
                InterpStringPart,
                PuncOpenBrace,
                Identifier,
                PuncCloseBrace,
                InterpStringPart,
                InterpStringEnd,
                PuncCloseBrace,
                InterpStringPart,
                InterpStringEnd,
                Eof,
            ]
        );
    }

    #[test]
    fn test_punctuation() {
        let ks =
            kinds(", ( ) { } [ ] ; . .. -> + ++ - -- * / % ! != ? : = == > >= < <= << >> && || |");
        assert_eq!(
            ks,
            vec![
                PuncComma,
                PuncOpenParen,
                PuncCloseParen,
                PuncOpenBrace,
                PuncCloseBrace,
                PuncOpenBracket,
                PuncCloseBracket,
                PuncSemicolon,
                PuncDot,
                PuncDotdot,
                PuncArrow,
                PuncPlus,
                PuncPlusplus,
                PuncMinus,
                PuncMinusminus,
                PuncMul,
                PuncDiv,
                PuncMod,
                PuncExclamationMark,
                PuncNotEqual,
                PuncQuestionMark,
                PuncColon,
                PuncEquals,
                PuncEqualsComparator,
                PuncGt,
                PuncGte,
                PuncLt,
                PuncLte,
                BinOpen,
                BinClose,
                LogicalAnd,
                LogicalOr,
                BitwiseOr,
                Eof,
            ]
        );
    }

    #[test]
    fn test_gt_lt_do_not_overconsume() {
        // Regression: V scanner had a bug where > and < always consumed the next char.
        let ks = kinds("a>b");
        assert_eq!(ks, vec![Identifier, PuncGt, Identifier, Eof]);
        let ks = kinds("a<b");
        assert_eq!(ks, vec![Identifier, PuncLt, Identifier, Eof]);
        let ks = kinds("a>=b");
        assert_eq!(ks, vec![Identifier, PuncGte, Identifier, Eof]);
    }

    #[test]
    fn test_bin_open_close() {
        let ks = kinds("<<1, 2>>");
        assert_eq!(
            ks,
            vec![
                BinOpen,
                LiteralNumber,
                PuncComma,
                LiteralNumber,
                BinClose,
                Eof
            ]
        );
        let ks = kinds("<<>>");
        assert_eq!(ks, vec![BinOpen, BinClose, Eof]);
        // Single < / > between << >> must remain comparison ops.
        let ks = kinds("<< a < b >>");
        assert_eq!(
            ks,
            vec![BinOpen, Identifier, PuncLt, Identifier, BinClose, Eof]
        );
        // Maximal munch: <<= is << then =, not < then <=.
        let ks = kinds("<<= >>=");
        assert_eq!(ks, vec![BinOpen, PuncEquals, BinClose, PuncEquals, Eof]);
    }

    #[test]
    fn test_numbers() {
        let mut s = new_scanner("123 4.56 1..5".to_string());
        let toks = s.scan_all();
        assert_eq!(toks[0].kind, LiteralNumber);
        assert_eq!(toks[0].literal.as_deref(), Some("123"));
        assert_eq!(toks[1].kind, LiteralNumber);
        assert_eq!(toks[1].literal.as_deref(), Some("4.56"));
        // 1..5 → number(1), dotdot, number(5)
        assert_eq!(toks[2].kind, LiteralNumber);
        assert_eq!(toks[2].literal.as_deref(), Some("1"));
        assert_eq!(toks[3].kind, PuncDotdot);
        assert_eq!(toks[4].kind, LiteralNumber);
        assert_eq!(toks[4].literal.as_deref(), Some("5"));
    }

    #[test]
    fn test_string_literals() {
        let mut s = new_scanner("'hello' 'a\\nb'".to_string());
        let toks = s.scan_all();
        assert_eq!(toks[0].kind, LiteralString);
        assert_eq!(toks[0].literal.as_deref(), Some("hello"));
        assert_eq!(toks[1].kind, LiteralString);
        assert_eq!(toks[1].literal.as_deref(), Some("a\nb"));
    }

    #[test]
    fn test_keywords_vs_identifiers() {
        let ks = kinds("fn if else type foo _bar");
        assert_eq!(
            ks,
            vec![
                KwFunction, KwIf, KwElse, KwType, Identifier, Identifier, Eof
            ]
        );
    }

    #[test]
    fn test_trivia_attached_to_next_token() {
        let mut s = new_scanner("  // hi\nfoo".to_string());
        let toks = s.scan_all();
        assert_eq!(toks[0].kind, Identifier);
        let trivia = &toks[0].leading_trivia;
        assert_eq!(trivia.len(), 2);
        assert_eq!(trivia[0], Trivia::LineComment("// hi".to_string()));
        assert_eq!(trivia[1], Trivia::Newline);
    }

    #[test]
    fn test_single_ampersand_is_error() {
        let mut s = new_scanner("&".to_string());
        let toks = s.scan_all();
        assert_eq!(toks[0].kind, Error);
        assert_eq!(s.take_diagnostics().len(), 1);
    }

    #[test]
    fn test_double_quotes_accepted() {
        let mut s = new_scanner("\"hello\"".to_string());
        let toks = s.scan_all();
        assert_eq!(toks[0].kind, LiteralString);
        assert_eq!(toks[0].literal.as_deref(), Some("hello"));
        assert!(s.take_diagnostics().is_empty());
    }

    #[test]
    fn test_double_quote_with_single_inside() {
        let mut s = new_scanner("\"it's fine\"".to_string());
        let toks = s.scan_all();
        assert_eq!(toks[0].kind, LiteralString);
        assert_eq!(toks[0].literal.as_deref(), Some("it's fine"));
    }

    #[test]
    fn test_double_quote_interpolation() {
        let mut s = new_scanner("\"hi ${x}\"".to_string());
        let toks = s.scan_all();
        assert_eq!(toks[0].kind, InterpStringStart);
        assert!(s.take_diagnostics().is_empty());
    }

    #[test]
    fn test_mixed_quote_nesting() {
        let mut s = new_scanner("\"outer ${'inner'}\"".to_string());
        let kinds: Vec<_> = s.scan_all().iter().map(|t| t.kind).collect();
        assert!(kinds.contains(&InterpStringStart));
        assert!(kinds.contains(&LiteralString));
        assert!(kinds.contains(&InterpStringEnd));
        assert!(s.take_diagnostics().is_empty());
    }

    #[test]
    fn test_lone_backtick_no_crash() {
        let mut s = new_scanner("`".to_string());
        let ks: Vec<Kind> = s.scan_all().into_iter().map(|t| t.kind).collect();
        // completing scan_all without abort/hang IS the proof
        assert_eq!(*ks.last().unwrap(), Eof);
        assert!(ks.contains(&Error));
        assert!(!s.take_diagnostics().is_empty());
    }

    #[test]
    fn test_trailing_backslash_unterminated_string_no_crash() {
        let mut s = new_scanner("x := \"\\".to_string());
        let ks: Vec<Kind> = s.scan_all().into_iter().map(|t| t.kind).collect();
        assert_eq!(*ks.last().unwrap(), Eof);
        assert!(ks.contains(&Error));
        assert!(
            s.take_diagnostics()
                .iter()
                .any(|d| d.message.contains("Unterminated string literal"))
        );
    }

    #[test]
    fn test_backtick_then_eof_terminates() {
        // bare backtick at true EOF exercises the incr_pos-at-EOF path
        let ks: Vec<Kind> = new_scanner("a`".to_string())
            .scan_all()
            .into_iter()
            .map(|t| t.kind)
            .collect();
        assert_eq!(*ks.last().unwrap(), Eof);
    }

    #[test]
    fn test_unknown_escape_sequence_diagnostic() {
        // `\q` is not a recognized escape; the scanner reports it but recovers,
        // still emitting a well-formed string token for the rest of the input.
        let mut s = new_scanner("x = \"a\\qb\"".to_string());
        let ks: Vec<Kind> = s.scan_all().into_iter().map(|t| t.kind).collect();
        assert_eq!(*ks.last().unwrap(), Eof);
        assert!(ks.contains(&LiteralString));
        let diags = s.take_diagnostics();
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("Unknown escape sequence '\\q'")),
            "diagnostics: {diags:?}"
        );
    }
}
