//! Type-system and language-semantics coverage: `type` declarations,
//! constructors as values, exhaustiveness, refutability, `or`/field/`if`
//! typing, rigid type variables, record update, match guards, or-patterns,
//! binary patterns, typed discard. Stdlib runtime goldens live in
//! `tests/stdlib.rs`; opcode-level VM execution lives in `tests/vm_exec.rs`.

mod common;
use common::{check_ok, check_rejects, run_outputs};

run_case! {
    type_keyword_single_variant: (
        "type User { User(name String, age Int) }\n\
         pub fn main() {\n\
         \tu = User(name: 'al', age: 18)\n\
         \tprintln(u.name)\n\
         \tprintln(u.age)\n\
         }\n",
        "al\n18\n",
    ),

    type_keyword_multi_variant: (
        "type Shape {\n\tCircle(r Int)\n\tRect(w Int, h Int)\n}\n\
         fn area(s Shape) Int {\n\
         \tmatch s {\n\
         \t\tCircle(r) -> 3 * r * r\n\
         \t\tRect(w, h) -> w * h\n\
         \t}\n\
         }\n\
         pub fn main() {\n\
         \tprintln(area(Circle(10)))\n\
         \tprintln(area(Rect(4, 5)))\n\
         }\n",
        "300\n20\n",
    ),

    type_alias_is_transparent: (
        "type Id = Int\n\
         fn next(i Id) Id { i + 1 }\n\
         pub fn main() {\n\
         \tprintln(next(41))\n\
         }\n",
        "42\n",
    ),
}

reject_case! {
    /// Every field in a type definition must carry a label.
    unlabeled_field_in_def_is_rejected: (
        "type Wrap { Wrap(Int) }\n\
         pub fn main() {\n\
         \t_ = Wrap(1)\n\
         }\n",
        "constructor fields must be labeled",
    ),
}

run_case! {
    some_call_is_ordinary_call: (
        "pub fn main() {\n\
         \tx = Some(5)\n\
         \tprintln(x or 0)\n\
         }\n",
        "5\n",
    ),

    constructor_is_first_class: (
        "fn map(f fn(a) b, xs Array(a)) Array(b) {\n\
         \tmatch xs {\n\
         \t\t[] -> []\n\
         \t\t[h, ..t] -> [f(h), ..map(f, t)]\n\
         \t}\n\
         }\n\
         pub fn main() {\n\
         \tys = map(Some, [1, 2, 3])\n\
         \tprintln(ys[0] or None)\n\
         \tprintln(ys[2] or None)\n\
         }\n",
        "Some(1)\nSome(3)\n",
    ),

    nullary_constructor_is_value: (
        "pub fn main() {\n\
         \tx = None\n\
         \tprintln(x or 7)\n\
         }\n",
        "7\n",
    ),
}

reject_case! {
    // `if` requires `else`
    if_without_else_is_error: (
        "pub fn main() {\n\
         \tx = if True { 1 }\n\
         \tprintln(x)\n\
         }\n",
        "else",
    ),
    // Parentheses are tuples-only
    empty_parens_is_parse_error: (
        "pub fn main() {\n\
         \tx = ()\n\
         \tprintln(x)\n\
         }\n",
        "tuples need 2+ elements",
    ),
    single_parens_is_parse_error: (
        "pub fn main() {\n\
         \tx = (5)\n\
         \tprintln(x)\n\
         }\n",
        "single-element parens not allowed",
    ),
}

run_case! {
    block_is_grouping: (
        "pub fn main() {\n\
         \tprintln({1 + 2} * 3)\n\
         }\n",
        "9\n",
    ),
}

run_case! {
    index_returns_option: (
        "pub fn main() {\n\
         \txs = [10, 20, 30]\n\
         \tprintln(xs[0] or -1)\n\
         \tprintln(xs[9] or -1)\n\
         }\n",
        "10\n-1\n",
    ),
}

reject_case! {
    /// `xs[0] + 1` should be rejected because `xs[0]` is `Option(Int)`, not `Int`.
    index_without_unwrap_is_option_typed: (
        "pub fn main() {\n\
         \txs = [10, 20, 30]\n\
         \ty = xs[0] + 1\n\
         \tprintln(y)\n\
         }\n",
        "got 'Option(Int)'",
    ),
}

#[test]
#[ignore = "needs the VM"]
fn index_negative_returns_none() {
    // A negative index is rejected by `Op::Index`'s own `idx >= 0` guard, a
    // different path from the out-of-bounds `arr.get` returning `None`.
    run_outputs(
        "pub fn main() {\n\
         \txs = [10, 20, 30]\n\
         \tprintln(xs[-1])\n\
         \tprintln(xs[-100] or -1)\n\
         \tprintln(xs[0])\n\
         }\n",
        "None\n-1\nSome(10)\n",
    );
}

#[test]
#[ignore = "needs the VM"]
fn slice_in_bounds_returns_subarray() {
    // A slice is an `Array(Int)`, not an `Option`.
    run_outputs(
        "pub fn main() {\n\
         \txs = [10, 20, 30, 40, 50]\n\
         \ts = xs[1..4]\n\
         \tprintln(s)\n\
         \tprintln(s[0] or -1)\n\
         \tprintln(s[2] or -1)\n\
         \tprintln(s[3] or -1)\n\
         }\n",
        "[20, 30, 40]\n20\n40\n-1\n",
    );
}

