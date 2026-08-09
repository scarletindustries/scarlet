mod common;
use common::{check_rejects, run_outputs, run_rejects};

reject_case! {
    /// U1: `x = x` self-reference infers ⊥ and generalizes to ∀A.A. Pre-define
    /// is gated to lambda inits, so the init `x` is unbound.
    u1_self_reference_is_rejected: (
        "x = x\n\
         n Int = x\n\
         println(n + 1)\n",
        "Unknown identifier 'x'",
    ),

    /// U2: `if` with no else types as Nil but evaluates to `e`, smuggling a
    /// String into an Int through `Option(T)` widening.
    u2_if_no_else_is_rejected: (
        "fn smuggle() Option(Int) { if True { 'hello' } }\n\
         n = smuggle() or 0\n\
         println(n + 1)\n",
        "'if' requires an 'else' branch",
    ),

    /// U5: a generic type used without type args fills fresh vars per use,
    /// letting a String flow into an Int position.
    u5_generic_type_missing_args_is_rejected: (
        "type Box(t) { Box(value t) }\n\
         type Holder { Holder(box Box) }\n\
         h = Holder(box: Box(value: 'hello'))\n\
         println(h.box.value + 100)\n",
        "Type 'Box' expects 1 type argument",
    ),

    /// U7: or-pattern alternatives binding disjoint names let the body read an
    /// uninitialized local when the non-binding alternative matches.
    u7_or_pattern_disjoint_bindings_is_rejected: (
        "fn f(x Int) Int {\n\
         \tmatch x {\n\
         \t\t1 | y -> y + 100\n\
         \t\t_ -> 0\n\
         \t}\n\
         }\n\
         println(f(1))\n",
        "not bound in the first alternative",
    ),

    /// U8: range pattern bounds not unified with Int let `true..'z'` typecheck
    /// and crash the VM comparator.
    u8_range_pattern_bounds_must_be_int: (
        "r = match 5 {\n\
         \ttrue..'z' -> 'huh'\n\
         \t_ -> 'ok'\n\
         }\n\
         println(r)\n",
        "Range pattern bounds must be number literals",
    ),

    /// U9: a spread that is not the final element of an array pattern corrupts
    /// the stack at runtime.
    u9_array_pattern_non_tail_spread_is_rejected: (
        "xs = [1, 2, 3]\n\
         r = match xs {\n\
         \t[a, ..mid, z] -> a + z\n\
         \t_ -> 0\n\
         }\n\
         println(r)\n",
        "Spread in array pattern must be the last element",
    ),

    /// U10: dropping every prefix element after the first in `[a, b, ..rest]`
    /// exhaustiveness lets an Int-returning fn fall through to None.
    u10_spread_prefix_exhaustiveness_is_rejected: (
        "fn f(xs Array(Bool)) Int {\n\
         \tmatch xs {\n\
         \t\t[True, _, ..rest] -> 1\n\
         \t\t[False, _, ..rest] -> 2\n\
         \t\t[] -> 3\n\
         \t}\n\
         }\n\
         println(f([True]))\n",
        "not exhaustive",
    ),

    /// U12: a duplicate binding name in one pattern silently shadows, so
    /// `(x, x)` binds the second element only.
    u12_duplicate_pattern_binding_is_rejected: (
        "r = match (1, 2) {\n\
         \t(x, x) -> x\n\
         }\n\
         println(r)\n",
        "bound more than once in this pattern",
    ),

    /// U13: `Socket` is an opaque builtin with no constructors, so referencing
    /// it as a value is an unknown identifier.
    u13_socket_literal_is_rejected: (
        "s = Socket\n\
         println(s)\n",
        "Unknown identifier 'Socket'",
    ),

    /// U16: two functions with the same name must be a clean "already defined"
    /// rejection, never a compiler panic from the name-keyed hydrator map.
    u16_duplicate_fn_is_rejected_without_panic:
        ("fn a(x) {x}\nfn a() {}\n", "already defined"),

    /// U17: a bare `@vm` with no arg must be a clean parse error, never a
    /// panic from a body-less fn surviving in `decls`.
    u17_bare_vm_attr_is_rejected_without_panic:
        ("@vm\nfn foo() Int\n", "@vm takes exactly one argument"),

    /// U18: the type-decl twin of U16. Duplicate `type` declarations desync the
    /// name-keyed Pass-1 maps against the positional `type_decls` Vec.
    u18_duplicate_type_is_rejected_without_panic: (
        "type T = Int\ntype T = String\nx = 1\nprintln(x)\n",
        "already defined",
    ),
}

