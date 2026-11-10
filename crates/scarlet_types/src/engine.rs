//! Hindley-Milner inference for one module check. Each check makes its own
//! engine and drops it at the end, so nothing it holds can leak into another
//! module. [`Engine::freeze`] ends inference, and what is left is [`Types`],
//! which can only be read.
//!
//! Types are nodes in an arena, and variables are a union-find structure:
//! solving a variable links it to a type.
//!
//! Generalisation is level-based. Each variable has the depth of the `let` it
//! was made under. [`Engine::enter`] goes one level deeper and hands back a
//! [`Level`], which only [`Engine::leave`] or [`Engine::generalize`] take
//! back, so going deeper and coming back out are one value's two ends.

use std::collections::HashSet;

use crate::known::Known;
use crate::name::{Name, TypeName};
use crate::ty::{Constraint, Scheme, Type, TypeVar, show_together};

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
    /// Stands for "any type the caller picks": a generic written in a
    /// signature, or one generalisation quantified. Nothing can solve it. It
    /// unifies only with itself, or absorbs an unbound variable.
    Rigid {
        level: u32,
        constraint: Option<Constraint>,
    },
    Link(Ty),
}

/// One level deeper than the code around it. It comes from
/// [`Engine::enter`], and going back out takes it, through
/// [`Engine::leave`] or [`Engine::generalize`].
#[must_use = "a level must be left, by `leave` or `generalize`"]
#[derive(Debug)]
pub struct Level(u32);

/// A scheme inside one check: `ty`, generic over `vars`. Each use instantiates
/// it with fresh variables.
#[derive(Debug, Clone)]
pub struct LocalScheme {
    vars: Vec<VarId>,
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
    Arity {
        params: Vec<Ty>,
        ret: Ty,
    },
    NotFn,
}

/// What a type is, as far as inference knows right now.
#[derive(Debug, Clone)]
pub enum Shape {
    Var(VarId),
    Con(TypeName, Vec<Ty>),
    Fn(Vec<Ty>, Ty),
    Tuple(Vec<Ty>),
}

type Trail = Vec<(VarId, VarState)>;

/// One type variable: what inference knows of it, and the name the source
/// gave it, if it gave one.
#[derive(Clone)]
struct Var {
    state: VarState,
    name: Option<Name>,
}

#[derive(Clone)]
pub struct Engine {
    nodes: Vec<Node>,
    vars: Vec<Var>,
    level: u32,
    known: Known,
}

