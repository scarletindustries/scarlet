mod common;
use common::check_rejects;

reject_case! {
    int_plus_string_is_type_error: ("pub fn main() {\n\tx = 1 + 'a'\n}\n", "Type mismatch"),
    non_exhaustive_match_is_error: (
        "pub fn main() {\n\tf = fn(x) { match x { True -> 1 } }\n\tprintln(f(False))\n}\n",
        "not exhaustive",
    ),
    unknown_identifier_is_error: ("pub fn main() {\n\tx = foo\n}\n", "Unknown identifier"),
    // A stdlib module's name used with no import still means that module, and
    // the error says which import it needs.
    an_unimported_stdlib_module_names_its_import: (
        "pub fn main() {\n\tprintln(int::abs(1))\n}\n",
        "Unknown module 'int': it is the name of a stdlib module that is not imported. Add `import scarlet/int`",
    ),
    // A module isn't a value. The error says so and shows how to use it.
    a_module_is_not_a_value: (
        "import scarlet/int\npub fn main() {\n\tprintln(int)\n}\n",
        "'int' is the module `scarlet/int`, not a value. Use one of its members, like `int::abs`",
    ),
    // Destructuring a constructor through its module can fail when its type has
    // other constructors, so we refuse it and point at `match`.
    a_qualified_ctor_binding_must_be_irrefutable: (
        "import scarlet/io\npub fn main() {\n\tio::NotFound(p) = io::PermissionDenied('x')\n\tprintln(p)\n}\n",
        "A binding's pattern must match every value, and this one misses",
    ),
    // When a constructor is used by its bare name and an imported module has
    // it, the error names the module to reach it through. This holds in a
    // pattern and in an expression.
    a_bare_constructor_in_a_pattern_names_its_module: (
        "import scarlet/io\npub fn main() {\n\tmatch io::read_file('x') {\n\t\tErr(NotFound(p)) -> println(p)\n\t\t_ -> println('other')\n\t}\n}\n",
        "Unknown constructor 'NotFound' in pattern. It is `io::NotFound`",
    ),
    a_bare_constructor_in_an_expression_names_its_module: (
        "import scarlet/io\npub fn main() {\n\te = NotFound('x')\n\tprintln(e)\n}\n",
        "Unknown identifier 'NotFound'. A constructor of that name is `io::NotFound`",
    ),
    if_condition_must_be_bool:
        ("pub fn main() {\n\tprintln(if 1 { 2 } else { 3 })\n}\n", "Type mismatch"),

    // The three places a numeric literal compiles: an expression, a match
    // pattern and a range bound.
    integer_literal_overflow_is_error:
        ("pub fn main() {\n\tx = 99999999999999999999999999\n}\n", "out of range for Int"),
    integer_literal_overflow_in_match_pattern_is_error: (
        "pub fn main() {\n\tprintln(match 1 {\n\t\t99999999999999999999999999 -> 0\n\t\t_ -> 1\n\t})\n}\n",
        "out of range for Int",
    ),
    integer_literal_overflow_in_range_pattern_is_error: (
        "pub fn main() {\n\tprintln(match 1 {\n\t\t99999999999999999999999999..0 -> 0\n\t\t_ -> 1\n\t})\n}\n",
        "out of range for Int",
    ),
    hex_literal_overflow_is_error: (
        "pub fn main() {\n\tx = 0x8000000000000000\n}\n",
        "out of range for Int",
    ),
    bin_literal_overflow_is_error: (
        "pub fn main() {\n\tx = 0b1000000000000000000000000000000000000000000000000000000000000000\n}\n",
        "out of range for Int",
    ),
    hex_and_decimal_are_the_same_match_arm: (
        "pub fn main() {\n\tprintln(match 255 {\n\t\t0xFF -> 1\n\t\t255 -> 2\n\t})\n}\n",
        "unreachable",
    ),

    unused_let_binding_is_error: (
        "pub fn main() {\n\tx = 1\n\tprintln('done')\n}\n",
        "'x' is never used. Start its name with '_' if that's on purpose",
    ),
    unused_param_is_error: (
        "fn f(a Int, b Int) Int { a }\npub fn main() {\n\tprintln(f(1, 2))\n}\n",
        "'b' is never used. Start its name with '_' if that's on purpose",
    ),
    unused_match_binding_is_error: (
        "pub fn main() {\n\tprintln(match Some(1) {\n\t\tSome(x) -> 0\n\t\tNone -> 0\n\t})\n}\n",
        "'x' is never used. Start its name with '_' if that's on purpose",
    ),
}

