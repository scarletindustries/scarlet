//! Runtime golden tests for the pure-AL standard library modules under `al/*`.
//! Each test drives a stdlib module's public functions through `al run` and
//! pins the observed output; a change in behaviour (not just "it type-checks")
//! is caught here.

mod common;
use common::{check_ok, check_rejects, run_outputs};

#[test]
fn stdlib_option() {
    run_outputs(
        "import al/option\n\
         println(option.map(Some(5), fn(x) x * 2))\n\
         println(option.unwrap(None, 99))\n\
         println(option.is_some(Some(1)))\n",
        "Some(10)\n99\nTrue\n",
    );
    // then: Some threads the inner value into f (Some(5) -> Some(6)); None
    // short-circuits, returning None without ever calling f.
    run_outputs(
        "import al/option\n\
         println(option.then(Some(5), fn(x) Some(x + 1)))\n\
         println(option.then(None, fn(x) Some(x + 1)))\n",
        "Some(6)\nNone\n",
    );
    // or_else: Some keeps the original option; None calls the fallback thunk.
    run_outputs(
        "import al/option\n\
         println(option.or_else(Some(1), fn() Some(2)))\n\
         println(option.or_else(None, fn() Some(2)))\n",
        "Some(1)\nSome(2)\n",
    );
    // is_none: True on None, False on Some — exercising both match arms.
    run_outputs(
        "import al/option\n\
         println(option.is_none(None))\n\
         println(option.is_none(Some(1)))\n",
        "True\nFalse\n",
    );
    // Remaining arms not exercised above: map leaves None untouched,
    // unwrap on Some yields the inner value, is_some is False on None.
    run_outputs(
        "import al/option\n\
         println(option.map(None, fn(x) x * 2))\n\
         println(option.unwrap(Some(5), 99))\n\
         println(option.is_some(None))\n",
        "None\n5\nFalse\n",
    );
}

#[test]
fn stdlib_result() {
    run_outputs(
        "import al/result\n\
         println(result.map(Ok(5), fn(x) x + 1))\n\
         println(result.map_err(Err('bad'), fn(e) '${e}!'))\n",
        "Ok(6)\nErr(bad!)\n",
    );
    // then: Ok threads the inner value into f (here Ok(5) -> Ok(6)); Err
    // short-circuits, propagating the original error untouched.
    run_outputs(
        "import al/result\n\
         println(result.then(Ok(5), fn(x) Ok(x + 1)))\n\
         println(result.then(Err('e'), fn(x) Ok(x + 1)))\n",
        "Ok(6)\nErr(e)\n",
    );
    // unwrap: Ok yields the inner value; Err discards the error and returns
    // the supplied default.
    run_outputs(
        "import al/result\n\
         println(result.unwrap(Ok(5), 0))\n\
         println(result.unwrap(Err('e'), 99))\n",
        "5\n99\n",
    );
    // is_ok / is_err: each predicate is True on its own constructor and False
    // on the other, exercising both match arms of each.
    run_outputs(
        "import al/result\n\
         println(result.is_ok(Ok(1)))\n\
         println(result.is_ok(Err('x')))\n\
         println(result.is_err(Err('x')))\n\
         println(result.is_err(Ok(1)))\n",
        "True\nFalse\nTrue\nFalse\n",
    );
    // Remaining arms not exercised above: map leaves an Err untouched,
    // map_err leaves an Ok untouched.
    run_outputs(
        "import al/result\n\
         println(result.map(Err('e'), fn(x) x + 1))\n\
         println(result.map_err(Ok(5), fn(e) e))\n",
        "Err(e)\nOk(5)\n",
    );
}

