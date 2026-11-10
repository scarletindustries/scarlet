//! Hindley-Milner inference for one module check. Each check makes its own
//! engine and drops it at the end, so nothing it holds can leak into another
//! module. What leaves the check is a finished [`Type`], made by
//! [`Engine::zonk`].
//!
//! Types are nodes in an arena, and variables are a union-find structure:
//! solving a variable links it to a type, and [`Engine::find`] follows links.
//! Generalisation is level-based. Each variable has the depth of the `let` it
//! was made under, and binding a variable lowers every variable inside the
//! type it binds to, so an inner variable an outer scope can see is never
//! quantified too early.

use std::collections::HashSet;

use crate::known::Known;
use crate::name::{Name, TypeName};
use crate::ty::{Constraint, Prim, Scheme, Type, TypeVar};

/// An inference type: an index into the engine's arena.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Ty(u32);

/// A type variable's id, unique within one engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct VarId(u32);

#[derive(Debug, Clone)]
enum Node {
    Var(VarId),
    Con { name: TypeName, args: Vec<Ty> },
    Fn { params: Vec<Ty>, ret: Ty },
    Tuple(Vec<Ty>),
}

#[derive(Debug, Clone, Copy)]
enum VarState {
    /// Not solved yet. Unifying can bind it.
    Unbound {
        level: u32,
        constraint: Option<Constraint>,
    },
    /// A variable that stands for "any type the caller picks": a generic
    /// written in a signature, or one generalisation quantified. Nothing can
    /// solve it to a concrete type. It unifies only with itself, or absorbs an
    /// unbound variable.
    Rigid {
        level: u32,
        constraint: Option<Constraint>,
    },
    Link(Ty),
}

/// A scheme inside one check: `ty`, generic over `vars`. Each use instantiates
/// it with fresh variables. It stays an engine type until the module is done,
/// because the uses need it as one.
#[derive(Debug, Clone)]
pub struct LocalScheme {
    pub vars: Vec<VarId>,
    pub ty: Ty,
}

