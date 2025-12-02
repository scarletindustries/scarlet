module scanner

import compiler
import compiler.token
import compiler.scanner.state

@[heap]
pub struct Scanner {
	input string
mut:
	state &state.ScannerState
}

@[inline]
pub fn new_scanner(input string) &Scanner {
	return &Scanner{
		input: input
		state: &state.ScannerState{}
	}
}

pub fn (mut s Scanner) scan_next() compiler.Token {
	if s.state.get_pos() == s.input.len {
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
		identifier := s.scan_identifier(ch)

		if unwrapped := identifier.literal {
			if keyword_kind := token.match_keyword(unwrapped) {
				return s.new_token(keyword_kind, none)
			}
		}

		return identifier
	}

	if ch == `-` && s.peek_char() == `>` {
		s.incr_pos()
		return s.new_token(.punc_arrow, none)
	}

	// Must do this check before checking for numbers
	if ch == `.` && s.peek_char() == `.` {
		s.incr_pos()
		return s.new_token(.punc_dotdot, none)
	}

	if ch.is_alnum() {
		if ch.is_digit() {
			return s.scan_number(ch)
		}

		return s.scan_identifier(ch)
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
		mut has_interpolation := false

		for {
			next := s.peek_char()
			s.incr_pos()

			if next == ch {
				break
			}

			mut next_char := next.ascii_str()

			if next == `$` {
				has_interpolation = true
			}

			if next == `\\` {
				peeked := s.peek_char()
				s.incr_pos()

				if peeked == `n` {
					next_char = '\n'
				} else if peeked == `t` {
					next_char = '\t'
				} else if peeked == `r` {
					next_char = '\r'
				} else if peeked == `0` {
					next_char = '\0'
				} else if peeked == `"` {
					next_char = '"'
				} else if peeked == `'` {
					next_char = "'"
				} else if peeked == `\\` {
					next_char = '\\'
				} else if peeked == `$` {
					// Escaped $, don't mark as interpolation
					next_char = '$'
				} else {
					panic('unknown escape sequence \'${peeked}\'')
				}
			}

			result += next_char
		}

		if has_interpolation {
			return s.new_token(.literal_string_interpolation, result)
		}
		return s.new_token(.literal_string, result)
	}

	if ch == `&` {
		if s.peek_char() == `&` {
			s.incr_pos()
			return s.new_token(.logical_and, none)
		}
	}

	if ch == `|` {
		if s.peek_char() == `|` {
			s.incr_pos()
			return s.new_token(.logical_or, none)
		}
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
		`+` {
			if s.peek_char() == `+` {
				s.incr_pos()
				return s.new_token(.punc_plusplus, none)
			}

			return s.new_token(.punc_plus, none)
		}
		`-` {
			if s.peek_char() == `-` {
				s.incr_pos()
				return s.new_token(.punc_minusminus, none)
			}

			return s.new_token(.punc_minus, none)
		}
		`*` {
			s.new_token(.punc_mul, none)
		}
		`%` {
			s.new_token(.punc_mod, none)
		}
		`!` {
			if s.peek_char() == `=` {
				s.incr_pos()
				return s.new_token(.punc_not_equal, none)
			}
			s.new_token(.punc_exclamation_mark, none)
		}
		`?` {
			s.new_token(.punc_question_mark, none)
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
		`<` {
			next := s.peek_char()
			s.incr_pos()

			if next == `=` {
				return s.new_token(.punc_lte, none)
			}

			return s.new_token(.punc_lt, none)
		}
		`/` {
			next := s.peek_char()

			// Handling a comment, we should skip until the end of the line
			// In the future, we should make comments an AST node
			if next == `/` {
				mut end_of_line := s.state.get_pos()

				for {
					if s.input[end_of_line] == `\n` {
						break
					}

					end_of_line++
				}

				s.state.set_pos(end_of_line)

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

			if next == `>` {
				s.incr_pos()
				return s.new_token(.punc_arrow, none)
			}

			return s.new_token(.punc_equals, none)
		}
		else {
			panic('unexpected character \'${ch.ascii_str()}\' at line ${s.state.get_line()} column ${s.state.get_column()}')
		}
	}
}

pub fn (mut s Scanner) scan_all() []compiler.Token {
	mut tokens := []compiler.Token{}

	for {
		t := s.scan_next()
		tokens << t

		if t.kind == .eof {
			break
		}
	}

	return tokens
}

fn (s Scanner) new_token(kind token.Kind, literal ?string) compiler.Token {
	return compiler.Token{
		kind:    kind
		literal: literal
		line:    s.state.get_line()
		column:  s.state.get_column()
	}
}

// scan_identifier scans until the next non-alphanumeric character
fn (mut s Scanner) scan_identifier(from u8) compiler.Token {
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

	return s.new_token(.identifier, result)
}

// Not a big fan of how this is implemented right now, it's
// too greedy and requires backtracking to figure out
// if the dots represent other tokens, or just a dotdot
fn (mut s Scanner) scan_number(from u8) compiler.Token {
	mut result := from.ascii_str()

	mut has_dot := false

	for {
		next := s.peek_char()

		if next == `.` && has_dot {
			result = result[..result.len - 1]
			s.decr_pos()
			break
		}

		if next.is_digit() {
			s.incr_pos()
			result += next.ascii_str()
		} else if next == `.` && !has_dot {
			// Only works if the chars after the dot
			// are also numerical

			has_dot = true
			s.incr_pos()
			result += next.ascii_str()
		} else {
			break
		}
	}

	return s.new_token(.literal_number, result)
}

fn (mut s Scanner) peek_char() u8 {
	if s.state.get_pos() >= s.input.len {
		return 0 // EOF
	}
	return s.input[s.state.get_pos()]
}

pub fn (mut s Scanner) incr_pos() {
	if s.input[s.state.get_pos()] == `\n` {
		s.state.incr_line()
	} else {
		s.state.incr_column()
	}

	s.state.incr_pos()
}

fn (mut s Scanner) decr_pos() {
	if s.input[s.state.get_pos()] == `\n` {
		s.state.decr_line()
	} else {
		s.state.decr_column()
	}

	s.state.decr_pos()
}

pub fn (s Scanner) get_state() &state.ScannerState {
	return s.state
}
