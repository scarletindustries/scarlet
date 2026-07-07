//! End-to-end VM execution coverage: opcodes only reachable through real
//! `scan -> parse -> typecheck -> compile -> run` programs that the golden
//! examples don't already exercise. Each test pins a *discriminating* output so
//! a regression in the targeted opcode (not just "it ran") is caught.

mod common;
use common::run_outputs;

// `range[i] or default` lowers to `Op::IndexOrElse`. The golden examples only
// use it on arrays (`numbers[0] or 0`); a lazy Range scrutinee takes a distinct
// VM arm (`range_elem` instead of `Seq::get`). In-bounds yields the element;
// out-of-bounds branches to the recovery value without ever boxing an Option.
#[test]
fn range_index_or_else() {
    run_outputs(
        "r = 5..10\n\
         println(r[2] or -1)\n\
         println(r[99] or -1)\n",
        "7\n-1\n",
    );
}

// `range[i]` (no `or`) lowers to `Op::Index`, producing an Option. The Range
// arm must offset from the start (`5 + 2 = 7`), and an out-of-bounds index must
// read as `None`, not a wrapped value.
#[test]
fn range_index_option() {
    run_outputs(
        "r = 5..10\n\
         println(r[2])\n\
         println(r[99])\n",
        "Some(7)\nNone\n",
    );
}

// `range[a..b]` lowers to `Op::ArraySlice`. The Range arm keeps the result lazy
// (`rs+start .. rs+end`) rather than materialising, so the slice of `5..10` at
// `[1..3]` is `[6, 7]`.
#[test]
fn range_slice() {
    run_outputs(
        "r = 5..10\n\
         println(r[1..3])\n",
        "[6, 7]\n",
    );
}

// Matching a Range value against an array pattern `[h, ..t]` drives `Op::ElemAt`
// (head) and `Op::Drop` (tail) on a Range, not an Array. `Drop` on a Range stays
// O(1) (`s+n .. e`); reconstructing `[h, ..t]` must reproduce the full sequence.
#[test]
fn match_range_with_array_pattern() {
    run_outputs(
        "r = 0..5\n\
         out = match r {\n\
         \t[h, ..t] -> [h, ..t]\n\
         \t[] -> []\n\
         }\n\
         println(out)\n\
         empty = match 3..3 {\n\
         \t[h, ..t] -> [h, ..t]\n\
         \t[] -> [0 - 1]\n\
         }\n\
         println(empty)\n",
        "[0, 1, 2, 3, 4]\n[-1]\n",
    );
}

// A polymorphic unary minus (`fn n(x) { -x }` with `x` left generic, constrained
// only `Numeric`) compiles to the *unspecialized* `Op::Neg`, which dispatches on
// the runtime tag. The same compiled function must negate an Int and a Float,
// preserving the IEEE sign for the float.
#[test]
fn generic_unary_neg_dispatches_on_runtime_tag() {
    run_outputs(
        "fn n(x) { -x }\n\
         println(n(5))\n\
         println(n(0 - 7))\n\
         println(n(2.5))\n",
        "-5\n7\n-2.5\n",
    );
}

// A top-level function passed *as a value* to another function (not called
// directly) emits `Op::PushSelf` at the self-reference site. `down` hands itself
// to `step`, which invokes the callback — exercising the capture-free PushSelf
// fast path (the cached closure clone) plus an indirect `Op::Call`.
#[test]
fn recursive_fn_passed_as_value() {
    run_outputs(
        "fn step(f fn(Int) Int, n Int) Int {\n\
         \tif n <= 0 { 0 } else { f(n - 1) }\n\
         }\n\
         fn down(n Int) Int { step(down, n) }\n\
         println(down(5))\n\
         println(down(0))\n",
        "0\n0\n",
    );
}

// `string.split` with a non-empty delimiter takes `Op::StrSplit`'s `split(&delim)`
// arm (the empty-delimiter char-explode arm is the one stdlib_string covers).
// Trailing/empty fields are preserved, so `'a,,b,'` splits into four parts.
#[test]
fn string_split_nonempty_delimiter() {
    run_outputs(
        "import al/string\n\
         println(string.split('a,b,c', ','))\n\
         println(string.split('a,,b,', ','))\n\
         println(string.split('nodelim', 'X'))\n",
        "[a, b, c]\n[a, , b, ]\n[nodelim]\n",
    );
}