ok_case! {
    underscore_prefix_suppresses_unused_error:
        ("pub fn main() {\n\t_x = 1\n\tprintln('done')\n}\n"),

    used_let_binding_is_ok: ("pub fn main() {\n\tx = 1\n\tprintln(x)\n}\n"),

    closure_capture_counts_as_use:
        ("pub fn main() {\n\tx = 1\n\tf = fn() { x }\n\tprintln(f())\n}\n"),
}

reject_case! {
    // A program is declarations plus `pub fn main()`. These are the entry
    // rule's diagnostics as `check` reports them. `check` itself doesn't need a
    // `main` (see the library cases below), but `run` does.
    statement_at_module_scope_is_error: (
        "println('early')\n\npub fn main() {\n\tprintln('main')\n}\n",
        "Statements aren't allowed at module scope. Put a program's code inside `pub fn main()`",
    ),
    binding_at_module_scope_is_error: (
        "x = 1\n\npub fn main() {\n\tprintln(x)\n}\n",
        "Statements aren't allowed at module scope. Put a program's code inside `pub fn main()`",
    ),
    private_main_is_error: (
        "fn main() {\n\tprintln('hi')\n}\n",
        "`main` must be public, because a program starts at `pub fn main()`",
    ),
    main_with_parameters_is_error: (
        "pub fn main(args Array(String)) {\n\tprintln(args)\n}\n",
        "`main` takes no parameters. Read the program's arguments with `os.argv()` instead",
    ),
}

run_reject_case! {
    program_without_main_does_not_run: (
        "fn helper() Int { 1 }\npub fn exported() Int { helper() }\n",
        "There's no `main` function. A program starts at `pub fn main()`, so add one",
    ),
}

ok_case! {
    // A library file has no `main` and still checks.
    library_without_main_checks: ("fn helper() Int { 1 }\npub fn exported() Int { helper() }\n"),
}

reject_case! {
    // Mistakes at a call site, both reported by `match_fun_type`.
    wrong_argument_count_is_error: (
        "fn f(a Int, b Int) Int { a + b }\npub fn main() {\n\tprintln(f(1))\n}\n",
        "Expected 2 argument(s), got 1",
    ),
    calling_non_function_is_error: (
        "pub fn main() {\n\tx = 5\n\ty = x(3)\n\tprintln(y)\n}\n",
        "This value of type 'Int' is not a function and cannot be called",
    ),

    // Backpassing desugars to a trailing lambda added to the call before
    // inference, so a callee whose last param isn't a matching fn gives an
    // ordinary type error at the call site.
    backpass_into_non_function_param_is_error: (
        "fn g(a Int, b Int) Int { a + b }\nfn h() Int {\n\tx <- g(1)\n\tx + 0\n}\n\
         pub fn main() {\n\tprintln(h())\n}\n",
        "Type mismatch",
    ),
    backpass_overflows_callee_arity_is_error: (
        "fn g(a Int, b Int) Int { a + b }\nfn h() Int {\n\tx <- g(1, 2)\n\tx + 0\n}\n\
         pub fn main() {\n\tprintln(h())\n}\n",
        "Expected 2 argument(s), got 3",
    ),

    // Diagnostics for the arguments where a value is constructed, all from
    // `slot_ctor_args`.
    ctor_missing_field_is_error: (
        "type P { P(name String, age Int) }\npub fn main() {\n\tprintln(P(name: 'a'))\n}\n",
        "Constructor 'P' is missing field(s): age",
    ),
    ctor_unknown_field_is_error: (
        "type P { P(name String, age Int) }\npub fn main() {\n\tprintln(P(name: 'a', bogus: 1))\n}\n",
        "Constructor 'P' has no field 'bogus'. Available: name, age",
    ),
    ctor_too_many_positional_is_error: (
        "type P { P(name String, age Int) }\npub fn main() {\n\tprintln(P('a', 'b', 'c'))\n}\n",
        "Constructor 'P' has 2 field(s) but more were supplied",
    ),
    ctor_nullary_with_args_is_error: (
        "type C {\n\tRed\n\tGreen\n}\npub fn main() {\n\tprintln(Red(1))\n}\n",
        "Constructor 'Red' has 0 field(s) but more were supplied",
    ),
    ctor_duplicate_field_is_error: (
        "type P { P(name String, age Int) }\n\
         pub fn main() {\n\tprintln(P(name: 'a', name: 'b', age: 1))\n}\n",
        "Field 'name' is specified more than once",
    ),

    /// `type_ctor_pattern` is a separate path from the construction checks
    /// above. Only a name cased like a constructor reaches it, because the
    /// parser sends a lowercase callee to a different pattern form.
    ctor_unknown_in_pattern_is_error: (
        "pub fn main() {\n\tr = match Some(1) {\n\t\tBogus(x) -> 0\n\t\t_ -> 1\n\t}\n\tprintln(r)\n}\n",
        "Unknown constructor 'Bogus' in pattern",
    ),
    ctor_nullary_with_args_in_pattern_is_error: (
        "type C {\n\tRed\n\tGreen\n}\nfn f(c C) Int { match c { Red(x) -> x\n Green -> 0 } }\n\
         pub fn main() {\n\tprintln(f(Red))\n}\n",
        "Constructor 'Red' takes no arguments but 1 were given",
    ),

    /// Inside a `{ }` block every non-last statement must be `Nil` or consumed.
    unconsumed_block_expr_statement_is_error: (
        "pub fn main() {\n\tr = {\n\t\t1 + 2\n\t\t9\n\t}\n\tprintln(r)\n}\n",
        "isn't used. Assign it to a name",
    ),
}

