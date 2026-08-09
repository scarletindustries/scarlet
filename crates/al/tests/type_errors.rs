mod common;
use common::check_rejects;

reject_case! {
    int_plus_string_is_type_error: ("x = 1 + 'a'\n", "Type mismatch"),
    non_exhaustive_match_is_error:
        ("f = fn(x) { match x { True -> 1 } }\nf(False)\n", "not exhaustive"),
    unknown_identifier_is_error: ("x = foo\n", "Unknown identifier"),
    if_condition_must_be_bool: ("if 1 { 2 } else { 3 }\n", "Type mismatch"),

    // All three numeric-literal compile sites: expression, match pattern, range bound.
    integer_literal_overflow_is_error:
        ("x = 99999999999999999999999999\n", "out of range for Int"),
    integer_literal_overflow_in_match_pattern_is_error:
        ("match 1 { 99999999999999999999999999 -> 0\n _ -> 1 }\n", "out of range for Int"),
    integer_literal_overflow_in_range_pattern_is_error:
        ("match 1 { 99999999999999999999999999..0 -> 0\n _ -> 1 }\n", "out of range for Int"),

    unused_let_binding_is_error:
        ("x = 1\nprintln('done')\n", "'x' is unused; prefix with '_' to ignore"),
    unused_param_is_error: (
        "fn f(a Int, b Int) Int { a }\nprintln(f(1, 2))\n",
        "'b' is unused; prefix with '_' to ignore",
    ),
    unused_match_binding_is_error: (
        "match Some(1) { Some(x) -> 0\n None -> 0 }\n",
        "'x' is unused; prefix with '_' to ignore",
    ),
}

ok_case! {
    underscore_prefix_suppresses_unused_error: ("_x = 1\nprintln('done')\n"),

    used_let_binding_is_ok: ("x = 1\nprintln(x)\n"),

    closure_capture_counts_as_use: ("x = 1\nf = fn() { x }\nprintln(f())\n"),
}

reject_case! {
    // Call-site mistakes, both from `match_fun_type`.
    wrong_argument_count_is_error:
        ("fn f(a Int, b Int) Int { a + b }\nprintln(f(1))\n", "Expected 2 argument(s), got 1"),
    calling_non_function_is_error: (
        "x = 5\ny = x(3)\nprintln(y)\n",
        "This value of type 'Int' is not a function and cannot be called",
    ),

    // Construction-site argument diagnostics, all from `slot_ctor_args`.
    ctor_missing_field_is_error: (
        "type P { P(name String, age Int) }\nP(name: 'a')\n",
        "Constructor 'P' is missing field(s): age",
    ),
    ctor_unknown_field_is_error: (
        "type P { P(name String, age Int) }\nP(name: 'a', bogus: 1)\n",
        "Constructor 'P' has no field 'bogus'. Available: name, age",
    ),
    ctor_too_many_positional_is_error: (
        "type P { P(name String, age Int) }\nP('a', 'b', 'c')\n",
        "Constructor 'P' has 2 field(s) but more were supplied",
    ),
    ctor_nullary_with_args_is_error: (
        "type C {\n\tRed\n\tGreen\n}\nRed(1)\n",
        "Constructor 'Red' has 0 field(s) but more were supplied",
    ),
    ctor_duplicate_field_is_error: (
        "type P { P(name String, age Int) }\nP(name: 'a', name: 'b', age: 1)\n",
        "Field 'name' is specified more than once",
    ),

    /// `type_ctor_pattern` is a separate path from the construction-site checks
    /// above. Only reachable with a constructor-cased name: the parser routes a
    /// lowercase callee to a different pattern form.
    ctor_unknown_in_pattern_is_error: (
        "r = match Some(1) { Bogus(x) -> 0\n _ -> 1 }\nprintln(r)\n",
        "Unknown constructor 'Bogus' in pattern",
    ),
    ctor_nullary_with_args_in_pattern_is_error: (
        "type C {\n\tRed\n\tGreen\n}\nfn f(c C) Int { match c { Red(x) -> x\n Green -> 0 } }\nprintln(f(Red))\n",
        "Constructor 'Red' takes no arguments but 1 were given",
    ),

    /// Inside a `{ }` block every non-last statement must be `Nil` or consumed.
    /// Top-level statements go through a different path and are exempt.
    unconsumed_block_expr_statement_is_error:
        ("r = {\n\t1 + 2\n\t9\n}\nprintln(r)\n", "must be consumed"),
}

