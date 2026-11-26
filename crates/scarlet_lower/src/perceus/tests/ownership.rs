//! A checker of finished Core IR, for tests. It goes along every way through a
//! function and checks four things. Every local is bound in one place and read
//! only in its scope. Every jump names a join that is in scope, with as many
//! arguments as it has parameters, of the same types. Every value that gets a drop
//! is given up exactly once, and not read after. And a switch has each case once and
//! leaves none out, a cell is taken apart only where the case it is in says which
//! constructor it is, and a probe has its operands and a place for what it finds.
//!
//! `scarlet_vm`'s tests run it over every program in the repository, so it reads
//! `scarlet_ir` and nothing else.

use std::collections::{BTreeMap, BTreeSet};

use scarlet_ir::TypeId;
use scarlet_ir::core_ir::{
    Atom, Cases, CoreBind, CoreExpr, CoreFn, GlobalSlot, JoinId, LocalId, PrimOp, ProbeOp,
    VariantRef,
};
use scarlet_ir::rty::{RTy, ResolvedPool};

type Set = BTreeSet<LocalId>;

/// Whether a function may set globals.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Body {
    Function,
    Toplevel,
}

/// How many fields each constructor of a type has, which is none when the type has
/// no constructors.
pub(crate) type Fields<'f> = &'f dyn Fn(TypeId) -> Vec<usize>;

/// Check `f`, or say what is wrong with it.
pub(crate) fn check(
    pool: &ResolvedPool,
    fields: Fields<'_>,
    f: &CoreFn,
    body: Body,
) -> Result<(), String> {
    let mut walk = Walk {
        pool,
        fields,
        body,
        ret: f.ret_ty,
        types: BTreeMap::new(),
        joins: BTreeMap::new(),
        declared: BTreeSet::new(),
        reached: BTreeSet::new(),
        slots: BTreeSet::new(),
    };
    let mut at = State::default();
    for p in &f.params {
        walk.bind(p, &mut at)?;
    }
    walk.expr(&f.body, at)
}

/// What is true at a point of a function.
#[derive(Clone, Default)]
struct State {
    scope: Set,
    /// The values that have to be given up from here on.
    owned: Set,
    /// Those that were owned and turned out to be a constructor with no fields.
    plain: Set,
    /// Those that are this constructor.
    known: BTreeMap<LocalId, VariantRef>,
}

impl State {
    fn read(&self, l: LocalId, walk: &Walk<'_>) -> Result<(), String> {
        if !self.scope.contains(&l) {
            return Err(format!("{l} is read out of its scope"));
        }
        if walk.gets_drop(l)? && !self.owned.contains(&l) && !self.plain.contains(&l) {
            return Err(format!("{l} is read after it was given up"));
        }
        Ok(())
    }
}

struct Join {
    params: Vec<RTy>,
    /// What its continuation reads or drops, beside its parameters.
    needs: Set,
    /// Whether every way through its continuation stops the run. Nothing need be
    /// given up then.
    stops: bool,
}

struct Walk<'p> {
    pool: &'p ResolvedPool,
    fields: Fields<'p>,
    body: Body,
    ret: RTy,
    types: BTreeMap<LocalId, RTy>,
    /// The joins in scope.
    joins: BTreeMap<JoinId, Join>,
    declared: BTreeSet<JoinId>,
    /// Those that something jumps to.
    reached: BTreeSet<JoinId>,
    slots: BTreeSet<GlobalSlot>,
}