ok_case! {
    // The control for the rule above. It checks the `is_nil` guard doesn't fire
    // too often.
    nil_typed_block_expr_statement_is_ok:
        ("pub fn main() {\n\tr = {\n\t\tprintln(1)\n\t\t9\n\t}\n\tprintln(r)\n}\n"),
}

reject_case! {
    // Diagnostics from the `Hydrator`, which turns a written type into a `Ty`.

    type_arg_arity_mismatch_is_error: (
        "fn f(_x Option(Int, String)) Int { 1 }\n",
        "Type 'Option' expects 1 type argument, got 2",
    ),
    /// A field's right-hand side hydrates with `permit_new` off, so a var we
    /// haven't seen is an error rather than an implicit fresh one.
    unknown_type_variable_in_ctor_field_is_error:
        ("type Box { Box(v t) }\n", "Unknown type variable 't'"),
    type_variable_cannot_take_arguments_is_error:
        ("fn f(x a(Int)) a { x }\n", "Type variable 'a' cannot take arguments"),
    /// Without a return type there's no inference context to solve, so `fn(Int)`
    /// would mint an unconstrained var that type-checks against anything.
    fn_type_without_return_in_ctor_field_is_error: (
        "type F { F(g fn(Int)) }\n",
        "A function type in a type definition must declare its return type, like `fn(Int) Int`",
    ),
    /// The same rule on an alias's right-hand side, which also hydrates with
    /// `permit_new` off.
    fn_type_without_return_in_alias_is_error: (
        "type Callback = fn(Int)\n",
        "A function type in a type definition must declare its return type, like `fn(Int) Int`",
    ),

    duplicate_type_parameter_is_error:
        ("type Box(t, t) { Box(a t) }\n", "Duplicate type parameter 't'"),
    recursive_type_alias_self_is_error: ("type A = A\n", "Recursive type alias 'A'"),
    /// The cycle detector picks which member of the cycle to name, so this only
    /// pins the kind of diagnostic.
    mutually_recursive_type_alias_is_error: ("type A = B\ntype B = A\n", "Recursive type alias"),
}

ok_case! {
    /// The rule that a return type must be declared only applies to type
    /// definitions. Binding annotations also turn off `permit_new`, so keying
    /// the rule on that flag would wrongly reject this.
    fn_type_without_return_in_binding_annotation_is_ok:
        ("pub fn main() {\n\tx fn(Int) = fn(a Int) { a }\n\tprintln(x(1))\n}\n"),
}

/// The runtime version of the case above.
#[test]
fn fn_type_without_return_in_binding_annotation_runs() {
    common::run_outputs(
        "pub fn main() {\n\tf fn(Int) = fn(x Int) { x * 2 }\n\tprintln(f(3))\n}\n",
        "6\n",
    );
}

// A cycle among some aliases must not stop the acyclic ones from registering.
#[test]
fn recursive_type_alias_cycle_does_not_drop_other_aliases() {
    let src = "type Good = Int\ntype A = B\ntype B = A\nfn f(x Good) Good { x }\n\
               pub fn main() {\n\tprintln(f(1))\n}\n";
    let out = check_rejects(src, "Recursive type alias");
    assert!(
        !out.combined().contains("Unknown type"),
        "the A/B cycle must not poison `Good`"
    );
}

