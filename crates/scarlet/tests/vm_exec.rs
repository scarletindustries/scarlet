//! End-to-end VM execution coverage: opcodes only reachable through real
//! `scan -> parse -> typecheck -> compile -> run` programs that the golden
//! examples don't already exercise. Each test pins a *discriminating* output so
//! a regression in the targeted opcode (not just "it ran") is caught.

mod common;
use common::run_outputs;

run_case! {
    // `_` separators in number literals are spelling only: the same value in
    // expressions, in patterns (which must also match their plain spelling —
    // exhaustiveness keys on the digits, so `1_000` and `1000` are one arm),
    // and in floats.
    digit_separators: (
        "pub fn main() {\n\
         \tprintln(1_000_000 + 1)\n\
         \tprintln(match 1000 {\n\
         \t\t1_000 -> 'grouped'\n\
         \t\t_ -> 'other'\n\
         \t})\n\
         \tprintln(1_0.2_5 == 10.25)\n\
         }\n",
        "1000001\ngrouped\nTrue\n",
    ),

    // `range[i] or default` lowers to `Op::Index` plus an Option match. The golden
    // examples only index arrays (`numbers[0] or 0`); a lazy Range scrutinee takes
    // a distinct arm inside `Op::Index` (`range_elem` instead of `Seq::get`).
    // In-bounds yields the element; out-of-bounds yields the recovery value.
    range_index_or_else: (
        "pub fn main() {\n\
         \tr = 5..10\n\
         \tprintln(r[2] or -1)\n\
         \tprintln(r[99] or -1)\n\
         }\n",
        "7\n-1\n",
    ),

    // `range[i]` (no `or`) lowers to `Op::Index`, producing an Option. The Range
    // arm must offset from the start (`5 + 2 = 7`), and an out-of-bounds index must
    // read as `None`, not a wrapped value.
    range_index_option: (
        "pub fn main() {\n\
         \tr = 5..10\n\
         \tprintln(r[2])\n\
         \tprintln(r[99])\n\
         }\n",
        "Some(7)\nNone\n",
    ),

    // `range[a..b]` lowers to `Op::ArraySlice`. The Range arm keeps the result lazy
    // (`rs+start .. rs+end`) rather than materialising, so the slice of `5..10` at
    // `[1..3]` is `[6, 7]`.
    range_slice: (
        "pub fn main() {\n\
         \tr = 5..10\n\
         \tprintln(r[1..3])\n\
         }\n",
        "[6, 7]\n",
    ),

    // Matching a Range value against an array pattern `[h, ..t]` drives `Op::ElemAt`
    // (head) and `Op::SeqDrop` (tail) on a Range, not an Array. `SeqDrop` on a Range stays
    // O(1) (`s+n .. e`); reconstructing `[h, ..t]` must reproduce the full sequence.
    match_range_with_array_pattern: (
        "pub fn main() {\n\
         \tr = 0..5\n\
         \tout = match r {\n\
         \t\t[h, ..t] -> [h, ..t]\n\
         \t\t[] -> []\n\
         \t}\n\
         \tprintln(out)\n\
         \tempty = match 3..3 {\n\
         \t\t[h, ..t] -> [h, ..t]\n\
         \t\t[] -> [0 - 1]\n\
         \t}\n\
         \tprintln(empty)\n\
         }\n",
        "[0, 1, 2, 3, 4]\n[-1]\n",
    ),

    // A polymorphic unary minus (`fn n(x) { -x }` with `x` left generic, constrained
    // only `Numeric`) compiles to the *unspecialized* `Op::Neg`, which dispatches on
    // the runtime tag. The same compiled function must negate an Int and a Float,
    // preserving the IEEE sign for the float.
    generic_unary_neg_dispatches_on_runtime_tag: (
        "fn n(x) { -x }\n\
         pub fn main() {\n\
         \tprintln(n(5))\n\
         \tprintln(n(0 - 7))\n\
         \tprintln(n(2.5))\n\
         }\n",
        "-5\n7\n-2.5\n",
    ),

    // A top-level function passed *as a value* to another function (not called
    // directly) emits `Op::PushSelf` at the self-reference site. `down` hands itself
    // to `step`, which invokes the callback — exercising the capture-free PushSelf
    // fast path (the cached closure clone) plus an indirect `Op::Call`.
    recursive_fn_passed_as_value: (
        "fn step(f fn(Int) Int, n Int) Int {\n\
         \tif n <= 0 { 0 } else { f(n - 1) }\n\
         }\n\
         fn down(n Int) Int { step(down, n) }\n\
         pub fn main() {\n\
         \tprintln(down(5))\n\
         \tprintln(down(0))\n\
         }\n",
        "0\n0\n",
    ),

    // `string.split` with a non-empty delimiter takes `Op::StrSplit`'s `split(&delim)`
    // arm (the empty-delimiter char-explode arm is the one stdlib_string covers).
    // Trailing/empty fields are preserved, so `'a,,b,'` splits into four parts.
    string_split_nonempty_delimiter: (
        "import scarlet/string\n\
         pub fn main() {\n\
         \tprintln(string.split('a,b,c', ','))\n\
         \tprintln(string.split('a,,b,', ','))\n\
         \tprintln(string.split('nodelim', 'X'))\n\
         }\n",
        "[a, b, c]\n[a, , b, ]\n[nodelim]\n",
    ),

    // `values_equal`'s Binary arm: compare structurally, byte for byte.
    binary_value_equality: (
        "pub fn main() {\n\
         \tprintln(<<1, 2, 3>> == <<1, 2, 3>>)\n\
         \tprintln(<<1, 2, 3>> == <<1, 2, 4>>)\n\
         \tprintln(<<1, 2>> != <<1, 2, 3>>)\n\
         }\n",
        "True\nFalse\nTrue\n",
    ),

    // `Op::DivFloat` is total: `x / 0.0 == 0.0`, mirroring the integer
    // `x / 0 == 0` convention, rather than Infinity/NaN.
    float_division_is_total: (
        "pub fn main() {\n\
         \tprintln(7.0 / 2.0)\n\
         \tprintln(1.0 / 0.0)\n\
         \tprintln(0.0 - 9.0 / 3.0)\n\
         }\n",
        "3.5\n0.0\n-3.0\n",
    ),

    // `Op::Index` yields `Option`: `None` out of bounds, not a wrap or panic.
    array_index_yields_option: (
        "pub fn main() {\n\
         \txs = [10, 20, 30]\n\
         \tprintln(xs[1])\n\
         \tprintln(xs[99])\n\
         }\n",
        "Some(20)\nNone\n",
    ),

    // A capturing closure naming itself in value position takes `PushSelf`'s
    // capture-carrying branch: rebuild from the live frame, not from the
    // cached capture-free closure, so the recursion still sees `base`.
    capturing_self_referential_closure: (
        "fn apply(f fn(Int) Int, n Int) Int { f(n) }\n\
         fn make(base Int) Int {\n\
         \thelper = fn(n) {\n\
         \t\tif n <= 0 { base } else { apply(helper, n - 1) }\n\
         \t}\n\
         \thelper(3)\n\
         }\n\
         pub fn main() {\n\
         \tprintln(make(42))\n\
         \tprintln(make(7))\n\
         }\n",
        "42\n7\n",
    ),

    // `vm::inspect`'s multiline layout through the real binary, not just the
    // unit tests.
    inspect_multiline_structures_e2e: (
        "type Point {\n\tx Int\n\ty Int\n}\n\
         type Seg {\n\ta Point\n\tb Point\n}\n\
         pub fn main() {\n\
         \tprintln(Seg(a: Point(x: 1, y: 2), b: Point(x: 3, y: 4)))\n\
         \tprintln([[1, 2], [3, 4]])\n\
         }\n",
        "Seg {\n  a: Point{ x: 1, y: 2 },\n  b: Point{ x: 3, y: 4 }\n}\n[\n  [1, 2],\n  [3, 4]\n]\n",
    ),

    // Or-pattern alternatives are one logical binding: every alternative must
    // store into the same slot the arm body reads. Pinned on the first,
    // middle, and last alternative, both as a function's tail expression and
    // as a binding's initialiser in `main`'s body (the two ways an arm's slot
    // gets allocated).
    or_pattern_binds_same_slot_in_every_alternative: (
        "type Shape {\n\
         \tCircle(r Int)\n\
         \tSquare(r Int)\n\
         \tRect(r Int, h Int)\n\
         }\n\
         fn size(s Shape) Int {\n\
         \tmatch s {\n\
         \t\tCircle(r) | Square(r) | Rect(r, _) -> r\n\
         \t}\n\
         }\n\
         pub fn main() {\n\
         \tprintln(size(Circle(3)))\n\
         \tprintln(size(Square(4)))\n\
         \tprintln(size(Rect(5, 9)))\n\
         \tfirst = match Circle(7) {\n\
         \t\tCircle(r) | Square(r) | Rect(r, _) -> r\n\
         \t}\n\
         \tprintln(first)\n\
         \tlast = match Rect(8, 1) {\n\
         \t\tCircle(r) | Square(r) | Rect(r, _) -> r\n\
         \t}\n\
         \tprintln(last)\n\
         }\n",
        "3\n4\n5\n7\n8\n",
    ),

    // Op::BinIndexOf: `from` is clamped into range and an empty needle matches
    // at the clamped start.
    binary_index_of: (
        "import scarlet/binary\n\
         pub fn main() {\n\
         \th = binary.from_string('abcabc')\n\
         \tprintln(binary.index_of(h, binary.from_string('bc'), 0))\n\
         \tprintln(binary.index_of(h, binary.from_string('bc'), 2))\n\
         \tprintln(binary.index_of(h, binary.from_string('xy'), 0))\n\
         \tprintln(binary.index_of(h, binary.from_string(''), 3))\n\
         }\n",
        "Some(1)\nSome(4)\nNone\nSome(3)\n",
    ),

    // Op::BinParseInt must reject an overflowing value as `Err(Nil)`, never a
    // wrapped int: Scarlet arithmetic wraps, and this is the request-smuggling
    // defense.
    binary_parse_int: (
        "import scarlet/binary.{Dec, Hex}\n\
         pub fn main() {\n\
         \tprintln(binary.parse_int(binary.from_string('255'), Dec))\n\
         \tprintln(binary.parse_int(binary.from_string('ff'), Hex))\n\
         \tprintln(binary.parse_int(binary.from_string('FF'), Hex))\n\
         \tprintln(binary.parse_int(binary.from_string('99999999999999999999'), Dec))\n\
         \tprintln(binary.parse_int(binary.from_string('12x'), Dec))\n\
         \tprintln(binary.parse_int(binary.from_string(''), Dec))\n\
         }\n",
        "Ok(255)\nOk(255)\nOk(255)\nErr(Nil)\nErr(Nil)\nErr(Nil)\n",
    ),

    // Op::IntFromString is the total inverse of `to_string`: unlike a
    // hand-rolled "strip a sign, delegate to the unsigned digit walk" parse
    // (which cannot represent `min_value`'s magnitude in a positive Int), it
    // round-trips every value `to_string` produces, `min_value` included.
    int_from_string: (
        "import scarlet/int\n\
         pub fn main() {\n\
         \tprintln(int.from_string('42'))\n\
         \tprintln(int.from_string('0'))\n\
         \tprintln(int.from_string('-42'))\n\
         \tprintln(int.from_string('+42'))\n\
         \tprintln(int.from_string('007'))\n\
         \tprintln(int.from_string(''))\n\
         \tprintln(int.from_string('-'))\n\
         \tprintln(int.from_string('12x'))\n\
         \tprintln(int.from_string(' 42'))\n\
         \tprintln(int.from_string('9223372036854775808'))\n\
         \tprintln(int.from_string(int.to_string(int.min_value)))\n\
         \tprintln(int.from_string(int.to_string(int.max_value)))\n\
         }\n",
        "Ok(42)\nOk(0)\nOk(-42)\nOk(42)\nOk(7)\nErr(Nil)\nErr(Nil)\nErr(Nil)\nErr(Nil)\nErr(Nil)\nOk(-9223372036854775808)\nOk(9223372036854775807)\n",
    ),

    // Op::BinEqIgnoreAsciiCase: ASCII-case-insensitive header-name matching.
    binary_eq_ignore_ascii_case: (
        "import scarlet/binary\n\
         pub fn main() {\n\
         \ta = binary.from_string('Content-Length')\n\
         \tprintln(binary.eq_ignore_ascii_case(a, binary.from_string('content-length')))\n\
         \tprintln(binary.eq_ignore_ascii_case(binary.from_string('abc'), binary.from_string('abd')))\n\
         }\n",
        "True\nFalse\n",
    ),

    // Op::BinToAsciiLower: non-letter bytes pass through.
    binary_to_ascii_lower: (
        "import scarlet/binary\n\
         pub fn main() {\n\
         \tprintln(binary.to_string(binary.to_ascii_lower(binary.from_string('AbC-123'))))\n\
         }\n",
        "Ok(abc-123)\n",
    ),

    // Op::BinFromIntAscii: radix 10/16, lowercase hex, zero and negatives.
    binary_from_int_ascii: (
        "import scarlet/binary.{Dec, Hex}\n\
         pub fn main() {\n\
         \tprintln(binary.to_string(binary.from_int_ascii(255, Dec)))\n\
         \tprintln(binary.to_string(binary.from_int_ascii(255, Hex)))\n\
         \tprintln(binary.to_string(binary.from_int_ascii(0, Dec)))\n\
         \tprintln(binary.to_string(binary.from_int_ascii(0 - 42, Dec)))\n\
         \tprintln(binary.parse_int(binary.from_int_ascii(4096, Hex), Hex))\n\
         }\n",
        "Ok(255)\nOk(ff)\nOk(0)\nOk(-42)\nOk(4096)\n",
    ),
}

