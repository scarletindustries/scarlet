module compiler

import lib.compiler.token

[inline; minify]
pub struct Token {
pub:
	kind    token.Kind // The token number/enum; for quick comparisons
	literal ?string    // Literal representation of the token
	line    int        // The line number in the source where the token occurred
	column  int        // The column number in the source where the token occurred
}

fn (t &Token) str() string {
	if literal := t.literal {
		if t.kind == .literal_string {
			return '\'${literal}\''
		}
	}

	return t.literal or {
		return match t.kind {
			.kw_from { 'from' }
			.kw_import { 'import' }
			.kw_function { 'fn' }
			.kw_return { 'return' }
			.punc_comma { ',' }
			.punc_open_paren { '(' }
			.punc_close_paren { ')' }
			.punc_open_brace { '{' }
			.punc_close_brace { '}' }
			.punc_open_bracket { '[' }
			.punc_close_bracket { ']' }
			.punc_colon { ':' }
			.punc_semicolon { ';' }
			.punc_declaration { ':=' }
			.punc_dot { '.' }
			.punc_gt { '>' }
			.punc_lt { '<' }
			.punc_gte { '>=' }
			.punc_lte { '<=' }
			.eof { 'EOF' }
			.logical_and { '&&' }
			.logical_or { '||' }
			.bitwise_and { '&' }
			.bitwise_or { '|' }
			.bitwise_xor { '^' }
			.bitwise_not { '~' }
			.kw_const { 'const' }
			.kw_if { 'if' }
			.kw_else { 'else' }
			.kw_for { 'for' }
			.kw_break { 'break' }
			.kw_continue { 'continue' }
			.kw_true { 'true' }
			.kw_false { 'false' }
			.kw_assert { 'assert' }
			.kw_export { 'export' }
			.kw_struct { 'struct' }
			.kw_in { 'in' }
			.kw_none { 'none' }
			.kw_or { 'or' }
			.punc_dotdot { '..' }
			.punc_ellipsis { '...' }
			.punc_question_mark { '?' }
			.punc_exclamation_mark { '!' }
			.punc_at { '@' }
			.punc_equals { '=' }
			.punc_equals_comparator { '==' }
			.punc_not_equal { '!=' }
			.punc_plus { '+' }
			.punc_minus { '-' }
			.punc_mul { '*' }
			.punc_div { '/' }
			.punc_mod { '%' }
			._end_ { 'end' }
			else { panic('unimplemented Token.str() call for kind ${t.kind.str()}') }
		}
	}
}

pub const total_known_tokens = int(token.Kind._end_)

// AtKind is used to inject information into the token stream
// when the @ token is encountered. This is used to get information
// about the current file, function, etc, at compile time.
pub enum AtKind {
	fn_name // @fn – Gets the name of the current function
	method_name // @method – Gets the name of the current method
	file_path // @path – Gets the path of the current file
	line // @line – Gets the line number of the current line where the token appears
}
