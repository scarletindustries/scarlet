@vm(int__to_string)
pub fn to_string(n Int) String

pub fn max(a Int, b Int) Int {
	if a > b { a } else { b }
}

pub fn min(a Int, b Int) Int {
	if a < b { a } else { b }
}

pub fn abs(n Int) Int {
	if n < 0 { 0 - n } else { n }
}

pub fn clamp(n Int, lo Int, hi Int) Int {
	if n < lo { lo } else if n > hi { hi } else { n }
}
