mod common;
use common::{check_rejects, run_outputs, run_rejects};

reject_case! {
    /// U1: `x = x` self-reference infers ⊥, generalizes to ∀A.A, runtime slot is None.
    /// Post-fix: pre-define is gated to lambda inits only; init `x` is unbound.
    u1_self_reference_is_rejected: (
        "x = x\n\
         n Int = x\n\
         println(n + 1)\n",
        "Unknown identifier 'x'",
    ),

    /// U2: `if cond { e }` (no else) used to type as Nil but evaluate to `e` when
    /// cond was true, exploitable through `Option(T)` widening to smuggle a String
    /// as Int. Post-fix: `if` without `else` is a parse/type error.
    u2_if_no_else_is_rejected: (
        "fn smuggle() Option(Int) { if True { 'hello' } }\n\
         n = smuggle() or 0\n\
         println(n + 1)\n",
        "'if' requires an 'else' branch",
    ),

    /// U5: generic type used without type args silently fills fresh vars per use,
    /// letting a String flow into an Int position.
    /// Post-fix: missing type arguments at the field declaration is a type error.
    u5_generic_type_missing_args_is_rejected: (
        "type Box(t) { Box(value t) }\n\
         type Holder { Holder(box Box) }\n\
         h = Holder(box: Box(value: 'hello'))\n\
         println(h.box.value + 100)\n",
        "Type 'Box' expects 1 type argument",
    ),

    /// U7: or-pattern alternatives may bind disjoint name sets; the body reads an
    /// uninitialized local when the non-binding alternative matches.
    /// Post-fix: asymmetric bindings across `|` alternatives are rejected.
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

    /// U8: range pattern bounds are not unified with Int, so `true..'z'` typechecks
    /// and crashes the VM comparator.
    /// Post-fix: non-Int bounds are a type error.
    u8_range_pattern_bounds_must_be_int: (
        "r = match 5 {\n\
         \ttrue..'z' -> 'huh'\n\
         \t_ -> 'ok'\n\
         }\n\
         println(r)\n",
        "Range pattern bounds must be number literals",
    ),

    /// U9: array pattern with a spread that is not the final element corrupts the
    /// stack at runtime.
    /// Post-fix: non-tail (or multiple) spreads in an array pattern are rejected.
    u9_array_pattern_non_tail_spread_is_rejected: (
        "xs = [1, 2, 3]\n\
         r = match xs {\n\
         \t[a, ..mid, z] -> a + z\n\
         \t_ -> 0\n\
         }\n\
         println(r)\n",
        "Spread in array pattern must be the last element",
    ),

    /// U10: exhaustiveness for `[a, b, ..rest]` drops every prefix element after
    /// the first, so a non-exhaustive match is accepted and falls through to None
    /// at runtime for an Int-returning function.
    /// Post-fix: the missing length-1 case is reported as non-exhaustive.
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

    /// U12: duplicate binding name within a single pattern silently shadows, so
    /// `(x, x)` binds the second element only.
    /// Post-fix: duplicate names in one pattern are rejected.
    u12_duplicate_pattern_binding_is_rejected: (
        "r = match (1, 2) {\n\
         \t(x, x) -> x\n\
         }\n\
         println(r)\n",
        "bound more than once in this pattern",
    ),

    /// U13: `Socket` is an opaque builtin type with no constructors. Under the
    /// types redesign there is no `{}`-init at all; `Socket` referenced as a value
    /// is an unknown identifier.
    u13_socket_literal_is_rejected: (
        "s = Socket\n\
         println(s)\n",
        "Unknown identifier 'Socket'",
    ),

    /// U16: two functions with the same name used to panic the compiler in
    /// analysis Pass 5 — the name-keyed hydrator map only kept the last decl, so
    /// the first decl's `hydrators.remove(name).expect(...)` blew up. It must be a
    /// clean "already defined" rejection, never a panic.
    u16_duplicate_fn_is_rejected_without_panic:
        ("fn a(x) {x}\nfn a() {}\n", "already defined"),

    /// U17: a bare `@vm` (no op arg) used to panic an `unreachable!()` in analysis
    /// Pass 5. The parser keyed body-presence on the `@vm` *name* while the siphon
    /// keyed removal on the `@vm` *arg*; a zero-arg `@vm` diverged the two, kept a
    /// body-less fn in `decls`, and blew up. The `FnBody` enum makes that
    /// unrepresentable: it must be a clean parse error, never a panic.
    u17_bare_vm_attr_is_rejected_without_panic:
        ("@vm\nfn foo() Int\n", "@vm requires an op"),

    /// U18: the type-decl twin of U16. Duplicate `type` declarations used to desync
    /// the name-keyed Pass-1 maps (`type_param_generics` + the shared `hydrators`
    /// field) against the positional `type_decls` Vec — the exact class of the
    /// canonical bug, left unfixed for types. The `PreparedType` positional refactor
    /// + the Pass-0 dedup make it a clean rejection, never a panic/miscompile.
    u18_duplicate_type_is_rejected_without_panic: (
        "type T = Int\ntype T = String\nx = 1\nprintln(x)\n",
        "already defined",
    ),
}

