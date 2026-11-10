//! The resolved tree: the source with every name settled. Each name says
//! what it is, each call's arguments know the parameters they fill, each
//! lambda knows what it captures, and each annotation is a finished type.
//! Inference reads this, so it has no scope to keep track of.

use scarlet_syntax::ast::{BinaryOp, UnaryOp};
use scarlet_syntax::span::Span;
use scarlet_types::name::{CtorName, Name};
use scarlet_types::ty::{Scheme, Type, TypeVar};

use crate::declare::DeclId;
use crate::typed::{BinderId, Rest, Var};

/// A local binder: a parameter, a `let`, a pattern name, an `or` receiver.
#[derive(Debug, Clone)]
pub(crate) struct BinderInfo {
    pub name: Name,
    pub span: Span,
    /// At module scope, where functions read it as a global.
    pub module_scope: bool,
    /// A `_` name with no written type, which throws its value away. If the
    /// value is `Nil`, the `Nil` pattern says so instead.
    pub discards: bool,
}

/// What a name in value position is.
#[derive(Debug, Clone)]
pub(crate) enum Ref {
    Local(BinderId),
    /// One of this module's functions, consts or `@vm` functions.
    Own(DeclId, Var),
    /// A value another module declares, with its type.
    Foreign(Var, Scheme),
    /// A constructor, this module's or another's, with its type.
    Ctor(CtorName, Scheme),
}

#[derive(Debug, Clone)]
pub(crate) struct RExpr {
    pub span: Span,
    pub kind: RKind,
}

#[derive(Debug, Clone)]
pub(crate) enum RKind {
    Int(i64),
    Float(f64),
    String(String),
    Interp(Vec<RPart>),
    Binary(Vec<RBinSeg>),
    /// A name, and where it is written: the member, for `module::member`.
    Ref {
        r: Ref,
        at: Span,
    },
    Lambda(Box<RLambda>),
    Call {
        callee: Box<RExpr>,
        args: Vec<RArg>,
    },
    Construct(Box<RConstruct>),
    Tuple(Vec<RExpr>),
    TupleIndex {
        tuple: Box<RExpr>,
        index: u32,
        index_span: Span,
    },
    Field {
        record: Box<RExpr>,
        label: Name,
        label_span: Span,
    },
    Array(Vec<RElem>),
    Index {
        array: Box<RExpr>,
        index: Box<RExpr>,
    },
    Slice {
        array: Box<RExpr>,
        start: Box<RExpr>,
        end: Box<RExpr>,
    },
    Range {
        start: Box<RExpr>,
        end: Box<RExpr>,
    },
    BinOp {
        op: BinaryOp,
        left: Box<RExpr>,
        right: Box<RExpr>,
    },
    Unary {
        op: UnaryOp,
        operand: Box<RExpr>,
    },
    If {
        cond: Box<RExpr>,
        then: Box<RExpr>,
        els: Box<RExpr>,
        /// Where each branch's type comes from: its last expression.
        then_span: Span,
        els_span: Span,
    },
    Match {
        subject: Box<RExpr>,
        arms: Vec<RArm>,
    },
    OrElse {
        value: Box<RExpr>,
        receiver: Option<(BinderId, Span)>,
        fallback: Box<RExpr>,
        fallback_span: Span,
    },
    Block {
        stmts: Vec<RStmt>,
        tail: Option<Box<RExpr>>,
    },
    /// Something that didn't resolve. It is only made next to the diagnostic
    /// that says why.
    Error,
}

/// A constructor applied to its fields.
#[derive(Debug, Clone)]
pub(crate) struct RConstruct {
    pub ctor: CtorName,
    pub scheme: Scheme,
    pub labels: Vec<Name>,
    pub args: Vec<RArg>,
    /// `..base`, and how many arguments were written before it.
    pub spread: Option<(u32, Box<RExpr>)>,
    /// Where the constructor's name is written.
    pub at: Span,
}