// Closure capture of an enclosing function's local — the `PushCapture` /
// `MakeClosure(capture_count > 0)` path. Distinct from capturing a module
// global (U22 in unsound.rs), which resolves to `PushGlobal`. Here the
// captured binding lives only in the outer call's frame, so it must be
// materialized into the closure at `MakeClosure` time.

#[test]
fn closure_captures_enclosing_function_local() {
    // Two closures over distinct captures: a shared or global slot would make
    // both print the last `x` written.
    run_outputs(
        "fn make_adder(x Int) fn(Int) Int {\n\
         \tfn(y Int) x + y\n\
         }\n\
         pub fn main() {\n\
         \tadd5 = make_adder(5)\n\
         \tadd10 = make_adder(10)\n\
         \tprintln(add5(3))\n\
         \tprintln(add10(3))\n\
         }\n",
        "8\n13\n",
    );
}

#[test]
fn closure_captures_multiple_enclosing_locals() {
    // Two captures: `MakeClosure(capture_count = 2)` plus `PushCapture` at
    // indices 0 and 1, read back in order.
    run_outputs(
        "fn make_affine(a Int, b Int) fn(Int) Int {\n\
         \tfn(x Int) a * x + b\n\
         }\n\
         pub fn main() {\n\
         \tf = make_affine(2, 3)\n\
         \tg = make_affine(5, 1)\n\
         \tprintln(f(10))\n\
         \tprintln(g(10))\n\
         }\n",
        "23\n51\n",
    );
}

