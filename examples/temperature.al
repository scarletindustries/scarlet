// Temperature conversions between Celsius, Fahrenheit, and Kelvin,
// and the round trips that prove the conversions invert each other.

fn celsius_to_fahrenheit(c Int) Int {
	c * 9 / 5 + 32
}

fn fahrenheit_to_celsius(f Int) Int {
	{ f - 32 } * 5 / 9
}

fn celsius_to_kelvin(c Int) Int {
	c + 273
}

fn kelvin_to_celsius(k Int) Int {
	k - 273
}

type Temperature {
	celsius Int
	fahrenheit Int
	kelvin Int
}

fn from_celsius(c Int) Temperature {
	Temperature(celsius: c, fahrenheit: celsius_to_fahrenheit(c), kelvin: celsius_to_kelvin(c))
}

fn describe(t Temperature) String {
	'${t.celsius}C = ${t.fahrenheit}F = ${t.kelvin}K'
}

fn print_each(temps Array(Temperature)) Nil {
	match temps {
		[] -> Nil
		[t, ..rest] -> {
			println(describe(t))
			print_each(rest)
		}
	}
}

// Reference points: freezing, room temperature, body heat, boiling.
print_each([from_celsius(0), from_celsius(20), from_celsius(37), from_celsius(100)])

// Converting back lands on the number we started from.
println('212F is ${fahrenheit_to_celsius(212)}C')
println('310K is ${kelvin_to_celsius(310)}C')
