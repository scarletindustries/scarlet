// Exact decimal arithmetic on a scaled integer, for money-like values.
//
// A `Decimal` is `units * 10^(-scale)`: `new(1999, 2)` is 19.99. The scale is
// carried through arithmetic automatically — addition aligns scales,
// multiplication adds them. The only operations that discard digits are `div`
// and `round`, which take an explicit target scale and rounding mode.
//
// The type is opaque: values can only be built through the functions here, so
// downstream code can wrap it in its own opaque type for nominal safety:
//
//   pub opaque type Usd { Usd(amount Decimal) }
//   pub fn usd(units Int) Usd { Usd(decimal.new(units, 2)) }
//   pub fn add(a Usd, b Usd) Usd {
//       match a { Usd(x) -> match b { Usd(y) -> Usd(decimal.add(x, y)) } }
//   }
//
// `units` follows the language's Int semantics (64-bit, wrapping), so keep
// magnitudes below 9.2 * 10^18 and scales at or below 18. Aligning scales in
// `add`, `sub`, and `compare` multiplies units by 10^(scale difference), so
// that product must also fit in 64 bits or it wraps. `parse` rejects inputs
// that would overflow rather than wrapping, and `from_float` is None on them.

import al/binary.{Dec}
import al/float
import al/int
import al/option
import al/string

pub opaque type Decimal {
	units Int
	scale Int
}

// How to resolve digits discarded by `div` and `round`.
//
// `HalfEven` (banker's rounding) is the default used by `div` and `round`:
// ties go to the even neighbour, so repeated rounding doesn't drift. `HalfUp`
// is schoolbook rounding (ties away from zero). `Down`/`Up` truncate toward /
// away from zero; `Floor`/`Ceiling` toward negative / positive infinity.
pub type Rounding {
	HalfEven
	HalfUp
	Down
	Up
	Floor
	Ceiling
}

// `units * 10^(-scale)`: `new(1999, 2)` is 19.99. A negative scale is
// multiplied out: `new(5, 0 - 3)` is 5000 at scale 0.
pub fn new(units Int, scale Int) Decimal {
	if scale < 0 {
		Decimal(units * pow10(0 - scale), 0)
	} else {
		Decimal(units, scale)
	}
}

pub fn from_int(n Int) Decimal {
	Decimal(n, 0)
}

// The raw scaled integer: `units(new(1999, 2))` is 1999. Together with
// `scale` this is the lossless way to serialize a Decimal.
pub fn units(d Decimal) Int {
	match d {
		Decimal(u, _) -> u
	}
}

pub fn scale(d Decimal) Int {
	match d {
		Decimal(_, s) -> s
	}
}

pub fn add(a Decimal, b Decimal) Decimal {
	s = int.max(scale(a), scale(b))
	Decimal(units(a) * pow10(s - scale(a)) + units(b) * pow10(s - scale(b)), s)
}

pub fn sub(a Decimal, b Decimal) Decimal {
	add(a, neg(b))
}

pub fn neg(d Decimal) Decimal {
	Decimal(0 - units(d), scale(d))
}

pub fn abs(d Decimal) Decimal {
	Decimal(int.abs(units(d)), scale(d))
}

// Exact product; the result scale is the sum of the operand scales.
// 19.99 * 3 is 59.97 at scale 2; 1.5 * 0.25 is 0.375 at scale 3.
pub fn mul(a Decimal, b Decimal) Decimal {
	Decimal(units(a) * units(b), scale(a) + scale(b))
}

// Quotient at the given scale, rounding half-to-even. None when `b` is zero,
// when `places` is below -18 (the result could not be scaled back up, as in
// `from_float`), or when the rescale exponent `places + scale(b) - scale(a)`
// exceeds 18 (10^19 no longer fits in Int). A negative `places` rounds the
// quotient to a multiple of 10^|places| at scale 0:
// `div(from_int(1234), from_int(1), 0 - 2)` is 1200.
pub fn div(a Decimal, b Decimal, places Int) Option(Decimal) {
	div_with(a, b, places, HalfEven)
}

pub fn div_with(a Decimal, b Decimal, places Int, mode Rounding) Option(Decimal) {
	e = places + scale(b) - scale(a)
	if is_zero(b) || places < 0 - 18 || e > 18 {
		None
	} else {
		q = if e >= 0 {
			div_units(units(a) * pow10(e), units(b), mode)
		} else if e >= 0 - 18 {
			div_units(units(a), units(b) * pow10(0 - e), mode)
		} else if units(b) < 0 {
			div_round_huge(0 - units(a), 0 - units(b), 0 - e, mode)
		} else {
			div_round_huge(units(a), units(b), 0 - e, mode)
		}
		Some(at_places(q, places))
	}
}