// A constructor with a duplicate name is reported once and dropped, like a
// duplicate fn or const, and the first one registered survives.
#[test]
fn duplicate_constructor_is_dropped_not_double_defined() {
    let src = "type T {\n\tDup(a Int)\n\tDup\n}\npub fn main() {\n\tprintln(Dup(1))\n}\n";
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

    /// The occurs check: `x(x)` is an infinite type. Inference must reject it,
    /// not loop for ever.
    infinite_type_self_application_is_error: ("fn f(x) { x(x) }\n", "Infinite type detected"),

    // Diagnostics for field and tuple access, from `compile_binary` and
    // `compile_field_access`.

    tuple_index_out_of_bounds_is_error: (
        "pub fn main() {\n\tt = (1, 2)\n\tprintln(t.5)\n}\n",
        "Tuple index 5 is out of range, because the tuple has 2 elements",
    ),
    numeric_index_on_non_tuple_is_error: (
        "pub fn main() {\n\tx = 5\n\tprintln(x.0)\n}\n",
        "Can't use .0 here, because the value isn't a tuple",
    ),
    /// We don't know the runtime variant statically, so a projected label must
    /// be on every variant.
    field_not_on_every_variant_is_error: (
        "type Shape {\n\tCircle(r Int)\n\tSquare(side Int)\n}\nfn g(s Shape) Int { s.r }\n",
        "Field 'r' is not present on every variant of 'Shape' (missing on 'Square')",
    ),
    field_access_partial_is_rejected: (
        "type Named {\n\tPerson(name String, age Int)\n\tOrg(name String, size Int)\n}\n\
         fn age_of(n Named) Int { n.age }\n\
         pub fn main() {\n\tprintln(age_of(Person(name: 'al', age: 18)))\n}\n",
        "Field 'age' is not present on every variant of 'Named' (missing on 'Org')",
    ),
    field_access_on_tuple_is_error: (
        "pub fn main() {\n\tt = (1, 2)\n\tprintln(t.x)\n}\n",
        "Type '(Int, Int)' has no field 'x'",
    ),
    field_access_on_unknown_type_is_error: (
        "fn g(x) { x.name }\n",
        "Can't access field 'name' because the value's type isn't known here. Add a type annotation",
    ),

    /// The did-you-mean path in `compile_identifier`. The no-suggestion path is
    /// `unknown_identifier_is_error` above.
    unknown_identifier_suggests_close_name: (
        "pub fn main() {\n\tprintln = 1\n\tfoo = prntln\n}\n",
        "Did you mean 'println'?",
    ),

    /// This tests the tuple path. The constructor path emits a different
    /// message.
    refutable_tuple_destructuring_binding_is_error: (
        "pub fn main() {\n\t(x, 1) = (1, 2)\n\tprintln(x)\n}\n",
        "A binding's pattern must match every value, and this one misses",
    ),
}

ok_case! {
    // The control. The check above fires on refutability, not on tuple
    // destructuring.
    irrefutable_tuple_destructuring_binding_is_ok:
        ("pub fn main() {\n\t(x, y) = (1, 2)\n\tprintln(x + y)\n}\n"),

    // A builtin in value position gets an eta wrapper (see `typed_ir::eta`).
    builtin_used_as_value_is_ok: ("pub fn main() {\n\tf = println\n\tf('done')\n}\n"),
}

reject_case! {
    /// Declarations are only allowed at a module's top level.
    nested_type_declaration_is_error: (
        "fn outer() Int {\n\ttype Inner { A }\n\t1\n}\n",
        "type declarations are only allowed at the top level",
    ),
    /// `or` with a receiver binds the failure payload. `Result`'s `Err` carries
    /// one; `Option`'s `None` does not.
    or_with_receiver_on_option_is_error: (
        "pub fn main() {\n\tx = Some(5) or v -> 0\n\tprintln(x)\n}\n",
        "'or' on an Option can't bind a value",
    ),
}

ok_case! {
    // The control. The diagnostic above fires on the receiver, not on `or` over
    // `Option`.
    or_without_receiver_on_option_is_ok:
        ("pub fn main() {\n\tx = Some(5) or 0\n\tprintln(x)\n}\n"),
}

reject_case! {
    /// A local type sharing its name with a stdlib type is still a different
    /// type. Unifying on names instead of nominal ids is a soundness hole.
    same_named_type_from_another_module_does_not_unify: (
        "import scarlet/binary\n\
         import scarlet/http/h1\n\
         type Parsed {\n\
         \tLocalDone(x Int)\n\
         \tLocalOther\n\
         }\n\
         fn f() Parsed {\n\
         \th1::parse_request(binary::from_string('GET / HTTP/1.1\\r\\n\\r\\n'), 0)\n\
         }\n\
         pub fn main() {\n\
         \tprintln(f())\n\
         }\n",
        "Type mismatch",
    ),
}

