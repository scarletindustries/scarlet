//! Type-system and language-semantics coverage: `type` declarations,
//! constructors as values, exhaustiveness, refutability, `or`/field/`if`
//! typing, rigid type variables, record update, match guards, or-patterns,
//! binary patterns, typed discard. Stdlib runtime goldens live in
//! `tests/stdlib.rs`; opcode-level VM execution lives in `tests/vm_exec.rs`.

mod common;
use common::{check_ok, check_rejects, run_outputs};

// ===========================================================================
// `type` keyword — definitions
// ===========================================================================

run_case! {
    type_keyword_single_variant: (
        "type User { User(name String, age Int) }\n\
         u = User(name: 'al', age: 18)\n\
         println(u.name)\n\
         println(u.age)\n",
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
         println(area(Circle(10)))\n\
         println(area(Rect(4, 5)))\n",
        "300\n20\n",
    ),

    type_alias_is_transparent: (
        "type Id = Int\n\
         fn next(i Id) Id { i + 1 }\n\
         println(next(41))\n",
        "42\n",
    ),
}

reject_case! {
    /// Every field in a type definition must carry a label.
    unlabeled_field_in_def_is_rejected:
        ("type Wrap { Wrap(Int) }\nx = Wrap(1)\n", "constructor fields must be labeled"),
}

// ===========================================================================
// Constructors are functions
// ===========================================================================

run_case! {
    some_call_is_ordinary_call: (
        "x = Some(5)\n\
         println(x or 0)\n",
        "5\n",
    ),

    constructor_is_first_class: (
        "fn map(f fn(a) b, xs Array(a)) Array(b) {\n\
         \tmatch xs {\n\
         \t\t[] -> []\n\
         \t\t[h, ..t] -> [f(h), ..map(f, t)]\n\
         \t}\n\
         }\n\
         ys = map(Some, [1, 2, 3])\n\
         println(ys[0] or None)\n\
         println(ys[2] or None)\n",
        "Some(1)\nSome(3)\n",
    ),

    nullary_constructor_is_value: (
        "x = None\n\
         println(x or 7)\n",
        "7\n",
    ),
}

reject_case! {
    // `if` requires `else`
    if_without_else_is_error: ("x = if True { 1 }\nprintln(x)\n", "else"),
    // Parentheses are tuples-only
    empty_parens_is_parse_error: ("x = ()\nprintln(x)\n", "tuples need 2+ elements"),
    single_parens_is_parse_error: ("x = (5)\nprintln(x)\n", "single-element parens not allowed"),
}

run_case! {
    block_is_grouping: ("println({1 + 2} * 3)\n", "9\n"),
}

// ===========================================================================
// Array index returns Option
// ===========================================================================

run_case! {
    index_returns_option: (
        "xs = [10, 20, 30]\n\
         println(xs[0] or -1)\n\
         println(xs[9] or -1)\n",
        "10\n-1\n",
    ),
}

reject_case! {
    /// `xs[0] + 1` should be rejected because `xs[0]` is `Option(Int)`, not `Int`.
    index_without_unwrap_is_option_typed:
        ("xs = [10, 20, 30]\ny = xs[0] + 1\nprintln(y)\n", "got 'Option(Int)'"),
}

#[test]
fn index_negative_returns_none() {
    // A negative index hits the `idx >= 0` guard in `Op::Index`, a path
    // distinct from the positive out-of-bounds case
    // (`xs[9]`): instead of `arr.get` returning `None`, the guard itself
    // rejects the access. Both `-1` and a large negative magnitude yield the
    // `None` Option, while a valid in-bounds index still boxes `Some`.
    run_outputs(
        "xs = [10, 20, 30]\n\
         println(xs[-1])\n\
         println(xs[-100] or -1)\n\
         println(xs[0])\n",
        "None\n-1\nSome(10)\n",
    );
}

// ===========================================================================
// Array slicing `xs[start..end]` (Op::ArraySlice)
// ===========================================================================

#[test]
fn slice_in_bounds_returns_subarray() {
    // `xs[start..end]` is the half-open sub-array, itself an `Array(Int)` (not
    // an `Option`). Indexing back into the slice confirms its exact contents
    // and length: `[20, 30, 40]` has elements at 0..3, so index 3 is `None`.
    run_outputs(
        "xs = [10, 20, 30, 40, 50]\n\
         s = xs[1..4]\n\
         println(s)\n\
         println(s[0] or -1)\n\
         println(s[2] or -1)\n\
         println(s[3] or -1)\n",
        "[20, 30, 40]\n20\n40\n-1\n",
    );
}

