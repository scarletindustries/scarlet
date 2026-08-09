//! End-to-end VM execution coverage: opcodes only reachable through real
//! `scan -> parse -> typecheck -> compile -> run` programs that the golden
//! examples don't already exercise. Each test pins a *discriminating* output so
//! a regression in the targeted opcode (not just "it ran") is caught.

mod common;
use common::run_outputs;

run_case! {
    // `range[i] or default` lowers to `Op::Index` plus an Option match. The golden
    // examples only index arrays (`numbers[0] or 0`); a lazy Range scrutinee takes
    // a distinct arm inside `Op::Index` (`range_elem` instead of `Seq::get`).
    // In-bounds yields the element; out-of-bounds yields the recovery value.
    range_index_or_else: (
        "r = 5..10\n\
         println(r[2] or -1)\n\
         println(r[99] or -1)\n",
        "7\n-1\n",
    ),

    // `range[i]` (no `or`) lowers to `Op::Index`, producing an Option. The Range
    // arm must offset from the start (`5 + 2 = 7`), and an out-of-bounds index must
    // read as `None`, not a wrapped value.
    range_index_option: (
        "r = 5..10\n\
         println(r[2])\n\
         println(r[99])\n",
        "Some(7)\nNone\n",
    ),

    // `range[a..b]` lowers to `Op::ArraySlice`. The Range arm keeps the result lazy
    // (`rs+start .. rs+end`) rather than materialising, so the slice of `5..10` at
    // `[1..3]` is `[6, 7]`.
    range_slice: (
        "r = 5..10\n\
         println(r[1..3])\n",
        "[6, 7]\n",
    ),

    // Matching a Range value against an array pattern `[h, ..t]` drives `Op::ElemAt`
    // (head) and `Op::SeqDrop` (tail) on a Range, not an Array. `SeqDrop` on a Range stays
    // O(1) (`s+n .. e`); reconstructing `[h, ..t]` must reproduce the full sequence.
    match_range_with_array_pattern: (
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
    ),

    // A polymorphic unary minus (`fn n(x) { -x }` with `x` left generic, constrained
    // only `Numeric`) compiles to the *unspecialized* `Op::Neg`, which dispatches on
    // the runtime tag. The same compiled function must negate an Int and a Float,
    // preserving the IEEE sign for the float.
    generic_unary_neg_dispatches_on_runtime_tag: (
        "fn n(x) { -x }\n\
         println(n(5))\n\
         println(n(0 - 7))\n\
         println(n(2.5))\n",
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
         println(down(5))\n\
         println(down(0))\n",
        "0\n0\n",
    ),

    // `string.split` with a non-empty delimiter takes `Op::StrSplit`'s `split(&delim)`
    // arm (the empty-delimiter char-explode arm is the one stdlib_string covers).
    // Trailing/empty fields are preserved, so `'a,,b,'` splits into four parts.
    string_split_nonempty_delimiter: (
        "import al/string\n\
         println(string.split('a,b,c', ','))\n\
         println(string.split('a,,b,', ','))\n\
         println(string.split('nodelim', 'X'))\n",
        "[a, b, c]\n[a, , b, ]\n[nodelim]\n",
    ),

    // `values_equal`'s Binary arm: compare structurally, byte for byte.
    binary_value_equality: (
        "println(<<1, 2, 3>> == <<1, 2, 3>>)\n\
         println(<<1, 2, 3>> == <<1, 2, 4>>)\n\
         println(<<1, 2>> != <<1, 2, 3>>)\n",
        "True\nFalse\nTrue\n",
    ),

    // `Op::DivFloat` is total: `x / 0.0 == 0.0`, mirroring the integer
    // `x / 0 == 0` convention, rather than Infinity/NaN.
    float_division_is_total: (
        "println(7.0 / 2.0)\n\
         println(1.0 / 0.0)\n\
         println(0.0 - 9.0 / 3.0)\n",
        "3.5\n0.0\n-3.0\n",
    ),

    // `Op::Index` yields `Option`: `None` out of bounds, not a wrap or panic.
    array_index_yields_option: (
        "xs = [10, 20, 30]\n\
         println(xs[1])\n\
         println(xs[99])\n",
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
         println(make(42))\n\
         println(make(7))\n",
        "42\n7\n",
    ),

    // `vm::inspect`'s multiline layout through the real binary, not just the
    // unit tests.
    inspect_multiline_structures_e2e: (
        "type Point {\n\tx Int\n\ty Int\n}\n\
         type Seg {
         	a Point
         	b Point
         }\n\
         println(Seg(a: Point(x: 1, y: 2), b: Point(x: 3, y: 4)))\n\
         println([[1, 2], [3, 4]])\n",
        "Seg {\n  a: Point{ x: 1, y: 2 },\n  b: Point{ x: 3, y: 4 }\n}\n[\n  [1, 2],\n  [3, 4]\n]\n",
    ),

    // Or-pattern alternatives are one logical binding: every alternative must
    // store into the same slot the arm body reads. Module scope's
    // shadow-gets-a-fresh-slot rule is the hazard, so both scopes are pinned,
    // matching on the first, middle, and last alternative.
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
    ),

    // Op::BinIndexOf: `from` is clamped into range and an empty needle matches
    // at the clamped start.
    binary_index_of: (
        "import al/binary\n\
         h = binary.from_string('abcabc')\n\
         println(binary.index_of(h, binary.from_string('bc'), 0))\n\
         println(binary.index_of(h, binary.from_string('bc'), 2))\n\
         println(binary.index_of(h, binary.from_string('xy'), 0))\n\
         println(binary.index_of(h, binary.from_string(''), 3))\n",
        "Some(1)\nSome(4)\nNone\nSome(3)\n",
    ),

    // Op::BinParseInt must reject an overflowing value as `Err(Nil)`, never a
    // wrapped int: AL arithmetic wraps, and this is the request-smuggling
    // defense.
    binary_parse_int: (
        "import al/binary.{Dec, Hex}\n\
         println(binary.parse_int(binary.from_string('255'), Dec))\n\
         println(binary.parse_int(binary.from_string('ff'), Hex))\n\
         println(binary.parse_int(binary.from_string('FF'), Hex))\n\
         println(binary.parse_int(binary.from_string('99999999999999999999'), Dec))\n\
         println(binary.parse_int(binary.from_string('12x'), Dec))\n\
         println(binary.parse_int(binary.from_string(''), Dec))\n",
        "Ok(255)\nOk(255)\nOk(255)\nErr(Nil)\nErr(Nil)\nErr(Nil)\n",
    ),

    // Op::BinEqIgnoreAsciiCase: ASCII-case-insensitive header-name matching.
    binary_eq_ignore_ascii_case: (
        "import al/binary\n\
         a = binary.from_string('Content-Length')\n\
         println(binary.eq_ignore_ascii_case(a, binary.from_string('content-length')))\n\
         println(binary.eq_ignore_ascii_case(binary.from_string('abc'), binary.from_string('abd')))\n",
        "True\nFalse\n",
    ),

    // Op::BinToAsciiLower: non-letter bytes pass through.
    binary_to_ascii_lower: (
        "import al/binary\n\
         println(binary.to_string(binary.to_ascii_lower(binary.from_string('AbC-123'))))\n",
        "Ok(abc-123)\n",
    ),

    // Op::BinFromIntAscii: radix 10/16, lowercase hex, zero and negatives.
    binary_from_int_ascii: (
        "import al/binary.{Dec, Hex}\n\
         println(binary.to_string(binary.from_int_ascii(255, Dec)))\n\
         println(binary.to_string(binary.from_int_ascii(255, Hex)))\n\
         println(binary.to_string(binary.from_int_ascii(0, Dec)))\n\
         println(binary.to_string(binary.from_int_ascii(0 - 42, Dec)))\n\
         println(binary.parse_int(binary.from_int_ascii(4096, Hex), Hex))\n",
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
         add5 = make_adder(5)\n\
         add10 = make_adder(10)\n\
         println(add5(3))\n\
         println(add10(3))\n",
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
         f = make_affine(2, 3)\n\
         g = make_affine(5, 1)\n\
         println(f(10))\n\
         println(g(10))\n",
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
         p = counter_from(1)\n\
         q = counter_from(2)\n\
         println(p(5))\n\
         println(q(5))\n",
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
         println(False && loud(True))\n\
         println(True || loud(False))\n",
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
         println(True && loud(False))\n\
         println(False || loud(True))\n",
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
         println(1 != 2)\n\
         println(1 != 1)\n\
         println(Good('x') != Good('y'))\n\
         println(Good('x') != Good('x'))\n",
        "True\nFalse\nTrue\nFalse\n",
    );
}