#[test]
#[ignore = "needs the VM"]
fn range_as_value_materializes() {
    // A bare `start..end` is a first-class `Array(Int)`. A reversed range
    // saturates to length 0 rather than a negative length or a crash.
    run_outputs(
        "pub fn main() {\n\
         \tprintln(0..5)\n\
         \tprintln(3..3)\n\
         \tprintln(5..2)\n\
         }\n",
        "[0, 1, 2, 3, 4]\n[]\n[]\n",
    );
}

run_case! {
    field_access_total_across_variants: (
        "type Named {\n\tPerson(name String, age Int)\n\tOrg(name String, size Int)\n}\n\
         fn name_of(n Named) String { n.name }\n\
         pub fn main() {\n\
         \tprintln(name_of(Person(name: 'al', age: 18)))\n\
         \tprintln(name_of(Org(name: 'anthropic', size: 1000)))\n\
         }\n",
        "al\nanthropic\n",
    ),
}

ok_case! {
    recursive_type_compiles: (
        "type Tree(a) {\n\tLeaf\n\tNode(l Tree(a), v a, r Tree(a))\n}\n\
         pub fn main() {\n\
         \tt Tree(Int) = Node(l: Leaf, v: 1, r: Leaf)\n\
         \tt\n\
         }\n",
    ),
}

run_case! {
    recursive_type_runs: (
        "type Tree(a) {\n\tLeaf\n\tNode(l Tree(a), v a, r Tree(a))\n}\n\
         fn size(t Tree(a)) Int {\n\
         \tmatch t {\n\
         \t\tLeaf -> 0\n\
         \t\tNode(l, _, r) -> 1 + size(l) + size(r)\n\
         \t}\n\
         }\n\
         pub fn main() {\n\
         \tt = Node(l: Node(l: Leaf, v: 1, r: Leaf), v: 2, r: Leaf)\n\
         \tprintln(size(t))\n\
         }\n",
        "2\n",
    ),
}

ok_case! {
    nested_option_match_is_exhaustive: (
        "pub fn main() {\n\
         \tx = Some(Some(5))\n\
         \tr = match x {\n\
         \t\tSome(Some(n)) -> 'ss ${n}'\n\
         \t\tSome(None) -> 'sn'\n\
         \t\tNone -> 'n'\n\
         \t}\n\
         \tprintln(r)\n\
         }\n",
    ),
}

run_case! {
    nested_option_match_runs: (
        "pub fn main() {\n\
         \tx = Some(Some(5))\n\
         \tr = match x {\n\
         \t\tSome(Some(n)) -> 'ss ${n}'\n\
         \t\tSome(None) -> 'sn'\n\
         \t\tNone -> 'n'\n\
         \t}\n\
         \tprintln(r)\n\
         }\n",
        "ss 5\n",
    ),
}

ok_case! {
    nested_result_match_is_exhaustive: (
        "fn classify(x Result(Result(Int, String), String)) String {\n\
         \tmatch x {\n\
         \t\tOk(Ok(n)) -> 'ok ${n}'\n\
         \t\tOk(Err(e)) -> 'okerr ${e}'\n\
         \t\tErr(e) -> 'err ${e}'\n\
         \t}\n\
         }\n\
         pub fn main() {\n\
         \tprintln(classify(Ok(Ok(1))))\n\
         }\n",
    ),
}

reject_case! {
    /// The witness names the missing inner variant, not just `Some(_)`.
    nested_option_missing_inner_arm_reports_precise_witness: (
        "pub fn main() {\n\
         \tx = Some(Some(5))\n\
         \tr = match x {\n\
         \t\tSome(Some(n)) -> 'ss ${n}'\n\
         \t\tNone -> 'n'\n\
         \t}\n\
         \tprintln(r)\n\
         }\n",
        "Some(None)",
    ),
    /// The explicit inner arms already cover `Some(_)`, so the wildcard is
    /// dead code.
    nested_option_redundant_wildcard_is_rejected: (
        "pub fn main() {\n\
         \tx = Some(Some(5))\n\
         \tr = match x {\n\
         \t\tSome(Some(n)) -> 'ss ${n}'\n\
         \t\tSome(None) -> 'sn'\n\
         \t\tSome(_) -> 'other'\n\
         \t\tNone -> 'n'\n\
         \t}\n\
         \tprintln(r)\n\
         }\n",
        "unreachable",
    ),
}

#[test]
fn non_uniform_recursive_type_resolution_terminates() {
    // `Nest`'s argument grows at every level, so the instance key never
    // repeats and only the recurrence bound stops resolution looping. The
    // recursive position is cut off, so the wildcard arm is required.
    check_ok(
        "type Nest(t) {\n\tMore(inner Nest((t, t)))\n\tDone\n}\n\
         fn f(n Nest(Int)) Int {\n\
         \tmatch n {\n\
         \t\tMore(_) -> 1\n\
         \t\tDone -> 0\n\
         \t}\n\
         }\n\
         pub fn main() {\n\
         \tprintln('${f(Done)}')\n\
         }\n",
    );
}

