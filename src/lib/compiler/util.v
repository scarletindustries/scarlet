module compiler

[inline]
pub fn is_valid_identifier_char(c u8) bool {
	return (c >= `a` && c <= `z`) || (c >= `A` && c <= `Z`) || c == `_`
}
