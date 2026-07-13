// Numeric edges: Int, Float, Decimal, and the string boundaries between them.
//
// The three number types are deliberately *total* — no arithmetic operation
// traps, so every edge has a defined answer that this program pins:
//
//   Int   is exactly i64 and wraps on overflow. Division and modulo by zero
//         are defined (`x / 0 == 0`, `x % 0 == x`); division truncates toward
//         zero and the remainder takes the dividend's sign. Values outside the
//         48-bit NaN-boxed small-int range are boxed, which must be invisible.
//   Float canonicalizes every non-finite result to 0.0, so an overflow or a
//         division by zero collapses rather than producing inf/NaN.
//   Decimal is exact scaled-integer arithmetic; its `eq` compares *numbers*
//         where the built-in `==` compares the representation.

import al/binary.{Dec, Hex}
import al/decimal.{Down, Floor, HalfUp}
import al/float
import al/int
import al/result
import al/string

// --- Int: bounds, wrapping, boxing ---------------------------------------

// `min_value` is spelled as a subtraction: 9223372036854775808 does not lex.
println('bounds: ${int.max_value} ${int.min_value}')
println('wrap up: ${int.max_value + 1}')
println('wrap down: ${int.min_value - 1}')
println('wrap mul: ${1000000000000000 * 1000000}')
println('wrap neg: ${0 - int.min_value}')

// 2^47 - 1 is the largest small (unboxed) Int; crossing it boxes the value on
// the heap, which arithmetic, equality and printing must not notice.
small_max = 140737488355327
boxed = small_max + 1
println('boxing: ${small_max} ${boxed} ${boxed - 1 == small_max}')
println('boxed round trip: ${9223372036854775807} ${9223372036854775807 - 1 + 1}')
println('boxed equality: ${boxed * 2 == 281474976710656}')

// abs saturates rather than wrapping back to a negative.
println('abs: ${int.abs(-7)} ${int.abs(7)} ${int.abs(int.min_value)}')
println('max/min: ${int.max(3, 9)} ${int.min(3, 9)} ${int.max(-3, -9)}')
println('clamp: ${int.clamp(5, 0, 10)} ${int.clamp(50, 0, 10)} ${int.clamp(-50, 0, 10)}')

// --- Int: total division, truncation, remainder sign ----------------------

p7 = 7
n7 = -7
p2 = 2
n2 = -2

println('div by zero: ${p7 / 0} ${n7 / 0}')
println('mod by zero: ${p7 % 0} ${n7 % 0}')
println('trunc div: ${p7 / p2} ${n7 / p2} ${p7 / n2} ${n7 / n2}')
println('rem sign: ${p7 % p2} ${n7 % p2} ${p7 % n2} ${n7 % n2}')
println('exact div: ${6 / 3} ${6 % 3} ${1 / 2}')

// --- Float: formatting and non-finite collapse ----------------------------

// Printing matches Rust's f64 Display with a forced `.0` when the shortest
// round-trip form carries no fraction — so a whole float never looks like an
// Int.
println('float fmt: ${2.0} ${0.5} ${-0.0}')
println('float repr: ${0.1 + 0.2} ${1.0 / 3.0}')
println('float big: ${float.from_int(int.max_value)}')

// No exponent literal syntax, so a huge magnitude is built by multiplying.
// 10^160 is finite and prints in full; 10^320 is not representable, and every
// non-finite result — infinity or NaN — canonicalizes to 0.0.
big = 100000000000000000000.0 * 100000000000000000000.0
println('float huge: ${big * big * big * big}')
println('float overflow: ${big * big * big * big * big * big * big * big}')
println('float div zero: ${1.0 / 0.0} ${0.0 / 0.0} ${-1.0 / 0.0}')
println('float mod zero: ${1.5 % 0.0}')

println('floor: ${float.floor(2.7)} ${float.floor(-2.7)}')
println('ceil: ${float.ceil(2.1)} ${float.ceil(-2.1)}')
println('round ties: ${float.round(2.5)} ${float.round(3.5)} ${float.round(-1.5)}')
println('truncate: ${float.truncate(2.9)} ${float.truncate(-2.9)}')
println('float abs: ${float.abs(-2.5)} ${float.abs(0.0)}')
println('float max/min: ${float.max(1.5, 1.25)} ${float.min(1.5, 1.25)}')
println('float.to_string: ${float.to_string(1.0)} ${float.to_string(0.25)}')
println('from_int: ${float.from_int(3)} ${float.from_int(-3)}')

// --- Int <-> String round trips -------------------------------------------

// `int.to_string` goes one way; the other way runs through the binary parser,
// which is strict: it rejects a sign, an empty string, trailing junk, and
// anything that does not fit in an Int.
fn parse_int(s String) Result(Int, Nil) {
	binary.parse_int(binary.from_string(s), Dec)
}

fn parse_signed(s String) Result(Int, Nil) {
	b = binary.from_string(s)
	if binary.byte_size(b) > 0 && binary.byte_at(b, 0) == 45 {
		rest = binary.slice_bytes(b, 1, binary.byte_size(b) - 1)
		result.map(binary.parse_int(rest, Dec), fn(n) -n)
	} else {
		binary.parse_int(b, Dec)
	}
}