run_case! {
    mutual_recursion_functions: (
        "fn is_even(n Int) Bool {\n\
         \tif n == 0 { True } else { is_odd(n - 1) }\n\
         }\n\
         fn is_odd(n Int) Bool {\n\
         \tif n == 0 { False } else { is_even(n - 1) }\n\
         }\n\
         pub fn main() {\n\
         \tprintln(is_even(10))\n\
         \tprintln(is_odd(7))\n\
         }\n",
        "True\nTrue\n",
    ),
}

reject_case! {
    unreachable_arm_is_error: (
        "fn f(b Bool) Int {\n\
         \tmatch b {\n\
         \t\tTrue -> 1\n\
         \t\tFalse -> 2\n\
         \t\tTrue -> 3\n\
         \t}\n\
         }\n\
         pub fn main() {\n\
         \tprintln(f(True))\n\
         }\n",
        "unreachable",
    ),
    ctor_pattern_missing_fields_without_spread_is_error: (
        "type User { User(name String, age Int, email String) }\n\
         fn f(u User) String {\n\
         \tmatch u {\n\
         \t\tUser(name: n) -> n\n\
         \t}\n\
         }\n\
         pub fn main() {\n\
         \tprintln(f(User(name: 'a', age: 1, email: 'e')))\n\
         }\n",
        "missing field(s): age, email",
    ),
}

run_case! {
    ctor_pattern_with_spread_is_ok: (
        "type User { User(name String, age Int, email String) }\n\
         fn f(u User) String {\n\
         \tmatch u {\n\
         \t\tUser(name: n, ..) -> n\n\
         \t}\n\
         }\n\
         pub fn main() {\n\
         \tprintln(f(User(name: 'alice', age: 30, email: 'a@b')))\n\
         }\n",
        "alice\n",
    ),
}

reject_case! {
    or_on_non_option_result_is_rejected: (
        "pub fn main() {\n\
         \tx = 5 or 0\n\
         \tprintln(x)\n\
         }\n",
        "'or' requires the left side to be Option(_) or Result(_, _)",
    ),
}

run_case! {
    or_on_result_unwraps_ok: (
        "fn f(b Bool) Result(Int, String) {\n\
         \tif b { Ok(42) } else { Err('nope') }\n\
         }\n\
         pub fn main() {\n\
         \tprintln(f(True) or -1)\n\
         \tprintln(f(False) or -1)\n\
         }\n",
        "42\n-1\n",
    ),
}

reject_case! {
    /// Body returns concrete `Int` where signature promised `a`.
    rigid_tyvar_body_mismatch_is_rejected: (
        "fn bad(x a) a { 1 }\n\
         pub fn main() {\n\
         \tprintln(bad('s'))\n\
         }\n",
        "Type mismatch: expected 'a', got 'Int'",
    ),
    /// `f` declares both params as `a`, so `f(1, 's')` must be rejected.
    rigid_tyvar_same_var_unifies_args: (
        "fn f(x a, y a) a { x }\n\
         pub fn main() {\n\
         \tprintln(f(1, 's'))\n\
         }\n",
        "Type mismatch: expected 'Int', got 'String'",
    ),
}

run_case! {
    rigid_tyvar_same_var_accepts_same_type: (
        "fn f(x a, _y a) a { x }\n\
         pub fn main() {\n\
         \tprintln(f(1, 2))\n\
         }\n",
        "1\n",
    ),
}

run_case! {
    positional_construction: (
        "type Pair { Pair(fst Int, snd Int) }\n\
         pub fn main() {\n\
         \tp = Pair(1, 2)\n\
         \tprintln(p.fst + p.snd)\n\
         }\n",
        "3\n",
    ),

    labeled_construction_reordered: (
        "type Pair { Pair(fst Int, snd Int) }\n\
         pub fn main() {\n\
         \tp = Pair(snd: 2, fst: 1)\n\
         \tprintln(p.fst)\n\
         \tprintln(p.snd)\n\
         }\n",
        "1\n2\n",
    ),
}

#[test]
#[ignore = "needs the VM"]
fn ctor_record_update_overrides_and_projects() {
    // Record-update builds a fresh value: `base` is left untouched.
    run_outputs(
        "type P { P(name String, age Int) }\n\
         pub fn main() {\n\
         \tbase = P(name: 'al', age: 18)\n\
         \tolder = P(..base, age: 19)\n\
         \tprintln(older.name)\n\
         \tprintln(older.age)\n\
         \tprintln(base.age)\n\
         }\n",
        "al\n19\n18\n",
    );
}

