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
use scarlet_elaborate::{RTy, ResolvedPool};

/// Run Perceus on one lowered function. `pool` is only used to check whether a
/// bind's `ty` is a heap type and to read a tuple's width. Its types are always
/// resolved, so no unsolved variable can get here and make `is_heap` answer
/// `false` by accident.
pub fn perceus(pool: &ResolvedPool, f: CoreFn) -> CoreFn {
    let CoreFn {
        params,
        body,
        ret_ty,
    } = f;
    let mut cx = Perceus::new(pool);
    for p in &params {
        cx.record_bind(p, None);
    }
    let (body, live) = cx.drop_pass(body);
    // Params are owned on entry. Any the body doesn't read drop at its head,
    // so the frame still releases everything it owns.
    let dead_params: Vec<LocalId> = params
        .iter()
        .map(|p| p.id)
        .filter(|id| !live.contains(id))
        .collect();
    let body = cx.wrap_drops(&dead_params, None, body);
    CoreFn {
        params,
        body: reuse_pass(body),
        ret_ty,
    }
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
    /// Every bind's resolved type, used to decide whether it needs a `Drop`.
    ty: BTreeMap<LocalId, RTy>,
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

    /// Whether the local's type allocates a heap cell that Perceus manages. A
    /// local with no recorded type was never bound by this pass, so it owns
    /// nothing.
    fn is_heap_local(&self, id: LocalId) -> bool {
        self.ty.get(&id).is_some_and(|&t| self.pool.is_heap(t))
    }

    /// Transform `e` and return `(e', live)`, where `live` is the locals from
    /// outer scopes that `e'` reads, which the caller is responsible for.
    /// Drops are inserted for locals that die inside `e`. The spine is walked
    /// in a loop, so a long straight-line body doesn't recurse on the Rust
    /// stack.
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
                    // Transform this before the body, so its live set is known at
                    // every `Goto(id)` the body contains.
                    let (cont, cont_live) = self.drop_pass(*cont);
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
        // The match reads `scrut`, and then each arm owns it. `scrut` is live
        // on entry to the Match, but the arms don't add it to `live_union`. It
        // drops at the head of each arm unless that arm reads it again.
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
                // When the pattern proved the variant, the scrutinee's drop
                // carries that arity, so the reuse walk sees a sized token even
                // for a multi-variant type.
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
                    Some(ReuseShape::ctor(fields.len())),
                )
            }
        }
    }

    /// Put `Drop{x}` in front of `body` for each heap-typed `x` in `dead`,
    /// going in reverse so the printed order matches `dead`. Locals that
    /// aren't on the heap are skipped, because dropping an unboxed primitive
    /// does nothing but still costs an instruction. `arm_scrut` overrides the
    /// recorded shape for one id.
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

/// The state of one function's reuse walk. Each [`reuse_pass`] builds a new
/// one, so a walk never sees another function's back-edge token sets.
struct ReuseWalk {
    /// The token sets that reach each `Tail(Call{Self_})` in the current walk.
    self_tails: Vec<Vec<Token>>,
}

/// Frame-limited reuse over the body once it has its drops. A second walk
/// handles tokens carried round a loop, for a body that tail-calls itself.
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
                            .rposition(|t| t.slot == bind.id && t.shape == want)
                            .or_else(|| avail.iter().rposition(|t| !t.carried && t.shape == want));
                        if let Some(i) = i {
                            *reuse = Some(avail.remove(i).slot);
                        }
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

/// A `Goto` whose join was never peeled is a lowering bug. Carrying on with an
/// empty live set would drop locals the continuation still reads, so this
/// aborts in release builds too.
#[allow(clippy::panic)]
#[cold]
#[inline(never)]
fn unscoped_goto(id: JoinId) -> ! {
    panic!(
        "internal compiler error: perceus reached goto to join {id} with no \
         enclosing LetCont. Report this as a compiler bug."
    )
}

/// Take a module toplevel's pinned globals back out of Perceus's hands, so no
/// `Drop` releases one and no `Ctor` reuses one's cell.
///
/// Perceus only sees the toplevel, where a global's last read is not its last
/// use, because function bodies read it through `Load::Global` for as long as
/// the program runs. Releasing or overwriting it at the toplevel's last read would
/// free a value the program still holds.
pub fn keep_globals(mut f: CoreFn) -> CoreFn {
    let pinned: BTreeSet<LocalId> = f
        .body
        .toplevel_globals()
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    if !pinned.is_empty() {
        spare(&mut f.body, &pinned);
    }
    f
}

