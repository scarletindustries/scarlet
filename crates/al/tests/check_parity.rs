//! `al check` and `al build` must agree on the shape of the program they
//! elaborate: same functions, same indices, same capture counts.
//!
//! `check_only` truncates the pipeline in one place (`elaborate_body` returns
//! before `perceus`/`emit`) and elides the toplevel init and the peephole
//! pass. Everything upstream — including which `Function` slot a body owns —
//! is mode-independent, which is what these tests pin.

mod common;
use common::parse;

/// `(name, arity, capture_count)`, one per registered function.
type FnShape = Vec<(String, i32, i32)>;

/// Every registered function's `(name, arity, capture_count)`, in index
/// order. `code_start`/`code_len`/`locals` are excluded: those are the emit
/// half, which `check` legitimately leaves empty.
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
        built.success(),
        "compile failed:\n{source}\n{:#?}",
        built.diagnostics
    );
    let checked = al::bytecode::check(&ast, None, Some(&al::STDLIB));
    assert!(
        checked.success(),
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

/// A nested closure's `func_idx` is what `Atom::Closure` carries, so both
/// modes must reserve the lambda's slot identically.
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
    assert!(
        built.iter().any(|(_, _, caps)| *caps == 1),
        "no capturing function registered: {built:?}"
    );
}

/// An eta-wrapper (`Some` used as a value) is synthesised by `lower`, which
/// runs in both modes, so its `Function` entry exists in both at one index.
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

/// Mutual recursion parks both bodies in one SCC. Their reserved slots must
/// survive that round trip identically in both modes.
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
/// against. Jump operands are frame-relative, so a bare `ins.operand` is not a
/// `program.code` index; this table turns it into one. Code spliced around the
/// bodies (jump-overs, the toplevel init) runs in the entry frame, base 0.
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

/// Every function body is a closed region: nothing outside it may jump in. A
/// deferral region emits all of an SCC's jump-overs before any of its bodies
/// (`[J_a, J_b, body_a, Ret, body_b, Ret]`), so a jump-over patched to "just
/// past my own `Ret`" lands inside the next body. Nothing else in the suite
/// looks at code layout, and the toplevel init overwrites the first jump-over,
/// hiding wrong targets behind dead code.
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
    // Guards the guard: without at least one jump-over spanning a body the
    // assertion below is trivially true.
    let mut skips = 0;
    for (pc, ins) in p.code.iter().enumerate() {
        if !matches!(ins.op, al::bytecode::Op::Jump) {
            continue;
        }
        let target = base_of[pc] + ins.operand;
        // Pin the address space first: an absolute operand inside a body would
        // overshoot the stream and land in no body, which the check below would
        // wave through.
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

/// An SCC emits both bodies back to back, so `ping`'s jump-over must skip
/// `pong`'s body too. The closure pins the same for a body parked inside an
/// SCC member, emitted ahead of both.
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
    assert!(built.success(), "{:#?}", built.diagnostics);
    let built = built.emitted.expect("compile emits").program;
    assert_no_jump_into_a_foreign_body(&built);
}

/// The entry point is the last function either mode registers.
#[test]
fn check_and_compile_agree_on_the_entry_index() {
    let ast = parse("fn f(x) {\n\tx\n}\nprintln(f(1))\n");
    let built = al::bytecode::compile(&ast, None, Some(&al::STDLIB));
    let checked = al::bytecode::check(&ast, None, Some(&al::STDLIB));
    assert!(built.success() && checked.success());
    let built = built.emitted.expect("compile emits").program;
    let checked = checked.emitted.expect("check registers the function table");
    assert_eq!(built.entry, checked.program.entry);
    assert_eq!(&*built.functions[built.entry as usize].name, "__main__");
}
