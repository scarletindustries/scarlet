// Tail-call elimination and closure capture, adversarially.
//
// Three tail ops come out of core_ir::emit — TailCallSelf (a function calling
// itself), TailCallKnown (a known non-self function: mutual recursion), and
// TailCall (an indirect call through a closure value). All three must run in
// constant stack space, so every loop here recurses deep enough to blow a
// frame-per-call stack and then reports `internal.stack_depth()` at the bottom:
// two frames (the top level, plus the single live frame) no matter how deep.
//
// The closure half pins capture-by-value at creation time, capture of bindings
// born inside a branch, closures returned from functions, closures held in
// arrays and record fields, and recursion routed through a captured fn value.

import al/array
import al/internal
import al/option
import al/string

// --- TailCallSelf --------------------------------------------------------

fn sum_to(n Int, acc Int) Int {
	if n == 0 {
		acc
	} else {
		sum_to(n - 1, acc + n)
	}
}

fn self_depth(n Int) Int {
	if n == 0 {
		internal.stack_depth()
	} else {
		self_depth(n - 1)
	}
}

// --- TailCallKnown (mutual recursion) ------------------------------------

fn ping(n Int, acc Int) Int {
	if n == 0 {
		acc
	} else {
		pong(n - 1, acc + 1)
	}
}

fn pong(n Int, acc Int) Int {
	if n == 0 {
		0 - acc
	} else {
		ping(n - 1, acc + 1)
	}
}

fn mutual_depth_a(n Int) Int {
	if n == 0 {
		internal.stack_depth()
	} else {
		mutual_depth_b(n - 1)
	}
}

fn mutual_depth_b(n Int) Int {
	if n == 0 {
		internal.stack_depth()
	} else {
		mutual_depth_a(n - 1)
	}
}

// --- TailCall (indirect, through a closure value) ------------------------

// A closure cannot name itself, so the recursion is routed through a record
// that carries it: `run` tail-calls the field (an indirect TailCall) and the
// closure tail-calls `run` back (a TailCallKnown). Neither may grow the stack.
type Stepper {
	step fn(Stepper, Int, Int) Int
}

fn run(s Stepper, n Int, acc Int) Int {
	if n == 0 {
		acc
	} else {
		s.step(s, n - 1, acc + 2)
	}
}

// Same shape, but the base case reports the frame count instead of the
// accumulator: an indirect tail call must not grow the stack either.
fn run_depth(s Stepper, n Int, acc Int) Int {
	if n == 0 {
		internal.stack_depth() + acc
	} else {
		s.step(s, n - 1, acc)
	}
}

// --- Non-tail recursion --------------------------------------------------

// Frames live on the heap, so a non-tail recursion 100_000 deep is fine even
// though nothing can be reused: the `+ 1` has to happen after the call returns.
fn depth(n Int) Int {
	if n == 0 {
		0
	} else {
		1 + depth(n - 1)
	}
}

fn fib(n Int) Int {
	if n < 2 {
		n
	} else {
		fib(n - 1) + fib(n - 2)
	}
}

// Side effects inside a non-tail recursion: the prints must come back out in
// call order, i.e. deepest-first on the way down and shallowest-last.
fn countdown(n Int) Int {
	if n == 0 {
		println('countdown: liftoff')
		0
	} else {
		println('countdown: ${n}')
		n + countdown(n - 1)
	}
}

// --- Closures ------------------------------------------------------------

/** The returned closure carries `k` — the parameter outlives the call. */
fn adder(k Int) fn(Int) Int {
	fn(x) x + k
}

/** A binding born inside a match branch is captured the same as any other. */
fn pick(flag Bool) fn() String {
	match flag {
		True -> {
			tag = 'yes'
			fn() '${tag}!'
		}
		False -> {
			tag = 'no'
			fn() '${tag}?'
		}
	}
}

fn compose(f fn(Int) Int, g fn(Int) Int) fn(Int) Int {
	fn(x) g(f(x))
}

/** Recursion through a captured fn value: `f` is applied to its own result. */
fn twice(f fn(Int) Int, x Int) Int {
	f(f(x))
}

fn double(x Int) Int {
	x * 2
}

// A never-called `pub fn` is not an entrypoint and must not warn.
pub fn main() Nil {
	println('unreachable')
}

// --- Tail calls ----------------------------------------------------------

println('sum_to(1000000) = ${sum_to(1000000, 0)}')
println('self depth at 1000000: ${self_depth(1000000)}')

println('ping(500001) = ${ping(500001, 0)}')
println('mutual depth at 500000: ${mutual_depth_a(500000)}')

stepper = Stepper(step: fn(s Stepper, n Int, acc Int) run(s, n, acc))
println('indirect run(200000) = ${run(stepper, 200000, 0)}')

depth_stepper = Stepper(step: fn(s Stepper, n Int, acc Int) run_depth(s, n, acc))
println('indirect depth at 200000: ${run_depth(depth_stepper, 200000, 0)}')

// --- Non-tail recursion --------------------------------------------------

println('depth(100000) = ${depth(100000)}')
println('fib(20) = ${fib(20)}')
println('countdown total = ${countdown(3)}')

// --- Closure capture -----------------------------------------------------

// Capture is by value, at creation: rebinding `n` afterwards does not reach
// back into the closure that already took a copy.
n = 5
get = fn() n
n = 99
println('captured ${get()}, n is now ${n}')

add10 = adder(10)
add_neg = adder(-10)
println('adder: ${add10(1)}, ${add_neg(1)}, ${add10(add_neg(7))}')

println('pick: ${pick(True)()} ${pick(False)()}')

// A closure over closures.
add1_then_double = compose(adder(1), double)
double_then_add1 = compose(double, adder(1))
println('compose: ${add1_then_double(5)} vs ${double_then_add1(5)}')

// Closures in an array, applied by map; each one captured a different `k`.
steps = [adder(1), double, compose(adder(2), double)]
applied = array.map(steps, fn(f) f(10))
println('array of closures: ${string.inspect(applied)}')

// A closure held in a record field, reached from a match arm.
holder = Stepper(step: fn(s Stepper, n Int, acc Int) run(s, n, acc))
picked = match option.is_some(Some(holder)) {
	True -> run(holder, 3, 100)
	False -> 0
}
println('field closure from match arm: ${picked}')

// Recursion through a captured fn value, and a top-level fn passed by name.
println('twice: ${twice(add10, 1)} ${twice(double, 3)} ${twice(fn(x) x * x, 3)}')

// Constructors are values too.
const wrap = Some
println('constructor as value: ${string.inspect(array.map([1, 2], wrap))}')

println('done')
