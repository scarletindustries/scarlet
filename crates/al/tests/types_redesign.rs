mod common;
use common::{check_ok, check_rejects, run_outputs};

// ===========================================================================
// `type` keyword — definitions
// ===========================================================================

#[test]
fn type_keyword_single_variant() {
    run_outputs(
        "type User { User(name String age Int) }\n\
         u = User(name: 'al', age: 18)\n\
         println(u.name)\n\
         println(u.age)\n",
        "al\n18\n",
    );
}

#[test]
fn type_keyword_multi_variant() {
    run_outputs(
        "type Shape { Circle(r Int) Rect(w Int h Int) }\n\
         fn area(s Shape) Int {\n\
         \tmatch s {\n\
         \t\tCircle(r) -> 3 * r * r\n\
         \t\tRect(w, h) -> w * h\n\
         \t}\n\
         }\n\
         println(area(Circle(10)))\n\
         println(area(Rect(4, 5)))\n",
        "300\n20\n",
    );
}

#[test]
fn type_alias_is_transparent() {
    run_outputs(
        "type Id = Int\n\
         fn next(i Id) Id { i + 1 }\n\
         println(next(41))\n",
        "42\n",
    );
}

#[test]
fn unlabeled_field_in_def_is_rejected() {
    // Every field in a type definition must carry a label.
    check_rejects(
        "type Wrap { Wrap(Int) }\n\
         x = Wrap(1)\n",
        "",
    );
}

// ===========================================================================
// Constructors are functions
// ===========================================================================

#[test]
fn some_call_is_ordinary_call() {
    run_outputs(
        "x = Some(5)\n\
         println(x or 0)\n",
        "5\n",
    );
}

#[test]
fn constructor_is_first_class() {
    run_outputs(
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
    );
}

#[test]
fn nullary_constructor_is_value() {
    run_outputs(
        "x = None\n\
         println(x or 7)\n",
        "7\n",
    );
}

// ===========================================================================
// `if` requires `else`
// ===========================================================================

#[test]
fn if_without_else_is_error() {
    check_rejects(
        "x = if True { 1 }\n\
         println(x)\n",
        "else",
    );
}

// ===========================================================================
// Parentheses are tuples-only
// ===========================================================================

#[test]
fn empty_parens_is_parse_error() {
    check_rejects("x = ()\nprintln(x)\n", "");
}

#[test]
fn single_parens_is_parse_error() {
    check_rejects("x = (5)\nprintln(x)\n", "");
}

#[test]
fn block_is_grouping() {
    run_outputs("println({1 + 2} * 3)\n", "9\n");
}

// ===========================================================================
// Array index returns Option
// ===========================================================================

#[test]
fn index_returns_option() {
    run_outputs(
        "xs = [10, 20, 30]\n\
         println(xs[0] or -1)\n\
         println(xs[9] or -1)\n",
        "10\n-1\n",
    );
}

#[test]
fn index_without_unwrap_is_option_typed() {
    // `xs[0] + 1` should be rejected because `xs[0]` is `Option(Int)`, not `Int`.
    check_rejects(
        "xs = [10, 20, 30]\n\
         y = xs[0] + 1\n\
         println(y)\n",
        "",
    );
}

