mod common;
use common::{check_ok, check_rejects};

#[test]
fn int_plus_string_is_type_error() {
    check_rejects("x = 1 + 'a'\n", "Type mismatch");
}

#[test]
fn non_exhaustive_match_is_error() {
    check_rejects(
        "f = fn(x) { match x { True -> 1 } }\nf(False)\n",
        "not exhaustive",
    );
}

#[test]
fn unknown_identifier_is_error() {
    check_rejects("x = foo\n", "Unknown identifier");
}

#[test]
fn if_condition_must_be_bool() {
    check_rejects("if 1 { 2 } else { 3 }\n", "Type mismatch");
}

// An integer literal whose source text overflows i64 was previously coerced
// silently to `0` with zero diagnostics (`println(99999999999999999999999999)`
// printed 0). These cover all three numeric-literal compile sites: the
// expression path, the match-pattern path, and the range-bound path.

#[test]
fn integer_literal_overflow_is_error() {
    check_rejects("x = 99999999999999999999999999\n", "out of range for Int");
}

#[test]
fn integer_literal_overflow_in_match_pattern_is_error() {
    check_rejects(
        "match 1 { 99999999999999999999999999 -> 0\n _ -> 1 }\n",
        "out of range for Int",
    );
}

#[test]
fn integer_literal_overflow_in_range_pattern_is_error() {
    check_rejects(
        "match 1 { 99999999999999999999999999..0 -> 0\n _ -> 1 }\n",
        "out of range for Int",
    );
}

#[test]
fn unused_let_binding_is_error() {
    check_rejects(
        "x = 1\nprintln('done')\n",
        "'x' is unused; prefix with '_' to ignore",
    );
}

#[test]
fn unused_param_is_error() {
    check_rejects(
        "fn f(a Int, b Int) Int { a }\nprintln(f(1, 2))\n",
        "'b' is unused; prefix with '_' to ignore",
    );
}

#[test]
fn unused_match_binding_is_error() {
    check_rejects(
        "match Some(1) { Some(x) -> 0\n None -> 0 }\n",
        "'x' is unused; prefix with '_' to ignore",
    );
}

#[test]
fn underscore_prefix_suppresses_unused_error() {
    check_ok("_x = 1\nprintln('done')\n");
}

#[test]
fn used_let_binding_is_ok() {
    check_ok("x = 1\nprintln(x)\n");
}

#[test]
fn closure_capture_counts_as_use() {
    check_ok("x = 1\nf = fn() { x }\nprintln(f())\n");
}

// Two of the most common call-site mistakes. `match_fun_type` reports a known
// function called with the wrong number of arguments as `IncorrectArity`
// (rendered "Expected N argument(s), got M"), and a non-function callee as
// `NotFn` (rendered "This value of type 'X' is not a function...").

#[test]
fn wrong_argument_count_is_error() {
    check_rejects(
        "fn f(a Int, b Int) Int { a + b }\nprintln(f(1))\n",
        "Expected 2 argument(s), got 1",
    );
}

#[test]
fn calling_non_function_is_error() {
    check_rejects(
        "x = 5\ny = x(3)\nprintln(y)\n",
        "This value of type 'Int' is not a function and cannot be called",
    );
}

// Construction-site argument diagnostics. `slot_ctor_args` slots positional and
// labelled args into field-declaration order, reporting the four ways a call can
// disagree with the constructor's fields. Each pin names the specific field /
// arity / available-list so a wrong-field or wrong-count regression would fail.

#[test]
fn ctor_missing_field_is_error() {
    check_rejects(
        "type P { P(name String age Int) }\nP(name: 'a')\n",
        "Constructor 'P' is missing field(s): age",
    );
}

#[test]
fn ctor_unknown_field_is_error() {
    check_rejects(
        "type P { P(name String age Int) }\nP(name: 'a', bogus: 1)\n",
        "Constructor 'P' has no field 'bogus'. Available: name, age",
    );
}

#[test]
fn ctor_too_many_positional_is_error() {
    check_rejects(
        "type P { P(name String age Int) }\nP('a', 'b', 'c')\n",
        "Constructor 'P' has 2 field(s) but more were supplied",
    );
}

#[test]
fn ctor_nullary_with_args_is_error() {
    check_rejects(
        "type C { Red Green }\nRed(1)\n",
        "Constructor 'Red' has 0 field(s) but more were supplied",
    );
}

