// Temperature conversion utilities

fn celsius_to_fahrenheit(c Int) Int { c * 9 / 5 + 32 }

fn fahrenheit_to_celsius(f Int) Int { f - 32 * 5 / 9 }

fn celsius_to_kelvin(c Int) Int { c + 273 }

fn kelvin_to_celsius(k Int) Int { k - 273 }

struct Temperature {
	celsius Int,
	fahrenheit Int,
	kelvin Int,
}

fn from_celsius(c Int) Temperature {
	Temperature{
		celsius: c,
		fahrenheit: celsius_to_fahrenheit(c),
		kelvin: celsius_to_kelvin(c),
	}
}

// Common temperature reference points
[from_celsius(0), from_celsius(20), from_celsius(37), from_celsius(100)]
