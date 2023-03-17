module token

[inline]
pub fn is_name_char(c byte) bool {
	return (c >= `a` && c <= `z`) || (c >= `A` && c <= `Z`) || c == `_` || c.is_digit()
}

// is_valid_identifier checks if the given identifier is a valid identifier. It accepts
// a parameter "is_fully_qualified" which indicates if the identifier is fully qualified.
// If it is fully qualified, it will not allow keywords to be used as identifiers.
[inline]
pub fn is_valid_identifier(ident string, is_fully_qualified bool) bool {
	if ident.len == 0 {
		return false
	}

	if is_fully_qualified && is_keyword(ident) {
		return false
	}

	// Check first character is
	if !ident[0].is_letter() && ident[0] != `_` {
		return false
	}

	// Check the rest of the characters
	for i := 1; i < ident.len; i++ {
		if !is_name_char(ident[i]) {
			return false
		}
	}

	return true
}

[inline]
pub fn is_keyword(ident string) bool {
	return ident in keyword_map
}

[inline]
pub fn match_keyword(ident ?string) ?Kind {
	if unwrapped := ident {
		return keyword_map[unwrapped] or { return none }
	}

	return none
}

// is_quote returns true if the given character is a quote character.
// AL supports ' and ` as quote characters. Single quotes for regular strings
// and backticks for character literals.
[inline]
pub fn is_quote(c char) bool {
	return c == `'` || c == `\``
}