#[test]
fn ctor_duplicate_field_is_error() {
    check_rejects(
        "type P { P(name String age Int) }\nP(name: 'a', name: 'b', age: 1)\n",
        "Field 'name' is specified more than once",
    );
}

// Constructor-in-pattern diagnostics. `type_ctor_pattern` is a distinct compiler
// path from the construction-site checks above: it resolves the constructor name
// against the *pattern* position. An UPPERCASE name that is neither a known
// constructor nor a bound identifier is reported as an unknown constructor (a
// lowercase callee is intercepted earlier by the parser as a different pattern
// form, so this path is only reachable with a constructor-cased name).
#[test]
fn ctor_unknown_in_pattern_is_error() {
    check_rejects(
        "r = match Some(1) { Bogus(x) -> 0\n _ -> 1 }\nprintln(r)\n",
        "Unknown constructor 'Bogus' in pattern",
    );
}

// A nullary constructor (its instantiated type is not a function) given argument
// sub-patterns is rejected, naming the constructor and the supplied count.
#[test]
fn ctor_nullary_with_args_in_pattern_is_error() {
    check_rejects(
        "type C { Red Green }\nfn f(c C) Int { match c { Red(x) -> x\n Green -> 0 } }\nprintln(f(Red))\n",
        "Constructor 'Red' takes no arguments but 1 were given",
    );
}

// Inside a `{ }` block, every statement except the last must either be the
// unit type `Nil` or have its value consumed; a dangling non-Nil expression
// statement is almost always a mistake. The `1 + 2` below is a non-last,
// non-Nil expression statement, so it must be consumed. (Top-level statements
// go through a different path and are *not* subject to this rule.)
#[test]
fn unconsumed_block_expr_statement_is_error() {
    check_rejects("r = {\n\t1 + 2\n\t9\n}\nprintln(r)\n", "must be consumed");
}

// The flip side of the rule above: a *Nil*-typed non-last statement (here a
// `println` call) is allowed, since there is no value to drop. This pins the
// `is_nil` guard so the consumption check can't start over-firing on ordinary
// side-effecting statements.
#[test]
fn nil_typed_block_expr_statement_is_ok() {
    check_ok("r = {\n\tprintln(1)\n\t9\n}\nprintln(r)\n");
}

// Type-annotation hydration diagnostics (the `Hydrator` that turns a written
// type like `Option(Int)` into a `Ty`). These three pin the user-facing message
// for the ways an annotation can be ill-formed against the env's type info.

// Applying a known nominal type to the wrong number of type arguments. `Option`
// has arity 1, so two args is rejected naming the type, the expected count
// (singular noun) and the supplied count. The well-formed `Option(Int)` is
// accepted, so this fails only on a genuine arity-check regression.
#[test]
fn type_arg_arity_mismatch_is_error() {
    check_rejects(
        "fn f(_x Option(Int, String)) Int { 1 }\n",
        "Type 'Option' expects 1 type argument, got 2",
    );
}

// A lowercase name in a constructor field is a type variable, but a field may
// only reference variables bound by the type's own parameter list. `Box` here
// declares none, so `t` is unbound. (Field RHS hydrates with `permit_new`
// disabled, so an unseen var is an error rather than an implicit fresh one.)
#[test]
fn unknown_type_variable_in_ctor_field_is_error() {
    check_rejects("type Box { Box(v t) }\n", "Unknown type variable 't'");
}

// A lowercase name is a type variable and cannot be applied to type arguments
// the way a nominal type constructor can; `a(Int)` is therefore rejected.
#[test]
fn type_variable_cannot_take_arguments_is_error() {
    check_rejects(
        "fn f(x a(Int)) a { x }\n",
        "Type variable 'a' cannot take arguments",
    );
}

// Type-declaration analysis diagnostics. A type's parameter list must have
// distinct names: the second `t` in `Box(t, t)` is a duplicate.
#[test]
fn duplicate_type_parameter_is_error() {
    check_rejects(
        "type Box(t, t) { Box(a t) }\n",
        "Duplicate type parameter 't'",
    );
}

// A type alias may not (directly or transitively) refer to itself: the alias
// dependency graph must be acyclic. `type A = A` is a self-cycle; the analysis
// pass detects it before hydrating the RHS and reports the offending alias.
#[test]
fn recursive_type_alias_self_is_error() {
    check_rejects("type A = A\n", "Recursive type alias 'A'");
}