#[test]
fn closure_captures_non_parameter_local() {
    // `base` is a let-binding rather than a parameter, still `PushCapture`.
    run_outputs(
        "fn counter_from(start Int) fn(Int) Int {\n\
         \tbase = start * 100\n\
         \tfn(n Int) base + n\n\
         }\n\
         pub fn main() {\n\
         \tp = counter_from(1)\n\
         \tq = counter_from(2)\n\
         \tprintln(p(5))\n\
         \tprintln(q(5))\n\
         }\n",
        "105\n205\n",
    );
}

#[test]
fn and_or_short_circuit_skips_rhs() {
    // `loud` prints when its argument is computed, so a missing 'evaluated'
    // line proves the RHS was never reached.
    run_outputs(
        "fn loud(b Bool) Bool {\n\
         \tprintln('evaluated')\n\
         \tb\n\
         }\n\
         pub fn main() {\n\
         \tprintln(False && loud(True))\n\
         \tprintln(True || loud(False))\n\
         }\n",
        "False\nTrue\n",
    );
}

#[test]
fn and_or_evaluate_rhs_when_lhs_undecided() {
    // Control for `and_or_short_circuit_skips_rhs`: the LHS does not decide
    // the result, so the RHS must run and `loud` must print.
    run_outputs(
        "fn loud(b Bool) Bool {\n\
         \tprintln('evaluated')\n\
         \tb\n\
         }\n\
         pub fn main() {\n\
         \tprintln(True && loud(False))\n\
         \tprintln(False || loud(True))\n\
         }\n",
        "evaluated\nFalse\nevaluated\nTrue\n",
    );
}

