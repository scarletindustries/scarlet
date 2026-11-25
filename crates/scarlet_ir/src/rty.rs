//! Resolved types. Every type in a compiled program is an index into this
//! arena, and the arena has no way to hold an unsolved type variable.
//!
//! [`ResolvedNode`] has no `Var` arm, unlike inference's `TypeNode`, so a
//! consumer can never mistake a fresh variable for a solved type.
//!
//! Each lowered function carries the pool its types index. An [`RTy`] means
//! nothing without that pool.

use crate::TypeId;
use crate::tivec::TableFull;

/// The primitive types an operator can be specialised to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prim {
    Int,
    Float,
    String,
}

/// Whether a value of a type can be a heap cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Repr {
    /// Never. It is a Float, or a type whose constructors have no fields.
    Word,
    /// It can: an Int can be a big one, and nothing is known of a type
    /// variable's values or of a type that has no constructors.
    Counted,
}

/// The nominal ids of the primitives the inference engine knows directly. Int,
/// float and string literals get these types, and `Array` is here so arrays
/// can resolve structurally.
#[derive(Debug, Clone, Copy)]
pub struct PrimIds {
    pub int: TypeId,
    pub float: TypeId,
    pub string: TypeId,
    pub array: TypeId,
}

impl PrimIds {
    /// The primitive a nominal type id stands for, if it is one. This goes by
    /// id, never by name, so a user's `type Int { }` is not `Int`.
    /// `InferEngine::as_prim` and `ResolvedPool::as_prim` both call this, so
    /// the `Ty` side and the `RTy` side can't disagree.
    fn prim_of(self, id: TypeId) -> Option<Prim> {
        if id == self.int {
            Some(Prim::Int)
        } else if id == self.float {
            Some(Prim::Float)
        } else if id == self.string {
            Some(Prim::String)
        } else {
            None
        }
    }
}

impl Default for PrimIds {
    fn default() -> Self {
        // Placeholders for tests that use the engine alone. They differ from
        // each other and from the ids `register_type_head` hands out, which
        // start at 1. `set_prim_ids` replaces them as soon as a compiler owns
        // the engine.
        PrimIds {
            int: TypeId(-1),
            float: TypeId(-2),
            string: TypeId(-3),
            array: TypeId(-4),
        }
    }
}

/// An index into [`ResolvedPool::nodes`]. It only means something to the pool
/// that made it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RTy(pub u32);

impl std::fmt::Display for RTy {
    /// Prints the bare index with no prefix, because the golden harness in
    /// `crates/scarlet/tests/core_ir.rs` renumbers core IR's `:N` sigils by
    /// scanning for digits after the colon.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// An [`RTy`] that isn't in the pool it was looked up in. The pool never
/// shrinks, so this means the `RTy` came from a different pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForeignRTy(pub RTy);

impl std::fmt::Display for ForeignRTy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "type {} isn't in this type pool", self.0)
    }
}

/// A contiguous run of child [`RTy`]s in [`ResolvedPool::children`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RSlice {
    start: u32,
    len: u32,
}

impl RSlice {
    const EMPTY: RSlice = RSlice { start: 0, len: 0 };
}

/// A fully resolved type node. It's `types::TypeNode` without the `Var` arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResolvedNode {
    /// A quantified variable, as an index into the enclosing scheme's list of
    /// quantified variables. It's rigid, so nothing can solve it to a type, and
    /// its representation isn't known at compile time.
    Bound(u32),
    /// `Name(args...)`, identified by `id` alone: a user's `type Parsed` is
    /// not `scarlet/http/h1.Parsed`.
    Con {
        id: TypeId,
        repr: Repr,
        args: RSlice,
    },
    /// `fn(params...) ret`.
    Fun { params: RSlice, ret: RTy },
    /// `(elems...)`.
    Tuple { elems: RSlice },
}

/// An append-only arena of resolved types. It has no union-find, and nothing
/// changes it after the elaborator builds it, so an `RTy` always reads back the
/// same node.
#[derive(Debug, Clone)]
pub struct ResolvedPool {
    nodes: Vec<ResolvedNode>,
    children: Vec<RTy>,
    prims: PrimIds,
}

impl ResolvedPool {
    pub fn new(prims: PrimIds) -> Self {
        ResolvedPool {
            nodes: Vec::new(),
            children: Vec::new(),
            prims,
        }
    }

    /// The primitive ids this pool classifies types against.
    pub fn prims(&self) -> PrimIds {
        self.prims
    }

    fn push_children(&mut self, xs: &[RTy]) -> Result<RSlice, TableFull> {
        if xs.is_empty() {
            return Ok(RSlice::EMPTY);
        }
        let start = u32::try_from(self.children.len()).map_err(|_| TableFull)?;
        let len = u32::try_from(xs.len()).map_err(|_| TableFull)?;
        // `children` reads back `start..start + len`, so that end must fit too.
        start.checked_add(len).ok_or(TableFull)?;
        self.children.extend_from_slice(xs);
        Ok(RSlice { start, len })
    }

