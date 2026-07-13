// Hindley-Milner regression surface: parameter inference, generalization,
// let-polymorphism, constrained type variables, and fn-type unification.
// Every binding here is inferred unless the annotation is the point.

// --- parameter types inferred from the body ---------------------------------

fn double(x) {
	x * 2
}
fn concat(a, b) {
	a + b
}
fn is_positive(n) {
	n > 0
}
fn identity(x) {
	x
}
println(double(21))
println(concat(10, 5))
println(concat('Hello, ', 'World'))
println(is_positive(42))
println(identity('polymorphic'))

// A return annotation flows backwards into an un-annotated parameter.
fn as_int(x) Int {
	x
}
println(as_int(3))

// The two halves of a signature are independent: annotate the parameters and
// leave the return type off, and the body still decides what comes back.
fn shout(s String) {
	s + '!'
}
println(shout('half annotated'))

// --- recursion -------------------------------------------------------------

// Self-recursive lambda: the binding is in scope inside its own initializer.
countdown = fn(n) if n > 0 {
	println(n)
	countdown(n - 1)
} else {
	Nil
}
countdown(2)

// Mutual recursion: both fns are in one SCC and generalize together.
fn is_even(n) {
	if n == 0 {
		True
	} else {
		is_odd(n - 1)
	}
}
fn is_odd(n) {
	if n == 0 {
		False
	} else {
		is_even(n - 1)
	}
}
println(is_even(10))
println(is_odd(10))

// --- let-polymorphism ------------------------------------------------------

// A let-bound lambda generalizes: three instantiations of one binding.
id = fn(v) v
println(id(1))
println(id('one'))
println(id(True))

// ...and it generalizes inside a function body too, not just at module scope.
fn describe_all() String {
	show = fn(v) '<${v}>'
	'${show(1)}${show('x')}${show(2.5)}'
}
println(describe_all())

// --- constrained type variables (addable / numeric) -------------------------

// `+` is addable: Int, Float, String. One generic fn, three instantiations.
fn twice_added(x) {
	x + x
}
println(twice_added(2))
println(twice_added(1.5))
println(twice_added('ab'))

// `*` is numeric: Int and Float only.
fn square(x) {
	x * x
}
println(square(7))
println(square(0.5))

// --- fn-type unification ---------------------------------------------------

fn ident(x a) a {
	x
}
fn call_with_seven(g fn(Int) Int) Int {
	g(7)
}
// A generic fn unifies against a concrete fn slot.
println(call_with_seven(ident))
println(call_with_seven(fn(n Int) n * 3))

fn compose(f fn(b) c, g fn(a) b) fn(a) c {
	fn(x) f(g(x))
}
inc = fn(n Int) n + 1
render = fn(n Int) '#${n}'
println(compose(render, inc)(41))

fn apply_twice(x, f) {
	f(f(x))
}
println(apply_twice(5, fn(n) n + 1))
println(apply_twice('a', fn(s) '${s}!'))

// --- tuple / array element unification -------------------------------------

fn divmod(a Int, b Int) (Int, Int) {
	(a / b, a % b)
}
(q, r) = divmod(17, 5)
println('${q} rem ${r}')

fn firsts(xs Array((a, b))) Array(a) {
	match xs {
		[] -> []
		[(x, _), ..t] -> [x, ..firsts(t)]
	}
}
println(firsts([(1, 'a'), (2, 'b')]))

fn fold(xs Array(a), init b, f fn(b, a) b) b {
	match xs {
		[] -> init
		[h, ..t] -> fold(t, f(init, h), f)
	}
}
println(fold([1, 2, 3], 0, fn(acc, x) acc + x))
println(fold(['a', 'b'], '', fn(acc, x) acc + x))

// --- rebinding retypes ------------------------------------------------------

// A rebinding is a fresh binding, so its type may differ from the old one.
v = 10
println(v)
v = 'ten'
println(v)

// --- typing of the remaining value-producing forms ---------------------------

const origin = 0

// A constructor is an ordinary function value and can be bound as one.
const wrap = Some

type Failure {
	Boom(why String)
}

fn find(id Int) Option(Int) {
	if id == 0 {
		None
	} else {
		Some(id)
	}
}

fn risky(n Int) Result(Int, Failure) {
	if n < 0 {
		Err(Boom(why: 'negative'))
	} else {
		Ok(n * 2)
	}
}

println(wrap('ctor value'))

// `or` unwraps Option and Result; its body must have the payload's type.
println(find(0) or origin)
println(find(3) or origin)
println(risky(4) or -1)
println(
	risky(-1) or e -> match e {
		Boom(_) -> -1
	},
)

// Indexing an array yields Option, so `or` is its natural consumer.
items = [10, 20, 30]
println(items[1] or origin)
println(items[9] or origin)

// A range expression is an Array(Int), half-open.
println(0..5)

// An if/else-if chain is one expression; every branch unifies to one type.
fn grade(n Int) String {
	if n >= 90 {
		'A'
	} else if n >= 80 {
		'B'
	} else {
		'F'
	}
}
println(grade(85))
println(!False || False && True)

// --- An external type -------------------------------------------------------

// A type declared with no body is *external*: a nominal type with no
// constructors. It is how the stdlib names a value that only the runtime can
// make (al/net's `Server`). The checker tracks it like any other type — it
// unifies, it instantiates `Array(a)` — but no expression in AL produces one,
// so `hold` type checks and can never be called.
type Handle

fn hold(h Handle) Option(Handle) {
	Some(h)
}

handles Array(Handle) = []
println(handles)

// A block is an expression: its value is its final expression.
total = {
	a = 10
	b = 20
	a + b
}
println(total)