// `==`/`!=` over Ints lower to `Op::EqInt`/`Op::NeqInt`; over anything else to
// the generic structural `Op::Eq`/`Op::Neq`. Elsewhere the suite only asserts
// Int `==` and discards `!=` results, and the generic `Op::Eq` is exercised
// only inside the match matcher.

#[test]
fn neq_on_int_and_enum() {
    // Both directions per opcode, so an always-true, always-false, or
    // accidental-`==` lowering flips exactly one line.
    run_outputs(
        "type C {\n\tGood(v String)\n\tBad(v String)\n}\n\
         pub fn main() {\n\
         \tprintln(1 != 2)\n\
         \tprintln(1 != 1)\n\
         \tprintln(Good('x') != Good('y'))\n\
         \tprintln(Good('x') != Good('x'))\n\
         }\n",
        "True\nFalse\nTrue\nFalse\n",
    );
}

#[test]
fn eq_on_string_array_tuple() {
    // Generic `Op::Eq` as a value-producing expression over each compound
    // kind, both directions.
    run_outputs(
        "pub fn main() {\n\
         \tprintln('ab' == 'ab')\n\
         \tprintln('ab' == 'ac')\n\
         \tprintln([1, 2] == [1, 2])\n\
         \tprintln([1, 2] == [1, 3])\n\
         \tprintln((1, 'a') == (1, 'a'))\n\
         \tprintln((1, 'a') == (1, 'b'))\n\
         }\n",
        "True\nFalse\nTrue\nFalse\nTrue\nFalse\n",
    );
}

