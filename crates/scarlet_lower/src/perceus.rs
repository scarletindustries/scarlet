//! Perceus is the pass that adds reference counting to Core IR. It decides
//! where each heap value is released. This is the dropping half of Lorenzen &
//! Leijen (ICFP'22, Fig. 5), run as a pass from Core IR to Core IR. In ANF,
//! finding a value's last use is one backward scan.
//!
//! A backward liveness walk inserts a `Drop` after each heap bind's last read.
//! Where `If` and `Match` branches join, ownership is evened out so every path
//! releases every owned slot exactly once (Fig. 5's Δ-rule).

use std::collections::{BTreeMap, BTreeSet};

use super::{Atom, CoreBind, CoreExpr, CoreFn, CorePat, JoinId, LocalId};
use crate::program::InternalLinkError;
use crate::typed_ir::{RTy, ResolvedPool};
use scarlet_ir::rty::Repr;
use scarlet_ir::tivec::TiVec;

/// A function with its drops.
pub(crate) struct Dropped {
    pub(crate) core: CoreFn,
    /// [`LoweredFn::undropped`](super::LoweredFn::undropped).
    pub(crate) undropped: bool,
}

/// Run Perceus on one lowered function. `locals` is the type of every local
/// `lower` made for it, and `pool` says which of those types can be cells. Its
/// types are always resolved, so no unsolved variable can get here and make
/// `is_heap` answer `false` by accident.
///
/// A bind pinned to a global slot is never released here, because function
/// bodies read the global for as long as the program runs, long after the
/// toplevel's last read of it.
///
/// A value that can be no cell gets no drop, unless the function holds one that
/// may be a handle and that no drop gives up. Which registers the VM gives out
/// there says when that one is released, and a drop says how long a register is
/// held. So there each value keeps its drop, and the VM leaves out those of
/// values that are no cell.
pub(crate) fn perceus(
    pool: &ResolvedPool,
    locals: &TiVec<LocalId, RTy>,
    f: CoreFn,
) -> Result<Dropped, InternalLinkError> {
    let mut cx = Perceus::new(pool, locals, Words::Bare);
    let core = cx.function(f)?;
    let core = if cx.undropped && cx.left_bare {
        Perceus::new(pool, locals, Words::Dropped).function(core)?
    } else {
        core
    };
    Ok(Dropped {
        core,
        undropped: cx.undropped,
    })
}

/// Whether a value that can be no cell gets a drop.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Words {
    Bare,
    Dropped,
}

/// Whether what `atom` gives holds nothing, as a constant does not, or only what
/// something else goes on holding: a global, or the closure that the frame runs
/// as.
fn holds_nothing(atom: &Atom) -> bool {
    match atom {
        Atom::Const(_) | Atom::Nil | Atom::Bool(_) | Atom::Load(_) => true,
        Atom::Ctor { fields, .. } => fields.is_empty(),
        Atom::Closure { captures, .. } => captures.is_empty(),
        Atom::Local(_)
        | Atom::Update { .. }
        | Atom::PrimOp { .. }
        | Atom::Call { .. }
        | Atom::Intrinsic { .. } => false,
    }
}

/// The locals live at a program point. It's a `BTreeSet` so the order drops
/// are inserted in, and so the golden snapshots, never changes between runs.
type Live = BTreeSet<LocalId>;

struct Perceus<'p> {
    pool: &'p ResolvedPool,
    locals: &'p TiVec<LocalId, RTy>,
    /// The binds pinned to a global slot, which this function never owns.
    pinned: BTreeSet<LocalId>,
    /// The set of locals live on entry to each `LetCont`'s continuation. It is
    /// recorded when the `LetCont` is peeled, which is before its body, where
    /// every `Goto` to it is. So a `Goto` can always look its target up here.
    cont_live: BTreeMap<JoinId, Live>,
    /// The locals whose type is a variable and whose value holds nothing.
    nothing: BTreeSet<LocalId>,
    /// The locals that are a constructor with no fields in the arm being walked,
    /// because its pattern says so.
    plain: BTreeSet<LocalId>,
    words: Words,
    /// Whether a value got no drop so far because it can be no cell.
    left_bare: bool,
    /// Whether a value that may be a handle, or hold one, got no drop so far.
    undropped: bool,
}

