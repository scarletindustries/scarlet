// The largest Int <= `f`, saturating at the Int bounds. Total: Float values
// are canonicalized finite, so there is no NaN/infinity case.
@vm(float__floor)
pub fn floor(f Float) Int

// The smallest Int >= `f`, saturating at the Int bounds.
@vm(float__ceil)
pub fn ceil(f Float) Int

// The nearest Int to `f` (ties round away from zero), saturating at the Int
// bounds.
@vm(float__round)
pub fn round(f Float) Int

// `f` with its fractional part discarded (rounds toward zero), saturating at
// the Int bounds.
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
