//! Runtime golden tests for the pure-Scarlet stdlib modules under `scarlet/*`: each
//! drives a module's public functions through `al run` and pins the output.

mod common;
use common::{check_ok, check_rejects, run_outputs};

#[test]
#[ignore = "needs the VM"]
fn stdlib_option() {
    run_outputs(
        "import scarlet/option\n\
         pub fn main() {\n\
         \tprintln(option.map(Some(5), fn(x) x * 2))\n\
         \tprintln(option.unwrap(None, 99))\n\
         \tprintln(option.is_some(Some(1)))\n\
         }\n",
        "Some(10)\n99\nTrue\n",
    );
    // then: None short-circuits without calling f.
    run_outputs(
        "import scarlet/option\n\
         pub fn main() {\n\
         \tprintln(option.then(Some(5), fn(x) Some(x + 1)))\n\
         \tprintln(option.then(None, fn(x) Some(x + 1)))\n\
         }\n",
        "Some(6)\nNone\n",
    );
    // or_else: None calls the fallback thunk.
    run_outputs(
        "import scarlet/option\n\
         pub fn main() {\n\
         \tprintln(option.or_else(Some(1), fn() Some(2)))\n\
         \tprintln(option.or_else(None, fn() Some(2)))\n\
         }\n",
        "Some(1)\nSome(2)\n",
    );
    run_outputs(
        "import scarlet/option\n\
         pub fn main() {\n\
         \tprintln(option.is_none(None))\n\
         \tprintln(option.is_none(Some(1)))\n\
         }\n",
        "True\nFalse\n",
    );
    // The remaining arms.
    run_outputs(
        "import scarlet/option\n\
         pub fn main() {\n\
         \tprintln(option.map(None, fn(x) x * 2))\n\
         \tprintln(option.unwrap(Some(5), 99))\n\
         \tprintln(option.is_some(None))\n\
         }\n",
        "None\n5\nFalse\n",
    );
}

#[test]
#[ignore = "needs the VM"]
fn stdlib_result() {
    run_outputs(
        "import scarlet/result\n\
         pub fn main() {\n\
         \tprintln(result.map(Ok(5), fn(x) x + 1))\n\
         \tprintln(result.map_err(Err('bad'), fn(e) '${e}!'))\n\
         }\n",
        "Ok(6)\nErr(bad!)\n",
    );
    // then: Err short-circuits, propagating the original error untouched.
    run_outputs(
        "import scarlet/result\n\
         pub fn main() {\n\
         \tprintln(result.then(Ok(5), fn(x) Ok(x + 1)))\n\
         \tprintln(result.then(Err('e'), fn(x) Ok(x + 1)))\n\
         }\n",
        "Ok(6)\nErr(e)\n",
    );
    // unwrap: only defined for Result(a, Nil) — the Err carries nothing to
    // discard, so it collapses to the default.
    run_outputs(
        "import scarlet/result\n\
         pub fn main() {\n\
         \tprintln(result.unwrap(Ok(5), 0))\n\
         \tprintln(result.unwrap(Err(Nil), 99))\n\
         }\n",
        "5\n99\n",
    );
    // replace_err: the Nil error is swapped for a meaningful one; Ok passes
    // through untouched.
    run_outputs(
        "import scarlet/result\n\
         pub fn main() {\n\
         \tprintln(result.replace_err(Ok(5), 'boom'))\n\
         \tprintln(result.replace_err(Err(Nil), 'boom'))\n\
         }\n",
        "Ok(5)\nErr(boom)\n",
    );
    run_outputs(
        "import scarlet/result\n\
         pub fn main() {\n\
         \tprintln(result.is_ok(Ok(1)))\n\
         \tprintln(result.is_ok(Err('x')))\n\
         \tprintln(result.is_err(Err('x')))\n\
         \tprintln(result.is_err(Ok(1)))\n\
         }\n",
        "True\nFalse\nTrue\nFalse\n",
    );
    // The remaining arms.
    run_outputs(
        "import scarlet/result\n\
         pub fn main() {\n\
         \tprintln(result.map(Err('e'), fn(x) x + 1))\n\
         \tprintln(result.map_err(Ok(5), fn(e) e))\n\
         }\n",
        "Err(e)\nOk(5)\n",
    );
}

#[test]
#[ignore = "needs the VM"]
fn stdlib_resource() {
    // Acquire, use, release — in that order — and the use's value comes back.
    run_outputs(
        "import scarlet/resource\n\
         pub fn main() {\n\
         \tr = resource.with(fn() 10, fn(c) println('release ${c}'), fn(c) c * 2)\n\
         \tprintln(r)\n\
         }\n",
        "release 10\n20\n",
    );
    // try_with: Ok acquires, uses, releases; Err short-circuits, releasing
    // and using nothing.
    run_outputs(
        "import scarlet/resource\n\
         pub fn main() {\n\
         \tprintln(resource.try_with(fn() Ok(1), fn(c) println('release ${c}'), fn(c) c + 1))\n\
         \tprintln(resource.try_with(fn() Err(Nil), fn(_) println('never'), fn(c Int) c))\n\
         }\n",
        "release 1\nOk(2)\nErr(Nil)\n",
    );
    // The backpass idiom: the rest of the block is `next`, and a trailing
    // backpass passes an empty (Nil) continuation.
    run_outputs(
        "import scarlet/resource\n\
         fn demo() Nil {\n\
         \tconn <- resource.with(fn() 7, fn(_) Nil)\n\
         \tprintln('conn ${conn}')\n\
         }\n\
         pub fn main() {\n\
         \tprintln(demo())\n\
         }\n",
        "conn 7\nNil\n",
    );
}

