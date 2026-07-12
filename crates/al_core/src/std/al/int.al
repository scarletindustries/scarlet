@vm(int__to_string)
pub fn to_string(n Int) String

pub fn max(a Int, b Int) Int {
	if a > b { a } else { b }
}

pub fn min(a Int, b Int) Int {
	if a < b { a } else { b }
}

pub const min_value = 0 - 9223372036854775807 - 1

pub const max_value = 9223372036854775807

// Total: `abs` of Int min (whose negation does not fit in an Int) saturates
// to Int max rather than wrapping back to a negative value.
pub fn abs(n Int) Int {
	if n == min_value {
		max_value
	} else if n < 0 {
		0 - n
	} else {
		n
	}
}

// With inverted bounds (lo > hi), hi deterministically wins; for lo <= hi
// this is the usual clamp.
pub fn clamp(n Int, lo Int, hi Int) Int {
	min(max(n, lo), hi)
}
