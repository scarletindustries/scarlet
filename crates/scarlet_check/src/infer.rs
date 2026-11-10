//! Type inference over the resolved tree, a group of declarations at a time.
//! Names are already settled, so all inference carries down the tree is a
//! [`Ctx`]: the monotypes of the group being inferred and of the bindings
//! whose own initialiser is being checked. Every binder's type is written
//! once, where it is bound.

use scarlet_syntax::ast::{BinaryOp, UnaryOp};
use scarlet_syntax::span::Span;
use scarlet_types::def::TypeDef;
use scarlet_types::engine::{Engine, LocalScheme, NotCallable, Shape, Ty, UnifyError};
use scarlet_types::known::Known;
use scarlet_types::name::{Name, TypeName};
use scarlet_types::ty::{Constraint, Type, TypeVar};

use crate::declare::{DeclBody, DeclId, Declared};
use crate::env::Env;
use crate::index::{EntityKind, Site, Use, UseKind};
use crate::report::Report;
use crate::resolved::*;
use crate::typed::{
    self, Arg, Arm, ArrayElem, BinOp, BinSeg, BinSegPat, BinUnit, BinderId, Block, Expr, ExprKind,
    FieldAt, InterpPart, Lambda, OrShape, Pattern, PatternKind, Spread, Stmt, Var,
};
use crate::types::TypeDefs;

/// What inference carries down the tree.
#[derive(Clone, Copy)]
struct Ctx<'c> {
    /// The group being inferred, each with the one type every use in the
    /// group shares.
    group: &'c [(DeclId, Ty)],
    /// The bindings whose own initialiser is being checked: `x` in
    /// `x = fn(..) { .. x(..) .. }`, with its type so far.
    pending: Option<&'c Pending<'c>>,
    /// The declaration the code belongs to, which owns the uses found.
    owner: &'c Option<Site>,
}

struct Pending<'c> {
    binder: BinderId,
    ty: Ty,
    outer: Option<&'c Pending<'c>>,
}

impl Ctx<'_> {
    fn pending(&self, b: BinderId) -> Option<Ty> {
        let mut p = self.pending;
        while let Some(node) = p {
            if node.binder == b {
                return Some(node.ty);
            }
            p = node.outer;
        }
        None
    }

    fn group(&self, d: DeclId) -> Option<Ty> {
        self.group.iter().find(|(g, _)| *g == d).map(|&(_, t)| t)
    }
}

pub(crate) struct Inferred {
    pub engine: Engine,
    /// In declaration order.
    pub functions: Vec<typed::Function>,
    /// In the order they must run.
    pub consts: Vec<typed::Const>,
    pub statements: Vec<Stmt>,
    /// Each binder's type.
    pub binder_types: Vec<Ty>,
    /// Each declaration's scheme.
    pub schemes: Vec<LocalScheme>,
    /// The type at each name written in the module, for the editor: every
    /// declaration, every use and every field read. Binders are not here;
    /// their spans and types already are.
    pub hovers: Vec<(Span, Ty)>,
    /// Binders whose binding was itself an error, like a receiver on an `or`
    /// that has no error to give it. That has been reported, so nothing says
    /// they are unused too.
    pub refused: Vec<BinderId>,
}

struct Infer<'r, 'a> {
    env: &'r Env<'a>,
    declared: &'r Declared<'a>,
    defs: &'r TypeDefs,
    e: Engine,
    /// Written once each, where the binder is bound.
    binders: Vec<Option<LocalScheme>>,
    /// Written once each, when the declaration's group is generalised.
    decls: Vec<Option<LocalScheme>>,
    hovers: Vec<(Span, Ty)>,
    refused: Vec<BinderId>,
    report: Report,
}

pub(crate) fn infer(
    env: &Env<'_>,
    declared: &Declared<'_>,
    defs: &TypeDefs,
    resolved: &Resolved,
) -> (Inferred, Report) {
    let mut i = Infer {
        env,
        declared,
        defs,
        e: Engine::new(Known::new()),
        binders: vec![None; resolved.binders.len()],
        decls: vec![None; resolved.decls.len()],
        hovers: Vec::new(),
        refused: Vec::new(),
        report: Report::new(env.input.source),
    };
    let mut functions: Vec<(DeclId, typed::Function)> = Vec::new();
    let mut consts = Vec::new();
    for group in &resolved.groups {
        let (fs, cs) = i.group(group, resolved);
        functions.extend(fs);
        consts.extend(cs);
    }
    functions.sort_by_key(|(d, _)| *d);
    let no_owner = None;
    let top = Ctx {
        group: &[],
        pending: None,
        owner: &no_owner,
    };
    let statements = resolved.statements.iter().map(|s| i.stmt(s, top)).collect();
    // A binder with no type was bound in code that didn't check, which has
    // been reported; it keeps a variable nothing solves.
    let mut binder_types = Vec::with_capacity(i.binders.len());
    for s in std::mem::take(&mut i.binders) {
        binder_types.push(match s {
            Some(s) => s.ty,
            None => i.e.fresh(),
        });
    }
    let mut schemes = Vec::with_capacity(i.decls.len());
    for s in std::mem::take(&mut i.decls) {
        schemes.push(match s {
            Some(s) => s,
            None => LocalScheme::mono(i.e.fresh()),
        });
    }
    let inferred = Inferred {
        engine: i.e,
        functions: functions.into_iter().map(|(_, f)| f).collect(),
        consts,
        statements,
        binder_types,
        schemes,
        hovers: i.hovers,
        refused: i.refused,
    };
    (inferred, i.report)
}

impl Infer<'_, '_> {
    // --- Groups ---

