// Every pattern form the AST can produce, plus the exhaustiveness rules that
// govern them. `check` prints PASS/FAIL so a wrong arm shows up in the golden
// as a FAIL line, not as an unexplained value.

import al/decimal
import al/decimal.{Rounding, Down}

fn check(name String, expected String, actual String) Nil {
	if expected == actual {
		println('PASS ${name}')
	} else {
		println('FAIL ${name}: expected ${expected}, got ${actual}')
	}
}

// --- literal, range, and negative-literal patterns ---------------------------

fn classify(n Int) String {
	match n {
		0 -> 'zero'
		-5 -> 'minus five'
		1..10 -> 'small'
		10..100 -> 'medium'
		else -> 'large'
	}
}
check('literal zero', 'zero', classify(0))
check('negative literal', 'minus five', classify(-5))
check('range low', 'small', classify(3))
check('range boundary is exclusive', 'medium', classify(10))
check('range fallthrough', 'large', classify(1000))

fn word(s String) String {
	match s {
		'' -> 'empty'
		'hi' -> 'greeting'
		else -> 'other'
	}
}
check('string literal', 'greeting', word('hi'))
check('empty string literal', 'empty', word(''))

fn approx(f Float) String {
	match f {
		3.14 -> 'pi'
		else -> 'not pi'
	}
}
check('float literal', 'pi', approx(3.14))

// --- or-patterns --------------------------------------------------------------

type Suit {
	Hearts
	Diamonds
	Clubs
	Spades
}

fn colour(s Suit) String {
	match s {
		Hearts | Diamonds -> 'red'
		Clubs | Spades -> 'black'
	}
}
check('or-pattern ctors red', 'red', colour(Diamonds))
check('or-pattern ctors black', 'black', colour(Spades))

fn tiny(n Int) String {
	match n {
		1 | 2 | 3 -> 'yes'
		else -> 'no'
	}
}
check('or-pattern literals', 'yes', tiny(2))
check('or-pattern literals fall through', 'no', tiny(4))

// An or-pattern is a pattern, so it nests wherever a pattern goes: in a tuple
// slot, in an array slot, and around a whole ctor arm. (The one place it may
// *not* go is inside a constructor's arguments.) A binding in one alternative
// must be bound by all of them, so the alternatives here bind nothing and the
// surrounding pattern does the binding.
fn ticket(t (Int, String)) String {
	match t {
		(1 | 2, name) -> 'priority ${name}'
		(_, name) -> 'standard ${name}'
	}
}
check('or-pattern in tuple slot', 'priority ann', ticket((2, 'ann')))
check('or-pattern in tuple slot misses', 'standard bob', ticket((7, 'bob')))

fn header(xs Array(Int)) String {
	match xs {
		[1 | 2 | 3, ..rest] -> 'lead ${rest}'
		[_, ..] -> 'other'
		[] -> 'empty'
	}
}
check('or-pattern in array slot', 'lead [9]', header([3, 9]))
check('or-pattern in array slot misses', 'other', header([8, 9]))

// A bare `_` arm is a catch-all and the parser accepts it, but it is not the
// canonical spelling: the formatter rewrites `_ ->` to `else ->`, so that is
// what an or-pattern's fallback looks like once the file is formatted.
fn parity(n Int) String {
	match n {
		0 | 2 | 4 -> 'even digit'
		else -> 'anything else'
	}
}
check('wildcard arm matches', 'even digit', parity(4))
check('wildcard arm catches the rest', 'anything else', parity(5))

// --- guards -------------------------------------------------------------------

// A guarded arm never counts toward exhaustiveness: the unguarded `n` arm is
// required even though the two guards look total.
fn sign(n Int) String {
	match n {
		x if x > 0 -> 'positive'
		x if x < 0 -> 'negative'
		else -> 'zero'
	}
}
check('guard true branch', 'positive', sign(4))
check('guard second branch', 'negative', sign(-4))
check('guard falls through', 'zero', sign(0))

// --- constructor patterns: positional, labeled, and rest ----------------------

type Shape {
	Circle(radius Int)
	Rect(width Int, height Int)
}

fn area(s Shape) Int {
	match s {
		Circle(r) -> 3 * r * r
		Rect(width: w, height: h) -> w * h
	}
}
check('positional ctor args', '12', '${area(Circle(radius: 2))}')
check('labeled ctor args', '12', '${area(Rect(width: 3, height: 4))}')

fn shape_name(s Shape) String {
	match s {
		Circle(..) -> 'circle'
		Rect(..) -> 'rect'
	}
}
check('ctor rest pattern', 'rect', shape_name(Rect(width: 1, height: 1)))

// A constructor reached through its module qualifier, and the same
// constructor imported by name — both denote one constructor.
fn rounding_name(m Rounding) String {
	match m {
		decimal.HalfUp -> 'half-up'
		Down -> 'down'
		else -> 'other'
	}
}
check('qualified ctor pattern', 'half-up', rounding_name(decimal.HalfUp))
check('imported ctor pattern', 'down', rounding_name(Down))

// A literal inside a constructor payload: the more specific arm must come
// first, and the general one is still required for the rest of the domain.
type Reply {
	Text(body String)
	Code(n Int)
}

