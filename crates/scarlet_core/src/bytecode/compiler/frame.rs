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

use indexmap::IndexMap;

use super::{Scheme, Span, StrId, Ty};
use crate::typed_ir::{BinderId, CaptureIdx, GlobalSlot};

/// The bodies the walk is inside, innermost first. Never empty: the module
/// toplevel is always there.
pub(in crate::bytecode) struct Frames {
    /// The body being walked now.
    pub(in crate::bytecode) current: Frame,
    /// The bodies around it, innermost last. `outer[0]`, when there is one,
    /// is the module toplevel.
    outer: Vec<Frame>,
    /// Entry-frame slots handed out so far, by this module and every module
    /// compiled before it: the toplevel bindings of every module share the
    /// entry frame.
    pub(in crate::bytecode) globals: i32,
}

/// Proof that a frame was entered, which [`Frames::leave`] takes back. Holding
/// one is the only way to leave, so a body cannot pop a frame it did not push.
#[must_use = "a frame entered must be left"]
pub(in crate::bytecode) struct Entered(());

impl Frames {
    /// A module's toplevel, whose bindings take entry-frame slots from
    /// `globals` on.
    pub(in crate::bytecode) fn toplevel(globals: i32) -> Frames {
        let root = Block {
            names: IndexMap::new(),
            unused: None,
        };
        Frames {
            current: Frame::with_root(root),
            outer: Vec::new(),
            globals,
        }
    }

    /// Start walking a nested body. `self_name` is the name the body can call
    /// itself by, if it has one. Type variables rigid around it stay rigid in
    /// it.
    pub(in crate::bytecode) fn enter(&mut self, self_name: Option<SelfName>) -> Entered {
        let mut inner = Frame::with_root(Block::reporting());
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

    /// The binding `name` has in a body around the current one, with that
    /// body's depth: 0 is the module toplevel.
    pub(in crate::bytecode) fn outer_local(&self, name: StrId) -> Option<(usize, Local)> {
        self.outer
            .iter()
            .enumerate()
            .rev()
            .find_map(|(i, f)| f.lookup(name).map(|l| (i, *l)))
    }

    /// Hand out the next entry-frame slot.
    pub(in crate::bytecode) fn alloc_global(&mut self) -> GlobalSlot {
        self.globals += 1;
        GlobalSlot(self.globals - 1)
    }

    /// Every name the current body and the bodies around it bind, outermost
    /// first.
    pub(in crate::bytecode) fn names(&self) -> impl Iterator<Item = StrId> + '_ {
        self.outer
            .iter()
            .chain(std::iter::once(&self.current))
            .flat_map(|f| f.blocks.iter())
            .flat_map(|b| b.names.keys().copied())
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

/// The name a body can call itself by.
#[derive(Clone, Copy)]
pub(in crate::bytecode) enum SelfName {
    /// A top-level `fn`: the module's own entry for it says what it is.
    Decl(StrId),
    /// The lambda a `name = fn(..)` binding makes, whose type is still being
    /// inferred. Nothing else may read a name before it is bound: `x = x`
    /// would infer ⊥ and generalize to ∀A.A, which is unsound.
    Lambda(StrId, Ty),
}

impl SelfName {
    pub(in crate::bytecode) fn name(self) -> StrId {
        match self {
            SelfName::Decl(n) | SelfName::Lambda(n, _) => n,
        }
    }
}

/// What one name a body binds means.
#[derive(Clone, Copy, Debug)]
pub(in crate::bytecode) struct Local {
    pub(in crate::bytecode) scheme: Scheme,
    pub(in crate::bytecode) binder: BinderId,
    /// The entry-frame slot, for a binding the module's own statements make
    /// at its toplevel. A body nested in the toplevel loads the binding from
    /// there instead of capturing it.
    pub(in crate::bytecode) global: Option<GlobalSlot>,
}

/// One body being checked.
pub(in crate::bytecode) struct Frame {
    /// Never empty. `blocks[0]` is the body's own level: its parameters, or
    /// the module toplevel's own names.
    blocks: Vec<Block>,
    /// The next [`BinderId`]. Binders count from zero in every body.
    next_binder: u32,
    /// Names from enclosing bodies this body reads.
    pub(in crate::bytecode) captures: Captures,
    /// The name this body calls itself by.
    pub(in crate::bytecode) self_name: Option<SelfName>,
    /// Type variable ids that are rigid in this body: an annotation's type
    /// parameters, which `instantiate` must not freshen.
    pub(in crate::bytecode) rigid: HashSet<i32>,
}

/// One block's names.
pub(in crate::bytecode) struct Block {
    /// In the order the block binds them.
    names: IndexMap<StrId, Local>,
    /// The names bound here and not read yet, with where they were bound.
    /// `None` for the module toplevel's own level, whose names are never
    /// reported unused.
    pub(in crate::bytecode) unused: Option<HashMap<StrId, Span>>,
}

impl Block {
    fn reporting() -> Block {
        Block {
            names: IndexMap::new(),
            unused: Some(HashMap::new()),
        }
    }
}

impl Frame {
    fn with_root(root: Block) -> Frame {
        Frame {
            blocks: vec![root],
            next_binder: 0,
            captures: Captures::default(),
            self_name: None,
            rigid: HashSet::new(),
        }
    }

    pub(in crate::bytecode) fn mint_binder(&mut self) -> BinderId {
        self.next_binder += 1;
        BinderId(self.next_binder - 1)
    }

    /// What `name` means in this body.
    pub(in crate::bytecode) fn lookup(&self, name: StrId) -> Option<&Local> {
        self.blocks.iter().rev().find_map(|b| b.names.get(&name))
    }

    /// Bind `name` in the innermost block.
    pub(in crate::bytecode) fn insert(&mut self, name: StrId, entry: Local) {
        let last = self.blocks.len() - 1;
        self.blocks[last].names.insert(name, entry);
    }

    /// The blocks open in this body, beyond its own level.
    pub(in crate::bytecode) fn depth(&self) -> usize {
        self.blocks.len() - 1
    }

    /// Note that `name` was bound at `span`. If an earlier binding of it in the
    /// same block was never read, this returns where it was made, because now
    /// that it is shadowed nothing ever can read it.
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