// Binary values compare structurally via `Op::Eq` -> `values_equal` (the Binary
// arm). Equal bytes are equal; a single differing byte is not.
#[test]
fn binary_value_equality() {
    run_outputs(
        "println(<<1, 2, 3>> == <<1, 2, 3>>)\n\
         println(<<1, 2, 3>> == <<1, 2, 4>>)\n\
         println(<<1, 2>> != <<1, 2, 3>>)\n",
        "True\nFalse\nTrue\n",
    );
}

// Concrete `Float` division lowers to `Op::DivFloat`. Normal division divides;
// division by zero is *total* (`x / 0.0 == 0.0`), mirroring the integer
// `x / 0 == 0` convention, rather than producing Infinity/NaN.
#[test]
fn float_division_is_total() {
    run_outputs(
        "println(7.0 / 2.0)\n\
         println(1.0 / 0.0)\n\
         println(0.0 - 9.0 / 3.0)\n",
        "3.5\n0.0\n-3.0\n",
    );
}

// `arr[i]` without `or` lowers to `Op::Index`, yielding `Option`: `Some(elem)`
// in bounds, `None` (not a wrap or panic) out of bounds.
#[test]
fn array_index_yields_option() {
    run_outputs(
        "xs = [10, 20, 30]\n\
         println(xs[1])\n\
         println(xs[99])\n",
        "Some(20)\nNone\n",
    );
}

// A *capturing* closure that refers to itself by name in value position emits
// `Op::PushSelf` on the capture-carrying branch (rebuild from the live frame's
// captures, not the cached capture-free closure). `helper` closes over `base`
// and hands itself to `apply`; the recursion must still see the captured `base`.
#[test]
fn capturing_self_referential_closure() {
    run_outputs(
        "fn apply(f fn(Int) Int, n Int) Int { f(n) }\n\
         fn make(base Int) Int {\n\
         \thelper = fn(n) {\n\
         \t\tif n <= 0 { base } else { apply(helper, n - 1) }\n\
         \t}\n\
         \thelper(3)\n\
         }\n\
         println(make(42))\n\
         println(make(7))\n",
        "42\n7\n",
    );
}

// `println` of nested / record / variant values flows through `vm::inspect`'s
// multiline layout in the real binary (main.rs path), not just the unit tests.
#[test]
fn inspect_multiline_structures_e2e() {
    run_outputs(
        "type Point { x Int  y Int }\n\
         type Seg { a Point  b Point }\n\
         println(Seg(a: Point(x: 1, y: 2), b: Point(x: 3, y: 4)))\n\
         println([[1, 2], [3, 4]])\n",
        "Seg {\n  a: Point{ x: 1, y: 2 },\n  b: Point{ x: 3, y: 4 }\n}\n[\n  [1, 2],\n  [3, 4]\n]\n",
    );
}

// Or-pattern alternatives are ONE logical binding: every alternative must
// store the bound name into the same slot the arm body reads. At module scope
// the shadow-gets-a-fresh-slot rule used to give each alternative its own
// slot, so a match on the *first* alternative left the body reading an
// unwritten local (printed `0`). Pins both module scope and fn scope, with
// the match landing on first, middle, and last alternatives.
#[test]
fn or_pattern_binds_same_slot_in_every_alternative() {
    run_outputs(
        "type Shape {\n\
         \tCircle(r Int)\n\
         \tSquare(r Int)\n\
         \tRect(r Int h Int)\n\
         }\n\
         fn size(s Shape) Int {\n\
         \tmatch s {\n\
         \t\tCircle(r) | Square(r) | Rect(r, _) -> r\n\
         \t}\n\
         }\n\
         println(size(Circle(3)))\n\
         println(size(Square(4)))\n\
         println(size(Rect(5, 9)))\n\
         first = match Circle(7) {\n\
         \tCircle(r) | Square(r) | Rect(r, _) -> r\n\
         }\n\
         println(first)\n\
         last = match Rect(8, 1) {\n\
         \tCircle(r) | Square(r) | Rect(r, _) -> r\n\
         }\n\
         println(last)\n",
        "3\n4\n5\n7\n8\n",
    );
}

// Op::BinIndexOf — byte-substring search returning Option(Int). `from` is
// clamped into range and an empty needle matches at the (clamped) start. The
// four lines discriminate: first hit, hit past an offset, miss (None), and the
// empty-needle convention.
#[test]
fn binary_index_of() {
    run_outputs(
        "import al/binary\n\
         h = binary.from_string('abcabc')\n\
         println(binary.index_of(h, binary.from_string('bc'), 0))\n\
         println(binary.index_of(h, binary.from_string('bc'), 2))\n\
         println(binary.index_of(h, binary.from_string('xy'), 0))\n\
         println(binary.index_of(h, binary.from_string(''), 3))\n",
        "Some(1)\nSome(4)\nNone\nSome(3)\n",
    );
}

