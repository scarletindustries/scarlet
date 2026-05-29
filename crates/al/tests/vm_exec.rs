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
        "range_index_or_else",
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
        "range_index_option",
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
        "range_slice",
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
        "match_range_array_pat",
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
        "generic_neg",
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
        "push_self",
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
        "split_nonempty",
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
        "binary_eq",
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
        "float_div",
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
        "array_index_option",
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
        "push_self_capture",
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
        "inspect_multiline",
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
        "or_pattern_bindings",
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
