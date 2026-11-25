//! A checker of finished Core IR, for tests. It goes along every way through a
//! function and checks three things. Every local is bound in one place and read
//! only in its scope. Every jump names a join that is in scope, with as many
//! arguments as it has parameters, of the same types. And every value that gets
//! a drop is given up exactly once, and not read after.
//!
//! `scarlet_vm`'s tests run it over every program in the repository, so it reads
//! `scarlet_ir` and nothing else.

use std::collections::{BTreeMap, BTreeSet};

use scarlet_ir::core_ir::{CoreBind, CoreExpr, CoreFn, CorePat, GlobalSlot, JoinId, LocalId};
use scarlet_ir::rty::{RTy, ResolvedPool};

type Set = BTreeSet<LocalId>;

/// Whether a function may set globals.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Body {
    Function,
    Toplevel,
}

/// Check `f`, or say what is wrong with it.
pub(crate) fn check(pool: &ResolvedPool, f: &CoreFn, body: Body) -> Result<(), String> {
    let mut walk = Walk {
        pool,
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

    fn expr(&mut self, mut e: &CoreExpr, mut at: State) -> Result<(), String> {
        loop {
            match e {
                CoreExpr::Let { bind, rhs, body } => {
                    let mut reads = Vec::new();
                    rhs.for_each_operand(|l| reads.push(l));
                    reads.iter().try_for_each(|l| at.read(*l, self))?;
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
                        stops,
                    } = free(cont);
                    params.iter().for_each(|p| {
                        needs.remove(&p.id);
                    });
                    // It hands over what the joins it goes on to need.
                    let further = jumps.iter().filter_map(|to| self.joins.get(to));
                    needs.extend(further.flat_map(|join| &join.needs));
                    if let Some(l) = needs.iter().find(|l| !at.scope.contains(l)) {
                        return Err(format!("the continuation of {id} reads {l} out of scope"));
                    }
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
                CoreExpr::Match { scrut, arms } => {
                    at.read(*scrut, self)?;
                    for (pat, arm) in arms {
                        let mut at = at.clone();
                        let no_fields =
                            matches!(pat, CorePat::Ctor { fields, .. } if fields.is_empty());
                        if no_fields && at.owned.remove(scrut) {
                            at.plain.insert(*scrut);
                        }
                        for b in pat.binds() {
                            self.bind(b, &mut at)?;
                        }
                        self.expr(arm, at)?;
                    }
                    return Ok(());
                }
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

    /// Whether everything owned is among `given`.
    fn nothing_but(&self, at: &State, given: &Set, end: &str) -> Result<(), String> {
        match at.owned.difference(given).next() {
            Some(l) => Err(format!("{l} is never given up on a way to {end}")),
            None => Ok(()),
        }
    }
}

/// What an expression does of what is outside it. What nothing jumps to in it is
/// left out.
pub(crate) struct Free {
    /// What it reads or drops of the locals bound outside it.
    pub(crate) named: Set,
    /// The joins outside it that it jumps to.
    pub(crate) jumps: BTreeSet<JoinId>,
    /// Whether every way through it stops the run.
    pub(crate) stops: bool,
}

pub(crate) fn free(e: &CoreExpr) -> Free {
    let mut names = Names::default();
    names.walk(e);
    Free {
        named: names.named.difference(&names.bound).copied().collect(),
        stops: !names.ends && names.jumps.is_empty(),
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
                CoreExpr::Match { scrut, arms } => {
                    self.named.insert(*scrut);
                    for (pat, arm) in arms {
                        self.bound.extend(pat.binds().map(|b| b.id));
                        self.walk(arm);
                    }
                    return;
                }
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
