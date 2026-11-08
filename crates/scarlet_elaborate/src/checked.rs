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
//! Every local binding the walk made has a [`BinderId`], recorded where the
//! binding is made and on every use the walk resolved to it, so the elaborator
//! never looks a name up by its spelling.
//!
//! Patterns stay as the source wrote them, because the elaborator reads only
//! the pattern and its scrutinee's type. The expressions inside one (a binary
//! segment's size) are checked like any other and kept beside it, in the order
//! the pattern visits them.

use scarlet_syntax::ast;
use scarlet_syntax::span::Span;

use super::{Denotation, GlobalSlot};
use scarlet_ir::core_ir::FuncIdx;
use scarlet_types::types::{StrId, Ty};

#[derive(Debug, Clone)]
pub struct CExpr {
    pub ty: Ty,
    pub span: Span,
    pub kind: CKind,
}

#[derive(Debug, Clone)]
pub enum CKind {
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
        /// The `Err` payload's name and binder, when the source names one.
        receiver: Option<(StrId, BinderId)>,
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
pub enum CInterpPart {
    Literal(String),
    Expr(CExpr),
}

#[derive(Debug, Clone)]
pub struct CBinSeg {
    pub value: CExpr,
    pub spec: ast::BinSpec,
    /// The checked size expression, when the spec has one.
    pub size: Option<CExpr>,
}

#[derive(Debug, Clone)]
pub enum CArrayElem {
    Elem(CExpr),
    Spread(CExpr),
}

/// A call's callee. A name is resolved where it stands, never checked as an
/// expression of its own.
#[derive(Debug, Clone)]
pub enum CCallee {
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
pub enum CArg {
    Positional(CExpr),
    Labeled { label: StrId, value: CExpr },
    Spread(CExpr),
}

#[derive(Debug, Clone)]
pub struct CPattern {
    pub pattern: ast::Pattern,
    /// The pattern's size expressions, in the order the pattern visits them.
    pub sizes: Vec<CExpr>,
    /// The binder of each name the pattern binds. A pattern binds a name once,
    /// across all the alternatives of an or-pattern too, so the name is the
    /// key. `_`-prefixed names are never read and have none.
    pub binders: Vec<(StrId, BinderId)>,
}

#[derive(Debug, Clone)]
pub struct CArm {
    pub pattern: CPattern,
    pub guard: Option<CExpr>,
    pub body: CExpr,
}

#[derive(Debug, Clone)]
pub enum CNode {
    Expr(CExpr),
    /// `pattern = init`. A plain name is a pattern too.
    Binding {
        pattern: CPattern,
        init: CExpr,
        /// The entry-frame slot of each name this binding binds at module
        /// scope, which lambdas already checked read it from. Empty for a
        /// binding anywhere else. A pattern binds a name once, so the name is
        /// the key.
        globals: Vec<(StrId, GlobalSlot)>,
    },
    /// A top-level `fn` with a body. Its body is elaborated on its own, so
    /// only its name is needed here.
    Fn {
        name: StrId,
        binder: BinderId,
        span: Span,
    },
    Const {
        name: StrId,
        binder: BinderId,
        init: CExpr,
    },
    /// A statement evaluated only for its effect: a backpass the desugarer
    /// left behind.
    Effect(CExpr),
    /// A type declaration, an import, a `@vm` fn: nothing to elaborate.
    Nothing,
}

/// A name as the check walk resolved it, in the frame that was live then.
#[derive(Debug, Clone)]
pub struct NameFact {
    /// The name's type at this use, as the walk instantiated it.
    pub ty: Ty,
    pub at: NameAt,
    /// The declared parameter labels when the name is a module `fn` called
    /// here, `None` for every other name. Labels are refused on anything else.
    pub param_labels: Option<Vec<StrId>>,
}

/// One local binding of one body: a parameter, a name a binding statement or
/// a pattern binds, an `or` receiver, or a module's own top-level declaration.
/// Each binding gets its own, so a name bound twice has two. Unique within the
/// body that binds it, and counted from zero there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BinderId(pub u32);

#[derive(Debug, Clone, Copy)]
pub enum NameAt {
    /// Bound by the body being elaborated. At a module toplevel it may be a
    /// declaration the toplevel reaches before the declaration's statement.
    Local(BinderId),
    /// Anything else, fixed by the walk. A module-scope value may still turn
    /// out to be a known function once every body has its slot.
    Den(Denotation),
    /// Nothing at runtime binds it. The walk has reported an error for it, so
    /// it is never elaborated.
    Unbound,
}

/// A lambda as the check walk closed it.
#[derive(Debug, Clone)]
pub struct ClosureFact {
    pub func_idx: FuncIdx,
    /// Every captured name, in capture-index order, with how it loads in the
    /// enclosing body.
    pub captures: Vec<(StrId, NameFact)>,
}