#[test]
fn stdlib_array() {
    run_outputs(
        "import al/array\n\
         println(array.map([1, 2, 3], fn(x) x * 10))\n\
         println(array.filter([1, 2, 3, 4], fn(x) x > 2))\n\
         println(array.fold([1, 2, 3, 4], 0, fn(a, b) a + b))\n\
         println(array.reverse([1, 2, 3]))\n\
         println(array.contains([1, 2, 3], 2))\n",
        "[10, 20, 30]\n[3, 4]\n10\n[3, 2, 1]\nTrue\n",
    );
    // find: returns Some(first match), None when nothing satisfies the predicate.
    run_outputs(
        "import al/array\n\
         println(array.find([1, 2, 3], fn(x) x > 1))\n\
         println(array.find([1, 2, 3], fn(x) x > 9))\n",
        "Some(2)\nNone\n",
    );
    // any / all: any is True iff some element matches; all is True iff every one does.
    run_outputs(
        "import al/array\n\
         println(array.any([1, 2, 3], fn(x) x > 2))\n\
         println(array.any([1, 2, 3], fn(x) x > 9))\n\
         println(array.all([2, 4, 6], fn(x) x % 2 == 0))\n\
         println(array.all([2, 3], fn(x) x % 2 == 0))\n",
        "True\nFalse\nTrue\nFalse\n",
    );
    // Empty-array base cases: the `[] ->` arm of each recursive array fn and the
    // not-found terminal of contains.
    run_outputs(
        "import al/array\n\
         println(array.map([], fn(x) x * 10))\n\
         println(array.filter([], fn(x) x > 2))\n\
         println(array.fold([], 0, fn(a, b) a + b))\n\
         println(array.length([]))\n\
         println(array.reverse([]))\n\
         println(array.contains([1, 2], 9))\n",
        "[]\n[]\n0\n0\n[]\nFalse\n",
    );
}

run_case! {
    stdlib_int: (
        "import al/int\n\
         println(int.max(3, 7))\n\
         println(int.min(3, 7))\n\
         println(int.abs(0 - 5))\n\
         println(int.abs(0 - 9223372036854775807 - 1))\n\
         println(int.clamp(99, 0, 10))\n\
         println(int.clamp(5, 10, 0))\n\
         println(int.to_string(42))\n",
        "7\n3\n5\n9223372036854775807\n10\n0\n42\n",
    ),

    stdlib_bool: (
        "import al/bool\n\
         println(bool.negate(True))\n\
         println(bool.to_string(False))\n",
        "False\nFalse\n",
    ),
}