/// What becomes of a local's value where nothing reads it any more.
enum Dead {
    /// It gets a drop.
    Drops,
    /// It needs none, because it can be no cell.
    Bare,
    /// It needs none: it holds nothing, or something else goes on holding it.
    Nothing,
    /// It needs one and gets none, because its type does not say what it is.
    Left,
}

/// One `Let` or `LetJoin` peeled off the spine. It waits for its body's live
/// set to decide which drops go between the right-hand side and the body.
struct SpineLet {
    bind: CoreBind,
    rhs_live: Live,
    rhs: SpineRhs,
}

/// One node peeled off the spine by [`Perceus::drop_pass`].
enum SpineFrame {
    Let(SpineLet),
    /// A `LetCont` whose continuation got its drops when it was peeled. It is
    /// rebuilt around the body with no drops of its own, because the
    /// continuation's live locals reach the enclosing live set through the
    /// `Goto`s to it.
    Cont {
        id: JoinId,
        cont: CoreExpr,
    },
}

/// The right-hand side of a peeled [`SpineLet`].
enum SpineRhs {
    Atom(Atom),
    /// A `LetJoin`'s right-hand side is a whole `CoreExpr`, which liveness
    /// treats as a black box. `rhs_live` is `join_live(join)`, and no drops
    /// are inserted inside it. So a heap temporary made inside a join is held
    /// until the enclosing frame ends. This is still sound, because the VM
    /// releases every register a frame still holds when the frame returns.
    Join(Box<CoreExpr>),
}

impl<'p> Perceus<'p> {
    fn new(pool: &'p ResolvedPool, locals: &'p TiVec<LocalId, RTy>, words: Words) -> Self {
        Perceus {
            pool,
            locals,
            pinned: BTreeSet::new(),
            cont_live: BTreeMap::new(),
            nothing: BTreeSet::new(),
            plain: BTreeSet::new(),
            words,
            left_bare: false,
            undropped: false,
        }
    }

    fn function(&mut self, f: CoreFn) -> Result<CoreFn, InternalLinkError> {
        let CoreFn {
            params,
            body,
            ret_ty,
        } = f;
        for p in &params {
            self.record_bind(p);
        }
        let (body, live) = self.drop_pass(body)?;
        // Params are owned on entry. Any the body doesn't read drop at its head,
        // so the frame still releases everything it owns.
        let dead_params: Vec<LocalId> = params
            .iter()
            .map(|p| p.id)
            .filter(|id| !live.contains(id))
            .collect();
        let body = self.wrap_drops(&dead_params, body)?;
        Ok(CoreFn {
            params,
            body,
            ret_ty,
        })
    }

    fn record_bind(&mut self, b: &CoreBind) {
        if b.global.is_some() {
            self.pinned.insert(b.id);
        }
    }

    /// What becomes of the local's value where it dies. A pinned global is owned
    /// by the program, so this function never releases it.
    fn dead(&self, id: LocalId) -> Result<Dead, InternalLinkError> {
        let ty = *self
            .locals
            .get(id)
            .ok_or(InternalLinkError::UntypedLocal(id))?;
        Ok(if self.pinned.contains(&id) {
            Dead::Nothing
        } else if self.pool.is_heap(ty)? {
            let bare = self.pool.repr(ty)? == Repr::Word || self.plain.contains(&id);
            match self.words {
                Words::Bare if bare => Dead::Bare,
                Words::Bare | Words::Dropped => Dead::Drops,
            }
        } else if self.pool.prim_of(ty)?.is_some() || self.nothing.contains(&id) {
            Dead::Nothing
        } else {
            Dead::Left
        })
    }

    /// Note the bind of a peeled `Let`.
    fn record_let(&mut self, b: &CoreBind, rhs: &Atom) -> Result<(), InternalLinkError> {
        self.record_bind(b);
        if holds_nothing(rhs) && !self.pool.is_heap(b.ty)? && self.pool.prim_of(b.ty)?.is_none() {
            self.nothing.insert(b.id);
        }
        Ok(())
    }

