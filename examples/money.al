// Money without floats. al/decimal is exact decimal arithmetic on a scaled
// integer, so 0.1 + 0.2 is 0.3 and a tax calculation never loses a cent.
//
// Usd wraps Decimal in its own type so an amount can't be confused with a
// bare number. In a real project the wrapper would live in its own module as
// a `pub opaque type` — then nothing outside that module can construct one,
// and mixing currencies (Usd + Gbp) is a type error.

import al/decimal.{Decimal, Down}
import al/array
import al/option

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

fn show(u Usd) String {
	'\$${decimal.to_string(amount(u))}'
}

// Multiply exactly — the scale grows to hold every digit — then round back
// to cents. decimal.round is banker's rounding (half to even), the default
// throughout al/decimal.
fn tax(u Usd, rate Decimal) Usd {
	Usd(decimal.round(decimal.mul(amount(u), rate), 2))
}

// Split a total into n shares that sum back to exactly the total: every
// share gets the rounded-down amount, and the leftover cents go one each to
// the first shares. Dividing and multiplying back would either lose or
// invent money; this is the standard allocation fix.
fn split(total Usd, n Int) Array(Usd) {
	base = option.unwrap(
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

fn d(s String) Decimal {
	option.unwrap(decimal.parse(s), decimal.from_int(0))
}

// The classic float trap, avoided.
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