#[test]
fn stdlib_decimal() {
    // Construction, exact arithmetic, and scale propagation: add aligns to the
    // wider scale, mul sums scales, sub is add of the negation.
    run_outputs(
        "import al/decimal\n\
         a = decimal.new(1999, 2)\n\
         println(decimal.to_string(decimal.add(a, decimal.new(1, 2))))\n\
         println(decimal.to_string(decimal.sub(a, decimal.new(1, 2))))\n\
         println(decimal.to_string(decimal.mul(a, decimal.from_int(3))))\n\
         println(decimal.to_string(decimal.mul(decimal.new(15, 1), decimal.new(25, 2))))\n\
         println(decimal.units(a))\n\
         println(decimal.scale(a))\n\
         println(decimal.to_string(decimal.new(5, 0 - 3)))\n",
        "20.00\n19.98\n59.97\n0.375\n1999\n2\n5000\n",
    );
    // Rounding: HalfEven is the default (ties to even — 0.125 down, 0.135 up),
    // HalfUp breaks ties away from zero on both signs, Down truncates, and a
    // wider target scale zero-pads instead of rounding.
    run_outputs(
        "import al/decimal.{HalfUp, Down}\n\
         x = decimal.new(2345, 3)\n\
         println(decimal.to_string(decimal.round(x, 2)))\n\
         println(decimal.to_string(decimal.round(decimal.new(125, 3), 2)))\n\
         println(decimal.to_string(decimal.round(decimal.new(135, 3), 2)))\n\
         println(decimal.to_string(decimal.round_with(x, 2, HalfUp)))\n\
         println(decimal.to_string(decimal.round_with(decimal.neg(x), 2, HalfUp)))\n\
         println(decimal.to_string(decimal.round_with(x, 2, Down)))\n\
         println(decimal.to_string(decimal.round(x, 5)))\n",
        "2.34\n0.12\n0.14\n2.35\n-2.35\n2.34\n2.34500\n",
    );
    // Division takes an explicit result scale and is None on a zero divisor.
    run_outputs(
        "import al/decimal\n\
         import al/option\n\
         bill = decimal.new(10000, 2)\n\
         println(option.map(decimal.div(bill, decimal.from_int(3), 2), decimal.to_string))\n\
         println(option.map(decimal.div(decimal.from_int(1), decimal.from_int(8), 4), decimal.to_string))\n\
         println(decimal.div(bill, decimal.from_int(0), 2))\n",
        "Some(33.33)\nSome(0.1250)\nNone\n",
    );
    // Numeric comparison is scale-blind (1.5 == 1.500) even though the
    // representation differs; normalize strips the trailing zeros.
    run_outputs(
        "import al/decimal\n\
         a = decimal.new(15, 1)\n\
         b = decimal.new(1500, 3)\n\
         println(decimal.eq(a, b))\n\
         println(decimal.compare(decimal.new(0 - 1, 2), decimal.from_int(0)))\n\
         println(decimal.lt(a, decimal.new(16, 1)))\n\
         println(decimal.to_string(decimal.max(a, decimal.new(2, 0))))\n\
         println(decimal.scale(decimal.normalize(b)))\n\
         println(decimal.is_negative(decimal.neg(a)))\n\
         println(decimal.is_zero(decimal.new(0, 5)))\n",
        "True\nLt\nTrue\n2\n1\nTrue\nTrue\n",
    );
    // parse round-trips through to_string, keeps the written scale, handles
    // signs, and rejects malformed or Int-overflowing input instead of
    // wrapping. -0.05 exercises the sign-on-zero-whole-part case.
    run_outputs(
        "import al/decimal\n\
         import al/option\n\
         println(option.map(decimal.parse('19.99'), decimal.to_string))\n\
         println(option.map(decimal.parse('-0.05'), decimal.to_string))\n\
         println(option.map(decimal.parse('+1.50'), decimal.to_string))\n\
         println(option.map(decimal.parse('42'), decimal.to_string))\n\
         println(decimal.parse('1.'))\n\
         println(decimal.parse('.5'))\n\
         println(decimal.parse('1.2.3'))\n\
         println(decimal.parse(''))\n\
         println(decimal.parse('-'))\n\
         println(decimal.parse('9223372036854775807.99'))\n\
         println(option.map(decimal.parse('92233720368547758.07'), decimal.units))\n",
        "Some(19.99)\nSome(-0.05)\nSome(1.50)\nSome(42)\nNone\nNone\nNone\nNone\nNone\nNone\nSome(9223372036854775807)\n",
    );
    // Negative `places` rounds to a multiple of 10^|places| at scale 0, in
    // round and div alike.
    run_outputs(
        "import al/decimal\n\
         import al/option\n\
         println(decimal.to_string(decimal.round(decimal.new(1250, 0), 0 - 2)))\n\
         println(decimal.to_string(decimal.round(decimal.new(12345, 1), 0 - 1)))\n\
         println(decimal.scale(decimal.round(decimal.new(1250, 0), 0 - 2)))\n\
         println(option.map(decimal.div(decimal.from_int(1234), decimal.from_int(1), 0 - 2), decimal.to_string))\n",
        "1200\n1230\n0\nSome(1200)\n",
    );
    // Dropping more than 18 digits (scale - places >= 19): 10^k would wrap,
    // so the quotient regime is {-1, 0, 1} and is computed without it. The
    // half boundary is reachable only at k == 19 (5 * 10^18); div is None
    // when places < -18 or the rescale exponent exceeds 18.
    run_outputs(
        "import al/decimal.{HalfUp, Up}\n\
         import al/option\n\
         println(decimal.to_string(decimal.round(decimal.new(123456789012345678, 18), 0 - 1)))\n\
         println(option.map(decimal.div(decimal.new(9000000000000000000, 18), decimal.from_int(1), 0 - 1), decimal.to_string))\n\
         println(decimal.to_string(decimal.round(decimal.new(1234, 0), 0 - 19)))\n\
         println(decimal.to_string(decimal.round(decimal.new(19, 1), 0 - 18)))\n\
         println(decimal.to_string(decimal.round_with(decimal.new(1, 18), 0 - 1, Up)))\n\
         println(decimal.to_string(decimal.round_with(decimal.new(5000000000000000000, 18), 0 - 1, HalfUp)))\n\
         println(decimal.to_string(decimal.round(decimal.new(5000000000000000000, 18), 0 - 1)))\n\
         println(decimal.div(decimal.from_int(1), decimal.from_int(1), 0 - 19))\n\
         println(decimal.div(decimal.from_int(1), decimal.new(1, 18), 21))\n",
        "0\nSome(10)\n0\n0\n10\n10\n0\nNone\nNone\n",
    );
    // Float bridges are explicitly lossy conveniences; from_float is None
    // instead of wrapping when units would leave Int range.
    run_outputs(
        "import al/decimal\n\
         import al/option\n\
         println(decimal.to_float(decimal.new(25, 1)))\n\
         println(option.map(decimal.from_float(2.5, 2), decimal.to_string))\n\
         println(decimal.from_float(10000000000000000000.0, 2))\n\
         println(decimal.from_float(0.5, 19))\n\
         println(option.map(decimal.from_float(149.0, 0 - 1), decimal.to_string))\n",
        "2.5\nSome(2.50)\nNone\nNone\nSome(150)\n",
    );
}