    /// Infer one group of declarations that use each other: each against one
    /// shared type, then all generalised together.
    fn group(
        &mut self,
        group: &[DeclId],
        resolved: &Resolved,
    ) -> (Vec<(DeclId, typed::Function)>, Vec<typed::Const>) {
        let level = self.e.enter();
        let sigs: Vec<(DeclId, Ty, Vec<Ty>, Ty)> = group
            .iter()
            .map(|&d| {
                let (ty, params, ret) = self.signature(&resolved.decls[d.0 as usize]);
                (d, ty, params, ret)
            })
            .collect();
        let tys: Vec<(DeclId, Ty)> = sigs.iter().map(|&(d, ty, _, _)| (d, ty)).collect();
        let mut functions = Vec::new();
        let mut consts = Vec::new();
        for (d, ty, params, ret) in &sigs {
            let decl = self.declared.get(*d);
            self.hovers.push((decl.span, *ty));
            let owner = Some(self.env.site(decl.span, decl.entity()));
            let ctx = Ctx {
                group: &tys,
                pending: None,
                owner: &owner,
            };
            match (&resolved.decls[d.0 as usize], &decl.body) {
                (
                    RDecl::Fn {
                        params: ps,
                        body,
                        body_span,
                        ..
                    },
                    _,
                ) => {
                    for ((b, _), &t) in ps.iter().zip(params) {
                        self.define(*b, LocalScheme::mono(t));
                    }
                    let body = self.expr(body, ctx);
                    self.unify(*ret, body.ty, *body_span);
                    functions.push((
                        *d,
                        typed::Function {
                            name: decl.name.clone(),
                            params: ps.iter().map(|(b, _)| *b).collect(),
                            ty: *ty,
                            body: typed::FnBody::Expr(body),
                        },
                    ));
                }
                (RDecl::Builtin { .. }, DeclBody::Builtin(_, intrinsic)) => functions.push((
                    *d,
                    typed::Function {
                        name: decl.name.clone(),
                        params: Vec::new(),
                        ty: *ty,
                        body: typed::FnBody::Builtin(*intrinsic),
                    },
                )),
                (
                    RDecl::Const {
                        annot,
                        value,
                        value_span,
                    },
                    _,
                ) => {
                    let hint = annot.as_ref().map(|_| *ty);
                    let value = self.expr_hinted(value, hint, ctx);
                    self.unify(*ty, value.ty, *value_span);
                    consts.push(typed::Const {
                        name: decl.name.clone(),
                        value,
                    });
                }
                (RDecl::Builtin { .. }, DeclBody::Fn(..) | DeclBody::Const(_)) => {}
            }
        }
        // A const has already been evaluated, so it falls under the value
        // restriction. A function's variables all sit under its arrow.
        let restricted = self.env.restricted.clone();
        let items: Vec<(Ty, &[TypeName])> = sigs
            .iter()
            .map(|&(d, ty, _, _)| match resolved.decls[d.0 as usize] {
                RDecl::Const { .. } => (ty, &restricted[..]),
                RDecl::Fn { .. } | RDecl::Builtin { .. } => (ty, &[][..]),
            })
            .collect();
        let schemes = self.e.generalize_all(level, &items);
        for ((d, ..), scheme) in sigs.iter().zip(schemes) {
            self.decls[d.0 as usize] = Some(scheme);
        }
        (functions, consts)
    }

    /// A declaration's type before its body is inferred: what its signature
    /// says, with a fresh variable for anything it leaves out. Returns the
    /// whole type, and a function's parameter and return types.
    fn signature(&mut self, d: &RDecl) -> (Ty, Vec<Ty>, Ty) {
        match d {
            RDecl::Fn {
                params, ret, vars, ..
            } => {
                let mut subst = self.rigid(vars);
                let ps: Vec<Ty> = params
                    .iter()
                    .map(|(_, t)| self.import_opt(t.as_ref(), &mut subst))
                    .collect();
                let r = self.import_opt(ret.as_ref(), &mut subst);
                (self.e.fun(ps.clone(), r), ps, r)
            }
            RDecl::Builtin { params, ret, vars } => {
                let mut subst = self.rigid(vars);
                let ps: Vec<Ty> = params
                    .iter()
                    .map(|t| self.import_opt(t.as_ref(), &mut subst))
                    .collect();
                let r = self.import_opt(ret.as_ref(), &mut subst);
                (self.e.fun(ps.clone(), r), ps, r)
            }
            RDecl::Const { annot, .. } => {
                let mut subst = Vec::new();
                let t = self.import_opt(annot.as_ref(), &mut subst);
                (t, Vec::new(), t)
            }
        }
    }

    /// The signature's generics, as rigid variables: whatever the caller
    /// picks.
    fn rigid(&mut self, vars: &[TypeVar]) -> Vec<(TypeVar, Ty)> {
        vars.iter()
            .map(|v| {
                let name = v.name.clone().unwrap_or_else(|| "_".into());
                (v.clone(), self.e.rigid(name))
            })
            .collect()
    }

    fn import_opt(&mut self, t: Option<&Type>, subst: &mut Vec<(TypeVar, Ty)>) -> Ty {
        match t {
            Some(t) => self.e.import(t, subst),
            None => self.e.fresh(),
        }
    }

    // --- Binders ---

    fn define(&mut self, b: BinderId, scheme: LocalScheme) {
        self.binders[b.0 as usize] = Some(scheme);
    }

    /// Bind a pattern's binder to `ty`. The alternatives of an `|` share
    /// their binders, so a binder already bound must have the same type.
    fn bind_pattern(&mut self, b: BinderId, ty: Ty, span: Span) -> bool {
        match self.binders[b.0 as usize].as_ref().map(|s| s.ty) {
            Some(existing) => self.unify(existing, ty, span),
            None => {
                self.define(b, LocalScheme::mono(ty));
                true
            }
        }
    }