ok_case! {
    // Each `Parsed` still answers for its own constructors and exhaustiveness.
    same_named_local_and_stdlib_types_coexist: (
        "import scarlet/binary\n\
         import scarlet/http/h1\n\
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
         pub fn main() {\n\
         \tremote = match h1::parse_request(binary::from_string('GET / HTTP/1.1\\r\\n\\r\\n'), 0) {\n\
         \t\th1::Done(_, _, _, _, _, consumed) -> consumed\n\
         \t\th1::NeedMore -> 0 - 1\n\
         \t\th1::Bad(s) -> s\n\
         \t}\n\
         \tprintln(remote + local_value(LocalDone(41)))\n\
         }\n",
    ),
}

/// Constructor arguments are type-checked in declared field order, whatever
/// order the labels are written in. The elaborator records types in source
/// order, so it's easy to let that order drag the checking order along.
#[test]
fn ctor_arg_diagnostics_come_out_in_declared_field_order() {
    let out = common::run_source(
        "check",
        "type P {\n\tx Int\n\ty Int\n}\npub fn main() {\n\tprintln(P(y: 'why', x: 'ex'))\n}\n",
    );
    let text = out.combined();
    let x_at = text
        .find("6:25")
        .expect("expected a diagnostic on `x: 'ex'`");
    let y_at = text
        .find("6:15")
        .expect("expected a diagnostic on `y: 'why'`");
    assert!(
        x_at < y_at,
        "expected the `x` field's diagnostic before the `y` field's:\n{text}"
    );
}

/// A `..base` spread must unify the result type before we check any argument,
/// so a function literal passed as an argument still gets a concrete
/// parameter type.
#[test]
fn ctor_spread_solves_type_params_before_lambda_args_are_hinted() {
    common::run_outputs(
        "type Pair(a) {\n\tfst a\n\tsnd fn(a) a\n}\n\
         pub fn main() {\n\
         \tp = Pair(1, fn(x) { x + 1 })\n\
         \tq = Pair(snd: fn(x) { x * 2 }, ..p)\n\
         \tprintln(q.snd(q.fst))\n\
         }\n",
        "2\n",
    );
}

// A top-level value may share its name with an imported module. `::` reaches
// the module, and a bare name or `.` reaches the value, so neither hides the
// other.
ok_case! {
    a_fn_named_like_an_import_does_not_hide_it: (
        "import scarlet/http\n\npub fn http() {\n    http::Unsized\n}\n",
    ),
    qualified_member_access_works_beside_a_plain_name: (
        "import scarlet/http\n\npub fn serve() {\n    http::Unsized\n}\n",
    ),
}

// Qualified type names. `module::Type` resolves through the mangled
// `qualifier::Name` keys that `process_import` registers for every public type
// of an imported module.
ok_case! {
    qualified_type_in_alias_position: (
        "import scarlet/http\n\ntype C = http::Method\n\npub fn new() {\n    2\n}\n",
    ),
    qualified_type_in_annotation_position: (
        "import scarlet/http\n\npub fn f(m http::Method) http::Method {\n    m\n}\n",
    ),
    qualified_type_through_import_alias: (
        "import scarlet/http as h\n\ntype C = h::Method\n\npub fn new() {\n    2\n}\n",
    ),
}

reject_case! {
    unknown_qualified_type_names_module_and_member: (
        "import scarlet/http\n\ntype C = http::Nope\n",
        "Unknown type 'http::Nope'. Check that 'http' is imported and exports a type called 'Nope'.",
    ),
    qualified_type_arity_error_uses_qualified_name: (
        "import scarlet/http\n\ntype C = http::Method(Int)\n",
        "Type 'http::Method' expects 0 type arguments, got 1",
    ),
    unimported_qualifier_in_type_position_is_unknown: (
        "type C = http::Method\n",
        "Unknown type 'http::Method'",
    ),
}

// Two `scarlet/json/decode` mistakes the type system rules out. We pin them
// here rather than in `tests/programs/json.sca` because neither program gets
// far enough to print anything, and the golden run can only test decoders that
// compile.
reject_case! {
    /// `Failure.found` is a `Found`, not free text, so a decoder can't put a
    /// sentence where the document's shape belongs.
    decode_failure_found_rejects_free_text: (
        "import scarlet/json/decode\n\ndecode::Failure([], 'a thing', 'a string')\n",
        "expected 'Found', got 'String'",
    ),
    /// `one_of` takes its first alternative separately, so you can't build a
    /// union with nothing to try.
    decode_one_of_rejects_no_alternatives: (
        "import scarlet/json/decode\n\ndecode::one_of([])\n",
        "Expected 2 argument(s), got 1",
    ),
}

