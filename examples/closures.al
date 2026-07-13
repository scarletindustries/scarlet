// Closures and higher-order functions. A function is a value: it can capture
// the bindings around it, be stored in an array, be passed to another function,
// and be returned from one.

import al/array
import al/option
import al/string

type Line {
	sku String
	qty Int
	cents Int
}

/** Returns a function that adds `k`. The returned closure carries `k` with it. */
fn adder(k Int) fn(Int) Int {
	fn(x) x + k
}

// A predicate factory: `floor` outlives this call, inside the closure. The
// parameter is annotated because a field access needs to know the record it is
// reading — inference alone cannot pick a type out of `.qty`.
fn at_least(floor Int) fn(Line) Bool {
	fn(o Line) o.qty >= floor
}

/** A pipeline is an array of steps folded over the value being transformed. */
fn pipeline(steps Array(fn(Int) Int)) fn(Int) Int {
	fn(x) array.fold(steps, x, fn(acc, step) step(acc))
}

orders = [
	Line(sku: 'AL-1', qty: 4, cents: 1250),
	Line(sku: 'AL-2', qty: 12, cents: 800),
	Line(sku: 'AL-3', qty: 40, cents: 250),
	Line(sku: 'AL-4', qty: 7, cents: 9900),
]

// map / filter / fold read as a chain of small anonymous functions.
bulk = array.filter(orders, at_least(10))
lines = array.map(bulk, fn(o) '${o.sku} x${o.qty}')
revenue = array.fold(orders, 0, fn(sum, o) sum + o.qty * o.cents)

array.each(lines, fn(line) println('bulk: ${line}'))
println('revenue: ${revenue} cents over ${array.length(orders)} orders')

// A closure captures the *value* a name holds when the closure is made, so
// rebinding the name afterwards does not reach back into the closure.
base = 10
add_base = adder(base)
base = 100
println('add_base(1) = ${add_base(1)}, base is now ${base}')

// Steps are ordinary values, so a pipeline can be assembled at run time.
normalise = pipeline([adder(-1), fn(x) x * x, adder(100)])
println('normalise ${string.inspect(array.map(1..5, normalise))}')

// Spread splices one array into another; ranges are arrays, so they splice too.
qtys = [..array.map(orders, fn(o) o.qty), ..{ 1..3 }]
println('quantities ${string.inspect(qtys)}')

// `find` hands back an Option, `option.map` works inside it, and `or` supplies
// the fallback when there is nothing there.
biggest = array.find(orders, fn(o) o.qty > 30)
println('biggest sku: ${option.map(biggest, fn(o) o.sku) or 'none'}')

cheap = array.find(orders, fn(o) o.cents < 100)
println('cheap sku: ${option.map(cheap, fn(o) o.sku) or 'none'}')