#[test]
fn stdlib_binary() {
    run_outputs(
        "import al/binary\n\
         b = binary.from_string('hi')\n\
         println(binary.to_string(b))\n\
         println(binary.bit_size(b))\n\
         println(binary.byte_size(b))\n\
         println(b)\n",
        "Ok(hi)\n16\n2\n<<104, 105>>\n",
    );
    run_outputs(
        "import al/binary\n\
         b = binary.from_string('ABC')\n\
         println(binary.slice(b, 8, 8))\n\
         println(binary.slice(b, 0, 99))\n\
         joined = binary.append(binary.from_string('AB'), binary.from_string('C'))\n\
         println(binary.to_string(joined))\n\
         println(binary.bit_size(binary.slice(b, 0, 5) or binary.from_string('')))\n",
        "Ok(<<66>>)\nErr(Nil)\nOk(ABC)\n5\n",
    );
    // Op::BinReadUtf8 — `<<c:utf8, ..>>` decodes one *codepoint*, not one byte.
    // [195, 169] is the UTF-8 encoding of 'é' (U+00E9); the pattern binds the
    // codepoint 233 and advances 16 bits so `..` swallows the empty remainder.
    // A byte-wise read would bind 195 instead, so 233 discriminates the opcode.
    run_outputs(
        "import al/binary\n\
         r = match <<195, 169>> {\n\
         \t<<c:utf8, ..>> -> c\n\
         \telse -> 0\n\
         }\n\
         println(r)\n",
        "233\n",
    );
    // Op::BinTake — `:bytes(n)` splices the first n bytes (prefix, clamped).
    // From a 5-byte source `bytes(3)` keeps exactly 65,66,67 ('A','B','C'):
    // discriminates count (not 5) and prefix-vs-suffix (not 'C','D','E').
    run_outputs(
        "import al/binary\n\
         import al/string\n\
         src = binary.from_string('ABCDE')\n\
         println(string.inspect(<<src:bytes(3)>>))\n",
        "<<65, 66, 67>>\n",
    );
    // binary.to_string Err branches: 0xFF is not valid UTF-8 (byte-aligned but
    // undecodable), and `<<1:4>>` is bit-unaligned (bit_len % 8 != 0). Both
    // yield Err(Nil) rather than panicking or lossily decoding.
    run_outputs(
        "import al/binary\n\
         println(binary.to_string(<<255>>))\n\
         println(binary.to_string(<<1:4>>))\n",
        "Err(Nil)\nErr(Nil)\n",
    );
    // binary.byte_size rounds up: a 4-bit binary occupies 1 byte (div_ceil),
    // not 0. The existing aligned case (16 bits -> 2) cannot catch the rounding.
    run_outputs(
        "import al/binary\n\
         println(binary.byte_size(<<1:4>>))\n",
        "1\n",
    );
    // binary.slice with a negative offset takes the `at < 0` Err branch (a
    // different guard than the existing OOB `at + take > bit_len` case).
    run_outputs(
        "import al/binary\n\
         println(binary.slice(binary.from_string('ABC'), 0 - 1, 8))\n",
        "Err(Nil)\n",
    );
    check_rejects(
        "import al/net/socket.{Socket}\n\
         fn f(c Socket) Nil { socket.write(c, 'nope') or Nil }\n",
        "Type mismatch",
    );
}