    // --- Diagnostics ---

    fn unify(&mut self, expected: Ty, given: Ty, span: Span) -> bool {
        match self.e.unify(expected, given) {
            Ok(()) => true,
            Err(err) => {
                let msg = self.unify_message(err);
                self.report.error(span, msg);
                false
            }
        }
    }

    fn unify_message(&self, err: UnifyError) -> String {
        match err {
            UnifyError::Mismatch {
                expected,
                given,
                arity,
            } => {
                let shown = self.e.show(&[expected, given]);
                let mut msg = format!("Type mismatch: expected '{}', got '{}'", shown[0], shown[1]);
                if let Some(a) = arity {
                    msg.push_str(&format!(
                        "\nExpected a function that takes {} argument(s), but got one that takes {}.",
                        a.expected, a.given
                    ));
                }
                msg
            }
            UnifyError::Recursive => {
                "Infinite type detected: this would need a type that contains itself, as `x(x)` does".to_string()
            }
        }
    }

    fn show(&self, t: Ty) -> String {
        self.e.show(&[t]).pop().unwrap_or_default()
    }

    fn error(&mut self) -> (Ty, ExprKind) {
        (self.e.fresh(), ExprKind::Error)
    }

    /// The definition of any type the module can reach.
    fn type_def(&self, name: &TypeName) -> Option<&TypeDef> {
        if *name.module() == self.env.key {
            return self.defs.types.get(name.name());
        }
        self.env.interface_of(name.module())?.types.get(name.name())
    }

    // --- Statements ---

    fn stmt(&mut self, s: &RStmt, ctx: Ctx<'_>) -> Stmt {
        match s {
            RStmt::Expr(x) => Stmt::Expr(self.expr(x, ctx)),
            RStmt::Let {
                pattern,
                annot,
                value,
                value_span,
                name: Some(binder),
            } => self.name_binding(*binder, pattern, annot.as_ref(), value, *value_span, ctx),
            RStmt::Let {
                pattern,
                annot,
                value,
                value_span,
                name: None,
            } => {
                let value = self.expr(value, ctx);
                if let Some(a) = annot {
                    let a = self.e.import(a, &mut Vec::new());
                    self.unify(a, value.ty, *value_span);
                }
                let pattern = self.pattern(pattern, value.ty, ctx);
                Stmt::Let { pattern, value }
            }
        }
    }

    /// `x = e`: `e` is checked a level deeper, so what it leaves open can be
    /// generalised. Inside a function literal `e`, `x` is the literal itself.
    fn name_binding(
        &mut self,
        binder: BinderId,
        pattern: &RPattern,
        annot: Option<&Type>,
        value: &RExpr,
        value_span: Span,
        ctx: Ctx<'_>,
    ) -> Stmt {
        let annotated = annot.map(|a| self.e.import(a, &mut Vec::new()));
        let level = self.e.enter();
        let self_ty = self.e.fresh();
        let pending = Pending {
            binder,
            ty: self_ty,
            outer: ctx.pending,
        };
        let inner = Ctx {
            pending: Some(&pending),
            ..ctx
        };
        let value = self.expr_hinted(value, annotated, inner);
        self.unify(self_ty, value.ty, value.span);
        let ty = match annotated {
            Some(a) => {
                self.unify(a, value.ty, value_span);
                a
            }
            None => value.ty,
        };
        let restricted = self.env.restricted.clone();
        let scheme = self.e.generalize(level, ty, &restricted);
        self.define(binder, scheme);
        let kind = match pattern.kind {
            RPatKind::Bind(b) => PatternKind::Bind(b),
            RPatKind::Wild
            | RPatKind::Int(_)
            | RPatKind::Float(_)
            | RPatKind::String(_)
            | RPatKind::Range(..)
            | RPatKind::Ctor { .. }
            | RPatKind::Tuple(_)
            | RPatKind::Array { .. }
            | RPatKind::Binary { .. }
            | RPatKind::Or(_)
            | RPatKind::Error => PatternKind::Wild,
        };
        Stmt::Let {
            pattern: Pattern {
                ty,
                span: pattern.span,
                kind,
            },
            value,
        }
    }

    // --- Expressions ---

    fn expr(&mut self, x: &RExpr, ctx: Ctx<'_>) -> Expr {
        let (ty, kind) = self.expr_kind(x, ctx);
        Expr {
            ty,
            span: x.span,
            kind,
        }
    }

    fn boxed(&mut self, x: &RExpr, ctx: Ctx<'_>) -> Box<Expr> {
        Box::new(self.expr(x, ctx))
    }

    /// Check `x` knowing the type it should have. Only a function literal
    /// uses the hint: its parameters take their types from it, so its body
    /// can read their fields.
    fn expr_hinted(&mut self, x: &RExpr, hint: Option<Ty>, ctx: Ctx<'_>) -> Expr {
        if let (Some(h), RKind::Lambda(l)) = (hint, &x.kind)
            && let Shape::Fn(params, _) = self.e.shape(h)
            && params.len() == l.params.len()
        {
            let (ty, lambda) = self.lambda(l, Some(params), ctx);
            return Expr {
                ty,
                span: x.span,
                kind: ExprKind::Lambda(lambda),
            };
        }
        self.expr(x, ctx)
    }

    fn int(&mut self, x: &RExpr, ctx: Ctx<'_>) -> Expr {
        let v = self.expr(x, ctx);
        let int = self.e.int();
        self.unify(int, v.ty, x.span);
        v
    }

