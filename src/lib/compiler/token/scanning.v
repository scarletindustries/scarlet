module token

[inline]
pub fn is_name_char(c char) bool {
	return (c >= `a` && c <= `z`) || (c >= `A` && c <= `Z`) || c == `_`
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

[inline]
pub fn is_quote(c char) bool {
	return c == `'` || c == `'`
}