#[test]
fn stdlib_binary_byte_at() {
    // byte_at : (Binary, Int) -> Int — in-bounds bytes, -1 out of bounds
    // (both sides), and views (slices) read through their offset.
    run_outputs(
        "import al/binary\n\
         b = binary.from_string('AZ')\n\
         println(binary.byte_at(b, 0))\n\
         println(binary.byte_at(b, 1))\n\
         println(binary.byte_at(b, 2))\n\
         println(binary.byte_at(b, 0 - 1))\n\
         tail = match b {\n\
         \t<<_, ..rest>> -> rest\n\
         \telse -> b\n\
         }\n\
         println(binary.byte_at(tail, 0))\n",
        "65\n90\n-1\n-1\n90\n",
    );
}

#[test]
fn stdlib_http_builtins() {
    // The native h1 ops surfaced through al/http/h1 and al/http/headers:
    // parse_request / framing / serialize_head / headers.get / headers.has
    // each hydrate a Scheme from their @vm declaration. Behaviour is golden
    // tested (tests/programs/http_parse.al); these pin the call-site types.
    check_ok(
        "import al/binary\n\
         import al/http/h1.{Done, NeedMore, Bad, Http10, Http11}\n\
         r = match h1.parse_request(binary.from_string('GET / HTTP/1.1\\r\\n\\r\\n'), 0) {\n\
         \tDone(_, _, version, _, consumed) -> match version { Http10 -> 10 Http11 -> 11 } + consumed\n\
         \tNeedMore -> 0\n\
         \tBad(s) -> s\n\
         }\n\
         println(r)\n",
    );
    check_ok(
        "import al/binary\n\
         import al/http/h1.{Done, NoBody, Length, Chunked, Invalid}\n\
         r = match h1.parse_request(binary.from_string('GET / HTTP/1.1\\r\\n\\r\\n'), 0) {\n\
         \tDone(_, _, _, hdrs, _) -> match h1.framing(hdrs) {\n\
         \t\tNoBody -> 0\n\
         \t\tLength(n) -> n\n\
         \t\tChunked -> 0 - 2\n\
         \t\tInvalid(s) -> s\n\
         \t}\n\
         \telse -> 0 - 1\n\
         }\n\
         println(r)\n",
    );
    // chunk_decode : (Binary, Int, Int) -> ChunkBody, with the decoded body /
    // trailers / consumed offset destructurable from ChunkedDone.
    check_ok(
        "import al/binary\n\
         import al/http/h1.{ChunkedDone, ChunkedNeedMore, ChunkedBad}\n\
         import al/http/headers\n\
         r = match h1.chunk_decode(binary.from_string('5\\r\\nhello\\r\\n0\\r\\n\\r\\n'), 0, 1024) {\n\
         \tChunkedDone(body, trailers, consumed) -> {\n\
         \t\thas_sum = headers.has(trailers, binary.from_string('x-sum'))\n\
         \t\tif has_sum { consumed } else { binary.byte_size(body) + consumed }\n\
         \t}\n\
         \tChunkedNeedMore -> 0\n\
         \tChunkedBad(s) -> s\n\
         }\n\
         println(r)\n",
    );
    check_ok(
        "import al/binary\n\
         import al/http/h1\n\
         import al/http/headers.{Header}\n\
         head = h1.serialize_head(200, [Header(name: binary.from_string('A'), value: binary.from_string('b'))])\n\
         println(binary.byte_size(head))\n",
    );
    check_ok(
        "import al/binary\n\
         import al/http/headers.{Header}\n\
         hs = [Header(name: binary.from_string('Host'), value: binary.from_string('x'))]\n\
         v = headers.get(hs, binary.from_string('host')) or binary.from_string('')\n\
         println(binary.to_string(v))\n\
         println(headers.has(hs, binary.from_string('HOST')))\n",
    );
}