#[test]
#[ignore = "needs the VM"]
fn stdlib_array() {
    run_outputs(
        "import scarlet/array\n\
         pub fn main() {\n\
         \tprintln(array.map([1, 2, 3], fn(x) x * 10))\n\
         \tprintln(array.filter([1, 2, 3, 4], fn(x) x > 2))\n\
         \tprintln(array.fold([1, 2, 3, 4], 0, fn(a, b) a + b))\n\
         \tprintln(array.reverse([1, 2, 3]))\n\
         \tprintln(array.contains([1, 2, 3], 2))\n\
         }\n",
        "[10, 20, 30]\n[3, 4]\n10\n[3, 2, 1]\nTrue\n",
    );
    run_outputs(
        "import scarlet/array\n\
         pub fn main() {\n\
         \tprintln(array.find([1, 2, 3], fn(x) x > 1))\n\
         \tprintln(array.find([1, 2, 3], fn(x) x > 9))\n\
         }\n",
        "Some(2)\nNone\n",
    );
    run_outputs(
        "import scarlet/array\n\
         pub fn main() {\n\
         \tprintln(array.any([1, 2, 3], fn(x) x > 2))\n\
         \tprintln(array.any([1, 2, 3], fn(x) x > 9))\n\
         \tprintln(array.all([2, 4, 6], fn(x) x % 2 == 0))\n\
         \tprintln(array.all([2, 3], fn(x) x % 2 == 0))\n\
         }\n",
        "True\nFalse\nTrue\nFalse\n",
    );
    // Empty-array base cases.
    run_outputs(
        "import scarlet/array\n\
         pub fn main() {\n\
         \tprintln(array.map([], fn(x) x * 10))\n\
         \tprintln(array.filter([], fn(x) x > 2))\n\
         \tprintln(array.fold([], 0, fn(a, b) a + b))\n\
         \tprintln(array.length([]))\n\
         \tprintln(array.reverse([]))\n\
         \tprintln(array.contains([1, 2], 9))\n\
         }\n",
        "[]\n[]\n0\n0\n[]\nFalse\n",
    );
    // `length` wraps a @vm builtin in a plain pub fn, so it stays first-class.
    run_outputs(
        "import scarlet/array\n\
         pub fn main() {\n\
         \tf = array.length\n\
         \tprintln(f([1, 2, 3]))\n\
         \tprintln(array.map([[1], [2, 3], []], array.length))\n\
         }\n",
        "3\n[1, 2, 0]\n",
    );
}

run_case! {
    stdlib_int: (
        "import scarlet/int\n\
         pub fn main() {\n\
         \tprintln(int.max(3, 7))\n\
         \tprintln(int.min(3, 7))\n\
         \tprintln(int.abs(0 - 5))\n\
         \tprintln(int.abs(0 - 9223372036854775807 - 1))\n\
         \tprintln(int.clamp(99, 0, 10))\n\
         \tprintln(int.clamp(5, 10, 0))\n\
         \tprintln(int.to_string(42))\n\
         }\n",
        "7\n3\n5\n9223372036854775807\n10\n0\n42\n",
    ),

    stdlib_bool: (
        "import scarlet/bool\n\
         pub fn main() {\n\
         \tprintln(bool.negate(True))\n\
         \tprintln(bool.to_string(False))\n\
         }\n",
        "False\nFalse\n",
    ),
}

