module compiler

import lib.compiler.token

pub struct Scanner {
	input string
mut:
	pos    int // current position in input (points to current char)
	line   int = 1
	column int = 1
}

pub fn new_scanner(input string) &Scanner {
	return &Scanner{
		input: input
		pos: 0
	}
}

pub fn (mut s Scanner) scan_next() Token {
	// Read next character from input
	ch := s.peek_char()
	s.incr_pos()

	// Skip whitespace
	if ch.is_space() {
		return s.scan_next()
	}

	if ch.is_alnum() {
		return s.scan_identifier_or_keyword(ch)
	}

	if ch.is_digit() {
		return s.scan_number(ch)
	}

	if token.is_quote(ch) {
		return s.scan_string(ch)
	}

	if token.is_name_char(ch) {
		return s.scan_identifier(ch)
	}

	panic('unexpected character "${ch.ascii_str()}" at line ${s.line} column ${s.column}')
}

pub fn (mut s Scanner) scan_identifier_or_keyword(ch byte) Token {
	identifier := s.scan_identifier(ch)

	if unwrapped := identifier.literal {
		if keyword_kind := token.match_keyword(unwrapped) {
			return s.new_token(keyword_kind, none)
		}
	}

	return identifier
}

pub fn (mut s Scanner) scan_string(q byte) Token {
	mut result := ''

	for {
		ch := s.peek_char()
		s.incr_pos()

		if ch == q {
			break
		}

		result += ch.ascii_str()
	}

	return s.new_token(.literal_string, result)
}

pub fn (mut s Scanner) scan_all() []Token {
	mut tokens := []Token{}

	for {
		t := s.scan_next()
		tokens << t

		if t.kind == .eof {
			break
		}
	}

	return tokens
}

fn (s Scanner) new_token(kind token.Kind, literal ?string) Token {
	return Token{
		kind: kind
		literal: literal
		line: s.line
		column: s.column
	}
}

// scan_identifier scans until the next non-alphanumeric character
fn (mut s Scanner) scan_identifier(from byte) Token {
	mut current := from
	mut result := ''

	for {
		result += current.ascii_str()
		next := s.peek_char()

		if token.is_name_char(next) {
			s.incr_pos()
			current = next
		} else {
			break
		}
	}

	return s.new_token(.literal_ident, result)
}

fn (mut s Scanner) scan_number(from byte) Token {
	mut current := from
	mut result := ''

	for current.is_digit() {
		current = s.peek_char()
		s.incr_pos()
		result += current.ascii_str()
	}

	return s.new_token(.literal_number, result)
}

fn (mut s Scanner) peek_char() byte {
	if s.pos >= s.input.len {
		panic('Scanner: at end of input')
	}

	ch := s.input[s.pos]

	return ch
}

fn (mut s Scanner) incr_pos() {
	s.pos++

	if s.input[s.pos] == `\n` {
		s.line++
		s.column = 0
	} else {
		s.column++
	}
}
