//! `al dis` — the compiled bytecode as text.
//!
//! The interesting test here is not the formatting: it is that every jump
//! target lands inside its own function. Jump operands are function-relative
//! (the VM assigns one straight to `ip`), and `Core`'s control flow is
//! structured, so an out-of-range target would mean `emit` had produced a jump
//! into another function's code — which is exactly the miscompile that
//! function-relative operands exist to make unrepresentable.

mod common;

use al::bytecode::{self, Op};
use al::dis;
use al::{STDLIB, ast, parser, scanner};

fn program_of(src: &str) -> bytecode::Program {
    let mut sc = scanner::new_scanner(src.to_string());
    let mut p = parser::new_parser(&mut sc);
    let parsed = p.parse_program();
    let expr = ast::Expression::BlockExpression(parsed.ast);
    let r = bytecode::compile(&expr, None, Some(&STDLIB));
    assert!(r.success, "compile failed: {:?}", r.diagnostics);
    r.program
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

/// A `PushConst` operand indexes `constants` directly — the one operand whose
/// meaning needs no per-opcode table — so it renders its value.
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

/// The property that matters. Every jump operand is an `ip` within its own
/// function; `code_len` itself is the merge point of an `if` whose arms both
/// return, and is never executed.
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

/// Two literals of the same big int must share one pool entry. The dedup used
/// to key on `Value::to_bits()` — a *pointer* for a boxed int (outside ±2^47)
/// — so every use site pooled a fresh copy: 92% of a compiled program's
/// constant pool was duplicate enum-variant hashes.
///
/// The value must not collide with a stdlib constant: a hydrated constant
/// points into the *static* frozen area and a fresh literal into the
/// program's builder, so those two are (acceptably) distinct allocations —
/// one duplicate per value that appears on both sides, bounded by distinct
/// values rather than use sites.
#[test]
fn a_big_int_constant_is_pooled_once() {
    let p = program_of("a = 4611686018427387905\nb = 4611686018427387905\nprintln(a == b)\n");
    let hits = p
        .constants
        .iter()
        .filter(|c| c.as_int() == Some(4611686018427387905))
        .count();
    assert_eq!(hits, 1, "one value, one pool entry");
    // And the whole pool is small now: the stdlib's worth of constants, not
    // one entry per constructor *use site*.
    assert!(
        p.constants.len() < 600,
        "pool regressed to per-use-site duplicates: {} entries",
        p.constants.len()
    );
}