    /// Note that nothing bound inside `join` gets a drop there.
    fn leave_inside(&mut self, join: &CoreExpr) -> Result<(), InternalLinkError> {
        let mut stack = vec![join];
        while let Some(e) = stack.pop()
            && !self.undropped
        {
            match e {
                CoreExpr::Let { bind, rhs, body } => {
                    self.undropped |= !holds_nothing(rhs) && self.may_hold(bind)?;
                    stack.push(body);
                }
                CoreExpr::LetJoin { bind, join, body } => {
                    self.undropped |= self.may_hold(bind)?;
                    stack.push(join);
                    stack.push(body);
                }
                CoreExpr::LetCont { cont, body, .. } => {
                    stack.push(cont);
                    stack.push(body);
                }
                CoreExpr::Drop { body, .. } => stack.push(body),
                CoreExpr::Match { arms, .. } => {
                    for (pat, arm) in arms {
                        for b in pat.binds() {
                            self.undropped |= self.may_hold(b)?;
                        }
                        stack.push(arm);
                    }
                }
                CoreExpr::If { then, els, .. } => {
                    stack.push(then);
                    stack.push(els);
                }
                CoreExpr::Tail(_) | CoreExpr::Goto(_) => {}
            }
        }
        Ok(())
    }

    /// Whether a value of `b`'s type may be a handle or hold one. One that is no
    /// cell holds nothing, and neither does a number or a string.
    fn may_hold(&self, b: &CoreBind) -> Result<bool, InternalLinkError> {
        Ok(self.pool.repr(b.ty)? == Repr::Counted && self.pool.prim_of(b.ty)?.is_none())
    }

    /// Transform `e` and return `(e', live)`, where `live` is the locals from
    /// outer scopes that `e'` reads, which the caller is responsible for.
    /// Drops are inserted for locals that die inside `e`. The spine is walked
    /// in a loop, so a long straight-line body doesn't recurse on the Rust
    /// stack.
    fn drop_pass(&mut self, mut e: CoreExpr) -> Result<(CoreExpr, Live), InternalLinkError> {
        let mut spine: Vec<SpineFrame> = Vec::new();
        let (mut body, mut live) = loop {
            match e {
                CoreExpr::Let { bind, rhs, body } => {
                    let rhs_live = atom_live(&rhs);
                    self.record_let(&bind, &rhs)?;
                    spine.push(SpineFrame::Let(SpineLet {
                        bind,
                        rhs_live,
                        rhs: SpineRhs::Atom(rhs),
                    }));
                    e = *body;
                }
                CoreExpr::LetJoin { bind, join, body } => {
                    let rhs_live = join_live(&join);
                    self.record_bind(&bind);
                    self.leave_inside(&join)?;
                    spine.push(SpineFrame::Let(SpineLet {
                        bind,
                        rhs_live,
                        rhs: SpineRhs::Join(join),
                    }));
                    e = *body;
                }
                CoreExpr::LetCont { id, cont, body } => {
                    // Transform this before the body, so its live set is known at
                    // every `Goto(id)` the body contains.
                    let (cont, cont_live) = self.drop_pass(*cont)?;
                    self.cont_live.insert(id, cont_live);
                    spine.push(SpineFrame::Cont { id, cont });
                    e = *body;
                }
                CoreExpr::Drop { body, .. } => {
                    // Remove existing drops and work them out again, so the
                    // pass is safe to run twice.
                    e = *body;
                }
                // A tail call gives up every reference its frame still holds
                // once it has read its arguments, so its arguments need no
                // `Drop` here. One here would come before the read.
                CoreExpr::Tail(a) => {
                    let live = atom_live(&a);
                    break (CoreExpr::Tail(a), live);
                }
                CoreExpr::Goto(id) => {
                    // The edge hands the continuation ownership of exactly the
                    // locals live on its entry. Treating the `Goto` as an end
                    // that reads that set makes the backward pass drop
                    // everything else before the jump. So no local is released
                    // both before an edge and inside the continuation, or on
                    // neither.
                    let live = self
                        .cont_live
                        .get(&id)
                        .cloned()
                        .ok_or(InternalLinkError::UnboundJoin(id))?;
                    break (CoreExpr::Goto(id), live);
                }
                CoreExpr::If {
                    cond,
                    then,
                    els,
                    ty,
                } => {
                    let (then, live_t) = self.drop_pass(*then)?;
                    let (els, live_e) = self.drop_pass(*els)?;
                    // Both branches must release the same set, so the branch
                    // that does not need a local drops it at entry.
                    let then = self.wrap_drops(&set_diff(&live_e, &live_t), then)?;
                    let els = self.wrap_drops(&set_diff(&live_t, &live_e), els)?;
                    let mut live: Live = &live_t | &live_e;
                    live.insert(cond);
                    break (
                        CoreExpr::If {
                            cond,
                            then: Box::new(then),
                            els: Box::new(els),
                            ty,
                        },
                        live,
                    );
                }
                CoreExpr::Match { scrut, arms, ty } => {
                    break self.drop_match(scrut, arms, ty)?;
                }
            }
        };
        while let Some(frame) = spine.pop() {
            let frame = match frame {
                SpineFrame::Let(frame) => frame,
                SpineFrame::Cont { id, cont } => {
                    body = CoreExpr::LetCont {
                        id,
                        cont: Box::new(cont),
                        body: Box::new(body),
                    };
                    continue;
                }
            };
            // These die here: operands whose last read is this right-hand side,
            // plus the bind itself if the body never reads it. They drop
            // between the right-hand side and the body, as early as Perceus
            // allows.
            let mut dead: Vec<LocalId> = frame
                .rhs_live
                .iter()
                .copied()
                .filter(|x| !live.contains(x))
                .collect();
            if !live.contains(&frame.bind.id) {
                dead.push(frame.bind.id);
            }
            body = self.wrap_drops(&dead, body)?;
            live.remove(&frame.bind.id);
            live.extend(frame.rhs_live);
            body = match frame.rhs {
                SpineRhs::Atom(rhs) => CoreExpr::Let {
                    bind: frame.bind,
                    rhs,
                    body: Box::new(body),
                },
                SpineRhs::Join(join) => CoreExpr::LetJoin {
                    bind: frame.bind,
                    join,
                    body: Box::new(body),
                },
            };
        }
        Ok((body, live))
    }

