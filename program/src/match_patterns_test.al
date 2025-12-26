// ============================================================================
// Match Pattern Tests
// ============================================================================

fn test(name a, expected a, actual a) {
	if expected == actual {
		println('PASS: ${name}')
	} else {
		println('')
		println('⚠️ FAIL: ${name} - expected "${expected}", got "${actual}"')
		println('')
	}
}

// ----------------------------------------------------------------------------
// Literal matching
// ----------------------------------------------------------------------------

r1 = match 42 {
	0 -> 'zero',
	42 -> 'forty-two',
	else -> 'other',
}
test('int literal match', 'forty-two', r1)

r2 = match 99 {
	0 -> 'zero',
	42 -> 'forty-two',
	else -> 'other',
}
test('int literal else', 'other', r2)

r3 = match 'hello' {
	'world' -> 'w',
	'hello' -> 'h',
	else -> 'other',
}
test('string literal match', 'h', r3)

// ----------------------------------------------------------------------------
// Range matching
// ----------------------------------------------------------------------------

r4 = match 5 {
	0..10 -> 'single digit',
	10..100 -> 'double digit',
	else -> 'big',
}
test('range match low', 'single digit', r4)

r5 = match 50 {
	0..10 -> 'single digit',
	10..100 -> 'double digit',
	else -> 'big',
}
test('range match mid', 'double digit', r5)

r6 = match 100 {
	0..10 -> 'single digit',
	10..100 -> 'double digit',
	else -> 'big',
}
test('range match boundary', 'big', r6)

// ----------------------------------------------------------------------------
// Array patterns - length
// ----------------------------------------------------------------------------

r7 = match [1, 2, 3] {
	[] -> 'empty',
	[x] -> 'one',
	[x, y] -> 'two',
	else -> 'many',
}
test('array length 3', 'many', r7)

r8 = match [99] {
	[] -> 'empty',
	[x] -> 'one: ${x}',
	else -> 'many',
}
test('array length 1', 'one: 99', r8)

empty_arr []Int = []
r9 = match empty_arr {
	[] -> 'empty',
	[x] -> 'one',
	else -> 'many',
}
test('array empty', 'empty', r9)

// ----------------------------------------------------------------------------
// Array patterns - literals
// ----------------------------------------------------------------------------

r10 = match [1, 2, 3] {
	[1, 2, 3] -> 'exact',
	[1, 2] -> 'partial',
	else -> 'other',
}
test('array exact match', 'exact', r10)

r11 = match [1, 2, 99] {
	[1, 2, 3] -> 'exact',
	[1, 2, x] -> 'first two match: ${x}',
	else -> 'other',
}
test('array partial literal', 'first two match: 99', r11)

r12 = match [9, 9, 9] {
	[1, 2, 3] -> 'exact',
	[1, 2, x] -> 'first two',
	else -> 'other',
}
test('array no match', 'other', r12)

// ----------------------------------------------------------------------------
// Array patterns - spread
// ----------------------------------------------------------------------------

r13 = match [1, 2, 3, 4, 5] {
	[1, ..rest] -> 'starts with 1: ${rest}',
	else -> 'other',
}
test('array spread', 'starts with 1: [2, 3, 4, 5]', r13)

r14 = match [1] {
	[1, ..rest] -> 'starts with 1: ${rest}',
	else -> 'other',
}
test('array spread empty rest', 'starts with 1: []', r14)

r15 = match [99, 2, 3] {
	[1, ..] -> 'starts with 1',
	[x, ..rest] -> 'starts with ${x}: ${rest}',
	else -> 'other',
}
test('array spread bind first', 'starts with 99: [2, 3]', r15)

// ----------------------------------------------------------------------------
// Tuple patterns - literals
// ----------------------------------------------------------------------------

r16 = match (42, 'hello') {
	(42, 'hello') -> 'exact',
	(42, x) -> 'forty-two with ${x}',
	else -> 'other',
}
test('tuple exact match', 'exact', r16)