#[test]
#[ignore = "needs the VM"]
fn stdlib_decimal() {
    // Scale propagation: add aligns to the wider scale, mul sums scales.
    run_outputs(
        "import scarlet/decimal\n\
         pub fn main() {\n\
         \ta = decimal.new(1999, 2)\n\
         \tprintln(decimal.to_string(decimal.add(a, decimal.new(1, 2))))\n\
         \tprintln(decimal.to_string(decimal.sub(a, decimal.new(1, 2))))\n\
         \tprintln(decimal.to_string(decimal.mul(a, decimal.from_int(3))))\n\
         \tprintln(decimal.to_string(decimal.mul(decimal.new(15, 1), decimal.new(25, 2))))\n\
         \tprintln(decimal.units(a))\n\
         \tprintln(decimal.scale(a))\n\
         \tprintln(decimal.to_string(decimal.new(5, 0 - 3)))\n\
         }\n",
        "20.00\n19.98\n59.97\n0.375\n1999\n2\n5000\n",
    );
    // HalfEven is the default; a wider target scale zero-pads.
    run_outputs(
        "import scarlet/decimal.{HalfUp, Down}\n\
         pub fn main() {\n\
         \tx = decimal.new(2345, 3)\n\
         \tprintln(decimal.to_string(decimal.round(x, 2)))\n\
         \tprintln(decimal.to_string(decimal.round(decimal.new(125, 3), 2)))\n\
         \tprintln(decimal.to_string(decimal.round(decimal.new(135, 3), 2)))\n\
         \tprintln(decimal.to_string(decimal.round_with(x, 2, HalfUp)))\n\
         \tprintln(decimal.to_string(decimal.round_with(decimal.neg(x), 2, HalfUp)))\n\
         \tprintln(decimal.to_string(decimal.round_with(x, 2, Down)))\n\
         \tprintln(decimal.to_string(decimal.round(x, 5)))\n\
         }\n",
        "2.34\n0.12\n0.14\n2.35\n-2.35\n2.34\n2.34500\n",
    );
    run_outputs(
        "import scarlet/decimal\n\
         import scarlet/result\n\
         pub fn main() {\n\
         \tbill = decimal.new(10000, 2)\n\
         \tprintln(result.map(decimal.div(bill, decimal.from_int(3), 2), decimal.to_string))\n\
         \tprintln(result.map(decimal.div(decimal.from_int(1), decimal.from_int(8), 4), decimal.to_string))\n\
         \tprintln(decimal.div(bill, decimal.from_int(0), 2))\n\
         }\n",
        "Ok(33.33)\nOk(0.1250)\nErr(DividedByZero)\n",
    );
    // Half-tie rounding with divisor units near Int max: `2 * r` would wrap
    // at remainders past 2^62. The wrap-free `r` vs `d - r` comparison must
    // agree with the small-scale equivalent of the same fraction.
    run_outputs(
        "import scarlet/decimal.{HalfUp, HalfEven}\n\
         import scarlet/result\n\
         pub fn main() {\n\
         \tbig = decimal.from_int(9000000000000000000)\n\
         \tshow = fn(q) { result.map(q, decimal.to_string) }\n\
         \tprintln(show(decimal.div_with(decimal.from_int(5000000000000000000), big, 0, HalfUp)))\n\
         \tprintln(show(decimal.div_with(decimal.from_int(5), decimal.from_int(9), 0, HalfUp)))\n\
         \tprintln(show(decimal.div_with(decimal.from_int(5000000000000000000), big, 0, HalfEven)))\n\
         \tprintln(show(decimal.div_with(decimal.from_int(5), decimal.from_int(9), 0, HalfEven)))\n\
         \tprintln(show(decimal.div_with(decimal.from_int(4000000000000000000), big, 0, HalfUp)))\n\
         \tprintln(show(decimal.div_with(decimal.from_int(0 - 5000000000000000000), big, 0, HalfUp)))\n\
         \tprintln(show(decimal.div_with(decimal.from_int(8999999999999999999), big, 0, HalfEven)))\n\
         \tprintln(show(decimal.div_with(decimal.from_int(4500000000000000000), big, 0, HalfUp)))\n\
         \tprintln(show(decimal.div_with(decimal.from_int(4500000000000000000), big, 0, HalfEven)))\n\
         }\n",
        "Ok(1)\nOk(1)\nOk(1)\nOk(1)\nOk(0)\nOk(-1)\nOk(1)\nOk(1)\nOk(0)\n",
    );
    // Comparison is scale-blind (1.5 == 1.500); normalize strips the zeros.
    run_outputs(
        "import scarlet/decimal\n\
         pub fn main() {\n\
         \ta = decimal.new(15, 1)\n\
         \tb = decimal.new(1500, 3)\n\
         \tprintln(decimal.eq(a, b))\n\
         \tprintln(decimal.compare(decimal.new(0 - 1, 2), decimal.from_int(0)))\n\
         \tprintln(decimal.lt(a, decimal.new(16, 1)))\n\
         \tprintln(decimal.to_string(decimal.max(a, decimal.new(2, 0))))\n\
         \tprintln(decimal.scale(decimal.normalize(b)))\n\
         \tprintln(decimal.is_negative(decimal.neg(a)))\n\
         \tprintln(decimal.is_zero(decimal.new(0, 5)))\n\
         }\n",
        "True\nLt\nTrue\n2\n1\nTrue\nTrue\n",
    );
    // parse keeps the written scale and rejects malformed or Int-overflowing
    // input instead of wrapping. -0.05 is the sign-on-zero-whole-part case.
    run_outputs(
        "import scarlet/decimal\n\
         import scarlet/result\n\
         pub fn main() {\n\
         \tprintln(result.map(decimal.parse('19.99'), decimal.to_string))\n\
         \tprintln(result.map(decimal.parse('-0.05'), decimal.to_string))\n\
         \tprintln(result.map(decimal.parse('+1.50'), decimal.to_string))\n\
         \tprintln(result.map(decimal.parse('42'), decimal.to_string))\n\
         \tprintln(decimal.parse('1.'))\n\
         \tprintln(decimal.parse('.5'))\n\
         \tprintln(decimal.parse('1.2.3'))\n\
         \tprintln(decimal.parse(''))\n\
         \tprintln(decimal.parse('-'))\n\
         \tprintln(decimal.parse('9223372036854775807.99'))\n\
         \tprintln(result.map(decimal.parse('92233720368547758.07'), decimal.units))\n\
         }\n",
        "Ok(19.99)\nOk(-0.05)\nOk(1.50)\nOk(42)\nErr(Nil)\nErr(Nil)\nErr(Nil)\nErr(Nil)\nErr(Nil)\nErr(Nil)\nOk(9223372036854775807)\n",
    );
    // Negative `places` rounds to a multiple of 10^|places| at scale 0.
    run_outputs(
        "import scarlet/decimal\n\
         import scarlet/result\n\
         pub fn main() {\n\
         \tprintln(decimal.to_string(decimal.round(decimal.new(1250, 0), 0 - 2)))\n\
         \tprintln(decimal.to_string(decimal.round(decimal.new(12345, 1), 0 - 1)))\n\
         \tprintln(decimal.scale(decimal.round(decimal.new(1250, 0), 0 - 2)))\n\
         \tprintln(result.map(decimal.div(decimal.from_int(1234), decimal.from_int(1), 0 - 2), decimal.to_string))\n\
         }\n",
        "1200\n1230\n0\nOk(1200)\n",
    );
    // Dropping more than 18 digits: 10^k would wrap, so the quotient regime
    // is {-1, 0, 1} and is computed without it. div is Err(ScaleOutOfRange)
    // once |places| or the rescale exponent exceeds 18.
    run_outputs(
        "import scarlet/decimal.{HalfUp, Up}\n\
         import scarlet/result\n\
         pub fn main() {\n\
         \tprintln(decimal.to_string(decimal.round(decimal.new(123456789012345678, 18), 0 - 1)))\n\
         \tprintln(result.map(decimal.div(decimal.new(9000000000000000000, 18), decimal.from_int(1), 0 - 1), decimal.to_string))\n\
         \tprintln(decimal.to_string(decimal.round(decimal.new(1234, 0), 0 - 19)))\n\
         \tprintln(decimal.to_string(decimal.round(decimal.new(19, 1), 0 - 18)))\n\
         \tprintln(decimal.to_string(decimal.round_with(decimal.new(1, 18), 0 - 1, Up)))\n\
         \tprintln(decimal.to_string(decimal.round_with(decimal.new(5000000000000000000, 18), 0 - 1, HalfUp)))\n\
         \tprintln(decimal.to_string(decimal.round(decimal.new(5000000000000000000, 18), 0 - 1)))\n\
         \tprintln(decimal.div(decimal.from_int(1), decimal.from_int(1), 0 - 19))\n\
         \tprintln(decimal.div(decimal.from_int(1), decimal.new(1, 18), 21))\n\
         \tprintln(decimal.div(decimal.new(1, 12), decimal.from_int(1), 21))\n\
         }\n",
        "0\nOk(10)\n0\n0\n10\n10\n0\nErr(ScaleOutOfRange)\nErr(ScaleOutOfRange)\nErr(ScaleOutOfRange)\n",
    );
    // Float bridges are lossy; from_float is Err(Nil) rather than wrapping.
    run_outputs(
        "import scarlet/decimal\n\
         import scarlet/result\n\
         pub fn main() {\n\
         \tprintln(decimal.to_float(decimal.new(25, 1)))\n\
         \tprintln(result.map(decimal.from_float(2.5, 2), decimal.to_string))\n\
         \tprintln(decimal.from_float(10000000000000000000.0, 2))\n\
         \tprintln(decimal.from_float(0.5, 19))\n\
         \tprintln(result.map(decimal.from_float(149.0, 0 - 1), decimal.to_string))\n\
         }\n",
        "2.5\nOk(2.50)\nErr(Nil)\nErr(Nil)\nOk(150)\n",
    );
}