// A catch-all may only stand for values that can't be listed. Once a match
// names some variants of a value, it names them all, so a variant added later
// can't fall silently into an arm written before it existed.
reject_case! {
    a_wildcard_may_not_stand_for_named_variants: (
        "type Color {\n\tRed\n\tGreen\n\tBlue\n}\npub fn main() {\n\tprintln(match Red {\n\t\tRed -> 1\n\t\t_ -> 2\n\t})\n}\n",
        "This pattern stands for Green, Blue. Name them instead, like `Green | Blue ->`",
    ),
    /// A bare name is the same hole as `_`, not a different one.
    a_bare_name_may_not_stand_for_named_variants: (
        "type Color {\n\tRed\n\tGreen\n\tBlue\n}\npub fn main() {\n\tprintln(match Red {\n\t\tRed -> 1\n\t\tc -> show(c)\n\t})\n}\nfn show(c Color) Int { 2 }\n",
        "This pattern stands for Green, Blue",
    ),
    /// A guarded arm still names its variant.
    a_guarded_arm_still_names_its_variant: (
        "pub fn main() {\n\tprintln(match Some(1) {\n\t\tSome(n) if n > 0 -> n\n\t\t_ -> 0\n\t})\n}\n",
        "This pattern stands for None",
    ),
    /// Inside a tuple, the rule applies to each column the arms split.
    a_wildcard_in_a_tuple_column_is_held_to_it_too: (
        "pub fn main() {\n\tprintln(match (True, 1) {\n\t\t(True, n) -> n\n\t\t(_, _) -> 0\n\t})\n}\n",
        "This pattern stands for False",
    ),
    /// A wildcard that covers nothing left is dead code, and is reported as
    /// that.
    a_wildcard_covering_nothing_is_unreachable: (
        "type Color {\n\tRed\n\tGreen\n}\npub fn main() {\n\tprintln(match Red {\n\t\tRed -> 0\n\t\tGreen -> 1\n\t\t_ -> 2\n\t})\n}\n",
        "unreachable",
    ),
    /// `@exhaustive` is gone, because the rule it opted into now applies to
    /// every match.
    the_exhaustive_attribute_is_unknown: (
        "@exhaustive\ntype Color {\n\tRed\n\tGreen\n\tBlue\n}\n",
        "Unknown attribute '@exhaustive'",
    ),
}

ok_case! {
    /// A wildcard over a whole value nobody split is "I don't care".
    a_wildcard_over_an_unsplit_value_is_fine: (
        "pub fn main() {\n\tprintln(match (Some(1), 2) {\n\t\t(Some(_), n) -> n\n\t\t(None, _) -> 0\n\t})\n}\n",
    ),
    /// Numbers, strings and binaries can't be listed, so `_` stays.
    a_wildcard_over_values_that_cannot_be_listed_is_fine: (
        "pub fn main() {\n\tprintln(match 3 {\n\t\t1 -> 'one'\n\t\t_ -> 'many'\n\t})\n}\n",
    ),
    /// An or-pattern inside a constructor names its variants in place.
    an_or_pattern_inside_a_constructor_names_them: (
        "type Color {\n\tRed\n\tGreen\n\tBlue\n}\npub fn main() {\n\tprintln(match Some(Red) {\n\t\tSome(Red) -> 1\n\t\tSome(Green | Blue) -> 2\n\t\tNone -> 3\n\t})\n}\n",
    ),
}

// ===========================================================================
// scarlet/wire refusals (T-342). The refusals *are* the feature. `wire.encode`
// never fails precisely because the compiler rejects a type it couldn't
// rebuild, so each of these is a guarantee rather than an edge case.
//
// Every case here goes through `al check`, which never emits. That's exactly
// why elaboration raises the refusal. A refusal raised at emission would leave
// every one of these green while `al run` rejected the same file.
// ===========================================================================

/// Asserts that `al check` rejects `src` with `needle`, and that the wire
/// refusal is the *only* error. Returns the combined output so a caller can
/// assert more.
///
/// The single-error assertion matters, and not for the reason it seems to.
/// Elaboration only runs on a module the check walk left clean, so an
/// unrelated mistake in a fixture doesn't add a second diagnostic beside the
/// wire one. It **deletes** the wire one. We saw this while writing these: a
/// fixture calling a `scarlet/process` member that doesn't exist reported that
/// twice and the wire refusal not at all. Pinning the count tells "the fixture
/// is wrong" from "the compiler is wrong" without reading the output by hand.
fn wire_rejects(src: &str, needle: &str) -> String {
    let out = check_rejects(src, needle);
    let all = out.combined();
    assert!(
        all.contains("Found 1 error"),
        "the wire refusal must be the only error; got:\n{all}"
    );
    all
}