r17 = match (42, 'world') {
	(42, 'hello') -> 'exact',
	(42, x) -> 'forty-two with ${x}',
	else -> 'other',
}
test('tuple partial literal', 'forty-two with world', r17)

r18 = match (99, 'test') {
	(42, 'hello') -> 'exact',
	(42, x) -> 'forty-two',
	else -> 'other',
}
test('tuple no match', 'other', r18)

// ----------------------------------------------------------------------------
// Tuple patterns - all bindings
// ----------------------------------------------------------------------------

r19 = match (100, 'alice') {
	(id, name) -> 'user ${id}: ${name}',
}
test('tuple all bindings', 'user 100: alice', r19)

// ----------------------------------------------------------------------------
// Nested tuples
// ----------------------------------------------------------------------------

r20 = match ((1, 2), 'outer') {
	((1, 2), x) -> 'nested match: ${x}',
	else -> 'other',
}
test('nested tuple literal', 'nested match: outer', r20)

r21 = match ((99, 2), 'outer') {
	((1, 2), x) -> 'exact inner',
	(inner, x) -> 'inner is ${inner}, outer is ${x}',
}
test('nested tuple bind whole', 'inner is (99, 2), outer is outer', r21)

// ----------------------------------------------------------------------------
// Mixed patterns
// ----------------------------------------------------------------------------

r22 = match [1, 2] {
	[0, 0] -> 'zeros',
	[1, x] -> 'one and ${x}',
	else -> 'other',
}
test('array mixed literal and binding', 'one and 2', r22)

r23 = match (true, 42) {
	(true, 0) -> 'true zero',
	(true, n) -> 'true ${n}',
	(false, n) -> 'false ${n}',
}
test('tuple bool and int', 'true 42', r23)

// ----------------------------------------------------------------------------
// Edge cases
// ----------------------------------------------------------------------------

r24 = match 0 {
	0 -> 'zero',
	else -> 'not zero',
}
test('match zero', 'zero', r24)

r25 = match '' {
	'' -> 'empty string',
	else -> 'not empty',
}
test('match empty string', 'empty string', r25)

r26 = match (0) {
	(0) -> 'tuple zero',
	else -> 'other',
}
test('single element tuple', 'tuple zero', r26)

// ============================================================================
// Boolean Exhaustiveness
// ============================================================================

b1 = match true {
	true -> 'yes',
	false -> 'no',
}
test('bool exhaustive true', 'yes', b1)

b2 = match false {
	true -> 'yes',
	false -> 'no',
}
test('bool exhaustive false', 'no', b2)

b3 = match true {
	false -> 'no',
	true -> 'yes',
}
test('bool reverse order', 'yes', b3)

// ============================================================================
// Nested Boolean Tuples (exhaustive)
// ============================================================================

bb1 = match (true, true) {
	(true, true) -> 'tt',
	(true, false) -> 'tf',
	(false, true) -> 'ft',
	(false, false) -> 'ff',
}
test('bool tuple tt', 'tt', bb1)

bb2 = match (false, true) {
	(true, true) -> 'tt',
	(true, false) -> 'tf',
	(false, true) -> 'ft',
	(false, false) -> 'ff',
}
test('bool tuple ft', 'ft', bb2)

// ============================================================================
// Triple Boolean Tuple
// ============================================================================

bbb = match (true, false, true) {
	(true, true, true) -> 'ttt',
	(true, true, false) -> 'ttf',
	(true, false, true) -> 'tft',
	(true, false, false) -> 'tff',
	(false, true, true) -> 'ftt',
	(false, true, false) -> 'ftf',
	(false, false, true) -> 'fft',
	(false, false, false) -> 'fff',
}
test('triple bool tuple', 'tft', bbb)

// ============================================================================
// Larger Tuples
// ============================================================================