#[test]
#[ignore = "needs the VM"]
fn stdlib_binary() {
    run_outputs(
        "import scarlet/binary\n\
         pub fn main() {\n\
         \tb = binary.from_string('hi')\n\
         \tprintln(binary.to_string(b))\n\
         \tprintln(binary.bit_size(b))\n\
         \tprintln(binary.byte_size(b))\n\
         \tprintln(b)\n\
         }\n",
        "Ok(hi)\n16\n2\n<<104, 105>>\n",
    );
    run_outputs(
        "import scarlet/binary\n\
         pub fn main() {\n\
         \tb = binary.from_string('ABC')\n\
         \tprintln(binary.slice_bits(b, 8, 8))\n\
         \tprintln(binary.slice_bits(b, 0, 99))\n\
         \tjoined = binary.append(binary.from_string('AB'), binary.from_string('C'))\n\
         \tprintln(binary.to_string(joined))\n\
         \tprintln(binary.bit_size(binary.slice_bits(b, 0, 5) or binary.from_string('')))\n\
         }\n",
        "Ok(<<66>>)\nErr(Nil)\nOk(ABC)\n5\n",
    );
    // Op::BinReadUtf8 decodes one codepoint, not one byte: [195, 169] is 'é',
    // so a byte-wise read would bind 195 instead of 233.
    run_outputs(
        "import scarlet/binary\n\
         pub fn main() {\n\
         \tr = match <<195, 169>> {\n\
         \t\t<<c:utf8, ..>> -> c\n\
         \t\t_ -> 0\n\
         \t}\n\
         \tprintln(r)\n\
         }\n",
        "233\n",
    );
    // Op::BinTake — `:bytes(n)` splices the first n bytes. 65,66,67
    // discriminates both the count and prefix-vs-suffix.
    run_outputs(
        "import scarlet/binary\n\
         import scarlet/string\n\
         pub fn main() {\n\
         \tsrc = binary.from_string('ABCDE')\n\
         \tprintln(string.inspect(<<src:bytes(3)>>))\n\
         }\n",
        "<<65, 66, 67>>\n",
    );
    // to_string Err branches: undecodable UTF-8, and bit-unaligned input.
    run_outputs(
        "import scarlet/binary\n\
         pub fn main() {\n\
         \tprintln(binary.to_string(<<255>>))\n\
         \tprintln(binary.to_string(<<1:4>>))\n\
         }\n",
        "Err(Nil)\nErr(Nil)\n",
    );
    // byte_size rounds up: a 4-bit binary occupies 1 byte, not 0.
    run_outputs(
        "import scarlet/binary\n\
         pub fn main() {\n\
         \tprintln(binary.byte_size(<<1:4>>))\n\
         }\n",
        "1\n",
    );
    // A negative offset takes the `at < 0` Err branch, not the OOB one.
    run_outputs(
        "import scarlet/binary\n\
         pub fn main() {\n\
         \tprintln(binary.slice_bits(binary.from_string('ABC'), 0 - 1, 8))\n\
         }\n",
        "Err(Nil)\n",
    );
    check_rejects(
        "import scarlet/net/socket.{Socket}\n\
         fn f(c Socket) Nil { socket.write(c, 'nope') or Nil }\n",
        "Type mismatch",
    );
}