impl Walk<'_> {
    fn gets_drop(&self, l: LocalId) -> Result<bool, String> {
        let ty = self.types.get(&l).ok_or(format!("{l} is bound nowhere"))?;
        self.pool.gets_drop(*ty).map_err(|e| e.to_string())
    }

    fn bind(&mut self, b: &CoreBind, at: &mut State) -> Result<(), String> {
        if self.types.insert(b.id, b.ty).is_some() {
            return Err(format!("{} is bound twice", b.id));
        }
        at.scope.insert(b.id);
        if self.gets_drop(b.id)? {
            at.owned.insert(b.id);
        }
        Ok(())
    }

    /// Whether a field that `rhs` reads is one that its constructor has, where that is
    /// known.
    fn part(&self, rhs: &Atom, at: &State) -> Result<(), String> {
        if let Atom::PrimOp {
            op: PrimOp::Field(i),
            args,
        } = rhs
            && let [cell] = args.as_slice()
            && let Some(v) = at.known.get(cell)
        {
            let all = (self.fields)(v.type_id);
            let all = all.get(usize::from(v.variant_idx)).copied();
            if all.is_none_or(|n| usize::from(*i) >= n) {
                return Err(format!("{cell} has no field {i}"));
            }
        }
        Ok(())
    }

    fn expr(&mut self, mut e: &CoreExpr, mut at: State) -> Result<(), String> {
        loop {
            match e {
                CoreExpr::Let { bind, rhs, body } => {
                    let mut reads = Vec::new();
                    rhs.for_each_operand(|l| reads.push(l));
                    reads.iter().try_for_each(|l| at.read(*l, self))?;
                    self.part(rhs, &at)?;
                    self.bind(bind, &mut at)?;
                    e = body;
                }
                CoreExpr::SetGlobal { slot, value, body } => {
                    at.read(*value, self)?;
                    if self.body != Body::Toplevel || !self.slots.insert(*slot) {
                        return Err(format!("global {} is set again or in a function", slot.0));
                    }
                    e = body;
                }
                CoreExpr::Drop { local, body } => {
                    if !at.owned.remove(local) {
                        return Err(format!("{local} is dropped and is not owned"));
                    }
                    e = body;
                }
                CoreExpr::Join {
                    id,
                    params,
                    cont,
                    body,
                } => {
                    if !self.declared.insert(*id) {
                        return Err(format!("{id} is declared twice"));
                    }
                    let Free {
                        named: mut needs,
                        jumps,
                        ends,
                    } = free(cont);
                    let stop = |to| self.joins.get(to).is_some_and(|join| join.stops);
                    let stops = !ends && jumps.iter().all(stop);
                    params.iter().for_each(|p| {
                        needs.remove(&p.id);
                    });
                    // It hands over what the joins it goes on to need.
                    let further = jumps.iter().filter_map(|to| self.joins.get(to));
                    needs.extend(further.flat_map(|join| &join.needs));
                    let join = Join {
                        params: params.iter().map(|p| p.ty).collect(),
                        needs: needs.clone(),
                        stops,
                    };
                    self.joins.insert(*id, join);
                    self.expr(body, at.clone())?;
                    self.joins.remove(id);
                    // Nothing runs a continuation that nothing jumps to.
                    if !self.reached.contains(id) {
                        return Ok(());
                    }
                    if let Some(l) = needs.iter().find(|l| !at.scope.contains(l)) {
                        return Err(format!("the continuation of {id} reads {l} out of scope"));
                    }
                    at.owned.retain(|l| needs.contains(l));
                    for p in params {
                        self.bind(p, &mut at)?;
                    }
                    e = cont;
                }
                CoreExpr::If { cond, then, els } => {
                    at.read(*cond, self)?;
                    self.expr(then, at.clone())?;
                    e = els;
                }
                CoreExpr::Probe {
                    op,
                    args,
                    binds,
                    hit,
                    miss,
                } => {
                    args.iter().try_for_each(|l| at.read(*l, self))?;
                    let want = match op {
                        ProbeOp::ArrayGet | ProbeOp::MapGet => 2,
                        ProbeOp::ArraySlice | ProbeOp::ArraySet => 3,
                    };
                    if args.len() != want || binds.len() != 1 {
                        return Err(format!("a probe of {args:?} that finds {binds:?}"));
                    }
                    let mut found = at.clone();
                    for bind in binds.iter().flatten() {
                        self.bind(bind, &mut found)?;
                    }
                    self.expr(hit, found)?;
                    e = miss;
                }
                CoreExpr::Open {
                    cell,
                    variant,
                    fields,
                    body,
                } => {
                    if !at.owned.remove(cell) {
                        return Err(format!("{cell} is taken apart and is not owned"));
                    }
                    if at.known.get(cell) != Some(variant) {
                        return Err(format!("{cell} is taken apart as what it may not be"));
                    }
                    let all = (self.fields)(variant.type_id);
                    let all = all.get(usize::from(variant.variant_idx)).copied();
                    let in_order = fields.is_sorted_by(|(a, _), (b, _)| a < b);
                    let last = fields.last().map(|(field, _)| usize::from(*field));
                    if !in_order || last.is_none() || last >= all {
                        return Err(format!("{cell} is taken apart into fields it has not"));
                    }
                    for (_, bind) in fields {
                        self.bind(bind, &mut at)?;
                    }
                    e = body;
                }
                CoreExpr::Switch {
                    scrut,
                    cases,
                    default,
                } => {
                    at.read(*scrut, self)?;
                    return self.switch(*scrut, cases, default.as_deref(), &at);
                }
                CoreExpr::NoArm => return Ok(()),
                CoreExpr::Ret(value) => {
                    at.read(*value, self)?;
                    if self.types.get(value) != Some(&self.ret) {
                        return Err(format!("{value} is returned and is of another type"));
                    }
                    return self.nothing_but(&at, &Set::from([*value]), "a return");
                }
                CoreExpr::TailCall { callee, args } => {
                    let mut passed: Set = args.iter().copied().collect();
                    passed.extend(callee.local());
                    passed.iter().try_for_each(|l| at.read(*l, self))?;
                    return self.nothing_but(&at, &passed, "a tail call");
                }
                CoreExpr::Jump { to, args } => {
                    args.iter().try_for_each(|l| at.read(*l, self))?;
                    let join = self.joins.get(to).ok_or(format!("{to} is not in scope"))?;
                    self.reached.insert(*to);
                    let types: Vec<RTy> = args.iter().map(|l| self.types[l]).collect();
                    if types != join.params {
                        return Err(format!("{to} is given other arguments than it takes"));
                    }
                    // An argument the continuation reads too is shared with it.
                    let mut given: Set = args.iter().copied().collect();
                    given.extend(&join.needs);
                    for l in &join.needs {
                        at.read(*l, self)
                            .map_err(|why| format!("{why}, for {to}"))?;
                    }
                    if join.stops {
                        return Ok(());
                    }
                    return self.nothing_but(&at, &given, "a jump");
                }
            }
        }
    }

    fn switch(
        &mut self,
        scrut: LocalId,
        cases: &Cases,
        default: Option<&CoreExpr>,
        at: &State,
    ) -> Result<(), String> {
        let Cases::Variant { ty, arms } = cases else {
            let keys = match cases {
                Cases::Variant { .. } => Vec::new(),
                Cases::Int(arms) => lists(arms),
                Cases::Float(arms) => lists(arms),
                Cases::Text(arms) => lists(arms),
            };
            once(scrut, &keys)?;
            if default.is_none() {
                return Err(format!("a value of {scrut} has nowhere to go"));
            }
            let arms = cases.arms().into_iter().chain(default);
            return arms.into_iter().try_for_each(|e| self.expr(e, at.clone()));
        };
        let fields = (self.fields)(*ty);
        once(scrut, &lists(arms))?;
        let named: BTreeSet<u16> = arms.iter().flat_map(|(v, _)| v.iter().copied()).collect();
        if let Some(v) = named.iter().find(|v| usize::from(**v) >= fields.len()) {
            return Err(format!("{scrut} is of type {}, which has no {v}", ty.0));
        }
        if default.is_none() && named.len() < fields.len() {
            return Err(format!("a value of {scrut} has nowhere to go"));
        }
        let all = (0..).zip(&fields).map(|(v, _)| v);
        let rest: Vec<u16> = all.filter(|v| !named.contains(v)).collect();
        let default = default.map(|e| (rest.as_slice(), e));
        let arms = arms.iter().map(|(v, e)| (v.as_slice(), e));
        for (variants, e) in arms.chain(default) {
            let mut at = at.clone();
            let plain = variants.iter().all(|v| fields[usize::from(*v)] == 0);
            if plain && at.owned.remove(&scrut) {
                at.plain.insert(scrut);
            }
            if let [v] = variants {
                let variant = VariantRef {
                    type_id: *ty,
                    variant_idx: *v,
                };
                at.known.insert(scrut, variant);
            }
            self.expr(e, at)?;
        }
        Ok(())
    }

    /// Whether everything owned is among `given`.
    fn nothing_but(&self, at: &State, given: &Set, end: &str) -> Result<(), String> {
        match at.owned.difference(given).next() {
            Some(l) => Err(format!("{l} is never given up on a way to {end}")),
            None => Ok(()),
        }
    }
}