t3 = match (1, 'hello', true) {
	(1, 'hello', true) -> 'exact',
	(1, 'hello', false) -> 'almost',
	(1, s, b) -> 'one with ${s}',
	else -> 'other',
}
test('triple tuple exact', 'exact', t3)

t4 = match (1, 2, 3, 4) {
	(1, 2, 3, 4) -> 'exact',
	(1, 2, 3, x) -> 'first three: ${x}',
	(1, 2, x, y) -> 'first two',
	else -> 'other',
}
test('quad tuple exact', 'exact', t4)

t5 = match (1, 2, 3, 99) {
	(1, 2, 3, 4) -> 'exact',
	(1, 2, 3, x) -> 'first three match, last: ${x}',
	else -> 'other',
}
test('quad tuple partial', 'first three match, last: 99', t5)

// ============================================================================
// Deep Nesting
// ============================================================================

deep1 = match (((true))) {
	(((true))) -> 'deep true',
	(((false))) -> 'deep false',
}
test('deeply nested bool', 'deep true', deep1)

deep2 = match ((1, 2), (3, 4)) {
	((1, 2), (3, 4)) -> 'exact',
	((1, 2), (3, x)) -> 'first pair exact',
	((1, 2), p) -> 'first pair only',
	else -> 'other',
}
test('tuple of tuples', 'exact', deep2)

deep3 = match ((1, (2, 3)), 4) {
	((1, (2, 3)), 4) -> 'exact',
	((1, (2, x)), 4) -> 'inner partial',
	else -> 'other',
}
test('nested inner tuple', 'exact', deep3)

// ============================================================================
// Arrays in Tuples
// ============================================================================

at1 = match ([1, 2], 'tag') {
	([1, 2], 'tag') -> 'exact',
	([1, 2], t) -> 'array match',
	([], t) -> 'empty array',
	([x, ..rest], t) -> 'nonempty',
}
test('array in tuple exact', 'exact', at1)

at2 = match ([1, 2, 3], 'tag') {
	([], t) -> 'empty',
	([x], t) -> 'single',
	([x, y], t) -> 'double',
	([x, ..rest], t) -> 'many: ${x}',
}
test('array in tuple spread', 'many: 1', at2)

// ============================================================================
// Tuples in Arrays
// ============================================================================

ta1 = match [(1, 'a'), (2, 'b')] {
	[] -> 'empty',
	[(1, 'a'), ..rest] -> 'starts with (1, a)',
	[first, ..rest] -> 'other start',
}
test('tuple array spread', 'starts with (1, a)', ta1)

ta2_arr = [(true, 1)]
ta2 = match ta2_arr {
	[] -> 'empty',
	[(true, n), ..rest] -> 'true first: ${n}',
	[(false, n), ..rest] -> 'false first: ${n}',
}
test('bool tuple in array', 'true first: 1', ta2)

// ============================================================================
// More Spread Patterns
// ============================================================================

sp1 = match [1, 2, 3, 4, 5] {
	[] -> 'empty',
	[a] -> 'one',
	[a, b] -> 'two',
	[a, b, c] -> 'three',
	[a, b, c, d] -> 'four',
	[a, b, c, d, e] -> 'five: ${a} ${b} ${c} ${d} ${e}',
	[a, ..rest] -> 'more',
}
test('exact length 5', 'five: 1 2 3 4 5', sp1)

sp2 = match [1, 2, 3, 4, 5, 6] {
	[] -> 'empty',
	[a] -> 'one',
	[a, b] -> 'two',
	[a, b, c] -> 'three',
	[a, b, c, d] -> 'four',
	[a, b, c, d, e] -> 'five',
	[a, ..rest] -> 'more than five',
}
test('more than 5 elements', 'more than five', sp2)

sp3 = match [10, 20] {
	[] -> 'empty',
	[1, ..rest] -> 'starts with 1',
	[10, ..rest] -> 'starts with 10: rest is ${rest}',
	[x, ..rest] -> 'starts with ${x}',
}
test('spread with literal check', 'starts with 10: rest is [20]', sp3)