reject_case! {
    /// A record-update accepts only one `..base`.
    ctor_record_update_at_most_one_spread: (
        "type P { P(name String, age Int) }\n\
         pub fn main() {\n\
         \tbase = P(name: 'al', age: 18)\n\
         \tolder = P(..base, ..base)\n\
         \tprintln(older.age)\n\
         }\n",
        "Constructor call may have at most one spread",
    ),
    /// A spread in an ordinary call is a placement error.
    spread_arg_in_plain_call_rejected: (
        "fn f(a Int) Int { a }\n\
         pub fn main() {\n\
         \tprintln(f(..[1]))\n\
         }\n",
        "Spread arguments are only allowed in constructor record-update calls",
    ),
    /// A callee that is only a value has no parameter names to label, however
    /// it was bound: `g` is a local binding, so the declaration `f`'s names are
    /// not in reach at this call site. The refusal is the feature here, not a
    /// gap — see T-126.
    labelled_arg_on_value_callee_rejected: (
        "fn f(a Int) Int { a }\n\
         pub fn main() {\n\
         \tg = f\n\
         \tprintln(g(a: 1))\n\
         }\n",
        "Labelled arguments need a callee whose parameter names are known",
    ),
    /// A label naming no parameter lists the ones there are.
    unknown_label_on_fn_call_rejected: (
        "fn diff(a Int, b Int) Int { a - b }\n\
         pub fn main() {\n\
         \tprintln(diff(a: 1, c: 2))\n\
         }\n",
        "This function has no parameter 'c'. Available: a, b",
    ),
    /// Two arguments claiming one parameter is reported, not silently dropped.
    duplicate_label_on_fn_call_rejected: (
        "fn diff(a Int, b Int) Int { a - b }\n\
         pub fn main() {\n\
         \tprintln(diff(a: 1, a: 2))\n\
         }\n",
        "Parameter 'a' is specified more than once",
    ),
}

run_case! {
    /// Labels reversed against the declaration, on two parameters of the SAME
    /// type and returning an order-revealing result. Both orders type-check —
    /// which is exactly why the type checker cannot witness this — so `-1` vs
    /// `1` is the only thing that distinguishes arguments passed in declared
    /// order from arguments passed in source order.
    ///
    /// This does not witness the *checking* order, only the order the call is
    /// emitted in; `labelled_args_evaluate_in_source_order` covers the other
    /// half.
    labelled_args_reversed_on_same_typed_params: (
        "fn diff(a Int, b Int) Int { a - b }\n\
         pub fn main() {\n\
         \tprintln(diff(b: 2, a: 1))\n\
         }\n",
        "-1\n",
    ),

    /// A full permutation of three same-typed parameters: `123` holds only if
    /// each label reached its declared position.
    labelled_args_full_permutation: (
        "fn pick(a Int, b Int, c Int) Int { a * 100 + b * 10 + c }\n\
         pub fn main() {\n\
         \tprintln(pick(c: 3, a: 1, b: 2))\n\
         }\n",
        "123\n",
    ),

    /// Arguments are evaluated in SOURCE order and passed in DECLARED order:
    /// `2` prints before `1`, and the call still computes `1 - 2`.
    labelled_args_evaluate_in_source_order: (
        "fn note(n Int) Int {\n\
         \tprintln(n)\n\
         \tn\n\
         }\n\
         fn diff(a Int, b Int) Int { a - b }\n\
         pub fn main() {\n\
         \tprintln(diff(b: note(2), a: note(1)))\n\
         }\n",
        "2\n1\n-1\n",
    ),

    /// Leading positionals still fill left-to-right with a label after them.
    labelled_args_mixed_with_positional: (
        "fn diff(a Int, b Int) Int { a - b }\n\
         pub fn main() {\n\
         \tprintln(diff(10, b: 4))\n\
         }\n",
        "6\n",
    ),
}

run_case! {
    match_guard_basic: (
        "fn classify(n Int) String {\n\
         \tmatch n {\n\
         \t\tx if x < 0 -> 'neg'\n\
         \t\t0 -> 'zero'\n\
         \t\tx if x < 10 -> 'small'\n\
         \t\t_ -> 'big'\n\
         \t}\n\
         }\n\
         pub fn main() {\n\
         \tprintln(classify(-5))\n\
         \tprintln(classify(0))\n\
         \tprintln(classify(3))\n\
         \tprintln(classify(99))\n\
         }\n",
        "neg\nzero\nsmall\nbig\n",
    ),

    match_guard_with_constructor: (
        "fn pos(o Option(Int)) Int {\n\
         \tmatch o {\n\
         \t\tSome(n) if n > 0 -> n\n\
         \t\tSome(_) -> 0\n\
         \t\tNone -> 0\n\
         \t}\n\
         }\n\
         pub fn main() {\n\
         \tprintln(pos(Some(5)))\n\
         \tprintln(pos(Some(-3)))\n\
         \tprintln(pos(None))\n\
         }\n",
        "5\n0\n0\n",
    ),
}

reject_case! {
    match_guard_non_exhaustive_errors: (
        "fn f(n Int) String {\n\
         \tmatch n {\n\
         \t\tx if x < 2 -> 'a'\n\
         \t}\n\
         }\n",
        "exhaustive",
    ),
    match_guard_type_must_be_bool: (
        "fn f(n Int) String {\n\
         \tmatch n {\n\
         \t\tx if x -> 'a'\n\
         \t\t_ -> 'b'\n\
         \t}\n\
         }\n",
        "Bool",
    ),
}

#[test]
#[ignore = "needs the VM"]
fn or_pattern_binding_after_or_in_tuple() {
    // A binding after an or-pattern, as in `(0 | 1, y)`, is in scope.
    run_outputs(
        "fn f(t (Int, Int)) Int {\n\
         \tmatch t {\n\
         \t\t(0 | 1, y) -> y\n\
         \t\t(x, y) -> x + y\n\
         \t}\n\
         }\n\
         pub fn main() {\n\
         \tprintln(f((1, 5)))\n\
         \tprintln(f((9, 5)))\n\
         }\n",
        "5\n14\n",
    );
}