// ===========================================================================
// Ranges are first-class Array(Int) values
// ===========================================================================

#[test]
fn range_as_value_materializes() {
    // A bare `start..end` is a first-class `Array(Int)`; printing it
    // materializes the half-open sequence. `3..3` is empty (start == end) and
    // `5..2` is empty too — a reversed range saturates to length 0, never a
    // negative length or a crash.
    run_outputs(
        "println(0..5)\n\
         println(3..3)\n\
         println(5..2)\n",
        "[0, 1, 2, 3, 4]\n[]\n[]\n",
    );
}

// ===========================================================================
// `.field` totality
// ===========================================================================

run_case! {
    field_access_total_across_variants: (
        "type Named {\n\tPerson(name String, age Int)\n\tOrg(name String, size Int)\n}\n\
         fn name_of(n Named) String { n.name }\n\
         println(name_of(Person(name: 'al', age: 18)))\n\
         println(name_of(Org(name: 'anthropic', size: 1000)))\n",
        "al\nanthropic\n",
    ),
}

// ===========================================================================
// Recursive and mutually-recursive types/functions
// ===========================================================================

ok_case! {
    recursive_type_compiles: (
        "type Tree(a) {\n\tLeaf\n\tNode(l Tree(a), v a, r Tree(a))\n}\n\
         t Tree(Int) = Node(l: Leaf, v: 1, r: Leaf)\n\
         t\n",
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
         t = Node(l: Node(l: Leaf, v: 1, r: Leaf), v: 2, r: Leaf)\n\
         println(size(t))\n",
        "2\n",
    ),
}

// ===========================================================================
// Exhaustiveness of self-nested generic types — Option(Option(_)), Result(...)
//
// The exhaustiveness checker resolves a matched type into its variant set
// before lowering it. A recursion guard keyed only on the nominal type id
// mistook the *inner* `Option` of `Option(Option(_))` for a recursive
// occurrence of the outer one, left it variant-less, and so demanded a
// wildcard arm for an already-exhaustive match. The guard is now keyed on the
// resolved instance, so a finite re-nesting expands while genuine recursion is
// still cut off.
// ===========================================================================

ok_case! {
    nested_option_match_is_exhaustive: (
        "x = Some(Some(5))\n\
         r = match x {\n\
         \tSome(Some(n)) -> 'ss ${n}'\n\
         \tSome(None) -> 'sn'\n\
         \tNone -> 'n'\n\
         }\n\
         println(r)\n",
    ),
}

run_case! {
    nested_option_match_runs: (
        "x = Some(Some(5))\n\
         r = match x {\n\
         \tSome(Some(n)) -> 'ss ${n}'\n\
         \tSome(None) -> 'sn'\n\
         \tNone -> 'n'\n\
         }\n\
         println(r)\n",
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
         println(classify(Ok(Ok(1))))\n",
    ),
}

reject_case! {
    /// Dropping `Some(None)` is genuinely non-exhaustive, and now that the inner
    /// `Option`'s variants are known the witness names it exactly instead of
    /// collapsing to `Some(_)`.
    nested_option_missing_inner_arm_reports_precise_witness: (
        "x = Some(Some(5))\n\
         r = match x {\n\
         \tSome(Some(n)) -> 'ss ${n}'\n\
         \tNone -> 'n'\n\
         }\n\
         println(r)\n",
        "Some(None)",
    ),
    /// Before the fix a `Some(_)` arm was *required* to silence a false
    /// non-exhaustiveness error; now the explicit inner arms already cover
    /// `Some(_)`, so that wildcard is correctly reported as dead code.
    nested_option_redundant_wildcard_is_rejected: (
        "x = Some(Some(5))\n\
         r = match x {\n\
         \tSome(Some(n)) -> 'ss ${n}'\n\
         \tSome(None) -> 'sn'\n\
         \tSome(_) -> 'other'\n\
         \tNone -> 'n'\n\
         }\n\
         println(r)\n",
        "unreachable",
    ),
}

