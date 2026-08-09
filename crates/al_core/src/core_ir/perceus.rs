//! Perceus reference-count insertion on Core IR: the frame-limited
//! drop-guided algorithm of Lorenzen & Leijen (ICFP'22, Fig. 5) as a Core→Core
//! pass. ANF makes last-use a linear backward scan.
//!
//!  1. Backward liveness inserts `Drop` after each heap bind's last read.
//!     `If`/`Match` joins equalise ownership so every path releases every owned
//!     slot exactly once (Fig. 5's Δ-rule).
//!  2. A forward walk pairs each `Drop` token with a later same-shape `Ctor`
//!     through a LIFO stack, forked at branches.
//!  3. The reuse walk runs a second time seeded with the intersection of the
//!     first run's back-edge token sets, so a tail-self-recursive loop can
//!     reuse the previous iteration's cells.

use std::collections::{BTreeMap, BTreeSet};

use super::{Atom, Callee, CoreBind, CoreExpr, CoreFn, CorePat, JoinId, LocalId, ReuseShape};
use crate::typed_ir::{RTy, ResolvedPool};

/// Run Perceus on a single lowered function. `pool` is consulted only to
/// classify a bind's `ty` as heap-shaped and to read a tuple's width. Its types
/// are resolved by construction, so no unsolved variable can reach here and
/// make `is_heap` answer `false` by accident.
pub fn perceus(pool: &ResolvedPool, f: CoreFn) -> CoreFn {
    let CoreFn {
        name,
        params,
        body,
        ret_ty,
    } = f;
    let mut cx = Perceus::new(pool);
    for p in &params {
        cx.record_bind(p, None);
    }
    let (body, live) = cx.drop_pass(body);
    // Params are owned on entry; any not read by the body drop at its head so
    // the frame's release count still balances.
    let dead_params: Vec<LocalId> = params
        .iter()
        .map(|p| p.id)
        .filter(|id| !live.contains(id))
        .collect();
    let body = cx.wrap_drops(&dead_params, None, body);
    CoreFn {
        name,
        params,
        body: reuse_pass(body),
        ret_ty,
    }
}

/// Locals live at a program point. `BTreeSet` so drop insertion order, and
/// thus golden snapshots, is deterministic.
type Live = BTreeSet<LocalId>;

/// A hollowed cell available for a downstream same-shape `Ctor` to overwrite.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Token {
    slot: LocalId,
    shape: ReuseShape,
    /// Loop-carried from a previous iteration via [`ReuseWalk::back_edge_seed`].
    /// Its slot may be bound inside an arbitrary branch, so the only site sure
    /// to have it in scope is the `Let` binding that same id. Carried tokens
    /// therefore pair by self-slot only, never the LIFO shape fallback.
    carried: bool,
}

struct Perceus<'p> {
    pool: &'p ResolvedPool,
    /// Every bind's resolved type, for the heap-shape gate on `Drop`.
    ty: BTreeMap<LocalId, RTy>,
    /// Allocation shape of a bind when its rhs proves one. Locals with no entry
    /// drop with `shape: None` and are never offered for reuse.
    shape: BTreeMap<LocalId, ReuseShape>,
    /// Live-in set of each `LetCont`'s continuation, recorded when the
    /// `LetCont` is peeled — before its body, which holds every `Goto` to it —
    /// so a `Goto` can always look its target up here.
    cont_live: BTreeMap<JoinId, Live>,
}

/// One `Let`/`LetJoin` peeled off the spine, awaiting its body's live set to
/// decide which drops go between rhs and body.
struct SpineLet {
    bind: CoreBind,
    rhs_live: Live,
    rhs: SpineRhs,
}

/// One node peeled off the spine by [`Perceus::drop_pass`].
enum SpineFrame {
    Let(SpineLet),
    /// A `LetCont` whose continuation was drop-processed at peel time. It is
    /// rebuilt around the body with no drops of its own: the continuation's
    /// live-ins reach the enclosing live set through the `Goto`s to it.
    Cont {
        id: JoinId,
        cont: CoreExpr,
    },
}

/// The rhs of a peeled [`SpineLet`].
enum SpineRhs {
    Atom(Atom),
    /// A `LetJoin`'s rhs is a whole `CoreExpr`, treated opaquely for liveness:
    /// `rhs_live` is `join_live(join)` and no drops are inserted inside it.
    /// A heap temp born inside a join is therefore held to the end of the
    /// enclosing frame and never becomes a reuse token. Still sound: the VM
    /// clones on `PushLocal` and releases on `StoreLocal`/frame teardown.
    Join(Box<CoreExpr>),
}

impl<'p> Perceus<'p> {
    fn new(pool: &'p ResolvedPool) -> Self {
        Perceus {
            pool,
            ty: BTreeMap::new(),
            shape: BTreeMap::new(),
            cont_live: BTreeMap::new(),
        }
    }

