// Tuples and records: indexing, destructuring, nested shapes, record update,
// and the ways both flow through functions, arrays, and map keys.

import al/map

fn check(name String, expected Bool, actual Bool) Nil {
	if expected == actual {
		println('PASS ${name}')
	} else {
		println('FAIL ${name}: expected ${expected}, got ${actual}')
	}
}

// --- tuple access ---------------------------------------------------------------

pair = (42, 'answer')
trio = (True, 100, 'world')
nested = ((1, 2), (3, (4, 5)))

check('tuple index 0', True, pair.0 == 42)
check('tuple index 1', True, pair.1 == 'answer')
check('tuple index 2', True, trio.2 == 'world')
check('tuple index 0 is Bool', True, trio.0)
check('nested tuple index', True, nested.1.1.0 == 4)
check('whole nested element', True, nested.0 == (1, 2))
check('tuple eq is structural', True, pair == (42, 'answer'))
check('tuple ne on one slot', True, pair != (42, 'other'))

// --- tuple destructuring ---------------------------------------------------------

(a, b) = pair
check('tuple destructuring', True, a == 42 && b == 'answer')

// Rebinding the same names at different types: a rebinding is a new binding.
(a, b) = ('flipped', 'again')
check('rebound tuple retypes', True, a == 'flipped' && b == 'again')

(_, count, label) = trio
check('destructuring with wildcards', True, count == 100 && label == 'world')

((x1, y1), (_, inner)) = nested
check('nested tuple destructuring', True, x1 == 1 && y1 == 2 && inner == (4, 5))

// --- tuples through functions, arrays, and map keys ------------------------------

fn min_max(xs Array(Int)) (Int, Int) {
	match xs {
		[] -> (0, 0)
		[h, ..t] -> {
			(lo, hi) = min_max_of(t, h, h)
			(lo, hi)
		}
	}
}

fn min_max_of(xs Array(Int), lo Int, hi Int) (Int, Int) {
	match xs {
		[] -> (lo, hi)
		[h, ..t] -> {
			next_lo = if h < lo { h } else { lo }
			next_hi = if h > hi { h } else { hi }
			min_max_of(t, next_lo, next_hi)
		}
	}
}
check('tuple returned from fn', True, min_max([3, 1, 4, 1, 5]) == (1, 5))

rows = [(1, 'one'), (2, 'two')]
check('tuple inside array', True, rows[1] == Some((2, 'two')))

grid = map.from_list([((0, 0), 'origin'), ((1, 2), 'here')])
check('tuple as map key', True, map.get(grid, (1, 2)) == Some('here'))
check('tuple key miss', True, map.get(grid, (9, 9)) == None)

// --- records ----------------------------------------------------------------------

type Point {
	x Int
	y Int
}

type Segment {
	from Point
	to Point
	label String
}

p = Point(x: 1, y: 2)
check('record field access', True, p.x == 1 && p.y == 2)
check('record eq is structural', True, p == Point(x: 1, y: 2))
check('record ne on one field', True, p != Point(x: 1, y: 3))

// Spread in a call position is record update: copy, then override.
moved = Point(..p, y: 20)
check('record update overrides', True, moved.y == 20)
check('record update copies the rest', True, moved.x == 1)
check('record update leaves the original alone', True, p.y == 2)

seg = Segment(from: p, to: moved, label: 'up')
check('nested record field access', True, seg.to.y == 20)
check('nested record eq', True, seg == Segment(from: p, to: moved, label: 'up'))

relabelled = Segment(..seg, label: 'down')
check('nested record update', True, relabelled.label == 'down' && relabelled.from.x == 1)

// A single-constructor type destructures without a match.
Point(px, py) = moved
check('record destructuring binding', True, px == 1 && py == 20)

// Records and tuples nest into each other freely.
tagged = (p, [moved, p])
check('record inside tuple', True, tagged.0.x == 1)
check('array of records in tuple', True, tagged.1 == [moved, p])

fn shift(s Segment, dx Int) Segment {
	Segment(..s, from: Point(..s.from, x: s.from.x + dx), to: Point(..s.to, x: s.to.x + dx))
}
shifted = shift(seg, 5)
check('record update inside a fn', True, shifted.from.x == 6 && shifted.to.x == 6)
check('record update keeps other fields', True, shifted.label == 'up' && shifted.to.y == 20)
