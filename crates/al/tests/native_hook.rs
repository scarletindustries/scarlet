//! The native-backend seam: `bytecode::compile_with_native` fires its hook
//! once per lowered function body, at the one point in the pipeline where the
//! body's post-perceus `CoreFn` and the `ResolvedPool` its `RTy`s index are
//! both alive, keyed by the same `FuncIdx` numbering as the emitted
//! `Program.functions` table — and installing a hook must not perturb that
//! numbering (the invariant `check_parity.rs` pins for check vs build).

mod common;

use std::cell::RefCell;
use std::rc::Rc;

use al::tivec::Idx;
use al::types::Prim;
use common::parse;

/// What the hook recorded for one body: its `FuncIdx`, its arity, and the
/// prims its param/return `RTy`s resolve to *through the pool it arrived
/// with*. Resolving them inside the hook is the point of the test: an `RTy`
/// is an index into a per-body pool, so a hook handed the wrong (or an
/// already-dropped) pool would panic or misresolve here.
#[derive(Debug, Clone, PartialEq)]
struct Seen {
    idx: usize,
    arity: usize,
    param_prims: Vec<Option<Prim>>,
    ret_prim: Option<Prim>,
}

/// Pin `AL_NATIVE=native` before the process-wide config is first read.
/// These tests assert the hook *fires*; inheriting `off` (which suppresses
/// the hook) or `mix` (which fires it for a random subset) from the suite's
/// outer environment would make them fail for reasons that have nothing to
/// do with the seam under test. `Once` runs the write exactly once, before
/// any thread can reach a `config()` read through `compile_with_native`.
///
/// Every `#[test]` in this binary calls this as its first line (idempotent
/// via `Once`); new tests must do the same before compiling anything.
fn pin_native_mode() {
    static PIN: std::sync::Once = std::sync::Once::new();
    PIN.call_once(|| {
        // SAFETY: called before any env read of AL_NATIVE in this process;
        // all tests in this binary funnel through this `Once` first.
        unsafe { std::env::set_var("AL_NATIVE", "native") };
    });
}

fn compile_recording(source: &str) -> (al::bytecode::Program, Vec<Seen>) {
    pin_native_mode();
    let ast = parse(source);
    let seen = Rc::new(RefCell::new(Vec::new()));
    let sink = Rc::clone(&seen);
    let hook: al::bytecode::NativeHook = Box::new(move |idx, f, pool| {
        sink.borrow_mut().push(Seen {
            idx: idx.index(),
            arity: f.params.len(),
            param_prims: f.params.iter().map(|b| pool.prim_of(b.ty)).collect(),
            ret_prim: pool.prim_of(f.ret_ty),
        });
    });
    let result = al::bytecode::compile_with_native(&ast, None, Some(&al::STDLIB), hook);
    assert!(
        result.success(),
        "compile failed:\n{source}\n{:#?}",
        result.diagnostics
    );
    let program = result.emitted.expect("compile emits").program;
    // The compiler dropped its `NativeHook` box when the compile finished, so
    // the recording is exclusively ours again.
    let seen = Rc::try_unwrap(seen).expect("hook released").into_inner();
    (program, seen)
}

const SOURCE: &str = r#"
fn add(a Int, b Int) Int {
    a + b
}

fn twice(x Int) Int {
    add(x, x)
}

twice(4)
"#;

#[test]
fn hook_fires_per_body_keyed_by_program_func_idx() {
    pin_native_mode();
    let (program, seen) = compile_recording(SOURCE);

    let mut idxs: Vec<usize> = seen.iter().map(|s| s.idx).collect();
    idxs.sort_unstable();
    idxs.dedup();
    assert_eq!(
        idxs.len(),
        seen.len(),
        "one hook call per FuncIdx: {seen:#?}"
    );

    for s in &seen {
        let f = &program.functions[s.idx];
        assert_eq!(
            f.arity as usize, s.arity,
            "hooked body {} disagrees with its Function entry on arity",
            f.name
        );
    }

    let name_of = |s: &Seen| program.functions[s.idx].name.to_string();
    let add = seen
        .iter()
        .find(|s| name_of(s) == "add")
        .expect("hook saw `add`");
    let twice = seen
        .iter()
        .find(|s| name_of(s) == "twice")
        .expect("hook saw `twice`");

    // The RTys resolved through the pool the hook was handed: this is the
    // type-directed-emit contract A0 builds on (prove Int, emit unboxed).
    assert_eq!(add.param_prims, vec![Some(Prim::Int), Some(Prim::Int)]);
    assert_eq!(add.ret_prim, Some(Prim::Int));
    assert_eq!(twice.param_prims, vec![Some(Prim::Int)]);
    assert_eq!(twice.ret_prim, Some(Prim::Int));
}

#[test]
fn installing_the_hook_does_not_perturb_fn_numbering() {
    pin_native_mode();
    let (hooked, _) = compile_recording(SOURCE);

    let ast = parse(SOURCE);
    let plain = al::bytecode::compile(&ast, None, Some(&al::STDLIB));
    assert!(plain.success(), "compile failed: {:#?}", plain.diagnostics);
    let plain = plain.emitted.expect("compile emits").program;

    let shape = |p: &al::bytecode::Program| -> Vec<(String, i32, i32)> {
        p.functions
            .iter()
            .map(|f| (f.name.to_string(), f.arity, f.capture_count))
            .collect()
    };
    assert_eq!(shape(&hooked), shape(&plain));
}

#[test]
fn toplevel_glue_is_never_hooked() {
    pin_native_mode();
    let (program, seen) = compile_recording(SOURCE);
    let entry = program.entry as usize;
    assert_eq!(&*program.functions[entry].name, "__main__");
    assert!(
        seen.iter().all(|s| s.idx != entry),
        "`__main__` is always-interpreted glue and must not reach the hook"
    );
}
