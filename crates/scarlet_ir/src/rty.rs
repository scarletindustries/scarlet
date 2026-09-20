//! Resolved types: the arena every type in a compiled program indexes, in
//! which an unsolved variable is unrepresentable.
//!
//! [`ResolvedNode`] has no `Var` arm, unlike inference's `TypeNode`, so a
//! consumer can never mistake a fresh variable for a solved type.
//!
//! Each lowered function carries the pool its types index. An [`RTy`] means
//! nothing without that pool.

use crate::TypeId;

/// The primitive types an operator can be specialised to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prim {
    Int,
    Float,
    String,
}

/// Nominal ids for the primitives the inference engine recognises directly:
/// int/float/string literals, plus `Array` for structural resolution.
#[derive(Debug, Clone, Copy)]
pub struct PrimIds {
    pub int: TypeId,
    pub float: TypeId,
    pub string: TypeId,
    pub array: TypeId,
}

impl PrimIds {
    /// Map a nominal type id to the corresponding primitive, if it is one.
    /// Identity is by id, never name: a user's `type Int { }` is not `Int`.
    /// `InferEngine::as_prim` and `ResolvedPool::as_prim` both delegate here so
    /// the two sides of the `Ty`/`RTy` divide cannot disagree.
    pub fn prim_of(self, id: TypeId) -> Option<Prim> {
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
        // Placeholders for engine-only tests, distinct from each other and from
        // the 1-based ids `register_type_head` allocates. `set_prim_ids`
        // overwrites them as soon as a compiler owns the engine.
        PrimIds {
            int: TypeId(-1),
            float: TypeId(-2),
            string: TypeId(-3),
            array: TypeId(-4),
        }
    }
}

/// Index into [`ResolvedPool::nodes`]. Only meaningful for the pool that
/// minted it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RTy(pub u32);

impl std::fmt::Display for RTy {
    /// Bare index, no prefix: the golden harness in
    /// `crates/scarlet/tests/core_ir.rs` renumbers core IR's `:N` sigils by scanning
    /// for digits after the colon.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A contiguous run of child [`RTy`]s in [`ResolvedPool::children`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RSlice {
    start: u32,
    pub len: u32,
}

impl RSlice {
    const EMPTY: RSlice = RSlice { start: 0, len: 0 };
}

/// A fully-resolved type node. `types::TypeNode` minus its `Var` arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResolvedNode {
    /// A rigid quantified variable, indexing the enclosing scheme's quantifier
    /// list. Its representation is not statically known.
    Bound(u32),
    /// `Name(args...)`, identified by `id` alone: a user's `type Parsed` is
    /// not `scarlet/http/h1.Parsed`.
    Con { id: TypeId, args: RSlice },
    /// `fn(params...) ret`.
    Fun { params: RSlice, ret: RTy },
    /// `(elems...)`.
    Tuple { elems: RSlice },
}

