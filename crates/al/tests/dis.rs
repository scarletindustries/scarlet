//! `al dis` — the compiled bytecode as text.
//!
//! The property that matters is that every jump target lands inside its own
//! function. Jump operands are function-relative (the VM assigns one straight
//! to `ip`), so an out-of-range target means `emit` produced a jump into
//! another function's code.

mod common;

use al::bytecode::{self, Op};
use al::dis;
use al::{STDLIB, ast, parser, scanner};

fn program_of(src: &str) -> bytecode::Program {
    let mut sc = scanner::new_scanner(src.to_string());
    let p = parser::new_parser(&mut sc);
    let parsed = p.parse_program();
    let expr = ast::Expression::BlockExpression(parsed.ast);
    let r = bytecode::compile(&expr, None, Some(&STDLIB));
    assert!(r.success(), "compile failed: {:?}", r.diagnostics);
    r.emitted.expect("a successful compile emits").program
}

const FACT: &str =
    "fn fact(n Int) Int {\n\tif n < 2 { 1 } else { n * fact(n - 1) }\n}\n\nprintln(fact(5))\n";

#[test]
fn a_function_disassembles_with_its_header_and_body() {
    let p = program_of(FACT);
    let text = dis::disassemble_fn(&p, "fact");
    assert!(text.contains("fact (arity 1, locals 2)"), "{text}");
    assert!(
        text.contains("CallSelf"),
        "the self-recursive call:\n{text}"
    );
    assert!(text.contains("Ret"), "{text}");
}

/// A `PushConst` operand indexes `constants` directly, so it renders its value.
#[test]
fn a_constant_push_shows_its_value() {
    let p = program_of(FACT);
    let text = dis::disassemble_fn(&p, "fact");
    let line = text
        .lines()
        .find(|l| l.contains("PushConst"))
        .unwrap_or_else(|| panic!("no PushConst:\n{text}"));
    assert!(line.contains("; 1"), "constant not rendered: {line}");
}

/// Zero is a real slot and a real constant id. Suppressing it would make
/// `PushLocal` (slot 0) look like it had no operand at all.
#[test]
fn a_zero_operand_is_printed_not_hidden() {
    let p = program_of(FACT);
    let text = dis::disassemble_fn(&p, "fact");
    assert!(
        text.lines()
            .any(|l| l.contains("PushLocal") && l.contains("op=0")),
        "a zero operand must print:\n{text}"
    );
}

/// `code_len` itself is the merge point of an `if` whose arms both return, so
/// it is a legal target and never executed.
#[test]
fn every_jump_target_is_inside_its_own_function() {
    for src in [
        FACT,
        "fn pick(n Int) Int {\n\tm = if n < 2 { 1 } else { 2 }\n\tm + 1\n}\nprintln(pick(5))\n",
        "import al/array\ntype W {\n\tW(v Int)\n}\nfn go(xs Array(Int)) Int {\n\tws = array.map(xs, W)\n\tif array.length(ws) > 2 { 111 } else { 222 }\n}\nprintln(go([1]))\n",
        "fn cls(n Int) Int {\n\tmatch n {\n\t\t0 -> 1\n\t\t1 -> 2\n\t\telse -> 3\n\t}\n}\nprintln(cls(1))\n",
    ] {
        let p = program_of(src);
        for f in &p.functions {
            let start = f.code_start as usize;
            let end = start + f.code_len as usize;
            for (ip, instr) in p.code[start..end].iter().enumerate() {
                if !instr.op.has_jump_target() {
                    continue;
                }
                assert!(
                    instr.operand >= 0 && instr.operand <= f.code_len,
                    "fn {} @{ip}: {:?} jumps to {} outside 0..{}",
                    f.name,
                    instr.op,
                    instr.operand,
                    f.code_len
                );
            }
        }
    }
}

/// `--fn` exists because the stdlib compiles into the same program: without it
/// a two-line file disassembles 160-odd functions.
#[test]
fn the_fn_filter_selects_one_function() {
    let p = program_of(FACT);
    let all = dis::disassemble(&p);
    let one = dis::disassemble_fn(&p, "fact");
    assert!(
        all.len() > one.len() * 10,
        "the filter must actually filter"
    );
    assert!(one.contains("fact"), "{one}");
    assert!(
        !one.contains("constants:"),
        "a filtered dump omits the pool"
    );
}

/// The whole program renders, including the constant pool.
#[test]
fn the_full_dump_carries_a_constant_pool() {
    let p = program_of(FACT);
    let text = dis::disassemble(&p);
    assert!(text.starts_with("; "), "header first:\n{}", &text[..80]);
    assert!(text.contains("\nconstants:\n"), "no constant pool");
    assert!(text.contains("c0 = "), "no constant rendered");
    let _ = Op::PushConst; // the op the pool exists for
}

/// Two literals of the same big int must share one pool entry. Dedup must not
/// key on `Value::to_bits()`, a *pointer* for a boxed int (outside ±2^47).
///
/// The fixture value must not collide with a stdlib constant: a hydrated
/// constant points into the static frozen area and a fresh literal into the
/// program's builder, so those two are acceptably distinct allocations.
#[test]
fn a_big_int_constant_is_pooled_once() {
    let p = program_of("a = 4611686018427387905\nb = 4611686018427387905\nprintln(a == b)\n");
    let hits = p
        .constants
        .iter()
        .filter(|c| c.as_int() == Some(4611686018427387905))
        .count();
    assert_eq!(hits, 1, "one value, one pool entry");
    // The whole pool stays the stdlib's worth of constants, not one entry per
    // constructor use site.
    assert!(
        p.constants.len() < 600,
        "pool regressed to per-use-site duplicates: {} entries",
        p.constants.len()
    );
}

