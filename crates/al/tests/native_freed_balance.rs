//! FREED_OBJECTS balance: the heap-accounting half of the native/interpreter
//! parity contract, on bench_typed-shaped programs.
//!
//! Every heap object a run allocates (`ProcHeap::alloc_object`) must be freed
//! exactly once through `free_object` — the single reclamation point both
//! backends share (`release`, `native_release_at_zero`, and the reuse path's
//! child releases all land there, each bumping `FREED_OBJECTS`). A native
//! body that skips a drop (leak), double-drops (the poison check catches the
//! crash but not a silently absorbed count), or allocates outside
//! `ProcHeap::alloc_object` breaks the equality even when the program's
//! *output* is still correct — which is exactly why output parity alone is
//! not the gate.
//!
//! These tests run the VM in-process (like vm_exec's Perceus reuse tests) so
//! they can read the two thread-local counters around the run: allocations
//! from `ProcHeap::alloc_count`, frees from `freed_objects_total` (fed by
//! every `take_freed_objects` drain plus the undrained remainder). Native
//! code is published for real — `clif::plan` hook, `clif::compile`,
//! `finalize_into` — the same three steps `main.rs` performs, so whatever
//! subset of the program the coverage gate admits runs natively and the
//! interpreter runs the rest; the balance must hold across the mix.

mod common;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Mutex;

use al::bytecode::value::{freed_objects_total, reset_freed_objects_total};
use al::core_ir::clif;
use al::heap::ProcHeap;
use al::tivec::Idx as _;
use al::{bytecode, vm};

/// Pin `AL_NATIVE=native` before the process-wide config is first read, so
/// these tests exercise the native backend no matter which mode the outer
/// `cargo test` run inherited (same pattern as `native_hook.rs`). Every
/// `#[test]` in this binary calls this before compiling anything.
fn pin_native_mode() {
    static PIN: std::sync::Once = std::sync::Once::new();
    PIN.call_once(|| {
        // SAFETY: called before any env read of AL_NATIVE in this process;
        // all tests in this binary funnel through this `Once` first.
        unsafe { std::env::set_var("AL_NATIVE", "native") };
    });
}

/// Serializes the balance tests. The counters are thread-local so parallel
/// tests would not corrupt each other, but serializing keeps each run's
/// alloc/free ledger attributable to exactly one program when a failure has
/// to be diagnosed.
static BALANCE_LOCK: Mutex<()> = Mutex::new(());

/// Compile `src` with the Cranelift backend hooked in, JIT every plan the
/// coverage gate admitted, and publish the entries into the program's
/// `NativeTable` — the in-process equivalent of `main.rs::publish_native`.
/// Returns the program plus the names of the natively published functions.
fn compile_with_backend(src: &str) -> (bytecode::Program, Vec<String>) {
    pin_native_mode();
    let ast = common::parse(src);
    let plans: Rc<RefCell<Vec<clif::NativePlan>>> = Rc::default();
    let sink = Rc::clone(&plans);
    let hook: bytecode::NativeHook = Box::new(move |idx, f, pool| {
        if let Some(p) = clif::plan(idx, f, pool, al::STDLIB.prelude) {
            sink.borrow_mut().push(p);
        }
    });
    let r = bytecode::compile_with_native(&ast, None, Some(&al::STDLIB), hook);
    assert!(
        r.success(),
        "compile failed: {:?}\n---\n{src}",
        r.diagnostics
    );
    let program = r.emitted.expect("a successful compile emits").program;
    let mut module = vm::jit::jit_module().expect("jit module");
    let mut defs = Vec::new();
    let plans = plans.take();
    let native = clif::native_set(&plans, &program);
    for plan in &plans {
        if let Some(body) =
            clif::compile(&mut module, plan, &native, &program).expect("clif define")
        {
            let name = program
                .functions
                .get(body.func_idx.index())
                .map(|f| f.name.to_string())
                .unwrap_or_default();
            defs.push(vm::jit::JitDef {
                fn_idx: body.func_idx,
                func_id: body.func_id,
                name,
                code_size: body.code_size,
            });
        }
    }
    vm::jit::finalize_into(&mut module, &defs, &program.native).expect("finalize jit module");
    let names = defs.into_iter().map(|d| d.name).collect();
    // Dropping the module keeps the executable mapping alive (vm::jit's
    // code-lifetime contract), so the published entries outlive this frame.
    (program, names)
}

