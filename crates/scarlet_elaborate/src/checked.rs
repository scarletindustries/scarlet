//! The checked tree is what the type-checking walk builds. It has one node for
//! each expression the walk checked.
//!
//! Every node carries the type the walk inferred for it, and every name and
//! lambda carries what the walk resolved it to, in the frame that was live at
//! the time. The elaborator turns this tree into [`super::TypedExpr`] once the
//! module's types are final. It reads everything it needs from the node in
//! hand, so nothing has to keep the two walks in step.
//!
//! Types here are live inference [`Ty`]s that aren't resolved yet. A body's
//! types are only final once its whole SCC (the group of definitions that call
//! each other) has been generalised. The decisions that need them, such as
//! which binop, which field slot and which `or` shape, wait until then.
//!
//! Every local binding the walk made has a [`BinderId`], recorded where the
//! binding is made and on every use the walk resolved to it, so the elaborator
//! never looks a name up by its spelling.
//!
//! Patterns are resolved once, here. A constructor pattern names its variant
//! and has one sub-pattern per declared field, and every name a pattern binds
//! has its binder. Exhaustiveness and the elaborator both read this, so
//! neither looks a constructor up again.

use scarlet_syntax::ast;
use scarlet_syntax::span::Span;

use super::{Denotation, GlobalSlot};
use scarlet_ir::core_ir::{FuncIdx, VariantRef};
use scarlet_types::types::{BinPart, BinSegShape, BinUnit, PatShape, PatternShape, StrId, Ty};

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
    /// `module::member` in value position. It is never a local, whatever its
    /// name.
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
    /// The nodes keep the indices the source gave them, because a module
    /// toplevel schedules its declarations by index.
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
    pub width: CWidth,
}

/// How wide a `<<..>>` segment is, with its size expression checked.
#[derive(Debug, Clone)]
pub enum CWidth {
    /// `:N` or `:size(..)`. `None` means the default of 8 bits.
    Int(Option<CExpr>),
    /// `:bytes(..)`. `None` means `:binary`, which takes the rest.
    Bytes(Option<CExpr>),
    /// `:utf8`, which is one codepoint, as wide as its encoding.
    Utf8,
}

#[derive(Debug, Clone)]
pub enum CArrayElem {
    Elem(CExpr),
    Spread(CExpr),
}

/// A call's callee. A name is resolved where it stands and is never checked as
/// an expression of its own.
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

/// A pattern as the walk checked it. Every node carries the type the walk
/// checked it against.
#[derive(Debug, Clone)]
pub struct CPat {
    pub ty: Ty,
    pub span: Span,
    pub kind: CPatKind,
}

#[derive(Debug, Clone)]
pub enum CPatKind {
    /// Matches anything and binds nothing. This is `_`, a write-only `_name`,
    /// or a constructor field the source left out.
    Wild,
    Bind {
        name: StrId,
        binder: BinderId,
    },
    Number(ast::NumberLiteral),
    String(String),
    Range {
        start: ast::NumberLiteral,
        end: ast::NumberLiteral,
    },
    /// One sub-pattern per declared field, in declared order.
    Ctor {
        variant: VariantRef,
        fields: Vec<CPat>,
    },
    Tuple(Vec<CPat>),
    Array {
        elem: Ty,
        prefix: Vec<CPat>,
        rest: CRest,
    },
    Binary {
        segs: Vec<CBinPatSeg>,
        rest: CRest,
    },
    /// The alternatives bind the same names, to the same binders.
    Or(Vec<CPat>),
    /// Something the walk reported an error for. A module holding one is
    /// never elaborated, and its matches are not checked for exhaustiveness.
    Error,
}

/// The `..rest` of an array or binary pattern.
#[derive(Debug, Clone)]
pub enum CRest {
    /// With no rest, the pattern matches exactly what it lists.
    None,
    /// `..` or `.._`.
    Discard,
    Bind {
        name: StrId,
        binder: BinderId,
    },
}

