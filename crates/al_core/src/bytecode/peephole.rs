//! Peephole superinstruction fusion over the fully-emitted code stream.
//!
//! Linearly scans and rewrites the hottest local+const Int sequences into
//! single dispatches:
//!
//!   PushLocal s; PushConst k; SubInt              → SubIntLC{a:s, b:k}
//!   PushLocal s; PushConst k; AddInt              → AddIntLC{a:s, b:k}
//!   PushLocal s; PushConst k; LtInt; JumpIfFalse t → JumpGeIntLC{a:s, b:k, operand:t}
//!   PushLocal s; PushConst k; EqInt; JumpIfFalse t → JumpNeIntLC{a:s, b:k, operand:t}
//!
//! The first slot of each window is overwritten with the fused op and the
//! remaining slots become `Nop`, so jump targets stay valid without relocation:
//! nothing moves.
//!
//! Jump operands are **frame-relative** — the VM computes an instruction's
//! target as `code_start + operand`, where `code_start` belongs to the frame
//! executing it. So resolving an operand to a `program.code` index needs the
//! owning frame, and `program.functions` is the only place that says which
//! frame owns which instruction. `base_of` records that per instruction: a
//! function body's own `code_start`, and the entry frame's `code_start` for
//! everything spliced around those bodies.
//!
//! Two rules follow. A window whose interior — every slot after the head — is
//! a jump target is left unfused: a branch landing on a `Nop` would skip the
//! fused effect entirely. And a window may not straddle two frames' regions:
//! its slots' operands would live in different address spaces, and the fused
//! op inherits the last slot's operand.

use super::{Function, Instruction, Op, op, op_ab};

