//! One module's typed tree, turned into typed IR. The checker has made every
//! decision already, so this only chooses how each one is built: which
//! operator a `+` is, the `match` an `or` or a per-variant field read becomes,
//! the closure a function used as a value is, and the order arguments run in.
//!
//! Every function and global a body names is in [`Symbols`] before any body is
//! made, and every binder a function binds is numbered before its body is. A
//! lookup that fails is a [`LinkError`], never a stand-in value.

use std::collections::HashMap;

use scarlet_check::typed::{
    self as t, Arg, ArrayElem, BinOp, BinSeg, BinSegPat, BinUnit, BinderId, Block, Expr, ExprKind,
    FieldAt, FnBody, InterpPart, OrShape, Pattern, PatternKind, Rest, Stmt, TypedModule, Var,
};
use scarlet_ir::core_ir::{CaptureIdx, Const, ConstId, PrimOp};
use scarlet_ir::rty::{Prim, RTy};
use scarlet_syntax::ast::UnaryOp;
use scarlet_types::name::{CtorName, ValueName};
use scarlet_types::ty::Type;

use crate::program::{Ids, InternalLinkError, LinkError, Minted, Named, Prelude, Symbols};
use crate::typed_ir::{
    BindingId, PatRest, TypedArm, TypedArrayElem, TypedBinPatSeg, TypedBinSeg, TypedBind,
    TypedCallee, TypedExpr, TypedFn, TypedInterpPart, TypedPat, ValueRef,
};

type R<T> = Result<T, LinkError>;

/// What translating any module reads and adds to.
pub(crate) struct Cx<'x, 'd> {
    pub ids: &'x mut Ids<'d>,
    pub prelude: &'x Prelude,
    pub symbols: &'x Symbols,
    pub minted: &'x mut Minted,
}

/// The `index`th function of `m`, which has a body.
pub(crate) fn function(cx: &mut Cx<'_, '_>, m: &TypedModule, index: usize) -> R<Named> {
    let f = m
        .functions
        .get(index)
        .ok_or_else(|| InternalLinkError::NoSuchFunction {
            module: m.key.clone(),
            index,
        })?;
    let FnBody::Expr(body) = &f.body else {
        return Err(InternalLinkError::NoBody(ValueName {
            module: m.key.clone(),
            name: f.name.clone(),
        })
        .into());
    };
    let mut tx = Tx { cx, m };
    let ty = tx.ty(f.ty);
    let ret = tx.ret_of(ty)?;
    let mut fx = Fx::new(&f.params, |out| body.for_each_bound(&mut |b| out(b)));
    fx.self_fn = Some(ValueName {
        module: m.key.clone(),
        name: f.name.clone(),
    });
    let params = f
        .params
        .iter()
        .map(|&p| tx.bind_own(&fx, p))
        .collect::<R<_>>()?;
    let body = tx.expr(&mut fx, body)?;
    Ok(Named {
        module: m.key.as_str().to_string(),
        name: f.name.to_string(),
        body: TypedFn {
            params,
            ret,
            body,
            binds: fx.next,
        },
    })
}