// ---------------------------------------------------------------------------
// U3: the `error` keyword is gone; `Err(x)` is an ordinary constructor and
// must typecheck as such. A function returning `Result(Int, E)` whose body
// returns `Err(...)` typechecks; the `or` handler observes the error.
#[test]
fn u3_err_constructor_typechecks() {
    run_outputs(
        "type E { E(msg String) }\n\
         fn f() Result(Int, E) {\n\
         \tErr(E(msg: 'boom'))\n\
         }\n\
         r = f() or _e -> 99\n\
         println(r)\n",
        "99\n",
    );
}

// ---------------------------------------------------------------------------
// U4: type-env is block-scoped but the local-slot map is function-scoped, so
// an inner `x = 'hi'` overwrites the outer slot while the outer type stays Int.
// Post-fix: inner binding gets a fresh slot; outer `x` remains 1.
#[test]
fn u4_block_scope_preserves_outer_slot() {
    run_outputs(
        "x = 1\n\
         r = {\n\
         \tx = 'hi'\n\
         \tx\n\
         }\n\
         println(r)\n\
         println(x + 100)\n",
        "hi\n101\n",
    );
}

// ---------------------------------------------------------------------------
// U6: bare variant name in a match over an unannotated subject is compiled as
// a wildcard binding, so every value falls into the first arm.
// Post-fix: variant ownership is resolved and dispatch is correct.
#[test]
fn u6_bare_variant_on_inferred_subject_dispatches() {
    run_outputs(
        "type E { A B }\n\
         fn f(e) {\n\
         \tmatch e {\n\
         \t\tA -> 1\n\
         \t\tB -> 2\n\
         \t}\n\
         }\n\
         println(f(B))\n",
        "2\n",
    );
}

// ---------------------------------------------------------------------------
// U14: generic type payload types are not substituted before exhaustiveness,
// so a fully exhaustive match over `Maybe(Bool)` is wrongly rejected.
// Post-fix: payloads are substituted; the program checks and runs.
#[test]
fn u14_generic_enum_exhaustiveness_substitutes_payload() {
    run_outputs(
        "type Maybe(t) { Just(value t) Nothing }\n\
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
    );
}

// ---------------------------------------------------------------------------
// U15: the scanner greedily consumes `.` after a digit, so `t.0.name` lexes
// as `t` `.` `0.` `name` and produces cascading parse/type errors.
// Post-fix: `.` is only consumed when followed by a digit; field access works.
#[test]
fn u15_tuple_index_then_field_access() {
    run_outputs(
        "type P { P(name String) }\n\
         t = (P(name: 'hi'), 5)\n\
         println(t.0.name)\n",
        "hi\n",
    );
}

// U19: a long `else if` ladder used to recurse `parse_if_expression` at
// constant guard depth and overflow the native stack — an uncatchable
// SIGSEGV, strictly worse than a panic. The guarded-wrapper split bounds it:
// it must be a clean "nesting too deep" rejection, never a crash.
#[test]
fn u19_deep_else_if_chain_is_rejected_without_overflow() {
    let mut source = String::from("x = if 1 < 2 { 0 }");
    for _ in 0..60_000 {
        source.push_str(" else if 1 < 2 { 0 }");
    }
    source.push_str(" else { 0 }\nprintln(x)\n");
    check_rejects(&source, "too deep");
}

