module compiler

import lib.compiler.token

// default_column_n exists because we don't like
// magic numbers, and we reuse the value in multiple
// places
const default_column_n = 0

pub struct Scanner {
	input string
mut:
	pos    int // current position in input (points to current char)
	column int = compiler.default_column_n
	line   int = 1
}

pub fn new_scanner(input string) &Scanner {
	return &Scanner{
		input: input
		pos: 0
	}
}

pub fn (mut s Scanner) scan_next() Token {
	if s.pos == s.input.len {
		return s.new_token(.eof, none)
	}

	// Read next character from input
	ch := s.peek_char()
	s.incr_pos()

	// Skip whitespace
	if ch.is_space() {
		return s.scan_next()
	}

	if token.is_valid_identifier(ch.ascii_str(), false) {
		return s.scan_identifier_or_keyword(ch)
	}

	if ch.is_alnum() {
		return s.scan_identifier_or_number(ch)
	}

	if token.is_quote(ch) {
		if ch == `\`` {
			next := s.peek_char()
			assert next != `\``, 'Char literals must not be empty'

			s.incr_pos()

			expected_closing_quote := s.peek_char()
			assert expected_closing_quote == `\``, 'Char literals must be a single character and end with a backtick (got ${expected_closing_quote.ascii_str()})'

			// Skip the closing quote
			s.incr_pos()

			return s.new_token(.literal_char, next.ascii_str())
		}

		mut result := ''

		for {
			next := s.peek_char()
			s.incr_pos()

			if next == ch {
				break
			}

			result += next.ascii_str()
		}

		return s.new_token(.literal_string, result)
	}

	return match ch {
		`,` {
			s.new_token(.punc_comma, none)
		}
		`(` {
			s.new_token(.punc_open_paren, none)
		}
		`)` {
			s.new_token(.punc_close_paren, none)
		}
		`{` {
			s.new_token(.punc_open_brace, none)
		}
		`}` {
			s.new_token(.punc_close_brace, none)
		}
		`[` {
			s.new_token(.punc_open_bracket, none)
		}
		`]` {
			s.new_token(.punc_close_bracket, none)
		}
		`;` {
			s.new_token(.punc_semicolon, none)
		}
		`.` {
			s.new_token(.punc_dot, none)
		}
		`:` {
			next := s.peek_char()
			s.incr_pos()

			if next == `=` {
				return s.new_token(.punc_declaration, none)
			}

			return s.new_token(.punc_colon, none)
		}
		`>` {
			next := s.peek_char()
			s.incr_pos()

			if next == `=` {
				return s.new_token(.punc_gte, none)
			}

			return s.new_token(.punc_gt, none)
		}
		`/` {
			next := s.peek_char()

			// Handling a comment, we should skip until the end of the line
			if next == `/` {
				mut end_of_line := s.pos

				for {
					if s.input[end_of_line] == `\n` {
						break
					}

					end_of_line++
				}

				s.set_pos(end_of_line)

				return s.scan_next()
			}

			return s.new_token(.punc_div, none)
		}
		`=` {
			next := s.peek_char()

			if next == `=` {
				s.incr_pos()
				return s.new_token(.punc_equals_comparator, none)
			}

			return s.new_token(.punc_equals, none)
		}
		else {
			panic('unexpected character \'${ch.ascii_str()}\' at line ${s.line} column ${s.column}')
		}
	}
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

pub fn (mut s Scanner) set_pos(pos int) {
	s.pos = pos
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
	mut result := from.ascii_str()

	for {
		next := result + s.peek_char().ascii_str()

		if token.is_valid_identifier(next, false) {
			s.incr_pos()
			result = next
		} else {
			break
		}
	}

	return s.new_token(.literal_ident, result)
}

fn (mut s Scanner) scan_identifier_or_number(from byte) Token {
	if from.is_digit() {
		return s.scan_number(from)
	}

	return s.scan_identifier(from)
}

fn (mut s Scanner) scan_number(from byte) Token {
	mut result := from.ascii_str()

	for {
		next := s.peek_char()

		if next.is_digit() {
			s.incr_pos()
			result += next.ascii_str()
		} else {
			break
		}
	}

	return s.new_token(.literal_number, result)
}

fn (mut s Scanner) peek_char() byte {
	assert s.pos < s.input.len, 'scanner at end of input'

	ch := s.input[s.pos]

	return ch
}

fn (mut s Scanner) incr_pos() {
	if s.input[s.pos] == `\n` {
		s.line++
		s.column = compiler.default_column_n
	} else {
		s.column++
	}

	s.pos++
}
