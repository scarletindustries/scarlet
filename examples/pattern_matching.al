// Pattern matching, on a stream of UI input events.
//
// A match must be exhaustive: leaving out a variant is a compile error
// ("Match is not exhaustive, missing: Quit"), and so is an arm that an earlier
// arm already covers ("This pattern is unreachable; a previous pattern matches
// the same values"). That is why `describe` below has no `else`: the four
// variants of Event are the whole space, and the compiler knows it.

import al/array
import al/string

type Point {
	x Int
	y Int
}

type Event {
	Click(at Point, button Int)
	Key(code Int, shift Bool)
	Scroll(dy Int)
	Quit
}

fn describe(e Event) String {
	match e {
		// A nested constructor pattern reaches into the payload's payload.
		Click(Point(0, 0), _) -> 'click at the origin'
		// A guard refines a pattern with a test the pattern cannot express.
		Click(Point(x, y), _) if x == y -> 'click on the diagonal at ${x}'
		// Fields can be matched by label, and `..` drops the ones not named.
		Click(at: p, ..) -> 'click at ${p.x},${p.y}'
		Key(27, _) -> 'escape'
		Key(code, True) -> 'shift + key ${code}'
		Key(code, False) -> 'key ${code}'
		// A payload can be a literal, negative ones included.
		Scroll(1) -> 'scroll nudge down'
		Scroll(-1) -> 'scroll nudge up'
		Scroll(dy) -> 'scroll ${dy}'
		Quit -> 'quit'
	}
}

// Range patterns are end-exclusive, exactly like range expressions: 400..500
// matches 400 through 499. `else` is the wildcard arm.
fn status_class(code Int) String {
	match code {
		// An or-pattern lets several literals share one body.
		200 | 201 | 204 -> 'success'
		300..400 -> 'redirect'
		400..500 -> 'client error'
		500..600 -> 'server error'
		else -> 'unclassified'
	}
}

// Literal patterns work on strings as well as numbers.
fn effect(method String) String {
	match method {
		'GET' | 'HEAD' -> 'read'
		'POST' | 'PUT' | 'PATCH' -> 'write'
		'DELETE' -> 'remove'
		else -> 'unknown'
	}
}

// Matching a tuple destructures both scrutinees in one pattern, which keeps
// the four cases side by side instead of nested in an if/else.
fn quadrant(p Point) String {
	match (p.x >= 0, p.y >= 0) {
		(True, True) -> 'top right'
		(True, False) -> 'bottom right'
		(False, True) -> 'top left'
		(False, False) -> 'bottom left'
	}
}

// Array patterns match by shape; `..rest` binds whatever is left over.
fn batch(codes Array(Int)) String {
	match codes {
		[] -> 'empty batch'
		[only] -> 'single ${only}'
		[first, second] -> 'pair ${first}, ${second}'
		[first, ..rest] -> 'first ${first} + ${string.inspect(rest)}'
	}
}

events = [
	Click(at: Point(x: 0, y: 0), button: 0),
	Click(at: Point(x: 3, y: 3), button: 0),
	Click(at: Point(x: 12, y: 40), button: 1),
	Key(code: 27, shift: False),
	Key(code: 65, shift: True),
	Scroll(dy: 1),
	Scroll(dy: -30),
	Quit,
]
array.each(events, fn(e) println(describe(e)))

array.each([200, 302, 404, 503, 100], fn(c) println('${c}: ${status_class(c)}'))
array.each(['GET', 'PATCH', 'DELETE', 'TRACE'], fn(m) println('${m} -> ${effect(m)}'))

println(quadrant(Point(x: 4, y: -2)))
println(batch([]))
println(batch([204]))
println(batch([200, 404]))
println(batch([1, 2, 3, 4]))