    fn drop_match(
        &mut self,
        scrut: LocalId,
        arms: Vec<(CorePat, CoreExpr)>,
        ty: RTy,
    ) -> Result<(CoreExpr, Live), InternalLinkError> {
        struct Arm {
            pat: CorePat,
            body: CoreExpr,
            live: Live,
            dead_binders: Vec<LocalId>,
        }
        // The match reads `scrut`, and then each arm owns it. `scrut` is live
        // on entry to the Match, but the arms don't add it to `live_union`. It
        // drops at the head of each arm unless that arm reads it again.
        let mut live_union = Live::new();
        let mut lowered: Vec<Arm> = Vec::with_capacity(arms.len());
        for (pat, body) in arms {
            pat.binds().for_each(|b| self.record_bind(b));
            let (body, mut live) = self.in_arm(scrut, &pat, |cx| cx.drop_pass(body))?;
            let bound = || pat.binds().map(|b| b.id);
            let dead_binders = bound().filter(|b| !live.contains(b)).collect();
            for b in bound() {
                live.remove(&b);
            }
            live_union.extend(live.iter().copied());
            lowered.push(Arm {
                pat,
                body,
                live,
                dead_binders,
            });
        }
        let scrut_live_after = live_union.contains(&scrut);
        let arms = lowered
            .into_iter()
            .map(|a| {
                let mut drops = set_diff(&live_union, &a.live);
                if !scrut_live_after && !a.live.contains(&scrut) {
                    drops.push(scrut);
                }
                drops.extend(a.dead_binders);
                let body = self.in_arm(scrut, &a.pat, |cx| cx.wrap_drops(&drops, a.body))?;
                Ok::<_, InternalLinkError>((a.pat, body))
            })
            .collect::<Result<_, _>>()?;
        let mut live = live_union;
        live.insert(scrut);
        Ok((CoreExpr::Match { scrut, arms, ty }, live))
    }