/// The keys of each case, as text.
fn lists<K: std::fmt::Debug>(arms: &[(Vec<K>, CoreExpr)]) -> Vec<Vec<String>> {
    let text = |keys: &Vec<K>| keys.iter().map(|k| format!("{k:?}")).collect();
    arms.iter().map(|(keys, _)| text(keys)).collect()
}

/// Whether no key is there twice, and no case is for none.
fn once(scrut: LocalId, keys: &[Vec<String>]) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    for key in keys.iter().flatten() {
        if !seen.insert(key) {
            return Err(format!("the switch on {scrut} has {key} twice"));
        }
    }
    match keys.iter().any(Vec::is_empty) {
        true => Err(format!("the switch on {scrut} has a case for nothing")),
        false => Ok(()),
    }
}

/// What an expression does of what is outside it. What nothing jumps to in it is
/// left out.
pub(crate) struct Free {
    /// What it reads or drops of the locals bound outside it.
    pub(crate) named: Set,
    /// The joins outside it that it jumps to.
    pub(crate) jumps: BTreeSet<JoinId>,
    /// Whether some way through it returns.
    ends: bool,
}

pub(crate) fn free(e: &CoreExpr) -> Free {
    let mut names = Names::default();
    names.walk(e);
    Free {
        named: names.named.difference(&names.bound).copied().collect(),
        ends: names.ends,
        jumps: names.jumps,
    }
}