const FN_FIELD: &str = "import scarlet/wire\n\
                        pub type Handler {\n\
                        \tHandler(name String, run fn(Int) Int)\n\
                        }\n\
                        pub fn main() {\n\
                        \tprintln(wire::encode(Handler('h', fn(x) { x + 1 })))\n\
                        }\n";

ok_case! {
    /// A record with a `fn` field checks clean. A closure's type doesn't fix its
    /// captures, so each one is described inline at encode time rather than
    /// refused. This is the control that shows no refusal fires on a `fn`
    /// field. `wire_closures.rs` tests its round trip, calling through the
    /// decoded copy.
    a_fn_field_is_admitted_since_closures_cross: (FN_FIELD),
}

/// A type we can't encode sits three levels down, and the assertion is on the
/// *path*.
///
/// This is the case the path code exists for. A refusal that only says
/// "cannot encode" leaves the reader to find which of nine positions it meant.
/// We assert the full chain, not just that a refusal happened.
///
/// The unknown type is the only refusal left, so this test uses a generic
/// function encoding its parameter. It also uses a positional shape, because
/// we walk a `Data` node's type arguments before its fields. `Outer(a)` refuses
/// at the argument with no path, and a field can only hold what a type was
/// applied to.
#[test]
#[ignore = "waits for the wire redesign: check stopped refusing wire types in #50"]
fn wire_names_the_whole_path_down_to_the_refusing_type() {
    let all = wire_rejects(
        "import scarlet/map\n\
         import scarlet/wire\n\
         fn send(o (Int, map::Map(String, Array(a)))) Binary {\n\
         \twire::encode(o)\n\
         }\n\
         pub fn main() {\n\
         \t_ = send\n\
         }\n",
        "[1] -> [value] -> [element]",
    );
    assert!(
        all.contains("cannot encode `a`"),
        "the refusal must name the sub-type at fault, not the type the call named:\n{all}"
    );
    assert!(
        all.contains("the type is still polymorphic here"),
        "the reason must be given:\n{all}"
    );
    assert!(
        !all.contains("no representation"),
        "the corrected rationale must not regress:\n{all}"
    );
}

const TAGGED_NATIVE: &str = "import scarlet/wire\n\
                             pub type Native\n\
                             type Tagged(t) {\n\
                             \tTagged(value Int)\n\
                             }\n\
                             fn tag(n Int) Tagged(Native) {\n\
                             \tTagged(n)\n\
                             }\n\
                             pub fn main() {\n\
                             \tprintln(wire::encode(tag(7)))\n\
                             }\n";

ok_case! {
    /// `Tagged(Native)` over an `Int` field checks clean. No value reaches the
    /// phantom argument, and the bytes are the `Int`'s. `wire_uninhabited.rs`
    /// tests its round trip.
    a_phantom_of_a_bodiless_type_is_admitted: (TAGGED_NATIVE),
}

/// `decode` with nothing to fix its payload, and `decode` inside a generic
/// function, are two different inputs. One is an unsolved variable and the
/// other a rigid quantified one, and the descriptor builder deliberately treats
/// them the same. `zonk` says why: both mean the representation is unknown. So
/// we pin both to the same wording, and having the pair shows the second case
/// isn't reaching the first one's code path by accident.
mod unknown_payload {
    use super::wire_rejects;

    const WANTED: &str = "the type `wire::decode` produces here is not known; annotate the binding";

    #[test]
    #[ignore = "waits for the wire redesign: check stopped refusing wire types in #50"]
    fn an_unconstrained_decode_is_refused() {
        wire_rejects(
            "import scarlet/wire\n\
             fn read(b Binary) {\n\
             \tmatch wire::decode(b) {\n\
             \t\tOk(v) -> Some(v)\n\
             \t\tErr(_) -> None\n\
             \t}\n\
             }\n\
             pub fn main() {\n\
             \tprintln(match read(<<1>>) {\n\
             \t\tSome(_) -> 'ok'\n\
             \t\tNone -> 'no'\n\
             \t})\n\
             }\n",
            WANTED,
        );
    }