// Type-directed opcodes and their dynamic fallbacks. The compiler picks a
// specialized opcode when `engine.find(ty)` resolves at emit time and falls
// back to the tag-dispatching generic op when it stays a `Var`. Each pair
// below drives one operation through both halves; they must agree, so a bug in
// either flips exactly one test.

run_case! {
    // Both directions per op, so an always-True, always-False, or
    // operand-swapped implementation flips exactly one line.
    typed_int_ordering_compares: (
        "pub fn main() {\n\
         \tprintln(1 < 2)\n\
         \tprintln(2 < 1)\n\
         \tprintln(2 > 1)\n\
         \tprintln(1 > 2)\n\
         \tprintln(2 <= 2)\n\
         \tprintln(3 <= 2)\n\
         \tprintln(2 >= 2)\n\
         \tprintln(1 >= 2)\n\
         }\n",
        "True\nFalse\nTrue\nFalse\nTrue\nFalse\nTrue\nFalse\n",
    ),

    // As above for Float. The `<=`/`>=` lines use equal operands, so a
    // strict-compare mislowering fails them.
    typed_float_ordering_compares: (
        "pub fn main() {\n\
         \tprintln(1.5 < 2.5)\n\
         \tprintln(2.5 < 1.5)\n\
         \tprintln(2.5 > 1.5)\n\
         \tprintln(1.5 > 2.5)\n\
         \tprintln(2.0 <= 2.0)\n\
         \tprintln(3.0 <= 2.0)\n\
         \tprintln(2.0 >= 2.0)\n\
         \tprintln(1.0 >= 2.0)\n\
         }\n",
        "True\nFalse\nTrue\nFalse\nTrue\nFalse\nTrue\nFalse\n",
    ),

    // A `Numeric`-constrained wrapper leaves the operand type unbound at emit
    // time, so the four bodies compile to the generic ops and must serve both
    // Int and Float callers, agreeing with the typed cases line for line.
    generic_ordering_compare_dispatches_on_runtime_tag: (
        "fn lt(a, b) { a < b }\n\
         fn gt(a, b) { a > b }\n\
         fn le(a, b) { a <= b }\n\
         fn ge(a, b) { a >= b }\n\
         pub fn main() {\n\
         \tprintln(lt(1, 2))\n\
         \tprintln(lt(2, 1))\n\
         \tprintln(gt(2, 1))\n\
         \tprintln(gt(1, 2))\n\
         \tprintln(le(2, 2))\n\
         \tprintln(le(3, 2))\n\
         \tprintln(ge(2, 2))\n\
         \tprintln(ge(1, 2))\n\
         \tprintln(lt(1.5, 2.5))\n\
         \tprintln(gt(2.5, 1.5))\n\
         \tprintln(le(2.0, 2.0))\n\
         \tprintln(ge(1.0, 2.0))\n\
         }\n",
        "True\nFalse\nTrue\nFalse\nTrue\nFalse\nTrue\nFalse\n\
         True\nTrue\nTrue\nFalse\n",
    ),

    // `Op::CallKnown` carries `func_idx` in its operand with no callee on the
    // stack. `twice`'s inner `inc(x)` is non-tail, the outer one is tail.
    direct_call_to_known_top_level_fn: (
        "fn inc(x Int) Int { x + 1 }\n\
         fn twice(x Int) Int { inc(inc(x)) }\n\
         pub fn main() {\n\
         \tprintln(twice(5))\n\
         \tprintln(inc(0 - 3))\n\
         }\n",
        "7\n-2\n",
    ),

    // `Op::TailCallKnown`. At n = 200_000 a frame-pushing lowering overflows
    // the stack, so terminating at all is the assertion.
    mutual_tail_recursion_between_known_fns: (
        "fn even(n Int) Bool { if n == 0 { True } else { odd(n - 1) } }\n\
         fn odd(n Int) Bool { if n == 0 { False } else { even(n - 1) } }\n\
         pub fn main() {\n\
         \tprintln(even(200000))\n\
         \tprintln(odd(200000))\n\
         \tprintln(even(7))\n\
         }\n",
        "True\nFalse\nFalse\n",
    ),

    // A callee that is a runtime value falls back to dynamic `Op::Call`.
    // `apply` compiles once but dispatches to two bodies, so a lowering that
    // baked in either target fails one line.
    indirect_call_through_value_is_dynamic: (
        "fn inc(x Int) Int { x + 1 }\n\
         fn dbl(x Int) Int { x * 2 }\n\
         fn apply(f fn(Int) Int, x Int) Int { f(x) }\n\
         pub fn main() {\n\
         \tprintln(apply(inc, 5))\n\
         \tprintln(apply(dbl, 5))\n\
         }\n",
        "6\n10\n",
    ),

    // `count` alternates `hop` (`TailCallKnown`) with `f` (dynamic
    // `TailCall`); at n = 200_000 both halves must reuse the frame.
    indirect_tail_call_through_value_reuses_frame: (
        "fn hop(f fn(Int, Int) Int, acc Int, n Int) Int {\n\
         \tif n == 0 { acc } else { f(acc + 1, n - 1) }\n\
         }\n\
         fn count(acc Int, n Int) Int { hop(count, acc, n) }\n\
         pub fn main() {\n\
         \tprintln(count(0, 200000))\n\
         }\n",
        "200000\n",
    ),

    // Bare constructor arms lower to `Op::SwitchTag`, one indexed jump on
    // `variant_idx`. Each arm yields a value only it can, so a mis-indexed
    // jump table fails a line.
    exhaustive_variant_match_is_jump_table: (
        "type T {\n\
         \tA(x Int)\n\
         \tB(x Int)\n\
         \tC(x Int, y Int)\n\
         }\n\
         fn pick(t T) Int {\n\
         \tmatch t {\n\
         \t\tA(x) -> x\n\
         \t\tB(x) -> x * 10\n\
         \t\tC(x, y) -> x * 100 + y\n\
         \t}\n\
         }\n\
         pub fn main() {\n\
         \tprintln(pick(A(3)))\n\
         \tprintln(pick(B(4)))\n\
         \tprintln(pick(C(5, 6)))\n\
         }\n",
        "3\n40\n506\n",
    ),

    // Two arms share a variant tag, which `variant_idx` alone cannot
    // distinguish, so the compiler must fall back to sequential
    // `Op::MatchEnum`.
    variant_match_with_nested_literal_falls_back: (
        "type T {\n\
         \tA(x Int)\n\
         \tB(x Int)\n\
         }\n\
         fn pick(t T) Int {\n\
         \tmatch t {\n\
         \t\tA(0) -> 999\n\
         \t\tA(x) -> x\n\
         \t\tB(x) -> x * 10\n\
         \t}\n\
         }\n\
         pub fn main() {\n\
         \tprintln(pick(A(0)))\n\
         \tprintln(pick(A(3)))\n\
         \tprintln(pick(B(4)))\n\
         }\n",
        "999\n3\n40\n",
    ),

    // `.field` on a resolved record lowers to `Op::GetFieldUnchecked`. Three
    // field indices pin the operand encoding; the `..p` spread projects the
    // unnamed fields through the same op.
    record_field_access_unchecked: (
        "type P {\n\tx Int\n\ty Int\n\tz Int\n}\n\
         pub fn main() {\n\
         \tp = P(x: 10, y: 20, z: 30)\n\
         \tprintln(p.x)\n\
         \tprintln(p.y)\n\
         \tprintln(p.z)\n\
         \tq = P(y: 99, ..p)\n\
         \tprintln(q.x)\n\
         \tprintln(q.y)\n\
         \tprintln(q.z)\n\
         }\n",
        "10\n20\n30\n10\n99\n30\n",
    ),

    // A field at the same position on every variant is read by index alone;
    // the runtime tag varies across calls but `GetFieldUnchecked` must not
    // consult it. Nominal typing makes the receiver always a resolved `Con`,
    // so the checked `Op::GetField` fallback is unreachable from surface
    // syntax and is pinned only indirectly, here.
    field_access_across_variants_ignores_runtime_tag: (
        "type S {\n\
         \tA(v Int, w Int)\n\
         \tB(v Int, w Int)\n\
         \tC(v Int, w Int)\n\
         }\n\
         fn sum(s S) Int { s.v + s.w }\n\
         pub fn main() {\n\
         \tprintln(sum(A(1, 2)))\n\
         \tprintln(sum(B(10, 20)))\n\
         \tprintln(sum(C(100, 200)))\n\
         }\n",
        "3\n30\n300\n",
    ),
}