// Op::BinParseInt — ASCII integer parse in radix 10/16. The checked multiply/
// add reject an overflowing value as `None` (not a wrapped int — the request-
// smuggling defense, since AL arithmetic wraps); a non-digit or empty input is
// also `None`. Hex accepts both cases.
#[test]
fn binary_parse_int() {
    run_outputs(
        "import al/binary.{Dec, Hex}\n\
         println(binary.parse_int(binary.from_string('255'), Dec))\n\
         println(binary.parse_int(binary.from_string('ff'), Hex))\n\
         println(binary.parse_int(binary.from_string('FF'), Hex))\n\
         println(binary.parse_int(binary.from_string('99999999999999999999'), Dec))\n\
         println(binary.parse_int(binary.from_string('12x'), Dec))\n\
         println(binary.parse_int(binary.from_string(''), Dec))\n",
        "Some(255)\nSome(255)\nSome(255)\nNone\nNone\nNone\n",
    );
}

// Op::BinEqIgnoreAsciiCase — ASCII-case-insensitive byte equality (header-name
// matching). Differing case compares equal; a real byte difference does not.
#[test]
fn binary_eq_ignore_ascii_case() {
    run_outputs(
        "import al/binary\n\
         a = binary.from_string('Content-Length')\n\
         println(binary.eq_ignore_ascii_case(a, binary.from_string('content-length')))\n\
         println(binary.eq_ignore_ascii_case(binary.from_string('abc'), binary.from_string('abd')))\n",
        "True\nFalse\n",
    );
}

// Op::BinToAsciiLower — ASCII lowercasing copy; non-letter bytes pass through.
#[test]
fn binary_to_ascii_lower() {
    run_outputs(
        "import al/binary\n\
         println(binary.to_string(binary.to_ascii_lower(binary.from_string('AbC-123'))))\n",
        "Ok(abc-123)\n",
    );
}

// Op::BinFromIntAscii — render an Int as ASCII (radix 10/16, lowercase hex),
// handling zero and negatives, and round-tripping through parse_int for the
// non-negative cases.
#[test]
fn binary_from_int_ascii() {
    run_outputs(
        "import al/binary.{Dec, Hex}\n\
         println(binary.to_string(binary.from_int_ascii(255, Dec)))\n\
         println(binary.to_string(binary.from_int_ascii(255, Hex)))\n\
         println(binary.to_string(binary.from_int_ascii(0, Dec)))\n\
         println(binary.to_string(binary.from_int_ascii(0 - 42, Dec)))\n\
         println(binary.parse_int(binary.from_int_ascii(4096, Hex), Hex))\n",
        "Ok(255)\nOk(ff)\nOk(0)\nOk(-42)\nSome(4096)\n",
    );
}

// ===========================================================================
// Closure capture of an *enclosing function's local* — the `PushCapture` /
// `MakeClosure(capture_count > 0)` path. Distinct from capturing a module-
// global (U22 in unsound.rs), which resolves to `PushGlobal` and reads the
// entry frame at call time. Here the inner `fn` captures a binding that lives
// only in the outer call's frame, so the value must be *materialized into the
// closure* at `MakeClosure` time and read back via `PushCapture` at call time.
// ===========================================================================

#[test]
fn closure_captures_enclosing_function_local() {
    // `x` is a parameter of `make_adder`; its frame is gone by the time the
    // returned closure is called. Two closures built over distinct captures
    // (5 and 10) prove the capture is materialized per-closure: a shared/global
    // slot would make both print the same value (the last `x` written).
    run_outputs(
        "fn make_adder(x Int) fn(Int) Int {\n\
         \tfn(y Int) x + y\n\
         }\n\
         add5 = make_adder(5)\n\
         add10 = make_adder(10)\n\
         println(add5(3))\n\
         println(add10(3))\n",
        "8\n13\n",
    );
}

#[test]
fn closure_captures_multiple_enclosing_locals() {
    // Two captures (`a`, `b`) exercise `MakeClosure(capture_count = 2)` and
    // `PushCapture` at indices 0 and 1. Distinct (a, b) per closure proves each
    // closure carries its own materialized capture array, read back in order:
    // f = 2*10 + 3 = 23, g = 5*10 + 1 = 51.
    run_outputs(
        "fn make_affine(a Int, b Int) fn(Int) Int {\n\
         \tfn(x Int) a * x + b\n\
         }\n\
         f = make_affine(2, 3)\n\
         g = make_affine(5, 1)\n\
         println(f(10))\n\
         println(g(10))\n",
        "23\n51\n",
    );
}