println('to_string: ${int.to_string(0)} ${int.to_string(-42)} ${int.to_string(int.min_value)}')
println('parse: ${string.inspect(parse_int('42'))} ${string.inspect(parse_int('0'))}')
signed = string.inspect(parse_int('-42'))
junk = string.inspect(parse_int('12abc'))
empty = string.inspect(parse_int(''))
println('parse rejects: ${signed} ${junk} ${empty}')
println('parse overflow: ${string.inspect(parse_int('9223372036854775808'))}')
println('signed round trip: ${string.inspect(parse_signed(int.to_string(-42)))}')
println('max round trip: ${string.inspect(parse_signed(int.to_string(int.max_value)))}')
println('hex: ${string.inspect(binary.to_string(binary.from_int_ascii(255, Hex)))}')
println('hex parse: ${string.inspect(binary.parse_int(<<'ff'>>, Hex))}')

// --- Decimal --------------------------------------------------------------

price = decimal.new(1999, 2)
println('decimal: ${decimal.to_string(price)}')
println('parts: units=${decimal.units(price)} scale=${decimal.scale(price)}')
println('from_int: ${decimal.to_string(decimal.from_int(7))}')

// Multiplication adds scales; addition aligns them.
println('mul: ${decimal.to_string(decimal.mul(price, decimal.from_int(3)))}')
println('add: ${decimal.to_string(decimal.add(price, decimal.new(1, 2)))}')
println('sub: ${decimal.to_string(decimal.sub(price, decimal.new(2000, 2)))}')
negated = decimal.to_string(decimal.neg(price))
absolute = decimal.to_string(decimal.abs(decimal.new(-1999, 2)))
println('neg/abs: ${negated} ${absolute}')

// Rounding is banker's by default: a tie goes to the even neighbour, so 2.345
// rounds down to 2.34 and 2.355 rounds up to 2.36.
down_tie = decimal.to_string(decimal.round(decimal.new(2345, 3), 2))
up_tie = decimal.to_string(decimal.round(decimal.new(2355, 3), 2))
println('round half even: ${down_tie} ${up_tie}')
println('round half up: ${decimal.to_string(decimal.round_with(decimal.new(2345, 3), 2, HalfUp))}')
toward_zero = decimal.round_with(decimal.new(-2349, 3), 2, Down)
toward_neg_inf = decimal.round_with(decimal.new(-2341, 3), 2, Floor)
println('round toward zero: ${decimal.to_string(toward_zero)}')
println('round toward -inf: ${decimal.to_string(toward_neg_inf)}')

div = decimal.div(decimal.from_int(10), decimal.from_int(3), 4)
println('div: ${string.inspect(result.map(div, decimal.to_string))}')
by_zero = decimal.div(decimal.from_int(1), decimal.from_int(0), 2)
println('div by zero: ${string.inspect(result.map(by_zero, decimal.to_string))}')

// Numeric comparison sees through scale; the built-in `==` does not, because it
// compares the representation (units and scale).
tenths = decimal.new(15, 1)
thousandths = decimal.new(1500, 3)
println('decimal.eq: ${decimal.eq(tenths, thousandths)} but == is ${tenths == thousandths}')
same = string.inspect(decimal.compare(tenths, thousandths))
bigger = string.inspect(decimal.compare(price, tenths))
println('compare: ${same} ${bigger}')
lt = decimal.lt(tenths, price)
lte = decimal.lte(tenths, thousandths)
gt = decimal.gt(price, tenths)
gte = decimal.gte(tenths, thousandths)
println('lt/lte/gt/gte: ${lt} ${lte} ${gt} ${gte}')
smallest = decimal.to_string(decimal.min(tenths, price))
largest = decimal.to_string(decimal.max(tenths, price))
println('min/max: ${smallest} ${largest}')
zero = decimal.is_zero(decimal.new(0, 4))
negative = decimal.is_negative(decimal.neg(price))
positive = decimal.is_positive(price)
println('is_zero/is_negative/is_positive: ${zero} ${negative} ${positive}')
println('normalize: ${decimal.to_string(decimal.normalize(decimal.new(1500, 3)))}')
parsed = string.inspect(result.map(decimal.parse('19.99'), decimal.units))
println('parse: ${parsed} ${string.inspect(decimal.parse('.5'))}')
println('to_float: ${decimal.to_float(price)}')
from_float = result.map(decimal.from_float(0.125, 3), decimal.to_string)
println('from_float: ${string.inspect(from_float)}')

// --- Comparisons across types ---------------------------------------------

// `<` is numeric only and both sides must be the same type — the type checker
// rejects `1 == 1.0` and `'a' < 'b'`, so the only cross-type comparison is one
// you write by hand.
println('int cmp: ${1 < 2} ${2 <= 2} ${3 > 4} ${4 >= 4} ${1 == 1} ${1 != 2}')
println('float cmp: ${1.5 < 2.5} ${-0.0 == 0.0} ${0.1 + 0.2 == 0.3}')
println('string eq: ${'abc' == 'abc'} ${'abc' != 'abd'}')
println('bool eq: ${True == True} ${True != False}')
println('mixed by hand: ${float.from_int(2) == 2.0} ${float.floor(2.5) == 2}')