// ============================================================================
// Wildcards in Various Positions
// ============================================================================

w1 = match (1, 2, 3) {
	(1, _, 3) -> 'first and last',
	(1, 2, _) -> 'first two',
	(_, _, _) -> 'any',
}
test('wildcard middle', 'first and last', w1)

w2 = match (5, 2, 3) {
	(1, _, 3) -> 'first and last',
	(1, 2, _) -> 'first two',
	(_, _, _) -> 'any',
}
test('wildcard fallthrough', 'any', w2)

w3 = match [1, 2, 3] {
	[] -> 'empty',
	[_, _, _] -> 'exactly three',
	[_, ..rest] -> 'other',
}
test('wildcard array exact', 'exactly three', w3)

w4 = match [1, 2, 3, 4] {
	[] -> 'empty',
	[_, _, _] -> 'exactly three',
	[_, ..rest] -> 'more than three',
}
test('wildcard array fallthrough', 'more than three', w4)

// ============================================================================
// Negative Numbers
// ============================================================================

neg1 = match 0 - 5 {
	0 -> 'zero',
	-5 -> 'negative five',
	else -> 'other',
}
test('negative literal', 'negative five', neg1)

neg2 = match 75 {
	0..50 -> 'small',
	50..100 -> 'medium',
	else -> 'large',
}
test('positive range', 'medium', neg2)

// ============================================================================
// Float Literals
// ============================================================================

f1 = match 3.14 {
	0.0 -> 'zero',
	3.14 -> 'pi',
	else -> 'other',
}
test('float literal', 'pi', f1)

f2 = match 2.5 {
	1.0 -> 'one',
	2.0 -> 'two',
	else -> 'other float',
}
test('float else', 'other float', f2)

// ============================================================================
// Complex Nested with Mixed Types
// ============================================================================

cx1 = match ((true, [1, 2]), 'tag') {
	((true, []), t) -> 'true empty',
	((true, [1, 2]), 'tag') -> 'exact',
	((true, [x, ..rest]), t) -> 'true with array',
	((false, arr), t) -> 'false',
}
test('complex nested exact', 'exact', cx1)

cx2 = match ((true, [1, 2, 3]), 'other') {
	((true, []), t) -> 'true empty',
	((true, [1, 2]), 'tag') -> 'exact',
	((true, [x, ..rest]), t) -> 'true with array: ${x}',
	((false, arr), t) -> 'false',
}
test('complex nested fallthrough', 'true with array: 1', cx2)

// ============================================================================
// String Patterns
// ============================================================================

s1 = match 'hello world' {
	'' -> 'empty',
	'hello' -> 'just hello',
	'hello world' -> 'full greeting',
	else -> 'other',
}
test('string full match', 'full greeting', s1)

s2 = match 'goodbye' {
	'hello' -> 'hello',
	'world' -> 'world',
	'goodbye' -> 'goodbye',
	else -> 'unknown',
}
test('string third option', 'goodbye', s2)

// ============================================================================
// Multiple Same-Type Comparisons
// ============================================================================

m1 = match 42 {
	1 -> 'one',
	2 -> 'two',
	3 -> 'three',
	42 -> 'answer',
	100 -> 'hundred',
	else -> 'other',
}
test('multiple int literals', 'answer', m1)

m2 = match 'cat' {
	'dog' -> 'd',
	'cat' -> 'c',
	'bird' -> 'b',
	'fish' -> 'f',
	else -> '?',
}
test('multiple string literals', 'c', m2)

// ============================================================================
// First Match Wins
// ============================================================================

fm1 = match 5 {
	5 -> 'first',
	else -> 'second',
}
test('first match wins literal', 'first', fm1)

fm2 = match [1, 2] {
	[1, 2] -> 'exact',
	[1, x] -> 'one and something',
	[x, y] -> 'two things',
	else -> 'other',
}
test('first match wins array', 'exact', fm2)