// Change the scale, rounding half-to-even when digits are dropped and
// zero-padding when the scale grows: `round(new(2345, 3), 2)` is 2.34.
// A negative `places` rounds to a multiple of 10^|places| at scale 0:
// `round(new(1250, 0), 0 - 2)` is 1200. Below `places` of -18 the only
// representable result is 0; a nonzero one (a ±10^|places| step, reachable
// only from magnitudes at or above 5 * 10^18) wraps like any overflow.
pub fn round(d Decimal, places Int) Decimal {
	round_with(d, places, HalfEven)
}

pub fn round_with(d Decimal, places Int, mode Rounding) Decimal {
	k = scale(d) - places
	if k <= 0 {
		Decimal(units(d) * pow10(0 - k), places)
	} else if k <= 18 {
		at_places(div_round(units(d), pow10(k), mode), places)
	} else {
		at_places(div_round_huge(units(d), 1, k, mode), places)
	}
}

// Numeric comparison across scales. `compare(new(15, 1), new(1500, 3))` is
// `Eq` — built-in `==` would say otherwise, because it compares the
// representation (units and scale), not the number.
pub fn compare(a Decimal, b Decimal) Order {
	s = int.max(scale(a), scale(b))
	ua = units(a) * pow10(s - scale(a))
	ub = units(b) * pow10(s - scale(b))
	if ua < ub {
		Lt
	} else if ua > ub {
		Gt
	} else {
		Eq
	}
}

pub fn eq(a Decimal, b Decimal) Bool {
	match compare(a, b) {
		Eq -> True
		else -> False
	}
}

pub fn lt(a Decimal, b Decimal) Bool {
	match compare(a, b) {
		Lt -> True
		else -> False
	}
}

pub fn lte(a Decimal, b Decimal) Bool {
	match compare(a, b) {
		Gt -> False
		else -> True
	}
}

pub fn gt(a Decimal, b Decimal) Bool {
	match compare(a, b) {
		Gt -> True
		else -> False
	}
}

pub fn gte(a Decimal, b Decimal) Bool {
	match compare(a, b) {
		Lt -> False
		else -> True
	}
}

pub fn min(a Decimal, b Decimal) Decimal {
	if lte(a, b) { a } else { b }
}

pub fn max(a Decimal, b Decimal) Decimal {
	if gte(a, b) { a } else { b }
}

pub fn is_zero(d Decimal) Bool {
	units(d) == 0
}

pub fn is_negative(d Decimal) Bool {
	units(d) < 0
}

pub fn is_positive(d Decimal) Bool {
	units(d) > 0
}

// Drop trailing zeros: `normalize(new(1500, 3))` is 1.5 at scale 1. Useful
// before serializing when the stored scale carries no meaning.
pub fn normalize(d Decimal) Decimal {
	if scale(d) > 0 && units(d) % 10 == 0 {
		normalize(Decimal(units(d) / 10, scale(d) - 1))
	} else {
		d
	}
}

// Accepts an optional sign, digits, and an optional fractional part:
// '42', '-19.99', '+0.05'. The scale is the number of fractional digits.
// Rejects anything else — including '.5', '1.', values that would overflow,
// and more than 18 fractional digits.
pub fn parse(s String) Option(Decimal) {
	b = binary.from_string(s)
	first = binary.byte_at(b, 0)
	if first == 45 {
		option.map(parse_unsigned(tail(b)), neg)
	} else if first == 43 {
		parse_unsigned(tail(b))
	} else {
		parse_unsigned(b)
	}
}

pub fn to_string(d Decimal) String {
	if scale(d) == 0 {
		int.to_string(units(d))
	} else {
		sign = if units(d) < 0 { '-' } else { '' }
		mag = int.abs(units(d))
		p = pow10(scale(d))
		'${sign}${mag / p}.${pad_zeros(int.to_string(mag % p), scale(d))}'
	}
}

// Nearest representable f64; exact only while units fits in 53 bits.
pub fn to_float(d Decimal) Float {
	float.from_int(units(d)) / float.from_int(pow10(scale(d)))
}

// `f` rounded to the nearest value at the given scale; None when the
// resulting `units` would leave 64-bit Int range, or when |places| exceeds
// 18 (10^19 no longer fits in `units`). A negative `places` rounds to a
// multiple of 10^|places| at scale 0. Floats are approximate, so prefer
// `parse` or `new` when exactness matters.
pub fn from_float(f Float, places Int) Option(Decimal) {
	if places > 18 || places < 0 - 18 {
		None
	} else {
		k = int.max(0 - places, 0)
		scaled = if places < 0 {
			f / float.from_int(pow10(k))
		} else {
			f * float.from_int(pow10(places))
		}
		// 9.2 * 10^18 sits safely below Int max, so rounding `scaled` (and,
		// for negative `places`, scaling back up by 10^k) cannot wrap.
		if float.abs(scaled) < 9200000000000000000.0 / float.from_int(pow10(k)) {
			Some(at_places(float.round(scaled), places))
		} else {
			None
		}
	}
}

