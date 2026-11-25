//! A spine taken apart: the chain of nodes that ends in a branch, a return or a
//! jump. A pass walks it in a loop, so a long one costs no stack depth, and puts
//! it together again. What comes after a join on it is the join's continuation.

use super::{Atom, Callee, CoreBind, CoreExpr, CorePat, GlobalSlot, JoinId, LocalId};

/// One node peeled off a spine, waiting for what comes after it.
pub(crate) enum Frame {
    Let {
        bind: CoreBind,
        rhs: Atom,
    },
    SetGlobal {
        slot: GlobalSlot,
        value: LocalId,
    },
    Join {
        id: JoinId,
        params: Vec<CoreBind>,
        body: Box<CoreExpr>,
    },
    Drop {
        local: LocalId,
    },
}

impl Frame {
    /// The node, in front of `rest`.
    pub(crate) fn wrap(self, rest: CoreExpr) -> CoreExpr {
        let rest = Box::new(rest);
        match self {
            Frame::Let { bind, rhs } => CoreExpr::Let {
                bind,
                rhs,
                body: rest,
            },
            Frame::SetGlobal { slot, value } => CoreExpr::SetGlobal {
                slot,
                value,
                body: rest,
            },
            Frame::Join { id, params, body } => CoreExpr::Join {
                id,
                params,
                cont: rest,
                body,
            },
            Frame::Drop { local } => CoreExpr::Drop { local, body: rest },
        }
    }

    /// Every one of `frames`, the last innermost, in front of `end`.
    pub(crate) fn wrap_all(frames: Vec<Frame>, end: CoreExpr) -> CoreExpr {
        let frames = frames.into_iter().rev();
        frames.fold(end, |rest, frame| frame.wrap(rest))
    }
}

/// What a spine ends in.
pub(crate) enum End {
    Ret(LocalId),
    TailCall {
        callee: Callee,
        args: Vec<LocalId>,
    },
    Jump {
        to: JoinId,
        args: Vec<LocalId>,
    },
    If {
        cond: LocalId,
        then: Box<CoreExpr>,
        els: Box<CoreExpr>,
    },
    Match {
        scrut: LocalId,
        arms: Vec<(CorePat, CoreExpr)>,
    },
}

impl End {
    pub(crate) fn expr(self) -> CoreExpr {
        match self {
            End::Ret(value) => CoreExpr::Ret(value),
            End::TailCall { callee, args } => CoreExpr::TailCall { callee, args },
            End::Jump { to, args } => CoreExpr::Jump { to, args },
            End::If { cond, then, els } => CoreExpr::If { cond, then, els },
            End::Match { scrut, arms } => CoreExpr::Match { scrut, arms },
        }
    }
}

/// The node `e` is and what comes after it, when it is one that goes on, or what
/// the spine ends in.
pub(crate) fn peel(e: CoreExpr) -> Result<(Frame, CoreExpr), End> {
    match e {
        CoreExpr::Let { bind, rhs, body } => Ok((Frame::Let { bind, rhs }, *body)),
        CoreExpr::SetGlobal { slot, value, body } => Ok((Frame::SetGlobal { slot, value }, *body)),
        CoreExpr::Join {
            id,
            params,
            cont,
            body,
        } => Ok((Frame::Join { id, params, body }, *cont)),
        CoreExpr::Drop { local, body } => Ok((Frame::Drop { local }, *body)),
        CoreExpr::Ret(value) => Err(End::Ret(value)),
        CoreExpr::TailCall { callee, args } => Err(End::TailCall { callee, args }),
        CoreExpr::Jump { to, args } => Err(End::Jump { to, args }),
        CoreExpr::If { cond, then, els } => Err(End::If { cond, then, els }),
        CoreExpr::Match { scrut, arms } => Err(End::Match { scrut, arms }),
    }
}