/// The unit a window is measured in lives in the function's name, and the two
/// names differ by a factor of eight. Before this, `slice` was bit-indexed and
/// `byte_size` answered in bytes, so a byte-indexed caller of `slice` read an
/// eighth of the window it asked for and was told nothing: `slice(b, 0, 2)`
/// answered `Ok(<<0:size(2)>>)`, a valid binary, rather than the first two
/// bytes. The old spelling no longer exists, so that call is now a compile
/// error rather than a wrong answer.
#[test]
#[ignore = "needs the VM"]
fn stdlib_binary_slice_units() {
    // The name `slice` is gone. This is the guard on reintroducing it: a
    // function of that name could only pick one of the two units, and the
    // callers meaning the other one would keep compiling.
    check_rejects(
        "import scarlet/binary\n\
         pub fn main() { println(binary.slice(<<1, 2, 3, 4, 5>>, 0, 2)) }\n",
        // Spelled out: bare `slice` is a substring of `slice_bits`, so the
        // loose form would pass on a diagnostic about a different name.
        "has no member 'slice'",
    );
    // Both units, over the same measured windows from T-208, side by side.
    run_outputs(
        "import scarlet/binary\n\
         pub fn main() {\n\
         \tb = <<1, 2, 3, 4, 5>>\n\
         \tprintln(binary.slice_bytes(b, 0, 2))\n\
         \tprintln(binary.slice_bits(b, 0, 2))\n\
         \tprintln(binary.slice_bytes(b, 1, 2))\n\
         \tprintln(binary.slice_bits(b, 8, 16))\n\
         \tprintln(binary.slice_bytes(b, 0, 5))\n\
         \tprintln(binary.slice_bits(b, 0, 40))\n\
         }\n",
        "Ok(<<1, 2>>)\nOk(<<0:size(2)>>)\nOk(<<2, 3>>)\nOk(<<2, 3>>)\n\
         Ok(<<1, 2, 3, 4, 5>>)\nOk(<<1, 2, 3, 4, 5>>)\n",
    );
    // T-214: every out-of-range case that used to collapse to `<<>>` — one
    // byte over, wholly past the end, negative offset, negative length. The
    // last two are why a Gleam `slice(b, byte_size(b), -n)` cannot be
    // transliterated: it is an error here, not a backwards window.
    run_outputs(
        "import scarlet/binary\n\
         pub fn main() {\n\
         \tb = <<1, 2, 3, 4, 5>>\n\
         \tprintln(binary.slice_bytes(b, 4, 1))\n\
         \tprintln(binary.slice_bytes(b, 4, 2))\n\
         \tprintln(binary.slice_bytes(b, 5, 1))\n\
         \tprintln(binary.slice_bytes(b, 0 - 1, 2))\n\
         \tprintln(binary.slice_bytes(b, 5, 0 - 4))\n\
         }\n",
        "Ok(<<5>>)\nErr(Nil)\nErr(Nil)\nErr(Nil)\nErr(Nil)\n",
    );
    // `byte_size` rounds up, so the last byte of a 12-bit binary is not a
    // whole byte and is not addressable by slice_bytes. Reaching it is what
    // slice_bits is for.
    run_outputs(
        "import scarlet/binary\n\
         pub fn main() {\n\
         \tb = <<1, 2:4>>\n\
         \tprintln(binary.byte_size(b))\n\
         \tprintln(binary.slice_bytes(b, 0, 2))\n\
         \tprintln(binary.slice_bits(b, 8, 4))\n\
         }\n",
        "2\nErr(Nil)\nOk(<<2:size(4)>>)\n",
    );
    // drop_bytes is total because it is given a position, not a length: there
    // is no requested size for the answer to fall short of. Past the end the
    // remainder really is empty, and it reaches the trailing bits that
    // slice_bytes cannot.
    run_outputs(
        "import scarlet/binary\n\
         import scarlet/string\n\
         pub fn main() {\n\
         \tb = <<1, 2, 3, 4, 5>>\n\
         \tprintln(string.inspect(binary.drop_bytes(b, 0)))\n\
         \tprintln(string.inspect(binary.drop_bytes(b, 3)))\n\
         \tprintln(string.inspect(binary.drop_bytes(b, 5)))\n\
         \tprintln(string.inspect(binary.drop_bytes(b, 900)))\n\
         \tprintln(string.inspect(binary.drop_bytes(b, 0 - 2)))\n\
         \tprintln(binary.bit_size(binary.drop_bytes(<<1, 2:4>>, 1)))\n\
         }\n",
        "<<1, 2, 3, 4, 5>>\n<<4, 5>>\n<<>>\n<<>>\n<<1, 2, 3, 4, 5>>\n4\n",
    );
    // split_at_bytes is total because it is given a position, not a length.
    // append(prefix, suffix) == b at every at, including the ones slice_bytes
    // would refuse. The mid-cut is the control: a clamp-only stub would pass
    // the four edges and fail this one.
    run_outputs(
        "import scarlet/binary\n\
         import scarlet/string\n\
         pub fn main() {\n\
         \tb = <<1, 2, 3, 4, 5>>\n\
         \tshow = fn(at) {\n\
         \t\tpair = binary.split_at_bytes(b, at)\n\
         \t\tprintln(string.inspect(pair.0))\n\
         \t\tprintln(string.inspect(pair.1))\n\
         \t\tprintln(binary.append(pair.0, pair.1) == b)\n\
         \t}\n\
         \tshow(0 - 3)\n\
         \tshow(0)\n\
         \tshow(2)\n\
         \tshow(5)\n\
         \tshow(900)\n\
         }\n",
        "<<>>\n<<1, 2, 3, 4, 5>>\nTrue\n\
         <<>>\n<<1, 2, 3, 4, 5>>\nTrue\n\
         <<1, 2>>\n<<3, 4, 5>>\nTrue\n\
         <<1, 2, 3, 4, 5>>\n<<>>\nTrue\n\
         <<1, 2, 3, 4, 5>>\n<<>>\nTrue\n",
    );
}