/// Append-only arena of resolved types. No
/// union-find and no mutation after the elaborator builds it, so an `RTy`
/// always reads back the same node.
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

    /// How many nodes the pool holds.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// The primitive ids this pool classifies types against.
    pub fn prims(&self) -> PrimIds {
        self.prims
    }

    fn push_children(&mut self, xs: &[RTy]) -> RSlice {
        if xs.is_empty() {
            return RSlice::EMPTY;
        }
        let start = self.children.len() as u32;
        self.children.extend_from_slice(xs);
        RSlice {
            start,
            len: xs.len() as u32,
        }
    }

    fn alloc(&mut self, n: ResolvedNode) -> RTy {
        let t = RTy(self.nodes.len() as u32);
        self.nodes.push(n);
        t
    }

    pub fn mk_bound(&mut self, idx: u32) -> RTy {
        self.alloc(ResolvedNode::Bound(idx))
    }

    pub fn mk_con(&mut self, id: TypeId, args: &[RTy]) -> RTy {
        let args = self.push_children(args);
        self.alloc(ResolvedNode::Con { id, args })
    }

    pub fn mk_fun(&mut self, params: &[RTy], ret: RTy) -> RTy {
        let params = self.push_children(params);
        self.alloc(ResolvedNode::Fun { params, ret })
    }

    pub fn mk_tuple(&mut self, elems: &[RTy]) -> RTy {
        let elems = self.push_children(elems);
        self.alloc(ResolvedNode::Tuple { elems })
    }

    /// The node `t` names. Panics on an `RTy` from a different pool.
    #[allow(clippy::indexing_slicing)]
    pub fn node(&self, t: RTy) -> ResolvedNode {
        self.nodes[t.0 as usize]
    }

    #[allow(clippy::indexing_slicing)]
    pub fn children(&self, s: RSlice) -> &[RTy] {
        let lo = s.start as usize;
        &self.children[lo..lo + s.len as usize]
    }

    /// Type arguments of a nominal type; empty for every other node.
    pub fn con_args(&self, t: RTy) -> &[RTy] {
        match self.node(t) {
            ResolvedNode::Con { args, .. } => self.children(args),
            _ => &[],
        }
    }

    /// `i`th type argument of a nominal type — `Result(_, E)`'s `E` is
    /// `con_arg(t, 1)`.
    pub fn con_arg(&self, t: RTy, i: usize) -> Option<RTy> {
        self.con_args(t).get(i).copied()
    }

    pub fn tuple_elems(&self, t: RTy) -> &[RTy] {
        match self.node(t) {
            ResolvedNode::Tuple { elems } => self.children(elems),
            _ => &[],
        }
    }

    pub fn tuple_elem(&self, t: RTy, i: usize) -> Option<RTy> {
        self.tuple_elems(t).get(i).copied()
    }

    pub fn fun_params(&self, t: RTy) -> &[RTy] {
        match self.node(t) {
            ResolvedNode::Fun { params, .. } => self.children(params),
            _ => &[],
        }
    }

    pub fn fun_ret(&self, t: RTy) -> Option<RTy> {
        match self.node(t) {
            ResolvedNode::Fun { ret, .. } => Some(ret),
            _ => None,
        }
    }

    /// [`PrimIds::prim_of`] against this pool's ids.
    fn as_prim(&self, id: TypeId) -> Option<Prim> {
        self.prims.prim_of(id)
    }

    /// The primitive `t` denotes, if any.
    pub fn prim_of(&self, t: RTy) -> Option<Prim> {
        match self.node(t) {
            ResolvedNode::Con { id, .. } => self.as_prim(id),
            ResolvedNode::Bound(_) | ResolvedNode::Fun { .. } | ResolvedNode::Tuple { .. } => None,
        }
    }

    /// Whether a value of type `t` occupies a Perceus-managed heap cell.
    ///
    /// `Bound` answers `false`: its representation is unknown here, so the
    /// value is handled dynamically.
    pub fn is_heap(&self, t: RTy) -> bool {
        match self.node(t) {
            ResolvedNode::Con { id, .. } => self.as_prim(id).is_none(),
            ResolvedNode::Tuple { .. } | ResolvedNode::Fun { .. } => true,
            ResolvedNode::Bound(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool() -> ResolvedPool {
        ResolvedPool::new(PrimIds {
            int: TypeId(1),
            float: TypeId(2),
            string: TypeId(3),
            array: TypeId(4),
        })
    }

    /// Core IR's `:{ty}` sigil must render as `:7`, not `:r7`.
    #[test]
    fn rty_prints_the_bare_pool_index() {
        assert_eq!(RTy(0).to_string(), "0");
        assert_eq!(RTy(42).to_string(), "42");
        assert_eq!(format!(":{}", RTy(7)), ":7");
    }

    #[test]
    fn primitives_are_not_heap() {
        let mut p = pool();
        let int = p.mk_con(TypeId(1), &[]);
        let float = p.mk_con(TypeId(2), &[]);
        let string = p.mk_con(TypeId(3), &[]);
        assert_eq!(p.prim_of(int), Some(Prim::Int));
        assert_eq!(p.prim_of(float), Some(Prim::Float));
        assert_eq!(p.prim_of(string), Some(Prim::String));
        assert!(!p.is_heap(int));
        assert!(!p.is_heap(float));
        // Strings are heap-allocated at runtime but are not Perceus cells.
        assert!(!p.is_heap(string));
    }

    #[test]
    fn user_types_tuples_and_functions_are_heap() {
        let mut p = pool();
        let int = p.mk_con(TypeId(1), &[]);
        let user = p.mk_con(TypeId(9), &[]);
        let tup = p.mk_tuple(&[int, int]);
        let fun = p.mk_fun(&[int], int);
        assert!(p.is_heap(user));
        assert!(p.is_heap(tup));
        assert!(p.is_heap(fun));
        assert_eq!(p.prim_of(user), None);
    }

    #[test]
    fn a_bound_variable_is_polymorphic_not_missing() {
        let mut p = pool();
        let b = p.mk_bound(0);
        assert!(!p.is_heap(b));
        assert_eq!(p.prim_of(b), None);
        assert_eq!(p.node(b), ResolvedNode::Bound(0));
    }

    #[test]
    fn children_slices_do_not_alias() {
        let mut p = pool();
        let int = p.mk_con(TypeId(1), &[]);
        let str_ = p.mk_con(TypeId(3), &[]);
        let a = p.mk_tuple(&[int, str_]);
        let b = p.mk_tuple(&[str_, int]);
        assert_eq!(p.tuple_elems(a), &[int, str_]);
        assert_eq!(p.tuple_elems(b), &[str_, int]);
        assert_eq!(p.tuple_elem(a, 1), Some(str_));
        assert_eq!(p.tuple_elem(a, 2), None);
        assert!(matches!(p.node(a), ResolvedNode::Tuple { elems } if elems.len == 2));
        assert!(!matches!(p.node(int), ResolvedNode::Tuple { .. }));
    }

    #[test]
    fn con_args_and_fun_shape() {
        let mut p = pool();
        let int = p.mk_con(TypeId(1), &[]);
        let nil = p.mk_con(TypeId(7), &[]);
        let res = p.mk_con(TypeId(8), &[int, nil]);
        assert_eq!(p.con_arg(res, 0), Some(int));
        assert_eq!(p.con_arg(res, 1), Some(nil));
        assert_eq!(p.con_arg(res, 2), None);
        let f = p.mk_fun(&[int, res], nil);
        assert_eq!(p.fun_params(f), &[int, res]);
        assert_eq!(p.fun_ret(f), Some(nil));
        assert_eq!(p.fun_ret(int), None);
    }
}