/// Run `src` (native published) to completion, assert the Int result, and
/// assert the heap ledger balances: allocations during the run == objects
/// freed by the run plus VM teardown. The program must leave its result as
/// an immediate and bind nothing at toplevel, so nothing heap-allocated
/// legitimately outlives the VM.
fn run_balanced(tag: &str, src: &str, expect: i64) -> Vec<String> {
    let _g = BALANCE_LOCK.lock().unwrap();
    let (program, native_fns) = compile_with_backend(src);
    ProcHeap::reset_alloc_count();
    reset_freed_objects_total();
    let mut v = vm::new_vm(program).expect("vm init");
    let val = v.run().expect("vm run");
    let shown = vm::inspect(&val, v.program());
    drop(val);
    drop(v);
    let allocs = ProcHeap::alloc_count() as u64;
    let freed = freed_objects_total();
    assert_eq!(shown, expect.to_string(), "[{tag}] result mismatch:\n{src}");
    assert!(
        allocs > 0,
        "[{tag}] the run allocated nothing — the balance check is vacuous \
         (every program here builds hundreds of heap cells)"
    );
    assert_eq!(
        allocs, freed,
        "[{tag}] FREED_OBJECTS out of balance: {allocs} allocations vs {freed} frees \
         (native fns: {native_fns:?}) — a native body leaked or double-counted a drop:\n{src}"
    );
    native_fns
}

/// `examples/bench_typed.al`'s function set — enum ctors + match (`build`/
/// `sum`), Bool constructor heads (`is_even`/`is_odd`), record ctors + field
/// projection (`dot`/`dot_loop` with its loop-carried reuse pair) — plus
/// `fact`, an Int-only body the A0 gate already admits, so at least one
/// function is natively published even while A1 coverage is landing. The
/// toplevel is a single expression: no globals survive the run, so the
/// ledger must close exactly.
const BENCH_TYPED_SHAPE: &str = "\
type Tree {\n\
\tLeaf(value Int)\n\
\tNode(left Tree, right Tree)\n\
}\n\
fn build(depth Int) Tree {\n\
\tif depth == 0 {\n\
\t\tLeaf(1)\n\
\t} else {\n\
\t\tNode(build(depth - 1), build(depth - 1))\n\
\t}\n\
}\n\
fn sum(t Tree) Int {\n\
\tmatch t {\n\
\t\tLeaf(v) -> v\n\
\t\tNode(l, r) -> sum(l) + sum(r)\n\
\t}\n\
}\n\
fn is_even(n Int) Bool {\n\
\tif n == 0 { True } else { is_odd(n - 1) }\n\
}\n\
fn is_odd(n Int) Bool {\n\
\tif n == 0 { False } else { is_even(n - 1) }\n\
}\n\
type Point {\n\
\tx Int\n\
\ty Int\n\
\tz Int\n\
}\n\
fn dot(a Point, b Point) Int {\n\
\ta.x * b.x + a.y * b.y + a.z * b.z\n\
}\n\
fn dot_loop(n Int, acc Int) Int {\n\
\tif n == 0 {\n\
\t\tacc\n\
\t} else {\n\
\t\tp = Point(n, n + 1, n + 2)\n\
\t\tq = Point(n + 3, n + 4, n + 5)\n\
\t\tdot_loop(n - 1, acc + dot(p, q))\n\
\t}\n\
}\n\
fn fact(n Int) Int {\n\
\tif n < 2 { 1 } else { n * fact(n - 1) }\n\
}\n\
sum(build(8)) + dot_loop(500, 0) + fact(12) + { if is_even(1000) { 1 } else { 0 } }\n";

