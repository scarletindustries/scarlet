use crate::diagnostic::{self, Diagnostic};
use crate::token::{self, Kind, Token, Trivia, TriviaKind};

pub struct Scanner {
    input: Vec<u8>,
    pos: i32,
    column: i32,
    line: i32,
    diagnostics: Vec<Diagnostic>,
    pending_trivia: Vec<Trivia>,
    token_start_column: i32,
    token_start_line: i32,
    interp_brace_depths: Vec<i32>,
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
        interp_brace_depths: Vec::new(),
    }
}

impl Scanner {
    fn add_error(&mut self, message: String) {
        self.diagnostics
            .push(diagnostic::error_at(self.line, self.column, message));
    }

    pub fn get_diagnostics(&self) -> Vec<Diagnostic> {
        self.diagnostics.clone()
    }

    fn in_interp_string(&self) -> bool {
        !self.interp_brace_depths.is_empty()
    }

    fn interp_brace_depth(&self) -> i32 {
        *self.interp_brace_depths.last().unwrap_or(&0)
    }

    fn enter_interp_string(&mut self) {
        self.interp_brace_depths.push(0);
    }

    fn exit_interp_string(&mut self) {
        if !self.interp_brace_depths.is_empty() {
            self.interp_brace_depths.pop();
        }
    }

    fn incr_interp_brace_depth(&mut self) {
        if let Some(last) = self.interp_brace_depths.last_mut() {
            *last += 1;
        }
    }

    fn decr_interp_brace_depth(&mut self) {
        if let Some(last) = self.interp_brace_depths.last_mut() {
            *last -= 1;
        }
    }

    fn set_interp_brace_depth(&mut self, depth: i32) {
        if let Some(last) = self.interp_brace_depths.last_mut() {
            *last = depth;
        }
    }

    fn input_len(&self) -> i32 {
        self.input.len() as i32
    }

    fn byte_at(&self, pos: i32) -> u8 {
        self.input[pos as usize]
    }

    fn slice(&self, start: i32, end: i32) -> String {
        String::from_utf8_lossy(&self.input[start as usize..end as usize]).into_owned()
    }

    fn collect_trivia(&mut self) {
        while self.pos < self.input_len() {
            let ch = self.peek_char();

            if ch == b' ' || ch == b'\t' {
                let start = self.pos;
                while self.pos < self.input_len() {
                    let c = self.peek_char();
                    if c != b' ' && c != b'\t' {
                        break;
                    }
                    self.incr_pos();
                }
                let text = self.slice(start, self.pos);
                self.pending_trivia.push(Trivia {
                    kind: TriviaKind::Whitespace,
                    text,
                });
                continue;
            }

            if ch == b'\n' {
                self.incr_pos();
                self.pending_trivia.push(Trivia {
                    kind: TriviaKind::Newline,
                    text: "\n".to_string(),
                });
                continue;
            }

            if ch == b'/' && self.pos + 1 < self.input_len() && self.byte_at(self.pos + 1) == b'/' {
                let start = self.pos;
                while self.pos < self.input_len() && self.peek_char() != b'\n' {
                    self.incr_pos();
                }
                let text = self.slice(start, self.pos);
                self.pending_trivia.push(Trivia {
                    kind: TriviaKind::LineComment,
                    text,
                });
                continue;
            }

            if ch == b'/' && self.pos + 1 < self.input_len() && self.byte_at(self.pos + 1) == b'*' {
                let start = self.pos;
                self.incr_pos(); // skip /
                self.incr_pos(); // skip *

                // Doc comment if next char is * but NOT followed by /
                // i.e., /** but not /**/
                let is_doc = self.pos < self.input_len()
                    && self.peek_char() == b'*'
                    && (self.pos + 1 >= self.input_len() || self.byte_at(self.pos + 1) != b'/');

                while self.pos + 1 < self.input_len() {
                    if self.peek_char() == b'*' && self.byte_at(self.pos + 1) == b'/' {
                        self.incr_pos(); // skip *
                        self.incr_pos(); // skip /
                        break;
                    }
                    self.incr_pos();
                }

                let text = self.slice(start, self.pos);
                self.pending_trivia.push(Trivia {
                    kind: if is_doc {
                        TriviaKind::DocComment
                    } else {
                        TriviaKind::BlockComment
                    },
                    text,
                });
                continue;
            }

            break;
        }
    }