#[derive(Debug, Clone)]
pub(crate) struct RLambda {
    pub params: Vec<(BinderId, Option<Type>)>,
    pub ret: Option<Type>,
    /// The generics its annotations name.
    pub vars: Vec<TypeVar>,
    pub body: RExpr,
    pub body_span: Span,
    pub captures: Vec<BinderId>,
    /// The binder of `x` in `x = fn(..) { .. }`, which inside the body is the
    /// lambda itself.
    pub self_name: Option<BinderId>,
}

#[derive(Debug, Clone)]
pub(crate) struct RArg {
    /// The parameter it fills, or its position when it fills none.
    pub slot: u32,
    pub value: RExpr,
    /// Whether the slot really is a parameter. An argument that names no
    /// parameter has been reported already, and its value is only checked.
    pub fills: bool,
}

#[derive(Debug, Clone)]
pub(crate) enum RElem {
    Elem(RExpr),
    Spread(RExpr),
}

#[derive(Debug, Clone)]
pub(crate) enum RPart {
    Text(String),
    Expr(RExpr),
}

#[derive(Debug, Clone)]
pub(crate) struct RBinSeg {
    pub value: RExpr,
    pub unit: RUnit,
}

#[derive(Debug, Clone)]
pub(crate) enum RUnit {
    Bits(Option<Box<RExpr>>),
    Bytes(Option<Box<RExpr>>),
    Utf8,
}

#[derive(Debug, Clone)]
pub(crate) enum RStmt {
    Let {
        pattern: RPattern,
        /// A written type, like `x Int = ..`.
        annot: Option<Type>,
        value: RExpr,
        value_span: Span,
        /// For `x = e`, the binder `x`. Its type is generalised.
        name: Option<BinderId>,
    },
    Expr(RExpr),
}

#[derive(Debug, Clone)]
pub(crate) struct RArm {
    pub pattern: RPattern,
    pub guard: Option<(RExpr, Span)>,
    pub body: RExpr,
    pub body_span: Span,
}

#[derive(Debug, Clone)]
pub(crate) struct RPattern {
    pub span: Span,
    pub kind: RPatKind,
}

#[derive(Debug, Clone)]
pub(crate) enum RPatKind {
    Wild,
    Bind(BinderId),
    Int(i64),
    Float(f64),
    String(String),
    Range(i64, i64),
    /// One pattern per declared field, in declared order.
    Ctor {
        ctor: CtorName,
        scheme: Scheme,
        fields: Vec<RPattern>,
        /// Where the constructor's name is written.
        at: Span,
    },
    Tuple(Vec<RPattern>),
    Array {
        prefix: Vec<RPattern>,
        rest: Rest,
    },
    Binary {
        segs: Vec<RBinSegPat>,
        rest: Rest,
    },
    Or(Vec<RPattern>),
    /// Something that didn't resolve, made only next to its diagnostic.
    Error,
}

#[derive(Debug, Clone)]
pub(crate) enum RBinSegPat {
    Text(String),
    Utf8(RPattern),
    Bits {
        bits: Option<RExpr>,
        value: RPattern,
    },
    Bytes {
        bytes: Option<RExpr>,
        value: RPattern,
    },
}

/// One module-level value, resolved.
#[derive(Debug, Clone)]
pub(crate) enum RDecl {
    Fn {
        params: Vec<(BinderId, Option<Type>)>,
        ret: Option<Type>,
        /// The generics the signature names.
        vars: Vec<TypeVar>,
        body: RExpr,
        body_span: Span,
    },
    Builtin {
        params: Vec<Option<Type>>,
        ret: Option<Type>,
        vars: Vec<TypeVar>,
    },
    Const {
        annot: Option<Type>,
        value: RExpr,
        value_span: Span,
    },
}

/// The whole module, resolved.
pub(crate) struct Resolved {
    /// By declaration.
    pub decls: Vec<RDecl>,
    pub statements: Vec<RStmt>,
    pub binders: Vec<BinderInfo>,
    /// The functions and consts, in groups that use each other, each group
    /// after every group it uses.
    pub groups: Vec<Vec<DeclId>>,
    /// The binders nothing reads, in source order.
    pub unused: Vec<BinderId>,
}
