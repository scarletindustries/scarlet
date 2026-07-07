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
        "constructor fields must be labeled",
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
    check_rejects("x = ()\nprintln(x)\n", "tuples need 2+ elements");
}

#[test]
fn single_parens_is_parse_error() {
    check_rejects("x = (5)\nprintln(x)\n", "single-element parens not allowed");
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
        "got 'Option(Int)'",
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
        "unreachable",
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
        "missing field(s): age, email",
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
        "'or' requires the left side to be Option(_) or Result(_, _)",
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
        "Type mismatch: expected 'a', got 'Int'",
    );
}

#[test]
fn rigid_tyvar_same_var_unifies_args() {
    // `f` declares both params as `a`, so `f(1, 's')` must be rejected.
    check_rejects(
        "fn f(x a, y a) a { x }\n\
         println(f(1, 's'))\n",
        "Type mismatch: expected 'Int', got 'String'",
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
    check_rejects("xs = [1] ++ [2]\nprintln(xs)\n", "Unexpected '++'");
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
