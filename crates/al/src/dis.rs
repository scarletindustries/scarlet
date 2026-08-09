//! `al dis` — the emitted bytecode, as text. A debugging view, not a stable
//! format.
//!
//! Operands are printed raw rather than decoded per opcode. The only
//! classification used is [`Op::has_jump_target`], which has no wildcard arm,
//! so a new opcode fails to compile until it is placed.

use std::fmt::Write as _;

use crate::bytecode::{Function, Instruction, Op, Program};
use crate::core_ir::clif::{self, NativePlan};
use crate::tivec::Idx as _;
use crate::vm::inspect;
use crate::vm::jit;

/// Render `program` as text: header, each function's code, then constants.
pub fn disassemble(program: &Program) -> String {
    render(program, None)
}

/// Only functions whose name contains `needle`.
pub fn disassemble_fn(program: &Program, needle: &str) -> String {
    render(program, Some(needle))
}

/// `--native <fn>`: the bytecode listing for functions matching `needle`, each
/// compiled body followed by its CLIF and machine-code size.
///
/// `plans` are the [`NativePlan`]s the runtime would actually compile. Bodies
/// are defined into a real host-ISA module to get the size, but nothing is
/// finalized or run.
pub fn disassemble_native(
    program: &Program,
    needle: &str,
    plans: Vec<NativePlan>,
) -> Result<String, String> {
    let mut out = render(program, Some(needle));
    let mut module = jit::jit_module().map_err(|e| e.to_string())?;
    let native = clif::native_set(&plans, program);
    let mut listed = false;
    for plan in plans {
        let idx = plan.func_idx.index();
        let Some(f) = program.functions.get(idx) else {
            continue;
        };
        if !f.name.contains(needle) {
            continue;
        }
        match clif::compile(&mut module, &plan, &native, program) {
            Ok(Some(body)) => {
                let _ = writeln!(
                    out,
                    "\nnative fn#{idx} {} ({} bytes)",
                    f.name, body.code_size
                );
                let _ = write!(out, "{}", body.clif);
                if !body.clif.ends_with('\n') {
                    let _ = writeln!(out);
                }
            }
            Ok(None) => {
                let _ = writeln!(
                    out,
                    "\n; native fn#{idx} {}: not compiled (a constant or match arm is outside coverage)",
                    f.name
                );
            }
            Err(e) => return Err(format!("native compile of {} failed: {e}", f.name)),
        }
        listed = true;
    }
    if !listed {
        let _ = writeln!(
            out,
            "\n; no native body matches {needle:?} (not covered, or excluded by AL_NATIVE)"
        );
    }
    Ok(out)
}

fn render(program: &Program, only: Option<&str>) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "; {} functions, {} instructions, {} constants, entry = fn#{}",
        program.functions.len(),
        program.code.len(),
        program.constants.len(),
        program.entry,
    );

    // Nothing promises function bodies are contiguous or in index order.
    let mut fns: Vec<(usize, &Function)> = program.functions.iter().enumerate().collect();
    fns.sort_by_key(|(_, f)| f.code_start);

    for (idx, f) in fns {
        if let Some(n) = only
            && !f.name.contains(n)
        {
            continue;
        }
        let _ = writeln!(out);
        let _ = write!(
            out,
            "fn#{idx} {} (arity {}, locals {}",
            f.name, f.arity, f.locals
        );
        if f.capture_count > 0 {
            let _ = write!(out, ", captures {}", f.capture_count);
        }
        let _ = writeln!(out, ") @{}..{}", f.code_start, f.code_start + f.code_len);

        let start = f.code_start as usize;
        let end = (f.code_start + f.code_len) as usize;
        for (ip, instr) in program.code[start.min(program.code.len())..end.min(program.code.len())]
            .iter()
            .enumerate()
        {
            let _ = writeln!(out, "{}", line(program, ip as i32, instr, f.code_len));
        }
    }

    if only.is_none() && !program.constants.is_empty() {
        let _ = writeln!(out, "\nconstants:");
        for (i, c) in program.constants.iter().enumerate() {
            let _ = writeln!(out, "  c{i} = {}", inspect(c, program));
        }
    }
    out
}

/// One instruction: `  0007  JumpIfFalse   a=0 b=0 op=4    ; -> 0004 (+4)`.
///
/// All three fields always print, including zeros, since which ones an opcode
/// uses is not knowable here. `ip` and jump operands are function-relative;
/// there is no absolute address to print.
fn line(program: &Program, ip: i32, instr: &Instruction, code_len: i32) -> String {
    let fields = format!("a={} b={} op={}", instr.a, instr.b, instr.operand);
    let mut s = format!(
        "  {:04}  {:<16}{:<22}",
        ip,
        format!("{:?}", instr.op),
        fields
    );

    let mut comment = String::new();
    if instr.op.has_jump_target() {
        let _ = write!(
            comment,
            "-> {:04} ({:+})",
            instr.operand,
            instr.operand - ip
        );
        // Jumps are intra-function, so an out-of-range target is a bug. The one
        // legal edge case is exactly `code_len`: `emit` leaves that when both
        // arms of an `if` return and the merge point is unreachable.
        if instr.operand < 0 || instr.operand > code_len {
            let _ = write!(comment, "  !! outside 0..{code_len}");
        } else if instr.operand == code_len {
            let _ = write!(comment, "  (merge, unreachable)");
        }
    } else if instr.op == Op::PushConst
        && let Some(c) = program.constants.get(instr.operand as usize)
    {
        let _ = write!(comment, "{}", inspect(c, program));
    }

    if comment.is_empty() {
        return s.trim_end().to_string();
    }
    let _ = write!(s, "; {comment}");
    s
}
