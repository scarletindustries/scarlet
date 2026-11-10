//! One module's typed tree, turned into typed IR. The checker has made every
//! decision already, so this only chooses how each one is built: which
//! operator a `+` is, the `match` an `or` or a per-variant field read becomes,
//! the closure a function used as a value is, and the order arguments run in.

use std::collections::HashMap;

use scarlet_check::typed::{
    self as t, Arg, ArrayElem, BinOp, BinSeg, BinSegPat, BinUnit, BinderId, Block, Expr, ExprKind,
    FieldAt, FnBody, InterpPart, OrShape, Pattern, PatternKind, Rest, Stmt, TypedModule, Var,
};
use scarlet_ir::core_ir::{Const, FuncIdx, PrimOp};
use scarlet_ir::rty::{Prim, RTy};
use scarlet_syntax::ast::UnaryOp;
use scarlet_types::name::{CtorName, ValueName};
use scarlet_types::ty::Type;

use crate::program::Builder;
use crate::typed_ir::{
    BindingId, PatRest, TypedArm, TypedArrayElem, TypedBinPatSeg, TypedBinSeg, TypedBind,
    TypedCallee, TypedExpr, TypedFn, TypedInterpPart, TypedPat, ValueRef,
};

/// Give each of the module's functions its `FuncIdx`, and each module-level
/// value its global slot, before any body is made, so a body can refer to
/// anything.
pub(crate) fn register(b: &mut Builder<'_>, m: &TypedModule) {
    for f in &m.functions {
        if let FnBody::Expr(_) = f.body {
            let idx = b.reserve_fn();
            b.fn_of.insert(
                ValueName {
                    module: m.key.clone(),
                    name: f.name.clone(),
                },
                idx,
            );
        }
    }
    for c in &m.consts {
        let slot = b.global();
        b.const_slot.insert(
            ValueName {
                module: m.key.clone(),
                name: c.name.clone(),
            },
            slot,
        );
    }
    for (i, binder) in m.binders.iter().enumerate() {
        if binder.module_scope {
            let slot = b.global();
            b.binder_slot
                .insert((m.key.clone(), BinderId(i as u32)), slot);
        }
    }
}

/// Make every function body of `m`, and return its top level: its consts and
/// statements, in the order they run.
pub(crate) fn module(b: &mut Builder<'_>, m: &TypedModule) -> TypedFn {
    let mut mx = Mx { b, m };
    for f in &m.functions {
        let FnBody::Expr(body) = &f.body else {
            continue;
        };
        let name = ValueName {
            module: m.key.clone(),
            name: f.name.clone(),
        };
        let Some(&idx) = mx.b.fn_of.get(&name) else {
            continue;
        };
        let ty = mx.ty(f.ty);
        let ret = mx.b.pool.fun_ret(ty).unwrap_or(mx.b.temps.int);
        let mut cx = Fx::new(Some(name));
        let params = f.params.iter().map(|&p| mx.bind(&mut cx, p)).collect();
        let body = mx.expr(&mut cx, body);
        let typed = TypedFn {
            params,
            ret,
            body,
            binds: cx.next,
        };
        mx.b.fill_fn(idx, &m.key, &f.name, typed);
    }

    let mut cx = Fx::new(None);
    let mut frames: Vec<Frame> = Vec::new();
    for c in &m.consts {
        let init = mx.expr(&mut cx, &c.value);
        let slot = mx.b.const_slot.get(&ValueName {
            module: m.key.clone(),
            name: c.name.clone(),
        });
        let bind = TypedBind {
            id: cx.fresh(),
            named: true,
            ty: init.ty(),
            global: slot.copied(),
        };
        frames.push(Frame::Let(bind, init));
    }
    // A script's last expression is its value, which a REPL shows.
    let (last, init) = match m.statements.split_last() {
        Some((Stmt::Expr(e), init)) => (Some(e), init),
        Some(_) | None => (None, &m.statements[..]),
    };
    for s in init {
        mx.stmt(&mut cx, s, &mut frames);
    }
    let tail = match last {
        Some(e) => mx.expr(&mut cx, e),
        None => {
            let nil = mx.nil_ty();
            TypedExpr::Nil { ty: nil }
        }
    };
    let ret = tail.ty();
    let body = fold(frames, tail);
    TypedFn {
        params: Vec::new(),
        ret,
        body,
        binds: cx.next,
    }
}

/// One step of a block's spine.
enum Frame {
    Let(TypedBind, TypedExpr),
    Seq(TypedExpr),
}

fn fold(frames: Vec<Frame>, tail: TypedExpr) -> TypedExpr {
    frames.into_iter().rev().fold(tail, |body, f| {
        let ty = body.ty();
        match f {
            Frame::Let(bind, init) => TypedExpr::Let {
                ty,
                bind,
                init: Box::new(init),
                body: Box::new(body),
            },
            Frame::Seq(effect) => TypedExpr::Seq {
                ty,
                effect: Box::new(effect),
                body: Box::new(body),
            },
        }
    })
}