/// The fused op evaluates its default eagerly, so `lower` may only fuse a
/// *pure* one. A call has an effect and must stay behind the lazy match.
#[test]
fn index_or_does_not_evaluate_an_impure_default() {
    run_outputs(
        "fn side() Int {\n\
         \tprintln('evaluated')\n\
         \t0\n\
         }\n\
         fn f(a Array(Int)) Int { a[0] or side() }\n\
         pub fn main() {\n\
         \tprintln(f([42]))\n\
         }\n",
        "42\n",
    );
}

/// Both encodings, plus every boundary: hit, past the end, negative, empty.
/// `False` is a nullary constructor, not a constant, so it exercises the
/// pushed-default path a grid walk's `row[x] or False` depends on.
#[test]
fn index_or_covers_both_encodings_and_every_boundary() {
    run_outputs(
        "fn f(a Array(Int), i Int) Int { a[i] or -1 }\n\
         pub fn main() {\n\
         \tprintln(f([7, 8], 0))\n\
         \tprintln(f([7, 8], 1))\n\
         \tprintln(f([7, 8], 5))\n\
         \tprintln(f([7, 8], 0 - 1))\n\
         \tprintln(f([], 0))\n\
         }\n",
        "7\n8\n-1\n-1\n-1\n",
    );
    run_outputs(
        "fn g(a Array(Bool)) Bool { a[9] or False }\n\
         pub fn main() {\n\
         \tprintln(g([True]))\n\
         }\n",
        "False\n",
    );
}

