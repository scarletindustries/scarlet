//! Zonking copies inference types into the [`ResolvedPool`], and this is the
//! only code that does it. It follows every union-find link, decides what each
//! remaining variable means, and copies the type into the pool. Because
//! nothing else builds an `RTy` from a `Ty`, every consumer downstream can
//! match [`ResolvedNode`] exhaustively.
//!
//! Two kinds of variable get this far. Both become a [`ResolvedNode::Bound`],
//! numbered in order of first appearance within one [`Zonker`]:
//!
//! * A **rigid** variable, quantified by `generalize`/`generalize_top`. This is
//!   real polymorphism.
//! * An **unbound** variable that inference never decided. At runtime this is
//!   the same as a rigid one: we don't know how the value is represented, so
//!   operations on it dispatch dynamically. That is why zonking never fails.
//!
//! An unsolved variable is not a compiler bug. `generalize_top` only rewrites
//! variables reachable from the *signature*, so one that appears only inside a
//! body (such as `array.length([])`'s element type) stays unbound, and nothing
//! ever looks at it.
//!
//! [`Zonker::zonk_or_opaque`] returns an `invented` bool for callers that
//! cache results across zonkers. A variable unsolved now may be solved by a
//! later body, so a node holding an invented `Bound` must not be cached as
//! that type's final answer. The per-`Zonker` memo keeps the bit for the same
//! reason.

use std::collections::HashMap;

use smallvec::SmallVec;

use super::rty::{RTy, ResolvedPool};
use scarlet_types::types::{InferEngine, Ty, TypeNode};

/// Copies inference types into a [`ResolvedPool`], memoising per resolved `Ty`
/// so a type shared by many expressions is allocated once. Each `Zonker`
/// numbers its own `Bound` indices, starting from zero.
pub struct Zonker<'e> {
    eng: &'e InferEngine,
    /// Keyed by the *representative* `Ty`, so types equal under `find_ref`
    /// share a node. The bool is the `invented` bit, kept so a memo hit
    /// reports what the build reported.
    memo: HashMap<Ty, (RTy, bool)>,
    /// Maps a rigid variable's id (`QuantVar::origin_id`) or an unsolved
    /// variable's id to its `Bound` index, so one variable is one type however
    /// often it appears.
    quants: HashMap<i32, u32>,
    next_bound: u32,
}

impl<'e> Zonker<'e> {
    pub fn new(eng: &'e InferEngine) -> Self {
        Zonker {
            eng,
            memo: HashMap::new(),
            quants: HashMap::new(),
            next_bound: 0,
        }
    }

    /// The `Bound` index of a variable, minting one on first appearance.
    fn bound_index(&mut self, origin: i32) -> u32 {
        if let Some(i) = self.quants.get(&origin) {
            return *i;
        }
        let i = self.next_bound;
        self.next_bound += 1;
        self.quants.insert(origin, i);
        i
    }

    /// Resolve `t` and copy it into `pool`. This never fails, because each
    /// unsolved variable becomes its own opaque `Bound`.
    ///
    /// The `bool` is true when an opaque `Bound` was invented anywhere in the
    /// type. A caller caching across zonkers must not keep such a node,
    /// because a later body may solve the variable.
    pub fn zonk_or_opaque(&mut self, pool: &mut ResolvedPool, t: Ty) -> (RTy, bool) {
        self.resolve(pool, t)
    }

    fn resolve(&mut self, pool: &mut ResolvedPool, t: Ty) -> (RTy, bool) {
        let root = self.eng.find_ref(t);
        if let Some(&hit) = self.memo.get(&root) {
            return hit;
        }
        let built = self.build(pool, root);
        self.memo.insert(root, built);
        built
    }

    /// `root` must already be a representative, so its node is never a `Link`.
    fn build(&mut self, pool: &mut ResolvedPool, root: Ty) -> (RTy, bool) {
        match self.eng.node(root) {
            TypeNode::Var(var_id) => match self.eng.root_generic_id(root) {
                Some(origin) => {
                    let i = self.bound_index(origin);
                    (pool.mk_bound(i), false)
                }
                // Keyed by var id, so two unrelated unknowns never collapse.
                None => {
                    let i = self.bound_index(var_id);
                    (pool.mk_bound(i), true)
                }
            },
            // A `Scheme.ty`'s `Bound` indices already belong to this scheme
            // and refer to nothing outside it, so they pass through unchanged.
            TypeNode::Bound(i) => (pool.mk_bound(i), false),
            TypeNode::Con { id, args, .. } => {
                let (kids, inv) = self.zonk_children(pool, args);
                (pool.mk_con(id, &kids), inv)
            }
            TypeNode::Fun { params, ret } => {
                let (ps, inv) = self.zonk_children(pool, params);
                let (r, inv_ret) = self.resolve(pool, ret);
                (pool.mk_fun(&ps, r), inv || inv_ret)
            }
            TypeNode::Tuple { elems } => {
                let (es, inv) = self.zonk_children(pool, elems);
                (pool.mk_tuple(&es), inv)
            }
        }
    }