    fn expr_kind(&mut self, x: &RExpr, ctx: Ctx<'_>) -> (Ty, ExprKind) {
        match &x.kind {
            RKind::Int(i) => (self.e.int(), ExprKind::Int(*i)),
            RKind::Float(f) => (self.e.float(), ExprKind::Float(*f)),
            RKind::String(s) => (self.e.string(), ExprKind::String(s.clone())),
            RKind::Interp(parts) => {
                let parts = parts
                    .iter()
                    .map(|p| match p {
                        RPart::Text(s) => InterpPart::Text(s.clone()),
                        RPart::Expr(e) => InterpPart::Expr(self.expr(e, ctx)),
                    })
                    .collect();
                (self.e.string(), ExprKind::Interp(parts))
            }
            RKind::Binary(segs) => {
                let segs = segs
                    .iter()
                    .map(|seg| {
                        let value = self.expr(&seg.value, ctx);
                        let expected = match seg.unit {
                            RUnit::Bits(_) => self.e.int(),
                            RUnit::Bytes(_) => self.e.binary(),
                            RUnit::Utf8 => self.e.string(),
                        };
                        self.unify(expected, value.ty, seg.value.span);
                        let unit = match &seg.unit {
                            RUnit::Bits(b) => {
                                BinUnit::Bits(b.as_ref().map(|b| Box::new(self.int(b, ctx))))
                            }
                            RUnit::Bytes(b) => {
                                BinUnit::Bytes(b.as_ref().map(|b| Box::new(self.int(b, ctx))))
                            }
                            RUnit::Utf8 => BinUnit::Utf8,
                        };
                        BinSeg { value, unit }
                    })
                    .collect();
                (self.e.binary(), ExprKind::Binary(segs))
            }
            RKind::Ref { r, at } => {
                let (ty, kind) = self.reference(r, x.span, ctx);
                self.hovers.push((*at, ty));
                (ty, kind)
            }
            RKind::Lambda(l) => {
                let (ty, lambda) = self.lambda(l, None, ctx);
                (ty, ExprKind::Lambda(lambda))
            }
            RKind::Call { callee, args } => self.call(callee, args, x.span, ctx),
            RKind::Construct(c) => self.construct(c, ctx),
            RKind::Tuple(xs) => {
                let elems: Vec<Expr> = xs.iter().map(|e| self.expr(e, ctx)).collect();
                let tys = elems.iter().map(|e| e.ty).collect();
                (self.e.tuple(tys), ExprKind::Tuple(elems))
            }
            RKind::TupleIndex {
                tuple,
                index,
                index_span,
            } => {
                let tuple = self.boxed(tuple, ctx);
                match self.e.shape(tuple.ty) {
                    Shape::Tuple(elems) => match elems.get(*index as usize) {
                        Some(&ty) => (
                            ty,
                            ExprKind::TupleIndex {
                                tuple,
                                index: *index,
                            },
                        ),
                        None => {
                            self.report.error(
                                *index_span,
                                format!(
                                    "Tuple index {index} is out of range, because the tuple has {} elements",
                                    elems.len()
                                ),
                            );
                            self.error()
                        }
                    },
                    Shape::Var(_) | Shape::Con(..) | Shape::Fn(..) => {
                        self.report.error(
                            *index_span,
                            format!("Can't use .{index} here, because the value isn't a tuple"),
                        );
                        self.error()
                    }
                }
            }
            RKind::Field {
                record,
                label,
                label_span,
            } => {
                let record = self.boxed(record, ctx);
                match self.field_access(record.ty, label, *label_span, ctx) {
                    Some((ty, at)) => (
                        ty,
                        ExprKind::Field {
                            record,
                            label: label.clone(),
                            at,
                        },
                    ),
                    None => self.error(),
                }
            }
            RKind::Array(elems) => {
                let elem = self.e.fresh();
                let elems = elems
                    .iter()
                    .map(|el| match el {
                        RElem::Elem(v) => {
                            let v = self.expr(v, ctx);
                            self.unify(elem, v.ty, v.span);
                            ArrayElem::Elem(v)
                        }
                        RElem::Spread(v) => {
                            let v = self.expr(v, ctx);
                            let arr = self.e.array(elem);
                            self.unify(arr, v.ty, v.span);
                            ArrayElem::Spread(v)
                        }
                    })
                    .collect();
                (self.e.array(elem), ExprKind::Array(elems))
            }
            RKind::Index { array, index } => {
                let array = self.boxed(array, ctx);
                let elem = self.e.fresh();
                let arr = self.e.array(elem);
                self.unify(arr, array.ty, array.span);
                let index = Box::new(self.int(index, ctx));
                (self.e.option(elem), ExprKind::Index { array, index })
            }
            RKind::Slice { array, start, end } => {
                let array = self.boxed(array, ctx);
                let elem = self.e.fresh();
                let arr = self.e.array(elem);
                self.unify(arr, array.ty, array.span);
                let start = Box::new(self.int(start, ctx));
                let end = Box::new(self.int(end, ctx));
                // A slice fails when its range isn't inside the array.
                let slice = self.e.array(elem);
                let nil = self.e.nil();
                (
                    self.e.result(slice, nil),
                    ExprKind::Slice { array, start, end },
                )
            }
            RKind::Range { start, end } => {
                let start = Box::new(self.int(start, ctx));
                let end = Box::new(self.int(end, ctx));
                let int = self.e.int();
                (self.e.array(int), ExprKind::Range { start, end })
            }
            RKind::BinOp { op, left, right } => self.binop(*op, left, right, x.span, ctx),
            RKind::Unary { op, operand } => {
                let operand = self.boxed(operand, ctx);
                let ty = match op {
                    UnaryOp::Not => {
                        let b = self.e.bool();
                        self.unify(b, operand.ty, operand.span);
                        b
                    }
                    UnaryOp::Neg => {
                        let n = self.e.fresh_constrained(Constraint::Numeric);
                        self.unify(n, operand.ty, operand.span);
                        n
                    }
                };
                (ty, ExprKind::Unary { op: *op, operand })
            }
            RKind::If {
                cond,
                then,
                els,
                then_span,
                els_span,
            } => {
                let cond = self.boxed(cond, ctx);
                let b = self.e.bool();
                self.unify(b, cond.ty, cond.span);
                let then = self.boxed(then, ctx);
                let els = self.boxed(els, ctx);
                let ret = self.e.fresh();
                self.unify(ret, then.ty, *then_span);
                self.unify(ret, els.ty, *els_span);
                (ret, ExprKind::If { cond, then, els })
            }
            RKind::Match { subject, arms } => {
                let subject = self.boxed(subject, ctx);
                let result = self.e.fresh();
                let arms = arms
                    .iter()
                    .map(|arm| {
                        let pattern = self.pattern(&arm.pattern, subject.ty, ctx);
                        let guard = arm.guard.as_ref().map(|(g, gspan)| {
                            let g = self.expr(g, ctx);
                            let b = self.e.bool();
                            self.unify(b, g.ty, *gspan);
                            g
                        });
                        let body = self.expr(&arm.body, ctx);
                        self.unify(result, body.ty, arm.body_span);
                        Arm {
                            pattern,
                            guard,
                            body,
                        }
                    })
                    .collect();
                (result, ExprKind::Match { subject, arms })
            }
            RKind::OrElse {
                value,
                receiver,
                fallback,
                fallback_span,
            } => self.or_else(value, *receiver, fallback, *fallback_span, ctx),
            RKind::Block { stmts, tail } => {
                let stmts: Vec<Stmt> = stmts.iter().map(|s| self.stmt(s, ctx)).collect();
                let tail = tail.as_ref().map(|t| self.boxed(t, ctx));
                let ty = match &tail {
                    Some(t) => t.ty,
                    None => self.e.nil(),
                };
                (ty, ExprKind::Block(Block { stmts, tail }))
            }
            RKind::Error => self.error(),
        }
    }

