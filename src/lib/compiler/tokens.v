module tokens

pub struct Token {
pub:
	kind Kind   // The token number/enum; for quick comparisons
	lit  string // Literal representation of the token
	line int    // The line number in the source where the token occured
	col  int    // The column in the source where the token occured	
	len  int    // Length of the literal
}

pub enum Kind {
	eof
	// Literals
	literal_symbol // Any symbol that is not a keyword
	literal_number // Any number
	literal_string // Any string
	literal_string_interpolation // Any string interpolation (e.g. "Hello, $name or ${name}")
	// Math
	math_plus // +
	math_minus // -
	math_mul // *
	math_div // /
	math_mod // %
	math_incr // ++
	math_decr // --
	// Assignment
	reassign // =
	declare // :=
	// Logical
	logical_and // &&
	logical_or // ||
	logical_not // !
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
	// Punctuation
	punc_comma // ,
	punc_colon // :
	punc_dot // .
	punc_dotdot // ..
	punc_ellipsis // ...
	punc_open_paren // (
	punc_close_paren // )
	punc_open_brace // {
	punc_close_brace // }
	punc_open_bracket // [
	punc_close_bracket // ]
}