// A rounded coefficient `q` placed at the target `places`: a negative
// `places` multiplies the magnitude back out to scale 0.
fn at_places(q Int, places Int) Decimal {
	if places < 0 {
		Decimal(q * pow10(0 - places), 0)
	} else {
		Decimal(q, places)
	}
}

fn pow10(n Int) Int {
	if n <= 0 {
		1
	} else {
		10 * pow10(n - 1)
	}
}

fn tail(b Binary) Binary {
	binary.slice_bytes(b, 1, binary.byte_size(b) - 1)
}

fn parse_unsigned(b Binary) Option(Decimal) {
	match binary.index_of(b, <<'.'>>, 0) {
		None -> match binary.parse_int(b, Dec) {
			Ok(n) -> Some(Decimal(n, 0))
			Err(Nil) -> None
		}
		Some(i) ->
			parse_parts(
				binary.slice_bytes(b, 0, i),
				binary.slice_bytes(b, i + 1, binary.byte_size(b) - i - 1),
			)
	}
}

fn parse_parts(whole Binary, frac Binary) Option(Decimal) {
	k = binary.byte_size(frac)
	match binary.parse_int(whole, Dec) {
		Ok(w) if k <= 18 -> match binary.parse_int(frac, Dec) {
			// w * 10^k + f overflows Int exactly when w exceeds
			// (Int max - f) / 10^k; wrapping would corrupt the value.
			Ok(f) if w <= { 9223372036854775807 - f } / pow10(k) ->
				Some(Decimal(w * pow10(k) + f, k))
			else -> None
		}
		else -> None
	}
}

fn pad_zeros(s String, width Int) String {
	if string.length(s) >= width {
		s
	} else {
		pad_zeros('0${s}', width)
	}
}

// `q + r/d` rounded to an integer per `mode`. `d` must be positive; `n` may
// be negative — truncating division means `r` carries the sign of `n` and
// "away from zero" is a step of `sign(n)`.
fn div_round(n Int, d Int, mode Rounding) Int {
	q = n / d
	r = int.abs(n % d)
	if r == 0 {
		q
	} else {
		match mode {
			Down -> q
			Up -> step_away(q, n)
			Floor -> if n < 0 {
				q - 1
			} else {
				q
			}
			Ceiling -> if n < 0 {
				q
			} else {
				q + 1
			}
			// Half-tie tests compare `r` against `d - r` rather than `2 * r`
			// against `d`: with 0 <= r < d the subtraction never wraps, while
			// doubling `r` would for divisors above 2^62 — inside the module's
			// documented |units| range.
			HalfUp -> if r >= d - r {
				step_away(q, n)
			} else {
				q
			}
			HalfEven -> if r > d - r {
				step_away(q, n)
			} else if r < d - r {
				q
			} else if q % 2 == 0 {
				q
			} else {
				step_away(q, n)
			}
		}
	}
}

// `n / (d * 10^k)` rounded per `mode`, for positive `d` and `k >= 19` —
// exponents where materializing 10^k would wrap. The divisor exceeds every
// representable |n|, so the truncated quotient is 0 and all of `n` is
// remainder; only the direction of the rounding step is left to decide. The
// half boundary `d * 5 * 10^(k-1)` sits inside Int range only at d == 1,
// k == 19; everywhere else the remainder is below it and the Half modes
// keep the quotient at 0.
fn div_round_huge(n Int, d Int, k Int, mode Rounding) Int {
	if n == 0 {
		0
	} else {
		on_half = d == 1 && k == 19
		match mode {
			Down -> 0
			Up -> step_away(0, n)
			Floor -> if n < 0 {
				0 - 1
			} else {
				0
			}
			Ceiling -> if n < 0 {
				0
			} else {
				1
			}
			HalfUp -> if on_half && int.abs(n) >= 5 * pow10(18) {
				step_away(0, n)
			} else {
				0
			}
			HalfEven -> if on_half && int.abs(n) > 5 * pow10(18) {
				step_away(0, n)
			} else {
				0
			}
		}
	}
}

fn div_units(n Int, d Int, mode Rounding) Int {
	if d < 0 {
		div_round(0 - n, 0 - d, mode)
	} else {
		div_round(n, d, mode)
	}
}

fn step_away(q Int, n Int) Int {
	if n < 0 {
		q - 1
	} else {
		q + 1
	}
}