    fn reference(&mut self, r: &Ref, span: Span, ctx: Ctx<'_>) -> (Ty, ExprKind) {
        match r {
            Ref::Local(b) => {
                let ty = match ctx.pending(*b) {
                    Some(t) => t,
                    None => match self.binders[b.0 as usize].clone() {
                        Some(s) => self.e.instantiate(&s),
                        None => return self.unbound(span),
                    },
                };
                (ty, ExprKind::Var(Var::Local(*b)))
            }
            Ref::Own(d, var) => {
                let ty = match ctx.group(*d) {
                    Some(t) => t,
                    None => match self.decls[d.0 as usize].clone() {
                        Some(s) => self.e.instantiate(&s),
                        None => return self.unbound(span),
                    },
                };
                (ty, ExprKind::Var(var.clone()))
            }
            Ref::Foreign(var, scheme) => (
                self.e.instantiate_scheme(scheme),
                ExprKind::Var(var.clone()),
            ),
            Ref::Ctor(ctor, scheme) => (
                self.e.instantiate_scheme(scheme),
                ExprKind::Var(Var::Ctor(ctor.clone())),
            ),
        }
    }

    /// A name read before anything gave it a type. Resolution orders the
    /// module so this can't happen; if it ever does, it is an error, never a
    /// guess.
    fn unbound(&mut self, span: Span) -> (Ty, ExprKind) {
        self.report.error(
            span,
            "internal compiler error: this name was read before it had a type",
        );
        self.error()
    }

    fn binop(
        &mut self,
        op: BinaryOp,
        left: &RExpr,
        right: &RExpr,
        span: Span,
        ctx: Ctx<'_>,
    ) -> (Ty, ExprKind) {
        use BinaryOp as B;
        let (constraint, compares, op) = match op {
            B::And | B::Or => {
                let l = self.boxed(left, ctx);
                let b = self.e.bool();
                self.unify(b, l.ty, left.span);
                let r = self.boxed(right, ctx);
                self.unify(b, r.ty, right.span);
                let kind = if op == B::And {
                    ExprKind::And(l, r)
                } else {
                    ExprKind::Or(l, r)
                };
                return (b, kind);
            }
            B::Eq | B::Ne => {
                let l = self.boxed(left, ctx);
                let r = self.boxed(right, ctx);
                self.unify(l.ty, r.ty, span);
                let op = if op == B::Eq { BinOp::Eq } else { BinOp::Ne };
                return (
                    self.e.bool(),
                    ExprKind::BinOp {
                        op,
                        left: l,
                        right: r,
                    },
                );
            }
            B::Add => (Constraint::Addable, false, BinOp::Add),
            B::Sub => (Constraint::Numeric, false, BinOp::Sub),
            B::Mul => (Constraint::Numeric, false, BinOp::Mul),
            B::Div => (Constraint::Numeric, false, BinOp::Div),
            B::Mod => (Constraint::Numeric, false, BinOp::Mod),
            B::Lt => (Constraint::Numeric, true, BinOp::Lt),
            B::Gt => (Constraint::Numeric, true, BinOp::Gt),
            B::Le => (Constraint::Numeric, true, BinOp::Le),
            B::Ge => (Constraint::Numeric, true, BinOp::Ge),
        };
        let l = self.boxed(left, ctx);
        let r = self.boxed(right, ctx);
        let operand = self.e.fresh_constrained(constraint);
        self.unify(operand, l.ty, left.span);
        self.unify(operand, r.ty, right.span);
        let ty = if compares { self.e.bool() } else { operand };
        (
            ty,
            ExprKind::BinOp {
                op,
                left: l,
                right: r,
            },
        )
    }

