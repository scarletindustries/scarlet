//! Which record updates are one [`TypedExpr::Update`].
//!
//! `R(..base, f: x)` gives `base` up once it has read it, before `x` is
//! computed. An `Update` holds it until then. A program can tell when `base` was
//! all that held a value of a field that is set: a handle is closed later, and
//! an array, a map, a string or a record that computing `x` writes to is copied,
//! where it was written in place. The fields that are kept are held either way.
//!
//! So an update is made one when the fields it sets hold nothing but numbers, or
//! when computing what they are set to is [`brief`].

use scarlet_ir::core_ir::{PrimOp, ProbeOp};

use crate::typed_ir::{PatRest, TypedArm, TypedArrayElem, TypedExpr, TypedPat};

/// Whether an update of `sets` fields that keeps `kept` is cheaper as one. A
/// backend has a short way to set up to three. Past that, setting one takes
/// about what two reads of a kept field do.
pub(crate) fn pays(sets: usize, kept: usize) -> bool {
    match sets {
        0 => false,
        1..=3 => kept > 0,
        _ => kept / 2 >= sets,
    }
}

/// Whether computing `e` takes a few steps, however large the values it reads
/// are, calls nothing and writes nothing in place. Arithmetic on Ints does not,
/// because they may be big.
pub(crate) fn brief(mut e: &TypedExpr) -> bool {
    // A block is one `Let` a line, so it is walked in a loop.
    loop {
        return match e {
            TypedExpr::Let {
                init: first, body, ..
            }
            | TypedExpr::Seq {
                effect: first,
                body,
                ..
            } => {
                if !brief(first) {
                    return false;
                }
                e = body;
                continue;
            }
            TypedExpr::Const { .. } | TypedExpr::Nil { .. } | TypedExpr::Var { .. } => true,
            TypedExpr::Binary { op, lhs, rhs, .. } => brief_op(*op) && brief(lhs) && brief(rhs),
            TypedExpr::Unary { op, operand, .. } => brief_op(*op) && brief(operand),
            TypedExpr::And { lhs, rhs, .. } | TypedExpr::Or { lhs, rhs, .. } => {
                brief(lhs) && brief(rhs)
            }
            TypedExpr::Tuple { elems, .. } => elems.iter().all(brief),
            TypedExpr::TupleIndex { recv, .. } | TypedExpr::Field { recv, .. } => brief(recv),
            TypedExpr::Ctor { args, .. } => args.iter().all(brief),
            TypedExpr::Array { elems, .. } => elems.iter().all(|e| match e {
                TypedArrayElem::Elem(e) => brief(e),
                TypedArrayElem::Spread(_) => false,
            }),
            TypedExpr::Probe { op, args, .. } => *op == ProbeOp::ArrayGet && args.iter().all(brief),
            TypedExpr::Range { start, end, .. } => brief(start) && brief(end),
            TypedExpr::Closure { captures, .. } => captures.iter().all(brief),
            TypedExpr::If {
                cond, then, els, ..
            } => brief(cond) && brief(then) && brief(els),
            TypedExpr::Match { scrut, arms, .. } => brief(scrut) && arms.iter().all(brief_arm),
            TypedExpr::Update { .. }
            | TypedExpr::Interp { .. }
            | TypedExpr::BinLit { .. }
            | TypedExpr::Call { .. } => false,
        };
    }
}

fn brief_arm(arm: &TypedArm) -> bool {
    brief_pat(&arm.pat) && arm.guard.as_ref().is_none_or(brief) && brief(&arm.body)
}

fn brief_pat(pat: &TypedPat) -> bool {
    match pat {
        TypedPat::Wild { .. }
        | TypedPat::Bind(_)
        | TypedPat::Lit { .. }
        | TypedPat::Range { .. } => true,
        TypedPat::Ctor { fields: of, .. }
        | TypedPat::Tuple { elems: of, .. }
        | TypedPat::Or { alts: of, .. } => of.iter().all(brief_pat),
        TypedPat::Array { prefix, rest, .. } => match rest {
            PatRest::None | PatRest::Discard => prefix.iter().all(brief_pat),
            // What is left of the array may be cut in place.
            PatRest::Bind(_) => false,
        },
        TypedPat::Bin { .. } => false,
    }
}

fn brief_op(op: PrimOp) -> bool {
    matches!(
        op,
        PrimOp::FloatAdd
            | PrimOp::FloatSub
            | PrimOp::FloatMul
            | PrimOp::FloatDiv
            | PrimOp::FloatRem
            | PrimOp::FloatNeg
            | PrimOp::FloatLt
            | PrimOp::FloatLe
            | PrimOp::FloatGt
            | PrimOp::FloatGe
            | PrimOp::Not
    )
}

#[cfg(test)]
mod tests;