fn triage(r Reply) String {
	match r {
		Text('') -> 'blank'
		Text(body) -> 'text ${body}'
		Code(404) -> 'missing'
		Code(n) -> 'code ${n}'
	}
}
check('literal in ctor payload', 'blank', triage(Text(body: '')))
check('binding after literal arm', 'text hi', triage(Text(body: 'hi')))
check('int literal in ctor payload', 'missing', triage(Code(n: 404)))
check('binding after int literal arm', 'code 500', triage(Code(n: 500)))

// --- nested constructors --------------------------------------------------------

fn describe(o Option(Result(Shape, String))) String {
	match o {
		Some(Ok(Circle(r))) -> 'circle ${r}'
		Some(Ok(Rect(..))) -> 'rect'
		Some(Err(e)) -> 'err ${e}'
		None -> 'none'
	}
}
check('nested ctor 3 deep', 'circle 9', describe(Some(Ok(Circle(radius: 9)))))
check('nested ctor err', 'err bad', describe(Some(Err('bad'))))
check('nested ctor none', 'none', describe(None))

// --- tuple patterns ---------------------------------------------------------------

fn quadrant(p (Int, Int)) String {
	match p {
		(0, 0) -> 'origin'
		(_, 0) -> 'x axis'
		(0, _) -> 'y axis'
		(x, y) -> '${x},${y}'
	}
}
check('tuple literal pair', 'origin', quadrant((0, 0)))
check('tuple wildcard', 'x axis', quadrant((3, 0)))
check('tuple bindings', '2,5', quadrant((2, 5)))

fn deep(t ((Int, Int), (Bool, String))) String {
	match t {
		((1, 2), (True, s)) -> 'exact ${s}'
		((_, _), (False, _)) -> 'false'
		(inner, _) -> 'other ${inner}'
	}
}
check('nested tuple exact', 'exact go', deep(((1, 2), (True, 'go'))))
check('nested tuple bind whole', 'other (9, 9)', deep(((9, 9), (True, 'x'))))

// Bool tuples exhaust without an else arm.
fn xor(t (Bool, Bool)) String {
	match t {
		(True, True) -> 'tt'
		(True, False) -> 'tf'
		(False, True) -> 'ft'
		(False, False) -> 'ff'
	}
}
check('bool tuple exhaustive', 'ft', xor((False, True)))

// --- array patterns -------------------------------------------------------------

fn shape_of(xs Array(Int)) String {
	match xs {
		[] -> 'empty'
		[x] -> 'one ${x}'
		[1, 2] -> 'exactly 1,2'
		[_, _] -> 'two'
		[first, ..rest] -> 'head ${first} tail ${rest}'
	}
}
empty Array(Int) = []
check('array empty', 'empty', shape_of(empty))
check('array single', 'one 7', shape_of([7]))
check('array literal elements', 'exactly 1,2', shape_of([1, 2]))
check('array fixed length', 'two', shape_of([4, 5]))
check('array spread binding', 'head 1 tail [2, 3, 4]', shape_of([1, 2, 3, 4]))

// `[]` plus `[_, ..]` is total — no else arm needed.
fn is_empty(xs Array(a)) Bool {
	match xs {
		[] -> True
		[_, ..] -> False
	}
}
check('array cons split is total (non-empty)', 'False', '${is_empty([1])}')
check('array cons split is total (empty)', 'True', '${is_empty(empty)}')

// Tuples inside arrays and arrays inside tuples.
fn lookup(rows Array((String, Int))) String {
	match rows {
		[] -> 'none'
		[('a', n), ..] -> 'a=${n}'
		[(k, _), ..] -> 'first ${k}'
	}
}
check('tuple in array', 'a=1', lookup([('a', 1), ('b', 2)]))
check('tuple in array fallthrough', 'first z', lookup([('z', 9)]))

fn tagged(t (Array(Int), String)) String {
	match t {
		([], tag) -> 'empty ${tag}'
		([x, ..], tag) -> '${x} ${tag}'
	}
}
check('array in tuple', '5 hi', tagged(([5, 6], 'hi')))

// --- binary patterns ---------------------------------------------------------

// A binary match is never exhaustive without a wildcard: the byte-string
// domain is infinite.
fn frame(b Binary) String {
	match b {
		<<'AL', version:8, len:16, body:bytes(len), ..rest>> -> 'v${version} ${body} +${rest}'
		<<>> -> 'empty'
		else -> 'unrecognised'
	}
}
check(
	'binary utf8 + widths + bytes(n) + rest',
	'v2 <<7, 8, 9>> +<<255>>',
	frame(<<'AL', 2, 3:16, 7, 8, 9, 255>>),
)
check('binary empty', 'empty', frame(<<>>))
check('binary wildcard', 'unrecognised', frame(<<1, 2>>))

// A segment width can be a runtime expression.
width = 3
check('binary size(expr)', '<<0, 0, 5>>', '${<<5:size(width * 8)>>}')

// --- irrefutable destructuring statements -------------------------------------

type Point {
	x Int
	y Int
}

// Single-constructor types destructure without a match.
Point(px, py) = Point(x: 3, y: 4)
check('ctor destructuring binding', '3,4', '${px},${py}')

(ta, tb) = ('left', 2)
check('tuple destructuring binding', 'left/2', '${ta}/${tb}')

// `Nil = expr` asserts the type of a discarded value.
Nil = println('typed discard runs for its effect')