    fn lambda(&mut self, l: &RLambda, hints: Option<Vec<Ty>>, ctx: Ctx<'_>) -> (Ty, Lambda) {
        let level = self.e.enter();
        let mut subst = self.rigid(&l.vars);
        let mut params = Vec::with_capacity(l.params.len());
        for (i, (b, annot)) in l.params.iter().enumerate() {
            let ty = match annot {
                Some(t) => self.e.import(t, &mut subst),
                None => match &hints {
                    Some(h) if i < h.len() => h[i],
                    _ => self.e.fresh(),
                },
            };
            self.define(*b, LocalScheme::mono(ty));
            params.push(ty);
        }
        let body = self.expr(&l.body, ctx);
        let ret = match &l.ret {
            Some(t) => {
                let a = self.e.import(t, &mut subst);
                self.unify(a, body.ty, l.body_span);
                a
            }
            None => body.ty,
        };
        self.e.leave(level);
        let ty = self.e.fun(params, ret);
        (
            ty,
            Lambda {
                params: l.params.iter().map(|(b, _)| *b).collect(),
                body: Box::new(body),
                captures: l.captures.clone(),
                self_name: l.self_name,
            },
        )
    }

    fn call(&mut self, callee: &RExpr, args: &[RArg], span: Span, ctx: Ctx<'_>) -> (Ty, ExprKind) {
        let callee = self.expr(callee, ctx);
        let (params, ret) = match self.e.callable(callee.ty, args.len()) {
            Ok(pr) => pr,
            Err(NotCallable::Arity { params, ret }) => {
                self.report.error(
                    span,
                    format!("Expected {} argument(s), got {}", params.len(), args.len()),
                );
                (params, ret)
            }
            Err(NotCallable::NotFn) => {
                let s = self.show(callee.ty);
                self.report.error(
                    span,
                    format!("This value of type '{s}' is not a function and cannot be called"),
                );
                for a in args {
                    self.expr(&a.value, ctx);
                }
                return self.error();
            }
        };
        let args = args
            .iter()
            .map(|a| {
                let param = a
                    .fills
                    .then(|| params.get(a.slot as usize).copied())
                    .flatten();
                let value = self.expr_hinted(&a.value, param, ctx);
                if let Some(p) = param {
                    self.unify(p, value.ty, value.span);
                }
                Arg {
                    slot: a.slot,
                    value,
                }
            })
            .collect();
        (
            ret,
            ExprKind::Call {
                callee: Box::new(callee),
                args,
            },
        )
    }

    /// A constructor applied to its fields. The spread is checked first, so
    /// its type settles the constructor's parameters before a function
    /// literal argument takes a hint from them.
    fn construct(&mut self, c: &RConstruct, ctx: Ctx<'_>) -> (Ty, ExprKind) {
        let RConstruct {
            ctor,
            scheme,
            labels,
            args,
            spread,
            at,
        } = c;
        let spread = spread.as_ref();
        let ty = self.e.instantiate_scheme(scheme);
        self.hovers.push((*at, ty));
        let (params, result) = match self.e.shape(ty) {
            Shape::Fn(params, ret) => (params, ret),
            Shape::Var(_) | Shape::Con(..) | Shape::Tuple(_) => (Vec::new(), ty),
        };
        let spread = spread.map(|(at, base)| {
            let base = self.expr(base, ctx);
            self.unify(result, base.ty, base.span);
            let given: Vec<u32> = args.iter().filter(|a| a.fills).map(|a| a.slot).collect();
            let unfilled: Vec<usize> = (0..labels.len())
                .filter(|i| !given.contains(&(*i as u32)))
                .collect();
            let fills = self.spread_fills(labels, result, &unfilled, base.span);
            Box::new(Spread {
                base,
                at: *at,
                fills,
            })
        });
        let mut checked: Vec<Arg> = Vec::with_capacity(args.len());
        let mut order: Vec<usize> = (0..args.len()).collect();
        order.sort_by_key(|&i| args[i].slot);
        let mut by_index: Vec<Option<Arg>> = (0..args.len()).map(|_| None).collect();
        for i in order {
            let a = &args[i];
            let param = a
                .fills
                .then(|| params.get(a.slot as usize).copied())
                .flatten();
            let value = self.expr_hinted(&a.value, param, ctx);
            if let Some(p) = param {
                self.unify(p, value.ty, value.span);
            }
            by_index[i] = Some(Arg {
                slot: a.slot,
                value,
            });
        }
        checked.extend(by_index.into_iter().flatten());
        (
            result,
            ExprKind::Construct {
                ctor: ctor.clone(),
                args: checked,
                spread,
            },
        )
    }

    /// Where `base.field` reads each field a spread fills. A field the type's
    /// variants don't all have can't be filled, because which variant `base`
    /// is isn't known until the program runs.
    fn spread_fills(
        &mut self,
        labels: &[Name],
        result: Ty,
        unfilled: &[usize],
        span: Span,
    ) -> Vec<(u32, FieldAt)> {
        let Shape::Con(type_name, type_args) = self.e.shape(result) else {
            return Vec::new();
        };
        let Some(def) = self.type_def(&type_name).cloned() else {
            return Vec::new();
        };
        let mut fills = Vec::new();
        for &i in unfilled {
            let Some(label) = labels.get(i) else { continue };
            match self.field_in_variants(&def, &type_args, label, span) {
                Ok((_, at)) => fills.push((i as u32, at)),
                Err(FieldMissing::On(variant)) => {
                    let why = missing_on(&variant, type_name.name());
                    self.report.error(
                        span,
                        format!("The spread cannot fill field '{label}', which {why}; write '{label}:' in the call"),
                    );
                }
                Err(FieldMissing::NotCustom) => {}
            }
        }
        fills
    }

