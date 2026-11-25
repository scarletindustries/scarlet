//! A spine taken apart: the chain of nodes that ends in a tail, a branch or a
//! jump. A pass walks it in a loop, so a long body costs no stack depth, and
//! puts it together again.

use super::{Atom, CoreBind, CoreExpr, JoinId, LocalId};

/// One node peeled off a spine, waiting for its rewritten body.
pub(crate) enum Frame {
    Let { bind: CoreBind, rhs: Atom },
    Join { bind: CoreBind, join: CoreExpr },
    Cont { id: JoinId, cont: CoreExpr },
    Drop { local: LocalId },
}

impl Frame {
    pub(crate) fn wrap(self, body: CoreExpr) -> CoreExpr {
        let body = Box::new(body);
        match self {
            Frame::Let { bind, rhs } => CoreExpr::Let { bind, rhs, body },
            Frame::Join { bind, join } => CoreExpr::LetJoin {
                bind,
                join: Box::new(join),
                body,
            },
            Frame::Cont { id, cont } => CoreExpr::LetCont {
                id,
                cont: Box::new(cont),
                body,
            },
            Frame::Drop { local } => CoreExpr::Drop { local, body },
        }
    }
}