    fn record_bind(&mut self, b: &CoreBind, rhs_shape: Option<ReuseShape>) {
        self.ty.insert(b.id, b.ty);
        if let Some(s) = rhs_shape {
            self.shape.insert(b.id, s);
        }
    }

    /// Whether the local's type allocates a Perceus-managed heap cell. A local
    /// with no recorded type was never bound by this pass, so it owns nothing.
    fn is_heap_local(&self, id: LocalId) -> bool {
        self.ty.get(&id).is_some_and(|&t| self.pool.is_heap(t))
    }

    /// Transform `e`, returning `(e', live)` where `live` is the outer-scope
    /// locals `e'` reads and so the caller's responsibility. Drops are inserted
    /// for locals that go dead inside `e`. The spine is walked iteratively so a
    /// long straight-line body does not recurse on the Rust stack.
    fn drop_pass(&mut self, mut e: CoreExpr) -> (CoreExpr, Live) {
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
                    // Transformed before the body so its live-in set is known at
                    // every `Goto(id)` the body contains.
                    let (cont, cont_live) = self.drop_pass(*cont);
                    self.cont_live.insert(id, cont_live);
                    spine.push(SpineFrame::Cont { id, cont });
                    e = *body;
                }
                CoreExpr::Drop { body, .. } => {
                    // Strip and re-derive, so the pass is safe to rerun.
                    e = *body;
                }
                CoreExpr::Tail(a) => {
                    let live = atom_live(&a);
                    break (self.release_self_tail_args(a), live);
                }
                CoreExpr::Goto(id) => {
                    // The edge hands the cont ownership of exactly its live-in
                    // set. Treating the `Goto` as a terminal reading that set
                    // makes the backward pass drop everything else before the
                    // jump, so no local is released both before an edge and
                    // inside the cont, or on neither.
                    let live = self
                        .cont_live
                        .get(&id)
                        .cloned()
                        .unwrap_or_else(|| unscoped_goto(id));
                    break (CoreExpr::Goto(id), live);
                }
                CoreExpr::If {
                    cond,
                    then,
                    els,
                    ty,
                } => {
                    let (then, live_t) = self.drop_pass(*then);
                    let (els, live_e) = self.drop_pass(*els);
                    // Both branches must release the same set, so the branch
                    // that does not need a local drops it at entry.
                    let then = self.wrap_drops(&set_diff(&live_e, &live_t), None, then);
                    let els = self.wrap_drops(&set_diff(&live_t, &live_e), None, els);
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
                    break self.drop_match(scrut, arms, ty);
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
            // Newly dead here: rhs operands whose last read is this one, plus
            // the bind itself if the body never reads it. They drop between rhs
            // and body, as early as Perceus permits, so reuse tokens are hot.
            let mut dead: Vec<LocalId> = frame
                .rhs_live
                .iter()
                .copied()
                .filter(|x| !live.contains(x))
                .collect();
            if !live.contains(&frame.bind.id) {
                dead.push(frame.bind.id);
            }
            body = self.wrap_drops(&dead, None, body);
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
        (body, live)
    }

    /// Release the argument slots of a self-tail-call.
    ///
    /// `TailCallSelf` keeps its frame: the VM's `collapse_tail_frame_self`
    /// swaps the new arguments into the parameter slots and leaves the
    /// remaining locals alone, because those slots *are* the per-frame reuse
    /// table (loop-carried `Drop`→`Reuse` pairing). Every other tail call
    /// drains `[base, args_start)`, which releases the caller's locals before
    /// the callee runs; the self-tail edge never does. So a heap argument that
    /// still sits in a local slot enters the next iteration at rc==2 and its
    /// `Drop` refuses to hollow it — the canonical
    /// `map xs f = match xs { Cons(h,t) -> Cons(f h, map t f) }` never reuses
    /// when a self-recursive driver threads the list through itself.
    ///
    /// Emit a `Drop` for each distinct heap argument immediately before the
    /// `Tail`; `emit` sinks them past the operand pushes (see
    /// `emit::split_self_tail_drops`), so the `PushLocal` dup keeps the value
    /// alive while the slot's own reference is released and the callee's
    /// parameter becomes the sole owner. `shape: None` keeps the reuse walk
    /// from parking a *live* cell as a token.
    fn release_self_tail_args(&mut self, a: Atom) -> CoreExpr {
        let Atom::Call {
            callee: Callee::Self_,
            args,
        } = &a
        else {
            return CoreExpr::Tail(a);
        };
        let mut moved: Vec<LocalId> = Vec::new();
        for &x in args {
            if !moved.contains(&x) && self.is_heap_local(x) {
                moved.push(x);
            }
        }
        let mut body = CoreExpr::Tail(a);
        for &x in moved.iter().rev() {
            body = CoreExpr::Drop {
                local: x,
                shape: None,
                body: Box::new(body),
            };
        }
        body
    }

