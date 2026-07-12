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
pub enum ValueBinop {
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
/// of [`ValueBinop`]: a caller that has one has already proven it is not `&&`/`||`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinopKind {
    Value(ValueBinop),
    ShortCircuit(ShortCircuitOp),
}

impl BinopKind {
    pub fn of(op: BinaryOp) -> BinopKind {
        use BinaryOp as B;
        match op {
            B::And => BinopKind::ShortCircuit(ShortCircuitOp::And),
            B::Or => BinopKind::ShortCircuit(ShortCircuitOp::Or),
            B::Add => BinopKind::Value(ValueBinop::Add),
            B::Sub => BinopKind::Value(ValueBinop::Sub),
            B::Mul => BinopKind::Value(ValueBinop::Mul),
            B::Div => BinopKind::Value(ValueBinop::Div),
            B::Mod => BinopKind::Value(ValueBinop::Mod),
            B::Eq => BinopKind::Value(ValueBinop::Eq),
            B::Ne => BinopKind::Value(ValueBinop::Ne),
            B::Lt => BinopKind::Value(ValueBinop::Lt),
            B::Le => BinopKind::Value(ValueBinop::Le),
            B::Gt => BinopKind::Value(ValueBinop::Gt),
            B::Ge => BinopKind::Value(ValueBinop::Ge),
        }
    }
}

/// Pick the opcode for an operator, specialized to the operand's prim when
/// inference has resolved one. An unresolved (still polymorphic) operand keeps
/// the generic op so the VM's tag-dispatching path handles it. Typed IR
/// elaboration (`typed_ir::elaborate`) is the only caller — it is the sole
/// route from source operators to opcodes — so this is where a new
/// specialization belongs.
///
/// Total: every `(ValueBinop, Option<Prim>)` names an opcode.
pub fn specialize_binop(op: ValueBinop, prim: Option<Prim>) -> Op {
    use ValueBinop as V;
    match (op, prim) {
        (V::Add, Some(Prim::Int)) => Op::AddInt,
        (V::Add, Some(Prim::Float)) => Op::AddFloat,
        (V::Add, Some(Prim::String)) => Op::AddStr,
        (V::Add, _) => Op::Add,
        (V::Sub, Some(Prim::Int)) => Op::SubInt,
        (V::Sub, Some(Prim::Float)) => Op::SubFloat,
        (V::Sub, _) => Op::Sub,
        (V::Mul, Some(Prim::Int)) => Op::MulInt,
        (V::Mul, Some(Prim::Float)) => Op::MulFloat,
        (V::Mul, _) => Op::Mul,
        (V::Div, Some(Prim::Int)) => Op::DivInt,
        (V::Div, Some(Prim::Float)) => Op::DivFloat,
        (V::Div, _) => Op::Div,
        (V::Mod, Some(Prim::Int)) => Op::ModInt,
        (V::Mod, _) => Op::Mod,
        (V::Eq, Some(Prim::Int)) => Op::EqInt,
        (V::Eq, _) => Op::Eq,
        (V::Ne, Some(Prim::Int)) => Op::NeqInt,
        (V::Ne, _) => Op::Neq,
        (V::Lt, Some(Prim::Int)) => Op::LtInt,
        (V::Lt, Some(Prim::Float)) => Op::LtFloat,
        (V::Lt, _) => Op::Lt,
        (V::Le, Some(Prim::Int)) => Op::LteInt,
        (V::Le, Some(Prim::Float)) => Op::LteFloat,
        (V::Le, _) => Op::Lte,
        (V::Gt, Some(Prim::Int)) => Op::GtInt,
        (V::Gt, Some(Prim::Float)) => Op::GtFloat,
        (V::Gt, _) => Op::Gt,
        (V::Ge, Some(Prim::Int)) => Op::GteInt,
        (V::Ge, Some(Prim::Float)) => Op::GteFloat,
        (V::Ge, _) => Op::Gte,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The classification is total and the two short-circuiting operators are
    /// the only ones that leave the `Value` side — so `specialize_binop` can
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
                BinopKind::Value(a) => {
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