fm3 = match [1, 99] {
	[1, 2] -> 'exact',
	[1, x] -> 'one and ${x}',
	[x, y] -> 'two things',
	else -> 'other',
}
test('second match array', 'one and 99', fm3)

// ============================================================================
// Array Complete Patterns (no else needed)
// ============================================================================

ac1 = match [1] {
	[] -> 'empty',
	[x, ..rest] -> 'nonempty: ${x}',
}
test('array complete single', 'nonempty: 1', ac1)

empty_arr2 []Int = []
ac2 = match empty_arr2 {
	[] -> 'empty',
	[x, ..rest] -> 'nonempty',
}
test('array complete empty', 'empty', ac2)

// ============================================================================
// Binding Same Value Multiple Ways
// ============================================================================

bv1 = match (1, 1) {
	(0, 0) -> 'zeros',
	(1, 1) -> 'ones',
	(a, b) -> 'different: ${a} ${b}',
}
test('same values exact', 'ones', bv1)

bv2 = match (1, 2) {
	(0, 0) -> 'zeros',
	(1, 1) -> 'ones',
	(a, b) -> 'different: ${a} ${b}',
}
test('different values bind', 'different: 1 2', bv2)

// ============================================================================
// Empty Structures
// ============================================================================

e1_arr []Int = []
e1 = match e1_arr {
	[] -> 'empty array',
	[x, ..rest] -> 'nonempty',
}
test('empty array match', 'empty array', e1)

e2 = match ('', 0) {
	('', 0) -> 'empty string and zero',
	(s, n) -> 'other',
}
test('empty string in tuple', 'empty string and zero', e2)

// ============================================================================
// Large Arrays
// ============================================================================

la1 = match [1, 2, 3, 4, 5, 6, 7, 8, 9, 10] {
	[] -> 'empty',
	[a, b, c, d, e, f, g, h, i, j] -> 'ten: ${a}..${j}',
	[x, ..rest] -> 'other',
}
test('ten element array', 'ten: 1..10', la1)

// ============================================================================
// Binding Unused
// ============================================================================

bu1 = match (1, 2) {
	(_, b) -> 'second is ${b}',
}
test('ignore first', 'second is 2', bu1)

bu2 = match (1, 2) {
	(a, _) -> 'first is ${a}',
}
test('ignore second', 'first is 1', bu2)

// ============================================================================
// Bool with Int
// ============================================================================

bi1 = match (true, 0) {
	(true, 0) -> 'true zero',
	(true, n) -> 'true nonzero',
	(false, 0) -> 'false zero',
	(false, n) -> 'false nonzero',
}
test('bool int true zero', 'true zero', bi1)

bi2 = match (false, 42) {
	(true, 0) -> 'true zero',
	(true, n) -> 'true nonzero',
	(false, 0) -> 'false zero',
	(false, n) -> 'false nonzero: ${n}',
}
test('bool int false nonzero', 'false nonzero: 42', bi2)

// ============================================================================
// Nested Arrays
// ============================================================================

na1_arr = [[1, 2], [3, 4]]
na1 = match na1_arr {
	[] -> 'empty outer',
	[first, ..rest] -> 'first inner: ${first}',
}
test('nested array', 'first inner: [1, 2]', na1)

ne2_arr = [[true, false], [false, true]]
na2 = match ne2_arr {
	[] -> 'empty outer',
	[[true, false], ..rest] -> 'first inner exact',
	else -> 'other',
}
test('nested array exact match', 'first inner exact', na2)

ne3_arr = [[1, 2, 3], [4, 5, 6]]
na3 = match ne3_arr {
	[] -> 'empty outer',
	[_, [4, 5]] -> 'first',
	[_, [4, 5, six]] -> 'second is ${six}',
	else -> 'other',
}
test('nested array exact match and invalid', 'second is 6', na3)