impl Engine {
    pub fn new(known: Known) -> Self {
        Engine {
            nodes: Vec::new(),
            vars: Vec::new(),
            level: 0,
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
        self.vars.push(Var { state, name });
        self.push(Node::Var(id))
    }

    fn fresh_named(&mut self, constraint: Option<Constraint>, name: Option<Name>) -> Ty {
        self.new_var(
            VarState::Unbound {
                level: self.level,
                constraint,
            },
            name,
        )
    }

    pub fn fresh(&mut self) -> Ty {
        self.fresh_named(None, None)
    }

    pub fn fresh_constrained(&mut self, c: Constraint) -> Ty {
        self.fresh_named(Some(c), None)
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

    fn con(&mut self, name: TypeName, args: Vec<Ty>) -> Ty {
        self.push(Node::Con { name, args })
    }

    pub fn fun(&mut self, params: Vec<Ty>, ret: Ty) -> Ty {
        self.push(Node::Fn { params, ret })
    }

    pub fn tuple(&mut self, elems: Vec<Ty>) -> Ty {
        self.push(Node::Tuple(elems))
    }

    pub fn int(&mut self) -> Ty {
        self.con(self.known.int.clone(), Vec::new())
    }

    pub fn float(&mut self) -> Ty {
        self.con(self.known.float.clone(), Vec::new())
    }

    pub fn string(&mut self) -> Ty {
        self.con(self.known.string.clone(), Vec::new())
    }

    pub fn bool(&mut self) -> Ty {
        self.con(self.known.bool.clone(), Vec::new())
    }

    pub fn nil(&mut self) -> Ty {
        self.con(self.known.nil.clone(), Vec::new())
    }

    pub fn binary(&mut self) -> Ty {
        self.con(self.known.binary.clone(), Vec::new())
    }

    pub fn array(&mut self, elem: Ty) -> Ty {
        self.con(self.known.array.clone(), vec![elem])
    }

    pub fn option(&mut self, value: Ty) -> Ty {
        self.con(self.known.option.clone(), vec![value])
    }

    pub fn result(&mut self, ok: Ty, err: Ty) -> Ty {
        self.con(self.known.result.clone(), vec![ok, err])
    }

    /// The type `t` stands for, following links.
    fn find(&self, t: Ty) -> Ty {
        let mut r = t;
        while let Node::Var(v) = *self.node(r)
            && let VarState::Link(next) = self.vars[v.0 as usize].state
        {
            r = next;
        }
        r
    }

    /// What a type is, as far as inference knows right now.
    pub fn shape(&self, t: Ty) -> Shape {
        match self.node(self.find(t)).clone() {
            Node::Var(v) => Shape::Var(v),
            Node::Con { name, args } => Shape::Con(name, args),
            Node::Fn { params, ret } => Shape::Fn(params, ret),
            Node::Tuple(elems) => Shape::Tuple(elems),
        }
    }

    pub fn enter(&mut self) -> Level {
        self.level += 1;
        Level(self.level)
    }

    pub fn leave(&mut self, level: Level) {
        debug_assert_eq!(
            level.0, self.level,
            "levels are left in the order they were entered"
        );
        self.level = level.0.saturating_sub(1);
    }

    /// Unify, or leave every variable as it was. A failure part way through
    /// mustn't leave half the types joined, or one real error turns into
    /// several false ones.
    pub fn unify(&mut self, expected: Ty, given: Ty) -> Result<(), UnifyError> {
        let mut trail = Trail::new();
        let r = self.unify_in(expected, given, &mut trail);
        if r.is_err() {
            for (v, old) in trail.into_iter().rev() {
                self.vars[v.0 as usize].state = old;
            }
        }
        r
    }

    fn set(&mut self, v: VarId, state: VarState, trail: &mut Trail) {
        trail.push((v, self.vars[v.0 as usize].state));
        self.vars[v.0 as usize].state = state;
    }

    fn unify_in(&mut self, expected: Ty, given: Ty, trail: &mut Trail) -> Result<(), UnifyError> {
        let a = self.find(expected);
        let b = self.find(given);
        if a == b {
            return Ok(());
        }
        match (self.node(a).clone(), self.node(b).clone()) {
            (Node::Var(x), Node::Var(y)) if x == y => Ok(()),
            (Node::Var(x), _) => self.unify_var(x, a, b, trail),
            (_, Node::Var(_)) => self.unify_in(b, a, trail).map_err(UnifyError::flip),
            (Node::Con { name: na, args: aa }, Node::Con { name: nb, args: ab }) => {
                if na != nb || aa.len() != ab.len() {
                    return Err(mismatch(a, b));
                }
                self.unify_all(&aa, &ab, a, b, trail)
            }
            (
                Node::Fn {
                    params: pa,
                    ret: ra,
                },
                Node::Fn {
                    params: pb,
                    ret: rb,
                },
            ) => {
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
                self.unify_all(&pa, &pb, a, b, trail)?;
                self.unify_in(ra, rb, trail).map_err(|e| e.enclosed(a, b))
            }
            (Node::Tuple(ea), Node::Tuple(eb)) => {
                if ea.len() != eb.len() {
                    return Err(mismatch(a, b));
                }
                self.unify_all(&ea, &eb, a, b, trail)
            }
            (Node::Con { .. } | Node::Fn { .. } | Node::Tuple(_), _) => Err(mismatch(a, b)),
        }
    }

    fn unify_all(
        &mut self,
        xs: &[Ty],
        ys: &[Ty],
        outer_a: Ty,
        outer_b: Ty,
        trail: &mut Trail,
    ) -> Result<(), UnifyError> {
        for (&x, &y) in xs.iter().zip(ys) {
            self.unify_in(x, y, trail)
                .map_err(|e| e.enclosed(outer_a, outer_b))?;
        }
        Ok(())
    }

    /// `var` is a root variable, and `ty` a root that isn't the same variable.
    fn unify_var(
        &mut self,
        var: VarId,
        var_ty: Ty,
        ty: Ty,
        trail: &mut Trail,
    ) -> Result<(), UnifyError> {
        match self.vars[var.0 as usize].state {
            VarState::Link(_) => Err(mismatch(var_ty, ty)),
            VarState::Rigid { constraint, .. } => {
                // A rigid variable only absorbs an unbound one, which then
                // links to it. It must already guarantee that one's
                // constraint, because linking drops it.
                if let Node::Var(other) = *self.node(ty)
                    && let VarState::Unbound {
                        constraint: required,
                        ..
                    } = self.vars[other.0 as usize].state
                {
                    if let Some(req) = required
                        && !constraint.is_some_and(|c| c.implies(req))
                    {
                        return Err(mismatch(var_ty, ty));
                    }
                    self.set(other, VarState::Link(var_ty), trail);
                    return Ok(());
                }
                Err(mismatch(var_ty, ty))
            }
            VarState::Unbound { level, constraint } => {
                if self.occurs_and_lower(var, level, ty, trail) {
                    return Err(UnifyError::Recursive);
                }
                if let Some(c) = constraint {
                    self.meet_constraint(c, level, var_ty, ty, trail)?;
                }
                self.set(var, VarState::Link(ty), trail);
                Ok(())
            }
        }
    }

    /// Check that `ty` can meet constraint `c`, moving the constraint onto
    /// `ty` when it is a variable itself.
    fn meet_constraint(
        &mut self,
        c: Constraint,
        level: u32,
        var_ty: Ty,
        ty: Ty,
        trail: &mut Trail,
    ) -> Result<(), UnifyError> {
        match self.node(ty).clone() {
            Node::Var(other) => match self.vars[other.0 as usize].state {
                VarState::Unbound {
                    level: other_level,
                    constraint: other_c,
                } => {
                    let state = VarState::Unbound {
                        level: other_level.min(level),
                        constraint: Some(match other_c {
                            Some(o) => o.intersect(c),
                            None => c,
                        }),
                    };
                    self.set(other, state, trail);
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

    /// Whether `var` occurs in `t`. On the way, every unbound variable in `t`
    /// deeper than `level` comes up to it, because `t` is about to be visible
    /// wherever `var` is.
    fn occurs_and_lower(&mut self, var: VarId, level: u32, t: Ty, trail: &mut Trail) -> bool {
        let r = self.find(t);
        match self.node(r).clone() {
            Node::Var(v) => {
                if v == var {
                    return true;
                }
                if let VarState::Unbound {
                    level: l,
                    constraint,
                } = self.vars[v.0 as usize].state
                    && l > level
                {
                    self.set(v, VarState::Unbound { level, constraint }, trail);
                }
                false
            }
            Node::Con { args, .. } | Node::Tuple(args) => args
                .iter()
                .any(|&a| self.occurs_and_lower(var, level, a, trail)),
            Node::Fn { params, ret } => {
                params
                    .iter()
                    .any(|&p| self.occurs_and_lower(var, level, p, trail))
                    || self.occurs_and_lower(var, level, ret, trail)
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
            Node::Var(v) => match self.vars[v.0 as usize].state {
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

    /// Leave `level`, and quantify every variable in `ty` made at it or
    /// deeper, except those `restricted` keeps weak.
    ///
    /// Under the relaxed value restriction, a variable reachable through one
    /// of the `restricted` types stays one type for every use, unless the
    /// type sits under a function, which hasn't built the value yet. For a
    /// mutable reference like `Subject`, quantifying its parameter would let
    /// one live value be written at one type and read at another. A weak
    /// variable comes up to the outer level, so no generalisation out there
    /// can quantify it either.
    pub fn generalize(&mut self, level: Level, ty: Ty, restricted: &[TypeName]) -> LocalScheme {
        self.generalize_all(level, &[(ty, restricted)])
            .pop()
            .unwrap_or(LocalScheme::mono(ty))
    }

    /// [`Engine::generalize`] for several types made at one level, like the
    /// members of a group of functions that use each other.
    pub fn generalize_all(
        &mut self,
        level: Level,
        items: &[(Ty, &[TypeName])],
    ) -> Vec<LocalScheme> {
        self.leave(level);
        items
            .iter()
            .map(|&(ty, restricted)| self.quantify(ty, restricted))
            .collect()
    }

    fn quantify(&mut self, ty: Ty, restricted: &[TypeName]) -> LocalScheme {
        let mut weak = HashSet::new();
        self.collect_weak(ty, restricted, false, &mut weak);
        for &v in &weak {
            if let VarState::Unbound { level, constraint } = self.vars[v.0 as usize].state
                && level > self.level
            {
                self.vars[v.0 as usize].state = VarState::Unbound {
                    level: self.level,
                    constraint,
                };
            }
        }
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
                match self.vars[v.0 as usize].state {
                    VarState::Unbound { level, constraint } if level > self.level => {
                        self.vars[v.0 as usize].state = VarState::Rigid { level, constraint };
                        out.push(v);
                    }
                    VarState::Rigid { level, .. } if level > self.level => out.push(v),
                    VarState::Unbound { .. } | VarState::Rigid { .. } | VarState::Link(_) => {}
                }
            }
            Node::Con { args, .. } | Node::Tuple(args) => args
                .iter()
                .for_each(|&a| self.collect_generalizable(a, weak, out)),
            Node::Fn { params, ret } => {
                params
                    .iter()
                    .for_each(|&p| self.collect_generalizable(p, weak, out));
                self.collect_generalizable(ret, weak, out);
            }
        }
    }

    fn collect_weak(
        &self,
        t: Ty,
        restricted: &[TypeName],
        inside: bool,
        weak: &mut HashSet<VarId>,
    ) {
        match self.node(self.find(t)) {
            Node::Var(v) => {
                if inside {
                    weak.insert(*v);
                }
            }
            Node::Con { name, args } => {
                let inside = inside || restricted.contains(name);
                args.iter()
                    .for_each(|&a| self.collect_weak(a, restricted, inside, weak));
            }
            Node::Fn { params, ret } => {
                if inside {
                    params
                        .iter()
                        .for_each(|&p| self.collect_weak(p, restricted, true, weak));
                    self.collect_weak(*ret, restricted, true, weak);
                }
            }
            Node::Tuple(elems) => elems
                .iter()
                .for_each(|&e| self.collect_weak(e, restricted, inside, weak)),
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
                let constraint = match self.vars[v.0 as usize].state {
                    VarState::Unbound { constraint, .. } | VarState::Rigid { constraint, .. } => {
                        constraint
                    }
                    VarState::Link(_) => None,
                };
                let name = self.vars[v.0 as usize].name.clone();
                (v, self.fresh_named(constraint, name))
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

    /// Bring a finished type into this engine. `subst` gives the engine type
    /// for each of its variables; a variable it doesn't cover gets a fresh
    /// one, the same one each time it comes up, and is added.
    pub fn import(&mut self, ty: &Type, subst: &mut Vec<(TypeVar, Ty)>) -> Ty {
        match ty {
            Type::Var(v) => {
                if let Some((_, t)) = subst.iter().find(|(q, _)| q == v) {
                    return *t;
                }
                let t = self.fresh_named(v.constraint, v.name.clone());
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
        self.import(&scheme.ty, &mut Vec::new())
    }

    /// The finished type `t` stands for now.
    fn zonk(&self, t: Ty) -> Type {
        match self.node(self.find(t)) {
            Node::Var(v) => Type::Var(self.type_var(*v)),
            Node::Con { name, args } => {
                Type::named(name.clone(), args.iter().map(|&a| self.zonk(a)).collect())
            }
            Node::Fn { params, ret } => Type::function(
                params.iter().map(|&p| self.zonk(p)).collect(),
                self.zonk(*ret),
            ),
            Node::Tuple(elems) => Type::tuple(elems.iter().map(|&e| self.zonk(e)).collect()),
        }
    }

    fn type_var(&self, v: VarId) -> TypeVar {
        let constraint = match self.vars[v.0 as usize].state {
            VarState::Unbound { constraint, .. } | VarState::Rigid { constraint, .. } => constraint,
            VarState::Link(_) => None,
        };
        TypeVar {
            id: v.0,
            name: self.vars[v.0 as usize].name.clone(),
            constraint,
        }
    }

    /// Show `types` together, so a variable they share gets one letter.
    pub fn show(&self, types: &[Ty]) -> Vec<String> {
        let zonked: Vec<Type> = types.iter().map(|&t| self.zonk(t)).collect();
        show_together(&zonked.iter().collect::<Vec<_>>())
    }

    /// End inference. What is left can only be read.
    pub fn freeze(self) -> Types {
        debug_assert_eq!(self.level, 0, "every level was left before inference ended");
        Types { engine: self }
    }
}

/// A module's types once its check is over. Nothing can solve a variable any
/// more, so a [`Ty`] always reads back the same [`Type`].
#[derive(Clone)]
pub struct Types {
    engine: Engine,
}

impl Types {
    pub fn get(&self, t: Ty) -> Type {
        self.engine.zonk(t)
    }

    /// A scheme, finished. Quantified variables are rigid, so each is still a
    /// root.
    pub fn scheme(&self, s: &LocalScheme) -> Scheme {
        Scheme {
            vars: s.vars.iter().map(|&v| self.engine.type_var(v)).collect(),
            ty: self.get(s.ty),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> Engine {
        Engine::new(Known::new())
    }

    #[test]
    fn a_variable_unifies_with_a_concrete_type() {
        let mut e = engine();
        let v = e.fresh();
        let i = e.int();
        assert!(e.unify(v, i).is_ok());
        assert_eq!(e.zonk(v).to_string(), "Int");
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
    fn a_failed_unify_changes_nothing() {
        let mut e = engine();
        let a = e.fresh();
        let i = e.int();
        let s = e.string();
        let left = e.fun(vec![a, i], i);
        let right = e.fun(vec![s, s], i);
        assert!(e.unify(left, right).is_err());
        assert!(
            matches!(e.shape(a), Shape::Var(_)),
            "`a` stayed joined to String"
        );
    }

    #[test]
    fn generalising_quantifies_only_deeper_variables() {
        let mut e = engine();
        let outer = e.fresh();
        let level = e.enter();
        let inner = e.fresh();
        let f = e.fun(vec![inner], outer);
        let s = e.generalize(level, f, &[]);
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
