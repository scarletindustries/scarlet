//! Source binary operator → [`PrimOp`] selection, specialised against the operand
//! type elaboration resolved.

use crate::ast::BinaryOp;
use crate::core_ir::PrimOp;
use crate::types::Prim;

/// A binary operator that denotes a [`PrimOp`].
///
/// `&&`/`||` are absent: they branch, so the right operand may never be
/// evaluated. That is why [`specialize_binop`] returns a `PrimOp`, not an
/// `Option`. The only constructor is [`BinopKind::of`], which routes the two
/// short-circuiting forms to [`ShortCircuitOp`] instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ValueBinop {
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
/// arm, so the elaborator builds `TypedExpr::And`/`Or`, not `TypedExpr::Binary`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShortCircuitOp {
    And,
    Or,
}

/// Which of the two a source [`BinaryOp`] is. Total, and the sole constructor
/// of [`ValueBinop`], so holding one proves the operator is not `&&`/`||`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BinopKind {
    Value(ValueBinop),
    ShortCircuit(ShortCircuitOp),
}

impl BinopKind {
    pub(crate) fn of(op: BinaryOp) -> BinopKind {
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

/// The [`PrimOp`] for an operator, specialised to the operand's primitive type
/// when elaboration resolved one. A still-unresolved operand keeps the generic
/// form, which the backend dispatches on the values. Total: every
/// `(ValueBinop, Option<Prim>)` names a `PrimOp`.
pub(crate) fn specialize_binop(op: ValueBinop, ty: Option<Prim>) -> PrimOp {
    use ValueBinop as V;
    match (op, ty) {
        (V::Add, Some(Prim::Int)) => PrimOp::IntAdd,
        (V::Add, Some(Prim::Float)) => PrimOp::FloatAdd,
        (V::Add, Some(Prim::String)) => PrimOp::StringConcat,
        (V::Add, _) => PrimOp::Add,
        (V::Sub, Some(Prim::Int)) => PrimOp::IntSub,
        (V::Sub, Some(Prim::Float)) => PrimOp::FloatSub,
        (V::Sub, _) => PrimOp::Sub,
        (V::Mul, Some(Prim::Int)) => PrimOp::IntMul,
        (V::Mul, Some(Prim::Float)) => PrimOp::FloatMul,
        (V::Mul, _) => PrimOp::Mul,
        (V::Div, Some(Prim::Int)) => PrimOp::IntDiv,
        (V::Div, Some(Prim::Float)) => PrimOp::FloatDiv,
        (V::Div, _) => PrimOp::Div,
        (V::Mod, Some(Prim::Int)) => PrimOp::IntRem,
        (V::Mod, _) => PrimOp::Rem,
        (V::Eq, Some(Prim::Int)) => PrimOp::IntEq,
        (V::Eq, _) => PrimOp::Eq,
        (V::Ne, Some(Prim::Int)) => PrimOp::IntNe,
        (V::Ne, _) => PrimOp::Ne,
        (V::Lt, Some(Prim::Int)) => PrimOp::IntLt,
        (V::Lt, Some(Prim::Float)) => PrimOp::FloatLt,
        (V::Lt, _) => PrimOp::Lt,
        (V::Le, Some(Prim::Int)) => PrimOp::IntLe,
        (V::Le, Some(Prim::Float)) => PrimOp::FloatLe,
        (V::Le, _) => PrimOp::Le,
        (V::Gt, Some(Prim::Int)) => PrimOp::IntGt,
        (V::Gt, Some(Prim::Float)) => PrimOp::FloatGt,
        (V::Gt, _) => PrimOp::Gt,
        (V::Ge, Some(Prim::Int)) => PrimOp::IntGe,
        (V::Ge, Some(Prim::Float)) => PrimOp::FloatGe,
        (V::Ge, _) => PrimOp::Ge,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The classification is total and only `&&`/`||` leave the `Value` side,
    /// so `specialize_binop` cannot be reached with a branching operator.
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
                    // Total over every primitive type, including "unresolved".
                    for ty in [None, Some(Prim::Int), Some(Prim::Float), Some(Prim::String)] {
                        let _: PrimOp = specialize_binop(a, ty);
                    }
                }
            }
        }
    }

    #[test]
    fn a_known_operand_type_picks_the_specialised_prim() {
        assert_eq!(
            specialize_binop(ValueBinop::Add, Some(Prim::Int)),
            PrimOp::IntAdd
        );
        assert_eq!(
            specialize_binop(ValueBinop::Add, Some(Prim::String)),
            PrimOp::StringConcat
        );
        assert_eq!(specialize_binop(ValueBinop::Add, None), PrimOp::Add);
    }
}