/// `examples/bench_typed.scrl` is the typed-opcode workload. Its output is
/// pinned here; its speed is not gated anywhere.
///
/// The spec's ≤610ms wall-clock gate was removed deliberately: it timed a debug
/// build against an absolute bar, never ran, and belongs in an interleaved
/// min-of-N bench, not here.
#[test]
fn bench_typed_output_is_pinned() {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/bench_typed.scrl");
    let out = common::run_al("run", &path);
    assert!(out.success, "bench_typed failed:\n{}", out.combined());
    assert_eq!(out.stdout, "1048576\nTrue\n1000009000022000000\n");
}

run_case! {
    // A closure directly in `main`'s body, inside a nested `if`/`match`
    // scope, capturing a local of that scope. `main` is an ordinary function
    // called from the entry frame, so this is a by-value `PushCapture` of a
    // frame local like any other. The script (REPL) emit path is the hazard:
    // there such a local was an entry-frame temp and `PushGlobal <slot>` read
    // whatever the toplevel emit had parked there. This pins that `main`'s
    // body never takes that path. Only single-use nested-scope locals hit
    // this, so each captured name is read exactly once.
    main_nested_scope_closure_capture: (
        "pub fn main() {\n\
         \tif True {\n\
         \t\tz = 7\n\
         \t\tf = fn() { z }\n\
         \t\tprintln(f())\n\
         \t} else { Nil }\n\
         \tmatch Ok(41) {\n\
         \t\tOk(y) -> {\n\
         \t\t\tw = y + 1\n\
         \t\t\tg = fn() { w }\n\
         \t\t\tprintln(g())\n\
         \t\t}\n\
         \t\tErr(_) -> Nil\n\
         \t}\n\
         }\n",
        "7\n42\n",
    ),
}

