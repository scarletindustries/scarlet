// Modules. Every file is a module, and `import` pulls another one in.
//
// A path that starts with `./` or `../` is relative to *this file's* directory,
// never to the directory the program was run from: `./lib/units` is
// examples/lib/units.al, and inside examples/lib/report/table.al that same
// module is `../units`. A path without a leading marker — `al/array` — names a
// stdlib module.

import al/array
import ./lib/report/table
import ./lib/units as units
import ./lib/units.{ABSOLUTE_ZERO_C, Celsius, celsius as c}

// The whole module lives behind `units.`, and the second import additionally
// pulls three names into this file's scope: the type `Celsius`, the exported
// `pub const`, and `celsius` renamed to `c` so the readings below stay readable.
readings = [c(-40.0), c(0.0), c(21.5), c(100.0), c(-300.0)]

fn conversions(t Celsius) Array(String) {
	fahrenheit = units.show_fahrenheit(units.to_fahrenheit(t))
	// `Kelvin` is an opaque type: `units.Kelvin(degrees: 0.0)` would not compile
	// here, because the constructor stays inside the module that defined it.
	// This is the only way to make one, and it can fail.
	kelvin = match units.to_kelvin(t) {
		Ok(k) -> units.show_kelvin(k)
		Err(why) -> 'no Kelvin (${why})'
	}
	[fahrenheit, kelvin]
}

println('absolute zero: ${ABSOLUTE_ZERO_C}C')

// `units.round1` is not exported, so the caller cannot reach past the API.
array.each(readings, fn(t) println(table.row(t, conversions(t))))
