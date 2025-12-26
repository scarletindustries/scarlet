module token

pub enum Kind {
	// End of file
	eof
	// Error token - used when scanner encounters invalid input
	error
	identifier // Any identifier that is not a keyword
	// Literals
	literal_number               // Any number
	literal_string               // Any string ('Hello, world')
	literal_string_interpolation // Any string interpolation (e.g. 'Hello, $name or ${name}')
	literal_char                 // Any character (`a`)
	// Logical
	logical_and // &&
	logical_or  // ||
	// Bitwise
	bitwise_and // &
	bitwise_or  // |
	bitwise_xor // ^
	bitwise_not // ~
	// Keywords
	kw_comptime // comptime
	kw_const    // const
	kw_enum     // enum
	kw_error    // error
	kw_if       // if
	kw_else     // else
	kw_function // fn
	kw_import   // import
	kw_from     // from
	kw_true     // true
	kw_false    // false
	kw_export   // export
	kw_struct   // struct
	kw_in       // in
	kw_match    // match
	kw_none     // none
	kw_or       // or
	// Punctuation
	punc_arrow             // ->
	punc_comma             // ,
	punc_colon             // :
	punc_semicolon         // ;
	punc_dot               // .
	punc_dotdot            // ..
	punc_ellipsis          // ...
	punc_open_paren        // (
	punc_close_paren       // )
	punc_open_brace        // {
	punc_close_brace       // }
	punc_open_bracket      // [
	punc_close_bracket     // ]
	punc_question_mark     // ?
	punc_exclamation_mark  // !
	punc_at                // @
	punc_equals            // =
	punc_equals_comparator // ==
	punc_not_equal         // !=
	punc_gt                // >
	punc_lt                // <
	punc_gte               // >=
	punc_lte               // <=
	punc_plus              // +
	punc_plusplus          // ++
	punc_minus             // -
	punc_minusminus        // --
	punc_mul               // *
	punc_div               // /
	punc_mod               // %
	// Misc
	_end_ // Used to mark the end of the token list, used to pull a length
}

pub fn (kind Kind) str() string {
	return match kind {
		.eof { 'EOF' }
		.error { '<error>' }
		.identifier { 'identifier' }
		.literal_number { 'number' }
		.literal_string { 'string' }
		.literal_string_interpolation { 'interpolated string' }
		.literal_char { 'char' }
		.logical_and { '&&' }
		.logical_or { '||' }
		.bitwise_and { '&' }
		.bitwise_or { '|' }
		.bitwise_xor { '^' }
		.bitwise_not { '~' }
		.kw_comptime { 'comptime' }
		.kw_const { 'const' }
		.kw_enum { 'enum' }
		.kw_error { 'error' }
		.kw_if { 'if' }
		.kw_else { 'else' }
		.kw_function { 'fn' }
		.kw_import { 'import' }
		.kw_from { 'from' }
		.kw_true { 'true' }
		.kw_false { 'false' }
		.kw_export { 'export' }
		.kw_struct { 'struct' }
		.kw_in { 'in' }
		.kw_match { 'match' }
		.kw_none { 'none' }
		.kw_or { 'or' }
		.punc_arrow { '->' }
		.punc_comma { ',' }
		.punc_colon { ':' }
		.punc_semicolon { ';' }
		.punc_dot { '.' }
		.punc_dotdot { '..' }
		.punc_ellipsis { '...' }
		.punc_open_paren { '(' }
		.punc_close_paren { ')' }
		.punc_open_brace { '{' }
		.punc_close_brace { '}' }
		.punc_open_bracket { '[' }
		.punc_close_bracket { ']' }
		.punc_question_mark { '?' }
		.punc_exclamation_mark { '!' }
		.punc_at { '@' }
		.punc_equals { '=' }
		.punc_equals_comparator { '==' }
		.punc_not_equal { '!=' }
		.punc_gt { '>' }
		.punc_lt { '<' }
		.punc_gte { '>=' }
		.punc_lte { '<=' }
		.punc_plus { '+' }
		.punc_plusplus { '++' }
		.punc_minus { '-' }
		.punc_minusminus { '--' }
		.punc_mul { '*' }
		.punc_div { '/' }
		.punc_mod { '%' }
		._end_ { '<end>' }
	}
}