    pub fn scan_next(&mut self) -> Token {
        if self.in_interp_string() && self.interp_brace_depth() == 0 {
            return self.scan_interp_string_content();
        }

        self.collect_trivia();

        self.token_start_column = self.column;
        self.token_start_line = self.line;

        if self.pos == self.input_len() {
            return self.new_token(Kind::Eof, None);
        }

        let ch = self.peek_char();
        self.incr_pos();

        if self.in_interp_string() {
            if ch == b'{' {
                self.incr_interp_brace_depth();
                return self.new_token(Kind::PuncOpenBrace, None);
            }
            if ch == b'}' {
                self.decr_interp_brace_depth();
                return self.new_token(Kind::PuncCloseBrace, None);
            }
        }

        let ch_str = (ch as char).to_string();
        if token::is_valid_identifier(&ch_str, false) {
            let identifier = self.scan_identifier(ch);

            if let Some(keyword_kind) = token::match_keyword(identifier.literal.as_deref()) {
                return self.new_token_with_trivia(keyword_kind, None, identifier.leading_trivia);
            }

            return identifier;
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

        if ch.is_ascii_alphanumeric() {
            if ch.is_ascii_digit() {
                return self.scan_number(ch);
            }

            return self.scan_identifier(ch);
        }

        if token::is_quote(ch) {
            if ch == b'`' {
                let next = self.peek_char();
                if next == b'`' {
                    self.add_error("Character literals must not be empty".to_string());
                    self.incr_pos();
                    return self.new_token(Kind::Error, None);
                }

                self.incr_pos();

                let expected_closing_quote = self.peek_char();
                if expected_closing_quote != b'`' {
                    self.add_error(format!(
                        "Character literals must be a single character and end with a backtick (got '{}')",
                        expected_closing_quote as char
                    ));
                    loop {
                        let peek = self.peek_char();
                        if peek == 0 || peek == b'\n' || peek == b'`' {
                            if peek == b'`' {
                                self.incr_pos();
                            }
                            break;
                        }
                        self.incr_pos();
                    }
                    return self.new_token(Kind::Error, None);
                }

                self.incr_pos();

                return self.new_token(Kind::LiteralChar, Some((next as char).to_string()));
            }

            if self.has_interpolation() {
                self.enter_interp_string();
                return self.new_token(Kind::InterpStringStart, None);
            }

            let mut result = String::new();
            loop {
                let next = self.peek_char();

                if next == 0 || next == b'\n' {
                    self.add_error("Unterminated string literal".to_string());
                    return self.new_token(Kind::Error, Some(result));
                }

                self.incr_pos();

                if next == ch {
                    break;
                }

                if next == b'\\' {
                    result += &self.scan_escape_sequence();
                } else {
                    result.push(next as char);
                }
            }
            return self.new_token(Kind::LiteralString, Some(result));
        }

        if ch == b'&' && self.peek_char() == b'&' {
            self.incr_pos();
            return self.new_token(Kind::LogicalAnd, None);
        }

        if ch == b'|' && self.peek_char() == b'|' {
            self.incr_pos();
            return self.new_token(Kind::LogicalOr, None);
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
            b'+' => {
                if self.peek_char() == b'+' {
                    self.incr_pos();
                    return self.new_token(Kind::PuncPlusplus, None);
                }
                self.new_token(Kind::PuncPlus, None)
            }
            b'-' => {
                if self.peek_char() == b'-' {
                    self.incr_pos();
                    return self.new_token(Kind::PuncMinusminus, None);
                }
                self.new_token(Kind::PuncMinus, None)
            }
            b'*' => self.new_token(Kind::PuncMul, None),
            b'%' => self.new_token(Kind::PuncMod, None),
            b'!' => {
                if self.peek_char() == b'=' {
                    self.incr_pos();
                    return self.new_token(Kind::PuncNotEqual, None);
                }
                self.new_token(Kind::PuncExclamationMark, None)
            }
            b'?' => self.new_token(Kind::PuncQuestionMark, None),
            b':' => self.new_token(Kind::PuncColon, None),
            b'>' => {
                if self.peek_char() == b'=' {
                    self.incr_pos();
                    return self.new_token(Kind::PuncGte, None);
                }
                self.new_token(Kind::PuncGt, None)
            }
            b'<' => {
                if self.peek_char() == b'=' {
                    self.incr_pos();
                    return self.new_token(Kind::PuncLte, None);
                }
                self.new_token(Kind::PuncLt, None)
            }
            b'/' => self.new_token(Kind::PuncDiv, None),
            b'|' => {
                if self.peek_char() == b'|' {
                    self.incr_pos();
                    return self.new_token(Kind::LogicalOr, None);
                }
                self.new_token(Kind::BitwiseOr, None)
            }
            b'=' => {
                if self.peek_char() == b'=' {
                    self.incr_pos();
                    return self.new_token(Kind::PuncEqualsComparator, None);
                }
                self.new_token(Kind::PuncEquals, None)
            }
            b'"' => {
                self.add_error(
                    "Double quotes are not valid string delimiters. Use single quotes (') for strings or backticks (`) for character literals."
                        .to_string(),
                );

                loop {
                    let next = self.peek_char();
                    if next == 0 || next == b'\n' {
                        break;
                    }
                    self.incr_pos();
                    if next == b'"' {
                        break;
                    }
                }
                self.new_token(Kind::Error, None)
            }
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
        let trivia = self.take_trivia();
        self.new_token_with_trivia(kind, literal, trivia)
    }

    fn new_token_with_trivia(
        &mut self,
        kind: Kind,
        literal: Option<String>,
        trivia: Vec<Trivia>,
    ) -> Token {
        Token {
            kind,
            literal,
            line: self.token_start_line,
            column: self.token_start_column,
            length: self.column - self.token_start_column,
            leading_trivia: trivia,
        }
    }

    fn take_trivia(&mut self) -> Vec<Trivia> {
        std::mem::take(&mut self.pending_trivia)
    }

    fn scan_identifier(&mut self, from: u8) -> Token {
        let mut result = (from as char).to_string();

        loop {
            let peeked = self.peek_char();
            let mut next = result.clone();
            next.push(peeked as char);

            if token::is_valid_identifier(&next, false) {
                self.incr_pos();
                result = next;
            } else {
                break;
            }
        }

        self.new_token(Kind::Identifier, Some(result))
    }

    fn scan_number(&mut self, from: u8) -> Token {
        let mut result = (from as char).to_string();
        let mut has_dot = false;
        let mut chars_after_dot = 0;

        loop {
            let next = self.peek_char();

            if next == b'.' && has_dot {
                for _ in 0..chars_after_dot + 1 {
                    result.truncate(result.len() - 1);
                    self.decr_pos();
                }
                break;
            }

            if next.is_ascii_digit() {
                self.incr_pos();
                result.push(next as char);
                if has_dot {
                    chars_after_dot += 1;
                }
            } else if next == b'.' && !has_dot {
                has_dot = true;
                self.incr_pos();
                result.push(next as char);
            } else {
                break;
            }
        }

        self.new_token(Kind::LiteralNumber, Some(result))
    }

    fn peek_char(&self) -> u8 {
        if self.pos >= self.input_len() {
            return 0;
        }
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

        if self.byte_at(self.pos) == b'\n' {
            self.line -= 1;
        } else {
            self.column -= 1;
        }
    }

    fn has_interpolation(&self) -> bool {
        let mut pos = self.pos;
        while pos < self.input_len() {
            let ch = self.byte_at(pos);
            if ch == b'\'' {
                return false;
            }
            if ch == b'\n' {
                return false;
            }
            if ch == b'\\' && pos + 1 < self.input_len() {
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

    fn scan_escape_sequence(&mut self) -> String {
        let peeked = self.peek_char();
        self.incr_pos();

        match peeked {
            b'n' => "\n".to_string(),
            b't' => "\t".to_string(),
            b'r' => "\r".to_string(),
            b'0' => "\0".to_string(),
            b'"' => "\"".to_string(),
            b'\'' => "'".to_string(),
            b'\\' => "\\".to_string(),
            b'$' => "$".to_string(),
            _ => {
                self.add_error(format!("Unknown escape sequence '\\{}'", peeked as char));
                (peeked as char).to_string()
            }
        }
    }

    fn scan_interp_string_content(&mut self) -> Token {
        self.token_start_column = self.column;
        self.token_start_line = self.line;

        let mut result = String::new();

        loop {
            let ch = self.peek_char();

            if ch == 0 || ch == b'\n' {
                self.add_error("Unterminated string literal".to_string());
                self.exit_interp_string();
                return self.new_token(Kind::Error, Some(result));
            }

            if ch == b'\'' {
                self.incr_pos();
                self.exit_interp_string();
                if !result.is_empty() {
                    self.decr_pos();
                    self.enter_interp_string();
                    return self.new_token(Kind::InterpStringPart, Some(result));
                }
                return self.new_token(Kind::InterpStringEnd, None);
            }

            if ch == b'$' {
                if !result.is_empty() {
                    return self.new_token(Kind::InterpStringPart, Some(result));
                }

                self.incr_pos();
                let next = self.peek_char();

                if next == b'{' {
                    self.incr_pos();
                    self.set_interp_brace_depth(1);
                    return self.new_token(Kind::PuncOpenBrace, None);
                } else if next.is_ascii_alphabetic() || next == b'_' {
                    self.incr_pos();
                    return self.scan_identifier(next);
                } else {
                    result.push('$');
                    continue;
                }
            }

            self.incr_pos();

            if ch == b'\\' {
                result += &self.scan_escape_sequence();
            } else {
                result.push(ch as char);
            }
        }
    }
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

    #[test]
    fn test_hello_al() {
        let src = include_str!("../../examples/hello.al");
        let mut s = new_scanner(src.to_string());
        let tokens = s.scan_all();
        let kinds: Vec<Kind> = tokens.iter().map(|t| t.kind).collect();

        assert!(
            s.get_diagnostics().is_empty(),
            "scanner produced diagnostics: {:?}",
            s.get_diagnostics()
        );
        assert!(!kinds.contains(&Error), "found error token");

        #[rustfmt::skip]
        let expected = vec![
            // struct Person {
            KwStruct, Identifier, PuncOpenBrace,
            // name String
            Identifier, Identifier,
            // age Int
            Identifier, Identifier,
            // }
            PuncCloseBrace,
            // fn greet(p Person) String {
            KwFunction, Identifier, PuncOpenParen, Identifier, Identifier, PuncCloseParen, Identifier, PuncOpenBrace,
            // 'Hello, ${p.name}! You are ${p.age} years old.'
            InterpStringStart,
            InterpStringPart, PuncOpenBrace, Identifier, PuncDot, Identifier, PuncCloseBrace,
            InterpStringPart, PuncOpenBrace, Identifier, PuncDot, Identifier, PuncCloseBrace,
            InterpStringPart, InterpStringEnd,
            // }
            PuncCloseBrace,
            // person = Person{ name: 'Alice', age: 30 }
            Identifier, PuncEquals, Identifier, PuncOpenBrace,
            Identifier, PuncColon, LiteralString, PuncComma,
            Identifier, PuncColon, LiteralNumber, PuncCloseBrace,
            // println(greet(person))
            Identifier, PuncOpenParen, Identifier, PuncOpenParen, Identifier, PuncCloseParen, PuncCloseParen,
            Eof,
        ];

        assert_eq!(kinds, expected);
    }

    #[test]
    fn test_fizzbuzz_al() {
        let src = include_str!("../../examples/fizzbuzz.al");
        let mut s = new_scanner(src.to_string());
        let tokens = s.scan_all();
        let kinds: Vec<Kind> = tokens.iter().map(|t| t.kind).collect();

        assert!(s.get_diagnostics().is_empty());
        assert!(!kinds.contains(&Error));

        #[rustfmt::skip]
        let expected_prefix = vec![
            // fn fizzbuzz(n Int) String {
            KwFunction, Identifier, PuncOpenParen, Identifier, Identifier, PuncCloseParen, Identifier, PuncOpenBrace,
            // match true {
            KwMatch, KwTrue, PuncOpenBrace,
            // n % 15 == 0 -> 'FizzBuzz',
            Identifier, PuncMod, LiteralNumber, PuncEqualsComparator, LiteralNumber, PuncArrow, LiteralString, PuncComma,
            // n % 3 == 0 -> 'Fizz',
            Identifier, PuncMod, LiteralNumber, PuncEqualsComparator, LiteralNumber, PuncArrow, LiteralString, PuncComma,
            // n % 5 == 0 -> 'Buzz',
            Identifier, PuncMod, LiteralNumber, PuncEqualsComparator, LiteralNumber, PuncArrow, LiteralString, PuncComma,
            // else -> '${n}',
            KwElse, PuncArrow, InterpStringStart, PuncOpenBrace, Identifier, PuncCloseBrace, InterpStringEnd, PuncComma,
            // }
            PuncCloseBrace,
            // }
            PuncCloseBrace,
            // [fizzbuzz(1), ...
            PuncOpenBracket,
        ];

        assert_eq!(&kinds[..expected_prefix.len()], &expected_prefix[..]);
        assert_eq!(*kinds.last().unwrap(), Eof);

        // The array literal: [ + 20*(ident ( num ) ,?) + ]
        // 20 calls = 20*4 tokens + 19 commas + open/close bracket = 101
        let array_part = &kinds[expected_prefix.len() - 1..kinds.len() - 1];
        assert_eq!(array_part[0], PuncOpenBracket);
        assert_eq!(*array_part.last().unwrap(), PuncCloseBracket);
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
        let ks = kinds(", ( ) { } [ ] ; . .. -> + ++ - -- * / % ! != ? : = == > >= < <= && || |");
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
    fn test_string_and_char() {
        let mut s = new_scanner("'hello' `c` 'a\\nb'".to_string());
        let toks = s.scan_all();
        assert_eq!(toks[0].kind, LiteralString);
        assert_eq!(toks[0].literal.as_deref(), Some("hello"));
        assert_eq!(toks[1].kind, LiteralChar);
        assert_eq!(toks[1].literal.as_deref(), Some("c"));
        assert_eq!(toks[2].kind, LiteralString);
        assert_eq!(toks[2].literal.as_deref(), Some("a\nb"));
    }

    #[test]
    fn test_keywords_vs_identifiers() {
        let ks = kinds("fn if else struct foo _bar");
        assert_eq!(
            ks,
            vec![
                KwFunction, KwIf, KwElse, KwStruct, Identifier, Identifier, Eof
            ]
        );
    }

    #[test]
    fn test_trivia_attached_to_next_token() {
        let mut s = new_scanner("  // hi\nfoo".to_string());
        let toks = s.scan_all();
        assert_eq!(toks[0].kind, Identifier);
        let trivia = &toks[0].leading_trivia;
        assert_eq!(trivia.len(), 3);
        assert_eq!(trivia[0].kind, TriviaKind::Whitespace);
        assert_eq!(trivia[1].kind, TriviaKind::LineComment);
        assert_eq!(trivia[1].text, "// hi");
        assert_eq!(trivia[2].kind, TriviaKind::Newline);
    }

    #[test]
    fn test_single_ampersand_is_error() {
        let mut s = new_scanner("&".to_string());
        let toks = s.scan_all();
        assert_eq!(toks[0].kind, Error);
        assert_eq!(s.get_diagnostics().len(), 1);
    }

    #[test]
    fn test_double_quotes_error() {
        let mut s = new_scanner("\"hello\"".to_string());
        let toks = s.scan_all();
        assert_eq!(toks[0].kind, Error);
        assert_eq!(s.get_diagnostics().len(), 1);
    }
}