run_case! {
    // U3: `Err(x)` is an ordinary constructor, so a fn returning
    // `Result(Int, E)` may return it and `or` observes the error.
    u3_err_constructor_typechecks: (
        "type E { E(msg String) }\n\
         fn f() Result(Int, E) {\n\
         \tErr(E(msg: 'boom'))\n\
         }\n\
         r = f() or _e -> 99\n\
         println(r)\n",
        "99\n",
    ),

    // U4: a block-scoped type env with a function-scoped slot map lets an inner
    // `x = 'hi'` overwrite the outer slot while the outer type stays Int.
    u4_block_scope_preserves_outer_slot: (
        "x = 1\n\
         r = {\n\
         \tx = 'hi'\n\
         \tx\n\
         }\n\
         println(r)\n\
         println(x + 100)\n",
        "hi\n101\n",
    ),

    // U6: a bare variant name over an unannotated subject must not compile to a
    // wildcard binding, which would send every value into the first arm.
    u6_bare_variant_on_inferred_subject_dispatches: (
        "type E {\n\tA\n\tB\n}\n\
         fn f(e) {\n\
         \tmatch e {\n\
         \t\tA -> 1\n\
         \t\tB -> 2\n\
         \t}\n\
         }\n\
         println(f(B))\n",
        "2\n",
    ),

    // U14: payload types must be substituted before exhaustiveness, or a fully
    // exhaustive match over `Maybe(Bool)` is wrongly rejected.
    u14_generic_enum_exhaustiveness_substitutes_payload: (
        "type Maybe(t) {\n\tJust(value t)\n\tNothing\n}\n\
         fn f(m Maybe(Bool)) Int {\n\
         \tmatch m {\n\
         \t\tJust(True) -> 1\n\
         \t\tJust(False) -> 2\n\
         \t\tNothing -> 3\n\
         \t}\n\
         }\n\
         println(f(Just(True)))\n\
         println(f(Just(False)))\n\
         println(f(Nothing))\n",
        "1\n2\n3\n",
    ),

    // U15: `.` after a digit is only part of the number when a digit follows,
    // or `t.0.name` lexes as `t` `.` `0.` `name`.
    u15_tuple_index_then_field_access: (
        "type P { P(name String) }\n\
         t = (P(name: 'hi'), 5)\n\
         println(t.0.name)\n",
        "hi\n",
    ),
}

// U19: a long `else if` ladder recursed `parse_if_expression` at constant
// guard depth and overflowed the native stack. It must be a clean "nesting
// too deep" rejection, never an uncatchable SIGSEGV.
#[test]
fn u19_deep_else_if_chain_is_rejected_without_overflow() {
    let mut source = String::from("x = if 1 < 2 { 0 }");
    for _ in 0..60_000 {
        source.push_str(" else if 1 < 2 { 0 }");
    }
    source.push_str(" else { 0 }\nprintln(x)\n");
    check_rejects(&source, "too deep");
}