ok_case! {
    // Control for the rule above: pins the `is_nil` guard against over-firing.
    nil_typed_block_expr_statement_is_ok: ("r = {\n\tprintln(1)\n\t9\n}\nprintln(r)\n"),
}

reject_case! {
    // Diagnostics from the `Hydrator`, which turns a written type into a `Ty`.

    type_arg_arity_mismatch_is_error: (
        "fn f(_x Option(Int, String)) Int { 1 }\n",
        "Type 'Option' expects 1 type argument, got 2",
    ),
    /// A field RHS hydrates with `permit_new` disabled, so an unseen var is an
    /// error rather than an implicit fresh one.
    unknown_type_variable_in_ctor_field_is_error:
        ("type Box { Box(v t) }\n", "Unknown type variable 't'"),
    type_variable_cannot_take_arguments_is_error:
        ("fn f(x a(Int)) a { x }\n", "Type variable 'a' cannot take arguments"),
    /// Without a return type there is no inference context to solve, so `fn(Int)`
    /// would mint an unconstrained var that typechecks against anything.
    fn_type_without_return_in_ctor_field_is_error: (
        "type F { F(g fn(Int)) }\n",
        "Function type in a type definition must declare a return type",
    ),
    /// Same rule on an alias RHS, which also hydrates with `permit_new` disabled.
    fn_type_without_return_in_alias_is_error: (
        "type Callback = fn(Int)\n",
        "Function type in a type definition must declare a return type",
    ),

    duplicate_type_parameter_is_error:
        ("type Box(t, t) { Box(a t) }\n", "Duplicate type parameter 't'"),
    recursive_type_alias_self_is_error: ("type A = A\n", "Recursive type alias 'A'"),
    /// Which member of the cycle gets named is up to the cycle detector, so this
    /// pins the diagnostic class only.
    mutually_recursive_type_alias_is_error: ("type A = B\ntype B = A\n", "Recursive type alias"),
}

ok_case! {
    /// The declared-return rule applies only to type definitions. Binding
    /// annotations also disable `permit_new`, so keying the rule on that flag
    /// wrongly rejects this.
    fn_type_without_return_in_binding_annotation_is_ok:
        ("x fn(Int) = fn(a Int) { a }\nprintln(x(1))\n"),
}

/// Runtime counterpart of the case above.
#[test]
fn fn_type_without_return_in_binding_annotation_runs() {
    common::run_outputs("f fn(Int) = fn(x Int) { x * 2 }\nprintln(f(3))\n", "6\n");
}

// A cycle among some aliases must not stop the acyclic ones from registering.
#[test]
fn recursive_type_alias_cycle_does_not_drop_other_aliases() {
    let src = "type Good = Int\ntype A = B\ntype B = A\nfn f(x Good) Good { x }\nprintln(f(1))\n";
    let out = check_rejects(src, "Recursive type alias");
    assert!(
        !out.combined().contains("Unknown type"),
        "cycle in A/B must not poison C"
    );
}

// A duplicate-named constructor is reported once and dropped, like a duplicate
// fn or const: the first registration survives.
#[test]
fn duplicate_constructor_is_dropped_not_double_defined() {
    let src = "type T {\n\tDup(a Int)\n\tDup\n}\nprintln(Dup(1))\n";
    let out = check_rejects(src, "'Dup' is already defined");
    let combined = out.combined();
    assert_eq!(
        combined.matches("'Dup' is already defined").count(),
        1,
        "expected exactly one duplicate diagnostic:\n{combined}"
    );
    assert!(
        !combined.contains("has 0 field(s)"),
        "duplicate ctor shadowed the first definition:\n{combined}"
    );
}