#[test]
#[ignore = "needs the VM"]
fn or_pattern_binding_before_or_in_tuple() {
    // A binding before an or-pattern, as in `(y, 0 | 1)`, is in scope.
    run_outputs(
        "fn g(t (Int, Int)) Int {\n\
         \tmatch t {\n\
         \t\t(y, 0 | 1) -> y\n\
         \t\t(x, y) -> x + y\n\
         \t}\n\
         }\n\
         pub fn main() {\n\
         \tprintln(g((5, 1)))\n\
         \tprintln(g((5, 9)))\n\
         }\n",
        "5\n14\n",
    );
}

reject_case! {
    /// Every alternative of an or-pattern must bind exactly the same names.
    or_pattern_unequal_bindings_still_rejected: (
        "type R {\n\tGood(v Int)\n\tBad(v Int)\n}\n\
         fn h(r R) Int {\n\
         \tmatch r {\n\
         \t\tGood(x) | Bad(z) -> 0\n\
         \t}\n\
         }\n\
         pub fn main() {\n\
         \tprintln(h(Good(1)))\n\
         }\n",
        "every alternative",
    ),
}

#[test]
#[ignore = "needs the VM"]
fn or_pattern_nested_in_non_first_alternative() {
    // A nested or-pattern is checked against the outer or's canonical set,
    // not treated as a fresh scope.
    run_outputs(
        "fn f(t (Int, (Int, Int))) Int {\n\
         \tmatch t {\n\
         \t\t(0, (x, 0)) | (1, (x, 1) | (x, 2)) -> x\n\
         \t\t_ -> 99\n\
         \t}\n\
         }\n\
         pub fn main() {\n\
         \tprintln(f((0, (10, 0))))\n\
         \tprintln(f((1, (20, 1))))\n\
         \tprintln(f((1, (30, 2))))\n\
         \tprintln(f((1, (40, 5))))\n\
         }\n",
        "10\n20\n30\n99\n",
    );
}

run_case! {
    array_spread_literal: (
        "pub fn main() {\n\
         \txs = [1, 2]\n\
         \tys = [4, 5]\n\
         \tzs = [..xs, 3, ..ys, 6]\n\
         \tprintln(zs)\n\
         }\n",
        "[1, 2, 3, 4, 5, 6]\n",
    ),
}

reject_case! {
    array_concat_operator_removed: (
        "pub fn main() {\n\
         \txs = [1] ++ [2]\n\
         \tprintln(xs)\n\
         }\n",
        "Unexpected '++'",
    ),
}

#[test]
fn nested_ctor_pattern_exhaustive() {
    // `resolve_icon`'s cycle guard must not leak across sibling type-args.
    check_ok(
        "pub fn main() {\n\
         \tmatch Ok(Nil) { Ok(Nil) -> println('y') Err(e) -> println(e) }\n\
         }\n",
    );
    check_ok(
        "type T {\n\tA\n\tB\n}\n\
         pub fn main() {\n\
         \tmatch Ok(A) { Ok(A) -> println('a') Ok(B) -> println('b') Err(e) -> println(e) }\n\
         }\n",
    );
}

#[test]
fn module_builtins_qualified_and_destructured() {
    check_ok(
        "import scarlet/net\n\
         pub fn main() {\n\
         \tmatch net.listen('0.0.0.0', 8080) { Ok(s) -> println(s) Err(e) -> println(e) }\n\
         }\n",
    );
    check_ok(
        "import scarlet/net.{listen, Server}\n\
         fn go(s Server) Nil { println(s) }\n\
         pub fn main() {\n\
         \tmatch listen('0.0.0.0', 8080) { Ok(s) -> go(s) Err(e) -> println(e) }\n\
         }\n",
    );
    check_ok(
        "import scarlet/io\n\
         pub fn main() {\n\
         \tx = io.read_text('a') or ''\n\
         \tprintln(x)\n\
         }\n",
    );
}

#[test]
fn vm_attribute_stdlib_only() {
    check_rejects(
        "@vm(tcp_listen)\nfn listen(p Int) Int\n",
        "'@vm' is only allowed in the standard library",
    );
    check_rejects("@nope\nfn f() Nil { Nil }\n", "Unknown attribute '@nope'");
}

/// Rejected deliberately, not just incidentally as one more unknown name:
/// Scarlet has no package boundary to give `@internal` a meaning and no docs
/// generator to withhold a name from, so accepting it would leave the function
/// exactly as `pub` as it was (`docs/visibility.md`). Witnesses the rejection
/// only; it says nothing about `pub`, which this decision leaves unchanged.
#[test]
fn internal_attribute_is_not_a_scarlet_attribute() {
    check_rejects(
        "@internal\npub fn reveal() Nil { Nil }\n",
        "Unknown attribute '@internal'",
    );
}

/// The `AttrTarget::Type` arm, which is named rather than negated so a new
/// attribute target has to decide whether `@vm` is legal on it.
///
/// A test source is never in the stdlib, so `'@vm' is only allowed in the
/// standard library` fires on this same attribute — the two checks are
/// sequential, not exclusive, and `al check` prints both. That is why the
/// expected diagnostic is spelled out in full and passed alone: `check_rejects`
/// matches one of the strings it is given, so naming both would let the
/// stdlib-only error satisfy it. The bare rejection is likewise not the
/// witness, since the stdlib-only error already produces one on its own.
#[test]
fn vm_attribute_may_not_be_used_on_a_type() {
    check_rejects(
        "@vm(tcp_listen)\ntype Handle { Handle(fd Int) }\n",
        "'@vm' may only be used on functions",
    );
}