#[test]
#[ignore = "needs the VM"]
fn stdlib_binary_concat() {
    // Empty input is the identity of append; a first-element stub would
    // pass the singleton and fail the rest.
    run_outputs(
        "import scarlet/binary\n\
         import scarlet/string\n\
         pub fn main() {\n\
         \tprintln(string.inspect(binary.concat([])))\n\
         \tprintln(string.inspect(binary.concat([<<1, 2>>])))\n\
         \tprintln(string.inspect(binary.concat([<<1, 2>>, <<3>>, <<4, 5>>])))\n\
         \tprintln(string.inspect(binary.concat([<<>>, <<9>>, <<>>])))\n\
         }\n",
        "<<>>\n<<1, 2>>\n<<1, 2, 3, 4, 5>>\n<<9>>\n",
    );
    // concat(split_at_bytes(b, at)) == b. A first-only or reverse-fold stub
    // fails here; the three-part case above already rules out those, and
    // this pins the identity against a different binary than the literals.
    run_outputs(
        "import scarlet/binary\n\
         pub fn main() {\n\
         \tb = <<1, 2, 3, 4, 5>>\n\
         \tpair = binary.split_at_bytes(b, 2)\n\
         \tprintln(binary.concat([pair.0, pair.1]) == b)\n\
         }\n",
        "True\n",
    );
}

#[test]
#[ignore = "needs the VM"]
fn stdlib_binary_byte_at() {
    // byte_at is -1 out of bounds on both sides; a view reads through its
    // offset.
    run_outputs(
        "import scarlet/binary\n\
         pub fn main() {\n\
         \tb = binary.from_string('AZ')\n\
         \tprintln(binary.byte_at(b, 0))\n\
         \tprintln(binary.byte_at(b, 1))\n\
         \tprintln(binary.byte_at(b, 2))\n\
         \tprintln(binary.byte_at(b, 0 - 1))\n\
         \ttail = match b {\n\
         \t\t<<_, ..rest>> -> rest\n\
         \t\t_ -> b\n\
         \t}\n\
         \tprintln(binary.byte_at(tail, 0))\n\
         }\n",
        "65\n90\n-1\n-1\n90\n",
    );
}

#[test]
fn stdlib_http_builtins() {
    // These pin the call-site types of the native h1 ops; behaviour is golden
    // tested in tests/programs/http_parse.scrl.
    check_ok(
        "import scarlet/binary\n\
         import scarlet/http/h1.{Done, NeedMore, Bad, Http10, Http11}\n\
         pub fn main() {\n\
         \tr = match h1.parse_request(binary.from_string('GET / HTTP/1.1\\r\\n\\r\\n'), 0) {\n\
         \t\tDone(_, _, version, _, _, consumed) ->\n\
         \t\t\tmatch version { Http10 -> 10 Http11 -> 11 } + consumed\n\
         \t\tNeedMore -> 0\n\
         \t\tBad(s) -> s\n\
         \t}\n\
         \tprintln(r)\n\
         }\n",
    );
    check_ok(
        "import scarlet/binary\n\
         import scarlet/http/h1.{Done, NoBody, Length, Chunked, Invalid}\n\
         pub fn main() {\n\
         \tr = match h1.parse_request(binary.from_string('GET / HTTP/1.1\\r\\n\\r\\n'), 0) {\n\
         \t\tDone(_, _, _, hdrs, _, _) -> match h1.framing(hdrs) {\n\
         \t\t\tNoBody -> 0\n\
         \t\t\tLength(n) -> n\n\
         \t\t\tChunked -> 0 - 2\n\
         \t\t\tInvalid(s) -> s\n\
         \t\t}\n\
         \t\t_ -> 0 - 1\n\
         \t}\n\
         \tprintln(r)\n\
         }\n",
    );
    check_ok(
        "import scarlet/binary\n\
         import scarlet/http/h1.{ChunkedDone, ChunkedNeedMore, ChunkedBad}\n\
         import scarlet/http/headers\n\
         pub fn main() {\n\
         \tr = match h1.chunk_decode(binary.from_string('5\\r\\nhello\\r\\n0\\r\\n\\r\\n'), 0, 1024) {\n\
         \t\tChunkedDone(body, trailers, consumed) -> {\n\
         \t\t\thas_sum = headers.has(trailers, binary.from_string('x-sum'))\n\
         \t\t\tif has_sum { consumed } else { binary.byte_size(body) + consumed }\n\
         \t\t}\n\
         \t\tChunkedNeedMore -> 0\n\
         \t\tChunkedBad(s) -> s\n\
         \t}\n\
         \tprintln(r)\n\
         }\n",
    );
    check_ok(
        "import scarlet/binary\n\
         import scarlet/http/h1\n\
         import scarlet/http/headers.{Header}\n\
         pub fn main() {\n\
         \thead = h1.serialize_head(200, [Header(name: binary.from_string('A'), value: binary.from_string('b'))])\n\
         \tprintln(binary.byte_size(head))\n\
         }\n",
    );
    check_ok(
        "import scarlet/binary\n\
         import scarlet/http/headers.{Header}\n\
         pub fn main() {\n\
         \ths = [Header(name: binary.from_string('Host'), value: binary.from_string('x'))]\n\
         \tv = headers.get(hs, binary.from_string('host')) or binary.from_string('')\n\
         \tprintln(binary.to_string(v))\n\
         \tprintln(headers.has(hs, binary.from_string('HOST')))\n\
         }\n",
    );
}