    /// Run `f` in an arm for values of `scrut` that fit `pat`.
    fn in_arm<T>(&mut self, scrut: LocalId, pat: &CorePat, f: impl FnOnce(&mut Self) -> T) -> T {
        let plain = matches!(pat, CorePat::Ctor { fields, .. } if fields.is_empty());
        let first = plain && self.plain.insert(scrut);
        let out = f(self);
        if first {
            self.plain.remove(&scrut);
        }
        out
    }

    /// Put `Drop{x}` in front of `body` for each `x` in `dead` that gets one,
    /// going in reverse so the printed order matches `dead`.
    fn wrap_drops(
        &mut self,
        dead: &[LocalId],
        mut body: CoreExpr,
    ) -> Result<CoreExpr, InternalLinkError> {
        for &x in dead.iter().rev() {
            match self.dead(x)? {
                Dead::Drops => {
                    body = CoreExpr::Drop {
                        local: x,
                        body: Box::new(body),
                    };
                }
                Dead::Bare => self.left_bare = true,
                Dead::Nothing => {}
                Dead::Left => self.undropped = true,
            }
        }
        Ok(body)
    }
}

/// The free locals of an atom. A repeated operand, as in `add(x, x)`, is
/// counted once. The VM takes a reference for each operand it passes on, so
/// the count stays right.
fn atom_live(a: &Atom) -> Live {
    let mut live = Live::new();
    a.for_each_operand(|id| {
        live.insert(id);
    });
    live
}

/// The free locals of a `LetJoin`'s right-hand side, so `drop_pass` can treat
/// the join as a black box for liveness. Drops go around a join, never inside
/// it.
pub(crate) fn join_live(e: &CoreExpr) -> Live {
    /// The spine is walked in a loop, so recursion only goes as deep as the
    /// branches nest, never as long as the spine. Ids bound on this spine
    /// collect in `scope` and are removed before returning, so a sibling
    /// branch never sees them.
    fn go(mut e: &CoreExpr, live: &mut Live, bound: &mut Live) {
        let mut scope: Vec<LocalId> = Vec::new();
        loop {
            match e {
                CoreExpr::Let { bind, rhs, body } => {
                    for x in atom_live(rhs) {
                        if !bound.contains(&x) {
                            live.insert(x);
                        }
                    }
                    if bound.insert(bind.id) {
                        scope.push(bind.id);
                    }
                    e = body;
                }
                CoreExpr::LetJoin { bind, join, body } => {
                    go(join, live, bound);
                    if bound.insert(bind.id) {
                        scope.push(bind.id);
                    }
                    e = body;
                }
                CoreExpr::LetCont { cont, body, .. } => {
                    // This is collected at the declaration, so a `Goto` adds
                    // nothing.
                    go(cont, live, bound);
                    e = body;
                }
                CoreExpr::Drop { body, .. } => e = body,
                CoreExpr::Tail(a) => {
                    for x in atom_live(a) {
                        if !bound.contains(&x) {
                            live.insert(x);
                        }
                    }
                    break;
                }
                CoreExpr::Goto(_) => break,
                CoreExpr::If {
                    cond, then, els, ..
                } => {
                    if !bound.contains(cond) {
                        live.insert(*cond);
                    }
                    go(then, live, bound);
                    go(els, live, bound);
                    break;
                }
                CoreExpr::Match { scrut, arms, .. } => {
                    if !bound.contains(scrut) {
                        live.insert(*scrut);
                    }
                    for (pat, body) in arms {
                        let mut intro: Vec<LocalId> = Vec::new();
                        for b in pat.binds() {
                            if bound.insert(b.id) {
                                intro.push(b.id);
                            }
                        }
                        go(body, live, bound);
                        for &b in &intro {
                            bound.remove(&b);
                        }
                    }
                    break;
                }
            }
        }
        for id in scope {
            bound.remove(&id);
        }
    }
    let mut live = Live::new();
    go(e, &mut live, &mut Live::new());
    live
}

/// `a \ b` as a sorted vec.
fn set_diff(a: &Live, b: &Live) -> Vec<LocalId> {
    a.difference(b).copied().collect()
}

#[cfg(test)]
mod tests;