// U20: arithmetic is TOTAL. `x/0 = 0`, `x%0 = x`, overflow wraps, non-finite
// float results collapse to 0.0. No panic, no abort, no non-zero exit.
#[test]
fn u20_arithmetic_is_total_vm_never_exits() {
    let exact = [
        ("println(1 / 0)\n", "0\n"),
        ("println(5 % 0)\n", "5\n"),
        ("println(1.0 / 0.0)\n", "0.0\n"),
        ("println(0.0 / 0.0)\n", "0.0\n"),
        // Float `%` is total like int `%`: the remainder takes the sign of the
        // dividend, and `x % 0.0 = x`.
        ("println(7.5 % 2.0)\n", "1.5\n"),
        ("println(7.5 % 0.0)\n", "7.5\n"),
        ("println({0.0 - 7.5} % 2.0)\n", "-1.5\n"),
        // Integer overflow wraps two's-complement, never traps.
        (
            "println(9223372036854775807 + 1)\n",
            "-9223372036854775808\n",
        ),
        ("println(9223372036854775807 * 2)\n", "-2\n"),
        // Signed division truncates toward zero; the remainder takes the sign
        // of the dividend. Grouping is `{expr}`, so `{0 - 7}` is the dividend.
        ("println({0 - 7} / 2)\n", "-3\n"),
        ("println({0 - 7} % 2)\n", "-1\n"),
        ("println(7 % {0 - 2})\n", "1\n"),
        // Negating i64::MIN wraps back to itself; `int.abs` saturates to Int
        // max. The literal exceeds the lexer's magnitude range, so reach
        // i64::MIN via the wrapped `MAX + 1`.
        (
            "m = 9223372036854775807 + 1\n\
             println(0 - m)\n",
            "-9223372036854775808\n",
        ),
        (
            "import al/int\n\
             m = 9223372036854775807 + 1\n\
             println(int.abs(m))\n",
            "9223372036854775807\n",
        ),
        // AL's value space has no Inf/NaN, so float overflow collapses to 0.0.
        // e-notation does not lex, so reach Inf by repeated squaring.
        (
            "fn sq(x Float, n Int) Float { if n == 0 { x } else { sq(x * x, n - 1) } }\n\
             println(sq(10.0, 12))\n",
            "0.0\n",
        ),
    ];
    for (src, want) in exact {
        run_outputs(src, want);
    }

    // 25! reduced mod 2^64 and reinterpreted as a signed i64.
    run_outputs(
        "fn f(n Int) Int { if n == 0 { 1 } else { n * f(n - 1) } }\nprintln(f(25))\n",
        "7034535277573963776\n",
    );
}

run_case! {
    // U20b: U20 for the generic (unspecialized) opcodes. An unannotated
    // `fn op(a, b)` generalizes to one body emitting `Op::Add`/`Lt`/... which
    // tag-dispatches at runtime. Calling each fn at BOTH Int and Float is what
    // proves the generic op is live; a specialized body could not serve both.
    u20b_generic_polymorphic_numeric_ops_are_total: (
        "fn subtract(a, b) { a - b }\n\
         fn multiply(a, b) { a * b }\n\
         fn divide(a, b) { a / b }\n\
         fn add(a, b) { a + b }\n\
         fn modulo(a, b) { a % b }\n\
         fn smaller(a, b) { a < b }\n\
         fn greater(a, b) { a > b }\n\
         fn at_most(a, b) { a <= b }\n\
         fn at_least(a, b) { a >= b }\n\
         println(subtract(10, 3))\n\
         println(subtract(2.5, 1.0))\n\
         println(multiply(6, 7))\n\
         println(multiply(1.5, 2.0))\n\
         println(divide(7, 2))\n\
         println(divide(7.0, 2.0))\n\
         println(divide(10, 0))\n\
         println(divide(10.0, 0.0))\n\
         println(add(1, 2))\n\
         println(add(1.5, 2.5))\n\
         println(add(9223372036854775807, 1))\n\
         println(modulo(17, 5))\n\
         println(modulo(7, 0))\n\
         println(smaller(1, 2))\n\
         println(smaller(2.5, 1.0))\n\
         println(greater(5, 3))\n\
         println(greater(1.0, 9.0))\n\
         println(at_most(2, 2))\n\
         println(at_most(3.0, 2.0))\n\
         println(at_least(2, 5))\n\
         println(at_least(5.0, 5.0))\n",
        "7\n1.5\n42\n3.0\n3\n3.5\n0\n0.0\n3\n4.0\n-9223372036854775808\n2\n7\n\
         True\nFalse\nTrue\nFalse\nTrue\nFalse\nFalse\nTrue\n",
    ),
}

