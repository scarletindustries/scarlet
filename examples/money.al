// Money without floats. A Decimal is `units * 10^(-scale)` — an exact scaled
// integer — so 0.1 + 0.2 is 0.3, and a tax line never loses a cent.
//
// Usd wraps Decimal in its own type so an amount cannot be confused with a
// bare number. In a real project the wrapper would be a `pub opaque type` in
// its own module; then nothing outside can construct one, and mixing
// currencies (Usd + Gbp) becomes a type error.

import al/array
import al/decimal.{Ceiling, Decimal, Down, Floor, HalfEven, HalfUp, Up}
import al/result

type Usd {
	amount Decimal
}

fn cents(n Int) Usd {
	Usd(decimal.new(n, 2))
}

fn amount(u Usd) Decimal {
	match u {
		Usd(d) -> d
	}
}

fn add(a Usd, b Usd) Usd {
	Usd(decimal.add(amount(a), amount(b)))
}

fn times(u Usd, n Int) Usd {
	Usd(decimal.mul(amount(u), decimal.from_int(n)))
}

// A Decimal prints as its representation, so always format through to_string.
fn show(u Usd) String {
	'\$${decimal.to_string(amount(u))}'
}

// `parse` is the honest way to write a literal amount: it is exact, and it
// fails on garbage instead of guessing.
fn d(s String) Decimal {
	result.unwrap(decimal.parse(s), decimal.from_int(0))
}

// Multiply exactly — the scale grows to hold every digit — then round back to
// cents. decimal.round is banker's rounding (half to even), the default
// throughout al/decimal.
fn tax(u Usd, rate Decimal) Usd {
	Usd(decimal.round(decimal.mul(amount(u), rate), 2))
}

// Split a total into n shares that sum back to exactly the total: every share
// gets the rounded-down amount, and the leftover cents go one each to the
// first shares. Dividing and multiplying back would either lose or invent
// money; this remainder-preserving allocation is the standard fix.
fn split(total Usd, n Int) Array(Usd) {
	base = result.unwrap(
		decimal.div_with(amount(total), decimal.from_int(n), 2, Down),
		decimal.from_int(0),
	)
	leftover = decimal.units(decimal.sub(amount(total), decimal.mul(base, decimal.from_int(n))))
	shares(base, leftover, n)
}

fn shares(base Decimal, extra Int, n Int) Array(Usd) {
	if n == 0 {
		[]
	} else if extra > 0 {
		[Usd(decimal.add(base, decimal.new(1, 2))), ..shares(base, extra - 1, n - 1)]
	} else {
		[Usd(base), ..shares(base, 0, n - 1)]
	}
}

type Item {
	name String
	price Usd
	qty Int
}

fn line_total(i Item) Usd {
	times(i.price, i.qty)
}

// The classic float trap, avoided. (examples/numbers.al shows the float side.)
println('0.1 + 0.2 = ${decimal.to_string(decimal.add(d('0.1'), d('0.2')))}')
println('')

items = [
	Item(name: 'coffee', price: cents(450), qty: 2),
	Item(name: 'bagel', price: cents(325), qty: 1),
	Item(name: 'orange juice', price: cents(599), qty: 3),
]

array.each(items, fn(i) println('${i.qty} x ${i.name} @ ${show(i.price)} = ${show(line_total(i))}'))

subtotal = array.fold(items, cents(0), fn(acc, i) add(acc, line_total(i)))
sales_tax = tax(subtotal, d('0.08875'))
total = add(subtotal, sales_tax)

println('')
println('subtotal ${show(subtotal)}')
println('tax      ${show(sales_tax)}')
println('total    ${show(total)}')

println('')
println('split 3 ways:')
array.each(split(total, 3), fn(share) println('  ${show(share)}'))
check = array.fold(split(total, 3), cents(0), fn(acc, s) add(acc, s))
println('shares sum back to ${show(check)}')
println('')

