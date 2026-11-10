//! The typed tree: one checked module, with every name resolved and every
//! type known. It is what the checker hands on, to lowering and to the editor.
//!
//! Each node carries a [`Ty`] into the module's [`Types`]. Inference is over
//! by the time anything reads the tree, so a `Ty` always reads back the same
//! finished [`Type`](scarlet_types::ty::Type).
//!
//! The tree keeps the source's shape: a call's arguments stay in the order
//! they were written, each with the parameter it fills, and a block keeps its
//! statements. What it adds is every fact the checker worked out, so nothing
//! later has to work one out again.

use scarlet_ir::intrinsic::Intrinsic;
use scarlet_syntax::ast::UnaryOp;
use scarlet_syntax::span::Span;
use scarlet_types::name::{CtorName, ModuleKey, Name, ValueName};

use scarlet_types::engine::{Ty, Types};

/// A local name's identity within one module: a parameter, a `let`, a
/// pattern binding, or a module-scope statement's binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BinderId(pub u32);

#[derive(Debug, Clone)]
pub struct Binder {
    pub name: Name,
    pub ty: Ty,
    /// Whether the binder lives at module scope, where functions read it as a
    /// global. Everything else lives in the frame of the function that binds it.
    pub module_scope: bool,
}

/// One module-level function, `fn` or `@vm`.
#[derive(Debug, Clone)]
pub struct Function {
    pub name: Name,
    pub params: Vec<BinderId>,
    /// The function's own type, generic variables and all.
    pub ty: Ty,
    pub body: FnBody,
}

#[derive(Debug, Clone)]
pub enum FnBody {
    Expr(Expr),
    /// A `@vm` function: no Scarlet body, the VM runs `intrinsic`.
    Builtin(Intrinsic),
}

/// A module-level `const`.
#[derive(Debug, Clone)]
pub struct Const {
    pub name: Name,
    pub value: Expr,
}

#[derive(Debug, Clone)]
pub struct TypedModule {
    pub key: ModuleKey,
    pub types: Types,
    pub binders: Vec<Binder>,
    pub functions: Vec<Function>,
    /// In the order they must run: a const comes after every const it reads.
    pub consts: Vec<Const>,
    /// The module's own statements, in source order. Only an entry module or
    /// a script has any. They run after the consts.
    pub statements: Vec<Stmt>,
}

impl TypedModule {
    pub fn binder(&self, b: BinderId) -> &Binder {
        &self.binders[b.0 as usize]
    }
}

#[derive(Debug, Clone)]
pub struct Expr {
    pub ty: Ty,
    pub(crate) span: Span,
    pub kind: ExprKind,
}

/// A binary operator that takes two values. `&&` and `||` are control flow,
/// so they are their own nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
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

/// What a name in value position refers to.
#[derive(Debug, Clone, PartialEq)]
pub enum Var {
    Local(BinderId),
    /// A module-level function, in this module or another.
    Fn(ValueName),
    Const(ValueName),
    /// A `@vm` function used as a value.
    Builtin(Intrinsic),
    /// A constructor used as a value: a nullary one, or one passed around as
    /// a function.
    Ctor(CtorName),
}