#[test]
fn non_uniform_recursive_type_resolution_terminates() {
    // `Nest`'s argument grows at every level, so the instance key never repeats
    // and the recurrence bound is what stops resolution from looping forever.
    // A match on it still type-checks: the recursive position is cut off to an
    // infinite-constructor type, so the wildcard arm is required and accepted.
    check_ok(
        "type Nest(t) {\n\tMore(inner Nest((t, t)))\n\tDone\n}\n\
         fn f(n Nest(Int)) Int {\n\
         \tmatch n {\n\
         \t\tMore(_) -> 1\n\
         \t\tDone -> 0\n\
         \t}\n\
         }\n\
         println('${f(Done)}')\n",
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
         println(is_even(10))\n\
         println(is_odd(7))\n",
        "True\nTrue\n",
    ),
}

// ===========================================================================
// Exhaustiveness — variants and fields
// ===========================================================================

reject_case! {
    unreachable_arm_is_error: (
        "fn f(b Bool) Int {\n\
         \tmatch b {\n\
         \t\tTrue -> 1\n\
         \t\tFalse -> 2\n\
         \t\tTrue -> 3\n\
         \t}\n\
         }\n\
         f(True)\n",
        "unreachable",
    ),
    ctor_pattern_missing_fields_without_spread_is_error: (
        "type User { User(name String, age Int, email String) }\n\
         fn f(u User) String {\n\
         \tmatch u {\n\
         \t\tUser(name: n) -> n\n\
         \t}\n\
         }\n\
         f(User(name: 'a', age: 1, email: 'e'))\n",
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
         println(f(User(name: 'alice', age: 30, email: 'a@b')))\n",
        "alice\n",
    ),
}

// ===========================================================================
// `or` typing
// ===========================================================================

reject_case! {
    or_on_non_option_result_is_rejected:
        ("x = 5 or 0\nprintln(x)\n", "'or' requires the left side to be Option(_) or Result(_, _)"),
}

run_case! {
    or_on_result_unwraps_ok: (
        "fn f(b Bool) Result(Int, String) {\n\
         \tif b { Ok(42) } else { Err('nope') }\n\
         }\n\
         println(f(True) or -1)\n\
         println(f(False) or -1)\n",
        "42\n-1\n",
    ),
}

// ===========================================================================
// Rigid type variables
// ===========================================================================

reject_case! {
    /// Body returns concrete `Int` where signature promised `a`.
    rigid_tyvar_body_mismatch_is_rejected:
        ("fn bad(x a) a { 1 }\nprintln(bad('s'))\n", "Type mismatch: expected 'a', got 'Int'"),
    /// `f` declares both params as `a`, so `f(1, 's')` must be rejected.
    rigid_tyvar_same_var_unifies_args:
        ("fn f(x a, y a) a { x }\nprintln(f(1, 's'))\n", "Type mismatch: expected 'Int', got 'String'"),
}

run_case! {
    rigid_tyvar_same_var_accepts_same_type: (
        "fn f(x a, _y a) a { x }\n\
         println(f(1, 2))\n",
        "1\n",
    ),
}

// ===========================================================================
// Positional vs labeled construction
// ===========================================================================

run_case! {
    positional_construction: (
        "type Pair { Pair(fst Int, snd Int) }\n\
         p = Pair(1, 2)\n\
         println(p.fst + p.snd)\n",
        "3\n",
    ),

    labeled_construction_reordered: (
        "type Pair { Pair(fst Int, snd Int) }\n\
         p = Pair(snd: 2, fst: 1)\n\
         println(p.fst)\n\
         println(p.snd)\n",
        "1\n2\n",
    ),
}

// ===========================================================================
// Constructor record-update: `Ctor(..base, field: newval)`
// ===========================================================================

#[test]
fn ctor_record_update_overrides_and_projects() {
    // `..base` projects the unmentioned fields out of `base`, the explicit
    // label overrides its own field, and `base` itself is left untouched.
    // The three prints pin all three behaviors at once: `name` is projected
    // (al), `age` is overridden (19), and the original `base.age` is unchanged
    // (18) — proving record-update builds a fresh value rather than mutating.
    run_outputs(
        "type P { P(name String, age Int) }\n\
         base = P(name: 'al', age: 18)\n\
         older = P(..base, age: 19)\n\
         println(older.name)\n\
         println(older.age)\n\
         println(base.age)\n",
        "al\n19\n18\n",
    );
}