    /// The call site pins `a` to `Int`, and it's still refused, because the body
    /// is elaborated at the generalised signature, where `a` is rigid. That's
    /// how it differs from the case above, and you can't see it in the text.
    #[test]
    #[ignore = "waits for the wire redesign: check stopped refusing wire types in #50"]
    fn a_decode_in_a_generic_fn_is_refused() {
        wire_rejects(
            "import scarlet/wire\n\
             fn read(b Binary, fallback a) a {\n\
             \tmatch wire::decode(b) {\n\
             \t\tOk(v) -> v\n\
             \t\tErr(_) -> fallback\n\
             \t}\n\
             }\n\
             pub fn main() {\n\
             \tprintln(read(<<1>>, 0))\n\
             }\n",
            WANTED,
        );
    }
}

/// An opaque type from another module crosses the wire in *both* directions.
///
/// A decoder rebuilds a value by constructor, and it's up to a module to
/// re-check its invariants on the values it's handed. Visibility isn't
/// consulted. The symmetry is the point. Encoding accepts exactly what
/// decoding accepts, so we run both halves rather than one. `Decimal`'s fields
/// are two `Int`s and `scarlet/decimal` never exports its constructor, so only
/// a visibility rule could refuse these programs. These go through `al run`,
/// not `al check`. The descriptor accepting is one half, and the decoder
/// finding a template for a constructor this module can't name is the other.
mod opaque_from_another_module {
    use crate::common::run_outputs;

    /// The value that comes back is the one that went in. Both structural `==`
    /// (which compares `units` and `scale`) and the declaring module's own
    /// reader agree.
    #[test]
    #[ignore = "needs the VM"]
    fn round_trips_with_equality() {
        run_outputs(
            "import scarlet/decimal\n\
             import scarlet/wire\n\
             fn report(d decimal::Decimal, back decimal::Decimal) {\n\
             \tprintln(back == d)\n\
             \tprintln(decimal::to_string(back))\n\
             }\n\
             pub fn main() {\n\
             \td = decimal::new(1999, 2)\n\
             \tmatch wire::decode(wire::encode(d)) {\n\
             \t\tOk(back) -> report(d, back)\n\
             \t\tErr(_) -> println('err')\n\
             \t}\n\
             }\n",
            "True\n19.99\n",
        );
    }

    /// The decode half alone, with the payload type fixed only by the
    /// declaring module's reader.
    #[test]
    #[ignore = "needs the VM"]
    fn decode_is_typed_by_the_declaring_modules_reader() {
        run_outputs(
            "import scarlet/decimal\n\
             import scarlet/wire\n\
             pub fn main() {\n\
             \tbytes = wire::encode(decimal::new(1, 2))\n\
             \tprintln(match wire::decode(bytes) {\n\
             \t\tOk(d) -> decimal::units(d) * 10 + decimal::scale(d)\n\
             \t\tErr(_) -> 0 - 1\n\
             \t})\n\
             }\n",
            "12\n",
        );
    }
}

ok_case! {
    /// The other half of the opaque pair, from inside the module. A module
    /// encodes its *own* opaque type the way every type is encoded, by its
    /// constructors.
    a_module_may_encode_its_own_opaque_type: (
        "import scarlet/binary\nimport scarlet/wire\npub opaque type Token {\n\tToken(id Int)\n}\npub fn main() {\n\tprintln(binary::byte_size(wire::encode(Token(1))))\n}\n"
    ),
}

/// `Map` lives in `scarlet/map`, and the import names it `map::Map`. The bare
/// name resolves to nothing, and the error says which spelling does.
#[test]
fn a_bare_type_from_an_imported_module_points_at_its_qualified_name() {
    check_rejects(
        "import scarlet/map\n\
         fn f(_m Map(String, Int)) Int { 0 }\n\
         pub fn main() {\n\
         \tprintln(f(map::new()))\n\
         }\n",
        "Unknown type 'Map'. Did you mean 'map::Map'?",
    );
    check_rejects(
        "import scarlet/map as m\n\
         fn f(_m Map(String, Int)) Int { 0 }\n\
         pub fn main() {\n\
         \tprintln(f(m::new()))\n\
         }\n",
        "Did you mean 'm::Map'?",
    );
}

/// A stdlib type isn't in scope until you import its module. The old
/// pre-built stdlib put every stdlib type name in every program.
#[test]
fn a_stdlib_type_is_unknown_without_its_import() {
    let out = check_rejects(
        "fn f(_m Map(String, Int)) Int { 0 }\n\
         pub fn main() {\n\
         \tprintln(f)\n\
         }\n",
        "Unknown type 'Map'",
    );
    assert!(
        !out.combined().contains("Did you mean"),
        "nothing imported exports a `Map`:\n{}",
        out.combined()
    );
}