reject_case! {
    /// Constructor names share the value namespace with fns and consts.
    duplicate_constructor_across_types_is_error:
        ("type A { Red }\ntype B { Red }\n", "'Red' is already defined"),
    constructor_colliding_with_fn_is_error:
        ("fn Red() {}\ntype A { Red }\n", "'Red' is already defined"),

    /// Occurs-check: `x(x)` is an infinite type. Inference must reject, not loop.
    infinite_type_self_application_is_error: ("fn f(x) { x(x) }\n", "Infinite type detected"),

    // Field/tuple access diagnostics from `compile_binary` / `compile_field_access`.

    tuple_index_out_of_bounds_is_error: (
        "t = (1, 2)\nprintln(t.5)\n",
        "Tuple index 5 out of bounds (tuple has 2 elements)",
    ),
    numeric_index_on_non_tuple_is_error:
        ("x = 5\nprintln(x.0)\n", "Cannot index .0 on non-tuple type"),
    /// The runtime variant is not statically known, so a projected label must be
    /// present on every variant.
    field_not_on_every_variant_is_error: (
        "type Shape {\n\tCircle(r Int)\n\tSquare(side Int)\n}\nfn g(s Shape) Int { s.r }\n",
        "Field 'r' is not present on every variant of 'Shape' (missing on 'Square')",
    ),
    field_access_partial_is_rejected: (
        "type Named {\n\tPerson(name String, age Int)\n\tOrg(name String, size Int)\n}\n\
         fn age_of(n Named) Int { n.age }\n\
         println(age_of(Person(name: 'al', age: 18)))\n",
        "Field 'age' is not present on every variant of 'Named' (missing on 'Org')",
    ),
    field_access_on_tuple_is_error:
        ("t = (1, 2)\nt.x\n", "Type '(Int, Int)' has no field 'x'"),
    field_access_on_unknown_type_is_error: (
        "fn g(x) { x.name }\n",
        "Cannot access field 'name' on a value of unknown type — add a type annotation",
    ),

    /// The did-you-mean path in `compile_identifier`. The no-suggestion path is
    /// `unknown_identifier_is_error` above.
    unknown_identifier_suggests_close_name:
        ("println = 1\nfoo = prntln\n", "Did you mean 'println'?"),

    /// The tuple path. The constructor path emits a different message.
    refutable_tuple_destructuring_binding_is_error: (
        "(x, 1) = (1, 2)\nprintln(x)\n",
        "Destructuring binding pattern must be irrefutable",
    ),
}

ok_case! {
    // Control: the check above fires on refutability, not on tuple destructuring.
    irrefutable_tuple_destructuring_binding_is_ok: ("(x, y) = (1, 2)\nprintln(x + y)\n"),

    // A builtin in value position gets an eta wrapper (see `typed_ir::eta`).
    builtin_used_as_value_is_ok: ("f = println\nf('done')\n"),
}

reject_case! {
    /// Declarations are only legal at a module's top level.
    nested_type_declaration_is_error: (
        "fn outer() Int {\n\ttype Inner { A }\n\t1\n}\n",
        "type declarations are only allowed at the top level",
    ),
    /// `or` with a receiver binds the failure payload. `Result`'s `Err` carries
    /// one; `Option`'s `None` does not.
    or_with_receiver_on_option_is_error:
        ("x = Some(5) or v -> 0\nprintln(x)\n", "'or' on an Option does not bind a value"),
}

ok_case! {
    // Control: the diagnostic above fires on the receiver, not on `or` over `Option`.
    or_without_receiver_on_option_is_ok: ("x = Some(5) or 0\nprintln(x)\n"),
}

reject_case! {
    /// A local type sharing its name with a stdlib type is still a different
    /// type. Unifying on names instead of nominal ids is a soundness hole.
    same_named_type_from_another_module_does_not_unify: (
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
    ),
}

ok_case! {
    // Each `Parsed` still answers for its own constructors and exhaustiveness.
    same_named_local_and_stdlib_types_coexist: (
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
         \tDone(_, _, _, _, _, consumed) -> consumed\n\
         \tNeedMore -> 0 - 1\n\
         \tBad(s) -> s\n\
         }\n\
         println(remote + local_value(LocalDone(41)))\n",
    ),
}

/// Constructor arguments are typechecked in declared-field order whatever order
/// the labels are written in. The elaborator records types in source order, so
/// it is easy to let that order drag the checking order along.
#[test]
fn ctor_arg_diagnostics_come_out_in_declared_field_order() {
    let out = common::run_source(
        "check",
        "type P {\n\tx Int\n\ty Int\n}\np = P(y: 'why', x: 'ex')\n",
    );
    let text = out.combined();
    let x_at = text
        .find("5:20")
        .expect("expected a diagnostic on `x: 'ex'`");
    let y_at = text
        .find("5:10")
        .expect("expected a diagnostic on `y: 'why'`");
    assert!(
        x_at < y_at,
        "expected the `x` field's diagnostic before the `y` field's:\n{text}"
    );
}

/// A `..base` spread must unify the result type before any argument is checked,
/// so a function-literal argument still gets a concrete parameter type.
#[test]
fn ctor_spread_solves_type_params_before_lambda_args_are_hinted() {
    common::run_outputs(
        "type Pair(a) {\n\tfst a\n\tsnd fn(a) a\n}\n\
         p = Pair(1, fn(x) { x + 1 })\n\
         q = Pair(snd: fn(x) { x * 2 }, ..p)\n\
         println(q.snd(q.fst))\n",
        "2\n",
    );
}