reject_case! {
    /// A constructor record-update accepts a single `..base`; a second spread is
    /// rejected.
    ctor_record_update_at_most_one_spread: (
        "type P { P(name String, age Int) }\n\
         base = P(name: 'al', age: 18)\n\
         older = P(..base, ..base)\n\
         println(older.age)\n",
        "Constructor call may have at most one spread",
    ),
    /// Spread arguments only make sense in constructor record-update calls; in an
    /// ordinary function call they are a placement error.
    spread_arg_in_plain_call_rejected: (
        "fn f(a Int) Int { a }\nprintln(f(..[1]))\n",
        "Spread arguments are only allowed in constructor record-update calls",
    ),
    /// Labelled arguments are a constructor-only affordance; passing one to an
    /// ordinary function is a placement error.
    labelled_arg_in_plain_call_rejected: (
        "fn f(a Int) Int { a }\nprintln(f(a: 1))\n",
        "Labelled arguments are only allowed in constructor calls",
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
         println(classify(-5))\n\
         println(classify(0))\n\
         println(classify(3))\n\
         println(classify(99))\n",
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
         println(pos(Some(5)))\n\
         println(pos(Some(-3)))\n\
         println(pos(None))\n",
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
fn or_pattern_binding_after_or_in_tuple() {
    // Regression: typing an or-pattern left `PatternBindings` stuck in
    // `Alternative` mode, so a sibling binding *after* it (`y`) was rejected as
    // "not bound in the first alternative" even though `(0 | 1, y)` is valid.
    run_outputs(
        "fn f(t (Int, Int)) Int {\n\
         \tmatch t {\n\
         \t\t(0 | 1, y) -> y\n\
         \t\t(x, y) -> x + y\n\
         \t}\n\
         }\n\
         println(f((1, 5)))\n\
         println(f((9, 5)))\n",
        "5\n14\n",
    );
}

#[test]
fn or_pattern_binding_before_or_in_tuple() {
    // Regression: `finish_alternative` compared `seen` against the *global*
    // initial-binding count, so a binding *before* an or-pattern (`y`) was
    // wrongly reported as "must be bound in every alternative" for `(y, 0 | 1)`.
    run_outputs(
        "fn g(t (Int, Int)) Int {\n\
         \tmatch t {\n\
         \t\t(y, 0 | 1) -> y\n\
         \t\t(x, y) -> x + y\n\
         \t}\n\
         }\n\
         println(g((5, 1)))\n\
         println(g((5, 9)))\n",
        "5\n14\n",
    );
}

reject_case! {
    /// The scoping fix must not relax the core invariant: every alternative of
    /// an or-pattern must bind exactly the same names.
    or_pattern_unequal_bindings_still_rejected: (
        "type R {\n\tGood(v Int)\n\tBad(v Int)\n}\n\
         fn h(r R) Int {\n\
         \tmatch r {\n\
         \t\tGood(x) | Bad(z) -> 0\n\
         \t}\n\
         }\n\
         println(h(Good(1)))\n",
        "every alternative",
    ),
}

#[test]
fn or_pattern_nested_in_non_first_alternative() {
    // Regression: the two-state save/restore in `PatternBindings` reset to
    // Initial mode on entering the inner or, so binding `x` in `(x, 1)` was
    // rejected as a duplicate of the outer canonical `x`. With the frame stack
    // the inner or is checked against the outer's canonical set.
    run_outputs(
        "fn f(t (Int, (Int, Int))) Int {\n\
         \tmatch t {\n\
         \t\t(0, (x, 0)) | (1, (x, 1) | (x, 2)) -> x\n\
         \t\t_ -> 99\n\
         \t}\n\
         }\n\
         println(f((0, (10, 0))))\n\
         println(f((1, (20, 1))))\n\
         println(f((1, (30, 2))))\n\
         println(f((1, (40, 5))))\n",
        "10\n20\n30\n99\n",
    );
}

run_case! {
    array_spread_literal: (
        "xs = [1, 2]\n\
         ys = [4, 5]\n\
         zs = [..xs, 3, ..ys, 6]\n\
         println(zs)\n",
        "[1, 2, 3, 4, 5, 6]\n",
    ),
}

reject_case! {
    array_concat_operator_removed: ("xs = [1] ++ [2]\nprintln(xs)\n", "Unexpected '++'"),
}

#[test]
fn nested_ctor_pattern_exhaustive() {
    // Regression: cycle-guard in resolve_icon leaked `seen` across sibling
    // type-args, so the payload type after specializing on Ok had no variants
    // and `Ok(Nil)` was reported as non-exhaustive.
    check_ok("match Ok(Nil) { Ok(Nil) -> println('y') Err(e) -> println(e) }\n");
    check_ok(
        "type T {\n\tA\n\tB\n}\n\
         match Ok(A) { Ok(A) -> println('a') Ok(B) -> println('b') Err(e) -> println(e) }\n",
    );
}

#[test]
fn module_builtins_qualified_and_destructured() {
    check_ok(
        "import al/net\n\
         match net.listen('0.0.0.0', 8080) { Ok(s) -> println(s) Err(e) -> println(e) }\n",
    );
    check_ok(
        "import al/net.{listen, Server}\n\
         fn go(s Server) Nil { println(s) }\n\
         match listen('0.0.0.0', 8080) { Ok(s) -> go(s) Err(e) -> println(e) }\n",
    );
    check_ok(
        "import al/io\n\
         x = io.read_text('a') or ''\n\
         println(x)\n",
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

// ===========================================================================
// PreludeBindings + Bool-as-enum + pure-AL stdlib
// ===========================================================================

#[test]
fn bool_is_a_normal_two_ctor_type() {
    run_outputs(
        "println(True)\nprintln(False)\nprintln(!True)\n",
        "True\nFalse\nFalse\n",
    );
    run_outputs(
        "fn show(b Bool) String { match b { True -> 'yes'\nFalse -> 'no' } }\n\
         println(show(True))\nprintln(show(1 == 2))\n",
        "yes\nno\n",
    );
    check_rejects(
        "fn f(b Bool) Int { match b { True -> 1 } }\nprintln(f(True))\n",
        "not exhaustive",
    );
    check_rejects(
        "type My { True }\n",
        "is defined in the prelude and cannot be redefined",
    );
}

reject_case! {
    lowercase_true_is_just_an_identifier: ("x = true\n", "Unknown identifier"),
}

#[test]
fn reserved_set_derived_from_prelude_iface() {
    // Prelude types/ctors are reserved...
    check_rejects(
        "type Option(a) {\n\tJust(value a)\n\tNothing\n}\n",
        "is defined in the prelude and cannot be redefined",
    );
    // ...but `@vm` functions are not.
    run_outputs("fn println(x Int) Int { x + 1 }\n_ = println(41)\n", "");
}

#[test]
fn binary_string_literal_patterns() {
    // A bare string-literal segment matches its UTF-8 bytes as a prefix
    // (Op::BinMatchPrefix): `<<'GET ', ..rest>>` is one bounds-checked byte
    // compare. The rest binding is a zero-copy view over the remainder.
    run_outputs(
        "import al/binary\n\
         r = match binary.from_string('GET /index.html') {\n\
         \t<<'GET ', ..rest>> -> binary.to_string(rest)\n\
         \telse -> Err(Nil)\n\
         }\n\
         println(r)\n",
        "Ok(/index.html)\n",
    );
    // Explicit `:utf8` on a string literal is the same pattern.
    run_outputs(
        "import al/binary\n\
         r = match binary.from_string('POST /x') {\n\
         \t<<'POST ':utf8, ..>> -> 1\n\
         \telse -> 0\n\
         }\n\
         println(r)\n",
        "1\n",
    );
    // Without `..rest` the whole binary must be consumed: 'GET' matches
    // exactly, 'GETX' and 'GE' fall through.
    run_outputs(
        "import al/binary\n\
         fn is_get(b Binary) Bool {\n\
         \tmatch b {\n\
         \t\t<<'GET'>> -> True\n\
         \t\telse -> False\n\
         \t}\n\
         }\n\
         println(is_get(binary.from_string('GET')))\n\
         println(is_get(binary.from_string('GETX')))\n\
         println(is_get(binary.from_string('GE')))\n",
        "True\nFalse\nFalse\n",
    );
    // A literal prefix longer than the scrutinee fails cleanly (no read past
    // the end), and arms are tried in order.
    run_outputs(
        "import al/binary\n\
         r = match binary.from_string('DELETE /v') {\n\
         \t<<'GET ', ..>> -> 'get'\n\
         \t<<'DELETE /very/long/path/that/overruns', ..>> -> 'overrun'\n\
         \t<<'DELETE ', ..>> -> 'delete'\n\
         \telse -> 'other'\n\
         }\n\
         println(r)\n",
        "delete\n",
    );
    // A literal prefix can be followed by destructuring segments: parse the
    // minor version digit out of an HTTP version token.
    run_outputs(
        "import al/binary\n\
         r = match binary.from_string('HTTP/1.1') {\n\
         \t<<'HTTP/1.', minor>> -> minor - 48\n\
         \telse -> 0 - 1\n\
         }\n\
         println(r)\n",
        "1\n",
    );
    // Multi-byte UTF-8 literals ('é' is 2 bytes) keep byte-accurate offsets
    // for the rest binding.
    run_outputs(
        "import al/binary\n\
         r = match binary.from_string('héllo world') {\n\
         \t<<'héllo ', ..rest>> -> binary.to_string(rest)\n\
         \telse -> Err(Nil)\n\
         }\n\
         println(r)\n",
        "Ok(world)\n",
    );
    // Consecutive integer literals coalesce into the same single-compare
    // prefix: <<13, 10, ..>> is CRLF.
    run_outputs(
        "import al/binary\n\
         r = match binary.from_string('\\r\\nrest') {\n\
         \t<<13, 10, ..rest>> -> binary.byte_size(rest)\n\
         \telse -> 0 - 1\n\
         }\n\
         println(r)\n",
        "4\n",
    );
    // Mixed string/int literal runs and sub-byte literal widths coalesce too;
    // compile-time encoding must match Op::BinFromInt's MSB-first layout.
    run_outputs(
        "import al/binary\n\
         packet = <<1:4, 2:4, 'AB', 7>>\n\
         r = match packet {\n\
         \t<<1:4, 2:4, 'AB', n>> -> n\n\
         \telse -> 0 - 1\n\
         }\n\
         println(r)\n",
        "7\n",
    );
    // String-literal segments in EXPRESSIONS build the UTF-8 bytes (and fold
    // to a constant): equal to the runtime-built binary.
    run_outputs(
        "import al/binary\n\
         println(<<'hi there'>> == binary.from_string('hi there'))\n\
         println(<<'AB', 67, 'D'>> == binary.from_string('ABCD'))\n\
         println(binary.bit_size(<<''>>))\n",
        "True\nTrue\n0\n",
    );
    // A string literal with an Int size spec is still a type error (the
    // Utf8 default applies only to bare string segments).
    check_rejects(
        "import al/binary\n\
         r = match binary.from_string('AB') {\n\
         \t<<'AB':16>> -> 1\n\
         \telse -> 0\n\
         }\n\
         println(r)\n",
        "Type mismatch",
    );
}

#[test]
fn binary_literal_and_pattern_e2e() {
    // <<a, b>> pattern: scan→parse→compile→VM. 'A'=65, 'B'=66, sum=131.
    run_outputs(
        "import al/binary\n\
         r = match binary.from_string('AB') {\n\
         \t<<a, b>> -> a + b\n\
         \telse -> 0\n\
         }\n\
         println(r)\n",
        "131\n",
    );
    // <<1:4, 2:4>> literal: 0001_0010 = 18, inspect emits whole-byte form.
    run_outputs(
        "import al/string\n\
         println(string.inspect(<<1:4, 2:4>>))\n",
        "<<18>>\n",
    );
}

// ===========================================================================
// `Ctor(..) = expr` — irrefutable constructor destructure (single-arm match)
// ===========================================================================

run_case! {
    ctor_destructure_single_variant_ok: (
        "type Box { Box(value Int) }\n\
         Box(n) = Box(42)\n\
         println(n)\n",
        "42\n",
    ),

    ctor_destructure_multi_field_ok: (
        "type Pair { Pair(a Int, b String) }\n\
         Pair(x, y) = Pair(7, 'hi')\n\
         println(x)\n\
         println(y)\n",
        "7\nhi\n",
    ),

    // Labels bind by declared field order, not by argument position.
    ctor_destructure_labeled_out_of_order: (
        "type Point { Point(x Int, y Int) }\n\
         Point(y: b, x: a) = Point(1, 2)\n\
         println(a)\n\
         println(b)\n",
        "1\n2\n",
    ),

    ctor_destructure_labeled_with_rest: (
        "type T { T(a Int, b Int, c Int) }\n\
         T(c: z, ..) = T(10, 20, 30)\n\
         println(z)\n",
        "30\n",
    ),
}

reject_case! {
    ctor_destructure_refutable_rejected: ("Some(x) = Some(1)\nprintln(x)\n", "refutable"),
    ctor_destructure_nested_refutable_rejected: (
        "type Box { Box(value Option(Int)) }\nBox(Some(n)) = Box(Some(1))\nprintln(n)\n",
        "refutable",
    ),
}

// ===========================================================================
// `TypeName = expr` — typed discard (assert expr's type, drop the value)
// ===========================================================================

run_case! {
    typed_discard_nil_println_ok: ("Nil = println('x')\n", "x\n"),
}

reject_case! {
    typed_discard_string_int_mismatch: ("String = 5\n", "Type mismatch: expected 'String', got 'Int'"),
    typed_discard_int_string_mismatch: ("Int = 'a'\n", "Type mismatch: expected 'Int', got 'String'"),
    typed_discard_constructor_is_not_a_type: ("Some = 1\n", "'Some' is not a type"),
}

// `lower` synthesises an eta-wrapper into `program.code` when a constructor is
// used as a first-class value, so the body's own code lands after it. `emit`
// bakes absolute jump targets, and it used to be handed the address captured
// *before* lowering — every taken jump in the body then pointed
// `eta_wrapper.len()` instructions too low, into the wrapper. The `else` arm is
// the one that jumps; the `then` arm falls through and hid this for both
// branches of the suite.
#[test]
fn a_branch_after_an_eta_wrapper_jumps_to_the_right_place() {
    let src = "import al/array\n\
               type W { W(v Int) }\n\
               fn pick(xs Array(Int)) Int {\n\
               \tws = array.map(xs, W)\n\
               \tif array.length(ws) > 2 { 111 } else { 222 }\n\
               }\n\
               println(pick([1]))\n\
               println(pick([1, 2, 3]))\n";
    run_outputs(src, "222\n111\n");
}

// ===========================================================================
// Inferred (unannotated) types must reach `lower`
//
// `lower` used to re-derive types by re-instantiating each constructor's and
// module function's scheme, so a match on an *inferred* scrutinee saw
// `Option(?a)` where the typechecker had `Option(User)`. An unresolved `?a` is
// not a `Con`, so the field lookup found nothing and lowering aborted — on
// programs `al check` had accepted. Both shapes work with an annotated
// scrutinee, which is what hid this.
// ===========================================================================

#[test]
fn field_access_through_a_constructor_inferred_scrutinee() {
    let src = "type User { User(id Int, name String) }\n\
               fn f() Int {\n\
               \tmatch Some(User(7, 'al')) {\n\
               \t\tNone -> 0\n\
               \t\tSome(u) -> u.id\n\
               \t}\n\
               }\n\
               println(f())\n";
    check_ok(src);
    run_outputs(src, "7\n");
}

#[test]
fn field_access_through_a_module_fn_inferred_scrutinee() {
    let src = "import al/map\n\
               type User { User(id Int, name String) }\n\
               fn f(m Map(Binary, User)) Int {\n\
               \tmatch map.get(m, <<'a'>>) {\n\
               \t\tNone -> 0\n\
               \t\tSome(u) -> u.id\n\
               \t}\n\
               }\n\
               println(f(map.set(map.new(), <<'a'>>, User(7, 'al'))))\n";
    check_ok(src);
    run_outputs(src, "7\n");
}

/// An inferred scrutinee's payload is a heap value, so binding it makes the arm
/// responsible for releasing it. Without the payload's real type Perceus saw
/// "not heap" and emitted no `Drop`, holding the `Boxed` to the end of the
/// frame. The `drop` itself is pinned by the `inferred_scrutinee_drops_heap_payload`
/// Core IR golden; this pins that the program still computes the right answer.
#[test]
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
               println(f())\n";
    run_outputs(src, "4\n");
}

/// The `Err` payload bound by `expr or e -> body` is the LHS type's second
/// argument, not a fresh variable — a heap error value has to be droppable, and
/// its fields have to be reachable.
#[test]
fn or_receiver_binds_a_heap_error_payload() {
    let src = "type Boxed { Boxed(n Int) }\n\
               fn bad() Result(Int, Boxed) { Err(Boxed(9)) }\n\
               fn f() Int { bad() or e -> e.n }\n\
               println(f())\n";
    check_ok(src);
    run_outputs(src, "9\n");
}
