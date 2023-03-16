module compiler

[inline; minify]
pub struct Token {
pub:
	kind    Kind   // The token number/enum; for quick comparisons
	literal string // Literal representation of the token
	line    int    // The line number in the source where the token occured
}

pub enum Kind {
	// End of file
	eof
	// Literals
	literal_ident // Any identifier that is not a keyword
	literal_number // Any number
	literal_string // Any string
	literal_string_interpolation // Any string interpolation (e.g. "Hello, $name or ${name}")
	// Math
	math_plus
	math_minus
	math_mul
	math_div
	math_mod
	math_incr
	math_decr
	// Assignment
	reassign // =
	declare // :=
	// Logical
	logical_and // &&
	logical_or // ||
	logical_not // !
	// Bitwise
	bitwise_and // &
	bitwise_or // |
	bitwise_xor // ^
	bitwise_not // ~
	// Comparison
	comp_equal // ==
	comp_not_equal // !=
	comp_greater_than // >
	comp_less_than // <
	comp_greater_than_or_equal // >=
	comp_less_than_or_equal // <=
	// Control flow
	ctrl_if // if
	ctrl_else // else
	ctrl_loop // loop
	// Keywords
	kw_function // fn
	kw_return // return
	kw_break // break
	kw_continue // continue
	kw_import // import
	kw_from // from
	kw_true // true
	kw_false // false
	kw_null // null
	kw_assert // assert
	kw_export // export
	kw_struct // struct
	kw_try // try
	// Punctuation
	punc_comma // ,
	punc_colon // :
	punc_semicolon // ;
	punc_dot // .
	punc_dotdot // ..
	punc_ellipsis // ...
	punc_open_paren // (
	punc_close_paren // )
	punc_open_brace // {
	punc_close_brace // }
	punc_open_bracket // [
	punc_close_bracket // ]
	punc_question_mark // ?
	punc_at // @
	// Misc
	_end_ // Used to mark the end of the token list, used to pull a length
}

pub const total_known_tokens = int(Kind._end_)

// AtKind is used to inject information into the token stream
// when the @ token is encountered. This is used to get information
// about the current file, function, etc, at compile time.
pub enum AtKind {
	fn_name // @fn – Gets the name of the current function
	method_name // @method – Gets the name of the current method
	file_path // @path – Gets the path of the current file
	line // @line – Gets the line number of the current line where the token appears
}

pub fn kind_to_string(kind Kind) string {
	return match kind {
		.eof { 'eof' }
		.literal_ident { 'literal_ident' }
		.literal_number { 'literal_number' }
		.literal_string { 'literal_string' }
		.literal_string_interpolation { 'literal_string_interpolation' }
		.math_plus { 'math_plus' }
		.math_minus { 'math_minus' }
		.math_mul { 'math_mul' }
		.math_div { 'math_div' }
		.math_mod { 'math_mod' }
		.math_incr { 'math_incr' }
		.math_decr { 'math_decr' }
		.reassign { 'reassign' }
		.declare { 'declare' }
		.logical_and { 'logical_and' }
		.logical_or { 'logical_or' }
		.logical_not { 'logical_not' }
		.bitwise_and { 'bitwise_and' }
		.bitwise_or { 'bitwise_or' }
		.bitwise_xor { 'bitwise_xor' }
		.bitwise_not { 'bitwise_not' }
		.comp_equal { 'comp_equal' }
		.comp_not_equal { 'comp_not_equal' }
		.comp_greater_than { 'comp_greater_than' }
		.comp_less_than { 'comp_less_than' }
		.comp_greater_than_or_equal { 'comp_greater_than_or_equal' }
		.comp_less_than_or_equal { 'comp_less_than_or_equal' }
		.ctrl_if { 'ctrl_if' }
		.ctrl_else { 'ctrl_else' }
		.ctrl_loop { 'ctrl_loop' }
		.kw_function { 'kw_function' }
		.kw_return { 'kw_return' }
		.kw_break { 'kw_break' }
		.kw_continue { 'kw_continue' }
		.kw_import { 'kw_import' }
		.kw_from { 'kw_from' }
		.kw_true { 'kw_true' }
		.kw_false { 'kw_false' }
		.kw_null { 'kw_null' }
		.kw_assert { 'kw_assert' }
		.kw_export { 'kw_export' }
		.punc_comma { 'punc_comma' }
		.punc_colon { 'punc_colon' }
		.punc_dot { 'punc_dot' }
		.punc_dotdot { 'punc_dotdot' }
		.punc_ellipsis { 'punc_ellipsis' }
		.punc_open_paren { 'punc_open_paren' }
		.punc_close_paren { 'punc_close_paren' }
		.punc_open_brace { 'punc_open_brace' }
		.punc_close_brace { 'punc_close_brace' }
		.punc_open_bracket { 'punc_open_bracket' }
		.punc_close_bracket { 'punc_close_bracket' }
		.punc_at { 'punc_at' }
		else { panic('Invalid token kind: ${kind}') }
	}
}
