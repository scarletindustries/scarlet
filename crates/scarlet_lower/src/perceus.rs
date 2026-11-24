//! Perceus is the pass that adds reference counting to Core IR. It decides
//! where each heap value is released, and lets a constructor reuse the memory
//! of a value that was just released. This is the frame-limited, drop-guided
//! algorithm of Lorenzen & Leijen (ICFP'22, Fig. 5), run as a pass from Core
//! IR to Core IR. In ANF, finding a value's last use is one backward scan.
//!
//!  1. A backward liveness walk inserts a `Drop` after each heap bind's last
//!     read. Where `If` and `Match` branches join, ownership is evened out so
//!     every path releases every owned slot exactly once (Fig. 5's Δ-rule).
//!  2. A forward walk pairs each `Drop` token with a later `Ctor` of the same
//!     shape, using a last-in-first-out stack that is copied at branches.
//!  3. The reuse walk runs a second time, starting from the tokens every
//!     back edge of the first run had, so a loop written as a tail call to
//!     itself can reuse the previous turn's cells.

use std::collections::{BTreeMap, BTreeSet};

use super::{Atom, Callee, CoreBind, CoreExpr, CoreFn, CorePat, JoinId, LocalId, ReuseShape};
use crate::program::InternalLinkError;
use crate::typed_ir::{RTy, ResolvedPool};
use scarlet_ir::tivec::TiVec;

/// Run Perceus on one lowered function. `locals` is the type of every local
/// `lower` made for it, and `pool` says which of those types live on the heap.
/// Its types are always resolved, so no unsolved variable can get here and
/// make `is_heap` answer `false` by accident.
///
/// A bind pinned to a global slot is never released or reused here, because
/// function bodies read the global for as long as the program runs, long after
/// the toplevel's last read of it.
pub(crate) fn perceus(
    pool: &ResolvedPool,
    locals: &TiVec<LocalId, RTy>,
    f: CoreFn,
) -> Result<CoreFn, InternalLinkError> {
    let CoreFn {
        params,
        body,
        ret_ty,
    } = f;
    let mut cx = Perceus::new(pool, locals);
    for p in &params {
        cx.record_bind(p, None);
    }
    let (body, live) = cx.drop_pass(body)?;
    // Params are owned on entry. Any the body doesn't read drop at its head,
    // so the frame still releases everything it owns.
    let dead_params: Vec<LocalId> = params
        .iter()
        .map(|p| p.id)
        .filter(|id| !live.contains(id))
        .collect();
    let body = cx.wrap_drops(&dead_params, None, body)?;
    Ok(CoreFn {
        params,
        body: reuse_pass(body),
        ret_ty,
    })
}

/// The locals live at a program point. It's a `BTreeSet` so the order drops
/// are inserted in, and so the golden snapshots, never changes between runs.
type Live = BTreeSet<LocalId>;

/// A released cell that a later `Ctor` of the same shape can overwrite.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Token {
    slot: LocalId,
    shape: ReuseShape,
    /// Carried over from a previous turn of a loop by
    /// [`ReuseWalk::back_edge_seed`]. Its slot may be bound inside any branch,
    /// so the only place sure to have it in scope is the `Let` that binds the
    /// same id. So a carried token only pairs with its own slot, never through
    /// the fallback that matches by shape.
    carried: bool,
}

struct Perceus<'p> {
    pool: &'p ResolvedPool,
    locals: &'p TiVec<LocalId, RTy>,
    /// The binds pinned to a global slot, which this function never owns.
    pinned: BTreeSet<LocalId>,
    /// A bind's allocation shape, when its right-hand side proves one. Locals
    /// with no entry drop with `shape: None` and are never offered for reuse.
    shape: BTreeMap<LocalId, ReuseShape>,
    /// The set of locals live on entry to each `LetCont`'s continuation. It is
    /// recorded when the `LetCont` is peeled, which is before its body, where
    /// every `Goto` to it is. So a `Goto` can always look its target up here.
    cont_live: BTreeMap<JoinId, Live>,
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
    /// until the enclosing frame ends and never becomes a reuse token. This is
    /// still sound, because the VM releases every register a frame still
    /// holds when the frame returns.
    Join(Box<CoreExpr>),
}

impl<'p> Perceus<'p> {
    fn new(pool: &'p ResolvedPool, locals: &'p TiVec<LocalId, RTy>) -> Self {
        Perceus {
            pool,
            locals,
            pinned: BTreeSet::new(),
            shape: BTreeMap::new(),
            cont_live: BTreeMap::new(),
        }
    }

    fn record_bind(&mut self, b: &CoreBind, rhs_shape: Option<ReuseShape>) {
        if b.global.is_some() {
            self.pinned.insert(b.id);
        }
        if let Some(s) = rhs_shape {
            self.shape.insert(b.id, s);
        }
    }

