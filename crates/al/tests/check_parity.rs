//! `al check` and `al build` must agree on the *shape* of the program they
//! elaborate — same functions, same indices, same capture counts.
//!
//! `check_only` used to be a second, silently-different compilation mode:
//! `enter_fn_frame` pushed its jump-over placeholder and `finish_fn_frame`
//! pushed the `Function` entry only when it was off, so under `al check` the
//! `func_idx` recorded into `global_to_func` / a frame's closure sites — and baked into
//! the Core IR that `lower` produces in *both* modes — was whatever index the
//! previous (usually stdlib) function happened to own. Nothing read it, so
//! nothing caught it, and every later pass that wanted to trust a `func_idx`
//! had to know which mode it was in.
//!
//! Now `check_only` truncates the pipeline in exactly one place
//! (`elaborate_body` returns before `perceus`/`emit`) and elides the toplevel
//! init and the peephole pass. Everything upstream of that — including which
//! `Function` slot a body owns — is mode-independent, which is what these
//! tests pin.

mod common;
use common::parse;

/// `(name, arity, capture_count)`, one per registered function.
type FnShape = Vec<(String, i32, i32)>;

/// The `(name, arity, capture_count)` of every function the compile
/// registered, in index order. `code_start`/`code_len`/`locals` are
/// deliberately excluded: those are the emit half, and `check` legitimately
/// leaves them empty.
fn fn_shape(p: &al::bytecode::Program) -> FnShape {
    p.functions
        .iter()
        .map(|f| (f.name.to_string(), f.arity, f.capture_count))
        .collect()
}

/// Compile and check the same AST; return each mode's function table shape.
fn both(source: &str) -> (FnShape, FnShape) {
    let ast = parse(source);
    let built = al::bytecode::compile(&ast, None, Some(&al::STDLIB));
    assert!(
        built.success,
        "compile failed:\n{source}\n{:#?}",
        built.diagnostics
    );
    let checked = al::bytecode::check(&ast, None, Some(&al::STDLIB));
    assert!(
        checked.success,
        "check failed:\n{source}\n{:#?}",
        checked.diagnostics
    );
    let built = built.emitted.expect("compile emits").program;
    let checked = checked.emitted.expect("check registers the function table");
    (fn_shape(&built), fn_shape(&checked.program))
}

#[test]
fn check_and_compile_register_the_same_functions() {
    let (built, checked) = both(
        r#"
fn add(a, b) {
	a + b
}
fn twice(x) {
	add(x, x)
}
println(twice(21))
"#,
    );
    assert_eq!(built, checked);
    let names: Vec<&str> = built.iter().map(|(n, _, _)| n.as_str()).collect();
    assert!(names.contains(&"add"), "{names:?}");
    assert!(names.contains(&"twice"), "{names:?}");
}

/// A nested closure's `func_idx` is what `Atom::Closure` carries, so this is
/// the case the old mode split corrupted most directly: the lambda's slot was
/// never reserved under `check`, and its recorded index aliased some stdlib
/// function's.
#[test]
fn check_and_compile_agree_on_nested_closures() {
    let (built, checked) = both(
        r#"
fn adder(n) {
	fn(x) {
		x + n
	}
}
add3 = adder(3)
println(add3(4))
"#,
    );
    assert_eq!(built, checked);
    // The lambda captures `n`, and both modes must say so.
    assert!(
        built.iter().any(|(_, _, caps)| *caps == 1),
        "no capturing function registered: {built:?}"
    );
}

/// An eta-wrapper (`Some` used as a value) is synthesised by `lower`, which
/// runs in both modes — so its `Function` entry exists in both, at the same
/// index.
#[test]
fn check_and_compile_agree_on_eta_wrappers() {
    let (built, checked) = both(
        r#"import al/array
type W { W(v Int) }
xs = [1, 2, 3]
ws = array.map(xs, W)
println(array.length(ws))
"#,
    );
    assert_eq!(built, checked);
    assert!(
        built.iter().any(|(n, _, _)| n == "W"),
        "no eta-wrapper registered: {built:?}"
    );
}

/// Mutual recursion puts both bodies in one SCC, so both are parked and
/// elaborated after generalization. Their reserved slots must survive that
/// round trip identically in both modes.
#[test]
fn check_and_compile_agree_on_mutually_recursive_scc() {
    let (built, checked) = both(
        r#"
fn is_even(n) {
	if n == 0 {
		True
	} else {
		is_odd(n - 1)
	}
}
fn is_odd(n) {
	if n == 0 {
		False
	} else {
		is_even(n - 1)
	}
}
println(is_even(10))
"#,
    );
    assert_eq!(built, checked);
}