#[test]
fn bool_is_a_normal_two_ctor_type() {
    check_rejects(
        "fn f(b Bool) Int { match b { True -> 1 } }\n\
         pub fn main() {\n\
         \tprintln(f(True))\n\
         }\n",
        "not exhaustive",
    );
    check_rejects(
        "type My { True }\n",
        "is defined in the prelude and cannot be redefined",
    );
}

#[test]
#[ignore = "needs the VM"]
fn bool_is_a_normal_two_ctor_type_runs() {
    run_outputs(
        "pub fn main() {\n\
         \tprintln(True)\n\
         \tprintln(False)\n\
         \tprintln(!True)\n\
         }\n",
        "True\nFalse\nFalse\n",
    );
    run_outputs(
        "fn show(b Bool) String { match b { True -> 'yes'\nFalse -> 'no' } }\n\
         pub fn main() {\n\
         \tprintln(show(True))\n\
         \tprintln(show(1 == 2))\n\
         }\n",
        "yes\nno\n",
    );
}

reject_case! {
    lowercase_true_is_just_an_identifier: (
        "pub fn main() {\n\
         \tx = true\n\
         \tprintln(x)\n\
         }\n",
        "Unknown identifier",
    ),
}

#[test]
fn reserved_set_derived_from_prelude_iface() {
    // Prelude types/ctors are reserved...
    check_rejects(
        "type Option(a) {\n\tJust(value a)\n\tNothing\n}\n",
        "is defined in the prelude and cannot be redefined",
    );
}

#[test]
#[ignore = "needs the VM"]
fn reserved_set_derived_from_prelude_iface_runs() {
    // ...but `@vm` functions are not.
    run_outputs(
        "fn println(x Int) Int { x + 1 }\n\
         pub fn main() {\n\
         \t_ = println(41)\n\
         }\n",
        "",
    );
}

#[test]
fn binary_string_literal_patterns() {
    // The Utf8 default applies only to bare string segments.
    check_rejects(
        "import scarlet/binary\n\
         pub fn main() {\n\
         \tr = match binary.from_string('AB') {\n\
         \t\t<<'AB':16>> -> 1\n\
         \t\t_ -> 0\n\
         \t}\n\
         \tprintln(r)\n\
         }\n",
        "Type mismatch",
    );
}

#[test]
#[ignore = "needs the VM"]
fn binary_string_literal_patterns_runs() {
    // A bare string-literal segment matches its UTF-8 bytes as a prefix
    // (Op::BinMatchPrefix); the rest binding is a zero-copy view.
    run_outputs(
        "import scarlet/binary\n\
         pub fn main() {\n\
         \tr = match binary.from_string('GET /index.html') {\n\
         \t\t<<'GET ', ..rest>> -> binary.to_string(rest)\n\
         \t\t_ -> Err(Nil)\n\
         \t}\n\
         \tprintln(r)\n\
         }\n",
        "Ok(/index.html)\n",
    );
    // Explicit `:utf8` on a string literal is the same pattern.
    run_outputs(
        "import scarlet/binary\n\
         pub fn main() {\n\
         \tr = match binary.from_string('POST /x') {\n\
         \t\t<<'POST ':utf8, ..>> -> 1\n\
         \t\t_ -> 0\n\
         \t}\n\
         \tprintln(r)\n\
         }\n",
        "1\n",
    );
    // Without `..rest` the whole binary must be consumed.
    run_outputs(
        "import scarlet/binary\n\
         fn is_get(b Binary) Bool {\n\
         \tmatch b {\n\
         \t\t<<'GET'>> -> True\n\
         \t\t_ -> False\n\
         \t}\n\
         }\n\
         pub fn main() {\n\
         \tprintln(is_get(binary.from_string('GET')))\n\
         \tprintln(is_get(binary.from_string('GETX')))\n\
         \tprintln(is_get(binary.from_string('GE')))\n\
         }\n",
        "True\nFalse\nFalse\n",
    );
    // A prefix longer than the scrutinee fails without reading past the end.
    run_outputs(
        "import scarlet/binary\n\
         pub fn main() {\n\
         \tr = match binary.from_string('DELETE /v') {\n\
         \t\t<<'GET ', ..>> -> 'get'\n\
         \t\t<<'DELETE /very/long/path/that/overruns', ..>> -> 'overrun'\n\
         \t\t<<'DELETE ', ..>> -> 'delete'\n\
         \t\t_ -> 'other'\n\
         \t}\n\
         \tprintln(r)\n\
         }\n",
        "delete\n",
    );
    // A literal prefix can be followed by destructuring segments.
    run_outputs(
        "import scarlet/binary\n\
         pub fn main() {\n\
         \tr = match binary.from_string('HTTP/1.1') {\n\
         \t\t<<'HTTP/1.', minor>> -> minor - 48\n\
         \t\t_ -> 0 - 1\n\
         \t}\n\
         \tprintln(r)\n\
         }\n",
        "1\n",
    );
    // Multi-byte UTF-8 literals keep byte-accurate offsets for the rest.
    run_outputs(
        "import scarlet/binary\n\
         pub fn main() {\n\
         \tr = match binary.from_string('héllo world') {\n\
         \t\t<<'héllo ', ..rest>> -> binary.to_string(rest)\n\
         \t\t_ -> Err(Nil)\n\
         \t}\n\
         \tprintln(r)\n\
         }\n",
        "Ok(world)\n",
    );
    // Consecutive integer literals coalesce into one prefix compare.
    run_outputs(
        "import scarlet/binary\n\
         pub fn main() {\n\
         \tr = match binary.from_string('\\r\\nrest') {\n\
         \t\t<<13, 10, ..rest>> -> binary.byte_size(rest)\n\
         \t\t_ -> 0 - 1\n\
         \t}\n\
         \tprintln(r)\n\
         }\n",
        "4\n",
    );
    // Mixed and sub-byte literal runs coalesce too; the compile-time
    // encoding must match Op::BinFromInt's MSB-first layout.
    run_outputs(
        "import scarlet/binary\n\
         pub fn main() {\n\
         \tpacket = <<1:4, 2:4, 'AB', 7>>\n\
         \tr = match packet {\n\
         \t\t<<1:4, 2:4, 'AB', n>> -> n\n\
         \t\t_ -> 0 - 1\n\
         \t}\n\
         \tprintln(r)\n\
         }\n",
        "7\n",
    );
    // String-literal segments in expressions fold to the same UTF-8 bytes.
    run_outputs(
        "import scarlet/binary\n\
         pub fn main() {\n\
         \tprintln(<<'hi there'>> == binary.from_string('hi there'))\n\
         \tprintln(<<'AB', 67, 'D'>> == binary.from_string('ABCD'))\n\
         \tprintln(binary.bit_size(<<''>>))\n\
         }\n",
        "True\nTrue\n0\n",
    );
}

