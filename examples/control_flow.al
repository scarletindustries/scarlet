// Values, bindings, operators and blocks, pricing one line of an order.
// Almost everything in AL is an expression: `if` yields a value, and so does a
// `{ ... }` block — there is no `return`.

import al/array
import al/int
import al/string

/** Orders at or above this subtotal (in cents) ship free. */
const FREE_SHIPPING_CENTS Int = 5000

const FLAT_SHIPPING_CENTS = 599

/** Bulk discount for a quantity, as a whole percentage of the subtotal. */
fn discount_percent(units Int) Int {
	// The if/else chain is the body: its value is what the function returns.
	if units >= 100 {
		20
	} else if units >= 25 {
		10
	} else if units >= 10 {
		5
	} else {
		0
	}
}

// Returns the amount payable and the amount saved.
fn price_line(unit_cents Int, units Int) (Int, Int) {
	subtotal = unit_cents * units
	// `{ ... }` is AL's grouping form. Parentheses build tuples, they do not
	// group, so `(subtotal * pct) / 100` would not mean what you want.
	discount = { subtotal * discount_percent(units) } / 100
	payable = subtotal - discount
	shipping = if payable >= FREE_SHIPPING_CENTS { 0 } else { FLAT_SHIPPING_CENTS }
	(payable + shipping, discount)
}

// Integer division and `%` split cents into the two halves of a price.
fn money(cents Int) String {
	whole = cents / 100
	part = cents % 100
	pad = if part < 10 { '0' } else { '' }
	'USD ${whole}.${pad}${part}'
}

fn describe(unit_cents Int, units Int) String {
	// Tuple destructuring binds both halves of the pair at once.
	(total, saved) = price_line(unit_cents, units)
	note = if saved != 0 {
		', saved ${money(saved)}'
	} else {
		''
	}
	'${units} x ${money(unit_cents)} -> ${money(total)}${note}'
}

println(describe(1250, 1))
println(describe(1250, 10))
println(describe(1250, 100))

// Bindings are immutable, but rebinding a name shadows the earlier one from
// that point on. An annotation goes between the name and the `=`.
units = 4
units = units + 2
restock Int = int.max(units * 2, 12)
println('ordered ${units}, restocking ${restock}')

/* Boolean operators short-circuit, and `!` negates — again grouping with `{}`. */
in_stock = True
backordered = False
can_ship = in_stock && !{ backordered || restock > 100 }
println('can ship: ${can_ship}, credit ${-FLAT_SHIPPING_CENTS}')

// A range is an array, and it is end-exclusive: 0..5 is 0, 1, 2, 3, 4.
tiers = array.map(0..5, fn(i) discount_percent(i * 25))
println('tiers ${string.inspect(tiers)}')

// Indexing yields an Option, so `or` supplies the value for a missing slot.
println('largest tier ${tiers[4] or -1}, off the end ${tiers[9] or -1}')

// Binding a call's result to `Nil` says out loud that it is run for its effect.
Nil = println('order placed: ${string.length('shipped') > 0}')