#[test]
fn freed_objects_balance_on_bench_typed_shape() {
    // sum(build(8)) = 2^8 leaves of 1; dot_loop(N,0) = Σ 3n²+15n+14;
    // fact(12); is_even(1000) = True → 1.
    let n: i64 = 500;
    let dot = 3 * (n * (n + 1) * (2 * n + 1) / 6) + 15 * (n * (n + 1) / 2) + 14 * n;
    let fact12: i64 = (1..=12).product();
    let expect = 256 + dot + fact12 + 1;
    let native_fns = run_balanced("bench_typed_shape", BENCH_TYPED_SHAPE, expect);
    assert!(
        !native_fns.is_empty(),
        "no function was published natively — the balance check ran interpreter-only \
         (the gate must admit at least `fact`, an A0 Int-only body)"
    );
}

/// The Perceus list scaffold from vm_exec, exercised both ways through one
/// balance ledger. Reuse must not distort the ledger: a hollowed cell
/// (unique drop parked for the next same-shape ctor) is one allocation and,
/// when finally released, one free — while its released children each count.
const LIST_SRC: &str = "\
type List {\n\
\tLNil\n\
\tLCons(head Int, tail List)\n\
}\n\
fn build(n Int) List {\n\
\tif n == 0 { LNil } else { LCons(n, build(n - 1)) }\n\
}\n\
fn lmap(xs List, f fn(Int) Int) List {\n\
\tmatch xs {\n\
\t\tLNil -> LNil\n\
\t\tLCons(h, t) -> LCons(f(h), lmap(t, f))\n\
\t}\n\
}\n\
fn lsum(xs List) Int {\n\
\tmatch xs {\n\
\t\tLNil -> 0\n\
\t\tLCons(h, t) -> h + lsum(t)\n\
\t}\n\
}\n\
fn double(x Int) Int { x * 2 }\n";

#[test]
fn freed_objects_balance_on_unique_reuse_chain() {
    // Ten re-maps of a uniquely owned 100-cell list: the in-place reuse path
    // (rc==1 hollow + same-shape ctor) dominates. Σ1..100 = 5050, doubled 10×.
    let src = format!(
        "{LIST_SRC}\
         fn chain(xs List, k Int) List {{\n\
         \tif k == 0 {{ xs }} else {{ chain(lmap(xs, double), k - 1) }}\n\
         }}\n\
         lsum(chain(build(100), 10))\n"
    );
    let native_fns = run_balanced("unique_reuse_chain", &src, 5050 * 1024);
    for f in ["build", "lmap"] {
        assert!(
            native_fns.iter().any(|n| n == f),
            "`{f}` was not published natively (got {native_fns:?}) — the balance check \
             degraded to interpreter-only and no longer guards the native reuse path"
        );
    }
}

#[test]
fn freed_objects_balance_on_shared_fallback() {
    // `xs` stays live across the map, so every cell is rc>=2 at its drop
    // site: the reuse gate fails, the shared-drop path (decrement, no free)
    // runs per cell, and the ctor allocates fresh. All local to `share`, so
    // everything is reclaimed by the run itself.
    let src = format!(
        "{LIST_SRC}\
         fn share(n Int) Int {{\n\
         \txs = build(n)\n\
         \tys = lmap(xs, double)\n\
         \tlsum(xs) + lsum(ys)\n\
         }}\n\
         share(100)\n"
    );
    let native_fns = run_balanced("shared_fallback", &src, 5050 + 10100);
    for f in ["build", "lmap"] {
        assert!(
            native_fns.iter().any(|n| n == f),
            "`{f}` was not published natively (got {native_fns:?}) — the balance check \
             degraded to interpreter-only and no longer guards the native shared-drop path"
        );
    }
}
