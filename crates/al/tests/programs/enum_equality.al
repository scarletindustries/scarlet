// `==` is structural everywhere: it walks payloads, elements, and fields
// rather than comparing identity. This pins that for every value shape, plus
// the places where structural equality is deliberately NOT the semantic one.

import al/decimal
import al/map

fn check(name String, expected Bool, actual Bool) Nil {
	if expected == actual {
		println('PASS ${name}')
	} else {
		println('FAIL ${name}: expected ${expected}, got ${actual}')
	}
}

// --- primitives ---------------------------------------------------------------

check('Int eq', True, 1 == 1)
check('Int ne', True, 1 != 2)
check('Float eq', True, 1.5 == 1.5)
check('String eq', True, 'ab' == 'ab')
check('String ne on prefix', True, 'ab' != 'abc')
check('Bool eq', True, True == True)
check('Nil eq', True, Nil == Nil)
check('Binary eq', True, <<1, 2, 3>> == <<1, 2, 3>>)
check('Binary ne', True, <<1, 2>> != <<1, 2, 0>>)

// Ordering operators sit alongside equality on the ordered primitives.
check('Int lt', True, 1 < 2)
check('Int gte', True, 2 >= 2)
check('Float gt', True, 2.5 > 2.0)

// Int is a wrapping 64-bit integer. Values past the 48-bit small-int range
// spill to a boxed representation, and equality must see through the box.
spilled = 140737488355327 + 1
check('boxed int equals literal', True, spilled == 140737488355328)
check('boxed int ne', True, spilled != 140737488355327)
// i64::MIN is not writable as a literal (the magnitude overflows), so name
// it by the wrap itself.
int_min = 9223372036854775807 + 1
check('i64 arithmetic wraps to MIN', True, int_min < 0)
check('wrap is exact', True, int_min - 1 == 9223372036854775807)

// --- containers ----------------------------------------------------------------

check('Array eq', True, [1, 2, 3] == [1, 2, 3])
check('Array ne on length', True, [1, 2] != [1, 2, 3])
check('nested Array eq', True, [[1], [2, 3]] == [[1], [2, 3]])
check('Tuple eq', True, (1, 'a') == (1, 'a'))
check('nested Tuple eq', True, (1, (2, (3, 'x'))) == (1, (2, (3, 'x'))))
check('Tuple ne', True, (1, 'a') != (1, 'b'))
check('Array of Tuple eq', True, [(1, 'a')] == [(1, 'a')])

// Maps compare by contents, not insertion order.
m1 = map.from_list([('a', 1), ('b', 2)])
m2 = map.from_list([('b', 2), ('a', 1)])
m3 = map.from_list([('a', 1)])
check('Map eq ignores insertion order', True, m1 == m2)
check('Map ne on size', True, m1 != m3)

// --- user types -----------------------------------------------------------------

type Point {
	x Int
	y Int
}

type Outcome {
	Good(value String)
	Bad(code Int)
}

type Wrapper {
	inner Outcome
}

check('record eq', True, Point(x: 1, y: 2) == Point(x: 1, y: 2))
check('record ne on one field', True, Point(x: 1, y: 2) != Point(x: 1, y: 3))
check('enum eq compares payloads', True, Good('hi') == Good('hi'))
check('enum ne on payload', True, Good('hi') != Good('bye'))
check('enum ne across variants', True, Good('1') != Bad(1))
check('nested user type eq', True, Wrapper(inner: Bad(7)) == Wrapper(inner: Bad(7)))
check('Option eq', True, Some([1, 2]) == Some([1, 2]))
none_int Option(Int) = None
check('Option None eq', True, none_int == None)
ok1 Result(Int, String) = Ok(1)
ok2 Result(Int, String) = Ok(1)
er1 Result(Int, String) = Err('x')
check('Result eq', True, ok1 == ok2)
check('Result ne across variants', True, ok1 != er1)
check('Order eq', True, decimal.compare(decimal.from_int(1), decimal.from_int(2)) == Lt)

// --- where structural equality is NOT the semantic one -------------------------

// A Decimal carries (units, scale), so 1.10 and 1.1 are structurally distinct
// even though they are numerically equal. `decimal.eq` is the numeric answer;
// `==` is the structural one. Both are correct — for different questions.
d_scaled = decimal.parse('1.10') or decimal.from_int(0)
d_plain = decimal.parse('1.1') or decimal.from_int(0)
check('Decimal == is structural', False, d_scaled == d_plain)
check('decimal.eq is numeric', True, decimal.eq(d_scaled, d_plain))
check('normalize makes them structurally equal', True, decimal.normalize(d_scaled) == d_plain)