#[test]
fn index_negative_returns_none() {
    // A negative index hits the `idx >= 0` guard in `Op::Index` /
    // `Op::IndexOrElse`, a path distinct from the positive out-of-bounds case
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
// Prelude name reservation
// ===========================================================================

#[test]
fn redefining_prelude_type_is_rejected() {
    check_rejects(
        "type Option(a) { Some(value a) None }\n\
         x = Some(1)\n",
        "",
    );
}

#[test]
fn redefining_prelude_ctor_is_rejected() {
    check_rejects(
        "type X { Ok(value Int) }\n\
         x = Ok(1)\n",
        "",
    );
}

// ===========================================================================
// `.field` totality
// ===========================================================================

#[test]
fn field_access_total_across_variants() {
    run_outputs(
        "type Named { Person(name String age Int) Org(name String size Int) }\n\
         fn name_of(n Named) String { n.name }\n\
         println(name_of(Person(name: 'al', age: 18)))\n\
         println(name_of(Org(name: 'anthropic', size: 1000)))\n",
        "al\nanthropic\n",
    );
}

#[test]
fn field_access_partial_is_rejected() {
    check_rejects(
        "type Named { Person(name String age Int) Org(name String size Int) }\n\
         fn age_of(n Named) Int { n.age }\n\
         println(age_of(Person(name: 'al', age: 18)))\n",
        "",
    );
}

#[test]
fn field_access_on_unbound_var_is_rejected() {
    check_rejects(
        "fn name_of(x) { x.name }\n\
         println(name_of(1))\n",
        "",
    );
}

// ===========================================================================
// Recursive and mutually-recursive types/functions
// ===========================================================================

#[test]
fn recursive_type_compiles() {
    check_ok(
        "type Tree(a) { Leaf Node(l Tree(a) v a r Tree(a)) }\n\
         t Tree(Int) = Node(l: Leaf, v: 1, r: Leaf)\n\
         t\n",
    );
}

#[test]
fn recursive_type_runs() {
    run_outputs(
        "type Tree(a) { Leaf Node(l Tree(a) v a r Tree(a)) }\n\
         fn size(t Tree(a)) Int {\n\
         \tmatch t {\n\
         \t\tLeaf -> 0\n\
         \t\tNode(l, _, r) -> 1 + size(l) + size(r)\n\
         \t}\n\
         }\n\
         t = Node(l: Node(l: Leaf, v: 1, r: Leaf), v: 2, r: Leaf)\n\
         println(size(t))\n",
        "2\n",
    );
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

#[test]
fn nested_option_match_is_exhaustive() {
    check_ok(
        "x = Some(Some(5))\n\
         r = match x {\n\
         \tSome(Some(n)) -> 'ss ${n}'\n\
         \tSome(None) -> 'sn'\n\
         \tNone -> 'n'\n\
         }\n\
         println(r)\n",
    );
}

#[test]
fn nested_option_match_runs() {
    run_outputs(
        "x = Some(Some(5))\n\
         r = match x {\n\
         \tSome(Some(n)) -> 'ss ${n}'\n\
         \tSome(None) -> 'sn'\n\
         \tNone -> 'n'\n\
         }\n\
         println(r)\n",
        "ss 5\n",
    );
}

#[test]
fn nested_result_match_is_exhaustive() {
    check_ok(
        "fn classify(x Result(Result(Int, String), String)) String {\n\
         \tmatch x {\n\
         \t\tOk(Ok(n)) -> 'ok ${n}'\n\
         \t\tOk(Err(e)) -> 'okerr ${e}'\n\
         \t\tErr(e) -> 'err ${e}'\n\
         \t}\n\
         }\n\
         println(classify(Ok(Ok(1))))\n",
    );
}

#[test]
fn nested_option_missing_inner_arm_reports_precise_witness() {
    // Dropping `Some(None)` is genuinely non-exhaustive, and now that the inner
    // `Option`'s variants are known the witness names it exactly instead of
    // collapsing to `Some(_)`.
    check_rejects(
        "x = Some(Some(5))\n\
         r = match x {\n\
         \tSome(Some(n)) -> 'ss ${n}'\n\
         \tNone -> 'n'\n\
         }\n\
         println(r)\n",
        "Some(None)",
    );
}

#[test]
fn nested_option_redundant_wildcard_is_rejected() {
    // Before the fix a `Some(_)` arm was *required* to silence a false
    // non-exhaustiveness error; now the explicit inner arms already cover
    // `Some(_)`, so that wildcard is correctly reported as dead code.
    check_rejects(
        "x = Some(Some(5))\n\
         r = match x {\n\
         \tSome(Some(n)) -> 'ss ${n}'\n\
         \tSome(None) -> 'sn'\n\
         \tSome(_) -> 'other'\n\
         \tNone -> 'n'\n\
         }\n\
         println(r)\n",
        "unreachable",
    );
}

#[test]
fn non_uniform_recursive_type_resolution_terminates() {
    // `Nest`'s argument grows at every level, so the instance key never repeats
    // and the recurrence bound is what stops resolution from looping forever.
    // A match on it still type-checks: the recursive position is cut off to an
    // infinite-constructor type, so the wildcard arm is required and accepted.
    check_ok(
        "type Nest(t) { More(inner Nest((t, t))) Done }\n\
         fn f(n Nest(Int)) Int {\n\
         \tmatch n {\n\
         \t\tMore(_) -> 1\n\
         \t\tDone -> 0\n\
         \t}\n\
         }\n\
         println('${f(Done)}')\n",
    );
}

#[test]
fn mutual_recursion_functions() {
    run_outputs(
        "fn is_even(n Int) Bool {\n\
         \tif n == 0 { True } else { is_odd(n - 1) }\n\
         }\n\
         fn is_odd(n Int) Bool {\n\
         \tif n == 0 { False } else { is_even(n - 1) }\n\
         }\n\
         println(is_even(10))\n\
         println(is_odd(7))\n",
        "True\nTrue\n",
    );
}

// ===========================================================================
// Exhaustiveness — variants and fields
// ===========================================================================

#[test]
fn unreachable_arm_is_error() {
    check_rejects(
        "fn f(b Bool) Int {\n\
         \tmatch b {\n\
         \t\tTrue -> 1\n\
         \t\tFalse -> 2\n\
         \t\tTrue -> 3\n\
         \t}\n\
         }\n\
         f(True)\n",
        "",
    );
}

#[test]
fn ctor_pattern_missing_fields_without_spread_is_error() {
    check_rejects(
        "type User { User(name String age Int email String) }\n\
         fn f(u User) String {\n\
         \tmatch u {\n\
         \t\tUser(name: n) -> n\n\
         \t}\n\
         }\n\
         f(User(name: 'a', age: 1, email: 'e'))\n",
        "",
    );
}

#[test]
fn ctor_pattern_with_spread_is_ok() {
    run_outputs(
        "type User { User(name String age Int email String) }\n\
         fn f(u User) String {\n\
         \tmatch u {\n\
         \t\tUser(name: n, ..) -> n\n\
         \t}\n\
         }\n\
         println(f(User(name: 'alice', age: 30, email: 'a@b')))\n",
        "alice\n",
    );
}

// ===========================================================================
// `or` typing
// ===========================================================================

#[test]
fn or_on_non_option_result_is_rejected() {
    check_rejects(
        "x = 5 or 0\n\
         println(x)\n",
        "",
    );
}

#[test]
fn or_on_result_unwraps_ok() {
    run_outputs(
        "fn f(b Bool) Result(Int, String) {\n\
         \tif b { Ok(42) } else { Err('nope') }\n\
         }\n\
         println(f(True) or -1)\n\
         println(f(False) or -1)\n",
        "42\n-1\n",
    );
}

// ===========================================================================
// Rigid type variables
// ===========================================================================

#[test]
fn rigid_tyvar_body_mismatch_is_rejected() {
    // Body returns concrete `Int` where signature promised `a`.
    check_rejects(
        "fn bad(x a) a { 1 }\n\
         println(bad('s'))\n",
        "",
    );
}

#[test]
fn rigid_tyvar_same_var_unifies_args() {
    // `f` declares both params as `a`, so `f(1, 's')` must be rejected.
    check_rejects(
        "fn f(x a, y a) a { x }\n\
         println(f(1, 's'))\n",
        "",
    );
}

#[test]
fn rigid_tyvar_same_var_accepts_same_type() {
    run_outputs(
        "fn f(x a, _y a) a { x }\n\
         println(f(1, 2))\n",
        "1\n",
    );
}

// ===========================================================================
// Positional vs labeled construction
// ===========================================================================

#[test]
fn positional_construction() {
    run_outputs(
        "type Pair { Pair(fst Int snd Int) }\n\
         p = Pair(1, 2)\n\
         println(p.fst + p.snd)\n",
        "3\n",
    );
}

#[test]
fn labeled_construction_reordered() {
    run_outputs(
        "type Pair { Pair(fst Int snd Int) }\n\
         p = Pair(snd: 2, fst: 1)\n\
         println(p.fst)\n\
         println(p.snd)\n",
        "1\n2\n",
    );
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
        "type P { P(name String age Int) }\n\
         base = P(name: 'al', age: 18)\n\
         older = P(..base, age: 19)\n\
         println(older.name)\n\
         println(older.age)\n\
         println(base.age)\n",
        "al\n19\n18\n",
    );
}

#[test]
fn ctor_record_update_at_most_one_spread() {
    // A constructor record-update accepts a single `..base`; a second spread is
    // rejected.
    check_rejects(
        "type P { P(name String age Int) }\n\
         base = P(name: 'al', age: 18)\n\
         older = P(..base, ..base)\n\
         println(older.age)\n",
        "Constructor call may have at most one spread",
    );
}

#[test]
fn spread_arg_in_plain_call_rejected() {
    // Spread arguments only make sense in constructor record-update calls; in an
    // ordinary function call they are a placement error.
    check_rejects(
        "fn f(a Int) Int { a }\n\
         println(f(..[1]))\n",
        "Spread arguments are only allowed in constructor record-update calls",
    );
}

#[test]
fn labelled_arg_in_plain_call_rejected() {
    // Labelled arguments are a constructor-only affordance; passing one to an
    // ordinary function is a placement error.
    check_rejects(
        "fn f(a Int) Int { a }\n\
         println(f(a: 1))\n",
        "Labelled arguments are only allowed in constructor calls",
    );
}

#[test]
fn match_guard_basic() {
    run_outputs(
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
    );
}

#[test]
fn match_guard_with_constructor() {
    run_outputs(
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
    );
}

#[test]
fn match_guard_non_exhaustive_errors() {
    check_rejects(
        "fn f(n Int) String {\n\
         \tmatch n {\n\
         \t\tx if x < 2 -> 'a'\n\
         \t}\n\
         }\n",
        "exhaustive",
    );
}

#[test]
fn match_guard_type_must_be_bool() {
    check_rejects(
        "fn f(n Int) String {\n\
         \tmatch n {\n\
         \t\tx if x -> 'a'\n\
         \t\t_ -> 'b'\n\
         \t}\n\
         }\n",
        "Bool",
    );
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

#[test]
fn or_pattern_unequal_bindings_still_rejected() {
    // The scoping fix must not relax the core invariant: every alternative of
    // an or-pattern must bind exactly the same names.
    check_rejects(
        "type R { Good(v Int) Bad(v Int) }\n\
         fn h(r R) Int {\n\
         \tmatch r {\n\
         \t\tGood(x) | Bad(z) -> 0\n\
         \t}\n\
         }\n\
         println(h(Good(1)))\n",
        "every alternative",
    );
}

#[test]
fn array_spread_literal() {
    run_outputs(
        "xs = [1, 2]\n\
         ys = [4, 5]\n\
         zs = [..xs, 3, ..ys, 6]\n\
         println(zs)\n",
        "[1, 2, 3, 4, 5, 6]\n",
    );
}

#[test]
fn array_concat_operator_removed() {
    check_rejects("xs = [1] ++ [2]\nprintln(xs)\n", "");
}

#[test]
fn nested_ctor_pattern_exhaustive() {
    // Regression: cycle-guard in resolve_icon leaked `seen` across sibling
    // type-args, so the payload type after specializing on Ok had no variants
    // and `Ok(Nil)` was reported as non-exhaustive.
    check_ok("match Ok(Nil) { Ok(Nil) -> println('y') Err(e) -> println(e) }\n");
    check_ok(
        "type T { A B }\n\
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

#[test]
fn lowercase_true_is_just_an_identifier() {
    check_rejects("x = true\n", "Unknown identifier");
}

#[test]
fn reserved_set_derived_from_prelude_iface() {
    // Prelude types/ctors are reserved...
    check_rejects(
        "type Option(a) { Just(value a) Nothing }\n",
        "is defined in the prelude and cannot be redefined",
    );
    // ...but `@vm` functions are not.
    run_outputs("fn println(x Int) Int { x + 1 }\n_ = println(41)\n", "");
}

#[test]
fn stdlib_option_result_list_int_bool() {
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
    // or_else: Some keeps the original option; None falls back to the supplied one.
    run_outputs(
        "import al/option\n\
         println(option.or_else(Some(1), Some(2)))\n\
         println(option.or_else(None, Some(2)))\n",
        "Some(1)\nSome(2)\n",
    );
    // is_none: True on None, False on Some — exercising both match arms.
    run_outputs(
        "import al/option\n\
         println(option.is_none(None))\n\
         println(option.is_none(Some(1)))\n",
        "True\nFalse\n",
    );
    // The arms the existing stdlib_option block misses: map leaves None untouched,
    // unwrap on Some yields the inner value, is_some is False on None.
    run_outputs(
        "import al/option\n\
         println(option.map(None, fn(x) x * 2))\n\
         println(option.unwrap(Some(5), 99))\n\
         println(option.is_some(None))\n",
        "None\n5\nFalse\n",
    );
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
    // Passthrough arms (the ones the existing stdlib_result misses): map leaves
    // an Err untouched, map_err leaves an Ok untouched.
    run_outputs(
        "import al/result\n\
         println(result.map(Err('e'), fn(x) x + 1))\n\
         println(result.map_err(Ok(5), fn(e) e))\n",
        "Err(e)\nOk(5)\n",
    );
    run_outputs(
        "import al/list\n\
         println(list.map([1, 2, 3], fn(x) x * 10))\n\
         println(list.filter([1, 2, 3, 4], fn(x) x > 2))\n\
         println(list.fold([1, 2, 3, 4], 0, fn(a, b) a + b))\n\
         println(list.reverse([1, 2, 3]))\n\
         println(list.contains([1, 2, 3], 2))\n",
        "[10, 20, 30]\n[3, 4]\n10\n[3, 2, 1]\nTrue\n",
    );
    // find: returns Some(first match), None when nothing satisfies the predicate.
    run_outputs(
        "import al/list\n\
         println(list.find([1, 2, 3], fn(x) x > 1))\n\
         println(list.find([1, 2, 3], fn(x) x > 9))\n",
        "Some(2)\nNone\n",
    );
    // any / all: any is True iff some element matches; all is True iff every one does.
    run_outputs(
        "import al/list\n\
         println(list.any([1, 2, 3], fn(x) x > 2))\n\
         println(list.any([1, 2, 3], fn(x) x > 9))\n\
         println(list.all([2, 4, 6], fn(x) x % 2 == 0))\n\
         println(list.all([2, 3], fn(x) x % 2 == 0))\n",
        "True\nFalse\nTrue\nFalse\n",
    );
    // Empty-list base cases: the `[] ->` arm of each recursive list fn and the
    // not-found terminal of contains.
    run_outputs(
        "import al/list\n\
         println(list.map([], fn(x) x * 10))\n\
         println(list.filter([], fn(x) x > 2))\n\
         println(list.fold([], 0, fn(a, b) a + b))\n\
         println(list.length([]))\n\
         println(list.reverse([]))\n\
         println(list.contains([1, 2], 9))\n",
        "[]\n[]\n0\n0\n[]\nFalse\n",
    );
    run_outputs(
        "import al/int\n\
         println(int.max(3, 7))\n\
         println(int.min(3, 7))\n\
         println(int.abs(0 - 5))\n\
         println(int.clamp(99, 0, 10))\n\
         println(int.to_string(42))\n",
        "7\n3\n5\n10\n42\n",
    );
    run_outputs(
        "import al/bool\n\
         println(bool.negate(True))\n\
         println(bool.to_string(False))\n",
        "False\nFalse\n",
    );
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
    // Float bridges are explicitly lossy conveniences.
    run_outputs(
        "import al/decimal\n\
         println(decimal.to_float(decimal.new(25, 1)))\n\
         println(decimal.to_string(decimal.from_float(2.5, 2)))\n",
        "2.5\n2.50\n",
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

// The ASCII byte builtins each hydrate a distinct `Scheme` from their `@vm`
// declaration; one `check_ok` per builtin pins that the call site type-checks
// against the declared signature (the result is consumed so the program is a
// complete, well-typed unit).
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

#[test]
fn stdlib_binary_ascii_builtins() {
    // index_of : (Binary, Binary, Int) -> Option(Int)
    check_ok(
        "import al/binary\n\
         i = binary.index_of(binary.from_string('abc'), binary.from_string('b'), 0) or 0\n\
         println(i)\n",
    );
    // parse_int : (Binary, Int) -> Option(Int)
    check_ok(
        "import al/binary\n\
         n = binary.parse_int(binary.from_string('42'), 10) or 0\n\
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
    // from_int_ascii : (Int, Int) -> Binary
    check_ok(
        "import al/binary\n\
         println(binary.to_string(binary.from_int_ascii(255, 16)))\n",
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
         println(float.min(1.5, 3.2))\n\
         println(float.max(1.5, 3.2))\n",
        "2.5\n1.5\n3.2\n",
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

#[test]
fn ctor_destructure_single_variant_ok() {
    run_outputs(
        "type Box { Box(value Int) }\n\
         Box(n) = Box(42)\n\
         println(n)\n",
        "42\n",
    );
}

#[test]
fn ctor_destructure_multi_field_ok() {
    run_outputs(
        "type Pair { Pair(a Int b String) }\n\
         Pair(x, y) = Pair(7, 'hi')\n\
         println(x)\n\
         println(y)\n",
        "7\nhi\n",
    );
}

#[test]
fn ctor_destructure_refutable_rejected() {
    check_rejects("Some(x) = Some(1)\nprintln(x)\n", "refutable");
}

#[test]
fn ctor_destructure_nested_refutable_rejected() {
    check_rejects(
        "type Box { Box(value Option(Int)) }\n\
         Box(Some(n)) = Box(Some(1))\n\
         println(n)\n",
        "refutable",
    );
}

// ===========================================================================
// `TypeName = expr` — typed discard (assert expr's type, drop the value)
// ===========================================================================

#[test]
fn typed_discard_nil_println_ok() {
    run_outputs("Nil = println('x')\n", "x\n");
}

#[test]
fn typed_discard_string_int_mismatch() {
    check_rejects(
        "String = 5\n",
        "Type mismatch: expected 'String', got 'Int'",
    );
}

#[test]
fn typed_discard_int_string_mismatch() {
    check_rejects("Int = 'a'\n", "Type mismatch: expected 'Int', got 'String'");
}

#[test]
fn typed_discard_constructor_is_not_a_type() {
    check_rejects("Some = 1\n", "'Some' is not a type");
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