#[derive(Default)]
struct Names {
    named: Set,
    bound: Set,
    jumps: BTreeSet<JoinId>,
    /// Whether some way returns.
    ends: bool,
}

impl Names {
    fn walk(&mut self, mut e: &CoreExpr) {
        loop {
            match e {
                CoreExpr::Let { bind, rhs, body } => {
                    self.bound.insert(bind.id);
                    rhs.for_each_operand(|l| {
                        self.named.insert(l);
                    });
                    e = body;
                }
                CoreExpr::SetGlobal { value, body, .. } => {
                    self.named.insert(*value);
                    e = body;
                }
                CoreExpr::Drop { local, body } => {
                    self.named.insert(*local);
                    e = body;
                }
                CoreExpr::Join {
                    id,
                    params,
                    cont,
                    body,
                } => {
                    self.bound.extend(params.iter().map(|p| p.id));
                    self.walk(body);
                    if !self.jumps.remove(id) {
                        return;
                    }
                    e = cont;
                }
                CoreExpr::If { cond, then, els } => {
                    self.named.insert(*cond);
                    self.walk(then);
                    e = els;
                }
                CoreExpr::Probe {
                    args,
                    binds,
                    hit,
                    miss,
                    ..
                } => {
                    self.named.extend(args);
                    self.bound.extend(binds.iter().flatten().map(|b| b.id));
                    self.walk(hit);
                    e = miss;
                }
                CoreExpr::Open {
                    cell, fields, body, ..
                } => {
                    self.named.insert(*cell);
                    self.bound.extend(fields.iter().map(|(_, b)| b.id));
                    e = body;
                }
                CoreExpr::Switch {
                    scrut,
                    cases,
                    default,
                } => {
                    self.named.insert(*scrut);
                    let arms = cases.arms().into_iter().chain(default.as_deref());
                    arms.for_each(|arm| self.walk(arm));
                    return;
                }
                CoreExpr::NoArm => return,
                CoreExpr::Ret(value) => {
                    self.ends = true;
                    self.named.insert(*value);
                    return;
                }
                CoreExpr::TailCall { callee, args } => {
                    self.ends = true;
                    self.named.extend(callee.local());
                    self.named.extend(args);
                    return;
                }
                CoreExpr::Jump { to, args } => {
                    self.jumps.insert(*to);
                    self.named.extend(args);
                    return;
                }
            }
        }
    }
}
