/**
 * Column formatting for a table of temperature readings.
 *
 * The import below steps *up* out of this directory: a relative path is
 * resolved against the importing file's own directory, so `../units` is
 * examples/lib/units.al — the same module `examples/modules.al` reaches as
 * `./lib/units`, named from where each file sits.
 */

import al/array
import al/string
import ../units.{Celsius, show}

const COLUMN = 12

/** A row: the reading in the first column, then what it was converted into. */
pub fn row(t Celsius, cells Array(String)) String {
	match cells {
		[] -> show(t)
		[first, ..rest] ->
			array.fold(rest, pad(show(t)) + first, fn(line, cell) line + ' = ' + cell)
	}
}

// Pad to COLUMN scalars so the second column lines up whatever precedes it.
fn pad(cell String) String {
	if string.length(cell) >= COLUMN {
		cell
	} else {
		pad(cell + ' ')
	}
}