/// The top level of `m`: its consts and statements, in the order they run.
pub(crate) fn toplevel(cx: &mut Cx<'_, '_>, m: &TypedModule) -> R<TypedFn> {
    let mut tx = Tx { cx, m };
    let mut fx = Fx::new(&[], |out| {
        for c in &m.consts {
            c.value.for_each_bound(&mut |b| out(b));
        }
        for s in &m.statements {
            s.for_each_bound(&mut |b| out(b));
        }
    });
    let mut frames: Vec<Frame> = Vec::new();
    for c in &m.consts {
        let init = tx.expr(&mut fx, &c.value)?;
        let slot = tx.cx.symbols.const_slot(&ValueName {
            module: m.key.clone(),
            name: c.name.clone(),
        })?;
        let bind = TypedBind {
            id: fx.fresh(),
            named: true,
            ty: init.ty(),
            global: Some(slot),
        };
        frames.push(Frame::Let(bind, init));
    }
    // A script's last expression is its value, which a REPL shows.
    let (last, init) = match m.statements.split_last() {
        Some((Stmt::Expr(e), init)) => (Some(e), init),
        Some(_) | None => (None, &m.statements[..]),
    };
    for s in init {
        tx.stmt(&mut fx, s, &mut frames)?;
    }
    let tail = match last {
        Some(e) => tx.expr(&mut fx, e)?,
        None => TypedExpr::Nil {
            ty: tx.cx.prelude.nil,
        },
    };
    let ret = tail.ty();
    Ok(TypedFn {
        params: Vec::new(),
        ret,
        body: fold(frames, tail),
        binds: fx.next,
    })
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

fn wrap(lets: Vec<(TypedBind, TypedExpr)>, tail: TypedExpr) -> TypedExpr {
    fold(
        lets.into_iter().map(|(b, e)| Frame::Let(b, e)).collect(),
        tail,
    )
}

/// The function being made.
struct Fx {
    /// Each binder this function binds, numbered before its body is made: its
    /// parameters, then the rest in the order they are written.
    own: HashMap<BinderId, BindingId>,
    /// The next id for a temporary. Temporaries come after every binder.
    next: u32,
    /// The enclosing function's locals this one reads, by capture index.
    captures: Vec<BinderId>,
    /// The binder that means this closure itself.
    self_name: Option<BinderId>,
    /// The module function this is, which calls itself directly.
    self_fn: Option<ValueName>,
}

impl Fx {
    /// A function with parameters `params`, whose body binds what `body`
    /// lists.
    fn new(params: &[BinderId], body: impl FnOnce(&mut dyn FnMut(BinderId))) -> Self {
        let mut own: HashMap<BinderId, BindingId> = HashMap::new();
        let mut number = |b: BinderId| {
            let next = BindingId(own.len() as u32);
            own.entry(b).or_insert(next);
        };
        params.iter().for_each(|&p| number(p));
        body(&mut number);
        Fx {
            next: own.len() as u32,
            own,
            captures: Vec::new(),
            self_name: None,
            self_fn: None,
        }
    }

    fn fresh(&mut self) -> BindingId {
        let id = BindingId(self.next);
        self.next += 1;
        id
    }
}

struct Tx<'t, 'x, 'd, 'm> {
    cx: &'t mut Cx<'x, 'd>,
    m: &'m TypedModule,
}

impl Tx<'_, '_, '_, '_> {
    fn ty(&mut self, t: scarlet_types::engine::Ty) -> RTy {
        let ty = self.m.types.get(t);
        self.cx.ids.rty(&ty)
    }

    fn final_ty(&self, t: scarlet_types::engine::Ty) -> Type {
        self.m.types.get(t)
    }

    /// What a function of type `ty` returns.
    fn ret_of(&self, ty: RTy) -> R<RTy> {
        self.cx
            .ids
            .pool()
            .fun_ret(ty)
            .ok_or(InternalLinkError::NotAFunction(ty).into())
    }

    fn int(&self) -> RTy {
        self.cx.prelude.temps.int
    }

    fn constant(&mut self, c: Const) -> ConstId {
        self.cx.ids.constant(c)
    }

    fn int_const(&mut self, i: i64) -> TypedExpr {
        let value = self.constant(Const::Int(i));
        TypedExpr::Const {
            ty: self.int(),
            value,
        }
    }

    /// The binding for `binder`, a binder of the function being made, at
    /// type `ty`.
    fn bind(&self, fx: &Fx, binder: BinderId, ty: RTy) -> R<TypedBind> {
        let id = *fx
            .own
            .get(&binder)
            .ok_or_else(|| InternalLinkError::UnownedBinder(self.m.binder(binder).name.clone()))?;
        Ok(TypedBind {
            id,
            named: true,
            ty,
            global: self.cx.symbols.binder_slot(&self.m.key, binder),
        })
    }

    /// The binding for `binder`, at the type the checker gave it.
    fn bind_own(&mut self, fx: &Fx, binder: BinderId) -> R<TypedBind> {
        let ty = self.ty(self.m.binder(binder).ty);
        self.bind(fx, binder, ty)
    }

    fn temp(&self, fx: &mut Fx, ty: RTy) -> TypedBind {
        TypedBind {
            id: fx.fresh(),
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
    fn load(&self, fx: &Fx, binder: BinderId, ty: RTy) -> TypedExpr {
        let place = if let Some(&id) = fx.own.get(&binder) {
            ValueRef::Local(id)
        } else if let Some(slot) = self.cx.symbols.binder_slot(&self.m.key, binder) {
            ValueRef::Global(slot)
        } else if let Some(i) = fx.captures.iter().position(|&c| c == binder) {
            ValueRef::Capture(CaptureIdx(i as i32))
        } else {
            ValueRef::SelfClosure
        };
        TypedExpr::Var { ty, place }
    }

    fn exprs(&mut self, fx: &mut Fx, xs: &[Expr]) -> R<Vec<TypedExpr>> {
        xs.iter().map(|x| self.expr(fx, x)).collect()
    }

    fn boxed(&mut self, fx: &mut Fx, x: &Expr) -> R<Box<TypedExpr>> {
        Ok(Box::new(self.expr(fx, x)?))
    }

    fn expr(&mut self, fx: &mut Fx, e: &Expr) -> R<TypedExpr> {
        let ty = self.ty(e.ty);
        Ok(match &e.kind {
            ExprKind::Int(i) => TypedExpr::Const {
                ty,
                value: self.constant(Const::Int(*i)),
            },
            ExprKind::Float(f) => TypedExpr::Const {
                ty,
                value: self.constant(Const::Float(*f)),
            },
            ExprKind::String(s) => TypedExpr::Const {
                ty,
                value: self.constant(Const::String(s.clone())),
            },
            ExprKind::Interp(parts) => {
                if parts.is_empty() {
                    let value = self.constant(Const::String(String::new()));
                    return Ok(TypedExpr::Const { ty, value });
                }
                let parts = parts
                    .iter()
                    .map(|p| {
                        Ok(match p {
                            InterpPart::Text(s) => {
                                TypedInterpPart::Str(self.constant(Const::String(s.clone())))
                            }
                            InterpPart::Expr(x) => TypedInterpPart::Expr(self.expr(fx, x)?),
                        })
                    })
                    .collect::<R<_>>()?;
                TypedExpr::Interp { ty, parts }
            }
            ExprKind::Binary(segs) => self.binary_literal(fx, segs, ty)?,
            ExprKind::Var(v) => self.var_value(fx, v, e, ty)?,
            ExprKind::Lambda(l) => self.lambda(fx, l, ty)?,
            ExprKind::Call { callee, args } => self.call(fx, callee, args, ty)?,
            ExprKind::Construct { ctor, args, spread } => {
                self.construct(fx, ctor, args, spread.as_deref(), e, ty)?
            }
            ExprKind::Tuple(xs) => TypedExpr::Tuple {
                ty,
                elems: self.exprs(fx, xs)?,
            },
            ExprKind::TupleIndex { tuple, index } => TypedExpr::TupleIndex {
                ty,
                recv: self.boxed(fx, tuple)?,
                idx: *index,
            },
            ExprKind::Field { record, at, .. } => {
                let rec_ty = self.final_ty(record.ty);
                let recv = self.expr(fx, record)?;
                self.field(fx, recv, &rec_ty, at, ty)?
            }
            ExprKind::Array(elems) => TypedExpr::Array {
                ty,
                elems: elems
                    .iter()
                    .map(|el| {
                        Ok(match el {
                            ArrayElem::Elem(x) => TypedArrayElem::Elem(self.expr(fx, x)?),
                            ArrayElem::Spread(x) => TypedArrayElem::Spread(self.expr(fx, x)?),
                        })
                    })
                    .collect::<R<_>>()?,
            },
            ExprKind::Index { array, index } => TypedExpr::Index {
                ty,
                recv: self.boxed(fx, array)?,
                index: self.boxed(fx, index)?,
            },
            ExprKind::Slice { array, start, end } => TypedExpr::Slice {
                ty,
                recv: self.boxed(fx, array)?,
                start: self.boxed(fx, start)?,
                end: self.boxed(fx, end)?,
            },
            ExprKind::Range { start, end } => TypedExpr::Range {
                ty,
                start: self.boxed(fx, start)?,
                end: self.boxed(fx, end)?,
            },
            ExprKind::BinOp { op, left, right } => {
                let lhs = self.expr(fx, left)?;
                let rhs = self.expr(fx, right)?;
                let prim = self.cx.ids.pool().prim_of(lhs.ty());
                TypedExpr::Binary {
                    ty,
                    op: specialize(*op, prim),
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                }
            }
            ExprKind::And(l, r) => TypedExpr::And {
                ty,
                lhs: self.boxed(fx, l)?,
                rhs: self.boxed(fx, r)?,
            },
            ExprKind::Or(l, r) => TypedExpr::Or {
                ty,
                lhs: self.boxed(fx, l)?,
                rhs: self.boxed(fx, r)?,
            },
            ExprKind::Unary { op, operand } => {
                let operand = self.expr(fx, operand)?;
                let op = match op {
                    UnaryOp::Not => PrimOp::Not,
                    UnaryOp::Neg => match self.cx.ids.pool().prim_of(operand.ty()) {
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
                cond: self.boxed(fx, cond)?,
                then: self.boxed(fx, then)?,
                els: self.boxed(fx, els)?,
            },
            ExprKind::Match { subject, arms } => {
                let scrut = self.boxed(fx, subject)?;
                let arms = arms
                    .iter()
                    .map(|a| {
                        Ok(TypedArm {
                            pat: self.pattern(fx, &a.pattern)?,
                            guard: a.guard.as_ref().map(|g| self.expr(fx, g)).transpose()?,
                            body: self.expr(fx, &a.body)?,
                        })
                    })
                    .collect::<R<_>>()?;
                TypedExpr::Match { ty, scrut, arms }
            }
            ExprKind::OrElse {
                shape,
                value,
                error,
                fallback,
            } => self.or_else(fx, *shape, value, *error, fallback, ty)?,
            ExprKind::Block(block) => self.block(fx, block, ty)?,
            ExprKind::Error => return Err(InternalLinkError::ErrorNode.into()),
        })
    }

    fn var_value(&mut self, fx: &mut Fx, v: &Var, e: &Expr, ty: RTy) -> R<TypedExpr> {
        Ok(match v {
            Var::Local(b) => self.load(fx, *b, ty),
            Var::Fn(name) => TypedExpr::Closure {
                ty,
                func_idx: self.cx.symbols.func(name)?,
                captures: Vec::new(),
            },
            Var::Const(name) => TypedExpr::Var {
                ty,
                place: ValueRef::Global(self.cx.symbols.const_slot(name)?),
            },
            Var::Builtin(intrinsic) => {
                let intrinsic = *intrinsic;
                self.eta(ty, &format!("{intrinsic:?}"), |params, ret| {
                    TypedExpr::Call {
                        ty: ret,
                        callee: TypedCallee::Builtin { intrinsic },
                        args: params,
                    }
                })?
            }
            Var::Ctor(ctor) => {
                let variant = self.cx.ids.ctor_ref(&ctor.type_name, ctor.variant);
                match self.final_ty(e.ty) {
                    Type::Fn { .. } => {
                        self.eta(ty, "ctor", move |params, ret| TypedExpr::Ctor {
                            ty: ret,
                            variant,
                            args: params,
                        })?
                    }
                    Type::Named { .. } | Type::Tuple(_) | Type::Var(_) => TypedExpr::Ctor {
                        ty,
                        variant,
                        args: Vec::new(),
                    },
                }
            }
        })
    }

    /// A function that takes `ty`'s parameters and gives `body` of them: a
    /// constructor or builtin used as a value.
    fn eta(
        &mut self,
        ty: RTy,
        name: &str,
        body: impl FnOnce(Vec<TypedExpr>, RTy) -> TypedExpr,
    ) -> R<TypedExpr> {
        let params: Vec<RTy> = self.cx.ids.pool().fun_params(ty).to_vec();
        let ret = self.ret_of(ty)?;
        let mut fx = Fx::new(&[], |_| {});
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
        let f = TypedFn {
            params: binds,
            ret,
            body: body(args, ret),
            binds: fx.next,
        };
        let func_idx = self.cx.minted.push(Named {
            module: self.m.key.as_str().to_string(),
            name: name.to_string(),
            body: f,
        });
        Ok(TypedExpr::Closure {
            ty,
            func_idx,
            captures: Vec::new(),
        })
    }

    fn lambda(&mut self, fx: &mut Fx, l: &t::Lambda, ty: RTy) -> R<TypedExpr> {
        let mut inner = Fx::new(&l.params, |out| l.body.for_each_bound(&mut |b| out(b)));
        inner.captures = l.captures.clone();
        inner.self_name = l.self_name;
        let params = l
            .params
            .iter()
            .map(|&p| self.bind_own(&inner, p))
            .collect::<R<_>>()?;
        let body = self.expr(&mut inner, &l.body)?;
        let ret = self.ret_of(ty)?;
        let name = l
            .self_name
            .map_or_else(|| "fn".to_string(), |b| self.m.binder(b).name.to_string());
        let func_idx = self.cx.minted.push(Named {
            module: self.m.key.as_str().to_string(),
            name,
            body: TypedFn {
                params,
                ret,
                body,
                binds: inner.next,
            },
        });
        let captures = l
            .captures
            .iter()
            .map(|&c| {
                let cty = self.ty(self.m.binder(c).ty);
                self.load(fx, c, cty)
            })
            .collect();
        Ok(TypedExpr::Closure {
            ty,
            func_idx,
            captures,
        })
    }

    fn call(&mut self, fx: &mut Fx, callee: &Expr, args: &[Arg], ty: RTy) -> R<TypedExpr> {
        let target = match &callee.kind {
            ExprKind::Var(Var::Fn(name)) if fx.self_fn.as_ref() == Some(name) => {
                Some(TypedCallee::SelfRec)
            }
            ExprKind::Var(Var::Fn(name)) => Some(TypedCallee::Known(self.cx.symbols.func(name)?)),
            ExprKind::Var(Var::Builtin(intrinsic)) => Some(TypedCallee::Builtin {
                intrinsic: *intrinsic,
            }),
            ExprKind::Var(Var::Local(b)) if fx.self_name == Some(*b) && !fx.own.contains_key(b) => {
                Some(TypedCallee::SelfRec)
            }
            _ => None,
        };
        let callee = match target {
            Some(c) => c,
            None => TypedCallee::Dynamic(self.boxed(fx, callee)?),
        };
        let (lets, args) = self.args_in_slot_order(fx, args)?;
        Ok(wrap(lets, TypedExpr::Call { ty, callee, args }))
    }

    /// The arguments in the order their parameters are declared. When that
    /// isn't the order they were written in, each runs first into a `let`,
    /// in the order written, so reordering them can't reorder what they do.
    #[expect(clippy::type_complexity, reason = "the lets, and the arguments")]
    fn args_in_slot_order(
        &mut self,
        fx: &mut Fx,
        args: &[Arg],
    ) -> R<(Vec<(TypedBind, TypedExpr)>, Vec<TypedExpr>)> {
        let in_order = args.iter().enumerate().all(|(i, a)| a.slot as usize == i);
        if in_order {
            let values = args
                .iter()
                .map(|a| self.expr(fx, &a.value))
                .collect::<R<_>>()?;
            return Ok((Vec::new(), values));
        }
        let mut lets = Vec::with_capacity(args.len());
        let mut slotted: Vec<(u32, TypedExpr)> = Vec::with_capacity(args.len());
        for a in args {
            let b = self.spill(fx, &a.value, &mut lets)?;
            slotted.push((a.slot, self.var(&b)));
        }
        slotted.sort_by_key(|(s, _)| *s);
        Ok((lets, slotted.into_iter().map(|(_, v)| v).collect()))
    }

    fn construct(
        &mut self,
        fx: &mut Fx,
        ctor: &CtorName,
        args: &[Arg],
        spread: Option<&t::Spread>,
        e: &Expr,
        ty: RTy,
    ) -> R<TypedExpr> {
        let variant = self.cx.ids.ctor_ref(&ctor.type_name, ctor.variant);
        let Some(spread) = spread else {
            let (lets, fields) = self.args_in_slot_order(fx, args)?;
            return Ok(wrap(
                lets,
                TypedExpr::Ctor {
                    ty,
                    variant,
                    args: fields,
                },
            ));
        };
        // Everything runs in the order it was written, the spread among the
        // arguments.
        let result = self.final_ty(e.ty);
        let mut lets = Vec::with_capacity(args.len() + 1);
        let mut fields: Vec<(u32, TypedExpr)> = Vec::new();
        let mut base_bind = None;
        for (i, a) in args.iter().enumerate() {
            if i as u32 == spread.at {
                base_bind = Some(self.spill(fx, &spread.base, &mut lets)?);
            }
            let b = self.spill(fx, &a.value, &mut lets)?;
            fields.push((a.slot, self.var(&b)));
        }
        let base_bind = match base_bind {
            Some(b) => b,
            None => self.spill(fx, &spread.base, &mut lets)?,
        };
        let field_tys = self.field_tys(&result, ctor.variant)?;
        for (slot, at) in &spread.fills {
            let fty =
                *field_tys
                    .get(*slot as usize)
                    .ok_or_else(|| InternalLinkError::NoFieldSlot {
                        ctor: ctor.clone(),
                        slot: *slot,
                    })?;
            let recv = self.var(&base_bind);
            let read = self.field(fx, recv, &result, at, fty)?;
            fields.push((*slot, read));
        }
        fields.sort_by_key(|(s, _)| *s);
        let ctor = TypedExpr::Ctor {
            ty,
            variant,
            args: fields.into_iter().map(|(_, v)| v).collect(),
        };
        Ok(wrap(lets, ctor))
    }

    /// Run `e` into a new `let`, returning its binding.
    fn spill(
        &mut self,
        fx: &mut Fx,
        e: &Expr,
        lets: &mut Vec<(TypedBind, TypedExpr)>,
    ) -> R<TypedBind> {
        let v = self.expr(fx, e)?;
        let b = self.temp(fx, v.ty());
        lets.push((b, v));
        Ok(b)
    }

    /// The field types of variant `v` of the named type `t`.
    fn field_tys(&mut self, t: &Type, v: u16) -> R<Vec<RTy>> {
        let (name, args) = t
            .as_named()
            .ok_or_else(|| InternalLinkError::FieldOfUnnamed(t.clone()))?;
        let tys = self
            .cx
            .ids
            .defs
            .type_def(name)
            .and_then(|d| d.field_types(v as usize, args))
            .ok_or_else(|| InternalLinkError::NoVariant {
                ty: name.clone(),
                variant: v,
            })?;
        Ok(tys.iter().map(|f| self.cx.ids.rty(f)).collect())
    }

    /// A field read. Where every variant holds it in the same slot it is one
    /// read; otherwise it is a `match` that reads each variant's own.
    fn field(
        &mut self,
        fx: &mut Fx,
        recv: TypedExpr,
        rec_ty: &Type,
        at: &FieldAt,
        ty: RTy,
    ) -> R<TypedExpr> {
        let layouts = match at {
            FieldAt::Same(idx) => {
                return Ok(TypedExpr::Field {
                    ty,
                    recv: Box::new(recv),
                    idx: *idx,
                });
            }
            FieldAt::PerVariant(layouts) => layouts,
        };
        let (name, _) = rec_ty
            .as_named()
            .ok_or_else(|| InternalLinkError::FieldOfUnnamed(rec_ty.clone()))?;
        let name = name.clone();
        let scrut_ty = recv.ty();
        let mut arms = Vec::with_capacity(layouts.len());
        for &(v, slot) in layouts {
            let b = self.temp(fx, ty);
            let fields = self
                .field_tys(rec_ty, v)?
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
                    variant: self.cx.ids.ctor_ref(&name, v),
                    fields,
                },
                guard: None,
                body: self.var(&b),
            });
        }
        Ok(TypedExpr::Match {
            ty,
            scrut: Box::new(recv),
            arms,
        })
    }

    fn or_else(
        &mut self,
        fx: &mut Fx,
        shape: OrShape,
        value: &Expr,
        error: Option<BinderId>,
        fallback: &Expr,
        ty: RTy,
    ) -> R<TypedExpr> {
        let lhs = self.expr(fx, value)?;
        let lhs_ty = lhs.ty();
        let ok_bind = self.temp(fx, ty);
        let ctors = self.cx.prelude.ctors;
        let (ok, fail, fail_fields) = match shape {
            OrShape::Option => (ctors.some, ctors.none, Vec::new()),
            OrShape::Result => {
                let ety = self
                    .cx
                    .ids
                    .pool()
                    .con_arg(lhs_ty, 1)
                    .ok_or(InternalLinkError::ResultWithoutError(lhs_ty))?;
                let err = match error {
                    Some(b) => self.bind(fx, b, ety)?,
                    None => self.temp(fx, ety),
                };
                (ctors.ok, ctors.err, vec![TypedPat::Bind(err)])
            }
        };
        let body = self.expr(fx, fallback)?;
        Ok(TypedExpr::Match {
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
        })
    }

    fn block(&mut self, fx: &mut Fx, block: &Block, ty: RTy) -> R<TypedExpr> {
        let mut frames = Vec::with_capacity(block.stmts.len());
        for s in &block.stmts {
            self.stmt(fx, s, &mut frames)?;
        }
        let tail = match &block.tail {
            Some(t) => self.expr(fx, t)?,
            None => TypedExpr::Nil { ty },
        };
        Ok(fold(frames, tail))
    }

    fn stmt(&mut self, fx: &mut Fx, s: &Stmt, frames: &mut Vec<Frame>) -> R<()> {
        match s {
            Stmt::Expr(x) => {
                let effect = self.expr(fx, x)?;
                frames.push(Frame::Seq(effect));
            }
            Stmt::Let { pattern, value } => {
                let init = self.expr(fx, value)?;
                match &pattern.kind {
                    PatternKind::Bind(b) => {
                        let bind = self.bind(fx, *b, init.ty())?;
                        frames.push(Frame::Let(bind, init));
                    }
                    PatternKind::Wild => frames.push(Frame::Seq(init)),
                    _ => {
                        let src = self.temp(fx, init.ty());
                        frames.push(Frame::Let(src, init));
                        self.project(fx, pattern, &src, frames)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Read a binding pattern's names out of `src`, one `let` per field. The
    /// checker has proven the pattern matches every value, so no read needs
    /// a check.
    fn project(
        &mut self,
        fx: &mut Fx,
        p: &Pattern,
        src: &TypedBind,
        frames: &mut Vec<Frame>,
    ) -> R<()> {
        match &p.kind {
            PatternKind::Wild
            | PatternKind::Int(_)
            | PatternKind::Float(_)
            | PatternKind::String(_)
            | PatternKind::Range(..)
            | PatternKind::Error => {}
            PatternKind::Bind(b) => {
                let bind = self.bind(fx, *b, src.ty)?;
                frames.push(Frame::Let(bind, self.var(src)));
            }
            PatternKind::Tuple(elems) => self.project_fields(fx, elems, src, true, frames)?,
            PatternKind::Ctor { fields, .. } => {
                self.project_fields(fx, fields, src, false, frames)?
            }
            // Only `[..rest]` and `<<..rest>>` match every value, and the rest
            // is the whole value.
            PatternKind::Array { rest, .. } | PatternKind::Binary { rest, .. } => {
                if let Rest::Bind(b) = rest {
                    let bind = self.bind(fx, *b, src.ty)?;
                    frames.push(Frame::Let(bind, self.var(src)));
                }
            }
            PatternKind::Or(alts) => {
                if let Some(first) = alts.first() {
                    self.project(fx, first, src, frames)?;
                }
            }
        }
        Ok(())
    }

    /// Read each field `subs` names out of `src`. A name binds the read
    /// itself, so it is an alias of the field, not a copy of it.
    fn project_fields(
        &mut self,
        fx: &mut Fx,
        subs: &[Pattern],
        src: &TypedBind,
        tuple: bool,
        frames: &mut Vec<Frame>,
    ) -> R<()> {
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
                let bind = self.bind(fx, b, ty)?;
                frames.push(Frame::Let(bind, read));
                continue;
            }
            let b = self.temp(fx, ty);
            frames.push(Frame::Let(b, read));
            self.project(fx, sub, &b, frames)?;
        }
        Ok(())
    }

    fn patterns(&mut self, fx: &mut Fx, ps: &[Pattern]) -> R<Vec<TypedPat>> {
        ps.iter().map(|p| self.pattern(fx, p)).collect()
    }

    fn pattern(&mut self, fx: &mut Fx, p: &Pattern) -> R<TypedPat> {
        let ty = self.ty(p.ty);
        Ok(match &p.kind {
            PatternKind::Wild | PatternKind::Error => TypedPat::Wild { ty },
            PatternKind::Bind(b) => {
                let mut bind = self.bind_own(fx, *b)?;
                bind.global = None;
                TypedPat::Bind(bind)
            }
            PatternKind::Int(i) => TypedPat::Lit {
                ty,
                value: self.constant(Const::Int(*i)),
            },
            PatternKind::Float(f) => TypedPat::Lit {
                ty,
                value: self.constant(Const::Float(*f)),
            },
            PatternKind::String(s) => TypedPat::Lit {
                ty,
                value: self.constant(Const::String(s.clone())),
            },
            PatternKind::Range(lo, hi) => TypedPat::Range {
                ty,
                lo: self.constant(Const::Int(*lo)),
                hi: self.constant(Const::Int(*hi)),
            },
            PatternKind::Ctor { ctor, fields } => TypedPat::Ctor {
                ty,
                variant: self.cx.ids.ctor_ref(&ctor.type_name, ctor.variant),
                fields: self.patterns(fx, fields)?,
            },
            PatternKind::Tuple(elems) => TypedPat::Tuple {
                ty,
                elems: self.patterns(fx, elems)?,
            },
            PatternKind::Array { prefix, rest } => {
                let elem_ty = self
                    .cx
                    .ids
                    .pool()
                    .con_arg(ty, 0)
                    .ok_or(InternalLinkError::NotAnArray(ty))?;
                let prefix = self.patterns(fx, prefix)?;
                let rest = self.rest(fx, rest, ty)?;
                let len = self.constant(Const::Int(prefix.len() as i64));
                TypedPat::Array {
                    ty,
                    elem_ty,
                    len,
                    prefix,
                    rest,
                }
            }
            PatternKind::Binary { segs, rest } => {
                let segs = segs
                    .iter()
                    .map(|s| self.bin_seg_pat(fx, s))
                    .collect::<R<_>>()?;
                let rest = self.rest(fx, rest, ty)?;
                let zero = self.constant(Const::Int(0));
                TypedPat::Bin {
                    ty,
                    zero,
                    segs,
                    rest,
                }
            }
            PatternKind::Or(alts) => TypedPat::Or {
                ty,
                alts: self.patterns(fx, alts)?,
            },
        })
    }

    fn rest(&mut self, fx: &mut Fx, rest: &Rest, ty: RTy) -> R<PatRest> {
        Ok(match rest {
            Rest::None => PatRest::None,
            Rest::Discard => PatRest::Discard,
            Rest::Bind(b) => {
                let mut bind = self.bind(fx, *b, ty)?;
                bind.global = None;
                PatRest::Bind(bind)
            }
        })
    }

    fn bin_seg_pat(&mut self, fx: &mut Fx, s: &BinSegPat) -> R<TypedBinPatSeg> {
        Ok(match s {
            BinSegPat::Text(text) => {
                let width = (text.len() as i64) * 8;
                let bytes = self.constant(Const::Binary {
                    bytes: text.as_bytes().to_vec(),
                    bit_len: width as u64,
                });
                let bits = self.constant(Const::Int(width));
                TypedBinPatSeg::Utf8Literal { bytes, bits }
            }
            BinSegPat::Utf8(p) => TypedBinPatSeg::Utf8 {
                value: self.pattern(fx, p)?,
            },
            BinSegPat::Bits { bits, value } => {
                let bits = match bits {
                    Some(e) => self.expr(fx, e)?,
                    None => self.int_const(8),
                };
                TypedBinPatSeg::Int {
                    bits,
                    value: self.pattern(fx, value)?,
                }
            }
            BinSegPat::Bytes { bytes, value } => {
                let bits = bytes
                    .as_ref()
                    .map(|e| self.bytes_to_bits(fx, e))
                    .transpose()?;
                TypedBinPatSeg::Binary {
                    bits,
                    value: self.pattern(fx, value)?,
                }
            }
        })
    }

    fn bytes_to_bits(&mut self, fx: &mut Fx, e: &Expr) -> R<TypedExpr> {
        let v = self.expr(fx, e)?;
        let eight = self.int_const(8);
        Ok(TypedExpr::Binary {
            ty: self.int(),
            op: PrimOp::IntMul,
            lhs: Box::new(v),
            rhs: Box::new(eight),
        })
    }

    fn binary_literal(&mut self, fx: &mut Fx, segs: &[BinSeg], ty: RTy) -> R<TypedExpr> {
        if segs.is_empty() {
            let value = self.constant(Const::Binary {
                bytes: Vec::new(),
                bit_len: 0,
            });
            return Ok(TypedExpr::Const { ty, value });
        }
        let segs = segs
            .iter()
            .map(|s| {
                let value = self.expr(fx, &s.value)?;
                Ok(match &s.unit {
                    BinUnit::Bits(bits) => {
                        let bits = match bits {
                            Some(e) => self.expr(fx, e)?,
                            None => self.int_const(8),
                        };
                        TypedBinSeg::Int { value, bits }
                    }
                    BinUnit::Bytes(bytes) => {
                        let bits = bytes
                            .as_ref()
                            .map(|e| self.bytes_to_bits(fx, e))
                            .transpose()?;
                        TypedBinSeg::Binary { value, bits }
                    }
                    BinUnit::Utf8 => TypedBinSeg::Utf8 { value },
                })
            })
            .collect::<R<_>>()?;
        Ok(TypedExpr::BinLit { ty, segs })
    }
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
