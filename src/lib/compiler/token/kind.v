module token

pub enum Kind {
	// End of file
	eof
	// Literals
	literal_ident // Any identifier that is not a keyword
	literal_number // Any number
	literal_string // Any string
	literal_string_interpolation // Any string interpolation (e.g. "Hello, $name or ${name}")
	// Math
	math_plus // +
	math_minus // -
	math_mul // *
	math_div // /
	math_mod // %
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
	// Keywords
	kw_if // if
	kw_else // else
	kw_loop // loop
	kw_function // fn
	kw_return // return
	kw_break // break
	kw_continue // continue
	kw_import // import
	kw_from // from
	kw_true // true
	kw_false // false
	kw_assert // assert
	kw_export // export
	kw_struct // struct
	kw_in // in
	kw_none // none
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