    /// Whether this function owns a heap cell through the local, so must
    /// release it. A pinned global is owned by the program instead.
    fn owns_cell(&self, id: LocalId) -> Result<bool, InternalLinkError> {
        let ty = self
            .locals
            .get(id)
            .ok_or(InternalLinkError::UntypedLocal(id))?;
        Ok(self.pool.is_heap(*ty)? && !self.pinned.contains(&id))
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
                    self.record_bind(&bind, ctor_shape(&rhs));
                    spine.push(SpineFrame::Let(SpineLet {
                        bind,
                        rhs_live,
                        rhs: SpineRhs::Atom(rhs),
                    }));
                    e = *body;
                }
                CoreExpr::LetJoin { bind, join, body } => {
                    let rhs_live = join_live(&join);
                    self.record_bind(&bind, None);
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
                    let then = self.wrap_drops(&set_diff(&live_e, &live_t), None, then)?;
                    let els = self.wrap_drops(&set_diff(&live_t, &live_e), None, els)?;
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
            // allows, so their cells are free for reuse as soon as possible.
            let mut dead: Vec<LocalId> = frame
                .rhs_live
                .iter()
                .copied()
                .filter(|x| !live.contains(x))
                .collect();
            if !live.contains(&frame.bind.id) {
                dead.push(frame.bind.id);
            }
            body = self.wrap_drops(&dead, None, body)?;
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
            scrut_shape: Option<ReuseShape>,
        }
        // The match reads `scrut`, and then each arm owns it. `scrut` is live
        // on entry to the Match, but the arms don't add it to `live_union`. It
        // drops at the head of each arm unless that arm reads it again.
        let mut live_union = Live::new();
        let mut lowered: Vec<Arm> = Vec::with_capacity(arms.len());
        for (pat, body) in arms {
            let (bound, scrut_shape) = self.record_pat(&pat, scrut);
            let (body, mut live) = self.drop_pass(body)?;
            let dead_binders: Vec<LocalId> = bound
                .iter()
                .copied()
                .filter(|b| !live.contains(b))
                .collect();
            for b in &bound {
                live.remove(b);
            }
            live_union.extend(live.iter().copied());
            lowered.push(Arm {
                pat,
                body,
                live,
                dead_binders,
                scrut_shape,
            });
        }
        let scrut_live_after = live_union.contains(&scrut);
        let arms = lowered
            .into_iter()
            .map(|a| {
                // When the pattern proved the variant, the scrutinee's drop
                // carries that arity, so the reuse walk sees a sized token even
                // for a multi-variant type.
                let mut drops = set_diff(&live_union, &a.live);
                if !scrut_live_after && !a.live.contains(&scrut) {
                    drops.push(scrut);
                }
                drops.extend(a.dead_binders);
                let body = self.wrap_drops(&drops, a.scrut_shape.map(|s| (scrut, s)), a.body)?;
                Ok::<_, InternalLinkError>((a.pat, body))
            })
            .collect::<Result<_, _>>()?;
        let mut live = live_union;
        live.insert(scrut);
        Ok((CoreExpr::Match { scrut, arms, ty }, live))
    }

    /// Record the locals a pattern binds, and return the scrutinee's shape. A
    /// `Ctor` arm proves an arity, and other patterns pass on what it had.
    fn record_pat(&mut self, pat: &CorePat, scrut: LocalId) -> (Vec<LocalId>, Option<ReuseShape>) {
        match pat {
            CorePat::Wild | CorePat::Lit(_) => (Vec::new(), self.shape.get(&scrut).copied()),
            CorePat::Bind(b) => {
                let s = self.shape.get(&scrut).copied();
                self.record_bind(b, s);
                (vec![b.id], s)
            }
            CorePat::Ctor { fields, .. } => {
                for fb in fields {
                    self.record_bind(fb, None);
                }
                (
                    fields.iter().map(|b| b.id).collect(),
                    ReuseShape::ctor(fields.len()),
                )
            }
        }
    }

    /// Put `Drop{x}` in front of `body` for each `x` in `dead` that owns a
    /// cell, going in reverse so the printed order matches `dead`. Locals that
    /// aren't on the heap are skipped, because dropping an unboxed primitive
    /// does nothing but still costs an instruction. `arm_scrut` overrides the
    /// recorded shape for one id.
    fn wrap_drops(
        &mut self,
        dead: &[LocalId],
        arm_scrut: Option<(LocalId, ReuseShape)>,
        mut body: CoreExpr,
    ) -> Result<CoreExpr, InternalLinkError> {
        for &x in dead.iter().rev() {
            if !self.owns_cell(x)? {
                continue;
            }
            let shape = match arm_scrut {
                Some((s, sh)) if s == x => Some(sh),
                _ => self.shape.get(&x).copied(),
            };
            body = CoreExpr::Drop {
                local: x,
                shape,
                body: Box::new(body),
            };
        }
        Ok(body)
    }
}

/// The state of one function's reuse walk. Each [`reuse_pass`] builds a new
/// one, so a walk never sees another function's back-edge token sets.
struct ReuseWalk {
    /// The token sets that reach each `Tail(Call{Self_})` in the current walk.
    self_tails: Vec<Vec<Token>>,
}