/// The size gate for join-point match lowering: a fallible match shares one
/// fallthrough continuation across its failure edges instead of re-lowering
/// every remaining arm at each edge. Without sharing the duplication is
/// multiplicative on binary patterns — `to_method` alone compiled to 48,100
/// instructions and this program to 72,333.
#[test]
fn bench_service_compiles_without_match_arm_duplication() {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/bench_service.al");
    let src =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let p = program_of(&src);
    assert!(
        p.code.len() < 30_000,
        "bench_service compiled to {} instructions (gate: < 30,000); \
         fallible-match lowering is duplicating arm bodies per failure edge",
        p.code.len()
    );
    let to_method = p
        .functions
        .iter()
        .find(|f| &*f.name == "to_method")
        .expect("bench_service imports al/http, which defines to_method");
    assert!(
        to_method.code_len < 500,
        "to_method compiled to {} instructions (expected ~150-250); its \
         binary match is re-lowering the remaining arms at every failure edge",
        to_method.code_len
    );
}

/// One function's instructions as `(op, operand)` pairs. Constant-pool
/// operands are program-absolute and shift with the stdlib, so callers blank
/// them; jump operands are function-relative and comparable as-is.
fn fn_instructions(p: &bytecode::Program, name: &str) -> Vec<(Op, i32)> {
    // Last match: user functions are emitted after the stdlib, so a stdlib
    // function of the same name cannot shadow the fixture's.
    let f = p
        .functions
        .iter()
        .rev()
        .find(|f| &*f.name == name)
        .unwrap_or_else(|| panic!("no function {name}"));
    p.code[f.code_start as usize..(f.code_start + f.code_len) as usize]
        .iter()
        .map(|i| (i.op, i.operand))
        .collect()
}

/// A match that cannot fail keeps the flat lowering byte-for-byte: no
/// continuation blocks, no failure-edge `Goto`s, because no failure edge
/// exists. A `LetCont` sneaking in would append a block or reroute an edge,
/// and either breaks these literal sequences.
#[test]
fn an_infallible_match_keeps_the_flat_lowering() {
    use Op::*;
    let p = program_of(
        "type Shape {\n\
         \tCircle(r Int)\n\
         \tRect(w Int, h Int)\n\
         }\n\
         fn area(s Shape) Int {\n\
         \tmatch s {\n\
         \t\tCircle(r) -> r * r * 3\n\
         \t\tRect(w, h) -> w * h\n\
         \t}\n\
         }\n\
         fn single(p (Int, Int)) Int {\n\
         \tmatch p {\n\
         \t\t(a, b) -> a + b\n\
         \t}\n\
         }\n\
         fn lits(n Int) String {\n\
         \tmatch n {\n\
         \t\t1 -> 'one'\n\
         \t\t2 -> 'two'\n\
         \t\telse -> 'many'\n\
         \t}\n\
         }\n\
         println(area(Circle(2)) + single((1, 2)))\n\
         println(lits(1))\n",
    );
    // Constant-pool indices move with the stdlib; blank them.
    let blanked = |name: &str| {
        fn_instructions(&p, name)
            .into_iter()
            .map(|(op, operand)| (op, if op == PushConst { -1 } else { operand }))
            .collect::<Vec<_>>()
    };

    // Exhaustive per-variant enum match: switch, jump table, each arm once.
    assert_eq!(
        blanked("area"),
        vec![
            (PushLocal, 0),
            (SwitchTag, 2), // table base: right after the switch
            (Jump, 4),      // tag 0 -> Circle arm
            (Jump, 14),     // tag 1 -> Rect arm
            (PushLocal, 0),
            (UnwrapEnum, 0),
            (StoreLocal, 1),
            (Drop, 0),
            (PushLocal, 1),
            (PushLocal, 1),
            (MulInt, 0),
            (PushConst, -1),
            (MulInt, 0),
            (Ret, 0),
            (PushLocal, 0),
            (UnwrapEnum, 0),
            (StoreLocal, 2),
            (StoreLocal, 1),
            (Drop, 0),
            (PushLocal, 1),
            (PushLocal, 2),
            (MulInt, 0),
            (Ret, 0),
        ]
    );

    // Single irrefutable arm: straight-line projections, no branch.
    assert_eq!(
        blanked("single"),
        vec![
            (PushLocal, 0),
            (TupleIndex, 0),
            (StoreLocal, 1),
            (PushLocal, 0),
            (TupleIndex, 1),
            (StoreLocal, 2),
            (Drop, 0),
            (PushLocal, 1),
            (PushLocal, 2),
            (AddInt, 0),
            (Ret, 0),
            (Halt, 0),
        ]
    );

    // Literal ladder: a head miss falls to the next arm's own test, with no
    // continuation minted for it.
    assert_eq!(
        blanked("lits"),
        vec![
            (PushLocal, 0),
            (PushConst, -1),
            (Eq, 0),
            (JumpIfFalse, 6),
            (PushConst, -1),
            (Ret, 0),
            (PushLocal, 0),
            (PushConst, -1),
            (Eq, 0),
            (JumpIfFalse, 12),
            (PushConst, -1),
            (Ret, 0),
            (PushConst, -1),
            (Ret, 0),
            (Halt, 0),
        ]
    );
}