// Token-list matching exists twice with no shared implementation: natively in
// `vm::http::has_token` and in Scarlet as `headers.contains_token`. This table
// drives one set of cases through both and demands the same answer.
#[test]
#[ignore = "needs the VM"]
fn native_and_al_token_matching_agree() {
    // (Connection value, does it carry the `close` token?)
    let cases: &[(&str, bool)] = &[
        ("close", true),
        ("CLOSE", true),
        // List element, OWS-trimmed.
        ("keep-alive, close", true),
        ("  close  ", true),
        // Trailing comma / empty elements are ignored, never a match.
        ("close,", true),
        ("a,,close", true),
        (",,", false),
        ("", false),
        // A token matches whole, not by prefix or substring.
        ("closed", false),
        ("no-close", false),
    ];
    for (value, expected) in cases {
        let source = format!(
            "import scarlet/binary\n\
             import scarlet/http/h1.{{Done, ConnNeither, ConnClose, ConnKeepAlive, ConnBoth}}\n\
             import scarlet/http/headers.{{Header}}\n\
             pub fn main() {{\n\
             \tname = binary.from_string('Connection')\n\
             \tvalue = binary.from_string('{value}')\n\
             \tnative = match h1.parse_request(binary.from_string('GET / HTTP/1.1\\r\\nConnection: {value}\\r\\n\\r\\n'), 0) {{\n\
             \t\tDone(_, _, _, _, flags, _) -> match flags.conn {{\n\
             \t\t\tConnClose -> True\n\
             \t\t\tConnBoth -> True\n\
             \t\t\tConnKeepAlive -> False\n\
             \t\t\tConnNeither -> False\n\
             \t\t}}\n\
             \t\t_ -> False\n\
             \t}}\n\
             \tal = headers.contains_token([Header(name: name, value: value)], name, binary.from_string('close'))\n\
             \tprintln(native)\n\
             \tprintln(al)\n\
             }}\n"
        );
        let want = if *expected {
            "True\nTrue\n"
        } else {
            "False\nFalse\n"
        };
        run_outputs(&source, want);
    }
}

// One `check_ok` per ASCII byte builtin, pinning that the call site
// type-checks against the `@vm`-declared signature.
#[test]
fn stdlib_binary_ascii_builtins() {
    // index_of : (Binary, Binary, Int) -> Option(Int)
    check_ok(
        "import scarlet/binary\n\
         pub fn main() {\n\
         \ti = binary.index_of(binary.from_string('abc'), binary.from_string('b'), 0) or 0\n\
         \tprintln(i)\n\
         }\n",
    );
    // parse_int : (Binary, Radix) -> Result(Int, Nil)
    check_ok(
        "import scarlet/binary.{Dec}\n\
         pub fn main() {\n\
         \tn = binary.parse_int(binary.from_string('42'), Dec) or 0\n\
         \tprintln(n)\n\
         }\n",
    );
    // eq_ignore_ascii_case : (Binary, Binary) -> Bool
    check_ok(
        "import scarlet/binary\n\
         pub fn main() {\n\
         \tprintln(binary.eq_ignore_ascii_case(binary.from_string('A'), binary.from_string('a')))\n\
         }\n",
    );
    // to_ascii_lower : (Binary) -> Binary
    check_ok(
        "import scarlet/binary\n\
         pub fn main() {\n\
         \tprintln(binary.to_string(binary.to_ascii_lower(binary.from_string('AB'))))\n\
         }\n",
    );
    // from_int_ascii : (Int, Radix) -> Binary
    check_ok(
        "import scarlet/binary.{Hex}\n\
         pub fn main() {\n\
         \tprintln(binary.to_string(binary.from_int_ascii(255, Hex)))\n\
         }\n",
    );
}

// Uppercase, zero-padded two-digit hex of the low 8 bits. from_int_ascii
// is lowercase and drops the leading zero of a value under 16, so %0D%0A
// is unreachable from it (T-197). The last line is the control: that
// existing spelling must stay as it is.
#[test]
#[ignore = "needs the VM"]
fn stdlib_binary_hex_byte() {
    run_outputs(
        "import scarlet/binary.{Hex}\n\
         pub fn main() {\n\
         \tprintln(binary.to_string(binary.hex_byte(10)))\n\
         \tprintln(binary.to_string(binary.hex_byte(13)))\n\
         \tprintln(binary.to_string(binary.hex_byte(255)))\n\
         \tprintln(binary.to_string(binary.hex_byte(0)))\n\
         \tprintln(binary.to_string(binary.hex_byte(256)))\n\
         \tprintln(binary.to_string(binary.hex_byte(0 - 1)))\n\
         \tprintln(binary.to_string(binary.from_int_ascii(13, Hex)))\n\
         }\n",
        "Ok(0A)\nOk(0D)\nOk(FF)\nOk(00)\nOk(00)\nOk(FF)\nOk(d)\n",
    );
}