#[derive(Debug, Clone)]
pub enum ExprKind {
    Int(i64),
    Float(f64),
    String(String),
    Interp(Vec<InterpPart>),
    Binary(Vec<BinSeg>),
    Var(Var),
    /// `fn(params) { body }`.
    Lambda(Lambda),
    Call {
        callee: Box<Expr>,
        args: Vec<Arg>,
    },
    /// A constructor applied to its fields.
    Construct {
        ctor: CtorName,
        args: Vec<Arg>,
        /// `..base`: the fields no argument gives come from it.
        spread: Option<Box<Spread>>,
    },
    Tuple(Vec<Expr>),
    TupleIndex {
        tuple: Box<Expr>,
        index: u32,
    },
    Field {
        record: Box<Expr>,
        label: Name,
        at: FieldAt,
    },
    Array(Vec<ArrayElem>),
    /// `xs[i]`, which gives an `Option`.
    Index {
        array: Box<Expr>,
        index: Box<Expr>,
    },
    /// `xs[a..b]`.
    Slice {
        array: Box<Expr>,
        start: Box<Expr>,
        end: Box<Expr>,
    },
    Range {
        start: Box<Expr>,
        end: Box<Expr>,
    },
    BinOp {
        op: BinOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Unary {
        op: UnaryOp,
        operand: Box<Expr>,
    },
    If {
        cond: Box<Expr>,
        then: Box<Expr>,
        els: Box<Expr>,
    },
    Match {
        subject: Box<Expr>,
        arms: Vec<Arm>,
    },
    /// `value or fallback`, or `value or e -> fallback`.
    OrElse {
        shape: OrShape,
        value: Box<Expr>,
        /// The binder for `e`, which only a `Result` has.
        error: Option<BinderId>,
        fallback: Box<Expr>,
    },
    Block(Block),
    /// Something that failed to check. Only a module with errors holds one.
    Error,
}

#[derive(Debug, Clone)]
pub struct Lambda {
    pub params: Vec<BinderId>,
    pub body: Box<Expr>,
    /// The enclosing function's locals the body reads, in first-use order.
    pub captures: Vec<BinderId>,
    /// `x` in `x = fn(..) { .. x(..) .. }`: inside the body, this binder is the
    /// closure itself.
    pub self_name: Option<BinderId>,
}

/// One call argument, as written.
#[derive(Debug, Clone)]
pub struct Arg {
    /// The parameter or field it fills.
    pub slot: u32,
    pub value: Expr,
}

#[derive(Debug, Clone)]
pub struct Spread {
    pub base: Expr,
    /// How many of the call's arguments were written before it, so it runs
    /// in its place among them.
    pub at: u32,
    /// For each field no argument gives: its slot in the new value, and where
    /// to read it from `base`.
    pub fills: Vec<(u32, FieldAt)>,
}

/// Where a field sits in the values a record can be.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldAt {
    /// Every variant holds it in the same slot.
    Same(u32),
    /// Each variant holds it somewhere else: `(variant, slot)` for each.
    PerVariant(Vec<(u16, u32)>),
}

/// Which type `or` unwraps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrShape {
    Option,
    Result,
}

#[derive(Debug, Clone)]
pub enum ArrayElem {
    Elem(Expr),
    Spread(Expr),
}

#[derive(Debug, Clone)]
pub enum InterpPart {
    Text(String),
    Expr(Expr),
}

/// One `<<..>>` segment in value position.
#[derive(Debug, Clone)]
pub struct BinSeg {
    pub value: Expr,
    pub unit: BinUnit,
}

/// How a binary segment is sized.
#[derive(Debug, Clone)]
pub enum BinUnit {
    /// An integer in this many bits, 8 when unwritten.
    Bits(Option<Box<Expr>>),
    /// A binary, cut to this many bytes, or whole when unwritten.
    Bytes(Option<Box<Expr>>),
    /// A string, as UTF-8.
    Utf8,
}

#[derive(Debug, Clone)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    /// The block's value: its last expression. A block that ends in a
    /// statement, or is empty, is `Nil`.
    pub tail: Option<Box<Expr>>,
}

#[derive(Debug, Clone)]
pub enum Stmt {
    /// `pattern = value`. The pattern matches every value of its type.
    Let { pattern: Pattern, value: Expr },
    /// An expression run for what it does. Its value is `Nil`.
    Expr(Expr),
}

#[derive(Debug, Clone)]
pub struct Arm {
    pub pattern: Pattern,
    pub guard: Option<Expr>,
    pub body: Expr,
}

#[derive(Debug, Clone)]
pub struct Pattern {
    pub ty: Ty,
    pub(crate) span: Span,
    pub kind: PatternKind,
}

#[derive(Debug, Clone)]
pub enum PatternKind {
    Wild,
    Bind(BinderId),
    Int(i64),
    Float(f64),
    String(String),
    /// `lo..hi`, both ends included.
    Range(i64, i64),
    /// One pattern per declared field, in declared order. A field the source
    /// left out is [`PatternKind::Wild`].
    Ctor {
        ctor: CtorName,
        fields: Vec<Pattern>,
    },
    Tuple(Vec<Pattern>),
    Array {
        prefix: Vec<Pattern>,
        rest: Rest,
    },
    Binary {
        segs: Vec<BinSegPat>,
        rest: Rest,
    },
    /// Two or more alternatives, all binding the same names.
    Or(Vec<Pattern>),
    Error,
}

/// The `..` at the end of an array or binary pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rest {
    /// No `..`: the length must match.
    None,
    Discard,
    Bind(BinderId),
}

/// One `<<..>>` segment in pattern position.
#[derive(Debug, Clone)]
pub enum BinSegPat {
    /// `<<'text'>>`: these bytes, as a prefix.
    Text(String),
    Utf8(Pattern),
    /// An integer in `bits` bits (8 when unwritten).
    Bits {
        bits: Option<Expr>,
        value: Pattern,
    },
    /// A binary of `bytes` bytes, or the rest when unwritten.
    Bytes {
        bytes: Option<Expr>,
        value: Pattern,
    },
}