#[test]
#[ignore = "needs the VM"]
fn binary_literal_and_pattern_e2e() {
    // <<a, b>> pattern: scan→parse→compile→VM. 'A'=65, 'B'=66, sum=131.
    run_outputs(
        "import scarlet/binary\n\
         pub fn main() {\n\
         \tr = match binary.from_string('AB') {\n\
         \t\t<<a, b>> -> a + b\n\
         \t\t_ -> 0\n\
         \t}\n\
         \tprintln(r)\n\
         }\n",
        "131\n",
    );
    // <<1:4, 2:4>> literal: 0001_0010 = 18, inspect emits whole-byte form.
    run_outputs(
        "import scarlet/string\n\
         pub fn main() {\n\
         \tprintln(string.inspect(<<1:4, 2:4>>))\n\
         }\n",
        "<<18>>\n",
    );
}

run_case! {
    ctor_destructure_single_variant_ok: (
        "type Box { Box(value Int) }\n\
         pub fn main() {\n\
         \tBox(n) = Box(42)\n\
         \tprintln(n)\n\
         }\n",
        "42\n",
    ),

    ctor_destructure_multi_field_ok: (
        "type Pair { Pair(a Int, b String) }\n\
         pub fn main() {\n\
         \tPair(x, y) = Pair(7, 'hi')\n\
         \tprintln(x)\n\
         \tprintln(y)\n\
         }\n",
        "7\nhi\n",
    ),

    // Labels bind by declared field order, not by argument position.
    ctor_destructure_labeled_out_of_order: (
        "type Point { Point(x Int, y Int) }\n\
         pub fn main() {\n\
         \tPoint(y: b, x: a) = Point(1, 2)\n\
         \tprintln(a)\n\
         \tprintln(b)\n\
         }\n",
        "1\n2\n",
    ),

    ctor_destructure_labeled_with_rest: (
        "type T { T(a Int, b Int, c Int) }\n\
         pub fn main() {\n\
         \tT(c: z, ..) = T(10, 20, 30)\n\
         \tprintln(z)\n\
         }\n",
        "30\n",
    ),
}

reject_case! {
    ctor_destructure_refutable_rejected: (
        "pub fn main() {\n\
         \tSome(x) = Some(1)\n\
         \tprintln(x)\n\
         }\n",
        "refutable",
    ),
    ctor_destructure_nested_refutable_rejected: (
        "type Box { Box(value Option(Int)) }\n\
         pub fn main() {\n\
         \tBox(Some(n)) = Box(Some(1))\n\
         \tprintln(n)\n\
         }\n",
        "refutable",
    ),
}

run_case! {
    typed_discard_nil_println_ok: (
        "pub fn main() {\n\
         \tNil = println('x')\n\
         }\n",
        "x\n",
    ),
}

reject_case! {
    typed_discard_string_int_mismatch: (
        "pub fn main() {\n\
         \tString = 5\n\
         }\n",
        "Type mismatch: expected 'String', got 'Int'",
    ),
    typed_discard_int_string_mismatch: (
        "pub fn main() {\n\
         \tInt = 'a'\n\
         }\n",
        "Type mismatch: expected 'Int', got 'String'",
    ),
    typed_discard_constructor_is_not_a_type: (
        "pub fn main() {\n\
         \tSome = 1\n\
         }\n",
        "'Some' is not a type",
    ),
}