/// Frame-limited reuse over the body once it has its drops. A second walk
/// handles tokens carried round a loop, for a body that tail-calls itself.
/// Each walk decides every constructor's `reuse`, so the body holds the last
/// walk's plan whole, never a mix of the two.
fn reuse_pass(mut body: CoreExpr) -> CoreExpr {
    let mut walk = ReuseWalk {
        self_tails: Vec::new(),
    };
    walk.pair(&mut body, &mut Vec::new());
    // Only tokens present at *every* tail call to self are sure to be there at
    // the top of the loop, so only those are safe to carry round it.
    if let Some(mut seed) = walk.back_edge_seed() {
        walk.pair(&mut body, &mut seed);
    }
    body
}

impl ReuseWalk {
    /// This takes `self_tails`, so a later walk can't mix in old snapshots.
    fn back_edge_seed(&mut self) -> Option<Vec<Token>> {
        let tails = std::mem::take(&mut self.self_tails);
        let first = tails.first()?;
        let seed: Vec<Token> = first
            .iter()
            .filter(|t| tails.iter().all(|ts| ts.contains(t)))
            .map(|t| Token {
                carried: true,
                ..*t
            })
            .collect();
        (!seed.is_empty()).then_some(seed)
    }

    /// Pair drops with constructors over `e`, last in first out. `avail` is the
    /// stack of released cells sure to exist at the current point. It is
    /// copied at branches, cleared before a `LetJoin`, and recorded at each
    /// tail call to self.
    fn pair(&mut self, mut e: &mut CoreExpr, avail: &mut Vec<Token>) {
        loop {
            match e {
                CoreExpr::Drop { local, shape, body } => {
                    // A cell with no fields has nothing to reuse, because the
                    // header is the whole allocation.
                    if let Some(s) = *shape
                        && s.fields > 0
                    {
                        avail.push(Token {
                            slot: *local,
                            shape: s,
                            carried: false,
                        });
                    }
                    e = body;
                }
                CoreExpr::Let { bind, rhs, body } => {
                    // A non-tail `Call` in `rhs` does NOT invalidate tokens,
                    // because the parked cell lives in this frame's slot and
                    // the callee can't touch it. Frame-limited (ICFP'22 §4)
                    // keeps reuse inside this frame, but calls made inside the
                    // frame don't block it.
                    if let Atom::Ctor { fields, reuse, .. } = rhs {
                        let want = ReuseShape::ctor(fields.len());
                        // Prefer this bind's own slot. This `Let` is about to
                        // overwrite it, so a token for that slot must be used
                        // here or be thrown away by the retain below.
                        let i = avail
                            .iter()
                            .rposition(|t| t.slot == bind.id && Some(t.shape) == want)
                            .or_else(|| {
                                avail
                                    .iter()
                                    .rposition(|t| !t.carried && Some(t.shape) == want)
                            });
                        *reuse = i.map(|i| avail.remove(i).slot);
                    }
                    // The slot now holds a live value, and a token for it
                    // would read that instead of a released cell.
                    avail.retain(|t| t.slot != bind.id);
                    e = body;
                }
                CoreExpr::LetJoin { body, .. } => {
                    // A join may contain a `Call` on some path, so clear the
                    // stack instead of walking into it.
                    avail.clear();
                    e = body;
                }
                CoreExpr::LetCont { cont, body, .. } => {
                    // A shared continuation is entered from many `Goto` edges,
                    // and a token parked on one edge may be a live slot on
                    // another, so it starts with an empty stack. Declaring a
                    // continuation runs nothing, so the body's `avail` is left
                    // alone.
                    self.pair(cont, &mut Vec::new());
                    e = body;
                }
                CoreExpr::If { then, els, .. } => {
                    let mut a2 = avail.clone();
                    self.pair(then, avail);
                    self.pair(els, &mut a2);
                    return;
                }
                CoreExpr::Match { arms, .. } => {
                    for (_, body) in arms {
                        self.pair(body, &mut avail.clone());
                    }
                    return;
                }
                CoreExpr::Tail(a) => {
                    match a {
                        Atom::Ctor { fields, reuse, .. } => {
                            let want = ReuseShape::ctor(fields.len());
                            let i = avail
                                .iter()
                                .rposition(|t| !t.carried && Some(t.shape) == want);
                            *reuse = i.map(|i| avail.remove(i).slot);
                        }
                        Atom::Call {
                            callee: Callee::Self_,
                            ..
                        } => {
                            self.self_tails.push(avail.clone());
                        }
                        // This only looks for tail calls to self. Every other
                        // atom, now or later, is not one.
                        #[allow(unknown_lints, wildcard_over_own_enum)]
                        _ => {}
                    }
                    return;
                }
                // Tokens die at the edge, because the target continuation
                // starts empty.
                CoreExpr::Goto(_) => return,
            }
        }
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
fn join_live(e: &CoreExpr) -> Live {
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

/// The known allocation shape of a `Let`'s right-hand side. Only a `Ctor`
/// proves a shape, because the arity of a `Call` or `PrimOp` result isn't
/// known at compile time.
fn ctor_shape(a: &Atom) -> Option<ReuseShape> {
    match a {
        Atom::Ctor { fields, .. } => ReuseShape::ctor(fields.len()),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