#[test]
fn eq_on_string_array_tuple() {
    // Generic `Op::Eq` as a value-producing expression over each compound
    // kind, both directions.
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

// Type-directed opcodes and their dynamic fallbacks. The compiler picks a
// specialized opcode when `engine.find(ty)` resolves at emit time and falls
// back to the tag-dispatching generic op when it stays a `Var`. Each pair
// below drives one operation through both halves; they must agree, so a bug in
// either flips exactly one test.

run_case! {
    // Both directions per op, so an always-True, always-False, or
    // operand-swapped implementation flips exactly one line.
    typed_int_ordering_compares: (
        "println(1 < 2)\n\
         println(2 < 1)\n\
         println(2 > 1)\n\
         println(1 > 2)\n\
         println(2 <= 2)\n\
         println(3 <= 2)\n\
         println(2 >= 2)\n\
         println(1 >= 2)\n",
        "True\nFalse\nTrue\nFalse\nTrue\nFalse\nTrue\nFalse\n",
    ),

    // As above for Float. The `<=`/`>=` lines use equal operands, so a
    // strict-compare mislowering fails them.
    typed_float_ordering_compares: (
        "println(1.5 < 2.5)\n\
         println(2.5 < 1.5)\n\
         println(2.5 > 1.5)\n\
         println(1.5 > 2.5)\n\
         println(2.0 <= 2.0)\n\
         println(3.0 <= 2.0)\n\
         println(2.0 >= 2.0)\n\
         println(1.0 >= 2.0)\n",
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
         println(lt(1, 2))\n\
         println(lt(2, 1))\n\
         println(gt(2, 1))\n\
         println(gt(1, 2))\n\
         println(le(2, 2))\n\
         println(le(3, 2))\n\
         println(ge(2, 2))\n\
         println(ge(1, 2))\n\
         println(lt(1.5, 2.5))\n\
         println(gt(2.5, 1.5))\n\
         println(le(2.0, 2.0))\n\
         println(ge(1.0, 2.0))\n",
        "True\nFalse\nTrue\nFalse\nTrue\nFalse\nTrue\nFalse\n\
         True\nTrue\nTrue\nFalse\n",
    ),

    // `Op::CallKnown` carries `func_idx` in its operand with no callee on the
    // stack. `twice`'s inner `inc(x)` is non-tail, the outer one is tail.
    direct_call_to_known_top_level_fn: (
        "fn inc(x Int) Int { x + 1 }\n\
         fn twice(x Int) Int { inc(inc(x)) }\n\
         println(twice(5))\n\
         println(inc(0 - 3))\n",
        "7\n-2\n",
    ),

    // `Op::TailCallKnown`. At n = 200_000 a frame-pushing lowering overflows
    // the stack, so terminating at all is the assertion.
    mutual_tail_recursion_between_known_fns: (
        "fn even(n Int) Bool { if n == 0 { True } else { odd(n - 1) } }\n\
         fn odd(n Int) Bool { if n == 0 { False } else { even(n - 1) } }\n\
         println(even(200000))\n\
         println(odd(200000))\n\
         println(even(7))\n",
        "True\nFalse\nFalse\n",
    ),

    // A callee that is a runtime value falls back to dynamic `Op::Call`.
    // `apply` compiles once but dispatches to two bodies, so a lowering that
    // baked in either target fails one line.
    indirect_call_through_value_is_dynamic: (
        "fn inc(x Int) Int { x + 1 }\n\
         fn dbl(x Int) Int { x * 2 }\n\
         fn apply(f fn(Int) Int, x Int) Int { f(x) }\n\
         println(apply(inc, 5))\n\
         println(apply(dbl, 5))\n",
        "6\n10\n",
    ),

    // `count` alternates `hop` (`TailCallKnown`) with `f` (dynamic
    // `TailCall`); at n = 200_000 both halves must reuse the frame.
    indirect_tail_call_through_value_reuses_frame: (
        "fn hop(f fn(Int, Int) Int, acc Int, n Int) Int {\n\
         \tif n == 0 { acc } else { f(acc + 1, n - 1) }\n\
         }\n\
         fn count(acc Int, n Int) Int { hop(count, acc, n) }\n\
         println(count(0, 200000))\n",
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
         println(pick(A(3)))\n\
         println(pick(B(4)))\n\
         println(pick(C(5, 6)))\n",
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
         println(pick(A(0)))\n\
         println(pick(A(3)))\n\
         println(pick(B(4)))\n",
        "999\n3\n40\n",
    ),

    // `.field` on a resolved record lowers to `Op::GetFieldUnchecked`. Three
    // field indices pin the operand encoding; the `..p` spread projects the
    // unnamed fields through the same op.
    record_field_access_unchecked: (
        "type P {\n\tx Int\n\ty Int\n\tz Int\n}\n\
         p = P(x: 10, y: 20, z: 30)\n\
         println(p.x)\n\
         println(p.y)\n\
         println(p.z)\n\
         q = P(y: 99, ..p)\n\
         println(q.x)\n\
         println(q.y)\n\
         println(q.z)\n",
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
         println(sum(A(1, 2)))\n\
         println(sum(B(10, 20)))\n\
         println(sum(C(100, 200)))\n",
        "3\n30\n300\n",
    ),
}

// Perceus drop-guided reuse (ICFP'22, frame-limited): a `map` over a uniquely
// owned linked list reuses each `Cons` cell in place, so its allocation count
// is independent of list length. When the list is shared the runtime
// `is_unique()` check fails, every `Cons` allocates fresh, cost scales with
// length, and the aliased original reads back unchanged.
//
// These are the only vm_exec cases that run the VM in-process: they read
// `ProcHeap::alloc_count()`, which is thread-local, so a mutex serializes
// them. Every other test here spawns a subprocess and cannot touch it.

use al::heap::ProcHeap;
use al::{bytecode, vm};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Mutex;

// bench_typed's speed depends on `AddInt`/`SubInt`/`EqInt` firing instead of
// tag-dispatching `Add`/`Sub`/`Eq`. Catches lowered locals carrying unbound
// type-vars, where `resolved_prim` never sees the concrete `Int`.
#[test]
fn core_ir_lower_selects_typed_int_ops() {
    use bytecode::Op;
    // `f`'s body is the bench_typed hot shape; `sq(n)` adds the
    // Callee::Known → return-type path.
    let src = "\
fn sq(x Int) Int { x * x }\n\
fn f(n Int) Int {\n\
\tif n == 0 { 0 } else { sq(n) + n - 1 }\n\
}\n\
f(3)\n";
    let ast = common::parse(src);
    let r = bytecode::compile(&ast, None, Some(&al::STDLIB));
    assert!(r.success(), "compile failed: {:?}", r.diagnostics);
    let r = r.emitted.expect("a successful compile emits");
    // Restrict to `f`'s bytecode range so stdlib generics don't false-positive.
    let f = r
        .program
        .functions
        .iter()
        .find(|fun| &*fun.name == "f")
        .expect("fn f in program");
    let (s, l) = (f.code_start as usize, f.code_len as usize);
    let ops: Vec<Op> = r.program.code[s..s + l].iter().map(|i| i.op).collect();
    let has = |o: Op| ops.contains(&o);
    // `emit` and the peephole fold `local OP const` into `*LC`
    // superinstructions, and an `EqInt` feeding a branch becomes
    // `JumpNeIntLC`, so accept either form.
    assert!(
        has(Op::EqInt) || has(Op::JumpNeIntLC),
        "EqInt not selected: {ops:?}"
    );
    assert!(
        has(Op::AddInt) || has(Op::AddIntLC),
        "AddInt not selected: {ops:?}"
    );
    assert!(
        has(Op::SubInt) || has(Op::SubIntLC),
        "SubInt not selected: {ops:?}"
    );
    for generic in [Op::Eq, Op::Add, Op::Sub] {
        assert!(
            !has(generic),
            "generic {generic:?} leaked (typed selection did not fire): {ops:?}"
        );
    }
}

/// Same requirement for a type nothing in the source states: `v` is `Int` only
/// because inference unified `Some`'s payload with the literal `3`, so `lower`
/// must read that back rather than re-instantiate `Some`'s scheme.
///
/// `fn g(a, b) { a + b }` would not test this: it really is
/// `Addable a => (a, a) -> a`, one body for every instantiation, and dynamic
/// `Add` is correct there. The failure is unresolved monomorphic types.
#[test]
fn typed_ops_fire_on_a_type_that_only_inference_knows() {
    use bytecode::Op;
    let src = "\
fn f() Int {\n\
\tmatch Some(3) {\n\
\t\tNone -> 0\n\
\t\tSome(v) -> v + 1\n\
\t}\n\
}\n\
println(f())\n";
    let ast = common::parse(src);
    let r = bytecode::compile(&ast, None, Some(&al::STDLIB));
    assert!(r.success(), "compile failed: {:?}", r.diagnostics);
    let r = r.emitted.expect("a successful compile emits");
    let f = r
        .program
        .functions
        .iter()
        .find(|fun| &*fun.name == "f")
        .expect("fn f in program");
    let (s, l) = (f.code_start as usize, f.code_len as usize);
    let ops: Vec<Op> = r.program.code[s..s + l].iter().map(|i| i.op).collect();
    assert!(
        ops.contains(&Op::AddInt) || ops.contains(&Op::AddIntLC),
        "AddInt not selected for an inferred Int: {ops:?}"
    );
    assert!(
        !ops.contains(&Op::Add),
        "dynamic Add leaked into f's body: {ops:?}"
    );
}

/// Serializes the Perceus alloc-counting tests: `ProcHeap::alloc_count()`
/// is thread-local and each test does reset→run→read.
static ALLOC_LOCK: Mutex<()> = Mutex::new(());

/// Linked-list scaffold for the reuse tests. `lmap` is the canonical Perceus
/// shape: destructure a `Cons`, construct a same-shape `Cons`, so the dropped
/// cell and the constructor pair up frame-locally, never across a call.
const LIST_SRC: &str = "\
type List {\n\
\tLNil\n\
\tLCons(head Int, tail List)\n\
}\n\
fn build(n Int) List {\n\
\tif n == 0 { LNil } else { LCons(n, build(n - 1)) }\n\
}\n\
fn lmap(xs List, f fn(Int) Int) List {\n\
\tmatch xs {\n\
\t\tLNil -> LNil\n\
\t\tLCons(h, t) -> LCons(f(h), lmap(t, f))\n\
\t}\n\
}\n\
fn lsum(xs List) Int {\n\
\tmatch xs {\n\
\t\tLNil -> 0\n\
\t\tLCons(h, t) -> h + lsum(t)\n\
\t}\n\
}\n\
fn double(x Int) Int { x * 2 }\n";

/// Compile `src` through the native backend's load-time pipeline, as `al run`
/// does: a Cranelift plan per mode-selected body, JITed and published into the
/// program's `NativeTable`.
///
/// `must_native` names the functions the caller's parity claim rides on. Its
/// table slot must be filled whenever `AL_NATIVE` selected it; otherwise a
/// coverage-gate rejection silently interprets the "native" run and the parity
/// assertion compares the interpreter to itself.
fn compile_native(src: &str, must_native: &[&str]) -> bytecode::Program {
    use al::core_ir::clif;
    use al::tivec::Idx as _;
    let ast = common::parse(src);
    let plans: Rc<RefCell<Vec<clif::NativePlan>>> = Rc::default();
    let sink = Rc::clone(&plans);
    let r = bytecode::compile_with_native(
        &ast,
        None,
        Some(&al::STDLIB),
        Box::new(move |idx, f, pool| {
            if let Some(p) = clif::plan(idx, f, pool, al::STDLIB.prelude) {
                sink.borrow_mut().push(p);
            }
        }),
    );
    assert!(
        r.success(),
        "compile failed: {:?}\n---\n{src}",
        r.diagnostics
    );
    let program = r.emitted.expect("a successful compile emits").program;

    let plans = plans.take();
    if !plans.is_empty() {
        let mut module = vm::jit::jit_module().expect("jit module");
        let native = clif::native_set(&plans, &program);
        let mut defs = Vec::with_capacity(plans.len());
        for plan in &plans {
            let body = clif::compile(&mut module, plan, &native, &program).expect("clif define");
            if let Some(body) = body {
                let name = program
                    .functions
                    .get(body.func_idx.index())
                    .map(|f| f.name.to_string())
                    .unwrap_or_default();
                defs.push(vm::jit::JitDef {
                    fn_idx: body.func_idx,
                    func_id: body.func_id,
                    name,
                    code_size: body.code_size,
                });
            }
        }
        vm::jit::finalize_into(&mut module, &defs, &program.native).expect("jit finalize");
        // Dropping the module keeps the executable mapping alive, so the
        // published entries outlive this scope (see vm::jit).
    }

    let cfg = bytecode::native::config();
    for name in must_native {
        let pos = program
            .functions
            .iter()
            .position(|f| &*f.name == *name)
            .unwrap_or_else(|| panic!("fn {name} in program"));
        let idx = al::core_ir::FuncIdx::from_usize(pos);
        if cfg.includes(idx) {
            assert!(
                program.native.get(idx).is_some(),
                "AL_NATIVE mode {:?} selected `{name}` but no native body was \
                 published (coverage gate rejected it?); the native half of this \
                 alloc-count parity test would silently interpret",
                cfg.mode
            );
        }
    }
    program
}

/// Run an already-built `program`, returning `(total ProcHeap allocations
/// during the run, rendered result value)`. Caller holds `ALLOC_LOCK`.
fn count_run(program: bytecode::Program) -> (usize, String) {
    ProcHeap::reset_alloc_count();
    let mut v = vm::new_vm(program).expect("vm init");
    let val = v.run().expect("vm run");
    let allocs = ProcHeap::alloc_count();
    (allocs, vm::inspect(&val, v.program()))
}

/// Run `src` twice — interpreter-only, then with the native backend published
/// — and assert the Perceus parity gate: identical result and identical exact
/// allocation count. Caller holds `ALLOC_LOCK`.
fn run_counting_allocs(src: &str, must_native: &[&str]) -> (usize, String) {
    let ast = common::parse(src);
    let r = bytecode::compile(&ast, None, Some(&al::STDLIB));
    assert!(
        r.success(),
        "compile failed: {:?}\n---\n{src}",
        r.diagnostics
    );
    let r = r.emitted.expect("a successful compile emits");
    let (interp_allocs, interp_out) = count_run(r.program);

    let (native_allocs, native_out) = count_run(compile_native(src, must_native));
    assert_eq!(
        interp_out, native_out,
        "native result diverged from the interpreter for:\n{src}"
    );
    assert_eq!(
        interp_allocs, native_allocs,
        "Perceus parity: native allocated {native_allocs} cells vs the \
         interpreter's {interp_allocs} for:\n{src}"
    );
    (interp_allocs, interp_out)
}

/// `a[i] or <default>` must not build the `Some` box that `Index` returns and
/// the next instruction throws away. `Op::IndexOr` fuses the pair; a constant
/// default rides in the operand, so nothing is pushed on a hit. A regression
/// here is invisible in results — it only shows up as one alloc per index.
#[test]
fn index_or_default_allocates_nothing() {
    let _g = ALLOC_LOCK.lock().unwrap();
    let prog = |body: &str| {
        format!(
            "fn go(a Array(Int), i Int, acc Int) Int {{\n\tif i == 0 {{ acc }} else {{ go(a, i - 1, {body}) }}\n}}\ngo([1, 2, 3], 2000, 0)\n"
        )
    };
    // `go` sits outside the native coverage gate, so no `must_native` claim;
    // the parity assertion still holds either way.
    let (with_or, out) = run_counting_allocs(&prog("acc + { a[0] or 0 }"), &[]);
    let (baseline, _) = run_counting_allocs(&prog("acc + 1"), &[]);
    assert_eq!(out.trim(), "2000");
    assert_eq!(
        with_or,
        baseline,
        "`a[0] or 0` allocated {} cells over 2000 iterations; the fused \
         Op::IndexOr must build no Option box",
        with_or as i64 - baseline as i64
    );
}

/// The fused op evaluates its default eagerly, so `lower` may only fuse a
/// *pure* one. A call has an effect and must stay behind the lazy match.
#[test]
fn index_or_does_not_evaluate_an_impure_default() {
    let src = "fn side() Int {\n\tprintln('evaluated')\n\t0\n}\nfn f(a Array(Int)) Int { a[0] or side() }\nprintln(f([42]))\n";
    run_outputs(src, "42\n");
}

/// Both encodings, plus every boundary: hit, past the end, negative, empty.
/// `False` is a nullary constructor, not a constant, so it exercises the
/// pushed-default path a grid walk's `row[x] or False` depends on.
#[test]
fn index_or_covers_both_encodings_and_every_boundary() {
    run_outputs(
        "fn f(a Array(Int), i Int) Int { a[i] or -1 }\nprintln(f([7, 8], 0))\nprintln(f([7, 8], 1))\nprintln(f([7, 8], 5))\nprintln(f([7, 8], 0 - 1))\nprintln(f([], 0))\n",
        "7\n8\n-1\n-1\n-1\n",
    );
    run_outputs(
        "fn g(a Array(Bool)) Bool { a[9] or False }\nprintln(g([True]))\n",
        "False\n",
    );
}

#[test]
fn list_map_unique_reuses_in_place() {
    let _g = ALLOC_LOCK.lock().unwrap();
    // `chain` re-maps its uniquely owned argument `k` times, so every `Cons`
    // is rc==1 at its drop. Varying only `k` isolates map's per-call cost;
    // build/sum contribute equally to both runs and cancel.
    let prog = |k: u32| {
        format!(
            "{LIST_SRC}\
             fn chain(xs List, k Int) List {{\n\
             \tif k == 0 {{ xs }} else {{ chain(lmap(xs, double), k - 1) }}\n\
             }}\n\
             lsum(chain(build(100), {k}))\n"
        )
    };
    // The reuse shapes under test live in the LIST_SRC fns; `chain` is only
    // the driver, so it carries no must-native claim (interpreted or native,
    // the alloc-count parity assertion binds either way).
    let native = &["build", "lmap", "lsum", "double"];
    let (a1, r1) = run_counting_allocs(&prog(1), native);
    let (a10, r10) = run_counting_allocs(&prog(10), native);
    // Correctness: build(100) sums to 5050; each map doubles every element.
    assert_eq!(r1, "10100", "1× doubled sum");
    assert_eq!(r10, "5171200", "10× doubled sum");
    // Nine additional `lmap` passes over 100 cells must allocate a *constant*
    // (length-independent) amount. Without reuse the delta is 9×100 = 900
    // fresh Cons cells; a bound of `< len` (100) discriminates decisively
    // while tolerating a handful of per-pass constants (e.g. LNil).
    let delta = a10.saturating_sub(a1);
    assert!(
        delta < 100,
        "unique list.map allocated per-element: Δ={delta} for 9 extra passes over 100 cells \
         (reuse ⇒ length-independent Δ; no-reuse ⇒ ~900)"
    );
}

#[test]
fn list_map_shared_falls_back_to_alloc() {
    let _g = ALLOC_LOCK.lock().unwrap();
    // Binding the list to a second name keeps every `Cons` at rc>=2, so the
    // runtime `is_unique()` check inside `lmap` is false at every drop and the
    // constructor allocates fresh. Varying ONLY `n` isolates the per-element
    // cost of build+map together.
    let prog = |n: u32| {
        format!(
            "{LIST_SRC}\
             xs = build({n})\n\
             alias = xs\n\
             ys = lmap(xs, double)\n\
             (lsum(alias), lsum(ys))\n"
        )
    };
    let native = &["build", "lmap", "lsum", "double"];
    let (a20, r20) = run_counting_allocs(&prog(20), native);
    let (a200, r200) = run_counting_allocs(&prog(200), native);
    // Correctness: `alias` still reads the ORIGINAL values — proving the
    // shared cells were not overwritten in place — and `ys` reads doubled.
    assert_eq!(r20, "(210, 420)");
    assert_eq!(r200, "(20100, 40200)");
    // Scales with length: 180 more elements ⇒ ~180 build Cons + ~180 map Cons
    // = ~360. A (wrong) reuse-regardless-of-rc implementation would show ~180
    // AND corrupt `alias` above; the bound of ≥300 separates the two by >100.
    let delta = a200.saturating_sub(a20);
    assert!(
        delta >= 300,
        "shared list.map did not fall back to fresh allocation: Δ={delta} for 180 extra \
         elements (fallback ⇒ ~360 = build+map; wrongly reused ⇒ ~180)"
    );
}

// ===========================================================================
// Bench gate (docs/core-ir-spec.md §Constraints): after the Core-IR perceus
// pass is on, `dot_loop` must show measurable reuse (alloc counter ≪ 2N) and
// `examples/bench_typed.al` must complete in ≤0.61s under a release build.
// Phase 2's AST-walker perceus regressed 0.61→0.77s because last-use is
// unknowable during a forward AST emit — it hedged Nop holes on every read
// with no compensating reuse in `dot_loop` (construct-then-drop, so no drop
// dominates a same-shape ctor). ANF makes last-use a linear backward scan and
// enables loop-carried pairing across `TailCallSelf`.
// ===========================================================================

/// `dot_loop` from `examples/bench_typed.al`, minus the `println` (the
/// in-process VM here returns the final value directly).
const DOT_SRC: &str = "\
type Point {\n\
\tPoint(x Int, y Int, z Int)\n\
}\n\
fn dot(a Point, b Point) Int {\n\
\ta.x * b.x + a.y * b.y + a.z * b.z\n\
}\n\
fn dot_loop(n Int, acc Int) Int {\n\
\tif n == 0 {\n\
\t\tacc\n\
\t} else {\n\
\t\tp = Point(n, n + 1, n + 2)\n\
\t\tq = Point(n + 3, n + 4, n + 5)\n\
\t\tdot_loop(n - 1, acc + dot(p, q))\n\
\t}\n\
}\n";

#[test]
fn dot_loop_perceus_reuse_gate() {
    let _g = ALLOC_LOCK.lock().unwrap();
    // Two `Point(x y z)` per iteration: without loop-carried reuse that is 2N
    // fresh heap objects. With the Core-IR perceus pass pairing end-of-body
    // drops of `p`/`q` with the next iteration's `Point(..)` constructors
    // across `TailCallSelf` (spec §Constraints "Loop-carried reuse" —
    // `collapse_tail_frame` must not drain reuse slots for self-tail-calls),
    // the two cells are overwritten in place and allocation is O(1) in N.
    // N=10_000 discriminates decisively (20_000 vs. constant) while keeping
    // the debug run fast; the spec's ≪2M for N=1M is this same ratio.
    const N: u64 = 10_000;
    let (allocs, r) = run_counting_allocs(
        &format!("{DOT_SRC}dot_loop({N}, 0)\n"),
        &["dot", "dot_loop"],
    );
    // ∑ₙ₌₁ᴺ 3n²+15n+14 at N=10_000.
    assert_eq!(r, "1000900220000", "dot_loop correctness at N={N}");
    // ≪ 2N: loop-carried reuse ⇒ a handful of fixed allocs; no reuse ⇒ 2N.
    // A bound of N/10 separates the two by an order of magnitude while
    // tolerating any per-run constants.
    assert!(
        allocs < (N as usize) / 10,
        "dot_loop allocated per-iteration: {allocs} allocs for {N} iterations \
         (loop-carried reuse ⇒ O(1); no reuse ⇒ {}; bench gate not met)",
        2 * N
    );
}

/// `examples/bench_typed.al` is the typed-opcode workload. Its *output* is
/// pinned here; its *speed* is not.
///
/// There used to be a `#[ignore]`d wall-clock gate asserting ≤610ms. It timed
/// the debug binary against an absolute bar, on a machine whose load average
/// has ranged 3–170, and it never ran. A benchmark belongs in an interleaved
/// min-of-N comparison against a named parent commit, not in the unit suite —
/// and a gate nobody runs reads as coverage it does not provide.
#[test]
fn bench_typed_output_is_pinned() {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/bench_typed.al");
    let out = common::run_al("run", &path);
    assert!(out.success, "bench_typed failed:\n{}", out.combined());
    assert_eq!(out.stdout, "1048576\nTrue\n1000009000022000000\n");
}

/// Emit-level half of the same gate: `Op::Reuse` must actually be *in*
/// `dot_loop`'s code, paired with a `MakeEnumPayload a=1` (the reuse-token
/// form of the constructor), once for each of `p` and `q`. The alloc-counter
/// test above proves the runtime effect; this one names the mechanism, so a
/// regression that (say) lets `peel_call_arg_drops` hand `p`/`q` to `dot` —
/// zeroing the slot before `Reuse` reads it — fails here with a readable
/// cause rather than as a mysterious allocation count.
#[test]
fn dot_loop_emits_paired_reuse() {
    use bytecode::Op;
    let ast = common::parse(&format!("{DOT_SRC}dot_loop(10, 0)\n"));
    let r = bytecode::compile(&ast, None, Some(&al::STDLIB));
    assert!(r.success(), "compile failed: {:?}", r.diagnostics);
    let r = r.emitted.expect("a successful compile emits");
    let f = r
        .program
        .functions
        .iter()
        .find(|fun| &*fun.name == "dot_loop")
        .expect("fn dot_loop in program");
    let (s, l) = (f.code_start as usize, f.code_len as usize);
    let code = &r.program.code[s..s + l];

    let reuses = code.iter().filter(|i| i.op == Op::Reuse).count();
    assert_eq!(
        reuses,
        2,
        "expected one `Reuse` per loop-carried `Point`, got {reuses}: {:?}",
        code.iter().map(|i| i.op).collect::<Vec<_>>()
    );
    // Every `Point(..)` in the body must consume one: `a = 1` is the flag that
    // makes `MakeEnumPayload` pop a reuse token instead of allocating fresh.
    let makes: Vec<u8> = code
        .iter()
        .filter(|i| i.op == Op::MakeEnumPayload)
        .map(|i| i.a)
        .collect();
    assert_eq!(
        makes,
        vec![1, 1],
        "both `Point` ctors must take a reuse token"
    );
    // The token has to reach the constructor: `Reuse` is the instruction
    // immediately before its `MakeEnumPayload` (payloads are already pushed).
    for (i, ins) in code.iter().enumerate() {
        if ins.op == Op::Reuse {
            assert_eq!(
                code[i + 1].op,
                Op::MakeEnumPayload,
                "`Reuse` at {i} is not immediately followed by its constructor"
            );
        }
    }
    // And the drops that make them reusable must NOT have been peeled into
    // `dot`'s call: they stay in this frame, after the `CallKnown`.
    let call = code
        .iter()
        .position(|i| i.op == Op::CallKnown)
        .expect("dot(p, q) is a CallKnown");
    let drops_after = code[call..].iter().filter(|i| i.op == Op::Drop).count();
    assert_eq!(
        drops_after, 2,
        "`p`/`q` drops must stay in dot_loop's frame (reuse tokens), not be \
         peeled into `dot`'s arguments"
    );
}

run_case! {
    // A closure written at *module* toplevel, but inside a nested `if`/`match`
    // scope, capturing a local of that scope. Such a local is an entry-frame temp,
    // not a published global: the closure must take it as a by-value capture
    // (`PushCapture`), exactly as it would inside a fn body. Resolving it to
    // `PushGlobal <entry-frame slot>` reads whatever the toplevel Core emit happens
    // to have parked in that slot — the `if` condition, an intermediate temp, ...
    // The bug only surfaced for single-use nested-scope locals, so read each
    // captured name exactly once.
    toplevel_nested_scope_closure_capture: (
        "if True {\n\
        \x20 z = 7\n\
        \x20 f = fn() { z }\n\
        \x20 println(f())\n\
         } else { Nil }\n\
         \n\
         match Ok(41) {\n\
        \x20 Ok(y) -> {\n\
        \x20   w = y + 1\n\
        \x20   g = fn() { w }\n\
        \x20   println(g())\n\
        \x20 }\n\
        \x20 Err(_) -> Nil\n\
         }\n",
        "7\n42\n",
    ),
}

run_case! {
    // A `@vm` builtin named without being called is a first-class value: the
    // elaborator synthesises an eta wrapper (`typed_ir::eta`) over the opcode,
    // the same as for a constructor used as a value. Bind it, pass it, call it
    // through the VM — not just the typechecker.
    builtin_bound_to_a_local_is_callable: (
        "import al/string\n\
         f = string.length\n\
         println(f('abc'))\n",
        "3\n",
    ),
    builtin_passed_as_a_function_argument: (
        "import al/array\n\
         import al/string\n\
         println(array.map(['a', 'bb', 'ccc'], string.length))\n",
        "[1, 2, 3]\n",
    ),
    // A bare (unqualified) builtin as a value — `println` itself — exercises
    // the identifier path rather than the `module.member` path.
    bare_builtin_as_value_is_callable: (
        "import al/array\n\
         each = array.each\n\
         each([1, 2], println)\n",
        "1\n2\n",
    ),
}
