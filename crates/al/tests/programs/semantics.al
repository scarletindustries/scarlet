// Core evaluation semantics: the rules a program relies on without ever
// naming them.
//
// Evaluation order (strictly left to right, with `&&`/`||` short-circuiting),
// blocks as expressions, rebinding versus shadowing across scopes, `const`,
// `or` on Result and Option, unary operators, string escapes and nested
// interpolation, array indexing (an Option, never a trap), spread in arrays and
// in constructor record-update, labelled constructor arguments, typed discard,
// and destructuring binds.
//
// The last line is a non-Nil expression: `al run` prints a program's tail value
// unless it is Nil, so the final `42` in the golden is that rule, not a print.

import al/array
import al/string

// --- Evaluation order -----------------------------------------------------

// `note` shouts as it is evaluated, so the print order *is* the evaluation
// order.
fn note(tag String, v Int) Int {
	println('  eval ${tag}')
	v
}

fn three(a Int, b Int, c Int) Int {
	a * 100 + b * 10 + c
}

fn loud(tag String, v Bool) Bool {
	println('  eval ${tag}')
	v
}

println('call args, left to right:')
println('= ${three(note('a', 1), note('b', 2), note('c', 3))}')

println('binary operands, left to right:')
println('= ${note('lhs', 2) * note('rhs', 3)}')

println('&& short-circuits on the first False:')
println('= ${loud('l', False) && loud('r', True)}')

println('|| short-circuits on the first True:')
println('= ${loud('l', True) || loud('r', False)}')

println('and both sides run when they must:')
println('= ${loud('l', True) && loud('r', False)}')

// --- Blocks as expressions ------------------------------------------------

// A block's value is its last expression, so it can stand anywhere a value can:
// on the right of a bind, inside an `if`, inside interpolation.
squared = {
	base = 6
	base * base
}
println('block value: ${squared}')
println('block in interpolation: ${{ 1 + 1 }}')

grade = if squared > 30 {
	tier = 'high'
	'${tier} (${squared})'
} else {
	'low'
}
println('if is an expression: ${grade}')

// --- Rebinding vs shadowing ------------------------------------------------

// A re-bind in the *same* scope overwrites the binding; a re-bind inside a
// nested block is a new binding in that block, and the outer one is untouched
// when the block ends.
counter = 1
counter = counter + 1
counter = counter * 10
println('same-scope rebind: ${counter}')

outer = 1
inner = {
	outer = 2
	outer + 10
}
println('nested shadow: outer=${outer} inner=${inner}')

// A parameter shadows a top-level name inside the function only.
const scale = 3

fn shadow_param(scale Int) Int {
	scale * 2
}

println('param shadows const: ${shadow_param(10)} const is still ${scale}')

// --- `or` on Result and Option ---------------------------------------------

fn checked(n Int) Result(Int, String) {
	if n >= 0 {
		Ok(n)
	} else {
		Err('negative: ${n}')
	}
}

println('or default: ${checked(4) or 0} ${checked(-4) or 0}')
println('or binding: ${checked(-4) or e -> string.length(e)}')
found = array.find([1, 2, 3], fn(x) x > 2)
missing = array.find([1, 2, 3], fn(x) x > 9)
println('or on Option: ${found or 0} ${missing or 0}')

// --- Unary operators --------------------------------------------------------

n = 7
flag = False
// Grouping is `{ }` — a single-element paren is a parse error, so a double
// negation is written `-{ -n }`.
println('unary: ${-n} ${-{ -n }} ${!flag} ${!{ !flag }} ${-3.5}')

// --- Strings: escapes and interpolation ------------------------------------

// The escape set is exactly \n \t \r \0 \" \' \\ \$ — no \u. Both quote styles
// hold interpolation; `\$` suppresses it.
println('escapes: a\tb "${'quoted'}" \'single\' back\\slash')
println('literal dollar: \${not interpolated}')
control = 'nul\0cr\rnl\n'
println('control lengths: ${string.length(control)} ${string.length('\t')} ${string.length('\\')}')
println("double quotes hold an apostrophe — it's ${1 + 1} of them — and interpolate")

