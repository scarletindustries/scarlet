@vm(float__floor)
pub fn floor(f Float) Int

@vm(float__ceil)
pub fn ceil(f Float) Int

@vm(float__round)
pub fn round(f Float) Int

@vm(float__truncate)
pub fn truncate(f Float) Int

@vm(float__from_int)
pub fn from_int(n Int) Float

@vm(float__to_string)
pub fn to_string(f Float) String

pub fn max(a Float, b Float) Float {
	if a > b { a } else { b }
}

pub fn min(a Float, b Float) Float {
	if a < b { a } else { b }
}

// `<=` so abs(-0.0) is 0.0, matching IEEE abs for both zeros.
pub fn abs(f Float) Float {
	if f <= 0.0 {
		0.0 - f
	} else {
		f
	}
}