// == compares the *representation* — units and scale — so 1.50 and 1.5 look
// different to it even though they are the same number. Compare Decimals with
// decimal.eq or decimal.compare, never with ==.
println('1.50 == 1.5      ${d('1.50') == d('1.5')}')
println('decimal.eq       ${decimal.eq(d('1.50'), d('1.5'))}')
println('decimal.compare  ${decimal.compare(d('1.50'), d('1.5'))}')
println('')

// The rest of the comparison suite is numeric across scales too.
a = d('19.99')
b = d('5.00')
str = decimal.to_string
println('lt lte           ${decimal.lt(a, b)} ${decimal.lte(a, b)}')
println('gt gte           ${decimal.gt(a, b)} ${decimal.gte(a, b)}')
println('min max          ${str(decimal.min(a, b))} ${str(decimal.max(a, b))}')
println('neg abs          ${str(decimal.neg(a))} ${str(decimal.abs(decimal.neg(a)))}')

// Each predicate is shown on both sides so you can see it answer False too.
zero = decimal.new(0, 2)
println('is_zero          ${decimal.is_zero(zero)} ${decimal.is_zero(a)}')
println('is_negative      ${decimal.is_negative(decimal.neg(a))} ${decimal.is_negative(a)}')
println('is_positive      ${decimal.is_positive(a)} ${decimal.is_positive(decimal.neg(a))}')
println('')

// units and scale are the lossless serialization: store the pair, rebuild it
// with new. normalize drops trailing zeros when the stored scale means nothing.
stored = d('12.3400')
println('units, scale     ${decimal.units(stored)} @ ${decimal.scale(stored)}')
rebuilt = decimal.new(decimal.units(stored), decimal.scale(stored))
println('rebuilt          ${str(rebuilt)}')
println('normalize        ${str(decimal.normalize(stored))}')

// A negative scale multiplies out: new(5, -3) is 5000.
println('new(5, -3)       ${str(decimal.new(5, -3))}')
println('from_int         ${str(decimal.from_int(7))}')
println('')

// Floats are the lossy boundary, and crossing it is explicit in both
// directions. from_float can fail — the type is telling you this is an
// approximation you have to accept.
approx = result.unwrap(decimal.from_float(0.1 + 0.2, 2), zero)
println('to_float         ${decimal.to_float(d('19.99'))}')
println('from_float       ${str(approx)}')
println('from_float 20dp  ${result.is_err(decimal.from_float(1.5, 20))}')
println('')

// div needs a target scale, because an exact quotient need not exist at any.
// Both ways it can fail live in the error type instead of in a crash.
third = result.unwrap(decimal.div(d('10'), d('3'), 4), zero)
println('10 / 3 @ 4       ${str(third)}')

match decimal.div(d('10'), zero, 2) {
	Ok(q) -> println('divided by zero? ${decimal.to_string(q)}')
	Err(e) -> println('div by zero      ${e}')
}

match decimal.div(d('10'), d('3'), 19) {
	Ok(q) -> println('19 places?       ${decimal.to_string(q)}')
	Err(e) -> println('places > 18      ${e}')
}

println('')

// Every rounding mode on the same tie, 2.345 rounded to 2 places. HalfEven is
// the default (ties go to the even neighbour, so repeated rounding does not
// drift); HalfUp is schoolbook; Down/Up truncate toward/away from zero;
// Floor/Ceiling go toward negative/positive infinity — which is where the sign
// of the input changes the answer.
tie = d('2.345')
modes = [
	('HalfEven', HalfEven),
	('HalfUp', HalfUp),
	('Down', Down),
	('Up', Up),
	('Floor', Floor),
	('Ceiling', Ceiling),
]

array.each(modes, fn(m) match m {
	(name, mode) -> {
		up = decimal.to_string(decimal.round_with(tie, 2, mode))
		down = decimal.to_string(decimal.round_with(decimal.neg(tie), 2, mode))
		println('${name}\t 2.345 -> ${up}\t-2.345 -> ${down}')
	}
})