pub(super) fn fuse(code: &mut [Instruction], functions: &[Function]) {
    let len = code.len();

    // `Function`'s layout fields are i32 as a cross-crate constraint — the VM
    // in crates/al shares the struct, so widening them to u32 is out of scope
    // here. Negative values would mean a corrupt function table.
    debug_assert!(
        functions
            .iter()
            .all(|f| f.code_start >= 0 && f.code_len >= 0),
        "function table holds a negative code_start/code_len"
    );

    // `base_of[i]` is the `code_start` a jump operand at `code[i]` resolves
    // against. Function bodies are disjoint regions carved out of the entry
    // frame's stream, and the entry frame — which spans the whole stream, so
    // has maximal `code_len` — participates in the fill like any other frame:
    // it is stamped first and every body then overwrites its own range. No
    // instruction inside a frame ever keeps the placeholder 0, so nothing
    // depends on the entry frame starting at 0.
    //
    // `functions` may hold more than one entry frame: seeding the precompiled
    // stdlib copies that compile's whole `functions` vec, whose last element is
    // its own `__main__` spanning the entire prelude stream. Filling in
    // descending `code_len` order makes a containing region lose to every
    // region nested inside it, whatever the iteration order — so neither entry
    // frame can stamp its base over a body's.
    let mut base_of = vec![0i32; len];
    // `code_start >= 0` guards the i32 layout fields (see the debug_assert
    // above): a negative start never names a real region.
    let mut order: Vec<usize> = (0..functions.len())
        .filter(|&i| functions[i].code_len > 0 && functions[i].code_start >= 0)
        .collect();
    order.sort_by_key(|&i| std::cmp::Reverse(functions[i].code_len));
    for i in order {
        let f = &functions[i];
        let lo = (f.code_start as usize).min(len);
        let hi = (lo + f.code_len as usize).min(len);
        base_of[lo..hi].fill(f.code_start);
    }

    // Absolute `code` indices anything branches to. Only the head of a fused
    // window may be a target; landing mid-window after fusion would be
    // incorrect. Targets are dense indices into `code`, so a bitmap beats a
    // HashSet.
    let mut is_target = vec![false; len];
    for (i, instr) in code.iter().enumerate() {
        if instr.op.has_jump_target() {
            let t = base_of[i] as i64 + instr.operand as i64;
            // `SwitchTag` dispatches to `base + tag` for any tag in `0..a`:
            // every jump-table row is a live branch target, not just the base.
            let span = match instr.op {
                Op::SwitchTag => (instr.a as i64).max(1),
                _ => 1,
            };
            for t in t..t + span {
                if t >= 0 && (t as usize) < len {
                    is_target[t as usize] = true;
                }
            }
        }
    }

    let nop = op(Op::Nop);
    let mut i = 0usize;
    while i + 2 < len {
        let i0 = code[i];
        let i1 = code[i + 1];
        let base = base_of[i];
        if i0.op != Op::PushLocal
            || i1.op != Op::PushConst
            || is_target[i + 1]
            || is_target[i + 2]
            || base_of[i + 1] != base
            || base_of[i + 2] != base
        {
            i += 1;
            continue;
        }
        // The operands must pack into the fused op's a/b fields; `try_from`
        // makes the narrowing checked instead of a bounds test plus a
        // wrapping cast that could silently drift apart.
        let (Ok(slot), Ok(konst)) = (u8::try_from(i0.operand), u16::try_from(i1.operand)) else {
            i += 1;
            continue;
        };
        let i2 = code[i + 2];

        // 4-wide: PushLocal; PushConst; {Lt,Eq}Int; JumpIfFalse
        if i + 3 < len && !is_target[i + 3] && base_of[i + 3] == base {
            let i3 = code[i + 3];
            if i3.op == Op::JumpIfFalse {
                let fused = match i2.op {
                    Op::LtInt => Some(Op::JumpGeIntLC),
                    Op::EqInt => Some(Op::JumpNeIntLC),
                    _ => None,
                };
                if let Some(f) = fused {
                    // `i3.operand` is already relative to `base`, which is the
                    // fused instruction's frame too.
                    code[i] = op_ab(f, slot, konst, i3.operand);
                    code[i + 1] = nop;
                    code[i + 2] = nop;
                    code[i + 3] = nop;
                    i += 4;
                    continue;
                }
            }
        }

        // 3-wide: PushLocal; PushConst; {Add,Sub}Int
        let fused = match i2.op {
            Op::AddInt => Some(Op::AddIntLC),
            Op::SubInt => Some(Op::SubIntLC),
            _ => None,
        };
        if let Some(f) = fused {
            code[i] = op_ab(f, slot, konst, 0);
            code[i + 1] = nop;
            code[i + 2] = nop;
            i += 3;
            continue;
        }

        i += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::op_arg;

    /// A function body at `code_start = 8` holding a 4-wide fusable window at
    /// 8..12, plus a *stale* entry frame spanning the whole stream — exactly
    /// the shape `seed_static` produces, where the hydrated stdlib's own
    /// `__main__` trails every prelude body in `functions`. That frame's base
    /// must not overwrite the body's, or every operand inside the body resolves
    /// against 0 instead of 8.
    fn body_and_stale_entry(jump_operand: i32) -> (Vec<Instruction>, Vec<Function>) {
        let f = |name: &str, code_start, code_len| Function {
            name: name.into(),
            arity: 0,
            locals: 0,
            capture_count: 0,
            code_start,
            code_len,
        };
        let mut code = vec![op(Op::Nop); 8];
        code.extend([
            // body of `f` starts here (code_start = 8)
            op_arg(Op::PushLocal, 0),
            op_arg(Op::PushConst, 0),
            op(Op::LtInt),
            op_arg(Op::JumpIfFalse, 4), // → 8 + 4 = 12
            op(Op::Nop),
            op_arg(Op::Jump, jump_operand),
            op(Op::Nop),
            op(Op::Nop),
        ]);
        let functions = vec![
            f("f", 8, 8),
            // the precompiled stdlib's own entry frame, still in `functions`
            f("__main__", 0, 16),
            // this compile's entry frame
            f("__main__", 0, 16),
        ];
        (code, functions)
    }

    #[test]
    fn stale_entry_frame_does_not_clobber_body_bases() {
        // `Jump` operand 2 → target `8 + 2 = 10`, the window's interior. Read
        // against a base of 0 it would point at `code[2]` and the interior
        // would look unreachable.
        let (mut code, functions) = body_and_stale_entry(2);
        fuse(&mut code, &functions);
        assert_eq!(
            code[8].op,
            Op::PushLocal,
            "window whose interior is a branch target must stay unfused"
        );
        assert_eq!(code[10].op, Op::LtInt);
    }

    #[test]
    fn switch_tag_table_rows_are_branch_targets() {
        // `SwitchTag` with 3 rows at 2..5 dispatches to `2 + tag` — every row
        // is a live branch target. The fusable window at 3..6 overlaps rows 1
        // and 2; fusing it would leave a `Nop` where a dispatch lands.
        let mut code = vec![
            op_ab(Op::SwitchTag, 3, 0, 2),
            op(Op::Nop),
            op(Op::Nop),
            op_arg(Op::PushLocal, 0),
            op_arg(Op::PushConst, 0),
            op(Op::AddInt),
            op(Op::Nop),
        ];
        let functions = vec![Function {
            name: "__main__".into(),
            arity: 0,
            locals: 0,
            capture_count: 0,
            code_start: 0,
            code_len: code.len() as i32,
        }];
        fuse(&mut code, &functions);
        assert_eq!(
            code[3].op,
            Op::PushLocal,
            "window overlapping a jump-table row must stay unfused"
        );
        assert_eq!(code[4].op, Op::PushConst);
        assert_eq!(code[5].op, Op::AddInt);
    }

    #[test]
    fn body_window_still_fuses_when_no_interior_target() {
        // `Jump` operand 6 → target `8 + 6 = 14`, outside the window.
        let (mut code, functions) = body_and_stale_entry(6);
        fuse(&mut code, &functions);
        assert_eq!(code[8].op, Op::JumpGeIntLC);
        assert_eq!(code[8].operand, 4, "fused operand stays frame-relative");
        assert_eq!(code[9].op, Op::Nop);
        assert_eq!(code[10].op, Op::Nop);
        assert_eq!(code[11].op, Op::Nop);
    }
}