    /// `.field` on a value of type `receiver`. Which variant the value is
    /// isn't known until the program runs, so every variant must hold the
    /// field, at one type, wherever it sits in each.
    fn field_access(
        &mut self,
        receiver: Ty,
        label: &Name,
        span: Span,
        ctx: Ctx<'_>,
    ) -> Option<(Ty, FieldAt)> {
        let (type_name, type_args) = match self.e.shape(receiver) {
            Shape::Con(n, a) => (n, a),
            Shape::Var(_) => {
                self.report.error(
                    span,
                    format!(
                        "Can't access field '{label}' because the value's type isn't known here. Add a type annotation"
                    ),
                );
                return None;
            }
            Shape::Fn(..) | Shape::Tuple(_) => {
                let s = self.show(receiver);
                self.report
                    .error(span, format!("Type '{s}' has no field '{label}'"));
                return None;
            }
        };
        let Some(def) = self.type_def(&type_name).cloned() else {
            self.report.error(
                span,
                format!("Type '{}' has no field '{label}'", type_name.name()),
            );
            return None;
        };
        match self.field_in_variants(&def, &type_args, label, span) {
            Ok(found) => {
                if let Some(site) = field_site(&def, label) {
                    self.report.uses.push(Use {
                        span,
                        kind: UseKind::Unqualified,
                        target: site,
                        owner: ctx.owner.clone(),
                    });
                }
                self.hovers.push((span, found.0));
                Some(found)
            }
            Err(FieldMissing::NotCustom) => {
                self.report.error(
                    span,
                    format!("Type '{}' has no accessible fields", type_name.name()),
                );
                None
            }
            Err(FieldMissing::On(variant)) => {
                let why = missing_on(&variant, type_name.name());
                self.report.error(span, format!("Field '{label}' {why}"));
                None
            }
        }
    }

    /// Where field `label` sits in each variant of `def`, and its type. The
    /// field types of the variants are unified, a mismatch reported at
    /// `span`.
    fn field_in_variants(
        &mut self,
        def: &TypeDef,
        args: &[Ty],
        label: &str,
        span: Span,
    ) -> Result<(Ty, FieldAt), FieldMissing> {
        let variants = def.variants().ok_or(FieldMissing::NotCustom)?;
        if variants.is_empty() {
            return Err(FieldMissing::NotCustom);
        }
        let mut subst: Vec<_> = def
            .params
            .iter()
            .cloned()
            .zip(args.iter().copied())
            .collect();
        let mut ty: Option<Ty> = None;
        let mut slots = Vec::with_capacity(variants.len());
        for v in variants {
            let Some((i, f)) = v
                .fields
                .iter()
                .enumerate()
                .find(|(_, f)| &*f.label == label)
            else {
                return Err(FieldMissing::On(v.name.clone()));
            };
            slots.push(i as u32);
            let fty = self.e.import(&f.ty, &mut subst);
            match ty {
                Some(t) => {
                    self.unify(t, fty, span);
                }
                None => ty = Some(fty),
            }
        }
        let ty = ty.ok_or(FieldMissing::NotCustom)?;
        let at = match slots.first() {
            Some(&first) if slots.iter().all(|&s| s == first) => FieldAt::Same(first),
            Some(_) | None => FieldAt::PerVariant(
                slots
                    .iter()
                    .enumerate()
                    .map(|(v, &s)| (v as u16, s))
                    .collect(),
            ),
        };
        Ok((ty, at))
    }

    fn or_else(
        &mut self,
        value: &RExpr,
        receiver: Option<(BinderId, Span)>,
        fallback: &RExpr,
        fallback_span: Span,
        ctx: Ctx<'_>,
    ) -> (Ty, ExprKind) {
        let value = self.boxed(value, ctx);
        let success = self.e.fresh();
        let known = self.e.known().clone();
        let shape = match self.e.shape(value.ty) {
            Shape::Con(n, _) if n == known.option => Some(OrShape::Option),
            Shape::Con(n, _) if n == known.result => Some(OrShape::Result),
            Shape::Con(..) | Shape::Var(_) | Shape::Fn(..) | Shape::Tuple(_) => None,
        };
        let Some(shape) = shape else {
            let s = self.show(value.ty);
            self.report.error(
                value.span,
                format!("'or' requires the left side to be Option(_) or Result(_, _), got '{s}'"),
            );
            if let Some((b, _)) = receiver {
                let t = self.e.fresh();
                self.define(b, LocalScheme::mono(t));
                self.refused.push(b);
            }
            self.expr(fallback, ctx);
            return self.error();
        };
        let error = match shape {
            OrShape::Option => {
                if let Some((b, span)) = receiver {
                    self.report.error(
                        span,
                        "'or' on an Option can't bind a value, because None carries nothing",
                    );
                    let t = self.e.fresh();
                    self.define(b, LocalScheme::mono(t));
                    self.refused.push(b);
                }
                let expected = self.e.option(success);
                self.unify(expected, value.ty, value.span);
                None
            }
            OrShape::Result => {
                let err = self.e.fresh();
                let expected = self.e.result(success, err);
                self.unify(expected, value.ty, value.span);
                receiver.map(|(b, _)| {
                    self.define(b, LocalScheme::mono(err));
                    b
                })
            }
        };
        let fallback = self.boxed(fallback, ctx);
        self.unify(success, fallback.ty, fallback_span);
        (
            success,
            ExprKind::OrElse {
                shape,
                value,
                error,
                fallback,
            },
        )
    }

    // --- Patterns ---

