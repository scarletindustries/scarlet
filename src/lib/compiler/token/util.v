module token

@[inline]
pub fn is_name_char(c u8) bool {
	return (c >= `a` && c <= `z`) || (c >= `A` && c <= `Z`) || c == `_` || c.is_digit()
}

// is_valid_identifier checks if the given identifier is a valid identifier. It accepts
// a parameter "is_fully_qualified" which indicates if the identifier is fully qualified.
// If it is fully qualified, it will not allow keywords to be used as identifiers.
@[inline]
pub fn is_valid_identifier(identifier string, is_fully_qualified bool) bool {
	if identifier.len == 0 {
		return false
	}

	if is_fully_qualified && is_keyword(identifier) {
		return false
	}

	// Check first character is a letter or an underscore
	if !identifier[0].is_letter() && identifier[0] != `_` {
		return false
	}

	// Check the rest of the characters
	for i := 1; i < identifier.len; i++ {
		if !is_name_char(identifier[i]) {
			return false
		}
	}

	return true
}

@[inline]
pub fn is_keyword(identifier string) bool {
	return identifier in keyword_map
}

@[inline]
pub fn match_keyword(identifier ?string) ?Kind {
	if unwrapped := identifier {
		return keyword_map[unwrapped] or { return none }
	}

	return none
}

// is_quote returns true if the given character is a quote character.
// AL supports ' and ` as quote characters. Single quotes for regular strings
// and backticks for character literals.
@[inline]
pub fn is_quote(c char) bool {
	return c == `'` || c == `\``
}