    fn alloc(&mut self, n: ResolvedNode) -> Result<RTy, TableFull> {
        let t = RTy(u32::try_from(self.nodes.len()).map_err(|_| TableFull)?);
        self.nodes.push(n);
        Ok(t)
    }

    pub fn mk_bound(&mut self, idx: u32) -> Result<RTy, TableFull> {
        self.alloc(ResolvedNode::Bound(idx))
    }

    pub fn mk_con(&mut self, id: TypeId, repr: Repr, args: &[RTy]) -> Result<RTy, TableFull> {
        let args = self.push_children(args)?;
        self.alloc(ResolvedNode::Con { id, repr, args })
    }

    pub fn mk_fun(&mut self, params: &[RTy], ret: RTy) -> Result<RTy, TableFull> {
        let params = self.push_children(params)?;
        self.alloc(ResolvedNode::Fun { params, ret })
    }

    pub fn mk_tuple(&mut self, elems: &[RTy]) -> Result<RTy, TableFull> {
        let elems = self.push_children(elems)?;
        self.alloc(ResolvedNode::Tuple { elems })
    }

    /// The node `t` names.
    fn node(&self, t: RTy) -> Result<ResolvedNode, ForeignRTy> {
        self.nodes.get(t.0 as usize).copied().ok_or(ForeignRTy(t))
    }

    /// The children of `t`'s node `s`. Only this pool makes an `RSlice`, so
    /// `s` misses only when `t` came from a different pool.
    fn children(&self, t: RTy, s: RSlice) -> Result<&[RTy], ForeignRTy> {
        self.children
            .get(s.start as usize..)
            .and_then(|rest| rest.get(..s.len as usize))
            .ok_or(ForeignRTy(t))
    }

    /// The type arguments of a nominal type, or nothing for any other node.
    fn con_args(&self, t: RTy) -> Result<&[RTy], ForeignRTy> {
        match self.node(t)? {
            ResolvedNode::Con { args, .. } => self.children(t, args),
            ResolvedNode::Bound(_) | ResolvedNode::Fun { .. } | ResolvedNode::Tuple { .. } => {
                Ok(&[])
            }
        }
    }

    /// The `i`th type argument of a nominal type. For example, `Result(_, E)`'s
    /// `E` is `con_arg(t, 1)`.
    pub fn con_arg(&self, t: RTy, i: usize) -> Result<Option<RTy>, ForeignRTy> {
        Ok(self.con_args(t)?.get(i).copied())
    }

    pub fn fun_params(&self, t: RTy) -> Result<&[RTy], ForeignRTy> {
        match self.node(t)? {
            ResolvedNode::Fun { params, .. } => self.children(t, params),
            ResolvedNode::Bound(_) | ResolvedNode::Con { .. } | ResolvedNode::Tuple { .. } => {
                Ok(&[])
            }
        }
    }

    pub fn fun_ret(&self, t: RTy) -> Result<Option<RTy>, ForeignRTy> {
        Ok(match self.node(t)? {
            ResolvedNode::Fun { ret, .. } => Some(ret),
            ResolvedNode::Bound(_) | ResolvedNode::Con { .. } | ResolvedNode::Tuple { .. } => None,
        })
    }

    /// [`PrimIds::prim_of`] against this pool's ids.
    fn as_prim(&self, id: TypeId) -> Option<Prim> {
        self.prims.prim_of(id)
    }

    /// The primitive `t` denotes, if any.
    pub fn prim_of(&self, t: RTy) -> Result<Option<Prim>, ForeignRTy> {
        Ok(match self.node(t)? {
            ResolvedNode::Con { id, .. } => self.as_prim(id),
            ResolvedNode::Bound(_) | ResolvedNode::Fun { .. } | ResolvedNode::Tuple { .. } => None,
        })
    }

    /// Whether a value of type `t` can be a heap cell. Every type in the pool
    /// has an answer, which the compiler gave it when it made the type.
    pub fn repr(&self, t: RTy) -> Result<Repr, ForeignRTy> {
        Ok(match self.node(t)? {
            ResolvedNode::Con { repr, .. } => repr,
            ResolvedNode::Bound(_) | ResolvedNode::Fun { .. } | ResolvedNode::Tuple { .. } => {
                Repr::Counted
            }
        })
    }

    /// Whether a value of type `t` gets a `Drop` where it dies: one that can be a
    /// cell does. An Int and a String are left to the frame that holds them.
    pub fn gets_drop(&self, t: RTy) -> Result<bool, ForeignRTy> {
        Ok(self.repr(t)? == Repr::Counted && self.prim_of(t)?.is_none())
    }
}

#[cfg(test)]
mod tests;