// U20: arithmetic is TOTAL — `/0`, `%0`, overflow, and non-finite float
// results must each yield a value and let the VM keep running. The runtime
// must never exit early on arithmetic: no Rust panic, no signal/abort, no
// non-zero exit. `x/0 = 0`, `x%0 = x`, overflow wraps (defined), non-finite
// float -> 0.0.
#[test]
fn u20_arithmetic_is_total_vm_never_exits() {
    let exact = [
        ("println(1 / 0)\n", "0\n"),
        ("println(5 % 0)\n", "5\n"),
        ("println(1.0 / 0.0)\n", "0.0\n"),
        ("println(0.0 / 0.0)\n", "0.0\n"),
        // Float `%` is total like int `%`: a normal remainder takes the sign of
        // the dividend, and `x % 0.0 = x`. (Regression: float modulo used to
        // type-check but crash the VM with "Unknown binary op".)
        ("println(7.5 % 2.0)\n", "1.5\n"),
        ("println(7.5 % 0.0)\n", "7.5\n"),
        ("println({0.0 - 7.5} % 2.0)\n", "-1.5\n"),
        // Integer overflow wraps two's-complement (defined), never traps:
        // i64::MAX + 1 == i64::MIN, i64::MAX * 2 == -2.
        (
            "println(9223372036854775807 + 1)\n",
            "-9223372036854775808\n",
        ),
        ("println(9223372036854775807 * 2)\n", "-2\n"),
        // Signed division truncates toward zero; the remainder takes the sign
        // of the dividend (Rust `wrapping_div`/`wrapping_rem`). Grouping is
        // `{expr}`, so `{0 - 7}` is the negative dividend.
        ("println({0 - 7} / 2)\n", "-3\n"),
        ("println({0 - 7} % 2)\n", "-1\n"),
        ("println(7 % {0 - 2})\n", "1\n"),
        // i64::MIN has no positive counterpart: both unary negation and
        // `int.abs` of it wrap back to itself. The i64::MIN literal is
        // unrepresentable (it exceeds the lexer's positive-magnitude range),
        // so reach it via the wrapped `MAX + 1`.
        (
            "m = 9223372036854775807 + 1\n\
             println(0 - m)\n",
            "-9223372036854775808\n",
        ),
        (
            "import al/int\n\
             m = 9223372036854775807 + 1\n\
             println(int.abs(m))\n",
            "-9223372036854775808\n",
        ),
        // Float overflow to +/-Inf collapses to 0.0 — AL's value space has no
        // Inf/NaN. e-notation does not lex, so reach Inf by squaring: 10^4096
        // overflows f64, becomes 0.0, and every later square stays 0.0.
        (
            "fn sq(x Float, n Int) Float { if n == 0 { x } else { sq(x * x, n - 1) } }\n\
             println(sq(10.0, 12))\n",
            "0.0\n",
        ),
    ];
    for (src, want) in exact {
        run_outputs(src, want);
    }

    // A real recursive computation that overflows mid-stream still completes
    // with the EXACT two's-complement wrapped product: 25! reduced mod 2^64
    // and reinterpreted as a signed i64 is 7034535277573963776.
    run_outputs(
        "fn f(n Int) Int { if n == 0 { 1 } else { n * f(n - 1) } }\nprintln(f(25))\n",
        "7034535277573963776\n",
    );
}

// ---------------------------------------------------------------------------
// U20b: the GENERIC polymorphic-fallback numeric ops, distinct from U20's
// type-specialized path. An unannotated `fn op(a, b)` is generalized (HM
// let-polymorphism) to a single body compiled once with the operand still a
// constrained var, so codegen takes the `(g, _) => g` arm and emits the
// generic opcode (`Op::Add`/`Sub`/`Mul`/`Div`/`Mod`, `Op::Lt`/`Gt`/`Lte`/
// `Gte`) instead of an `AddInt`/`AddFloat`/`LtInt`/... variant. That one body
// then serves both Int and Float at runtime via tag dispatch in `binary_op`/
// `float_op`/`compare`; calling each fn at BOTH types is what proves the
// generic op is live — a specialized body could not serve the other type. The
// generic arithmetic path must stay TOTAL exactly like the specialized one:
// x / 0 = 0, x % 0 = x, overflow wraps two's-complement, float / 0.0 = 0.0.
#[test]
fn u20b_generic_polymorphic_numeric_ops_are_total() {
    run_outputs(
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
        // subtract: Int 10-3=7, Float 2.5-1.0=1.5
        // multiply: Int 6*7=42, Float 1.5*2.0=3.0
        // divide:   Int 7/2=3 (trunc), Float 7.0/2.0=3.5, Int 10/0=0, Float 10.0/0.0=0.0
        // add:      Int 1+2=3, Float 1.5+2.5=4.0, i64::MAX+1 wraps to i64::MIN
        // modulo:   Int 17%5=2, Int 7%0=7 (x % 0 = x)
        // smaller/greater/at_most/at_least at Int then Float
        "7\n1.5\n42\n3.0\n3\n3.5\n0\n0.0\n3\n4.0\n-9223372036854775808\n2\n7\n\
         True\nFalse\nTrue\nFalse\nTrue\nFalse\nFalse\nTrue\n",
    );
}

