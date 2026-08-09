//! `bytecode::compile_with_native` fires its hook once per lowered function
//! body, at the one point where the body's post-perceus `CoreFn` and the
//! `ResolvedPool` its `RTy`s index are both alive, keyed by the same
//! `FuncIdx` numbering as `Program.functions`. Installing a hook must not
//! perturb that numbering.

mod common;

use std::cell::RefCell;
use std::rc::Rc;

use al::tivec::Idx;
use al::types::Prim;
use common::parse;

/// What the hook recorded for one body. The prims are resolved inside the
/// hook, through the pool it arrived with: an `RTy` indexes a per-body pool,
/// so a wrong or dropped pool panics or misresolves right there.
#[derive(Debug, Clone, PartialEq)]
struct Seen {
    idx: usize,
    arity: usize,
    param_prims: Vec<Option<Prim>>,
    ret_prim: Option<Prim>,
}

/// Pin `AL_NATIVE=native` before the process-wide config is first read.
/// Inheriting `off` or `mix` from the outer environment would stop the hook
/// firing. Every `#[test]` here must call this as its first line.
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
    // The compiler dropped its `NativeHook` box, so the recording is ours.
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
    // Stdlib bodies precede the user program and the stdlib has its own
    // `add`, so the user's bodies are the last bearers of these names.
    let add = seen
        .iter()
        .rfind(|s| name_of(s) == "add")
        .expect("hook saw `add`");
    let twice = seen
        .iter()
        .rfind(|s| name_of(s) == "twice")
        .expect("hook saw `twice`");

    // The RTys resolved through the pool the hook was handed.
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

/// Installing a hook makes `compile_impl` recompile the whole stdlib from
/// source instead of seeding the precompiled blob. That recompile must
/// reproduce the seeded program exactly, or the native path runs a different
/// program than `off` does.
#[test]
fn relowered_stdlib_reproduces_the_seeded_program() {
    pin_native_mode();
    // Real imports, so the comparison covers module init glue too.
    let src = r#"
import al/array
import al/string

fn double(x Int) Int { x * 2 }

println(array.length(array.map([1, 2, 3], double)))
println(string.length('hello'))
"#;
    let (relowered, seen) = compile_recording(src);
    assert!(
        seen.len() > 100,
        "expected the hook to see the whole stdlib (got {} bodies)",
        seen.len()
    );

    let plain = al::bytecode::compile(&parse(src), None, Some(&al::STDLIB));
    assert!(plain.success(), "compile failed: {:#?}", plain.diagnostics);
    let seeded = plain.emitted.expect("compile emits").program;

    let fns = |p: &al::bytecode::Program| -> Vec<(String, i32, i32, i32, i32, i32)> {
        p.functions
            .iter()
            .map(|f| {
                (
                    f.name.to_string(),
                    f.arity,
                    f.locals,
                    f.capture_count,
                    f.code_start,
                    f.code_len,
                )
            })
            .collect()
    };
    assert_eq!(fns(&relowered), fns(&seeded), "function tables diverge");
    assert_eq!(relowered.entry, seeded.entry);

    let code = |p: &al::bytecode::Program| -> Vec<(al::bytecode::Op, u8, u16, i32)> {
        p.code.iter().map(|i| (i.op, i.a, i.b, i.operand)).collect()
    };
    assert_eq!(code(&relowered), code(&seeded), "bytecode diverges");

    // `Value`'s Debug renders logically, with no addresses, so runtime-built
    // frozen constants compare equal to the blob's.
    let consts = |p: &al::bytecode::Program| -> Vec<String> {
        p.constants.iter().map(|v| format!("{v:?}")).collect()
    };
    assert_eq!(
        consts(&relowered),
        consts(&seeded),
        "constant pools diverge"
    );
}