/// The walk for [`keep_globals`]. It loops along each spine and only recurses
/// into branches, so a long toplevel costs no stack depth.
fn spare(mut e: &mut CoreExpr, pinned: &BTreeSet<LocalId>) {
    loop {
        if let CoreExpr::Drop { local, body, .. } = &mut *e
            && pinned.contains(local)
        {
            let rest = std::mem::replace(&mut **body, CoreExpr::Tail(Atom::Nil));
            *e = rest;
            continue;
        }
        match e {
            CoreExpr::Let { rhs, body, .. } => {
                spare_atom(rhs, pinned);
                e = body;
            }
            CoreExpr::LetJoin { join, body, .. } => {
                spare(join, pinned);
                e = body;
            }
            CoreExpr::LetCont { cont, body, .. } => {
                spare(cont, pinned);
                e = body;
            }
            CoreExpr::Drop { body, .. } => e = body,
            CoreExpr::If { then, els, .. } => {
                spare(then, pinned);
                e = els;
            }
            CoreExpr::Match { arms, .. } => {
                for (_, body) in arms {
                    spare(body, pinned);
                }
                return;
            }
            CoreExpr::Tail(a) => {
                spare_atom(a, pinned);
                return;
            }
            CoreExpr::Goto(_) => return,
        }
    }
}

fn spare_atom(a: &mut Atom, pinned: &BTreeSet<LocalId>) {
    if let Atom::Ctor { reuse, .. } = a
        && reuse.is_some_and(|r| pinned.contains(&r))
    {
        *reuse = None;
    }
}