impl Expr {
    /// Visit every expression in the tree below and including this one, outer
    /// first.
    pub(crate) fn walk<'a>(&'a self, f: &mut impl FnMut(&'a Expr)) {
        f(self);
        self.for_each_child(&mut |e| e.walk(f));
    }

    /// The expressions directly below this one, in source order.
    fn for_each_child<'a>(&'a self, f: &mut impl FnMut(&'a Expr)) {
        match &self.kind {
            ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::String(_)
            | ExprKind::Var(_)
            | ExprKind::Error => {}
            ExprKind::Interp(parts) => {
                for p in parts {
                    if let InterpPart::Expr(e) = p {
                        f(e);
                    }
                }
            }
            ExprKind::Binary(segs) => {
                for s in segs {
                    f(&s.value);
                    match &s.unit {
                        BinUnit::Bits(Some(e)) | BinUnit::Bytes(Some(e)) => f(e),
                        BinUnit::Bits(None) | BinUnit::Bytes(None) | BinUnit::Utf8 => {}
                    }
                }
            }
            ExprKind::Lambda(l) => f(&l.body),
            ExprKind::Call { callee, args } => {
                f(callee);
                args.iter().for_each(|a| f(&a.value));
            }
            ExprKind::Construct { args, spread, .. } => {
                args.iter().for_each(|a| f(&a.value));
                if let Some(s) = spread {
                    f(&s.base);
                }
            }
            ExprKind::Tuple(xs) => xs.iter().for_each(f),
            ExprKind::TupleIndex { tuple, .. } => f(tuple),
            ExprKind::Field { record, .. } => f(record),
            ExprKind::Array(elems) => {
                for e in elems {
                    match e {
                        ArrayElem::Elem(x) | ArrayElem::Spread(x) => f(x),
                    }
                }
            }
            ExprKind::Index { array, index } => {
                f(array);
                f(index);
            }
            ExprKind::Slice { array, start, end } => {
                f(array);
                f(start);
                f(end);
            }
            ExprKind::Range { start, end } => {
                f(start);
                f(end);
            }
            ExprKind::BinOp { left, right, .. }
            | ExprKind::And(left, right)
            | ExprKind::Or(left, right) => {
                f(left);
                f(right);
            }
            ExprKind::Unary { operand, .. } => f(operand),
            ExprKind::If { cond, then, els } => {
                f(cond);
                f(then);
                f(els);
            }
            ExprKind::Match { subject, arms } => {
                f(subject);
                for arm in arms {
                    arm.pattern.for_each_expr(f);
                    if let Some(g) = &arm.guard {
                        f(g);
                    }
                    f(&arm.body);
                }
            }
            ExprKind::OrElse {
                value, fallback, ..
            } => {
                f(value);
                f(fallback);
            }
            ExprKind::Block(b) => {
                for s in &b.stmts {
                    s.for_each_expr(f);
                }
                if let Some(t) = &b.tail {
                    f(t);
                }
            }
        }
    }
}

impl Stmt {
    pub(crate) fn for_each_expr<'a>(&'a self, f: &mut impl FnMut(&'a Expr)) {
        match self {
            Stmt::Let { pattern, value } => {
                pattern.for_each_expr(f);
                f(value);
            }
            Stmt::Expr(e) => f(e),
        }
    }
}

impl Pattern {
    /// The expressions inside the pattern: binary segment sizes.
    fn for_each_expr<'a>(&'a self, f: &mut impl FnMut(&'a Expr)) {
        match &self.kind {
            PatternKind::Wild
            | PatternKind::Bind(_)
            | PatternKind::Int(_)
            | PatternKind::Float(_)
            | PatternKind::String(_)
            | PatternKind::Range(..)
            | PatternKind::Error => {}
            PatternKind::Ctor { fields: ps, .. }
            | PatternKind::Tuple(ps)
            | PatternKind::Array { prefix: ps, .. }
            | PatternKind::Or(ps) => ps.iter().for_each(|p| p.for_each_expr(f)),
            PatternKind::Binary { segs, .. } => {
                for s in segs {
                    match s {
                        BinSegPat::Text(_) => {}
                        BinSegPat::Utf8(p) => p.for_each_expr(f),
                        BinSegPat::Bits { bits, value } => {
                            if let Some(b) = bits {
                                f(b);
                            }
                            value.for_each_expr(f);
                        }
                        BinSegPat::Bytes { bytes, value } => {
                            if let Some(b) = bytes {
                                f(b);
                            }
                            value.for_each_expr(f);
                        }
                    }
                }
            }
        }
    }
}