// The mutual case: `A = B`, `B = A` forms a two-node cycle that is likewise
// rejected (which member of the cycle is named is reported by the cycle
// detector, so this pins the diagnostic class rather than a specific name).
#[test]
fn mutually_recursive_type_alias_is_error() {
    check_rejects("type A = B\ntype B = A\n", "Recursive type alias");
}

// A cycle among some aliases must not stop the remaining, acyclic aliases from
// being registered. Previously the toposort's cycle branch returned early and
// dropped every alias in the module, so `Good` below surfaced as
// "Unknown type 'Good'" instead of the real recursive-alias error.
#[test]
fn recursive_type_alias_cycle_does_not_drop_other_aliases() {
    let src = "type Good = Int\ntype A = B\ntype B = A\nfn f(x Good) Good { x }\nprintln(f(1))\n";
    let path = common::write_temp(src);
    let out = common::run_al("check", &path);
    let _ = std::fs::remove_file(&path);
    let combined = out.combined();
    assert!(!out.success, "expected rejection:\n{combined}");
    assert!(
        combined.contains("Recursive type alias"),
        "expected recursive-alias diagnostic:\n{combined}"
    );
    assert!(
        !combined.contains("Unknown type"),
        "cycle in A/B must not cascade into other aliases:\n{combined}"
    );
}

// Constructor names share the value namespace with fns and consts, so a ctor
// that collides with another ctor (or a fn) is a duplicate definition and
// should be reported the same way.
#[test]
fn duplicate_constructor_across_types_is_error() {
    check_rejects(
        "type A { Red }\ntype B { Red }\n",
        "'Red' is already defined",
    );
}

#[test]
fn constructor_colliding_with_fn_is_error() {
    check_rejects("fn Red() {}\ntype A { Red }\n", "'Red' is already defined");
}

// HM inference occurs-check: `x(x)` forces `x`'s type to unify with `t -> u`
// where the argument `t` is `x` itself, an infinite type. Inference rejects it
// rather than looping.
#[test]
fn infinite_type_self_application_is_error() {
    check_rejects("fn f(x) { x(x) }\n", "Infinite type detected");
}

// Field/tuple access diagnostics. `tuple.N` numeric indexing and `value.field`
// label projection are compiled in `compile_binary` / `compile_field_access`.
// Each pin names the specific message so a regression in the relevant arm
// (bounds check, non-tuple guard, variant-coverage check, unknown-type guard)
// would fail.

// `tuple.N` with N past the end: the index parses but is beyond the statically
// known element count, reported with the actual element count.
#[test]
fn tuple_index_out_of_bounds_is_error() {
    check_rejects(
        "t = (1, 2)\nprintln(t.5)\n",
        "Tuple index 5 out of bounds (tuple has 2 elements)",
    );
}

// `.N` numeric projection is only valid on tuples; applying it to a non-tuple
// receiver (here an `Int`) is rejected naming the offending index.
#[test]
fn numeric_index_on_non_tuple_is_error() {
    check_rejects("x = 5\nprintln(x.0)\n", "Cannot index .0 on non-tuple type");
}

// `value.field` projects a label that must be present on EVERY variant, since
// the runtime variant is not statically known. `r` exists on `Circle` but not
// `Square`, so the projection is rejected naming the field, type, and the
// variant that lacks it.
#[test]
fn field_not_on_every_variant_is_error() {
    check_rejects(
        "type Shape { Circle(r Int) Square(side Int) }\nfn g(s Shape) Int { s.r }\n",
        "Field 'r' is not present on every variant of 'Shape' (missing on 'Square')",
    );
}

// A labelled-field projection onto a tuple type finds no field of that name;
// the receiver type is rendered structurally in the message.
#[test]
fn field_access_on_tuple_is_error() {
    check_rejects("t = (1, 2)\nt.x\n", "Type '(Int, Int)' has no field 'x'");
}

// Projecting a field off a value whose type is still an unresolved inference
// variable (an unannotated parameter) cannot be checked; the user is directed
// to add an annotation.
#[test]
fn field_access_on_unknown_type_is_error() {
    check_rejects(
        "fn g(x) { x.name }\n",
        "Cannot access field 'name' on a value of unknown type — add a type annotation",
    );
}

// Identifier-resolution diagnostics from `compile_identifier`. An unknown name
// close (small edit distance) to an in-scope name yields a did-you-mean
// suggestion — `prntln` is one deletion from the bound `println`. (The
// no-suggestion path is covered by `unknown_identifier_is_error` above.)
#[test]
fn unknown_identifier_suggests_close_name() {
    check_rejects("println = 1\nfoo = prntln\n", "Did you mean 'println'?");
}