// U21: exhaustiveness must lower constructor-pattern args into
// field-DECLARATION order, the order `slot_ctor_args` uses at runtime.
// Lowering in source order permutes the usefulness matrix against real match
// semantics, which both accepts non-exhaustive matches and rejects exhaustive
// ones.
#[test]
fn u21_exhaustiveness_respects_field_labels() {
    // Unsound direction: the two arms together miss (a=True, b=False), so the
    // match is genuinely non-exhaustive.
    check_rejects(
        "type Pair {\n\ta Bool\n\tb Bool\n}\n\
         fn f(p Pair) Int {\n\
         \tmatch p {\n\
         \t\tPair(b: True, ..) -> 1\n\
         \t\tPair(a: False, ..) -> 2\n\
         \t}\n\
         }\n\
         println(f(Pair(a: True, b: False)))\n",
        "not exhaustive",
    );

    // False-positive direction: an exhaustive match whose third arm names
    // fields in reverse order covers (a=False, b=True), so f returns 3.
    run_outputs(
        "type Pair {\n\ta Bool\n\tb Bool\n}\n\
         fn f(p Pair) Int {\n\
         \tmatch p {\n\
         \t\tPair(a: True, b: True) -> 1\n\
         \t\tPair(a: True, b: False) -> 2\n\
         \t\tPair(b: True, a: False) -> 3\n\
         \t\tPair(a: False, b: False) -> 4\n\
         \t}\n\
         }\n\
         println(f(Pair(a: False, b: True)))\n",
        "3\n",
    );
}

// U22: the module-scope twin of U4. A captured top-level binding resolves to
// `Global(slot)`, read from the entry frame at call time. AL is
// immutable-with-shadowing, so a module-scope re-binding must allocate a
// fresh slot; reusing it corrupts every closure that captured the old one.
#[test]
fn u22_module_shadow_preserves_closure_capture() {
    // Each closure captures the binding live at its definition; later shadows
    // must not retroactively change what an earlier closure sees.
    run_outputs(
        "x = 10\n\
         f = fn() x\n\
         x = 20\n\
         g = fn() x\n\
         x = 30\n\
         println(f())\n\
         println(g())\n\
         println(x)\n",
        "10\n20\n30\n",
    );

    // The fresh slot is allocated only after the initializer is compiled, so
    // `x = x + 1` observes the old `x`, not the new uninitialised slot.
    run_outputs(
        "x = 10\n\
         x = x + 1\n\
         println(x)\n",
        "11\n",
    );

    // Type-unsoundness direction: reusing the slot would let the String shadow
    // flow out of `f()` as an Int.
    run_outputs(
        "n = 1\n\
         f = fn() n + 100\n\
         n = 'hello'\n\
         println(f())\n\
         println(n)\n",
        "101\nhello\n",
    );
}

// U23: a slice whose bounds escape the array, or is reversed, is a clean
// runtime error: non-zero exit with a diagnostic, never a panic or abort.
#[test]
fn u23_oob_and_reversed_slice_are_clean_errors() {
    let cases = [
        (
            "println([1, 2, 3][0..10])\n",
            "Slice indices out of bounds: [0..10] (length 3)",
        ),
        (
            "println([1, 2, 3][2..1])\n",
            "Slice indices out of bounds: [2..1] (length 3)",
        ),
    ];
    for (src, want) in cases {
        run_rejects(src, want);
    }
}