/// The function being made.
struct Fx {
    binds: HashMap<BinderId, (BindingId, RTy)>,
    next: u32,
    /// The enclosing function's locals this one reads, by capture index.
    captures: Vec<BinderId>,
    /// The binder that means this closure itself.
    self_name: Option<BinderId>,
    /// The module function this is, which calls itself directly.
    self_fn: Option<ValueName>,
}

impl Fx {
    fn new(self_fn: Option<ValueName>) -> Self {
        Fx {
            binds: HashMap::new(),
            next: 0,
            captures: Vec::new(),
            self_name: None,
            self_fn,
        }
    }

    fn fresh(&mut self) -> BindingId {
        let id = BindingId(self.next);
        self.next += 1;
        id
    }
}

struct Mx<'b, 'd, 'm> {
    b: &'b mut Builder<'d>,
    m: &'m TypedModule,
}

impl Mx<'_, '_, '_> {
    fn ty(&mut self, t: scarlet_types::engine::Ty) -> RTy {
        let ty = self.m.types.get(t);
        self.b.rty(&ty)
    }

    fn final_ty(&self, t: scarlet_types::engine::Ty) -> Type {
        self.m.types.get(t)
    }

    fn nil_ty(&mut self) -> RTy {
        let n = self.b.known.nil.clone();
        self.b.rty(&Type::named(n, vec![]))
    }

    fn int_const(&mut self, i: i64) -> TypedExpr {
        let value = self.b.constant(Const::Int(i));
        TypedExpr::Const {
            ty: self.b.temps.int,
            value,
        }
    }

    /// A new binding for the source binder `binder`.
    fn bind(&mut self, cx: &mut Fx, binder: BinderId) -> TypedBind {
        if let Some(&(id, ty)) = cx.binds.get(&binder) {
            return TypedBind {
                id,
                named: true,
                ty,
                global: self.global_of(binder),
            };
        }
        let ty = self.ty(self.m.binder(binder).ty);
        let id = cx.fresh();
        cx.binds.insert(binder, (id, ty));
        TypedBind {
            id,
            named: true,
            ty,
            global: self.global_of(binder),
        }
    }

    fn global_of(&self, binder: BinderId) -> Option<scarlet_ir::core_ir::GlobalSlot> {
        self.b
            .binder_slot
            .get(&(self.m.key.clone(), binder))
            .copied()
    }

    fn temp(&mut self, cx: &mut Fx, ty: RTy) -> TypedBind {
        TypedBind {
            id: cx.fresh(),
            named: false,
            ty,
            global: None,
        }
    }

    fn var(&self, b: &TypedBind) -> TypedExpr {
        TypedExpr::Var {
            ty: b.ty,
            place: ValueRef::Local(b.id),
        }
    }

    /// Where a local reads from in the function being made.
    fn load(&mut self, cx: &Fx, binder: BinderId, ty: RTy) -> TypedExpr {
        let place = if let Some(&(id, _)) = cx.binds.get(&binder) {
            ValueRef::Local(id)
        } else if let Some(slot) = self.global_of(binder) {
            ValueRef::Global(slot)
        } else if let Some(i) = cx.captures.iter().position(|&c| c == binder) {
            ValueRef::Capture(scarlet_ir::core_ir::CaptureIdx(i as i32))
        } else {
            ValueRef::SelfClosure
        };
        TypedExpr::Var { ty, place }
    }

    fn expr(&mut self, cx: &mut Fx, e: &Expr) -> TypedExpr {
        let ty = self.ty(e.ty);
        match &e.kind {
            ExprKind::Int(i) => {
                let value = self.b.constant(Const::Int(*i));
                TypedExpr::Const { ty, value }
            }
            ExprKind::Float(f) => {
                let value = self.b.constant(Const::Float(*f));
                TypedExpr::Const { ty, value }
            }
            ExprKind::String(s) => {
                let value = self.b.constant(Const::String(s.clone()));
                TypedExpr::Const { ty, value }
            }
            ExprKind::Interp(parts) => {
                if parts.is_empty() {
                    let value = self.b.constant(Const::String(String::new()));
                    return TypedExpr::Const { ty, value };
                }
                let parts = parts
                    .iter()
                    .map(|p| match p {
                        InterpPart::Text(s) => {
                            TypedInterpPart::Str(self.b.constant(Const::String(s.clone())))
                        }
                        InterpPart::Expr(x) => TypedInterpPart::Expr(self.expr(cx, x)),
                    })
                    .collect();
                TypedExpr::Interp { ty, parts }
            }
            ExprKind::Binary(segs) => self.binary_literal(cx, segs, ty),
            ExprKind::Var(v) => self.var_value(cx, v, e, ty),
            ExprKind::Lambda(l) => self.lambda(cx, l, ty),
            ExprKind::Call { callee, args } => self.call(cx, callee, args, ty),
            ExprKind::Construct { ctor, args, spread } => {
                self.construct(cx, ctor, args, spread.as_deref(), e, ty)
            }
            ExprKind::Tuple(xs) => TypedExpr::Tuple {
                ty,
                elems: xs.iter().map(|x| self.expr(cx, x)).collect(),
            },
            ExprKind::TupleIndex { tuple, index } => TypedExpr::TupleIndex {
                ty,
                recv: Box::new(self.expr(cx, tuple)),
                idx: *index,
            },
            ExprKind::Field { record, at, .. } => {
                let rec_ty = self.final_ty(record.ty);
                let recv = self.expr(cx, record);
                self.field(cx, recv, &rec_ty, at, ty)
            }
            ExprKind::Array(elems) => TypedExpr::Array {
                ty,
                elems: elems
                    .iter()
                    .map(|el| match el {
                        ArrayElem::Elem(x) => TypedArrayElem::Elem(self.expr(cx, x)),
                        ArrayElem::Spread(x) => TypedArrayElem::Spread(self.expr(cx, x)),
                    })
                    .collect(),
            },
            ExprKind::Index { array, index } => TypedExpr::Index {
                ty,
                recv: Box::new(self.expr(cx, array)),
                index: Box::new(self.expr(cx, index)),
            },
            ExprKind::Slice { array, start, end } => TypedExpr::Slice {
                ty,
                recv: Box::new(self.expr(cx, array)),
                start: Box::new(self.expr(cx, start)),
                end: Box::new(self.expr(cx, end)),
            },
            ExprKind::Range { start, end } => TypedExpr::Range {
                ty,
                start: Box::new(self.expr(cx, start)),
                end: Box::new(self.expr(cx, end)),
            },
            ExprKind::BinOp { op, left, right } => {
                let lhs = self.expr(cx, left);
                let rhs = self.expr(cx, right);
                let prim = self.b.pool.prim_of(lhs.ty());
                TypedExpr::Binary {
                    ty,
                    op: specialize(*op, prim),
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                }
            }
            ExprKind::And(l, r) => TypedExpr::And {
                ty,
                lhs: Box::new(self.expr(cx, l)),
                rhs: Box::new(self.expr(cx, r)),
            },
            ExprKind::Or(l, r) => TypedExpr::Or {
                ty,
                lhs: Box::new(self.expr(cx, l)),
                rhs: Box::new(self.expr(cx, r)),
            },
            ExprKind::Unary { op, operand } => {
                let operand = self.expr(cx, operand);
                let op = match op {
                    UnaryOp::Not => PrimOp::Not,
                    UnaryOp::Neg => match self.b.pool.prim_of(operand.ty()) {
                        Some(Prim::Int) => PrimOp::IntNeg,
                        Some(Prim::Float) => PrimOp::FloatNeg,
                        Some(Prim::String) | None => PrimOp::Neg,
                    },
                };
                TypedExpr::Unary {
                    ty,
                    op,
                    operand: Box::new(operand),
                }
            }
            ExprKind::If { cond, then, els } => TypedExpr::If {
                ty,
                cond: Box::new(self.expr(cx, cond)),
                then: Box::new(self.expr(cx, then)),
                els: Box::new(self.expr(cx, els)),
            },
            ExprKind::Match { subject, arms } => {
                let scrut = Box::new(self.expr(cx, subject));
                let arms = arms
                    .iter()
                    .map(|a| TypedArm {
                        pat: self.pattern(cx, &a.pattern),
                        guard: a.guard.as_ref().map(|g| self.expr(cx, g)),
                        body: self.expr(cx, &a.body),
                    })
                    .collect();
                TypedExpr::Match { ty, scrut, arms }
            }
            ExprKind::OrElse {
                shape,
                value,
                error,
                fallback,
            } => self.or_else(cx, *shape, value, *error, fallback, ty),
            ExprKind::Block(block) => self.block(cx, block, ty),
            // A clean module holds no error node.
            ExprKind::Error => TypedExpr::Nil { ty },
        }
    }

    fn var_value(&mut self, cx: &mut Fx, v: &Var, e: &Expr, ty: RTy) -> TypedExpr {
        match v {
            Var::Local(b) => self.load(cx, *b, ty),
            Var::Fn(name) => match self.b.fn_of.get(name) {
                Some(&func_idx) => TypedExpr::Closure {
                    ty,
                    func_idx,
                    captures: Vec::new(),
                },
                None => TypedExpr::Nil { ty },
            },
            Var::Const(name) => match self.b.const_slot.get(name) {
                Some(&slot) => TypedExpr::Var {
                    ty,
                    place: ValueRef::Global(slot),
                },
                None => TypedExpr::Nil { ty },
            },
            Var::Builtin(intrinsic) => {
                let intrinsic = *intrinsic;
                self.eta(ty, &format!("{intrinsic:?}"), |_, params, ret| {
                    TypedExpr::Call {
                        ty: ret,
                        callee: TypedCallee::Builtin { intrinsic },
                        args: params,
                    }
                })
            }
            Var::Ctor(ctor) => {
                let variant = self.b.ctor_ref(&ctor.type_name, ctor.variant);
                match self.final_ty(e.ty) {
                    Type::Fn { .. } => {
                        self.eta(ty, "ctor", move |_, params, ret| TypedExpr::Ctor {
                            ty: ret,
                            variant,
                            args: params,
                        })
                    }
                    Type::Named { .. } | Type::Tuple(_) | Type::Var(_) => TypedExpr::Ctor {
                        ty,
                        variant,
                        args: Vec::new(),
                    },
                }
            }
        }
    }

    /// A function that takes `ty`'s parameters and gives `body` of them: a
    /// constructor or builtin used as a value.
    fn eta(
        &mut self,
        ty: RTy,
        name: &str,
        body: impl FnOnce(&mut Self, Vec<TypedExpr>, RTy) -> TypedExpr,
    ) -> TypedExpr {
        let params: Vec<RTy> = self.b.pool.fun_params(ty).to_vec();
        let ret = self.b.pool.fun_ret(ty).unwrap_or(self.b.temps.int);
        let mut fx = Fx::new(None);
        let binds: Vec<TypedBind> = params
            .iter()
            .map(|&p| TypedBind {
                id: fx.fresh(),
                named: true,
                ty: p,
                global: None,
            })
            .collect();
        let args = binds.iter().map(|b| self.var(b)).collect();
        let body = body(self, args, ret);
        let idx = self.b.reserve_fn();
        let f = TypedFn {
            params: binds,
            ret,
            body,
            binds: fx.next,
        };
        self.b.fill_fn(idx, &self.m.key, name, f);
        TypedExpr::Closure {
            ty,
            func_idx: idx,
            captures: Vec::new(),
        }
    }

    fn lambda(&mut self, cx: &mut Fx, l: &t::Lambda, ty: RTy) -> TypedExpr {
        let idx: FuncIdx = self.b.reserve_fn();
        let mut fx = Fx::new(None);
        fx.captures = l.captures.clone();
        fx.self_name = l.self_name;
        let params = l.params.iter().map(|&p| self.bind(&mut fx, p)).collect();
        let body = self.expr(&mut fx, &l.body);
        let ret = self.b.pool.fun_ret(ty).unwrap_or(body.ty());
        let name = l
            .self_name
            .map_or_else(|| "fn".to_string(), |b| self.m.binder(b).name.to_string());
        let f = TypedFn {
            params,
            ret,
            body,
            binds: fx.next,
        };
        self.b.fill_fn(idx, &self.m.key, &name, f);
        let captures = l
            .captures
            .iter()
            .map(|&c| {
                let cty = self.ty(self.m.binder(c).ty);
                self.load(cx, c, cty)
            })
            .collect();
        TypedExpr::Closure {
            ty,
            func_idx: idx,
            captures,
        }
    }

    fn call(&mut self, cx: &mut Fx, callee: &Expr, args: &[Arg], ty: RTy) -> TypedExpr {
        let target = match &callee.kind {
            ExprKind::Var(Var::Fn(name)) if cx.self_fn.as_ref() == Some(name) => {
                Some(TypedCallee::SelfRec)
            }
            ExprKind::Var(Var::Fn(name)) => self.b.fn_of.get(name).map(|&i| TypedCallee::Known(i)),
            ExprKind::Var(Var::Builtin(intrinsic)) => Some(TypedCallee::Builtin {
                intrinsic: *intrinsic,
            }),
            ExprKind::Var(Var::Local(b))
                if cx.self_name == Some(*b) && !cx.binds.contains_key(b) =>
            {
                Some(TypedCallee::SelfRec)
            }
            _ => None,
        };
        let callee = match target {
            Some(c) => c,
            None => TypedCallee::Dynamic(Box::new(self.expr(cx, callee))),
        };
        let (lets, args) = self.args_in_slot_order(cx, args);
        wrap(lets, TypedExpr::Call { ty, callee, args })
    }

    /// The arguments in the order their parameters are declared. When that
    /// isn't the order they were written in, each runs first into a `let`,
    /// in the order written, so reordering them can't reorder what they do.
    fn args_in_slot_order(
        &mut self,
        cx: &mut Fx,
        args: &[Arg],
    ) -> (Vec<(TypedBind, TypedExpr)>, Vec<TypedExpr>) {
        let in_order = args.iter().enumerate().all(|(i, a)| a.slot as usize == i);
        if in_order {
            return (
                Vec::new(),
                args.iter().map(|a| self.expr(cx, &a.value)).collect(),
            );
        }
        let mut lets = Vec::with_capacity(args.len());
        let mut slotted: Vec<(u32, TypedExpr)> = Vec::with_capacity(args.len());
        for a in args {
            let v = self.expr(cx, &a.value);
            let b = self.temp(cx, v.ty());
            slotted.push((a.slot, self.var(&b)));
            lets.push((b, v));
        }
        slotted.sort_by_key(|(s, _)| *s);
        (lets, slotted.into_iter().map(|(_, v)| v).collect())
    }

    fn construct(
        &mut self,
        cx: &mut Fx,
        ctor: &CtorName,
        args: &[Arg],
        spread: Option<&t::Spread>,
        e: &Expr,
        ty: RTy,
    ) -> TypedExpr {
        let variant = self.b.ctor_ref(&ctor.type_name, ctor.variant);
        let Some(spread) = spread else {
            let (lets, fields) = self.args_in_slot_order(cx, args);
            return wrap(
                lets,
                TypedExpr::Ctor {
                    ty,
                    variant,
                    args: fields,
                },
            );
        };
        // Everything runs in the order it was written, the spread among the
        // arguments.
        let result = self.final_ty(e.ty);
        let mut lets = Vec::with_capacity(args.len() + 1);
        let mut fields: Vec<(u32, TypedExpr)> = Vec::new();
        let mut base_bind = None;
        for (i, a) in args.iter().enumerate() {
            if i as u32 == spread.at {
                base_bind = Some(self.spill(cx, &spread.base, &mut lets));
            }
            let b = self.spill(cx, &a.value, &mut lets);
            fields.push((a.slot, self.var(&b)));
        }
        let base_bind = match base_bind {
            Some(b) => b,
            None => self.spill(cx, &spread.base, &mut lets),
        };
        let field_tys = self.field_tys(&result, ctor.variant);
        for (slot, at) in &spread.fills {
            let fty = field_tys
                .get(*slot as usize)
                .copied()
                .unwrap_or(self.b.temps.int);
            let recv = self.var(&base_bind);
            let read = self.field(cx, recv, &result, at, fty);
            fields.push((*slot, read));
        }
        fields.sort_by_key(|(s, _)| *s);
        let ctor = TypedExpr::Ctor {
            ty,
            variant,
            args: fields.into_iter().map(|(_, v)| v).collect(),
        };
        wrap(lets, ctor)
    }

    /// Run `e` into a new `let`, returning its binding.
    fn spill(
        &mut self,
        cx: &mut Fx,
        e: &Expr,
        lets: &mut Vec<(TypedBind, TypedExpr)>,
    ) -> TypedBind {
        let v = self.expr(cx, e);
        let b = self.temp(cx, v.ty());
        lets.push((b, v));
        b
    }

    /// The field types of variant `v` of the named type `t`.
    fn field_tys(&mut self, t: &Type, v: u16) -> Vec<RTy> {
        let Some((name, args)) = t.as_named() else {
            return Vec::new();
        };
        let tys = self
            .b
            .defs
            .type_def(name)
            .and_then(|d| d.field_types(v as usize, args))
            .unwrap_or_default();
        tys.iter().map(|f| self.b.rty(f)).collect()
    }

    /// A field read. Where every variant holds it in the same slot it is one
    /// read; otherwise it is a `match` that reads each variant's own.
    fn field(
        &mut self,
        cx: &mut Fx,
        recv: TypedExpr,
        rec_ty: &Type,
        at: &FieldAt,
        ty: RTy,
    ) -> TypedExpr {
        let layouts = match at {
            FieldAt::Same(idx) => {
                return TypedExpr::Field {
                    ty,
                    recv: Box::new(recv),
                    idx: *idx,
                };
            }
            FieldAt::PerVariant(layouts) => layouts,
        };
        let Some((name, _)) = rec_ty.as_named() else {
            return TypedExpr::Nil { ty };
        };
        let name = name.clone();
        let scrut_ty = recv.ty();
        let mut arms = Vec::with_capacity(layouts.len());
        for &(v, slot) in layouts {
            let b = self.temp(cx, ty);
            let fields = self
                .field_tys(rec_ty, v)
                .into_iter()
                .enumerate()
                .map(|(j, fty)| {
                    if j == slot as usize {
                        TypedPat::Bind(b)
                    } else {
                        TypedPat::Wild { ty: fty }
                    }
                })
                .collect();
            arms.push(TypedArm {
                pat: TypedPat::Ctor {
                    ty: scrut_ty,
                    variant: self.b.ctor_ref(&name, v),
                    fields,
                },
                guard: None,
                body: self.var(&b),
            });
        }
        TypedExpr::Match {
            ty,
            scrut: Box::new(recv),
            arms,
        }
    }

    fn or_else(
        &mut self,
        cx: &mut Fx,
        shape: OrShape,
        value: &Expr,
        error: Option<BinderId>,
        fallback: &Expr,
        ty: RTy,
    ) -> TypedExpr {
        let lhs = self.expr(cx, value);
        let lhs_ty = lhs.ty();
        let ok_bind = self.temp(cx, ty);
        let (ok, fail, fail_fields) = match shape {
            OrShape::Option => (self.b.ctors.some, self.b.ctors.none, Vec::new()),
            OrShape::Result => {
                let ety = self.b.pool.con_arg(lhs_ty, 1).unwrap_or(self.b.temps.int);
                let err = match error {
                    Some(b) => {
                        let mut bind = self.bind(cx, b);
                        bind.ty = ety;
                        bind
                    }
                    None => self.temp(cx, ety),
                };
                (self.b.ctors.ok, self.b.ctors.err, vec![TypedPat::Bind(err)])
            }
        };
        let body = self.expr(cx, fallback);
        TypedExpr::Match {
            ty,
            scrut: Box::new(lhs),
            arms: vec![
                TypedArm {
                    pat: TypedPat::Ctor {
                        ty: lhs_ty,
                        variant: fail,
                        fields: fail_fields,
                    },
                    guard: None,
                    body,
                },
                TypedArm {
                    pat: TypedPat::Ctor {
                        ty: lhs_ty,
                        variant: ok,
                        fields: vec![TypedPat::Bind(ok_bind)],
                    },
                    guard: None,
                    body: self.var(&ok_bind),
                },
            ],
        }
    }

    fn block(&mut self, cx: &mut Fx, block: &Block, ty: RTy) -> TypedExpr {
        let mut frames = Vec::with_capacity(block.stmts.len());
        for s in &block.stmts {
            self.stmt(cx, s, &mut frames);
        }
        let tail = match &block.tail {
            Some(t) => self.expr(cx, t),
            None => TypedExpr::Nil { ty },
        };
        fold(frames, tail)
    }

    fn stmt(&mut self, cx: &mut Fx, s: &Stmt, frames: &mut Vec<Frame>) {
        match s {
            Stmt::Expr(x) => {
                let effect = self.expr(cx, x);
                frames.push(Frame::Seq(effect));
            }
            Stmt::Let { pattern, value } => {
                let init = self.expr(cx, value);
                match &pattern.kind {
                    PatternKind::Bind(b) => {
                        let mut bind = self.bind(cx, *b);
                        bind.ty = init.ty();
                        cx.binds.insert(*b, (bind.id, bind.ty));
                        frames.push(Frame::Let(bind, init));
                    }
                    PatternKind::Wild => frames.push(Frame::Seq(init)),
                    _ => {
                        let src = self.temp(cx, init.ty());
                        frames.push(Frame::Let(src, init));
                        self.project(cx, pattern, &src, frames);
                    }
                }
            }
        }
    }

    /// Read a binding pattern's names out of `src`, one `let` per field. The
    /// checker has proven the pattern matches every value, so no read needs
    /// a check.
    fn project(&mut self, cx: &mut Fx, p: &Pattern, src: &TypedBind, frames: &mut Vec<Frame>) {
        match &p.kind {
            PatternKind::Wild
            | PatternKind::Int(_)
            | PatternKind::Float(_)
            | PatternKind::String(_)
            | PatternKind::Range(..)
            | PatternKind::Error => {}
            PatternKind::Bind(b) => {
                let mut bind = self.bind(cx, *b);
                bind.ty = src.ty;
                cx.binds.insert(*b, (bind.id, bind.ty));
                frames.push(Frame::Let(bind, self.var(src)));
            }
            PatternKind::Tuple(elems) => self.project_fields(cx, elems, src, true, frames),
            PatternKind::Ctor { fields, .. } => self.project_fields(cx, fields, src, false, frames),
            // Only `[..rest]` and `<<..rest>>` match every value, and the rest
            // is the whole value.
            PatternKind::Array { rest, .. } | PatternKind::Binary { rest, .. } => {
                if let Rest::Bind(b) = rest {
                    let mut bind = self.bind(cx, *b);
                    bind.ty = src.ty;
                    cx.binds.insert(*b, (bind.id, bind.ty));
                    frames.push(Frame::Let(bind, self.var(src)));
                }
            }
            PatternKind::Or(alts) => {
                if let Some(first) = alts.first() {
                    self.project(cx, first, src, frames);
                }
            }
        }
    }

    /// Read each field `subs` names out of `src`. A name binds the read
    /// itself, so it is an alias of the field, not a copy of it.
    fn project_fields(
        &mut self,
        cx: &mut Fx,
        subs: &[Pattern],
        src: &TypedBind,
        tuple: bool,
        frames: &mut Vec<Frame>,
    ) {
        for (i, sub) in subs.iter().enumerate() {
            if matches!(sub.kind, PatternKind::Wild) {
                continue;
            }
            let ty = self.ty(sub.ty);
            let recv = Box::new(self.var(src));
            let idx = i as u32;
            let read = if tuple {
                TypedExpr::TupleIndex { ty, recv, idx }
            } else {
                TypedExpr::Field { ty, recv, idx }
            };
            if let PatternKind::Bind(b) = sub.kind {
                let mut bind = self.bind(cx, b);
                bind.ty = ty;
                cx.binds.insert(b, (bind.id, bind.ty));
                frames.push(Frame::Let(bind, read));
                continue;
            }
            let b = self.temp(cx, ty);
            frames.push(Frame::Let(b, read));
            self.project(cx, sub, &b, frames);
        }
    }

    fn pattern(&mut self, cx: &mut Fx, p: &Pattern) -> TypedPat {
        let ty = self.ty(p.ty);
        match &p.kind {
            PatternKind::Wild | PatternKind::Error => TypedPat::Wild { ty },
            PatternKind::Bind(b) => {
                let mut bind = self.bind(cx, *b);
                bind.global = None;
                TypedPat::Bind(bind)
            }
            PatternKind::Int(i) => TypedPat::Lit {
                ty,
                value: self.b.constant(Const::Int(*i)),
            },
            PatternKind::Float(f) => TypedPat::Lit {
                ty,
                value: self.b.constant(Const::Float(*f)),
            },
            PatternKind::String(s) => TypedPat::Lit {
                ty,
                value: self.b.constant(Const::String(s.clone())),
            },
            PatternKind::Range(lo, hi) => TypedPat::Range {
                ty,
                lo: self.b.constant(Const::Int(*lo)),
                hi: self.b.constant(Const::Int(*hi)),
            },
            PatternKind::Ctor { ctor, fields } => TypedPat::Ctor {
                ty,
                variant: self.b.ctor_ref(&ctor.type_name, ctor.variant),
                fields: fields.iter().map(|f| self.pattern(cx, f)).collect(),
            },
            PatternKind::Tuple(elems) => TypedPat::Tuple {
                ty,
                elems: elems.iter().map(|e| self.pattern(cx, e)).collect(),
            },
            PatternKind::Array { prefix, rest } => {
                let elem_ty = self.b.pool.con_arg(ty, 0).unwrap_or(self.b.temps.int);
                let prefix: Vec<TypedPat> = prefix.iter().map(|e| self.pattern(cx, e)).collect();
                let rest = self.rest(cx, rest, ty);
                let len = self.b.constant(Const::Int(prefix.len() as i64));
                TypedPat::Array {
                    ty,
                    elem_ty,
                    len,
                    prefix,
                    rest,
                }
            }
            PatternKind::Binary { segs, rest } => {
                let segs = segs.iter().map(|s| self.bin_seg_pat(cx, s)).collect();
                let rest = self.rest(cx, rest, ty);
                let zero = self.b.constant(Const::Int(0));
                TypedPat::Bin {
                    ty,
                    zero,
                    segs,
                    rest,
                }
            }
            PatternKind::Or(alts) => TypedPat::Or {
                ty,
                alts: alts.iter().map(|a| self.pattern(cx, a)).collect(),
            },
        }
    }

    fn rest(&mut self, cx: &mut Fx, rest: &Rest, ty: RTy) -> PatRest {
        match rest {
            Rest::None => PatRest::None,
            Rest::Discard => PatRest::Discard,
            Rest::Bind(b) => {
                let mut bind = self.bind(cx, *b);
                bind.ty = ty;
                bind.global = None;
                PatRest::Bind(bind)
            }
        }
    }

    fn bin_seg_pat(&mut self, cx: &mut Fx, s: &BinSegPat) -> TypedBinPatSeg {
        match s {
            BinSegPat::Text(text) => {
                let width = (text.len() as i64) * 8;
                let bytes = self.b.constant(Const::Binary {
                    bytes: text.as_bytes().to_vec(),
                    bit_len: width as u64,
                });
                let bits = self.b.constant(Const::Int(width));
                TypedBinPatSeg::Utf8Literal { bytes, bits }
            }
            BinSegPat::Utf8(p) => TypedBinPatSeg::Utf8 {
                value: self.pattern(cx, p),
            },
            BinSegPat::Bits { bits, value } => {
                let bits = match bits {
                    Some(e) => self.expr(cx, e),
                    None => self.int_const(8),
                };
                TypedBinPatSeg::Int {
                    bits,
                    value: self.pattern(cx, value),
                }
            }
            BinSegPat::Bytes { bytes, value } => {
                let bits = bytes.as_ref().map(|e| self.bytes_to_bits(cx, e));
                TypedBinPatSeg::Binary {
                    bits,
                    value: self.pattern(cx, value),
                }
            }
        }
    }

    fn bytes_to_bits(&mut self, cx: &mut Fx, e: &Expr) -> TypedExpr {
        let v = self.expr(cx, e);
        let eight = self.int_const(8);
        TypedExpr::Binary {
            ty: self.b.temps.int,
            op: PrimOp::IntMul,
            lhs: Box::new(v),
            rhs: Box::new(eight),
        }
    }

    fn binary_literal(&mut self, cx: &mut Fx, segs: &[BinSeg], ty: RTy) -> TypedExpr {
        if segs.is_empty() {
            let value = self.b.constant(Const::Binary {
                bytes: Vec::new(),
                bit_len: 0,
            });
            return TypedExpr::Const { ty, value };
        }
        let segs = segs
            .iter()
            .map(|s| {
                let value = self.expr(cx, &s.value);
                match &s.unit {
                    BinUnit::Bits(bits) => {
                        let bits = match bits {
                            Some(e) => self.expr(cx, e),
                            None => self.int_const(8),
                        };
                        TypedBinSeg::Int { value, bits }
                    }
                    BinUnit::Bytes(bytes) => {
                        let bits = bytes.as_ref().map(|e| self.bytes_to_bits(cx, e));
                        TypedBinSeg::Binary { value, bits }
                    }
                    BinUnit::Utf8 => TypedBinSeg::Utf8 { value },
                }
            })
            .collect();
        TypedExpr::BinLit { ty, segs }
    }
}