#[test]
#[ignore = "needs the VM"]
fn stdlib_float() {
    run_outputs(
        "import scarlet/float\n\
         pub fn main() {\n\
         \tprintln(float.round(2.7))\n\
         \tprintln(float.floor(2.7))\n\
         \tprintln(float.ceil(2.1))\n\
         \tprintln(float.truncate(2.9))\n\
         \tprintln(float.from_int(5))\n\
         \tprintln(float.to_string(3.14))\n\
         }\n",
        "3\n2\n3\n2\n5.0\n3.14\n",
    );
    run_outputs(
        "import scarlet/float\n\
         pub fn main() {\n\
         \tprintln(float.abs(0.0 - 2.5))\n\
         \tprintln(float.abs(-0.0))\n\
         \tprintln(float.min(1.5, 3.2))\n\
         \tprintln(float.max(1.5, 3.2))\n\
         }\n",
        "2.5\n0.0\n1.5\n3.2\n",
    );
    // `-z` with z = 0.0 must preserve the IEEE-754 sign of zero. The `<=`/`>=`
    // pairs pin the equal boundary, which strict `<`/`>` would fail.
    run_outputs(
        "pub fn main() {\n\
         \tx = 2.5\n\
         \tz = 0.0\n\
         \tprintln(1.5 + 2.0)\n\
         \tprintln(1.5 * 2.0)\n\
         \tprintln(-x)\n\
         \tprintln(-z)\n\
         \tprintln(2.5 <= 2.5)\n\
         \tprintln(4.0 >= 4.0)\n\
         \tprintln(3.5 >= 4.0)\n\
         }\n",
        "3.5\n3.0\n-2.5\n-0.0\nTrue\nTrue\nFalse\n",
    );
    // On negatives floor goes toward -inf while truncate goes toward zero,
    // round is half-away-from-zero, ceil toward +inf.
    run_outputs(
        "import scarlet/float\n\
         pub fn main() {\n\
         \tprintln(float.floor(0.0 - 2.7))\n\
         \tprintln(float.ceil(0.0 - 2.1))\n\
         \tprintln(float.round(0.0 - 2.5))\n\
         \tprintln(float.truncate(0.0 - 2.9))\n\
         }\n",
        "-3\n-2\n-3\n-2\n",
    );
}

#[test]
#[ignore = "needs the VM"]
fn stdlib_string() {
    // length counts codepoints, not bytes: 'héllo' is 5 chars, 6 bytes.
    // split with an empty delimiter takes the char-split branch. trim strips
    // tabs and newlines too. inspect passes a String through verbatim.
    run_outputs(
        "import scarlet/string\n\
         pub fn main() {\n\
         \tprintln(string.length('héllo'))\n\
         \tprintln(string.split('abc', ''))\n\
         \tprintln(string.contains('abc', 'z'))\n\
         \tprintln(string.trim('\\t\\nhi\\n\\t'))\n\
         \tprintln(string.inspect('hi'))\n\
         \tprintln(string.inspect(42))\n\
         }\n",
        "5\n[a, b, c]\nFalse\nhi\nhi\n42\n",
    );
    // replace is a plain scalar-value substring replace: no grapheme barrier
    // blocks a bare \r inside a CRLF pair, unlike Gleam's grapheme-aware
    // replace (the hazard T-287 found does not exist in this module's model).
    run_outputs(
        "import scarlet/string\n\
         pub fn main() {\n\
         \tprintln(string.replace('a-b-c', '-', '_'))\n\
         \tprintln(string.replace('abc', 'z', 'Q'))\n\
         \tprintln(string.replace('a\\r\\nb', '\\r', ''))\n\
         }\n",
        "a_b_c\nabc\na\nb\n",
    );
    // to_graphemes groups an extended grapheme cluster into one piece where
    // split('') would cut it at every scalar value: 'e' + combining acute
    // (U+0301) is 2 scalar values, 1 cluster; the ZWJ family emoji from
    // T-287 is 7 scalar values, 1 cluster.
    run_outputs(
        "import scarlet/array\n\
         import scarlet/string\n\
         pub fn main() {\n\
         \tprintln(string.length('e\u{0301}b'))\n\
         \tprintln(array.length(string.to_graphemes('e\u{0301}b')))\n\
         \tprintln(string.length('\u{1F469}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}'))\n\
         \tprintln(array.length(string.to_graphemes('\u{1F469}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}')))\n\
         }\n",
        "3\n2\n7\n1\n",
    );
    // \u{...} spells a code point in Scarlet SOURCE, decoded by the
    // scanner — every \\u{...} below is literal backslash-u-brace text
    // handed to `al run`, not Rust's own escape (which only fires in the
    // `expected` argument, to name the same character a second, independent
    // way). Precomposed 'a\u{0301}' is 2 scalar values, matching
    // `to_graphemes`'s own count above for the same pair.
    run_outputs(
        "import scarlet/string\n\
         pub fn main() {\n\
         \tprintln('\\u{E9}')\n\
         \tprintln(string.length('\\u{E9}'))\n\
         \tprintln('\\u{1F469}')\n\
         \tprintln(string.length('\\u{1F469}'))\n\
         \tprintln('a\\u{0301}')\n\
         \tprintln(string.length('a\\u{0301}'))\n\
         }\n",
        "\u{E9}\n1\n\u{1F469}\n1\na\u{0301}\n2\n",
    );
    // from_code_points is the constructing inverse: an Array(Int) of scalar
    // values rebuilds the String \u{...} above spells one at a time.
    // Err(Nil) for a code point past U+10FFFF, a surrogate half, or
    // negative — none of which UTF-8 can encode alone.
    run_outputs(
        "import scarlet/string\n\
         pub fn main() {\n\
         \tprintln(string.from_code_points([233]))\n\
         \tprintln(string.from_code_points([128105]))\n\
         \tprintln(string.from_code_points([104, 105]))\n\
         \tprintln(string.from_code_points([]))\n\
         \tprintln(string.from_code_points([1114112]))\n\
         \tprintln(string.from_code_points([55296]))\n\
         \tprintln(string.from_code_points([-1]))\n\
         }\n",
        "Ok(\u{E9})\nOk(\u{1F469})\nOk(hi)\nOk()\nErr(Nil)\nErr(Nil)\nErr(Nil)\n",
    );
}
