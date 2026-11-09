//! The function bodies the check walk is inside, and the blocks inside them.
//!
//! Each body being checked has a [`Frame`]: the module toplevel at the
//! bottom, then each `fn` and lambda the walk is nested in. Everything that
//! belongs to one body lives on its frame, so entering a body is pushing a
//! fresh frame and leaving it is popping the finished one off.
//!
//! A frame's names live in [`Block`]s, innermost last. Opening a block pushes
//! an empty one and closing it drops it, which is all shadowing needs: a name
//! means its entry in the innermost block that has one.

use std::collections::{HashMap, HashSet};

use super::{LocalSlot, Span, StrId};
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
    /// The module toplevel, whose root block already holds `names`, in
    /// `slots` slots.
    pub(in crate::bytecode) fn toplevel(names: HashMap<StrId, LocalSlot>, slots: i32) -> Frames {
        let root = Block {
            names,
            unused: None,
        };
        Frames {
            current: Frame::with_root(root, slots),
            outer: Vec::new(),
        }
    }

    /// Start walking a nested body. `self_name` is the name the body can call
    /// itself by, if it has one. Type variables rigid around it stay rigid in
    /// it.
    pub(in crate::bytecode) fn enter(&mut self, self_name: Option<StrId>) -> Entered {
        let mut inner = Frame::with_root(Block::reporting(), 0);
        inner.self_name = self_name;
        inner.rigid = self.current.rigid.clone();
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

    /// Note a read of `name`, so it is not reported unused. A body may read a
    /// name from the bodies around it, so they are searched too.
    pub(in crate::bytecode) fn mark_used(&mut self, name: StrId) {
        let frames = std::iter::once(&mut self.current).chain(self.outer.iter_mut().rev());
        for frame in frames {
            for block in frame.blocks.iter_mut().rev() {
                if let Some(unused) = &mut block.unused
                    && unused.remove(&name).is_some()
                {
                    return;
                }
            }
        }
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
pub(in crate::bytecode) struct Frame {
    /// Never empty. `blocks[0]` is the body's own level: its parameters, or
    /// for the module toplevel, the root every module sees.
    blocks: Vec<Block>,
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

/// One block's names.
pub(in crate::bytecode) struct Block {
    names: HashMap<StrId, LocalSlot>,
    /// The names bound here and not read yet, with where they were bound.
    /// `None` for the module toplevel's own level, whose names are never
    /// reported unused.
    pub(in crate::bytecode) unused: Option<HashMap<StrId, Span>>,
}

impl Block {
    fn reporting() -> Block {
        Block {
            names: HashMap::new(),
            unused: Some(HashMap::new()),
        }
    }
}

impl Frame {
    fn with_root(root: Block, slots: i32) -> Frame {
        Frame {
            blocks: vec![root],
            slots,
            next_binder: 0,
            captures: Captures::default(),
            self_name: None,
            rigid: HashSet::new(),
        }
    }

    pub(in crate::bytecode) fn alloc_slot(&mut self) -> i32 {
        self.slots += 1;
        self.slots - 1
    }

    pub(in crate::bytecode) fn mint_binder(&mut self) -> BinderId {
        self.next_binder += 1;
        BinderId(self.next_binder - 1)
    }

    /// What `name` means in this body, and whether that binding was made in
    /// an enclosing block rather than the innermost one.
    pub(in crate::bytecode) fn lookup(&self, name: StrId) -> Option<(LocalSlot, bool)> {
        let innermost = self.blocks.len() - 1;
        self.blocks
            .iter()
            .enumerate()
            .rev()
            .find_map(|(i, b)| b.names.get(&name).map(|&e| (e, i < innermost)))
    }

    pub(in crate::bytecode) fn lookup_mut(&mut self, name: StrId) -> Option<&mut LocalSlot> {
        self.blocks
            .iter_mut()
            .rev()
            .find_map(|b| b.names.get_mut(&name))
    }

    /// Bind `name` in the innermost block.
    pub(in crate::bytecode) fn insert(&mut self, name: StrId, entry: LocalSlot) {
        self.innermost().names.insert(name, entry);
    }

    /// The blocks open in this body, beyond its own level.
    pub(in crate::bytecode) fn depth(&self) -> usize {
        self.blocks.len() - 1
    }

    /// Note that `name` was bound at `span`. Returns where an earlier binding
    /// of it in the same block was made, if nothing ever read that one: now it
    /// is shadowed, nothing can.
    pub(in crate::bytecode) fn track(&mut self, name: StrId, span: Span) -> Option<Span> {
        let block = self.blocks.iter_mut().rev().find(|b| b.unused.is_some())?;
        block.unused.as_mut()?.insert(name, span)
    }

    pub(in crate::bytecode) fn open_block(&mut self) {
        self.blocks.push(Block::reporting());
    }

    /// Close the innermost block. `None` when only the body's own level is
    /// left, which only a body's end closes.
    pub(in crate::bytecode) fn close_block(&mut self) -> Option<Block> {
        if self.blocks.len() > 1 {
            self.blocks.pop()
        } else {
            None
        }
    }

    /// The body's own level, for a body that is finished.
    pub(in crate::bytecode) fn into_root(mut self) -> Block {
        self.blocks.swap_remove(0)
    }

    fn innermost(&mut self) -> &mut Block {
        let last = self.blocks.len() - 1;
        &mut self.blocks[last]
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