/// `base_of[pc]` is the `code_start` the operand at `code[pc]` resolves
/// against. A jump operand is *frame-relative*: `emit` numbers a body's jumps
/// from that body's own `code_start`, and the VM branches to
/// `code_start + operand` using the frame it is executing. Everything spliced
/// around the bodies — the jump-overs, the toplevel init — runs in the entry
/// frame, whose `code_start` is 0. So a bare `ins.operand` is not a
/// `program.code` index and comparing it to one is a category error; this is
/// the table that turns it into one.
fn frame_bases(p: &al::bytecode::Program) -> Vec<i32> {
    let entry = p.entry as usize;
    let len = p.code.len();
    let mut base_of = vec![0i32; len];
    for (i, f) in p.functions.iter().enumerate() {
        if i == entry || f.code_len <= 0 || f.code_start < 0 {
            continue;
        }
        let lo = (f.code_start as usize).min(len);
        let hi = (lo + f.code_len as usize).min(len);
        base_of[lo..hi].fill(f.code_start);
    }
    base_of
}

/// Every function body is a closed region: nothing outside it may jump into
/// it. The jump-over `enter_fn_frame` pushes ahead of a body exists precisely
/// to *skip* the body, and a deferral region emits all of an SCC's jump-overs
/// before any of its bodies (`[J_a, J_b, body_a, Ret, body_b, Ret]`), so a
/// jump-over patched to "just past my own `Ret`" lands in the interior of the
/// next body. Nothing else in the suite looks at code layout: `fn_shape` sees
/// only the function table, and the toplevel init overwrites the first
/// jump-over with its own `Jump base`, which hides the wrong targets behind
/// dead code.
///
/// Targets are resolved through [`frame_bases`], not read raw: a body's jumps
/// count from its own `code_start`, so an unresolved operand of `3` would look
/// like an address inside whichever body happens to start at index ≤ 3.
fn assert_no_jump_into_a_foreign_body(p: &al::bytecode::Program) {
    let entry = p.entry as usize;
    let bodies: Vec<(usize, i32, i32)> = p
        .functions
        .iter()
        .enumerate()
        // The entry's own body *is* the code the toplevel `Jump base` targets.
        .filter(|(i, f)| *i != entry && f.code_len > 0)
        .map(|(i, f)| (i, f.code_start, f.code_start + f.code_len))
        .collect();
    assert!(!bodies.is_empty(), "no bodies to guard");
    let base_of = frame_bases(p);
    // Guards the guard: at least one `Jump` must precede a body and clear it
    // entirely, or the assertion below is trivially true for want of a
    // jump-over to check.
    let mut skips = 0;
    for (pc, ins) in p.code.iter().enumerate() {
        if !matches!(ins.op, al::bytecode::Op::Jump) {
            continue;
        }
        let target = base_of[pc] + ins.operand;
        // A resolved target is a `program.code` index. If `emit` ever regresses
        // to absolute operands inside a body, `code_start + abs` overshoots the
        // stream and lands in no body at all — which the `inside || !targets`
        // check below would wave through. Pin the address space first.
        assert!(
            target >= 0 && (target as usize) < p.code.len(),
            "Jump at {pc} resolves to {target}, outside code[0, {})",
            p.code.len(),
        );
        let pc = pc as i32;
        for &(idx, start, end) in &bodies {
            let inside = pc >= start && pc < end;
            let targets = target >= start && target < end;
            assert!(
                inside || !targets,
                "Jump at {pc} targets {target} — inside function {idx} ({}), whose body is [{start}, {end})",
                p.functions[idx].name,
            );
            if pc < start && target >= end {
                skips += 1;
            }
        }
    }
    assert!(skips > 0, "no jump-over spans a function body");
}

/// A mutually-recursive SCC parks both bodies and emits them back to back, so
/// `is_even`'s jump-over must skip `is_odd`'s body too. The closure pins the
/// same for a body parked *inside* an SCC member, which is emitted ahead of
/// both of them.
#[test]
fn jump_overs_skip_every_body_parked_beside_them() {
    let ast = parse(
        r#"
fn ping(n) {
	if n == 0 {
		0
	} else {
		1 + pong(n - 1)
	}
}
fn pong(n) {
	if n == 0 {
		0
	} else {
		1 + ping(n - 1)
	}
}
fn outer(n) {
	bump = fn(x) {
		x + 1
	}
	bump(ping(n))
}
println(outer(4))
"#,
    );
    let built = al::bytecode::compile(&ast, None, Some(&al::STDLIB));
    assert!(built.success, "{:#?}", built.diagnostics);
    let built = built.emitted.expect("compile emits").program;
    assert_no_jump_into_a_foreign_body(&built);
}

/// The entry point is the last function either mode registers.
#[test]
fn check_and_compile_agree_on_the_entry_index() {
    let ast = parse("fn f(x) {\n\tx\n}\nprintln(f(1))\n");
    let built = al::bytecode::compile(&ast, None, Some(&al::STDLIB));
    let checked = al::bytecode::check(&ast, None, Some(&al::STDLIB));
    assert!(built.success && checked.success);
    let built = built.emitted.expect("compile emits").program;
    let checked = checked.emitted.expect("check registers the function table");
    assert_eq!(built.entry, checked.program.entry);
    assert_eq!(&*built.functions[built.entry as usize].name, "__main__");
}