// Interpolation nests: the inner string is a whole expression, `if` and all.
mood = 'good'
println(
	'nested: ${if mood == 'good' {
		'yes: ${string.length(mood)}'
	} else {
		'no'
	}}',
)
println('empty interpolation of empty string: [${''}]')

// --- Array indexing is an Option --------------------------------------------

xs = [10, 20, 30]
println('index: ${string.inspect(xs[0])} ${string.inspect(xs[2])}')
println('out of bounds: ${string.inspect(xs[3])} ${string.inspect(xs[-1])}')
println('index with fallback: ${xs[1] or 0} ${xs[99] or 0}')

// --- Slicing ------------------------------------------------------------------

// A *range* subscript is a slice, not an index: it yields an Array directly
// rather than an Option, because its bounds are checked — an out-of-range slice
// traps instead of clamping, so every slice below is in range. The range is
// half-open: the end index is one past the last element kept, and `lo == hi` is
// the empty slice.
digits = [1, 2, 3, 4, 5]
println('slice: ${string.inspect(digits[1..3])}')
println('slice whole: ${string.inspect(digits[0..5])}')
println('slice empty: ${string.inspect(digits[2..2])}')

// The bounds are ordinary expressions, evaluated at run time.
lo = 1
println('slice computed bounds: ${string.inspect(digits[lo..{ lo + 3 }])}')

// --- A range is a value --------------------------------------------------------

// `a..b` standing alone is an expression, and it evaluates to the half-open
// array of its elements — so it is `==` to the array literal it spells out, and
// it indexes, slices and spreads like any other array.
span = 1..4
println('range value: ${string.inspect(span)} ${span == [1, 2, 3]}')
println('range with bounds that meet: ${string.inspect(3..3)}')
println('range indexed: ${string.inspect({ 10..20 }[2])}')

// --- Spread -----------------------------------------------------------------

// In an array literal, `..` splices another array in — and a range is an array.
mid = [2, 3]
println('array spread: ${string.inspect([1, ..mid, 4, ..{ 5..8 }])}')
println('spread only: ${string.inspect([..mid])}')

type Config {
	host String
	port Int
	debug Bool
}

// Labelled arguments and record-update spread are constructor-only.
base = Config(host: 'localhost', port: 80, debug: False)
dev = Config(..base, debug: True)
moved = Config(..dev, port: 8080)
println('record update: ${string.inspect(base)}')
println('record update: ${string.inspect(moved)}')

// --- Typed discard and destructuring ---------------------------------------

// `Nil = expr` discards a Nil-typed result explicitly; `_ = expr` discards any.
// The type named on the left is checked like any other annotation, so a discard
// is also an assertion about what came back — `Int = three(...)` says so.
Nil = println('typed discard runs its expression')
Int = three(1, 2, 3)
_ = three(1, 2, 3)

pair = (1, 'one')
(num, word) = pair
println('tuple destructure: ${num} ${word}')

Config(host, port, debug) = moved
println('positional destructure: ${host} ${port} ${debug}')

// A labelled constructor pattern must name every field or say `..`.
Config(port: only_port, ..) = base
println('labelled destructure: ${only_port}')

// A `_`-prefixed name is a binding you promise not to read.
Config(host: _kept, port: kept_port, debug: _flag) = dev
println('ignored bindings: ${kept_port}')

// --- Declaration forms ------------------------------------------------------

// A `const` may carry a type annotation, checked against its initialiser.
const limit Int = 100
const label String = 'cap'

// An external type has no body: no constructors, no fields. It names a value
// only the host can make. Declaring one must still typecheck.
pub type Handle

// A function type needs no return annotation. `fn(Int)` is a function of one
// Int whose result the caller discards — the shape every `each` callback has.
fn countdown(n Int, f fn(Int)) Nil {
	if n > 0 {
		f(n)
		countdown(n - 1, f)
	} else {
		Nil
	}
}

countdown(3, fn(i) println('tick ${i}'))
println('annotated consts: ${label}=${limit}')

// A never-called `pub fn` is not an entry point — there is no `main` magic —
// and an unused top-level function must not warn.
pub fn main() Nil {
	println('never runs')
}

// The tail expression of a program is printed unless it is Nil.
40 + 2
