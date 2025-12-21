module formatter

import compiler
import compiler.token
import compiler.scanner
import strings

pub fn format(input string) string {
	return format_with_debug(input, false)
}

pub fn format_with_debug(input string, debug bool) string {
	mut s := scanner.new_scanner(input)
	tokens := s.scan_all()

	if debug {
		for tok in tokens {
			eprintln('Token: ${tok.kind} "${tok.literal or { '' }}" trivia: ${tok.leading_trivia.len}')
			for t in tok.leading_trivia {
				eprintln('  Trivia: ${t.kind} "${t.text.replace('\n', '\\n')}"')
			}
		}
	}

	mut f := Formatter{
		tokens: tokens
		pos:    0
		output: strings.new_builder(input.len)
		indent: 0
	}
	return f.format()
}

struct Formatter {
	tokens []compiler.Token
mut:
	pos           int
	output        strings.Builder
	indent        int
	at_line_start bool       = true
	last_kind     token.Kind = .eof
}

fn (mut f Formatter) format() string {
	for f.pos < f.tokens.len {
		f.process_token()
		f.pos++
	}

	result := f.output.str()

	// Clean up trailing whitespace on lines and ensure final newline
	mut lines := result.split('\n')
	for i, line in lines {
		lines[i] = line.trim_right(' \t')
	}
	mut final := lines.join('\n')
	if final.len > 0 && final[final.len - 1] != `\n` {
		final += '\n'
	}
	return final
}

fn (mut f Formatter) emit(s string) {
	f.output.write_string(s)
	if s.len > 0 {
		f.at_line_start = s[s.len - 1] == `\n`
	}
}

fn (mut f Formatter) emit_indent() {
	for _ in 0 .. f.indent {
		f.output.write_u8(`\t`)
	}
	f.at_line_start = false
}

fn (mut f Formatter) emit_trivia(trivia []token.Trivia) {
	for t in trivia {
		match t.kind {
			.newline {
				f.emit('\n')
			}
			.line_comment {
				if !f.at_line_start {
					f.emit(' ')
				} else {
					f.emit_indent()
				}
				f.emit(t.text)
			}
			.whitespace {
				// Skip whitespace trivia, we control spacing
			}
		}
	}
}

fn (mut f Formatter) needs_space_before(kind token.Kind) bool {
	// No space after opening brackets or at line start
	if f.at_line_start {
		return false
	}

	// Check what came before
	match f.last_kind {
		.punc_open_paren, .punc_open_bracket, .punc_open_brace {
			return false
		}
		.punc_dot {
			return false
		}
		.punc_question_mark, .punc_exclamation_mark {
			// Space after ? or ! before identifiers/keywords
			return is_word_like(kind)
		}
		else {}
	}

	// Check what's coming
	match kind {
		.punc_comma, .punc_close_paren, .punc_close_bracket, .punc_close_brace, .punc_dot,
		.punc_question_mark, .punc_colon {
			return false
		}
		else {}
	}

	// Words after words need space
	if is_word_like(f.last_kind) && is_word_like(kind) {
		return true
	}

	// Words after close parens need space (e.g., `) Int`)
	if f.last_kind == .punc_close_paren && is_word_like(kind) {
		return true
	}

	return false
}

fn (mut f Formatter) process_token() {
	tok := f.tokens[f.pos]

	// Handle trivia first - but skip leading newlines right after open brace
	if f.last_kind != .punc_open_brace {
		f.emit_trivia(tok.leading_trivia)
	} else {
		// After open brace, skip leading whitespace/newlines but keep comments
		for t in tok.leading_trivia {
			if t.kind == .line_comment {
				if !f.at_line_start {
					f.emit(' ')
				} else {
					f.emit_indent()
				}
				f.emit(t.text)
				f.emit('\n')
			}
		}
	}

	// Handle the token itself
	match tok.kind {
		.eof {
			// Nothing to emit
		}
		.punc_open_brace {
			f.emit(' {\n')
			f.indent++
		}
		.punc_close_brace {
			f.indent--
			// Ensure newline before closing brace if we're not at line start
			if !f.at_line_start {
				f.emit('\n')
			}
			f.emit_indent()
			f.emit('}')
		}
		.punc_open_paren {
			f.emit('(')
		}
		.punc_close_paren {
			f.emit(')')
		}
		.punc_open_bracket {
			f.emit('[')
		}
		.punc_close_bracket {
			f.emit(']')
		}
		.punc_comma {
			f.emit(', ')
		}
		.punc_dot {
			f.emit('.')
		}
		.punc_dotdot {
			f.emit('..')
		}
		.punc_colon {
			f.emit(': ')
		}
		.punc_semicolon {
			f.emit(';')
		}
		.punc_question_mark {
			f.emit('?')
		}
		.punc_exclamation_mark {
			f.emit('!')
		}
		.punc_arrow {
			f.emit(' -> ')
		}
		.punc_equals {
			f.emit(' = ')
		}
		.punc_declaration {
			f.emit(' := ')
		}
		.punc_equals_comparator {
			f.emit(' == ')
		}
		.punc_not_equal {
			f.emit(' != ')
		}
		.punc_gt {
			f.emit(' > ')
		}
		.punc_lt {
			f.emit(' < ')
		}
		.punc_gte {
			f.emit(' >= ')
		}
		.punc_lte {
			f.emit(' <= ')
		}
		.punc_plus {
			f.emit(' + ')
		}
		.punc_minus {
			f.emit(' - ')
		}
		.punc_mul {
			f.emit(' * ')
		}
		.punc_div {
			f.emit(' / ')
		}
		.punc_mod {
			f.emit(' % ')
		}
		.punc_plusplus {
			f.emit('++')
		}
		.punc_minusminus {
			f.emit('--')
		}
		.logical_and {
			f.emit(' && ')
		}
		.logical_or {
			f.emit(' || ')
		}
		.literal_string {
			if f.at_line_start {
				f.emit_indent()
			} else if f.needs_space_before(.literal_string) {
				f.emit(' ')
			}
			f.emit("'${tok.literal or { '' }}'")
		}
		.literal_string_interpolation {
			if f.at_line_start {
				f.emit_indent()
			} else if f.needs_space_before(.literal_string_interpolation) {
				f.emit(' ')
			}
			f.emit("'${tok.literal or { '' }}'")
		}
		.literal_char {
			if f.at_line_start {
				f.emit_indent()
			} else if f.needs_space_before(.literal_char) {
				f.emit(' ')
			}
			f.emit('`${tok.literal or { '' }}`')
		}
		.literal_number {
			if f.at_line_start {
				f.emit_indent()
			} else if f.needs_space_before(.literal_number) {
				f.emit(' ')
			}
			f.emit(tok.literal or { '0' })
		}
		else {
			// Keywords and identifiers
			if f.at_line_start {
				f.emit_indent()
			} else if f.needs_space_before(tok.kind) {
				f.emit(' ')
			}
			if lit := tok.literal {
				f.emit(lit)
			} else {
				f.emit(tok.kind.str())
			}
		}
	}

	f.last_kind = tok.kind
}

fn is_word_like(k token.Kind) bool {
	return match k {
		.identifier, .literal_number, .literal_string, .literal_string_interpolation,
		.literal_char, .kw_comptime, .kw_const, .kw_enum, .kw_error, .kw_if, .kw_else,
		.kw_function, .kw_import, .kw_from, .kw_true, .kw_false, .kw_assert, .kw_export,
		.kw_struct, .kw_in, .kw_match, .kw_none, .kw_or {
			true
		}
		else {
			false
		}
	}
}
