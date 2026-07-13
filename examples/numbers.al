// Int, Float, Bool — and the edges of each. There is no implicit conversion
// between Int and Float: crossing between them is always an explicit call.

import al/bool
import al/float
import al/int

// An Int is a signed 64-bit integer. Division truncates toward zero, and `%`
// takes the sign of the dividend (it is a remainder, not a modulus).
println('7 / 2     ${7 / 2}')
println('-7 / 2    ${-7 / 2}')
println('7 % 3     ${7 % 3}')
println('-7 % 3    ${-7 % 3}')

println('max       ${int.max(3, 9)}')
println('min       ${int.min(3, 9)}')
println('abs       ${int.abs(-42)}')
println('clamp     ${int.clamp(150, 0, 100)} ${int.clamp(-5, 0, 100)} ${int.clamp(50, 0, 100)}')

// 64-bit means 64-bit: arithmetic wraps at the ends of the range rather than
// promoting to a wider type. There is no arbitrary-precision integer here.
println('max_value ${int.max_value}')
println('min_value ${int.min_value}')
println('wrap      ${int.max_value + 1 == int.min_value}')

// One consequence: |min_value| is not representable, so int.abs saturates
// there instead of wrapping back to a negative number.
println('abs(min)  ${int.abs(int.min_value) == int.max_value}')

// Nothing above depends on how a value is stored. Small ints live in the value
// word and large ones spill to the heap, but that boundary is invisible: the
// arithmetic and the printing are identical either side of it.
println('boxed     ${140737488355327 + 1}')
println('boxed     ${9223372036854775807 - 1}')
println('')

// A Float is an IEEE-754 double. from_int is the only way in from Int; floor,
// ceil, round, and truncate are the ways out.
println('from_int  ${float.to_string(float.from_int(3))}')
println('floor     ${float.floor(2.7)} ${float.floor(-2.7)}')
println('ceil      ${float.ceil(2.1)} ${float.ceil(-2.1)}')
println('truncate  ${float.truncate(2.7)} ${float.truncate(-2.7)}')

// round breaks ties away from zero: 2.5 goes to 3, -2.5 goes to -3.
println('round     ${float.round(2.5)} ${float.round(-2.5)} ${float.round(2.4)}')

println('max/min   ${float.to_string(float.max(1.5, 2.5))} ${float.to_string(float.min(1.5, 2.5))}')
println('abs       ${float.to_string(float.abs(-1.5))}')

// Doubles cannot represent 0.1 exactly, so the classic surprise survives here
// as it does everywhere. For money and any other exact decimal work, use
// al/decimal — see examples/money.al.
println('0.1 + 0.2 ${0.1 + 0.2}')
println('== 0.3    ${0.1 + 0.2 == 0.3}')
println('1 / 3     ${1.0 / 3.0}')
println('')

// Bool is an ordinary two-constructor enum: `type Bool { True False }`. It is
// not an Int in disguise, and `if` accepts nothing else.
println('&& ||     ${True && False} ${True || False}')
println('!         ${!True}')
println('negate    ${bool.negate(False)}')
println('to_string ${bool.to_string(True) + '/' + bool.to_string(False)}')

// && and || short-circuit: the right side is not evaluated when the left has
// already decided the answer, which is what makes a guard like this safe.
fn safe_div(a Int, b Int) Bool {
	b != 0 && a / b > 1
}

println('guard     ${safe_div(10, 0)} ${safe_div(10, 3)}')