    /// Type `p` against `expected`. A part that doesn't fit becomes an
    /// `Error` node, so nothing checks a mistyped pattern for exhaustiveness.
    fn pattern(&mut self, p: &RPattern, expected: Ty, ctx: Ctx<'_>) -> Pattern {
        let at = |kind| Pattern {
            ty: expected,
            span: p.span,
            kind,
        };
        let fits = |i: &mut Self, t: Ty, kind: PatternKind| {
            if i.unify(expected, t, p.span) {
                kind
            } else {
                PatternKind::Error
            }
        };
        match &p.kind {
            RPatKind::Wild => at(PatternKind::Wild),
            RPatKind::Error => at(PatternKind::Error),
            RPatKind::Bind(b) => {
                let kind = if self.bind_pattern(*b, expected, p.span) {
                    PatternKind::Bind(*b)
                } else {
                    PatternKind::Error
                };
                at(kind)
            }
            RPatKind::Int(i) => {
                let t = self.e.int();
                at(fits(self, t, PatternKind::Int(*i)))
            }
            RPatKind::Float(f) => {
                let t = self.e.float();
                at(fits(self, t, PatternKind::Float(*f)))
            }
            RPatKind::String(s) => {
                let t = self.e.string();
                at(fits(self, t, PatternKind::String(s.clone())))
            }
            RPatKind::Range(lo, hi) => {
                let t = self.e.int();
                at(fits(self, t, PatternKind::Range(*lo, *hi)))
            }
            RPatKind::Tuple(elems) => {
                let fresh: Vec<Ty> = elems.iter().map(|_| self.e.fresh()).collect();
                let tup = self.e.tuple(fresh.clone());
                let ok = self.unify(expected, tup, p.span);
                let elems = elems
                    .iter()
                    .zip(fresh)
                    .map(|(el, t)| self.pattern(el, t, ctx))
                    .collect();
                at(if ok {
                    PatternKind::Tuple(elems)
                } else {
                    PatternKind::Error
                })
            }
            RPatKind::Array { prefix, rest } => {
                let elem = self.e.fresh();
                let arr = self.e.array(elem);
                let ok = self.unify(expected, arr, p.span);
                let prefix = prefix
                    .iter()
                    .map(|el| self.pattern(el, elem, ctx))
                    .collect();
                if let crate::typed::Rest::Bind(b) = rest {
                    let rest_ty = self.e.array(elem);
                    self.bind_pattern(*b, rest_ty, p.span);
                }
                at(if ok {
                    PatternKind::Array {
                        prefix,
                        rest: *rest,
                    }
                } else {
                    PatternKind::Error
                })
            }
            RPatKind::Binary { segs, rest } => {
                let bin = self.e.binary();
                let ok = self.unify(expected, bin, p.span);
                let segs = segs
                    .iter()
                    .map(|seg| match seg {
                        RBinSegPat::Text(s) => BinSegPat::Text(s.clone()),
                        RBinSegPat::Utf8(v) => {
                            let int = self.e.int();
                            BinSegPat::Utf8(self.pattern(v, int, ctx))
                        }
                        RBinSegPat::Bits { bits, value } => {
                            let bits = bits.as_ref().map(|b| self.int(b, ctx));
                            let int = self.e.int();
                            BinSegPat::Bits {
                                bits,
                                value: self.pattern(value, int, ctx),
                            }
                        }
                        RBinSegPat::Bytes { bytes, value } => {
                            let bytes = bytes.as_ref().map(|b| self.int(b, ctx));
                            let bin = self.e.binary();
                            BinSegPat::Bytes {
                                bytes,
                                value: self.pattern(value, bin, ctx),
                            }
                        }
                    })
                    .collect();
                if let crate::typed::Rest::Bind(b) = rest {
                    let rest_ty = self.e.binary();
                    self.bind_pattern(*b, rest_ty, p.span);
                }
                at(if ok {
                    PatternKind::Binary { segs, rest: *rest }
                } else {
                    PatternKind::Error
                })
            }
            RPatKind::Or(alts) => {
                let alts = alts
                    .iter()
                    .map(|a| self.pattern(a, expected, ctx))
                    .collect();
                at(PatternKind::Or(alts))
            }
            RPatKind::Ctor {
                ctor,
                scheme,
                fields,
                at: name_at,
            } => {
                let ty = self.e.instantiate_scheme(scheme);
                self.hovers.push((*name_at, ty));
                match self.e.shape(ty) {
                    Shape::Fn(params, ret) => {
                        let ok = self.unify(expected, ret, p.span);
                        let fields = fields
                            .iter()
                            .enumerate()
                            .map(|(i, f)| {
                                let t = params.get(i).copied().unwrap_or_else(|| self.e.fresh());
                                self.pattern(f, t, ctx)
                            })
                            .collect();
                        at(if ok {
                            PatternKind::Ctor {
                                ctor: ctor.clone(),
                                fields,
                            }
                        } else {
                            PatternKind::Error
                        })
                    }
                    Shape::Var(_) | Shape::Con(..) | Shape::Tuple(_) => at(fits(
                        self,
                        ty,
                        PatternKind::Ctor {
                            ctor: ctor.clone(),
                            fields: Vec::new(),
                        },
                    )),
                }
            }
        }
    }
}

/// Why a type has no field to read.
enum FieldMissing {
    NotCustom,
    On(Name),
}

/// The end of a sentence that names a field some variant lacks.
fn missing_on(variant: &str, type_name: &str) -> String {
    format!("is not present on every variant of '{type_name}' (missing on '{variant}')")
}

/// The field's definition: where it first appears among the type's variants.
fn field_site(def: &TypeDef, label: &str) -> Option<Site> {
    let f = def
        .variants()?
        .iter()
        .flat_map(|v| &v.fields)
        .find(|f| &*f.label == label)?;
    Some(Site {
        module: def.name.module().clone(),
        span: f.span,
        entity: EntityKind::Field,
    })
}