// ---------------------------------------------------------------------------
// U21: the exhaustiveness checker lowered constructor-pattern args in SOURCE
// order, ignoring field labels and `..`-rest position, while the typechecker
// and runtime matcher slot each labeled arg into its field-DECLARATION index
// (`slot_ctor_args`). When a labeled pattern named fields out of declaration
// order, the usefulness matrix was permuted relative to real match semantics:
// it accepted non-exhaustive matches (unsound — a value matches no arm and an
// Int-typed fn falls through to Nil) and rejected genuinely-exhaustive ones.
// Post-fix: pattern args are lowered into field-declaration order, so the
// matrix agrees with runtime dispatch.
#[test]
fn u21_exhaustiveness_respects_field_labels() {
    // Unsound direction: `Pair(b: True, ..)` covers (a=_, b=True) and
    // `Pair(a: False, ..)` covers (a=False, b=_); together they MISS
    // (a=True, b=False), so the match is genuinely non-exhaustive. The old
    // source-order lowering read arm 1 as (a=True, b=_), making the matrix
    // look complete and letting `f(Pair(a: True, b: False))` return Nil.
    check_rejects(
        "type Pair { a Bool b Bool }\n\
         fn f(p Pair) Int {\n\
         \tmatch p {\n\
         \t\tPair(b: True, ..) -> 1\n\
         \t\tPair(a: False, ..) -> 2\n\
         \t}\n\
         }\n\
         println(f(Pair(a: True, b: False)))\n",
        "not exhaustive",
    );

    // False-positive direction: a genuinely EXHAUSTIVE match whose third arm
    // names fields in reverse order. The old lowering read `Pair(b: True,
    // a: False)` as (a=True, b=False) — a duplicate of arm 2 — and reported a
    // bogus "missing: Pair(False, True)". It must now be accepted and dispatch
    // correctly: the reversed arm covers (a=False, b=True), so f returns 3.
    run_outputs(
        "type Pair { a Bool b Bool }\n\
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

// ---------------------------------------------------------------------------
// U22: the module-scope twin of U4. A top-level binding captured by a closure
// resolves to `Global(slot)`, read from the entry frame at call time. Re-binding
// the same name at module scope used to REUSE that slot, so the shadow
// overwrote the very slot the closure reads — corrupting the closure (it
// returned the shadowing value, type-unsound when the shadow has a different
// type). AL is immutable-with-shadowing: each binding is distinct, so a closure
// must keep observing the binding it lexically captured. Post-fix: a module-
// scope re-binding allocates a fresh slot, so the earlier binding survives.
#[test]
fn u22_module_shadow_preserves_closure_capture() {
    // Each closure captures the binding live at its definition; later shadows
    // must not retroactively change what an earlier closure sees. Before the
    // fix every `fn() x` read the same reused slot, so all three printed 30.
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

    // The fix defers fresh-slot allocation until after the initializer is
    // compiled, so a re-binding whose init reads the prior binding still works:
    // `x = x + 1` must observe the old `x`, not the new (uninitialised) slot.
    run_outputs(
        "x = 10\n\
         x = x + 1\n\
         println(x)\n",
        "11\n",
    );

    // Type-unsoundness direction: the shadow has a different type than the
    // captured binding. The closure must still return the Int it captured;
    // reusing the slot would let the String shadow flow out of `f()` as an Int.
    run_outputs(
        "n = 1\n\
         f = fn() n + 100\n\
         n = 'hello'\n\
         println(f())\n\
         println(n)\n",
        "101\nhello\n",
    );
}

// ---------------------------------------------------------------------------
// U23: a slice `xs[start..end]` whose bounds escape the array — `end` past the
// length, or a reversed `start > end` — is a CLEAN runtime error: a non-zero
// process exit with a diagnostic, never an out-of-bounds panic or a signal/
// abort. `Op::ArraySlice` validates `start >= 0 && end <= len && start <= end`
// before slicing and returns `Err` otherwise, so the VM unwinds gracefully.
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