// The ASCII byte builtins each hydrate a distinct `Scheme` from their `@vm`
// declaration; one `check_ok` per builtin pins that the call site type-checks
// against the declared signature (the result is consumed so the program is a
// complete, well-typed unit).
#[test]
fn stdlib_binary_ascii_builtins() {
    // index_of : (Binary, Binary, Int) -> Option(Int)
    check_ok(
        "import al/binary\n\
         i = binary.index_of(binary.from_string('abc'), binary.from_string('b'), 0) or 0\n\
         println(i)\n",
    );
    // parse_int : (Binary, Radix) -> Result(Int, Nil)
    check_ok(
        "import al/binary.{Dec}\n\
         n = binary.parse_int(binary.from_string('42'), Dec) or 0\n\
         println(n)\n",
    );
    // eq_ignore_ascii_case : (Binary, Binary) -> Bool
    check_ok(
        "import al/binary\n\
         println(binary.eq_ignore_ascii_case(binary.from_string('A'), binary.from_string('a')))\n",
    );
    // to_ascii_lower : (Binary) -> Binary
    check_ok(
        "import al/binary\n\
         println(binary.to_string(binary.to_ascii_lower(binary.from_string('AB'))))\n",
    );
    // from_int_ascii : (Int, Radix) -> Binary
    check_ok(
        "import al/binary.{Hex}\n\
         println(binary.to_string(binary.from_int_ascii(255, Hex)))\n",
    );
}

#[test]
fn stdlib_float() {
    run_outputs(
        "import al/float\n\
         println(float.round(2.7))\n\
         println(float.floor(2.7))\n\
         println(float.ceil(2.1))\n\
         println(float.truncate(2.9))\n\
         println(float.from_int(5))\n\
         println(float.to_string(3.14))\n",
        "3\n2\n3\n2\n5.0\n3.14\n",
    );
    run_outputs(
        "import al/float\n\
         println(float.abs(0.0 - 2.5))\n\
         println(float.abs(-0.0))\n\
         println(float.min(1.5, 3.2))\n\
         println(float.max(1.5, 3.2))\n",
        "2.5\n0.0\n1.5\n3.2\n",
    );
    // VM float operators, each with a discriminating value assertion: `+`
    // (AddFloat), `*` (MulFloat), unary `-` (NegFloat) and `<=`/`>=`
    // (Lte/GteFloat). `-z` with z = 0.0 routes a zero through NegFloat and must
    // preserve the IEEE-754 sign of zero (`-0.0`, not `0.0`). The `<=`/`>=`
    // pairs pin the equal boundary — each would fail if the op were the strict
    // `<`/`>`.
    run_outputs(
        "x = 2.5\n\
         z = 0.0\n\
         println(1.5 + 2.0)\n\
         println(1.5 * 2.0)\n\
         println(-x)\n\
         println(-z)\n\
         println(2.5 <= 2.5)\n\
         println(4.0 >= 4.0)\n\
         println(3.5 >= 4.0)\n",
        "3.5\n3.0\n-2.5\n-0.0\nTrue\nTrue\nFalse\n",
    );
    // floor/ceil/round/truncate on negatives. floor goes toward -inf
    // (-2.7 -> -3) while truncate goes toward zero (-2.9 -> -2): together the
    // sign discriminator a positives-only test misses. round is
    // half-away-from-zero (-2.5 -> -3); ceil toward +inf (-2.1 -> -2).
    run_outputs(
        "import al/float\n\
         println(float.floor(0.0 - 2.7))\n\
         println(float.ceil(0.0 - 2.1))\n\
         println(float.round(0.0 - 2.5))\n\
         println(float.truncate(0.0 - 2.9))\n",
        "-3\n-2\n-3\n-2\n",
    );
}

#[test]
fn stdlib_string() {
    // string.length counts Unicode *codepoints* (StrLen -> chars().count()),
    // not bytes: 'héllo' is 5 chars but 6 bytes ('é' = U+00E9 is two UTF-8
    // bytes), so a chars-vs-bytes regression would print 6 here.
    // string.split with an empty delimiter takes the char-split branch
    // (StrSplit's `delim.is_empty()` arm), exploding into one entry per char.
    // string.contains pins the False arm ('z' absent from 'abc').
    // string.trim strips *all* leading/trailing whitespace, including the
    // non-space tab/newline a spaces-only test would miss (Rust str::trim).
    // string.inspect on a String passes the text through verbatim (the
    // ToString already-Str fast path) while on a scalar it stringifies (42).
    run_outputs(
        "import al/string\n\
         println(string.length('héllo'))\n\
         println(string.split('abc', ''))\n\
         println(string.contains('abc', 'z'))\n\
         println(string.trim('\\t\\nhi\\n\\t'))\n\
         println(string.inspect('hi'))\n\
         println(string.inspect(42))\n",
        "5\n[a, b, c]\nFalse\nhi\nhi\n42\n",
    );
}
