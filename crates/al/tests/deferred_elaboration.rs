//! Behavioural pins for the deferred-elaboration pass order.
//!
//! `analyse_module`'s Pass 5 parks every function body it walks and elaborates
//! it (`lower` → `perceus` → `emit`) only after the SCC has been generalized.
//! Elaboration therefore has to *rebuild* the codegen frame the walk saw —
//! captures, self-name, and the enclosing scope chain — and it has to place the
//! parked bodies in a code layout the walk never saw. Both are silent when they
//! go wrong: the program still runs, it just computes something else.

mod common;
use common::run_outputs;

/// A recursive local lambda whose self-name shadows a module-scope `fn`, used
/// as a *value* (passed to a HOF) inside its own body.
///
/// The walk resolves `foo` in `outer`'s frame and yields `VarLoad::Self_`.
/// Elaboration must resolve it the same way. If it only restores the module
/// scope, `resolve_variable`'s `current_binding` short-circuit — which fires
/// *before* the capture table is consulted — finds the module-level `foo`
/// instead, yields `SelfGlobal(module_slot)`, and `load_atom` emits
/// `PushGlobal` of the wrong function. The recursion then runs `n + 1000`
/// once and stops: 1002 instead of 100.
///
/// Recursive *calls* cannot catch this — `resolved_callee` collapses `Self_`
/// and `SelfGlobal` to the same `Callee::Self_`. Only a value load can.
#[test]
fn recursive_local_lambda_shadowing_a_module_fn_loads_itself() {
    run_outputs(
        "fn apply(f fn(Int) Int, x Int) Int { f(x) }\n\
         fn foo(n Int) Int { n + 1000 }\n\
         fn outer() Int {\n\
         \x20   foo = fn(n) { if n == 0 { 100 } else { apply(foo, n - 1) } }\n\
         \x20   foo(3)\n\
         }\n\
         println(outer())\n\
         println(foo(1))\n",
        "100\n1001\n",
    );
}

/// The same shape one frame deeper: the shadowing lambda is itself nested
/// inside another lambda, so elaboration has to restore a two-deep scope chain,
/// not just the innermost one.
#[test]
fn nested_recursive_lambda_shadowing_resolves_through_the_whole_chain() {
    run_outputs(
        "fn apply(f fn(Int) Int, x Int) Int { f(x) }\n\
         fn dbl(n Int) Int { n * 1000 }\n\
         fn outer(k Int) Int {\n\
         \x20   mid = fn(m) {\n\
         \x20       dbl = fn(n) { if n == 0 { m } else { apply(dbl, n - 1) } }\n\
         \x20       dbl(3)\n\
         \x20   }\n\
         \x20   mid(k)\n\
         }\n\
         println(outer(7))\n",
        "7\n",
    );
}

/// Every declaration in a mutual-recursion SCC is walked before any of its
/// bodies is emitted, so the SCC's jump-over placeholders sit contiguously
/// ahead of the whole run of bodies: `[J_a, J_b, body_a, Ret, body_b, Ret]`.
/// A jump-over patched to "just past my own `Ret`" therefore lands in the
/// *interior* of the next body. Each one must skip the entire run.
#[test]
fn mutually_recursive_scc_jumps_over_every_parked_body() {
    run_outputs(
        "fn is_even(n Int) Bool { if n == 0 { True } else { is_odd(n - 1) } }\n\
         fn is_odd(n Int) Bool { if n == 0 { False } else { is_even(n - 1) } }\n\
         fn ping(n Int) Int { if n == 0 { 0 } else { 1 + pong(n - 1) } }\n\
         fn pong(n Int) Int { if n == 0 { 0 } else { 1 + ping(n - 1) } }\n\
         println(is_even(10))\n\
         println(is_odd(10))\n\
         println(ping(9))\n",
        "True\nFalse\n9\n",
    );
}

/// A closure nested inside a mutually-recursive SCC member: its jump-over is
/// pushed during the walk of `a`, but its body is emitted first (innermost
/// closure first) — ahead of both `a`'s and `b`'s. Nothing may fall into it.
#[test]
fn closure_inside_a_mutual_scc_is_skipped_by_the_enclosing_stream() {
    run_outputs(
        "fn apply(f fn(Int) Int, x Int) Int { f(x) }\n\
         fn a(n Int) Int {\n\
         \x20   bump = fn(x) { x + 1 }\n\
         \x20   if n == 0 { 0 } else { apply(bump, b(n - 1)) }\n\
         }\n\
         fn b(n Int) Int { if n == 0 { 0 } else { a(n - 1) } }\n\
         println(a(4))\n",
        "2\n",
    );
}

/// Declared bodies are parked and elaborated *after* the toplevel `let` walk,
/// but `TypeEnv::define` overwrites a same-scope binding in place — so a
/// toplevel bind of a prelude name is visible in the env by the time `g`'s body
/// lowers. `resolve_name` re-reads the env for the callee's `ValueKind`; with
/// the rebound `Local` scheme it stops routing `println` as a builtin and, with
/// no entry-frame slot to find, falls back to `VarLoad::Self_` — `g` calls
/// itself and prints nothing. The env the decl walk saw has to be restored
/// around the parked bodies.
#[test]
fn toplevel_bind_shadowing_a_builtin_does_not_repoint_a_decl_body() {
    run_outputs(
        "fn g(x Int) Nil { println(x) }\n\
         println = 5\n\
         g(2)\n\
         g(println + 1)\n",
        "2\n6\n",
    );
}

/// The same rebinding one step milder: `h` is a declared `fn` (`ValueKind::
/// ModuleFn`), and the toplevel bind flips it to `Local`. `k`'s call to `h` must
/// still reach the *function*, not the `Int`.
#[test]
fn toplevel_bind_shadowing_a_decl_fn_does_not_repoint_a_sibling_body() {
    run_outputs(
        "fn h(x Int) Int { x + 1 }\n\
         fn k(y Int) Int { h(y) * 2 }\n\
         h = 100\n\
         println(k(3))\n\
         println(h)\n",
        "8\n100\n",
    );
}

/// The other half of the pin: a lambda parked by the toplevel `let` walk itself
/// legitimately needs those binds. `a` sits at module-scope depth, so the walk
/// resolves it to a `VarLoad::Global` rather than a capture — it is reachable at
/// elaboration time only through the *live* env, never through `capture_env`.
#[test]
fn toplevel_lambda_still_sees_earlier_toplevel_binds() {
    run_outputs(
        "a = 7\n\
         b = 3\n\
         g = fn(n Int) {\n\
         \x20   inner = fn(m Int) { m + a + b }\n\
         \x20   inner(n)\n\
         }\n\
         println(g(1))\n",
        "11\n",
    );
}
