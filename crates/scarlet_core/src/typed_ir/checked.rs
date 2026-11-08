//! The checked tree: what the type-checking walk built, one node per
//! expression it checked.
//!
//! Every node carries the type the walk inferred for it, and every name and
//! lambda carries what the walk resolved it to, in the frame that was live at
//! the time. The elaborator turns this tree into [`super::TypedExpr`] once the
//! module's types are final; it reads everything it needs off the node in
//! hand, so nothing has to line the two walks up.
//!
//! Types here are live inference [`Ty`]s, not yet resolved: a body's types are
//! only final once its whole SCC has been generalized, and the decisions that
//! need them (which binop, which field slot, which `or` shape) wait for that.
//!
//! Patterns stay as the source wrote them, because the elaborator reads only
//! the pattern and its scrutinee's type. The expressions inside one (a binary
//! segment's size) are checked like any other and kept beside it, in the order
//! the pattern visits them.

// Built by the check walk; the elaborator does not read it yet.
#![allow(dead_code)]

use scarlet_syntax::ast;
use scarlet_syntax::span::Span;

use super::{ClosureFact, NameFact};
use crate::types::{StrId, Ty};

#[derive(Debug, Clone)]
pub(crate) struct CExpr {
    pub(crate) ty: Ty,
    pub(crate) span: Span,
    pub(crate) kind: CKind,
}

#[derive(Debug, Clone)]
pub(crate) enum CKind {
    Number(ast::NumberLiteral),
    String(String),
    Interp(Vec<CInterpPart>),
    BinaryLiteral(Vec<CBinSeg>),
    /// An identifier in value position.
    Name {
        name: StrId,
        fact: NameFact,
    },
    /// `module::member` in value position. Never a local, whatever its name.
    Member {
        name: StrId,
        fact: NameFact,
    },
    Unary {
        op: ast::UnaryOp,
        operand: Box<CExpr>,
    },
    Binary {
        op: ast::BinaryOp,
        left: Box<CExpr>,
        right: Box<CExpr>,
    },
    Tuple(Vec<CExpr>),
    Array(Vec<CArrayElem>),
    Range {
        start: Box<CExpr>,
        end: Box<CExpr>,
    },
    Index {
        recv: Box<CExpr>,
        index: Box<CExpr>,
    },
    Slice {
        recv: Box<CExpr>,
        start: Box<CExpr>,
        end: Box<CExpr>,
    },
    Field {
        recv: Box<CExpr>,
        label: StrId,
    },
    TupleIndex {
        recv: Box<CExpr>,
        idx: u32,
    },
    Call {
        callee: CCallee,
        args: Vec<CArg>,
    },
    Closure(ClosureFact),
    If {
        cond: Box<CExpr>,
        then: Box<CExpr>,
        els: Box<CExpr>,
    },
    Match {
        subject: Box<CExpr>,
        arms: Vec<CArm>,
    },
    Or {
        lhs: Box<CExpr>,
        receiver: Option<StrId>,
        body: Box<CExpr>,
    },
    /// The nodes stay at the indices the source gave them: a module toplevel
    /// schedules its declarations by index.
    Block(Vec<CNode>),
    /// Something the check walk reported an error for. A module holding one
    /// is never elaborated.
    Error,
}

#[derive(Debug, Clone)]
pub(crate) enum CInterpPart {
    Literal(String),
    Expr(CExpr),
}

#[derive(Debug, Clone)]
pub(crate) struct CBinSeg {
    pub(crate) value: CExpr,
    pub(crate) spec: ast::BinSpec,
    /// The checked size expression, when the spec has one.
    pub(crate) size: Option<CExpr>,
}

#[derive(Debug, Clone)]
pub(crate) enum CArrayElem {
    Elem(CExpr),
    Spread(CExpr),
}

/// A call's callee. A name is resolved where it stands, never checked as an
/// expression of its own.
#[derive(Debug, Clone)]
pub(crate) enum CCallee {
    Name {
        name: StrId,
        fact: NameFact,
        span: Span,
    },
    /// `module::member(..)`.
    Member {
        name: StrId,
        fact: NameFact,
        span: Span,
    },
    Expr(Box<CExpr>),
}

#[derive(Debug, Clone)]
pub(crate) enum CArg {
    Positional(CExpr),
    Labeled { label: StrId, value: CExpr },
    Spread(CExpr),
}

#[derive(Debug, Clone)]
pub(crate) struct CPattern {
    pub(crate) pattern: ast::Pattern,
    /// The pattern's size expressions, in the order the pattern visits them.
    pub(crate) sizes: Vec<CExpr>,
}

#[derive(Debug, Clone)]
pub(crate) struct CArm {
    pub(crate) pattern: CPattern,
    pub(crate) guard: Option<CExpr>,
    pub(crate) body: CExpr,
}

#[derive(Debug, Clone)]
pub(crate) enum CNode {
    Expr(CExpr),
    /// `pattern = init`. A plain name is a pattern too.
    Binding {
        pattern: CPattern,
        init: CExpr,
    },
    /// A top-level `fn` with a body. Its body is elaborated on its own, so
    /// only its name is needed here.
    Fn {
        name: StrId,
        span: Span,
    },
    Const {
        name: StrId,
        init: CExpr,
    },
    /// A statement evaluated only for its effect: a backpass the desugarer
    /// left behind.
    Effect(CExpr),
    /// A type declaration, an import, a `@vm` fn: nothing to elaborate.
    Nothing,
}
