/**
 * Temperature units and the conversions between them.
 *
 * A doc comment on the first line of a file documents the *module*, not the
 * declaration that follows it. An imported module may only contain
 * declarations — no top-level statements — so nothing here runs on import.
 */

import al/float

/** A temperature in degrees Celsius. */
pub type Celsius {
	degrees Float
}

/** A temperature in degrees Fahrenheit. */
pub type Fahrenheit {
	degrees Float
}

/**
 * An absolute temperature. `opaque` exports the type but not its constructor,
 * so outside this file a Kelvin can only come from `to_kelvin` — and that
 * function refuses to build one below absolute zero.
 */
pub opaque type Kelvin {
	degrees Float
}

/** The bottom of the scale, in Celsius. A `pub const` is exported like a fn. */
pub const ABSOLUTE_ZERO_C Float = -273.15

pub fn celsius(degrees Float) Celsius {
	Celsius(degrees: degrees)
}

pub fn to_fahrenheit(t Celsius) Fahrenheit {
	Fahrenheit(degrees: t.degrees * 9.0 / 5.0 + 32.0)
}

pub fn to_kelvin(t Celsius) Result(Kelvin, String) {
	if t.degrees < ABSOLUTE_ZERO_C {
		Err('${show(t)} is below absolute zero')
	} else {
		Ok(Kelvin(degrees: t.degrees - ABSOLUTE_ZERO_C))
	}
}

pub fn show(t Celsius) String {
	'${round1(t.degrees)}C'
}

pub fn show_fahrenheit(t Fahrenheit) String {
	'${round1(t.degrees)}F'
}

pub fn show_kelvin(t Kelvin) String {
	'${round1(t.degrees)}K'
}

// No `pub`: a helper without it is private to this module, and importers
// cannot name it at all.
fn round1(x Float) Float {
	float.from_int(float.round(x * 10.0)) / 10.0
}
