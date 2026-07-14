//! `al dis` — the emitted bytecode, as text.
//!
//! A debugging view, not a stable format: it renders whatever the compiler
//! just produced, and nothing consumes it. It lives in the binary crate rather
//! than `al_core` for that reason — a disassembler is not part of the
//! compiler's contract with the VM.
//!
//! Operand meaning is *not* recovered from a table of opcode names. A table
//! rots the moment an opcode is added, and silently: the new op renders with a
//! misread operand and nobody notices. The only classification used here is
//! [`Op::has_jump_target`], whose wildcard-free match already fails to compile
//! until a new opcode is placed on one side of it. Everything else prints the
//! raw operand, which is honest.

use std::fmt::Write as _;

use crate::bytecode::{Function, Instruction, Op, Program};
use crate::core_ir::clif::{self, NativePlan};
use crate::tivec::Idx as _;
use crate::vm::inspect;
use crate::vm::jit;

/// Column at which the `;` comment starts.
/// Render `program` as text: a header, each function's code, then the
/// constant pool.
pub fn disassemble(program: &Program) -> String {
    render(program, None)
}

/// Only functions whose name contains `needle`.
pub fn disassemble_fn(program: &Program, needle: &str) -> String {
    render(program, Some(needle))
}

/// `--native <fn>`: the bytecode listing for functions matching `needle`,
/// each compiled body followed by its CLIF and finalized machine-code size.
///
/// `plans` are the hook-captured [`NativePlan`]s for every body the
/// `AL_NATIVE` mode selected and the A0 gate admitted — so this renders what
/// the runtime would actually compile, not a hypothetical. A matching
/// bytecode function without a plan gets a note instead of a listing. The
/// code size comes from defining the body into a real host-ISA module (the
/// same flags the runtime JIT uses), so it is the size of the machine code
/// the VM would execute; nothing is finalized or run here.
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

    // Functions are emitted in compilation order, but nothing promises their
    // code is contiguous or ordered, so index by `code_start` explicitly.
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
/// All three fields print, always. Suppressing a zero would lie: `PushLocal`
/// reads slot 0, `SubIntLC` reads local `a=0`. Which fields an opcode *uses* is
/// knowable only from a table of opcode names, and such a table rots silently
/// the moment an opcode is added — so this prints the instruction word as it
/// is, and lets the reader consult `Op`'s docs.
///
/// `ip` is function-relative, which is also what a jump operand holds: the VM
/// assigns it straight to `ip`. A jump therefore renders as a function-relative
/// target and a delta, never as an absolute address. None exists.
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
        // A target outside the function is only reachable through a bug — a
        // jump is intra-function by construction. `emit` does leave one at
        // exactly `code_len` when both arms of an `if` return: the merge point
        // is unreachable, so nothing ever lands there.
        if instr.operand < 0 || instr.operand > code_len {
            let _ = write!(comment, "  !! outside 0..{code_len}");
        } else if instr.operand == code_len {
            let _ = write!(comment, "  (merge, unreachable)");
        }
    } else if instr.op == Op::PushConst
        && let Some(c) = program.constants.get(instr.operand as usize)
    {
        // The one operand whose meaning needs no table: the VM indexes
        // `constants` with it directly.
        let _ = write!(comment, "{}", inspect(c, program));
    }

    if comment.is_empty() {
        return s.trim_end().to_string();
    }
    let _ = write!(s, "; {comment}");
    s
}