impl LocalScheme {
    pub fn mono(ty: Ty) -> Self {
        LocalScheme {
            vars: Vec::new(),
            ty,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum UnifyError {
    Mismatch {
        expected: Ty,
        given: Ty,
        arity: Option<ArityMismatch>,
    },
    /// A variable would have to contain itself, as `x(x)` asks for.
    Recursive,
}

#[derive(Debug, Clone, Copy)]
pub struct ArityMismatch {
    pub expected: usize,
    pub given: usize,
}

impl UnifyError {
    fn flip(self) -> Self {
        match self {
            UnifyError::Mismatch {
                expected,
                given,
                arity,
            } => UnifyError::Mismatch {
                expected: given,
                given: expected,
                arity,
            },
            UnifyError::Recursive => UnifyError::Recursive,
        }
    }

    /// The same failure, reported against the types that enclose the part
    /// that disagreed, so the user sees the whole shape.
    fn enclosed(self, expected: Ty, given: Ty) -> Self {
        match self {
            UnifyError::Mismatch { arity, .. } => UnifyError::Mismatch {
                expected,
                given,
                arity,
            },
            UnifyError::Recursive => UnifyError::Recursive,
        }
    }
}

fn mismatch(expected: Ty, given: Ty) -> UnifyError {
    UnifyError::Mismatch {
        expected,
        given,
        arity: None,
    }
}

/// Why a type can't be called with some number of arguments.
#[derive(Debug, Clone)]
pub enum NotCallable {
    /// It is a function, but it takes `params.len()` arguments.
    Arity { params: Vec<Ty>, ret: Ty },
    NotFn,
}

pub struct Engine {
    nodes: Vec<Node>,
    vars: Vec<VarState>,
    names: Vec<Option<Name>>,
    level: u32,
    used_names: HashSet<Name>,
    next_name: u32,
    known: Known,
}

impl Engine {
    pub fn new(known: Known) -> Self {
        Engine {
            nodes: Vec::new(),
            vars: Vec::new(),
            names: Vec::new(),
            level: 0,
            used_names: HashSet::new(),
            next_name: 0,
            known,
        }
    }

    pub fn known(&self) -> &Known {
        &self.known
    }

    fn push(&mut self, node: Node) -> Ty {
        let t = Ty(self.nodes.len() as u32);
        self.nodes.push(node);
        t
    }

    fn node(&self, t: Ty) -> &Node {
        &self.nodes[t.0 as usize]
    }

    fn new_var(&mut self, state: VarState, name: Option<Name>) -> Ty {
        let id = VarId(self.vars.len() as u32);
        self.vars.push(state);
        if let Some(n) = &name {
            self.used_names.insert(n.clone());
        }
        self.names.push(name);
        self.push(Node::Var(id))
    }

    pub fn fresh(&mut self) -> Ty {
        self.new_var(
            VarState::Unbound {
                level: self.level,
                constraint: None,
            },
            None,
        )
    }

    pub fn fresh_constrained(&mut self, c: Constraint) -> Ty {
        self.new_var(
            VarState::Unbound {
                level: self.level,
                constraint: Some(c),
            },
            None,
        )
    }

    /// A generic written in a signature, like `a` in `fn id(x a) a`.
    pub fn rigid(&mut self, name: Name) -> Ty {
        self.new_var(
            VarState::Rigid {
                level: self.level,
                constraint: None,
            },
            Some(name),
        )
    }

    pub fn con(&mut self, name: TypeName, args: Vec<Ty>) -> Ty {
        self.push(Node::Con { name, args })
    }

    pub fn fun(&mut self, params: Vec<Ty>, ret: Ty) -> Ty {
        self.push(Node::Fn { params, ret })
    }

    pub fn tuple(&mut self, elems: Vec<Ty>) -> Ty {
        self.push(Node::Tuple(elems))
    }

    /// A prelude type with no arguments, like `Int`.
    pub fn prelude(&mut self, name: &TypeName) -> Ty {
        self.con(name.clone(), Vec::new())
    }

    pub fn int(&mut self) -> Ty {
        let n = self.known.int.clone();
        self.prelude(&n)
    }

    pub fn float(&mut self) -> Ty {
        let n = self.known.float.clone();
        self.prelude(&n)
    }

    pub fn string(&mut self) -> Ty {
        let n = self.known.string.clone();
        self.prelude(&n)
    }

    pub fn bool(&mut self) -> Ty {
        let n = self.known.bool.clone();
        self.prelude(&n)
    }

    pub fn nil(&mut self) -> Ty {
        let n = self.known.nil.clone();
        self.prelude(&n)
    }

    pub fn binary(&mut self) -> Ty {
        let n = self.known.binary.clone();
        self.prelude(&n)
    }

    pub fn array(&mut self, elem: Ty) -> Ty {
        let n = self.known.array.clone();
        self.con(n, vec![elem])
    }

    /// Follow links to the type `t` stands for, shortening the chain on the
    /// way.
    pub fn find(&mut self, t: Ty) -> Ty {
        let mut root = t;
        while let Node::Var(v) = self.node(root)
            && let VarState::Link(next) = self.vars[v.0 as usize]
        {
            root = next;
        }
        let mut cur = t;
        while let Node::Var(v) = *self.node(cur)
            && let VarState::Link(next) = self.vars[v.0 as usize]
        {
            self.vars[v.0 as usize] = VarState::Link(root);
            cur = next;
        }
        root
    }

    /// The parts of a solved type: what it is, as far as inference knows.
    pub fn shape(&mut self, t: Ty) -> Shape {
        let r = self.find(t);
        match self.node(r).clone() {
            Node::Var(v) => Shape::Var(v),
            Node::Con { name, args } => Shape::Con(name, args),
            Node::Fn { params, ret } => Shape::Fn(params, ret),
            Node::Tuple(elems) => Shape::Tuple(elems),
        }
    }

    pub fn prim_of(&mut self, t: Ty) -> Option<Prim> {
        match self.shape(t) {
            Shape::Con(name, args) if args.is_empty() => self.known.prim_of(&name),
            Shape::Con(..) | Shape::Var(_) | Shape::Fn(..) | Shape::Tuple(_) => None,
        }
    }

    pub fn enter_level(&mut self) {
        self.level += 1;
    }

    pub fn leave_level(&mut self) {
        self.level -= 1;
    }

    pub fn unify(&mut self, expected: Ty, given: Ty) -> Result<(), UnifyError> {
        let a = self.find(expected);
        let b = self.find(given);
        if a == b {
            return Ok(());
        }
        match (self.node(a).clone(), self.node(b).clone()) {
            (Node::Var(x), Node::Var(y)) if x == y => Ok(()),
            (Node::Var(x), _) => self.unify_var(x, a, b),
            (_, Node::Var(_)) => self.unify(b, a).map_err(UnifyError::flip),
            (Node::Con { name: na, args: aa }, Node::Con { name: nb, args: ab }) => {
                if na != nb || aa.len() != ab.len() {
                    return Err(mismatch(a, b));
                }
                self.unify_all(&aa, &ab, a, b)
            }
            (Node::Fn { params: pa, ret: ra }, Node::Fn { params: pb, ret: rb }) => {
                if pa.len() != pb.len() {
                    return Err(UnifyError::Mismatch {
                        expected: a,
                        given: b,
                        arity: Some(ArityMismatch {
                            expected: pa.len(),
                            given: pb.len(),
                        }),
                    });
                }
                self.unify_all(&pa, &pb, a, b)?;
                self.unify(ra, rb).map_err(|e| e.enclosed(a, b))
            }
            (Node::Tuple(ea), Node::Tuple(eb)) => {
                if ea.len() != eb.len() {
                    return Err(mismatch(a, b));
                }
                self.unify_all(&ea, &eb, a, b)
            }
            (Node::Con { .. } | Node::Fn { .. } | Node::Tuple(_), _) => Err(mismatch(a, b)),
        }
    }

    fn unify_all(&mut self, xs: &[Ty], ys: &[Ty], outer_a: Ty, outer_b: Ty) -> Result<(), UnifyError> {
        for (&x, &y) in xs.iter().zip(ys) {
            self.unify(x, y).map_err(|e| e.enclosed(outer_a, outer_b))?;
        }
        Ok(())
    }

    /// `var` is a root variable and `ty` is a root that isn't the same
    /// variable.
    fn unify_var(&mut self, var: VarId, var_ty: Ty, ty: Ty) -> Result<(), UnifyError> {
        match self.vars[var.0 as usize] {
            VarState::Link(_) => Err(mismatch(var_ty, ty)),
            VarState::Rigid { constraint, .. } => {
                // A rigid variable only absorbs an unbound one, which then
                // links to it. It must already guarantee that one's
                // constraint, because linking drops it.
                if let Node::Var(other) = *self.node(ty)
                    && let VarState::Unbound {
                        constraint: required,
                        ..
                    } = self.vars[other.0 as usize]
                {
                    if let Some(req) = required
                        && !constraint.is_some_and(|c| c.implies(req))
                    {
                        return Err(mismatch(var_ty, ty));
                    }
                    self.vars[other.0 as usize] = VarState::Link(var_ty);
                    return Ok(());
                }
                Err(mismatch(var_ty, ty))
            }
            VarState::Unbound { level, constraint } => {
                let mut lowered = Vec::new();
                if self.occurs_and_lower(var, level, ty, &mut lowered) {
                    self.restore_levels(&lowered);
                    return Err(UnifyError::Recursive);
                }
                if let Some(c) = constraint
                    && let Err(e) = self.meet_constraint(c, level, var_ty, ty)
                {
                    // A failed unification must not leave levels lowered, or
                    // a later generalisation quantifies too little and one
                    // real error turns into several false ones.
                    self.restore_levels(&lowered);
                    return Err(e);
                }
                self.vars[var.0 as usize] = VarState::Link(ty);
                Ok(())
            }
        }
    }

    /// Check that `ty` can meet constraint `c`, moving the constraint onto
    /// `ty` when it is a variable itself.
    fn meet_constraint(&mut self, c: Constraint, level: u32, var_ty: Ty, ty: Ty) -> Result<(), UnifyError> {
        match self.node(ty).clone() {
            Node::Var(other) => match self.vars[other.0 as usize] {
                VarState::Unbound {
                    level: other_level,
                    constraint: other_c,
                } => {
                    self.vars[other.0 as usize] = VarState::Unbound {
                        level: other_level.min(level),
                        constraint: Some(match other_c {
                            Some(o) => o.intersect(c),
                            None => c,
                        }),
                    };
                    Ok(())
                }
                VarState::Rigid { constraint: rc, .. } => {
                    if rc.is_some_and(|r| r.implies(c)) {
                        Ok(())
                    } else {
                        Err(mismatch(var_ty, ty))
                    }
                }
                VarState::Link(_) => Err(mismatch(var_ty, ty)),
            },
            Node::Con { name, args } if args.is_empty() => match self.known.prim_of(&name) {
                Some(p) if c.allows(p) => Ok(()),
                Some(_) | None => Err(mismatch(var_ty, ty)),
            },
            Node::Con { .. } | Node::Fn { .. } | Node::Tuple(_) => Err(mismatch(var_ty, ty)),
        }
    }

    fn occurs_and_lower(&mut self, var: VarId, level: u32, t: Ty, lowered: &mut Vec<(VarId, u32)>) -> bool {
        let r = self.find(t);
        match self.node(r).clone() {
            Node::Var(v) => {
                if v == var {
                    return true;
                }
                if let VarState::Unbound {
                    level: l,
                    constraint,
                } = self.vars[v.0 as usize]
                    && l > level
                {
                    lowered.push((v, l));
                    self.vars[v.0 as usize] = VarState::Unbound { level, constraint };
                }
                false
            }
            Node::Con { args, .. } | Node::Tuple(args) => {
                args.iter().any(|&a| self.occurs_and_lower(var, level, a, lowered))
            }
            Node::Fn { params, ret } => {
                params.iter().any(|&p| self.occurs_and_lower(var, level, p, lowered))
                    || self.occurs_and_lower(var, level, ret, lowered)
            }
        }
    }

    fn restore_levels(&mut self, lowered: &[(VarId, u32)]) {
        for &(v, old) in lowered.iter().rev() {
            if let VarState::Unbound { constraint, .. } = self.vars[v.0 as usize] {
                self.vars[v.0 as usize] = VarState::Unbound {
                    level: old,
                    constraint,
                };
            }
        }
    }

    /// The parameters and return type of `t` called with `arity` arguments.
    /// An unbound variable becomes a function of that arity.
    pub fn callable(&mut self, t: Ty, arity: usize) -> Result<(Vec<Ty>, Ty), NotCallable> {
        let r = self.find(t);
        match self.node(r).clone() {
            Node::Fn { params, ret } => {
                if params.len() == arity {
                    Ok((params, ret))
                } else {
                    Err(NotCallable::Arity { params, ret })
                }
            }
            Node::Var(v) => match self.vars[v.0 as usize] {
                VarState::Unbound { .. } => {
                    let params: Vec<Ty> = (0..arity).map(|_| self.fresh()).collect();
                    let ret = self.fresh();
                    let f = self.fun(params.clone(), ret);
                    match self.unify(r, f) {
                        Ok(()) => Ok((params, ret)),
                        Err(_) => Err(NotCallable::NotFn),
                    }
                }
                VarState::Rigid { .. } | VarState::Link(_) => Err(NotCallable::NotFn),
            },
            Node::Con { .. } | Node::Tuple(_) => Err(NotCallable::NotFn),
        }
    }

    /// Quantify every unbound variable in `ty` made deeper than the current
    /// level, and every rigid one, except those `restricted` keeps weak.
    ///
    /// Under the relaxed value restriction, a variable reachable through one
    /// of the `restricted` types stays unquantified, unless the type sits
    /// under a function, which hasn't built the value yet. For a mutable
    /// reference like `Subject`, quantifying its parameter would let one live
    /// value be written at one type and read at another.
    pub fn generalize(&mut self, ty: Ty, restricted: &[TypeName]) -> LocalScheme {
        let mut weak = HashSet::new();
        self.collect_weak(ty, restricted, false, &mut weak);
        let mut vars = Vec::new();
        self.collect_generalizable(ty, &weak, &mut vars);
        LocalScheme { vars, ty }
    }

    fn collect_generalizable(&mut self, t: Ty, weak: &HashSet<VarId>, out: &mut Vec<VarId>) {
        let r = self.find(t);
        match self.node(r).clone() {
            Node::Var(v) => {
                if out.contains(&v) || weak.contains(&v) {
                    return;
                }
                match self.vars[v.0 as usize] {
                    VarState::Unbound { level, constraint } if level > self.level => {
                        self.vars[v.0 as usize] = VarState::Rigid { level, constraint };
                        out.push(v);
                    }
                    VarState::Rigid { level, .. } if level > self.level => out.push(v),
                    VarState::Unbound { .. } | VarState::Rigid { .. } | VarState::Link(_) => {}
                }
            }
            Node::Con { args, .. } | Node::Tuple(args) => {
                args.iter().for_each(|&a| self.collect_generalizable(a, weak, out))
            }
            Node::Fn { params, ret } => {
                params.iter().for_each(|&p| self.collect_generalizable(p, weak, out));
                self.collect_generalizable(ret, weak, out);
            }
        }
    }

    fn collect_weak(&mut self, t: Ty, restricted: &[TypeName], inside: bool, weak: &mut HashSet<VarId>) {
        let r = self.find(t);
        match self.node(r).clone() {
            Node::Var(v) => {
                if inside {
                    weak.insert(v);
                }
            }
            Node::Con { name, args } => {
                let inside = inside || restricted.contains(&name);
                args.iter().for_each(|&a| self.collect_weak(a, restricted, inside, weak));
            }
            Node::Fn { params, ret } => {
                if inside {
                    params.iter().for_each(|&p| self.collect_weak(p, restricted, true, weak));
                    self.collect_weak(ret, restricted, true, weak);
                }
            }
            Node::Tuple(elems) => {
                elems.iter().for_each(|&e| self.collect_weak(e, restricted, inside, weak));
            }
        }
    }

    /// A fresh copy of `scheme.ty`, with a new variable for each of its
    /// generic ones.
    pub fn instantiate(&mut self, scheme: &LocalScheme) -> Ty {
        if scheme.vars.is_empty() {
            return scheme.ty;
        }
        let fresh: Vec<(VarId, Ty)> = scheme
            .vars
            .iter()
            .map(|&v| {
                let constraint = match self.vars[v.0 as usize] {
                    VarState::Unbound { constraint, .. } | VarState::Rigid { constraint, .. } => constraint,
                    VarState::Link(_) => None,
                };
                let name = self.names[v.0 as usize].clone();
                let t = self.new_var(
                    VarState::Unbound {
                        level: self.level,
                        constraint,
                    },
                    None,
                );
                if let Some(n) = name {
                    self.set_name(t, n);
                }
                (v, t)
            })
            .collect();
        self.copy_with(scheme.ty, &fresh)
    }

    fn copy_with(&mut self, t: Ty, subst: &[(VarId, Ty)]) -> Ty {
        let r = self.find(t);
        match self.node(r).clone() {
            Node::Var(v) => subst.iter().find(|(q, _)| *q == v).map_or(r, |&(_, t)| t),
            Node::Con { name, args } => {
                let args = args.iter().map(|&a| self.copy_with(a, subst)).collect();
                self.con(name, args)
            }
            Node::Fn { params, ret } => {
                let params = params.iter().map(|&p| self.copy_with(p, subst)).collect();
                let ret = self.copy_with(ret, subst);
                self.fun(params, ret)
            }
            Node::Tuple(elems) => {
                let elems = elems.iter().map(|&e| self.copy_with(e, subst)).collect();
                self.tuple(elems)
            }
        }
    }

    /// Bring a finished type into this engine, with `subst` giving the engine
    /// type for each of its variables. A variable `subst` doesn't cover gets
    /// a fresh one, the same one each time it comes up in `ty`.
    pub fn import(&mut self, ty: &Type, subst: &mut Vec<(TypeVar, Ty)>) -> Ty {
        match ty {
            Type::Var(v) => {
                if let Some((_, t)) = subst.iter().find(|(q, _)| q == v) {
                    return *t;
                }
                let t = match v.constraint {
                    Some(c) => self.fresh_constrained(c),
                    None => self.fresh(),
                };
                self.set_name(t, v.name.clone());
                subst.push((v.clone(), t));
                t
            }
            Type::Named { name, args } => {
                let args = args.iter().map(|a| self.import(a, subst)).collect();
                self.con(name.clone(), args)
            }
            Type::Fn { params, ret } => {
                let params = params.iter().map(|p| self.import(p, subst)).collect();
                let ret = self.import(ret, subst);
                self.fun(params, ret)
            }
            Type::Tuple(elems) => {
                let elems = elems.iter().map(|e| self.import(e, subst)).collect();
                self.tuple(elems)
            }
        }
    }

    /// A fresh instance of a finished scheme.
    pub fn instantiate_scheme(&mut self, scheme: &Scheme) -> Ty {
        let mut subst = Vec::new();
        self.import(&scheme.ty, &mut subst)
    }

    fn set_name(&mut self, t: Ty, name: Name) {
        if let Node::Var(v) = *self.node(t) {
            self.used_names.insert(name.clone());
            self.names[v.0 as usize] = Some(name);
        }
    }

    fn var_name(&mut self, v: VarId) -> Name {
        if let Some(n) = &self.names[v.0 as usize] {
            return n.clone();
        }
        let name = loop {
            let candidate: Name = letters(self.next_name).into();
            self.next_name += 1;
            if !self.used_names.contains(&candidate) {
                break candidate;
            }
        };
        self.used_names.insert(name.clone());
        self.names[v.0 as usize] = Some(name.clone());
        name
    }

    /// The finished type `t` stands for now. A variable nothing solved stays
    /// a variable, named the same way every time.
    pub fn zonk(&mut self, t: Ty) -> Type {
        let r = self.find(t);
        match self.node(r).clone() {
            Node::Var(v) => Type::Var(self.type_var(v)),
            Node::Con { name, args } => {
                let args = args.iter().map(|&a| self.zonk(a)).collect::<Vec<_>>();
                Type::named(name, args)
            }
            Node::Fn { params, ret } => {
                let params = params.iter().map(|&p| self.zonk(p)).collect::<Vec<_>>();
                let ret = self.zonk(ret);
                Type::function(params, ret)
            }
            Node::Tuple(elems) => {
                let elems = elems.iter().map(|&e| self.zonk(e)).collect::<Vec<_>>();
                Type::tuple(elems)
            }
        }
    }

    pub fn zonk_scheme(&mut self, s: &LocalScheme) -> Scheme {
        let ty = self.zonk(s.ty);
        // Quantified variables are rigid, and a rigid variable is never
        // linked to anything, so each is still a root.
        let vars = s.vars.iter().map(|&v| self.type_var(v)).collect();
        Scheme { vars, ty }
    }

    fn type_var(&mut self, v: VarId) -> TypeVar {
        let constraint = match self.vars[v.0 as usize] {
            VarState::Unbound { constraint, .. } | VarState::Rigid { constraint, .. } => constraint,
            VarState::Link(_) => None,
        };
        TypeVar {
            id: v.0,
            name: self.var_name(v),
            constraint,
        }
    }

    pub fn display(&mut self, t: Ty) -> String {
        self.zonk(t).to_string()
    }
}

impl Engine {
    /// End inference. Every variable still open gets its display name now, so
    /// the frozen types read back the same every time.
    pub fn freeze(mut self) -> Types {
        for v in 0..self.vars.len() {
            if matches!(self.vars[v], VarState::Unbound { .. } | VarState::Rigid { .. }) {
                self.var_name(VarId(v as u32));
            }
        }
        Types { engine: self }
    }
}

/// A module's types once its check is over. Nothing can solve a variable any
/// more, so a [`Ty`] always reads back the same [`Type`].
pub struct Types {
    engine: Engine,
}

impl Types {
    fn find(&self, t: Ty) -> Ty {
        let mut r = t;
        while let Node::Var(v) = self.engine.node(r)
            && let VarState::Link(next) = self.engine.vars[v.0 as usize]
        {
            r = next;
        }
        r
    }

    pub fn get(&self, t: Ty) -> Type {
        let e = &self.engine;
        match e.node(self.find(t)) {
            Node::Var(v) => {
                let constraint = match e.vars[v.0 as usize] {
                    VarState::Unbound { constraint, .. } | VarState::Rigid { constraint, .. } => {
                        constraint
                    }
                    VarState::Link(_) => None,
                };
                Type::Var(TypeVar {
                    id: v.0,
                    name: e.names[v.0 as usize].clone().unwrap_or_else(|| "_".into()),
                    constraint,
                })
            }
            Node::Con { name, args } => {
                Type::named(name.clone(), args.iter().map(|&a| self.get(a)).collect())
            }
            Node::Fn { params, ret } => Type::function(
                params.iter().map(|&p| self.get(p)).collect(),
                self.get(*ret),
            ),
            Node::Tuple(elems) => Type::tuple(elems.iter().map(|&e| self.get(e)).collect()),
        }
    }

    pub fn known(&self) -> &Known {
        &self.engine.known
    }
}

impl std::fmt::Debug for Types {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Types")
    }
}

impl Clone for Types {
    fn clone(&self) -> Self {
        Types {
            engine: Engine {
                nodes: self.engine.nodes.clone(),
                vars: self.engine.vars.clone(),
                names: self.engine.names.clone(),
                level: self.engine.level,
                used_names: self.engine.used_names.clone(),
                next_name: self.engine.next_name,
                known: self.engine.known.clone(),
            },
        }
    }
}

/// What a type is, as far as inference knows right now.
#[derive(Debug, Clone)]
pub enum Shape {
    Var(VarId),
    Con(TypeName, Vec<Ty>),
    Fn(Vec<Ty>, Ty),
    Tuple(Vec<Ty>),
}

/// The base-26 display name for the `n`th variable: `a` to `z`, then `aa`.
fn letters(mut n: u32) -> String {
    let mut out = Vec::new();
    loop {
        out.push(b'a' + (n % 26) as u8);
        if n < 26 {
            break;
        }
        n = n / 26 - 1;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> Engine {
        Engine::new(Known::new())
    }

    #[test]
    fn letters_run_past_z() {
        assert_eq!(letters(0), "a");
        assert_eq!(letters(25), "z");
        assert_eq!(letters(26), "aa");
        assert_eq!(letters(27), "ab");
    }

    #[test]
    fn a_variable_unifies_with_a_concrete_type() {
        let mut e = engine();
        let v = e.fresh();
        let i = e.int();
        assert!(e.unify(v, i).is_ok());
        assert_eq!(e.display(v), "Int");
    }

    #[test]
    fn a_numeric_variable_refuses_a_string() {
        let mut e = engine();
        let v = e.fresh_constrained(Constraint::Numeric);
        let s = e.string();
        assert!(e.unify(v, s).is_err());
    }

    #[test]
    fn x_of_x_is_recursive() {
        let mut e = engine();
        let v = e.fresh();
        let f = e.fun(vec![v], v);
        assert!(matches!(e.unify(v, f), Err(UnifyError::Recursive)));
    }

    #[test]
    fn generalising_quantifies_only_deeper_variables() {
        let mut e = engine();
        let outer = e.fresh();
        e.enter_level();
        let inner = e.fresh();
        let f = e.fun(vec![inner], outer);
        e.leave_level();
        let s = e.generalize(f, &[]);
        assert_eq!(s.vars.len(), 1);
        let a = e.instantiate(&s);
        let b = e.instantiate(&s);
        let i = e.int();
        let st = e.string();
        let Ok((pa, _)) = e.callable(a, 1) else {
            panic!("an instance of a function scheme is callable")
        };
        let Ok((pb, _)) = e.callable(b, 1) else {
            panic!("an instance of a function scheme is callable")
        };
        assert!(e.unify(pa[0], i).is_ok());
        assert!(e.unify(pb[0], st).is_ok());
    }

    #[test]
    fn a_rigid_variable_refuses_a_concrete_type() {
        let mut e = engine();
        let a = e.rigid("a".into());
        let i = e.int();
        assert!(e.unify(a, i).is_err());
    }
}