    fn drop_match(
        &mut self,
        scrut: LocalId,
        arms: Vec<(CorePat, CoreExpr)>,
        ty: RTy,
    ) -> (CoreExpr, Live) {
        struct Arm {
            pat: CorePat,
            body: CoreExpr,
            live: Live,
            dead_binders: Vec<LocalId>,
            scrut_shape: Option<ReuseShape>,
        }
        // The match itself reads `scrut`; each arm then owns it (destructure
        // moves the fields out and the cell is dead). `scrut` is live-in to
        // the Match but is *not* propagated into `live_union` from the arms —
        // it drops at each arm head unless the arm body itself re-reads it.
        let mut live_union = Live::new();
        let mut lowered: Vec<Arm> = Vec::with_capacity(arms.len());
        for (pat, body) in arms {
            let (bound, scrut_shape) = self.record_pat(&pat, scrut);
            let (body, mut live) = self.drop_pass(body);
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
                // Outer locals live into some other arm but not this one drop
                // here; plus this arm's dead pattern binders; plus the
                // scrutinee if this arm doesn't re-read it. When the pattern
                // proved the variant, the scrutinee's drop carries that arity
                // so the reuse walk sees a sized token even for a
                // multi-variant type.
                let mut drops = set_diff(&live_union, &a.live);
                if !scrut_live_after && !a.live.contains(&scrut) {
                    drops.push(scrut);
                }
                drops.extend(a.dead_binders);
                let body = self.wrap_drops(&drops, a.scrut_shape.map(|s| (scrut, s)), a.body);
                (a.pat, body)
            })
            .collect();
        let mut live = live_union;
        live.insert(scrut);
        (CoreExpr::Match { scrut, arms, ty }, live)
    }

    /// Register the locals a pattern binds. For a `Ctor` arm, return the
    /// scrutinee's now-proven arity so its arm-head `Drop` can offer a
    /// correctly-sized reuse token; for `Bind`/`Wild`, propagate whatever
    /// shape the scrutinee already had.
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
                    Some(ReuseShape::enum_(fields.len())),
                )
            }
        }
    }

    /// Prefix `body` with `Drop{x}` for each heap-shaped `x` in `dead`,
    /// reverse-order so the textual sequence matches `dead`. Non-heap locals
    /// are skipped: `Op::Drop` on an unboxed prim is a no-op but still costs
    /// dispatch — the same over-emission that regressed Phase 2. `arm_scrut`
    /// overrides the recorded shape for one id (the match scrutinee inside a
    /// `Ctor` arm, whose arity is proven by the pattern rather than the type).
    fn wrap_drops(
        &mut self,
        dead: &[LocalId],
        arm_scrut: Option<(LocalId, ReuseShape)>,
        mut body: CoreExpr,
    ) -> CoreExpr {
        for &x in dead.iter().rev() {
            if !self.is_heap_local(x) {
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
        body
    }
}

// ── Phase 2: forward reuse pairing ──────────────────────────────────────────

/// State of one function's reuse walk. Constructed fresh inside
/// [`reuse_pass`], so a walk can never observe another function's back-edge
/// token sets.
struct ReuseWalk {
    /// Token sets reaching each `Tail(Call{Self_})` in the current walk.
    /// Consumed by [`reuse_pass`] to seed the loop-carried second walk.
    self_tails: Vec<Vec<Token>>,
}

/// Frame-limited reuse over the drop-annotated body, with a second seeded
/// walk for loop-carried tokens on tail-self-recursive bodies.
fn reuse_pass(mut body: CoreExpr) -> CoreExpr {
    let mut walk = ReuseWalk {
        self_tails: Vec::new(),
    };
    walk.pair(&mut body, &mut Vec::new());
    // Loop-carried: tokens present at *every* self-tail dominate the loop
    // head across `TailCallSelf`. Intersection guarantees the reuse slot
    // is either nil (first iter) or a hollowed cell (subsequent iters) at
    // every leading `Ctor` we pair — the runtime `into_reuse_addr`
    // debug-assert on rc==1 relies on that dominance.
    if let Some(mut seed) = walk.back_edge_seed() {
        walk.pair(&mut body, &mut seed);
    }
    body
}

impl ReuseWalk {
    /// Consumes `self_tails`, so a walk after the seeded second one starts
    /// accumulating snapshots from scratch instead of mixing in stale ones.
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

    /// LIFO reuse pairing over `e`. `avail` is the stack of hollowed cells
    /// dominating the current point; forked (cloned) at branches, cleared at
    /// non-tail calls, snapshotted at self-tails.
    fn pair(&mut self, mut e: &mut CoreExpr, avail: &mut Vec<Token>) {
        loop {
            match e {
                CoreExpr::Drop { local, shape, body } => {
                    // A 0-payload cell (e.g. a nullary variant) has nothing to
                    // reuse — the header is the whole allocation. Skipping it
                    // also keeps `LNil -> LNil` from emitting a pointless
                    // `Op::Reuse` before an arity-0 `MakeEnumPayload`.
                    if let Some(s) = *shape
                        && s.words > 0
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
                    // A non-tail `Call` in `rhs` does *not* invalidate tokens:
                    // the parked cell lives in this frame's slot, untouched by
                    // the callee. Frame-limited (ICFP'22 §4) constrains reuse
                    // to *this* frame; it does not fence intra-frame call
                    // sites — the paper's own `map` reuses across `f(h)`.
                    if let Atom::Ctor { fields, reuse, .. } = rhs {
                        let want = ReuseShape::enum_(fields.len());
                        // Prefer this bind's own slot: `StoreLocal` is about
                        // to overwrite it, so a same-slot token must be
                        // consumed here or discarded by the retain below.
                        // Only fall back to LIFO shape-match when no
                        // self-slot token exists. Re-derived on the seeded
                        // second walk (no `is_none` guard) so each token is
                        // claimed by exactly one Ctor.
                        let i = avail
                            .iter()
                            .rposition(|t| t.slot == bind.id && t.shape == want)
                            .or_else(|| avail.iter().rposition(|t| !t.carried && t.shape == want));
                        if let Some(i) = i {
                            *reuse = Some(avail.remove(i).slot);
                        }
                    }
                    // `StoreLocal bind.id` now holds a live value; any token
                    // for that slot would read it, not a hollowed cell.
                    avail.retain(|t| t.slot != bind.id);
                    e = body;
                }
                CoreExpr::LetJoin { body, .. } => {
                    // A join in operand position may contain a `Call` on some
                    // path; conservatively clear (frame-limited) rather than
                    // walk into it. The slot is overwritten by `StoreLocal`.
                    avail.clear();
                    e = body;
                }
                CoreExpr::LetCont { cont, body, .. } => {
                    // A shared cont is entered from many `Goto` edges: a
                    // token parked on one edge may be a live slot on another,
                    // so the cont starts with an EMPTY stack and pairs only
                    // tokens its own interior drops produce. (Seeding with
                    // the predecessor intersection is a later perf follow-up,
                    // deliberately absent.) The body keeps `avail` untouched:
                    // declaring a cont executes nothing.
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
                            let want = ReuseShape::enum_(fields.len());
                            if let Some(i) =
                                avail.iter().rposition(|t| !t.carried && t.shape == want)
                            {
                                *reuse = Some(avail.remove(i).slot);
                            }
                        }
                        Atom::Call {
                            callee: Callee::Self_,
                            ..
                        } => {
                            self.self_tails.push(avail.clone());
                        }
                        _ => {}
                    }
                    return;
                }
                // Tokens die at the edge: the target cont starts empty.
                CoreExpr::Goto(_) => return,
            }
        }
    }
}