// A constructor used as a value makes `lower` synthesise an eta-wrapper into
// `program.code` ahead of the body, so `emit` must bake absolute jump targets
// against the post-lowering address. Only the `else` arm jumps.
#[test]
#[ignore = "needs the VM"]
fn a_branch_after_an_eta_wrapper_jumps_to_the_right_place() {
    let src = "import scarlet/array\n\
               type W { W(v Int) }\n\
               fn pick(xs Array(Int)) Int {\n\
               \tws = array.map(xs, W)\n\
               \tif array.length(ws) > 2 { 111 } else { 222 }\n\
               }\n\
               pub fn main() {\n\
               \tprintln(pick([1]))\n\
               \tprintln(pick([1, 2, 3]))\n\
               }\n";
    run_outputs(src, "222\n111\n");
}

const FIELD_ACCESS_THROUGH_A_CONSTRUCTOR_INFERRED_SCRUTINEE_SRC: &str = "type User { User(id Int, name String) }\n\
               fn f() Int {\n\
               \tmatch Some(User(7, 'al')) {\n\
               \t\tNone -> 0\n\
               \t\tSome(u) -> u.id\n\
               \t}\n\
               }\n\
               pub fn main() {\n\
               \tprintln(f())\n\
               }\n";

#[test]
fn field_access_through_a_constructor_inferred_scrutinee() {
    check_ok(FIELD_ACCESS_THROUGH_A_CONSTRUCTOR_INFERRED_SCRUTINEE_SRC);
}

#[test]
#[ignore = "needs the VM"]
fn field_access_through_a_constructor_inferred_scrutinee_runs() {
    run_outputs(
        FIELD_ACCESS_THROUGH_A_CONSTRUCTOR_INFERRED_SCRUTINEE_SRC,
        "7\n",
    );
}

const FIELD_ACCESS_THROUGH_A_MODULE_FN_INFERRED_SCRUTINEE_SRC: &str = "import scarlet/map\n\
               type User { User(id Int, name String) }\n\
               fn f(m map.Map(Binary, User)) Int {\n\
               \tmatch map.get(m, <<'a'>>) {\n\
               \t\tNone -> 0\n\
               \t\tSome(u) -> u.id\n\
               \t}\n\
               }\n\
               pub fn main() {\n\
               \tprintln(f(map.set(map.new(), <<'a'>>, User(7, 'al'))))\n\
               }\n";

#[test]
fn field_access_through_a_module_fn_inferred_scrutinee() {
    check_ok(FIELD_ACCESS_THROUGH_A_MODULE_FN_INFERRED_SCRUTINEE_SRC);
}

#[test]
#[ignore = "needs the VM"]
fn field_access_through_a_module_fn_inferred_scrutinee_runs() {
    run_outputs(
        FIELD_ACCESS_THROUGH_A_MODULE_FN_INFERRED_SCRUTINEE_SRC,
        "7\n",
    );
}

/// Binding an inferred scrutinee's heap payload makes the arm responsible for
/// releasing it, so Perceus needs the payload's real type to emit the `Drop`.
/// The `drop` itself is pinned by the `inferred_scrutinee_drops_heap_payload`
/// Core IR golden; this pins the answer.
#[test]
#[ignore = "needs the VM"]
fn inferred_scrutinee_with_a_heap_payload_runs() {
    let src = "type Boxed { Boxed(n Int) }\n\
               fn f() Int {\n\
               \tmatch Some(Boxed(3)) {\n\
               \t\tNone -> 0\n\
               \t\tSome(b) -> {\n\
               \t\t\tx = b.n\n\
               \t\t\tx + 1\n\
               \t\t}\n\
               \t}\n\
               }\n\
               pub fn main() {\n\
               \tprintln(f())\n\
               }\n";
    run_outputs(src, "4\n");
}

const OR_RECEIVER_BINDS_A_HEAP_ERROR_PAYLOAD_SRC: &str = "type Boxed { Boxed(n Int) }\n\
               fn bad() Result(Int, Boxed) { Err(Boxed(9)) }\n\
               fn f() Int { bad() or e -> e.n }\n\
               pub fn main() {\n\
               \tprintln(f())\n\
               }\n";

/// The `Err` payload bound by `expr or e -> body` is the LHS type's second
/// argument, not a fresh variable, so a heap error stays droppable.
#[test]
fn or_receiver_binds_a_heap_error_payload() {
    check_ok(OR_RECEIVER_BINDS_A_HEAP_ERROR_PAYLOAD_SRC);
}

#[test]
#[ignore = "needs the VM"]
fn or_receiver_binds_a_heap_error_payload_runs() {
    run_outputs(OR_RECEIVER_BINDS_A_HEAP_ERROR_PAYLOAD_SRC, "9\n");
}

// T-148: `@exhaustive` is opt-in and only forbids a wildcard/bare-binder arm
// from covering variants it does not name — everything else about the type
// works exactly as it does without the attribute.
run_case! {
    /// Every variant named explicitly: `@exhaustive` has nothing to refuse.
    exhaustive_type_with_every_variant_named_runs: (
        "@exhaustive\ntype Color {\n\tRed\n\tGreen\n\tBlue\n}\n\
         pub fn main() {\n\
         \tprintln(match Green {\n\
         \t\tRed -> 0\n\
         \t\tGreen -> 1\n\
         \t\tBlue -> 2\n\
         \t})\n\
         }\n",
        "1\n",
    ),
}
