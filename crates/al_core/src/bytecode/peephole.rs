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
//! remaining slots become `Nop`, so absolute jump targets (which the compiler
//! emits as `code_start + ip` and the VM reads back as `operand - code_start`)
//! stay valid without relocation. A window whose interior — every slot after
//! the head — is itself a jump target is left unfused: a branch landing on a
//! Nop would skip the fused effect entirely.

use std::collections::HashSet;

use super::{Instruction, Op, op, op_ab};

pub(super) fn fuse(code: &mut [Instruction]) {
    // Absolute addresses anything branches to. Only the head of a fused window
    // may be a target; landing mid-window after fusion would be incorrect.
    let mut targets: HashSet<i32> = HashSet::new();
    for instr in code.iter() {
        if instr.op.has_jump_target() {
            targets.insert(instr.operand);
        }
    }

    let nop = op(Op::Nop);
    let len = code.len();
    let mut i = 0usize;
    while i + 2 < len {
        let i0 = code[i];
        let i1 = code[i + 1];
        if i0.op != Op::PushLocal
            || i1.op != Op::PushConst
            || i0.operand as u32 > u8::MAX as u32
            || i1.operand as u32 > u16::MAX as u32
            || targets.contains(&((i + 1) as i32))
            || targets.contains(&((i + 2) as i32))
        {
            i += 1;
            continue;
        }
        let (slot, konst) = (i0.operand as u8, i1.operand as u16);
        let i2 = code[i + 2];

        // 4-wide: PushLocal; PushConst; {Lt,Eq}Int; JumpIfFalse
        if i + 3 < len && !targets.contains(&((i + 3) as i32)) {
            let i3 = code[i + 3];
            if i3.op == Op::JumpIfFalse {
                let fused = match i2.op {
                    Op::LtInt => Some(Op::JumpGeIntLC),
                    Op::EqInt => Some(Op::JumpNeIntLC),
                    _ => None,
                };
                if let Some(f) = fused {
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