/// Free locals of an atom as a set; a duplicate operand (`add(x, x)`)
/// collapses — the VM's `PushLocal` dup keeps RC correct.
fn atom_live(a: &Atom) -> Live {
    let mut live = Live::new();
    a.for_each_operand(|id| {
        live.insert(id);
    });
    live
}

/// Free locals of a `LetJoin` rhs (a whole `CoreExpr` in operand position).
/// Collected in a single forward walk so `drop_pass` can treat the join as an
/// opaque atom for liveness — Perceus does not currently insert drops *inside*
/// a join, only around it.
fn join_live(e: &CoreExpr) -> Live {
    /// The `Let`/`LetJoin`/`Drop` spine is walked iteratively (mirroring
    /// `drop_pass`'s `SpineLet` peel) so recursion depth is branch nesting,
    /// never spine length; `go` recurses only at `If`/`Match` arms and into a
    /// `LetJoin`'s join, whose nesting is operand depth. Ids bound on this
    /// spine collect in `scope` and pop before returning so a sibling branch
    /// never sees them.
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
                    // The cont's reads are collected at its declaration; a
                    // `Goto` to it then contributes nothing of its own.
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

/// A `Goto` whose join was never peeled as an enclosing `LetCont` is a
/// lowering bug. Proceeding with an empty live-in set would drop locals the
/// continuation still reads — a runtime double-free — so abort in release
/// too, like emit's `unbound_join`.
#[allow(clippy::panic)]
#[cold]
#[inline(never)]
fn unscoped_goto(id: JoinId) -> ! {
    panic!(
        "internal compiler error: perceus reached goto to join {id} with no \
         enclosing LetCont. Please report this as a compiler bug."
    )
}

/// Known allocation shape of a `Let`'s rhs. Only a `Ctor` rhs proves a shape;
/// `Call`/`PrimOp` results have no statically-known arity.
fn ctor_shape(a: &Atom) -> Option<ReuseShape> {
    match a {
        Atom::Ctor { fields, .. } => Some(ReuseShape::enum_(fields.len())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::Op;
    use crate::core_ir::testkit::{bind, ctor, func, local, variant};
    use crate::core_ir::{ConstId, FuncIdx};
    use crate::type_def::TypeId;
    use crate::types::{PrimIds, StrId};

    /// The fixtures build their types directly in a [`ResolvedPool`] — the
    /// same arena the elaborator hands `perceus` in the real pipeline. Under
    /// the old `InferEngine` these were `mk_con`/`icon_int` on live inference
    /// nodes; the answers `is_heap` gives are identical, and an
    /// unsolved variable is now unspellable rather than silently non-heap.
    fn pool() -> ResolvedPool {
        ResolvedPool::new(PrimIds::default())
    }

    /// A nominal, non-primitive type: heap-shaped, unknown allocation width.
    fn con(p: &mut ResolvedPool, id: i32) -> RTy {
        p.mk_con(TypeId(id), StrId::NONE, &[])
    }

    /// The unboxed `Int` — heap-shaped `false`, so no `Drop`.
    fn int_ty(p: &mut ResolvedPool) -> RTy {
        let int = p.prims().int;
        p.mk_con(int, StrId::NONE, &[])
    }

    fn count_drops(e: &CoreExpr) -> usize {
        match e {
            CoreExpr::Drop { body, .. } => 1 + count_drops(body),
            CoreExpr::Let { body, .. } => count_drops(body),
            CoreExpr::LetJoin { join, body, .. } => count_drops(join) + count_drops(body),
            CoreExpr::LetCont { cont, body, .. } => count_drops(cont) + count_drops(body),
            CoreExpr::If { then, els, .. } => count_drops(then) + count_drops(els),
            CoreExpr::Match { arms, .. } => arms.iter().map(|(_, b)| count_drops(b)).sum(),
            CoreExpr::Tail(_) | CoreExpr::Goto(_) => 0,
        }
    }

    fn ctor_reuses(e: &CoreExpr) -> Vec<Option<LocalId>> {
        fn go(e: &CoreExpr, out: &mut Vec<Option<LocalId>>) {
            match e {
                CoreExpr::Let { rhs, body, .. } => {
                    if let Atom::Ctor { reuse, .. } = rhs {
                        out.push(*reuse);
                    }
                    go(body, out);
                }
                CoreExpr::LetJoin { join, body, .. } => {
                    go(join, out);
                    go(body, out);
                }
                CoreExpr::LetCont { cont, body, .. } => {
                    go(cont, out);
                    go(body, out);
                }
                CoreExpr::Drop { body, .. } => go(body, out),
                CoreExpr::If { then, els, .. } => {
                    go(then, out);
                    go(els, out);
                }
                CoreExpr::Match { arms, .. } => {
                    for (_, b) in arms {
                        go(b, out);
                    }
                }
                CoreExpr::Tail(Atom::Ctor { reuse, .. }) => out.push(*reuse),
                CoreExpr::Tail(_) | CoreExpr::Goto(_) => {}
            }
        }
        let mut out = Vec::new();
        go(e, &mut out);
        out
    }

    fn find_drop(mut e: &CoreExpr, id: LocalId) -> bool {
        loop {
            match e {
                CoreExpr::Drop { local, .. } if *local == id => return true,
                CoreExpr::Drop { body, .. }
                | CoreExpr::Let { body, .. }
                | CoreExpr::LetJoin { body, .. } => e = body,
                CoreExpr::LetCont { cont, body, .. } => {
                    return find_drop(cont, id) || find_drop(body, id);
                }
                CoreExpr::Tail(_) | CoreExpr::Goto(_) => return false,
                CoreExpr::If { then, els, .. } => {
                    return find_drop(then, id) || find_drop(els, id);
                }
                CoreExpr::Match { arms, .. } => {
                    return arms.iter().any(|(_, b)| find_drop(b, id));
                }
            }
        }
    }

    /// `map` shape: match xs { Cons(h,t) -> Cons(h+h, self t) | Nil -> Nil }.
    /// Scrutinee `%0` drops at each arm head with the pattern-proven arity.
    /// The Cons arm's tail Cons reuses `%0` *across* the recursive call — the
    /// parked cell lives in this frame's slot and the callee cannot observe
    /// it. The Nil arm's arity-0 ctor does NOT pair (0-payload cells are
    /// skipped as reuse tokens).
    #[test]
    fn map_reuse_across_call_and_arm_shape() {
        let mut pool = pool();
        let list = con(&mut pool, 99);
        let int = int_ty(&mut pool);
        let cons_body = CoreExpr::Let {
            bind: bind(3, int),
            rhs: Atom::prim(Op::AddInt, vec![local(1), local(1)]),
            body: Box::new(CoreExpr::Let {
                bind: bind(4, list),
                rhs: Atom::Call {
                    callee: Callee::Self_,
                    args: vec![local(2)],
                },
                body: Box::new(CoreExpr::Tail(ctor(&[3, 4]))),
            }),
        };
        let f = func(
            vec![bind(0, list)],
            CoreExpr::Match {
                scrut: local(0),
                arms: vec![
                    (
                        CorePat::Ctor {
                            variant: variant(),
                            fields: vec![bind(1, int), bind(2, list)],
                        },
                        cons_body,
                    ),
                    (
                        CorePat::Ctor {
                            variant: variant(),
                            fields: vec![],
                        },
                        CoreExpr::Tail(ctor(&[])),
                    ),
                ],
                ty: list,
            },
            list,
        );
        let f = perceus(&pool, f);
        let reuses = ctor_reuses(&f.body);
        assert_eq!(reuses.len(), 2);
        assert_eq!(
            reuses[0],
            Some(local(0)),
            "Cons-arm tail ctor reuses %0 across the call (canonical map reuse):\n{}",
            f.body
        );
        assert_eq!(
            reuses[1], None,
            "Nil-arm arity-0 ctor does not pair (0-payload reuse is a no-op):\n{}",
            f.body
        );
        let CoreExpr::Match { arms, .. } = &f.body else {
            panic!()
        };
        assert!(find_drop(&arms[0].1, local(0)));
        assert!(find_drop(&arms[1].1, local(0)));
    }

    /// Straight-line `match xs { Cons(h,t) -> Cons(h,t) }`: scrut drops at
    /// arm head with arity 2; the tail Cons (arity 2, no intervening call)
    /// reuses it.
    #[test]
    fn match_scrutinee_reused_in_arm() {
        let mut pool = pool();
        let list = con(&mut pool, 99);
        let int = int_ty(&mut pool);
        let f = func(
            vec![bind(0, list)],
            CoreExpr::Match {
                scrut: local(0),
                arms: vec![(
                    CorePat::Ctor {
                        variant: variant(),
                        fields: vec![bind(1, int), bind(2, list)],
                    },
                    CoreExpr::Tail(ctor(&[1, 2])),
                )],
                ty: list,
            },
            list,
        );
        let f = perceus(&pool, f);
        assert_eq!(ctor_reuses(&f.body), vec![Some(local(0))], "{}", f.body);
    }

    /// `dot_loop` shape: two 3-arity ctors, consumed by a Call, then a
    /// tail-self recursion. Loop-carried reuse pairs each ctor with the
    /// previous iteration's dropped cell.
    #[test]
    fn loop_carried_reuse_across_tail_self() {
        let mut pool = pool();
        let point = con(&mut pool, 99);
        let int = int_ty(&mut pool);
        // fn loop(%0:int, %1:int)
        //   let %2 = Ctor(..3)           ; p
        //   let %3 = Ctor(..3)           ; q
        //   let %4 = call fn#7(%2, %3)   ; frame-limit boundary; %2,%3 last-use
        //   -- Drop %2, Drop %3 land here
        //   if %0 then ret %1 else ret self(%0, %4)
        let body = CoreExpr::Let {
            bind: bind(2, point),
            rhs: ctor(&[0, 0, 0]),
            body: Box::new(CoreExpr::Let {
                bind: bind(3, point),
                rhs: ctor(&[0, 0, 0]),
                body: Box::new(CoreExpr::Let {
                    bind: bind(4, int),
                    rhs: Atom::Call {
                        callee: Callee::Known(FuncIdx(7)),
                        args: vec![local(2), local(3)],
                    },
                    body: Box::new(CoreExpr::If {
                        cond: local(0),
                        then: Box::new(CoreExpr::Tail(Atom::Local(local(1)))),
                        els: Box::new(CoreExpr::Tail(Atom::Call {
                            callee: Callee::Self_,
                            args: vec![local(0), local(4)],
                        })),
                        ty: int,
                    }),
                }),
            }),
        };
        let f = perceus(&pool, func(vec![bind(0, int), bind(1, int)], body, int));
        let reuses = ctor_reuses(&f.body);
        assert_eq!(
            reuses,
            vec![Some(local(2)), Some(local(3))],
            "each Ctor reuses its OWN slot (self-pairing); cross-pairing would read a live slot:\n{}",
            f.body
        );
        assert!(find_drop(&f.body, local(2)));
        assert!(find_drop(&f.body, local(3)));
    }

    /// Without a self-tail edge nothing loop-carries: same body ending in a
    /// plain return leaves the ctor unpaired.
    #[test]
    fn no_loop_carry_without_tail_self() {
        let mut pool = pool();
        let t = con(&mut pool, 99);
        let int = int_ty(&mut pool);
        let body = CoreExpr::Let {
            bind: bind(1, t),
            rhs: ctor(&[0, 0]),
            body: Box::new(CoreExpr::Let {
                bind: bind(2, int),
                rhs: Atom::prim(Op::AddInt, vec![local(1)]),
                body: Box::new(CoreExpr::Tail(Atom::Local(local(2)))),
            }),
        };
        let f = perceus(&pool, func(vec![bind(0, int)], body, int));
        assert_eq!(ctor_reuses(&f.body), vec![None], "{}", f.body);
        assert!(find_drop(&f.body, local(1)), "%1 dropped after last use");
    }

    #[test]
    fn no_drop_for_unboxed_prims() {
        let mut pool = pool();
        let int = int_ty(&mut pool);
        let f = perceus(
            &pool,
            func(
                vec![bind(0, int)],
                CoreExpr::Tail(Atom::Const(ConstId(0))),
                int,
            ),
        );
        assert_eq!(count_drops(&f.body), 0);
    }

    /// Heap local from a `Call` rhs (unknown arity) still drops — RC
    /// correctness — but with `shape: None`, so it never pairs.
    #[test]
    fn call_result_drops_without_shape() {
        let mut pool = pool();
        let t = con(&mut pool, 99);
        let f = perceus(
            &pool,
            func(
                vec![],
                CoreExpr::Let {
                    bind: bind(0, t),
                    rhs: Atom::Call {
                        callee: Callee::Known(FuncIdx(0)),
                        args: vec![],
                    },
                    body: Box::new(CoreExpr::Tail(ctor(&[]))),
                },
                t,
            ),
        );
        let CoreExpr::Let { body, .. } = &f.body else {
            panic!()
        };
        let CoreExpr::Drop { local, shape, .. } = &**body else {
            panic!("expected Drop for dead %0:\n{}", f.body)
        };
        assert_eq!(*local, LocalId(0));
        assert_eq!(*shape, None, "Call result has no known arity");
        assert_eq!(
            ctor_reuses(&f.body),
            vec![None],
            "shape-less drop never pairs"
        );
    }

    /// Ownership equalisation at an `If` join: a local live into one branch
    /// only is dropped at the head of the other, so both paths release it
    /// exactly once.
    #[test]
    fn if_join_equalises_ownership() {
        let mut pool = pool();
        let t = con(&mut pool, 99);
        let int = int_ty(&mut pool);
        let f = perceus(
            &pool,
            func(
                vec![bind(0, t), bind(1, int)],
                CoreExpr::If {
                    cond: local(1),
                    then: Box::new(CoreExpr::Tail(Atom::Local(local(0)))),
                    els: Box::new(CoreExpr::Tail(Atom::Call {
                        callee: Callee::Known(FuncIdx(0)),
                        args: vec![],
                    })),
                    ty: t,
                },
                t,
            ),
        );
        let CoreExpr::If { then, els, .. } = &f.body else {
            panic!()
        };
        assert_eq!(count_drops(then), 0, "then reads %0 as its return value");
        assert_eq!(count_drops(els), 1, "els drops %0 at head");
    }

    /// A rigid quantified variable (`fn id(x) { x }`'s param) is genuinely
    /// polymorphic: its representation is unknown, so it is handled
    /// dynamically and emits no `Drop`. This is the *only* non-heap answer
    /// that is not a primitive — the `Var` arm that used to suppress `Drop`
    /// for a type inference had merely lost is unrepresentable in
    /// [`ResolvedPool`], so a dead nominal bind (below) can no longer be
    /// silently skipped.
    #[test]
    fn bound_is_not_heap_but_a_nominal_is() {
        let mut pool = pool();
        let generic = pool.mk_bound(0);
        let int = int_ty(&mut pool);
        let t = con(&mut pool, 99);
        let f = perceus(
            &pool,
            func(
                vec![bind(0, generic), bind(1, int)],
                CoreExpr::Tail(Atom::Local(local(1))),
                int,
            ),
        );
        assert_eq!(count_drops(&f.body), 0, "rigid Bound owns no cell");

        let f = perceus(
            &pool,
            func(
                vec![bind(0, t), bind(1, int)],
                CoreExpr::Tail(Atom::Local(local(1))),
                int,
            ),
        );
        assert_eq!(count_drops(&f.body), 1, "dead heap param drops at head");
    }

    #[test]
    fn shape_mismatch_does_not_pair() {
        let mut pool = pool();
        let t = con(&mut pool, 99);
        // let %1 = Ctor(%0,%0) [arity 2]; use %1 in PrimOp; drop %1 [arity 2];
        // tail Ctor(%2,%2,%2) [arity 3] — mismatch, no reuse.
        let f = perceus(
            &pool,
            func(
                vec![bind(0, t)],
                CoreExpr::Let {
                    bind: bind(1, t),
                    rhs: ctor(&[0, 0]),
                    body: Box::new(CoreExpr::Let {
                        bind: bind(2, t),
                        rhs: Atom::prim(Op::AddInt, vec![local(1)]),
                        body: Box::new(CoreExpr::Tail(ctor(&[2, 2, 2]))),
                    }),
                },
                t,
            ),
        );
        assert_eq!(
            ctor_reuses(&f.body),
            vec![None, None],
            "arity-2 drop cannot feed arity-3 ctor:\n{}",
            f.body
        );
    }

    /// Join-edge ownership equalisation, the `if_join_equalises_ownership`
    /// discipline extended to `Goto` edges: `%0` is live into the shared cont
    /// (its return value), `%2` is not. The `Goto` edge drops `%2` before the
    /// jump and must NOT drop `%0` (the cont consumes it); the non-`Goto`
    /// path drops `%0` itself. Every owned local is released exactly once on
    /// every path — before the edge or inside the cont, never both.
    #[test]
    fn goto_edges_equalise_ownership() {
        let mut pool = pool();
        let t = con(&mut pool, 99);
        let int = int_ty(&mut pool);
        let f = perceus(
            &pool,
            func(
                vec![bind(0, t), bind(1, int), bind(2, t)],
                CoreExpr::LetCont {
                    id: JoinId(0),
                    cont: Box::new(CoreExpr::Tail(Atom::Local(local(0)))),
                    body: Box::new(CoreExpr::If {
                        cond: local(1),
                        then: Box::new(CoreExpr::Goto(JoinId(0))),
                        els: Box::new(CoreExpr::Tail(Atom::Local(local(2)))),
                        ty: t,
                    }),
                },
                t,
            ),
        );
        let CoreExpr::LetCont { cont, body, .. } = &f.body else {
            panic!("LetCont survives the pass:\n{}", f.body)
        };
        assert_eq!(count_drops(cont), 0, "cont returns %0:\n{}", f.body);
        let CoreExpr::If { then, els, .. } = &**body else {
            panic!("{}", f.body)
        };
        assert!(
            find_drop(then, local(2)),
            "Goto edge drops the local the cont does not consume:\n{}",
            f.body
        );
        assert!(
            !find_drop(then, local(0)),
            "Goto edge must not drop a cont live-in (the cont owns it):\n{}",
            f.body
        );
        assert!(
            find_drop(els, local(0)),
            "non-Goto path drops %0:\n{}",
            f.body
        );
        assert!(!find_drop(els, local(2)), "els returns %2:\n{}", f.body);
    }

    /// A local whose last use is inside the shared cont drops there, per
    /// path, exactly once — covering every entering edge — and no edge drops
    /// it before its `Goto`.
    #[test]
    fn drop_inside_shared_cont_not_on_edges() {
        let mut pool = pool();
        let t = con(&mut pool, 99);
        let int = int_ty(&mut pool);
        let cont = CoreExpr::Let {
            bind: bind(2, int),
            rhs: Atom::prim(Op::AddInt, vec![local(0)]),
            body: Box::new(CoreExpr::Tail(Atom::Local(local(2)))),
        };
        let f = perceus(
            &pool,
            func(
                vec![bind(0, t), bind(1, int)],
                CoreExpr::LetCont {
                    id: JoinId(0),
                    cont: Box::new(cont),
                    body: Box::new(CoreExpr::If {
                        cond: local(1),
                        then: Box::new(CoreExpr::Goto(JoinId(0))),
                        els: Box::new(CoreExpr::Goto(JoinId(0))),
                        ty: int,
                    }),
                },
                int,
            ),
        );
        assert_eq!(
            count_drops(&f.body),
            1,
            "exactly one Drop, inside the cont:\n{}",
            f.body
        );
        let CoreExpr::LetCont { cont, .. } = &f.body else {
            panic!("{}", f.body)
        };
        assert!(
            find_drop(cont, local(0)),
            "%0's last use is in the cont, so it drops there:\n{}",
            f.body
        );
    }

    /// A reuse token parked before the `LetCont` pairs with a `Ctor` on the
    /// fallthrough path but never with one inside the cont: a shared cont is
    /// entered from many edges, so its token stack starts empty.
    #[test]
    fn cont_enters_with_empty_reuse_stack() {
        let mut pool = pool();
        let t = con(&mut pool, 99);
        let int = int_ty(&mut pool);
        let f = perceus(
            &pool,
            func(
                vec![bind(0, int)],
                CoreExpr::Let {
                    bind: bind(1, t),
                    rhs: ctor(&[0, 0]),
                    body: Box::new(CoreExpr::Let {
                        bind: bind(2, int),
                        rhs: Atom::prim(Op::AddInt, vec![local(1)]),
                        // Drop %1 [Enum:2] lands here, ahead of the LetCont.
                        body: Box::new(CoreExpr::LetCont {
                            id: JoinId(0),
                            cont: Box::new(CoreExpr::Tail(ctor(&[0, 0]))),
                            body: Box::new(CoreExpr::If {
                                cond: local(2),
                                then: Box::new(CoreExpr::Goto(JoinId(0))),
                                els: Box::new(CoreExpr::Tail(ctor(&[0, 0]))),
                                ty: t,
                            }),
                        }),
                    }),
                },
                t,
            ),
        );
        assert_eq!(
            ctor_reuses(&f.body),
            vec![None, None, Some(local(1))],
            "the cont's ctor must not claim %1's token; the body-path ctor may:\n{}",
            f.body
        );
    }
}