run_case! {
    // A `@vm` builtin named without being called is a first-class value: the
    // elaborator synthesises an eta wrapper over the opcode, as for a ctor
    // used as a value. Driven through the VM, not just the typechecker.
    builtin_bound_to_a_local_is_callable: (
        "import scarlet/string\n\
         pub fn main() {\n\
         \tf = string.length\n\
         \tprintln(f('abc'))\n\
         }\n",
        "3\n",
    ),
    builtin_passed_as_a_function_argument: (
        "import scarlet/array\n\
         import scarlet/string\n\
         pub fn main() {\n\
         \tprintln(array.map(['a', 'bb', 'ccc'], string.length))\n\
         }\n",
        "[1, 2, 3]\n",
    ),
    // A bare builtin as a value takes the identifier path, not
    // `module.member`.
    bare_builtin_as_value_is_callable: (
        "import scarlet/array\n\
         pub fn main() {\n\
         \teach = array.each\n\
         \teach([1, 2], println)\n\
         }\n",
        "1\n2\n",
    ),
    // The three above name the value at the *toplevel*; these two name it inside
    // a function body, where the wrapper is written into the deferral region
    // instead. That is new coverage of the shape — eta-expanding a builtin and a
    // constructor reached through a body, and calling the result.
    //
    // It is not a witness for the jump-over mispatch `tests/check_parity.rs`
    // pins: both cases pass against the unfixed compiler, because the mispatched
    // jump is never executed. Only the layout assertion catches that.
    builtin_as_a_value_inside_a_function_body: (
        "import scarlet/array\n\
         import scarlet/string\n\
         fn lens(xs Array(String)) Array(Int) {\n\
         \tarray.map(xs, string.length)\n\
         }\n\
         pub fn main() {\n\
         \tprintln(lens(['a', 'bb', 'ccc']))\n\
         }\n",
        "[1, 2, 3]\n",
    ),
    ctor_as_a_value_inside_a_function_body: (
        "import scarlet/array\n\
         type W { W(v Int) }\n\
         fn wrap(xs Array(Int)) Array(W) {\n\
         \tarray.map(xs, W)\n\
         }\n\
         fn total(ws Array(W)) Int {\n\
         \tarray.fold(ws, 0, fn(a, w) match w {\n\
         \t\tW(v) -> a + v\n\
         \t})\n\
         }\n\
         pub fn main() {\n\
         \tprintln(array.length(wrap([1, 2, 3])))\n\
         \tprintln(total(wrap([4, 5, 6])))\n\
         }\n",
        "3\n15\n",
    ),

    // T-767: `array.map(bs, wire.decode)` reaches the VM through an eta wrapper,
    // so its descriptor comes from `eta_wire_imm`'s `Op::WireDecode` arm and not
    // from the direct-call spine. T-337 landed both arms and exercised only the
    // encode one, leaving this path written and never run.
    //
    // HOW THE PAYLOAD IS PINNED, and the first attempt was wrong: a downstream
    // `match` on the fold does NOT reach the wrapper. Measured — `rs =
    // array.map(bs, wire.decode)` with an `Ok(S(n))` arm further down is still
    // refused with "the type `wire.decode` produces here is not known", because
    // inference does not flow back into the eta wrapper's return type. It is
    // pinned here by `decode_all`'s monomorphic signature instead, which fixes
    // `a` to `S` at the point the wrapper is minted.
    //
    // The output is the SUM of the decoded payloads, not a count: a descriptor
    // that decoded to the wrong shape changes 7, where `array.length` would
    // still read 2. `dis.rs` covers the missing-descriptor half; only running it
    // can see a wrong one.
    eta_wrapped_wire_decode_round_trips: (
        "import scarlet/array\n\
         import scarlet/wire\n\
         type S { S(n Int) }\n\
         fn decode_all(bs Array(Binary)) Array(Result(S, wire.DecodeError)) {\n\
         \tarray.map(bs, wire.decode)\n\
         }\n\
         pub fn main() {\n\
         \tbs = array.map([S(3), S(4)], wire.encode)\n\
         \tprintln(array.fold(decode_all(bs), 0, fn(acc, r) match r {\n\
         \t\tOk(S(n)) -> acc + n\n\
         \t\tErr(_) -> 0 - 1\n\
         \t}))\n\
         }\n",
        "7\n",
    ),

    // An opaque type from another module crosses the wire (decided 2026-08-22,
    // owner). `json.Doc` is `opaque { arena Binary, tape Binary, idx Int }`:
    // the descriptor walks its three fields and the decoder rebuilds the
    // cursor by a constructor this module cannot name. The rebuilt cursor is
    // then READ through the parser's own natives, which is what says the
    // three fields came back as the parser's encoding and not merely as three
    // values of the right types — a `Doc` built wrong answers `None` or the
    // wrong member, never 42.
    an_opaque_json_doc_round_trips_through_wire: (
        "import scarlet/json\n\
         import scarlet/wire\n\
         fn n_of(d json.Doc) Int {\n\
         \tmatch json.field(d, 'n') {\n\
         \t\tSome(v) -> json.int(v) or 0 - 1\n\
         \t\tNone -> 0 - 2\n\
         \t}\n\
         }\n\
         fn through_wire(doc json.Doc) Int {\n\
         \tmatch wire.decode(wire.encode(doc)) {\n\
         \t\tOk(back) -> n_of(back) + 1\n\
         \t\tErr(_) -> 0 - 3\n\
         \t}\n\
         }\n\
         pub fn main() {\n\
         \tprintln(match json.parse('{\"m\": 7, \"n\": 41}') {\n\
         \t\tOk(doc) -> through_wire(doc)\n\
         \t\tErr(_) -> 0 - 4\n\
         \t})\n\
         }\n",
        "42\n",
    ),
}