/// The known allocation shape of a `Let`'s right-hand side. Only a `Ctor`
/// proves a shape, because the arity of a `Call` or `PrimOp` result isn't
/// known at compile time.
fn ctor_shape(a: &Atom) -> Option<ReuseShape> {
    match a {
        Atom::Ctor { fields, .. } => Some(ReuseShape::ctor(fields.len())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PrimOp;
    use crate::testkit::{bind, ctor, func, local, variant};
    use crate::{ConstId, FuncIdx};
    use scarlet_types::type_def::TypeId;
    use scarlet_types::types::PrimIds;

    /// The same kind of arena the elaborator hands `perceus` in the real
    /// pipeline.
    fn pool() -> ResolvedPool {
        ResolvedPool::new(PrimIds::default())
    }

    /// A nominal type that isn't a primitive, so it lives on the heap and its
    /// allocation width is unknown.
    fn con(p: &mut ResolvedPool, id: i32) -> RTy {
        p.mk_con(TypeId(id), &[])
    }

    /// The unboxed `Int`. It isn't on the heap, so it gets no `Drop`.
    fn int_ty(p: &mut ResolvedPool) -> RTy {
        let int = p.prims().int;
        p.mk_con(int, &[])
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

    /// A toplevel keeps every value it pins to a global. Perceus's drop of one
    /// and a constructor's reuse of its cell both go, but a plain temporary's
    /// drop stays.
    #[test]
    fn keep_globals_spares_pinned_globals_only() {
        let mut pool = pool();
        let obj = con(&mut pool, 7);
        let mut global = bind(0, obj);
        global.global = Some(scarlet_elaborate::GlobalSlot(0));
        let reuse_global = Atom::Ctor {
            variant: variant(),
            fields: Vec::new(),
            reuse: Some(local(0)),
        };
        let top = func(
            Vec::new(),
            CoreExpr::Let {
                bind: global,
                rhs: ctor(&[]),
                body: Box::new(CoreExpr::Let {
                    bind: bind(1, obj),
                    rhs: ctor(&[]),
                    body: Box::new(CoreExpr::Drop {
                        local: local(0),
                        shape: Some(ReuseShape::ctor(0)),
                        body: Box::new(CoreExpr::Drop {
                            local: local(1),
                            shape: Some(ReuseShape::ctor(0)),
                            body: Box::new(CoreExpr::Tail(reuse_global)),
                        }),
                    }),
                }),
            },
            obj,
        );
        let kept = keep_globals(top);
        assert_eq!(count_drops(&kept.body), 1, "only the temporary is dropped");
        assert!(
            matches!(
                kept.body,
                CoreExpr::Let { ref body, .. }
                    if matches!(**body, CoreExpr::Let { ref body, .. }
                        if matches!(**body, CoreExpr::Drop { local, .. } if local == LocalId(1)))
            ),
            "the global's drop is the one removed"
        );
        assert_eq!(
            ctor_reuses(&kept.body),
            vec![None, None, None],
            "no constructor reuses the global's cell"
        );
    }

    /// The shape of `map`:
    /// `match xs { Cons(h,t) -> Cons(h+h, self t) | Nil -> Nil }`. The Cons
    /// arm's tail Cons reuses `%0` across the recursive call, and the Nil arm's
    /// constructor has no fields, so it doesn't pair.
    #[test]
    fn map_reuse_across_call_and_arm_shape() {
        let mut pool = pool();
        let list = con(&mut pool, 99);
        let int = int_ty(&mut pool);
        let cons_body = CoreExpr::Let {
            bind: bind(3, int),
            rhs: Atom::prim(PrimOp::IntAdd, vec![local(1), local(1)]),
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

    /// Straight-line `match xs { Cons(h,t) -> Cons(h,t) }`. The two-field Cons
    /// at the tail reuses the scrutinee dropped at the arm head.
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

    /// The shape of `dot_loop`: two constructors with three fields each, used by
    /// a Call, then a tail call to self. Reuse carried round the loop pairs
    /// each constructor with the cell the previous turn dropped.
    #[test]
    fn loop_carried_reuse_across_tail_self() {
        let mut pool = pool();
        let point = con(&mut pool, 99);
        let int = int_ty(&mut pool);
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

    /// A tail call reads its arguments before its frame's references go, so
    /// no argument is dropped ahead of it: that drop would come before the
    /// read. Other locals the frame holds still drop as usual.
    #[test]
    fn a_tail_call_argument_is_not_dropped_before_the_call() {
        let mut pool = pool();
        let list = con(&mut pool, 99);
        for callee in [Callee::Self_, Callee::Known(FuncIdx(3))] {
            let body = CoreExpr::Let {
                bind: bind(1, list),
                rhs: ctor(&[0]),
                body: Box::new(CoreExpr::Tail(Atom::Call {
                    callee,
                    args: vec![local(1)],
                })),
            };
            let f = perceus(&pool, func(vec![bind(0, list)], body, list));
            assert!(!find_drop(&f.body, local(1)), "{}", f.body);
            assert!(find_drop(&f.body, local(0)), "{}", f.body);
        }
    }

    /// Without a tail call to self, nothing is carried round a loop.
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
                rhs: Atom::prim(PrimOp::IntAdd, vec![local(1)]),
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

    /// A heap local bound to a `Call` result still drops, but with
    /// `shape: None` because its arity is unknown, so it never pairs.
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

    /// A local live into one `If` branch only drops at the head of the other,
    /// so both paths release it exactly once.
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

    /// A rigid quantified variable's representation is unknown, so it is
    /// handled at runtime and gets no `Drop`. It is the only type that isn't a
    /// primitive and still counts as not on the heap. A dead nominal bind still
    /// drops.
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
        let f = perceus(
            &pool,
            func(
                vec![bind(0, t)],
                CoreExpr::Let {
                    bind: bind(1, t),
                    rhs: ctor(&[0, 0]),
                    body: Box::new(CoreExpr::Let {
                        bind: bind(2, t),
                        rhs: Atom::prim(PrimOp::IntAdd, vec![local(1)]),
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

    /// Ownership evens out across `Goto` edges. `%0` is live into the shared
    /// continuation and `%2` isn't. Every owned local is released exactly once
    /// on every path, before the edge or inside the continuation, never both.
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

    /// A local whose last use is inside the shared continuation drops there,
    /// once, and no edge drops it before its `Goto`.
    #[test]
    fn drop_inside_shared_cont_not_on_edges() {
        let mut pool = pool();
        let t = con(&mut pool, 99);
        let int = int_ty(&mut pool);
        let cont = CoreExpr::Let {
            bind: bind(2, int),
            rhs: Atom::prim(PrimOp::IntAdd, vec![local(0)]),
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

    /// A token parked before the `LetCont` pairs on the fallthrough path but
    /// never inside the continuation, whose token stack starts empty.
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
                        rhs: Atom::prim(PrimOp::IntAdd, vec![local(1)]),
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