#[test]
fn closure_captures_non_parameter_local() {
    // The captured binding `base` is a let-binding inside the enclosing
    // function, not a parameter — still an enclosing-function local, so still
    // the `PushCapture` path. p = 100 + 5 = 105, q = 200 + 5 = 205.
    run_outputs(
        "fn counter_from(start Int) fn(Int) Int {\n\
         \tbase = start * 100\n\
         \tfn(n Int) base + n\n\
         }\n\
         p = counter_from(1)\n\
         q = counter_from(2)\n\
         println(p(5))\n\
         println(q(5))\n",
        "105\n205\n",
    );
}

// ===========================================================================
// `&&` / `||` short-circuit evaluation
// ===========================================================================

#[test]
fn and_or_short_circuit_skips_rhs() {
    // `loud` prints 'evaluated' whenever its argument is actually computed, so a
    // missing 'evaluated' line proves the RHS was never reached. `False && _`
    // is decided by the LHS (JumpIfFalse), and `True || _` likewise (JumpIfTrue),
    // so neither RHS call runs: output is just the two boolean results.
    run_outputs(
        "fn loud(b Bool) Bool {\n\
         \tprintln('evaluated')\n\
         \tb\n\
         }\n\
         println(False && loud(True))\n\
         println(True || loud(False))\n",
        "False\nTrue\n",
    );
}

#[test]
fn and_or_evaluate_rhs_when_lhs_undecided() {
    // Control for `and_or_short_circuit_skips_rhs`: here the LHS does NOT decide
    // the result (`True && _`, `False || _`), so the RHS must run. Each call
    // emits 'evaluated' before the println prints the operator's result, which
    // also confirms `loud` genuinely prints when reached.
    run_outputs(
        "fn loud(b Bool) Bool {\n\
         \tprintln('evaluated')\n\
         \tb\n\
         }\n\
         println(True && loud(False))\n\
         println(False || loud(True))\n",
        "evaluated\nFalse\nevaluated\nTrue\n",
    );
}

// ===========================================================================
// Equality / inequality across types
//
// `==`/`!=` over Ints lower to specialized opcodes (`Op::EqInt`/`Op::NeqInt`);
// over any other value they lower to the generic structural `Op::Eq`/`Op::Neq`.
// Elsewhere the suite only ever asserts Int `==` (`1 == 2`, `x % 2 == 0`) and
// discards `!=` results, and the generic `Op::Eq` is only exercised implicitly
// inside the match matcher. These tests pin `!=` to a concrete result and drive
// the generic equality path as a value-producing expression over enum, String,
// Array, and Tuple values.
// ===========================================================================

#[test]
fn neq_on_int_and_enum() {
    // Ints take the specialized `Op::NeqInt`: `1 != 2` is True and `1 != 1` is
    // False, so the pair pins the opcode to genuine inequality (an always-true,
    // always-false, or accidental-`==` lowering would flip exactly one line).
    // Enum values take the *generic* `Op::Neq`, which compares the variant tag
    // and fields structurally: `Good('x') != Good('y')` differs in its field
    // (True), while `Good('x') != Good('x')` is structurally equal (False).
    run_outputs(
        "type C { Good(v String) Bad(v String) }\n\
         println(1 != 2)\n\
         println(1 != 1)\n\
         println(Good('x') != Good('y'))\n\
         println(Good('x') != Good('x'))\n",
        "True\nFalse\nTrue\nFalse\n",
    );
}

#[test]
fn eq_on_string_array_tuple() {
    // The generic `Op::Eq` path used as a value-producing expression (rather than
    // inside a match matcher) over each compound kind, pinned in both directions
    // so neither an always-True nor an always-False implementation could pass:
    // String compares by contents, Array element-wise, and Tuple component-wise
    // (the inequal tuples differ only in their second component).
    run_outputs(
        "println('ab' == 'ab')\n\
         println('ab' == 'ac')\n\
         println([1, 2] == [1, 2])\n\
         println([1, 2] == [1, 3])\n\
         println((1, 'a') == (1, 'a'))\n\
         println((1, 'a') == (1, 'b'))\n",
        "True\nFalse\nTrue\nFalse\nTrue\nFalse\n",
    );
}
