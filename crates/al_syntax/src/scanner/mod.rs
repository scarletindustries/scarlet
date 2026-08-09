use crate::diagnostic::{Diagnostic, DiagnosticCode};
use crate::span::Span;
use crate::token::{self, Kind, Token, Trivia};

/// One level of string-interpolation nesting. `brace_depth == 0` means the
/// scanner is in the string body; `> 0` means it is inside `${ ... }` and
/// counts unmatched `{` so a nested brace does not close the interpolation.
#[derive(Clone, Copy)]
struct InterpFrame {
    quote: u8,
    brace_depth: u32,
    // The opening quote, so a string abandoned at EOF is diagnosed there.
    start_line: i32,
    start_column: i32,
}

pub struct Scanner {
    input: Vec<u8>,
    pos: i32,
    column: i32,
    line: i32,
    diagnostics: Vec<Diagnostic>,
    pending_trivia: Vec<Trivia>,
    token_start_column: i32,
    token_start_line: i32,
    interp_stack: Vec<InterpFrame>,
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
        self.diagnostics.push(Diagnostic::error(
            Span::point(self.line, self.column),
            DiagnosticCode::ParseError,
            message,
        ));
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
        self.input.len() as i32
    }

    // The only byte accessor, and it is total: a negative `pos` wraps to a huge
    // usize, so both past-EOF and negative cursors read as the `0` EOF sentinel
    // every consumer already treats as end of input.
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

            if ch == b'/' && self.byte_at(self.pos + 1) == b'/' {
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

            if ch == b'/' && self.byte_at(self.pos + 1) == b'*' {
                let start = self.pos;
                let start_line = self.line;
                let start_column = self.column;
                self.incr_pos(); // skip /
                self.incr_pos(); // skip *

                // `/**` opens a doc comment, but `/**/` does not.
                let opens_doc = self.pos < self.input_len()
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
                    // Swallow the tail byte the loop bound left unconsumed so
                    // it is not re-lexed as a spurious token after the error.
                    while self.pos < self.input_len() {
                        self.incr_pos();
                    }
                    // Anchor at the opening `/*`; the cursor is at EOF.
                    self.diagnostics.push(Diagnostic::error(
                        Span::point(start_line, start_column),
                        DiagnosticCode::UnexpectedEof,
                        "Unterminated block comment".to_string(),
                    ));
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
            // EOF inside `${ ... }`: every frame still on the stack is an
            // unterminated string, diagnosed at its own opening quote.
            while let Some(frame) = self.interp_stack.pop() {
                self.diagnostics.push(Diagnostic::error(
                    Span::point(frame.start_line, frame.start_column),
                    DiagnosticCode::UnexpectedEof,
                    "Unterminated string literal".to_string(),
                ));
            }
            return self.new_token(Kind::Eof);
        }

        let ch = self.peek_char();
        self.incr_pos();

        if self.in_interp_string() {
            if ch == b'{' {
                if let Some(f) = self.interp_stack.last_mut() {
                    f.brace_depth += 1;
                }
                return self.new_token(Kind::PuncOpenBrace);
            }
            if ch == b'}' {
                if let Some(f) = self.interp_stack.last_mut() {
                    f.brace_depth -= 1;
                }
                return self.new_token(Kind::PuncCloseBrace);
            }
        }

        if token::is_name_start(ch) {
            let (start, end) = self.scan_name();

            // Keyword lookup on a borrowed slice, so keywords allocate no
            // String. Names are ASCII, so `from_utf8` cannot fail here.
            if let Some(keyword_kind) =
                std::str::from_utf8(&self.input[start as usize..end as usize])
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

        // Must do this check before checking for numbers
        if ch == b'.' && self.peek_char() == b'.' {
            self.incr_pos();
            return self.new_token(Kind::PuncDotdot);
        }

        if ch.is_ascii_digit() {
            return self.scan_number();
        }

        if is_quote(ch) {
            // Single pass: build the literal body while watching for `$`. On
            // seeing `${` / `$ident` we rewind to just after the opening quote
            // and hand off to the interp scanner, so plain (non-interpolated)
            // strings — the common case — are scanned once instead of twice.
            let start_pos = self.pos;
            let start_col = self.column;
            let start_line = self.line;
            let diag_len = self.diagnostics.len();

            let mut result: Vec<u8> = Vec::new();
            loop {
                let next = self.peek_char();

                if next == 0 || next == b'\n' || next == b'\r' {
                    // Anchor at the opening quote (one byte before the body
                    // start): the cursor sits at the line end / EOF, away from
                    // the string that is actually broken.
                    self.diagnostics.push(Diagnostic::error(
                        Span::point(start_line, start_col - 1),
                        DiagnosticCode::ParseError,
                        "Unterminated string literal".to_string(),
                    ));
                    return self.new_token(Kind::Error(utf8(result).into()));
                }

                if next == b'$' {
                    let follow = self.byte_at(self.pos + 1);
                    if follow == b'{' || token::is_name_start(follow) {
                        self.pos = start_pos;
                        self.column = start_col;
                        self.line = start_line;
                        self.diagnostics.truncate(diag_len);
                        self.enter_interp_string(ch, start_line, start_col - 1);
                        return self.new_token(Kind::InterpStringStart);
                    }
                }

                self.incr_pos();

                if next == ch {
                    break;
                }

                if next == b'\\' {
                    // None leaves the terminator unconsumed; the check at the
                    // top of the loop then reports the unterminated string.
                    if let Some(b) = self.scan_escape_sequence() {
                        result.push(b);
                    }
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
            b':' => self.new_token(Kind::PuncColon),
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
                _ => self.new_token(Kind::PuncLt),
            },
            b'/' => self.new_token(Kind::PuncDiv),
            b'|' => self.punc2(b'|', Kind::LogicalOr, Kind::BitwiseOr),
            b'=' => self.punc2(b'=', Kind::PuncEqualsComparator, Kind::PuncEquals),
            _ => {
                self.add_error(format!("Unexpected character '{}'", ch as char));
                self.new_token(Kind::Error((ch as char).to_string().into()))
            }
        }
    }

    pub fn scan_all(&mut self) -> (Vec<Token>, Vec<Diagnostic>) {
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
        self.new_token(Kind::Identifier(text.into()))
    }

    fn scan_number(&mut self) -> Token {
        let start = self.pos - 1;
        let mut has_dot = false;

        loop {
            let next = self.peek_char();

            if next.is_ascii_digit() {
                self.incr_pos();
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

    fn incr_pos(&mut self) {
        if self.byte_at(self.pos) == b'\n' {
            self.line += 1;
            self.column = 0;
        } else {
            self.column += 1;
        }

        self.pos += 1;
    }

    // Every recognized escape denotes a single ASCII byte, so this returns
    // the byte directly instead of allocating a String per escape. An unknown
    // escape recovers by yielding the escaped byte itself (preserving any
    // following raw UTF-8 continuation bytes) after reporting a diagnostic.
    //
    // Returns None — WITHOUT consuming — when the backslash sits at EOF or a
    // line ending, so the caller's unterminated-string check still sees the
    // terminator instead of the escape swallowing it.
    fn scan_escape_sequence(&mut self) -> Option<u8> {
        let peeked = self.peek_char();
        if peeked == 0 || peeked == b'\n' || peeked == b'\r' {
            return None;
        }
        self.incr_pos();

        Some(match peeked {
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
        })
    }

    fn scan_interp_string_content(&mut self, quote: u8) -> Token {
        self.token_start_column = self.column;
        self.token_start_line = self.line;

        let mut result: Vec<u8> = Vec::new();

        loop {
            let ch = self.peek_char();

            if ch == 0 || ch == b'\n' || ch == b'\r' {
                // Anchor at the opening quote — the frame carries its position
                // exactly for this. The cursor is at the line end / EOF, which
                // points away from the string that is actually broken.
                let anchor = self
                    .interp_stack
                    .last()
                    .map(|f| Span::point(f.start_line, f.start_column))
                    .unwrap_or_else(|| Span::point(self.line, self.column));
                self.diagnostics.push(Diagnostic::error(
                    anchor,
                    DiagnosticCode::ParseError,
                    "Unterminated string literal".to_string(),
                ));
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
                // None leaves the terminator unconsumed; the check at the
                // top of the loop then reports the unterminated string.
                if let Some(b) = self.scan_escape_sequence() {
                    result.push(b);
                }
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
    String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token::Keyword as Kw;
    use Kind::*;

    fn scan(src: &str) -> (Vec<Token>, Vec<Diagnostic>) {
        new_scanner(src).scan_all()
    }

    fn ident(s: &str) -> Kind {
        Identifier(s.into())
    }

    fn num(s: &str) -> Kind {
        LiteralNumber(s.into())
    }

    fn lit(s: &str) -> Kind {
        LiteralString(s.into())
    }

    fn part(s: &str) -> Kind {
        InterpStringPart(s.into())
    }

    #[track_caller]
    fn assert_tok(toks: &[Token], i: usize, kind: Kind) {
        assert_eq!(toks[i].kind, kind, "token {i}");
    }

    fn kinds(input: &str) -> Vec<Kind> {
        scan(input).0.into_iter().map(|t| t.kind).collect()
    }

    /// Like `kinds`, but asserts the scan produced no diagnostics or Error tokens.
    fn kinds_clean(input: &str) -> Vec<Kind> {
        let (toks, diags) = scan(input);
        assert!(diags.is_empty(), "scanner produced diagnostics: {diags:?}");
        let kinds: Vec<Kind> = toks.into_iter().map(|t| t.kind).collect();
        assert!(
            !kinds.iter().any(|k| matches!(k, Error(_))),
            "found error token"
        );
        kinds
    }

    // Inlined rather than include_str!'d from examples/: these tests assert an
    // exact token vector, which would otherwise freeze the example corpus.
    const HELLO_SRC: &str = "// Hello world, plus string interpolation with ${}.\n\nprintln('hello, world')\n\nname = 'AL'\nprintln('hello from ${name}')\nprintln('2 + 2 = ${2 + 2}')\n";

    const FIZZBUZZ_SRC: &str = "fn fizzbuzz(n Int) String {\n\tmatch (n % 3, n % 5) {\n\t\t(0, 0) -> 'FizzBuzz'\n\t\t(0, _) -> 'Fizz'\n\t\t(_, 0) -> 'Buzz'\n\t\telse -> '${n}'\n\t}\n}\n\nfn run(n Int, last Int) Nil {\n\tif n > last {\n\t\tNil\n\t} else {\n\t\tprintln(fizzbuzz(n))\n\t\trun(n + 1, last)\n\t}\n}\n\nrun(1, 20)\n";

    #[test]
    fn scans_hello_world() {
        let kinds = kinds_clean(HELLO_SRC);

        #[rustfmt::skip]
        let expected = vec![
            // println('hello, world')
            ident("println"), PuncOpenParen, lit("hello, world"), PuncCloseParen,
            // name = 'AL'
            ident("name"), PuncEquals, lit("AL"),
            // println('hello from ${name}')
            ident("println"), PuncOpenParen,
            InterpStringStart, part("hello from "), PuncOpenBrace, ident("name"), PuncCloseBrace, InterpStringEnd,
            PuncCloseParen,
            // println('2 + 2 = ${2 + 2}')
            ident("println"), PuncOpenParen,
            InterpStringStart, part("2 + 2 = "), PuncOpenBrace, num("2"), PuncPlus, num("2"), PuncCloseBrace, InterpStringEnd,
            PuncCloseParen,
            Eof,
        ];

        assert_eq!(kinds, expected);
    }

    #[test]
    fn scans_fizzbuzz() {
        let kinds = kinds_clean(FIZZBUZZ_SRC);

        #[rustfmt::skip]
        let expected = vec![
            // fn fizzbuzz(n Int) String {
            Keyword(Kw::Fn), ident("fizzbuzz"), PuncOpenParen, ident("n"), ident("Int"), PuncCloseParen, ident("String"), PuncOpenBrace,
            // match (n % 3, n % 5) {
            Keyword(Kw::Match), PuncOpenParen, ident("n"), PuncMod, num("3"), PuncComma, ident("n"), PuncMod, num("5"), PuncCloseParen, PuncOpenBrace,
            // (0, 0) -> 'FizzBuzz'
            PuncOpenParen, num("0"), PuncComma, num("0"), PuncCloseParen, PuncArrow, lit("FizzBuzz"),
            // (0, _) -> 'Fizz'
            PuncOpenParen, num("0"), PuncComma, ident("_"), PuncCloseParen, PuncArrow, lit("Fizz"),
            // (_, 0) -> 'Buzz'
            PuncOpenParen, ident("_"), PuncComma, num("0"), PuncCloseParen, PuncArrow, lit("Buzz"),
            // else -> '${n}'
            Keyword(Kw::Else), PuncArrow, InterpStringStart, PuncOpenBrace, ident("n"), PuncCloseBrace, InterpStringEnd,
            // } }
            PuncCloseBrace, PuncCloseBrace,
            // fn run(n Int, last Int) Nil {
            Keyword(Kw::Fn), ident("run"), PuncOpenParen, ident("n"), ident("Int"), PuncComma, ident("last"), ident("Int"), PuncCloseParen, ident("Nil"), PuncOpenBrace,
            // if n > last {
            Keyword(Kw::If), ident("n"), PuncGt, ident("last"), PuncOpenBrace,
            // Nil
            ident("Nil"),
            // } else {
            PuncCloseBrace, Keyword(Kw::Else), PuncOpenBrace,
            // println(fizzbuzz(n))
            ident("println"), PuncOpenParen, ident("fizzbuzz"), PuncOpenParen, ident("n"), PuncCloseParen, PuncCloseParen,
            // run(n + 1, last)
            ident("run"), PuncOpenParen, ident("n"), PuncPlus, num("1"), PuncComma, ident("last"), PuncCloseParen,
            // } }
            PuncCloseBrace, PuncCloseBrace,
            // run(1, 20)
            ident("run"), PuncOpenParen, num("1"), PuncComma, num("20"), PuncCloseParen,
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
                ident("a"),
                PuncCloseBrace,
                PuncOpenBrace,
                ident("b"),
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
                ident("a"),
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
                part("a"),
                PuncOpenBrace,
                InterpStringStart,
                part("b"),
                PuncOpenBrace,
                ident("c"),
                PuncCloseBrace,
                part("d"),
                InterpStringEnd,
                PuncCloseBrace,
                part("e"),
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
        assert_eq!(ks, vec![ident("a"), PuncGt, ident("b"), Eof]);
        let ks = kinds("a<b");
        assert_eq!(ks, vec![ident("a"), PuncLt, ident("b"), Eof]);
        let ks = kinds("a>=b");
        assert_eq!(ks, vec![ident("a"), PuncGte, ident("b"), Eof]);
    }

    #[test]
    fn test_bin_open_close() {
        let ks = kinds("<<1, 2>>");
        assert_eq!(
            ks,
            vec![BinOpen, num("1"), PuncComma, num("2"), BinClose, Eof]
        );
        let ks = kinds("<<>>");
        assert_eq!(ks, vec![BinOpen, BinClose, Eof]);
        // Single < / > between << >> must remain comparison ops.
        let ks = kinds("<< a < b >>");
        assert_eq!(
            ks,
            vec![BinOpen, ident("a"), PuncLt, ident("b"), BinClose, Eof]
        );
        // Maximal munch: <<= is << then =, not < then <=.
        let ks = kinds("<<= >>=");
        assert_eq!(ks, vec![BinOpen, PuncEquals, BinClose, PuncEquals, Eof]);
    }

    #[test]
    fn test_numbers() {
        let (toks, _) = scan("123 4.56 1..5");
        assert_tok(&toks, 0, num("123"));
        assert_tok(&toks, 1, num("4.56"));
        // 1..5 -> number(1), dotdot, number(5)
        assert_tok(&toks, 2, num("1"));
        assert_tok(&toks, 3, PuncDotdot);
        assert_tok(&toks, 4, num("5"));
    }

    #[test]
    fn test_string_literals() {
        let (toks, _) = scan("'hello' 'a\\nb'");
        assert_tok(&toks, 0, lit("hello"));
        assert_tok(&toks, 1, lit("a\nb"));
    }

    #[test]
    fn test_keywords_vs_identifiers() {
        let ks = kinds("fn if else type foo _bar");
        assert_eq!(
            ks,
            vec![
                Keyword(Kw::Fn),
                Keyword(Kw::If),
                Keyword(Kw::Else),
                Keyword(Kw::Type),
                ident("foo"),
                ident("_bar"),
                Eof
            ]
        );
    }

    #[test]
    fn test_trivia_attached_to_next_token() {
        let (toks, _) = scan("  // hi\nfoo");
        assert_eq!(toks[0].kind, ident("foo"));
        let trivia = &toks[0].leading_trivia;
        assert_eq!(trivia.len(), 2);
        assert_eq!(trivia[0], Trivia::LineComment("// hi".to_string()));
        assert_eq!(trivia[1], Trivia::Newline);
    }

    #[test]
    fn unterminated_doc_comment_is_not_a_doc_comment() {
        let (toks, diags) = scan("/** Module prose\npub fn f() Int { 1 }\n");
        assert_eq!(toks[0].kind, Eof, "the whole file is swallowed as trivia");
        let trivia = &toks[0].leading_trivia;
        assert!(
            !trivia.iter().any(|t| matches!(t, Trivia::DocComment(_))),
            "an unterminated `/**` must not donate a doc comment: {trivia:?}"
        );
        assert!(matches!(trivia[0], Trivia::BlockComment(_)));
        assert_eq!(diags.len(), 1);
    }

    #[test]
    fn unterminated_block_comment_reports_at_the_opening_delimiter() {
        let (_, diags) = scan("x = 1\n  /** oops\nmore text\n");
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].message, "Unterminated block comment");
        // Line 1, column 2: the `/` of the `/**`, not EOF on line 3.
        assert_eq!(
            (diags[0].span.start_line, diags[0].span.start_column),
            (1, 2)
        );
    }

    #[test]
    fn test_single_ampersand_is_error() {
        let (toks, diags) = scan("&");
        assert_eq!(toks[0].kind, Error("&".into()));
        assert_eq!(diags.len(), 1);
    }

    #[test]
    fn test_double_quotes_accepted() {
        let (toks, diags) = scan("\"hello\"");
        assert_tok(&toks, 0, lit("hello"));
        assert!(diags.is_empty());
    }

    #[test]
    fn test_double_quote_with_single_inside() {
        let (toks, _) = scan("\"it's fine\"");
        assert_tok(&toks, 0, lit("it's fine"));
    }

    #[test]
    fn test_double_quote_interpolation() {
        let (toks, diags) = scan("\"hi ${x}\"");
        assert_eq!(toks[0].kind, InterpStringStart);
        assert!(diags.is_empty());
    }

    #[test]
    fn test_mixed_quote_nesting() {
        let ks = kinds_clean("\"outer ${'inner'}\"");
        assert!(ks.contains(&InterpStringStart));
        assert!(ks.contains(&lit("inner")));
        assert!(ks.contains(&InterpStringEnd));
    }

    #[test]
    fn test_lone_backtick_no_crash() {
        let (toks, diags) = scan("`");
        let ks: Vec<Kind> = toks.into_iter().map(|t| t.kind).collect();
        // completing scan_all without abort/hang IS the proof
        assert_eq!(*ks.last().unwrap(), Eof);
        assert!(ks.iter().any(|k| matches!(k, Error(_))));
        assert!(!diags.is_empty());
    }

    #[test]
    fn test_trailing_backslash_unterminated_string_no_crash() {
        let (toks, diags) = scan("x := \"\\");
        let ks: Vec<Kind> = toks.into_iter().map(|t| t.kind).collect();
        assert_eq!(*ks.last().unwrap(), Eof);
        assert!(ks.iter().any(|k| matches!(k, Error(_))));
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("Unterminated string literal"))
        );
    }

    #[test]
    fn test_backtick_then_eof_terminates() {
        // bare backtick at true EOF exercises the incr_pos-at-EOF path
        assert_eq!(*kinds("a`").last().unwrap(), Eof);
    }

    #[test]
    fn test_float_then_range() {
        // Regression: `1.5..10` used to backtrack over the first `.` and lex
        // as [1][.][5][..][10]. It must lex as [1.5][..][10].
        let (toks, _) = scan("1.5..10");
        assert_tok(&toks, 0, num("1.5"));
        assert_tok(&toks, 1, PuncDotdot);
        assert_tok(&toks, 2, num("10"));
        assert_tok(&toks, 3, Eof);
    }

    #[test]
    fn test_float_then_method() {
        let (toks, _) = scan("1.5.foo");
        assert_tok(&toks, 0, num("1.5"));
        assert_tok(&toks, 1, PuncDot);
        assert_tok(&toks, 2, ident("foo"));
        assert_tok(&toks, 3, Eof);
    }

    #[test]
    fn test_bare_dollar_is_not_interpolation() {
        // A `$` not followed by `{` or an identifier start is a literal char,
        // so the scanner must not classify the string as interpolated.
        let (toks, _) = scan("'a$5'");
        assert_tok(&toks, 0, lit("a$5"));
        assert_tok(&toks, 1, Eof);

        // Bare `$` at end of string.
        assert_eq!(kinds("'a$'"), vec![lit("a$"), Eof]);

        // But `${` and `$ident` still trigger interpolation.
        assert_eq!(kinds("'a${x}'")[0], InterpStringStart);
        assert_eq!(kinds("'a$x'")[0], InterpStringStart);
    }

    #[test]
    fn test_backslash_at_newline_is_unterminated_not_unknown_escape() {
        // The `\` must not consume the newline: the string is reported as
        // unterminated (once, at the line end) and the next line still lexes.
        let (toks, diags) = scan("x = 'abc\\\ny = 1");
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("Unterminated string literal")),
            "diagnostics: {diags:?}"
        );
        assert!(
            !diags.iter().any(|d| d.message.contains("Unknown escape")),
            "the newline must not be treated as an escape body: {diags:?}"
        );
        let ks: Vec<Kind> = toks.into_iter().map(|t| t.kind).collect();
        // `y = 1` after the broken string survives as ordinary tokens.
        assert!(ks.ends_with(&[ident("y"), PuncEquals, num("1"), Eof]));
    }

    #[test]
    fn test_backslash_at_eof_single_unterminated_error() {
        let (toks, diags) = scan("x = '\\");
        assert_eq!(*toks.last().map(|t| t.kind.clone()).as_ref().unwrap(), Eof);
        assert_eq!(diags.len(), 1, "diagnostics: {diags:?}");
        assert!(diags[0].message.contains("Unterminated string literal"));
    }

    #[test]
    fn test_crlf_sources_scan_clean() {
        let ks = kinds_clean("a = 1\r\nb = 2\r\n");
        assert_eq!(
            ks,
            vec![
                ident("a"),
                PuncEquals,
                num("1"),
                ident("b"),
                PuncEquals,
                num("2"),
                Eof
            ]
        );

        // \r\n collapses to a single Newline trivia on the next token.
        let (toks, _) = scan("a\r\nb");
        assert_eq!(toks[1].leading_trivia, vec![Trivia::Newline]);

        // A lone \r is plain whitespace, not a newline and not an error.
        let (toks, diags) = scan("a \r b");
        assert!(diags.is_empty(), "diagnostics: {diags:?}");
        assert!(toks[1].leading_trivia.is_empty());

        // Line comments end before the \r so their text stays clean.
        let (toks, _) = scan("// hi\r\nfoo");
        assert_eq!(
            toks[0].leading_trivia[0],
            Trivia::LineComment("// hi".to_string())
        );

        // CRLF line accounting matches LF: `b` starts on line 1.
        let (toks, _) = scan("a\r\nb");
        assert_eq!(toks[1].span.start_line, 1);
    }

    #[test]
    fn test_string_cannot_swallow_half_a_crlf() {
        // An unterminated string ends at the \r; the \r\n then scans as one
        // newline instead of leaving a stray \n or \r token behind.
        let (toks, diags) = scan("x = 'abc\r\ny = 1");
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("Unterminated string literal")),
            "diagnostics: {diags:?}"
        );
        let ks: Vec<Kind> = toks.into_iter().map(|t| t.kind).collect();
        assert!(!ks.contains(&PuncDiv), "no stray tokens from the CRLF");
        assert!(ks.ends_with(&[ident("y"), PuncEquals, num("1"), Eof]));
    }

    #[test]
    fn test_unterminated_plain_string_reports_at_opening_quote() {
        // Anchored at the opening quote (line 0, column 4), not at EOF.
        let (_, diags) = scan("x = 'abc");
        assert_eq!(diags.len(), 1, "diagnostics: {diags:?}");
        assert!(diags[0].message.contains("Unterminated string literal"));
        assert_eq!(
            (diags[0].span.start_line, diags[0].span.start_column),
            (0, 4)
        );
    }

    #[test]
    fn test_unterminated_interp_string_content_reports_at_opening_quote() {
        // The `${b}` closes, then the trailing content runs into EOF: the
        // error is anchored at the opening quote (column 4), not at EOF.
        let (_, diags) = scan("x = 'a${b} cd");
        let unterminated: Vec<_> = diags
            .iter()
            .filter(|d| d.message.contains("Unterminated string literal"))
            .collect();
        assert_eq!(unterminated.len(), 1, "diagnostics: {diags:?}");
        assert_eq!(
            (
                unterminated[0].span.start_line,
                unterminated[0].span.start_column
            ),
            (0, 4)
        );
    }

    #[test]
    fn test_unterminated_interpolation_reports_at_opening_quote() {
        // EOF inside `${...}` used to abandon the interp frame silently:
        // `x = '${abc` produced no diagnostic at all while `x = 'abc` did.
        let (toks, diags) = scan("x = '${abc");
        assert_eq!(*toks.last().map(|t| t.kind.clone()).as_ref().unwrap(), Eof);
        let unterminated: Vec<_> = diags
            .iter()
            .filter(|d| d.message.contains("Unterminated string literal"))
            .collect();
        assert_eq!(unterminated.len(), 1, "diagnostics: {diags:?}");
        // Anchored at the opening quote (line 0, column 4), not at EOF.
        assert_eq!(
            (
                unterminated[0].span.start_line,
                unterminated[0].span.start_column
            ),
            (0, 4)
        );
    }

    #[test]
    fn test_nested_unterminated_interpolation_one_error_per_string() {
        // Two abandoned frames -> two errors, each at its own opening quote.
        let (_, diags) = scan("'${\"${a");
        let cols: Vec<i32> = diags
            .iter()
            .filter(|d| d.message.contains("Unterminated string literal"))
            .map(|d| d.span.start_column)
            .collect();
        assert_eq!(cols.len(), 2, "diagnostics: {diags:?}");
        assert!(cols.contains(&0) && cols.contains(&3), "cols: {cols:?}");
    }

    #[test]
    fn test_unknown_escape_sequence_diagnostic() {
        // `\q` is not a recognized escape; the scanner reports it but recovers,
        // still emitting a well-formed string token for the rest of the input.
        let (toks, diags) = scan("x = \"a\\qb\"");
        let ks: Vec<Kind> = toks.into_iter().map(|t| t.kind).collect();
        assert_eq!(*ks.last().unwrap(), Eof);
        assert!(ks.contains(&lit("aqb")));
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("Unknown escape sequence '\\q'")),
            "diagnostics: {diags:?}"
        );
    }
}