// A builtin has a type but no first-class runtime binding, so naming one in
// value position (rather than calling it) is rejected.
#[test]
fn builtin_used_as_value_is_error() {
    check_rejects(
        "x = println\n",
        "'println' is a builtin and cannot be used as a value",
    );
}

// Binding-pattern refutability, declaration placement, and `or`-binder
// diagnostics. Each negative pin is paired with a positive control that
// differs only in the one feature under test, so a regression that stopped
// firing — or started over-firing — would fail.

// A tuple destructuring binding must be irrefutable: every sub-pattern has to
// match unconditionally. A literal sub-pattern (`1`) is refutable, so
// `(x, 1) = (1, 2)` is rejected. (This is the tuple/non-constructor path; the
// constructor path emits a different "constructor destructuring binding..."
// message.)
#[test]
fn refutable_tuple_destructuring_binding_is_error() {
    check_rejects(
        "(x, 1) = (1, 2)\nprintln(x)\n",
        "Destructuring binding pattern must be irrefutable",
    );
}

// Positive control: an all-binders tuple pattern is irrefutable and accepted,
// so the check above fires on refutability, not on tuple destructuring itself.
#[test]
fn irrefutable_tuple_destructuring_binding_is_ok() {
    check_ok("(x, y) = (1, 2)\nprintln(x + y)\n");
}

// Declarations (`type`/`fn`/`const`) are only legal at a module's top level. A
// `type` nested inside a function body is rejected, and the `{kind}` in the
// message names the declaration's kind ("type").
#[test]
fn nested_type_declaration_is_error() {
    check_rejects(
        "fn outer() Int {\n\ttype Inner { A }\n\t1\n}\n",
        "type declarations are only allowed at the top level",
    );
}

// `or` with a receiver binds the failure payload — valid on `Result` (the
// `Err` carries a value) but not on `Option`, whose `None` carries nothing, so
// `Some(5) or v -> 0` is rejected for trying to bind `v`.
#[test]
fn or_with_receiver_on_option_is_error() {
    check_rejects(
        "x = Some(5) or v -> 0\nprintln(x)\n",
        "'or' on an Option does not bind a value",
    );
}

// Positive control: the same `or` on the same `Option` without a receiver is
// accepted, so the diagnostic above fires on the value-binding receiver, not
// on `or` over an `Option`.
#[test]
fn or_without_receiver_on_option_is_ok() {
    check_ok("x = Some(5) or 0\nprintln(x)\n");
}

// Nominal identity: a locally-declared type that shares its NAME with a
// stdlib type is still a different type. Before types carried their nominal
// id in the Con node, unification compared names, so h1's `Parsed` flowed
// silently into a binding annotated with the local `Parsed` — a soundness
// hole, not just a diagnostics gap.
#[test]
fn same_named_type_from_another_module_does_not_unify() {
    check_rejects(
        "import al/binary\n\
         import al/http/h1\n\
         type Parsed {\n\
         \tLocalDone(x Int)\n\
         \tLocalOther\n\
         }\n\
         fn f() Parsed {\n\
         \th1.parse_request(binary.from_string('GET / HTTP/1.1\\r\\n\\r\\n'), 0)\n\
         }\n\
         println(f())\n",
        "Type mismatch",
    );
}

// And the local type keeps working on its own terms next to the import: each
// `Parsed` answers for its own constructors and exhaustiveness.
#[test]
fn same_named_local_and_stdlib_types_coexist() {
    check_ok(
        "import al/binary\n\
         import al/http/h1.{Done, NeedMore, Bad}\n\
         type Parsed {\n\
         \tLocalDone(x Int)\n\
         \tLocalOther\n\
         }\n\
         fn local_value(p Parsed) Int {\n\
         \tmatch p {\n\
         \t\tLocalDone(x) -> x\n\
         \t\tLocalOther -> 0\n\
         \t}\n\
         }\n\
         remote = match h1.parse_request(binary.from_string('GET / HTTP/1.1\\r\\n\\r\\n'), 0) {\n\
         \tDone(_, _, version, _, _) -> version\n\
         \tNeedMore -> 0 - 1\n\
         \tBad(s) -> s\n\
         }\n\
         println(remote + local_value(LocalDone(41)))\n",
    );
}