#[derive(Debug, Clone)]
pub enum CBinPatSeg {
    /// `<<'GET ', ..>>`, which matches these bytes as a prefix.
    Literal(String),
    Value {
        width: CWidth,
        value: Box<CPat>,
    },
}

#[derive(Debug, Clone)]
pub struct CArm {
    pub pattern: CPat,
    pub guard: Option<CExpr>,
    pub body: CExpr,
}

#[derive(Debug, Clone)]
pub enum CNode {
    Expr(CExpr),
    /// `pattern = init`. A plain name is a pattern too.
    Binding {
        pattern: CPat,
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
    /// A statement run only for its side effect, which is a backpass the
    /// desugarer left behind.
    Effect(CExpr),
    /// A type declaration, an import or a `@vm` fn, none of which has anything
    /// to elaborate.
    Nothing,
}

/// A name as the check walk resolved it, in the frame that was live then.
#[derive(Debug, Clone)]
pub struct NameFact {
    /// The name's type at this use, as the walk instantiated it.
    pub ty: Ty,
    pub at: NameAt,
    /// The declared parameter labels when the name is a module `fn` called
    /// here, and `None` for every other name. Labels are refused on anything
    /// else.
    pub param_labels: Option<Vec<StrId>>,
}

/// One local binding of one body: a parameter, a name a binding statement or
/// a pattern binds, an `or` receiver, or a module's own top-level declaration.
/// Each binding gets its own, so a name bound twice has two. It is unique
/// within the body that binds it, and counted from zero there.
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

impl PatternShape for CPat {
    fn shape(&self) -> PatShape<'_, Self> {
        match &self.kind {
            CPatKind::Wild | CPatKind::Bind { .. } | CPatKind::Error => PatShape::Wild,
            CPatKind::Number(n) => PatShape::Number(n),
            CPatKind::String(s) => PatShape::String(s),
            CPatKind::Range { start, end } => PatShape::Range(start, end),
            CPatKind::Ctor { variant, fields } => PatShape::Variant {
                index: variant.variant_idx as usize,
                fields,
            },
            CPatKind::Tuple(elems) => PatShape::Tuple(elems),
            CPatKind::Array { prefix, rest, .. } => PatShape::Array {
                prefix,
                open: !matches!(rest, CRest::None),
            },
            // `<<..rest>>` matches every binary.
            CPatKind::Binary { segs, rest } if segs.is_empty() && !matches!(rest, CRest::None) => {
                PatShape::Wild
            }
            CPatKind::Binary { segs, rest } => PatShape::Binary {
                segs: segs.iter().map(bin_seg_shape).collect(),
                open: !matches!(rest, CRest::None),
            },
            CPatKind::Or(alts) => PatShape::Or(alts),
        }
    }
}

fn bin_seg_shape(seg: &CBinPatSeg) -> BinSegShape<'_> {
    match seg {
        CBinPatSeg::Literal(s) => BinSegShape {
            value: BinPart::Str(s),
            unit: BinUnit::Utf8,
            size: None,
        },
        CBinPatSeg::Value { width, value } => {
            let value = match &value.kind {
                CPatKind::Wild | CPatKind::Bind { .. } => BinPart::Any,
                CPatKind::Number(n) => BinPart::Int(n),
                CPatKind::String(s) => BinPart::Str(s),
                CPatKind::Range { .. }
                | CPatKind::Ctor { .. }
                | CPatKind::Tuple(_)
                | CPatKind::Array { .. }
                | CPatKind::Binary { .. }
                | CPatKind::Or(_)
                | CPatKind::Error => BinPart::At(value.span),
            };
            let (unit, size) = match width {
                CWidth::Int(e) => (BinUnit::Bits, size_part(e)),
                CWidth::Bytes(e) => (BinUnit::Bytes, size_part(e)),
                CWidth::Utf8 => (BinUnit::Utf8, None),
            };
            BinSegShape { value, unit, size }
        }
    }
}

fn size_part(e: &Option<CExpr>) -> Option<BinPart<'_>> {
    e.as_ref().map(|e| match &e.kind {
        CKind::Number(n) => BinPart::Int(n),
        _ => BinPart::At(e.span),
    })
}