    fn zonk_children(
        &mut self,
        pool: &mut ResolvedPool,
        sl: scarlet_types::types::ArenaSlice<scarlet_types::types::pool::Children>,
    ) -> (SmallVec<[RTy; 4]>, bool) {
        // Copy these out of the engine's arena first, because `resolve`
        // borrows `self` again.
        let kids: SmallVec<[Ty; 4]> = self.eng.children_of(sl).into();
        let mut out: SmallVec<[RTy; 4]> = SmallVec::with_capacity(kids.len());
        let mut invented = false;
        for k in kids {
            let (r, inv) = self.resolve(pool, k);
            invented |= inv;
            out.push(r);
        }
        (out, invented)
    }
}

/// A pool set up with the engine's primitive type ids, so `prim_of` gives the
/// right answer.
pub fn pool_for(eng: &InferEngine) -> ResolvedPool {
    ResolvedPool::new(eng.prim_ids())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rty::ResolvedNode;
    use scarlet_types::type_def::TypeId;
    use scarlet_types::types::{Prim, PrimIds, new_engine};

    fn is_bound(n: ResolvedNode) -> bool {
        matches!(n, ResolvedNode::Bound(_))
    }

    fn engine() -> InferEngine {
        let mut e = new_engine();
        e.set_prim_ids(PrimIds {
            int: TypeId(1),
            float: TypeId(2),
            string: TypeId(3),
            array: TypeId(4),
        });
        e
    }

    #[test]
    fn concrete_spines_round_trip() {
        let mut e = engine();
        let int = e.mk_con(TypeId(1), "Int", &[]);
        let arr = e.mk_con(TypeId(4), "Array", &[int]);
        let f = e.mk_fun(&[arr], int);
        let mut pool = pool_for(&e);
        let (r, invented) = Zonker::new(&e).zonk_or_opaque(&mut pool, f);
        assert!(!invented, "a concrete spine invents nothing");
        assert_eq!(crate::Arity::of(pool.fun_params(r)), crate::Arity(1));
        let p0 = pool.fun_params(r)[0];
        assert_eq!(pool.con_args(p0).len(), 1);
        assert_eq!(pool.prim_of(pool.con_arg(p0, 0).unwrap()), Some(Prim::Int));
        assert_eq!(pool.prim_of(pool.fun_ret(r).unwrap()), Some(Prim::Int));
        assert!(pool.is_heap(p0));
    }

    #[test]
    fn links_are_chased_without_compression() {
        let mut e = engine();
        let int = e.mk_con(TypeId(1), "Int", &[]);
        let v = e.fresh_var();
        e.unify(v, int).expect("unify");
        // find_ref agrees with find, and leaves the engine alone.
        assert_eq!(e.find_ref(v), e.find(v));
        let mut pool = pool_for(&e);
        let (r, invented) = Zonker::new(&e).zonk_or_opaque(&mut pool, v);
        assert!(!invented, "a solved variable invents nothing");
        assert_eq!(pool.prim_of(r), Some(Prim::Int));
    }

    #[test]
    fn a_shared_spine_is_allocated_once() {
        let mut e = engine();
        let int = e.mk_con(TypeId(1), "Int", &[]);
        let arr = e.mk_con(TypeId(4), "Array", &[int]);
        let tup = e.mk_tuple(&[arr, arr]);
        let mut pool = pool_for(&e);
        let mut z = Zonker::new(&e);
        let (r, _) = z.zonk_or_opaque(&mut pool, tup);
        let elems = pool.tuple_elems(r);
        assert_eq!(elems[0], elems[1], "shared spine must be memoised");
        // Int, Array(Int) and (Array, Array) make three nodes, not five.
        assert_eq!(pool.node_count(), 3);
        // Re-zonking the same type is free.
        let (again, _) = z.zonk_or_opaque(&mut pool, tup);
        assert_eq!(again, r);
        assert_eq!(pool.node_count(), 3);
    }

    #[test]
    fn a_link_and_its_target_share_one_node() {
        let mut e = engine();
        let int = e.mk_con(TypeId(1), "Int", &[]);
        let v = e.fresh_var();
        e.unify(v, int).expect("unify");
        let mut pool = pool_for(&e);
        let mut z = Zonker::new(&e);
        let (a, _) = z.zonk_or_opaque(&mut pool, v);
        let (b, _) = z.zonk_or_opaque(&mut pool, int);
        assert_eq!(a, b);
        assert_eq!(pool.node_count(), 1);
    }

    /// One variable is one `Bound` however often it appears; two are two.
    #[test]
    fn rigid_vars_are_numbered_in_order_of_first_appearance() {
        let mut e = engine();
        // fn(a, b) b is generalised, so both variables are rigid.
        let a = e.fresh_var();
        let b = e.fresh_var();
        let f = e.mk_fun(&[a, b], b);
        let _scheme = e.generalize_top(f);
        let mut pool = pool_for(&e);
        let mut z = Zonker::new(&e);
        let (ra, inv_a) = z.zonk_or_opaque(&mut pool, a);
        let (rb, inv_b) = z.zonk_or_opaque(&mut pool, b);
        assert!(!inv_a && !inv_b, "a rigid variable is not an invention");
        assert_eq!(pool.node(ra), ResolvedNode::Bound(0));
        assert_eq!(pool.node(rb), ResolvedNode::Bound(1));
        // Zonking again gives the same node, because the index is memoised
        // per variable.
        assert_eq!(z.zonk_or_opaque(&mut pool, a).0, ra);
        // A rigid variable is polymorphic, not missing, so we don't assume a
        // heap cell.
        assert!(!pool.is_heap(ra));
        assert!(is_bound(pool.node(ra)));
    }

    /// `Array(?v)` stays an `Array`, not the bare `Bound` buried inside it.
    #[test]
    fn an_opaque_spine_is_never_replaced_by_its_buried_var() {
        let mut e = engine();
        let v = e.fresh_var();
        let arr = e.mk_con(TypeId(4), "Array", &[v]);
        let int = e.mk_con(TypeId(1), "Int", &[]);
        let mut pool = pool_for(&e);
        let mut z = Zonker::new(&e);
        let (r, invented) = z.zonk_or_opaque(&mut pool, arr);
        assert!(
            invented,
            "an opaque Bound anywhere in the spine must be reported"
        );
        assert!(matches!(pool.node(r), ResolvedNode::Con { .. }));
        assert!(pool.is_heap(r));
        assert!(is_bound(pool.node(pool.con_arg(r, 0).expect("Array(_)"))));

        // A fully solved type reports no invention, so a caller may cache it.
        let (_, invented) = z.zonk_or_opaque(&mut pool, int);
        assert!(!invented);
    }

    /// `generalize_top` closes over the signature, so a body-only variable
    /// stays unbound, so a well-typed program can hand the zonker one.
    #[test]
    fn a_well_typed_body_can_leave_an_unbound_var() {
        let mut e = engine();
        let int = e.mk_con(TypeId(1), "Int", &[]);
        // fn f(n) { array.length([]); n }   with  array.length : fn(Array(a)) Int
        e.enter_level();
        let n = e.fresh_var();
        // `[]`'s element unifies with the callee's instantiated `a`. Both
        // sides are unbound and nothing ever decides them.
        let elem = e.fresh_var();
        let empty = e.mk_con(TypeId(4), "Array", &[elem]);
        let callee_a = e.fresh_var();
        let callee_arg = e.mk_con(TypeId(4), "Array", &[callee_a]);
        e.unify(callee_arg, empty).expect("well typed");
        e.leave_level();
        let f = e.mk_fun(&[n], n);
        let _scheme = e.generalize_top(f);

        let mut pool = pool_for(&e);
        let mut z = Zonker::new(&e);
        // The parameter was generalised, but the element type was not.
        assert!(!z.zonk_or_opaque(&mut pool, n).1, "rigid, not invented");
        let (opaque, invented) = z.zonk_or_opaque(&mut pool, empty);
        assert!(invented, "body-only var stays unbound after generalize_top");
        assert!(is_bound(
            pool.node(pool.con_arg(opaque, 0).expect("Array(_)"))
        ));
        assert!(
            pool.is_heap(opaque),
            "Array is a heap cell whatever it holds"
        );
        let (r_int, inv_int) = z.zonk_or_opaque(&mut pool, int);
        assert!(!inv_int);
        assert_eq!(pool.prim_of(r_int), Some(Prim::Int));

        // The invention survives the memo, so a cross-zonker cache never
        // adopts the node.
        let (again, invented) = z.zonk_or_opaque(&mut pool, empty);
        assert_eq!(again, opaque);
        assert!(invented, "a memo hit must report what the build reported");
    }

    #[test]
    fn an_opaque_var_keeps_its_spine_and_its_identity() {
        let mut e = engine();
        let int = e.mk_con(TypeId(1), "Int", &[]);
        let u = e.fresh_var();
        let w = e.fresh_var();
        let inner = e.mk_tuple(&[u, int]);
        let outer = e.mk_con(TypeId(4), "Array", &[inner]);
        let mut pool = pool_for(&e);
        let mut z = Zonker::new(&e);
        let (r, _) = z.zonk_or_opaque(&mut pool, outer);
        let t = pool.con_arg(r, 0).expect("Array(_)");
        assert!(matches!(pool.node(t), ResolvedNode::Tuple { elems } if elems.len == 2));
        assert!(is_bound(pool.node(pool.tuple_elem(t, 0).unwrap())));
        assert_eq!(
            pool.prim_of(pool.tuple_elem(t, 1).unwrap()),
            Some(Prim::Int)
        );
        assert!(pool.is_heap(r));
        // Two unrelated unknowns are two types, not one.
        let (ru, _) = z.zonk_or_opaque(&mut pool, u);
        let (rw, _) = z.zonk_or_opaque(&mut pool, w);
        assert_eq!(ru, pool.tuple_elem(t, 0).unwrap());
        assert_ne!(pool.node(ru), pool.node(rw));
    }
}
