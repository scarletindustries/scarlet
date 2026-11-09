//! The function bodies the check walk is inside.
//!
//! Each body being checked has a [`Frame`]: the module toplevel at the
//! bottom, then each `fn` and lambda the walk is nested in. Everything that
//! belongs to one body lives on its frame, so entering a body is pushing a
//! fresh frame and leaving it is popping the finished one off.

use std::collections::{HashMap, HashSet};

use super::{LocalSlot, StrId};
use crate::typed_ir::{BinderId, CaptureIdx};

/// The bodies the walk is inside, innermost first. Never empty: the module
/// toplevel is always there.
pub(in crate::bytecode) struct Frames {
    /// The body being walked now.
    pub(in crate::bytecode) current: Frame,
    /// The bodies around it, innermost last. `outer[0]`, when there is one,
    /// is the module toplevel.
    outer: Vec<Frame>,
}

/// Proof that a frame was entered, which [`Frames::leave`] takes back. Holding
/// one is the only way to leave, so a body cannot pop a frame it did not push.
#[must_use = "a frame entered must be left"]
pub(in crate::bytecode) struct Entered(());

impl Frames {
    /// The module toplevel, with entry-frame `locals` already holding `slots`
    /// slots.
    pub(in crate::bytecode) fn toplevel(locals: HashMap<StrId, LocalSlot>, slots: i32) -> Frames {
        Frames {
            current: Frame {
                locals,
                slots,
                ..Frame::default()
            },
            outer: Vec::new(),
        }
    }

    /// Start walking a nested body. `self_name` is the name the body can call
    /// itself by, if it has one. Type variables rigid around it stay rigid in
    /// it.
    pub(in crate::bytecode) fn enter(&mut self, self_name: Option<StrId>) -> Entered {
        let inner = Frame {
            self_name,
            rigid: self.current.rigid.clone(),
            ..Frame::default()
        };
        self.outer.push(std::mem::replace(&mut self.current, inner));
        Entered(())
    }

    /// Finish the body entered with `entered`, and hand back its frame.
    pub(in crate::bytecode) fn leave(&mut self, entered: Entered) -> Frame {
        let Entered(()) = entered;
        match self.outer.pop() {
            Some(outer) => std::mem::replace(&mut self.current, outer),
            None => left_the_toplevel(),
        }
    }

    /// Whether the walk is in the module toplevel, not inside any `fn`.
    pub(in crate::bytecode) fn at_toplevel(&self) -> bool {
        self.outer.is_empty()
    }

    /// The bodies around the current one, innermost last.
    pub(in crate::bytecode) fn outer(&self) -> &[Frame] {
        &self.outer
    }
}

/// [`Frames::leave`] with nothing to return to. An [`Entered`] is only minted
/// by a push, so only a compiler bug gets here.
#[allow(clippy::panic)]
#[cold]
#[inline(never)]
fn left_the_toplevel() -> ! {
    panic!("internal compiler error: left the module toplevel. Report this as a compiler bug.")
}

/// One body being checked.
#[derive(Default)]
pub(in crate::bytecode) struct Frame {
    /// Each name in scope in this body, to its binding.
    pub(in crate::bytecode) locals: HashMap<StrId, LocalSlot>,
    /// Every change to `locals` made inside an open block, as `(name, entry it
    /// replaced)`, so closing the block can put them back.
    pub(in crate::bytecode) undo_log: Vec<(StrId, Option<LocalSlot>)>,
    /// `undo_log`'s length when each open block began, innermost last.
    pub(in crate::bytecode) scope_marks: Vec<usize>,
    /// Slots handed out so far.
    pub(in crate::bytecode) slots: i32,
    /// The next [`BinderId`]. Binders count from zero in every body.
    pub(in crate::bytecode) next_binder: u32,
    /// Names from enclosing bodies this body reads.
    pub(in crate::bytecode) captures: Captures,
    /// The name this body calls itself by: a `fn`'s name, or the name a
    /// `name = fn(..)` binding gives its lambda.
    pub(in crate::bytecode) self_name: Option<StrId>,
    /// Type variable ids that are rigid in this body: an annotation's type
    /// parameters, which `instantiate` must not freshen.
    pub(in crate::bytecode) rigid: HashSet<i32>,
}

impl Frame {
    pub(in crate::bytecode) fn alloc_slot(&mut self) -> i32 {
        self.slots += 1;
        self.slots - 1
    }

    pub(in crate::bytecode) fn mint_binder(&mut self) -> BinderId {
        self.next_binder += 1;
        BinderId(self.next_binder - 1)
    }
}

/// The names a body reads from the bodies around it, each with its index in
/// the closure's captures.
#[derive(Default)]
pub(in crate::bytecode) struct Captures {
    index: HashMap<StrId, CaptureIdx>,
    /// In capture-index order.
    names: Vec<StrId>,
}

impl Captures {
    pub(in crate::bytecode) fn get(&self, name: StrId) -> Option<CaptureIdx> {
        self.index.get(&name).copied()
    }

    /// Capture `name`, which must not be captured yet.
    pub(in crate::bytecode) fn add(&mut self, name: StrId) -> CaptureIdx {
        let idx = CaptureIdx(self.names.len() as i32);
        self.index.insert(name, idx);
        self.names.push(name);
        idx
    }

    /// The captured names, in capture-index order.
    pub(in crate::bytecode) fn into_names(self) -> Vec<StrId> {
        self.names
    }
}