fn wrap(lets: Vec<(TypedBind, TypedExpr)>, tail: TypedExpr) -> TypedExpr {
    fold(
        lets.into_iter().map(|(b, e)| Frame::Let(b, e)).collect(),
        tail,
    )
}

/// The operator `op` is on operands of primitive `prim`. With no primitive,
/// as in `fn add(a, b) { a + b }`, the VM picks it from the values.
fn specialize(op: BinOp, prim: Option<Prim>) -> PrimOp {
    use BinOp as B;
    match (op, prim) {
        (B::Add, Some(Prim::Int)) => PrimOp::IntAdd,
        (B::Add, Some(Prim::Float)) => PrimOp::FloatAdd,
        (B::Add, Some(Prim::String)) => PrimOp::StringConcat,
        (B::Add, None) => PrimOp::Add,
        (B::Sub, Some(Prim::Int)) => PrimOp::IntSub,
        (B::Sub, Some(Prim::Float)) => PrimOp::FloatSub,
        (B::Sub, _) => PrimOp::Sub,
        (B::Mul, Some(Prim::Int)) => PrimOp::IntMul,
        (B::Mul, Some(Prim::Float)) => PrimOp::FloatMul,
        (B::Mul, _) => PrimOp::Mul,
        (B::Div, Some(Prim::Int)) => PrimOp::IntDiv,
        (B::Div, Some(Prim::Float)) => PrimOp::FloatDiv,
        (B::Div, _) => PrimOp::Div,
        (B::Mod, Some(Prim::Int)) => PrimOp::IntRem,
        (B::Mod, _) => PrimOp::Rem,
        (B::Eq, Some(Prim::Int)) => PrimOp::IntEq,
        (B::Eq, _) => PrimOp::Eq,
        (B::Ne, Some(Prim::Int)) => PrimOp::IntNe,
        (B::Ne, _) => PrimOp::Ne,
        (B::Lt, Some(Prim::Int)) => PrimOp::IntLt,
        (B::Lt, Some(Prim::Float)) => PrimOp::FloatLt,
        (B::Lt, _) => PrimOp::Lt,
        (B::Le, Some(Prim::Int)) => PrimOp::IntLe,
        (B::Le, Some(Prim::Float)) => PrimOp::FloatLe,
        (B::Le, _) => PrimOp::Le,
        (B::Gt, Some(Prim::Int)) => PrimOp::IntGt,
        (B::Gt, Some(Prim::Float)) => PrimOp::FloatGt,
        (B::Gt, _) => PrimOp::Gt,
        (B::Ge, Some(Prim::Int)) => PrimOp::IntGe,
        (B::Ge, Some(Prim::Float)) => PrimOp::FloatGe,
        (B::Ge, _) => PrimOp::Ge,
    }
}
