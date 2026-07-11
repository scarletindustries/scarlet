//! Source binary operator → VM opcode selection.
//!
//! Codegen-side, so it may lean on the AST and the inferencer; `bytecode::mod`
//! stays a pure instruction-set definition and re-exports this.

use crate::ast::BinaryOp;
use crate::bytecode::Op;
use crate::types::Prim;

/// A binary operator that denotes an *opcode*.
///
/// `&&`/`||` are control flow, not operators: they branch, so the right operand
/// may never be evaluated. They are absent from this enum, which is why
/// [`specialize_binop`] returns an `Op` rather than an `Option<Op>` — there is
/// no operator left for it to have no opcode for. The only way to obtain one is
/// [`BinopKind::of`], which routes the two short-circuiting forms to
/// [`ShortCircuitOp`] instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// The two branching binary forms. Their operands are a condition and a branch
/// arm, not two values, so the elaborator builds `TypedExpr::And`/`Or` from
/// them rather than a `TypedExpr::Binary`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortCircuitOp {
    And,
    Or,
}

/// Which of the two a source [`BinaryOp`] is. Total, and the sole constructor
/// of [`ArithOp`]: a caller that has one has already proven it is not `&&`/`||`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinopKind {
    Arith(ArithOp),
    ShortCircuit(ShortCircuitOp),
}

impl BinopKind {
    pub fn of(op: BinaryOp) -> BinopKind {
        use BinaryOp as B;
        match op {
            B::And => BinopKind::ShortCircuit(ShortCircuitOp::And),
            B::Or => BinopKind::ShortCircuit(ShortCircuitOp::Or),
            B::Add => BinopKind::Arith(ArithOp::Add),
            B::Sub => BinopKind::Arith(ArithOp::Sub),
            B::Mul => BinopKind::Arith(ArithOp::Mul),
            B::Div => BinopKind::Arith(ArithOp::Div),
            B::Mod => BinopKind::Arith(ArithOp::Mod),
            B::Eq => BinopKind::Arith(ArithOp::Eq),
            B::Ne => BinopKind::Arith(ArithOp::Ne),
            B::Lt => BinopKind::Arith(ArithOp::Lt),
            B::Le => BinopKind::Arith(ArithOp::Le),
            B::Gt => BinopKind::Arith(ArithOp::Gt),
            B::Ge => BinopKind::Arith(ArithOp::Ge),
        }
    }
}

/// Pick the opcode for an operator, specialized to the operand's prim when
/// inference has resolved one. An unresolved (still polymorphic) operand keeps
/// the generic op so the VM's tag-dispatching path handles it. Core IR
/// elaboration is the only caller — it is the sole bytecode emit route — so this
/// is where a new specialization belongs.
///
/// Total: every `(ArithOp, Option<Prim>)` names an opcode.
pub fn specialize_binop(op: ArithOp, prim: Option<Prim>) -> Op {
    use ArithOp as A;
    match (op, prim) {
        (A::Add, Some(Prim::Int)) => Op::AddInt,
        (A::Add, Some(Prim::Float)) => Op::AddFloat,
        (A::Add, Some(Prim::String)) => Op::AddStr,
        (A::Add, _) => Op::Add,
        (A::Sub, Some(Prim::Int)) => Op::SubInt,
        (A::Sub, Some(Prim::Float)) => Op::SubFloat,
        (A::Sub, _) => Op::Sub,
        (A::Mul, Some(Prim::Int)) => Op::MulInt,
        (A::Mul, Some(Prim::Float)) => Op::MulFloat,
        (A::Mul, _) => Op::Mul,
        (A::Div, Some(Prim::Int)) => Op::DivInt,
        (A::Div, Some(Prim::Float)) => Op::DivFloat,
        (A::Div, _) => Op::Div,
        (A::Mod, Some(Prim::Int)) => Op::ModInt,
        (A::Mod, _) => Op::Mod,
        (A::Eq, Some(Prim::Int)) => Op::EqInt,
        (A::Eq, _) => Op::Eq,
        (A::Ne, Some(Prim::Int)) => Op::NeqInt,
        (A::Ne, _) => Op::Neq,
        (A::Lt, Some(Prim::Int)) => Op::LtInt,
        (A::Lt, Some(Prim::Float)) => Op::LtFloat,
        (A::Lt, _) => Op::Lt,
        (A::Le, Some(Prim::Int)) => Op::LteInt,
        (A::Le, Some(Prim::Float)) => Op::LteFloat,
        (A::Le, _) => Op::Lte,
        (A::Gt, Some(Prim::Int)) => Op::GtInt,
        (A::Gt, Some(Prim::Float)) => Op::GtFloat,
        (A::Gt, _) => Op::Gt,
        (A::Ge, Some(Prim::Int)) => Op::GteInt,
        (A::Ge, Some(Prim::Float)) => Op::GteFloat,
        (A::Ge, _) => Op::Gte,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The classification is total and the two short-circuiting operators are
    /// the only ones that leave the `Arith` side — so `specialize_binop` can
    /// never be reached with an operator that has no opcode.
    #[test]
    fn every_binary_op_classifies_and_only_and_or_short_circuit() {
        use BinaryOp as B;
        let all = [
            B::Add,
            B::Sub,
            B::Mul,
            B::Div,
            B::Mod,
            B::Eq,
            B::Ne,
            B::Lt,
            B::Le,
            B::Gt,
            B::Ge,
            B::And,
            B::Or,
        ];
        for op in all {
            match BinopKind::of(op) {
                BinopKind::ShortCircuit(_) => assert!(matches!(op, B::And | B::Or)),
                BinopKind::Arith(a) => {
                    assert!(!matches!(op, B::And | B::Or));
                    // Total over every prim, including "unresolved".
                    for prim in [None, Some(Prim::Int), Some(Prim::Float), Some(Prim::String)] {
                        let _: Op = specialize_binop(a, prim);
                    }
                }
            }
        }
    }
}
