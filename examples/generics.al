// Generics and Hindley-Milner inference. AL infers the most general type a
// definition can have; an annotation is documentation the compiler checks, not
// information it needs. Lowercase names in a type position are type variables.

import al/array
import al/string

/** A generic record. `a` and `b` are filled in fresh at every use site. */
type Pair(a, b) {
	first a
	second b
}

/** A generic alias: `Bag(Int)` and `Array(Int)` are the same type. */
type Bag(a) = Array(a)

/** An alias for a function type, so signatures can say what they mean. */
type Validate = fn(Int) Result(Int, String)

/** A generic enum: a persistent stack, recursive in its own type parameter. */
type Stack(a) {
	Empty
	Top(head a, rest Stack(a))
}

// Inferred as `fn(a) a` with no annotation at all — nothing about the body
// constrains x, so it generalises.
fn identity(x) {
	x
}

// The same function, written out. `f` maps some type to itself, so `twice`
// does too: one type variable, used three times, ties the signature together.
fn twice(f fn(a) a, x a) a {
	f(f(x))
}

// Functions are values: `compose` takes two and returns a third.
fn compose(f fn(b) c, g fn(a) b) fn(a) c {
	fn(x) f(g(x))
}

fn swap(p Pair(a, b)) Pair(b, a) {
	Pair(first: p.second, second: p.first)
}

fn push(s Stack(a), x a) Stack(a) {
	Top(head: x, rest: s)
}

fn to_array(s Stack(a)) Bag(a) {
	match s {
		Empty -> []
		Top(head, rest) -> [head, ..to_array(rest)]
	}
}

// Indexing any array — Bag(a) included — yields an Option, never a crash.
fn first_of(xs Bag(a)) Option(a) {
	xs[0]
}

fn positive(n Int) Result(Int, String) {
	if n > 0 {
		Ok(n)
	} else {
		Err('${n} is not positive')
	}
}

// One definition, instantiated at Int and at String in the same expression.
println('${identity(42)} and ${identity('forty-two')}')
println('twice doubling 3: ${twice(fn(x) x * 2, 3)}')
println('twice shouting: ${twice(fn(s) '${s}!', 'go')}')

// b is Int here, so `size_then_double` is fn(String) Int.
size_then_double = compose(fn(n) n * 2, fn(s) string.length(s))
println('size_then_double(claude) = ${size_then_double('claude')}')

pair = Pair(first: 'one', second: 1)
println('${string.inspect(pair)} swapped is ${string.inspect(swap(pair))}')

stack = push(push(push(Empty, 1), 2), 3)
println('stack ${string.inspect(to_array(stack))}')
println('first ${string.inspect(first_of(to_array(stack)))}')
println('first of nothing ${string.inspect(first_of(to_array(Empty)))}')

// A Validate is any fn(Int) Result(Int, String) — the alias checks the shape.
check Validate = positive
println('${string.inspect(check(7))} / ${string.inspect(check(-1))}')

// The element type is inferred from the array; `sizes` is Bag(Int).
sizes Bag(Int) = array.map(['al', 'claude', 'inference'], fn(s) string.length(s))
println('sizes ${string.inspect(sizes)}')
