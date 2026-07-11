//! The one bridge from inference types into [`ResolvedPool`].
//!
//! `types::Ty` indexes a mutable arena whose `TypeNode` has a `Var` arm;
//! [`RTy`] indexes an immutable pool whose [`ResolvedNode`] does not. Zonking
//! is the act of resolving every union-find link, deciding what each surviving
//! variable *means*, and copying the resulting spine into the pool. Because
//! this is the only constructor of an `RTy` from a `Ty`, every consumer
//! downstream of it may match exhaustively and never ask "but what if
//! inference lost this type?".
//!
//! Two kinds of variable can survive to here, and they are not the same fact:
//!
//! * A **rigid** variable (`TyVarState::Generic`) is honest polymorphism. It
//!   became rigid because `generalize`/`generalize_top` quantified it, and it
//!   maps onto [`ResolvedNode::Bound`] via the quantifier list of the `Scheme`
//!   that quantified it ([`QuantVar::origin_id`] is exactly that link).
//! * An **unbound** variable is a type inference never determined. `zonk`
//!   refuses it: [`UnsolvedVar`] names the offending `Ty` and var id.
//!
//! ## Unsolved variables reach here for well-typed programs
//!
//! `generalize_impl` rewrites only the variables reachable from the type it is
//! handed. A module-level `fn` is generalized over its *signature*
//! (`analysis.rs`'s `generalize_top(fn_ty)`), so a variable that appears only
//! inside the body — `array.length([])`'s element type, an empty array passed
//! straight to a generic callee — is never visited and stays `Unbound`. Its
//! type is genuinely undetermined and genuinely irrelevant: nothing observes
//! the element type of an array that never has an element.
//!
//! So "an unsolved variable is a compiler bug" is **false**, and `zonk`'s
//! `Err` is not a panic in disguise. This was settled against the real
//! compiler, not by reading: for `fn ignore(_x) { 0 }` / `fn f(n) { _ =
//! ignore([]); n }` one variable reachable from the check walk's recorded
//! expression types has an
//! `Unbound` root after the module is checked, while a body-local `xs = []`
//! instead leaves a *rigid* root — `compiler.rs` generalizes at every `let` —
//! whose id the enclosing function's scheme does not quantify.
//!
//! A caller elaborating a body therefore picks its policy per position:
//! [`Zonker::zonk`] where a determined type is part of the contract (a
//! function's own signature), and [`Zonker::zonk_or_opaque`] — undetermined
//! encodes as a fresh `Bound`, unknown representation, dispatch dynamically —
//! for a body's own scratch types.

use std::collections::HashMap;
use std::convert::Infallible;

use smallvec::SmallVec;

use super::rty::{RTy, ResolvedPool};
use crate::types::{InferEngine, Scheme, Ty, TypeNode};

/// A variable that inference never solved, reported by [`zonk`].
///
/// `ty` is the arena index of the variable's representative (so `eng.node(ty)`
/// is `TypeNode::Var(var_id)`), not the type the caller passed in — that type
/// may be a whole spine with the variable buried inside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnsolvedVar {
    pub ty: Ty,
    pub var_id: i32,
}

impl std::fmt::Display for UnsolvedVar {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unsolved type variable ?{}", self.var_id)
    }
}

/// What to do with a variable inference never solved. Chosen per call, not per
/// zonker: the same body has both kinds of position.
///
/// A *type*, not a flag: `Opaque::Err` is [`Infallible`], so the opaque walk is
/// total by construction and no caller has to invent a node for an `Err` the
/// code claims cannot happen.
trait Unknown {
    /// The failure the walk can report. `Infallible` when it cannot fail.
    type Err;
    /// Whether nodes containing invented `Bound`s may be read back from the
    /// opaque memo. Only true for the policy that invents them.
    const READS_OPAQUE_MEMO: bool;
    /// `root` is a `Var(var_id)` representative that no scheme quantifies.
    fn unsolved(
        z: &mut Zonker<'_>,
        pool: &mut ResolvedPool,
        root: Ty,
        var_id: i32,
    ) -> Result<(RTy, bool), Self::Err>;
}

/// Report it. Right where a determined type is part of the contract — a
/// function's own signature, a constructor's field types.
struct Reject;

impl Unknown for Reject {
    type Err = UnsolvedVar;
    const READS_OPAQUE_MEMO: bool = false;

    fn unsolved(
        _z: &mut Zonker<'_>,
        _pool: &mut ResolvedPool,
        root: Ty,
        var_id: i32,
    ) -> Result<(RTy, bool), UnsolvedVar> {
        Err(UnsolvedVar { ty: root, var_id })
    }
}

/// Encode it as a fresh `Bound`: undetermined and rigidly polymorphic are the
/// same operational fact, and a body may legitimately contain the former (see
/// the module docs).
struct Opaque;

impl Unknown for Opaque {
    type Err = Infallible;
    const READS_OPAQUE_MEMO: bool = true;

    fn unsolved(
        z: &mut Zonker<'_>,
        pool: &mut ResolvedPool,
        _root: Ty,
        var_id: i32,
    ) -> Result<(RTy, bool), Infallible> {
        // Keyed by var id for rigid and unsolved alike, so two unrelated
        // unknowns never collapse into one type.
        let i = z.bound_index(var_id);
        Ok((pool.mk_bound(i), true))
    }
}

/// Copies inference types into a [`ResolvedPool`], memoising per resolved `Ty`
/// so a spine shared by many expressions is allocated once.
///
/// The memo and the quantifier map are two halves of the same scope: a
/// `Bound(i)` index is meaningful only relative to one `Scheme`, so caching a
/// node that contains one is only sound while that scheme is current. Hence
/// [`Zonker::enter_scheme`] clears both together.
pub struct Zonker<'e> {
    eng: &'e InferEngine,
    /// Nodes with no invented variable in them, keyed by the *representative*
    /// `Ty` so `find_ref`-equal types share a node even when the caller spells
    /// them differently. Sound for either policy to read.
    memo: HashMap<Ty, RTy>,
    /// Nodes that may embed an opaque `Bound` standing in for an unsolved
    /// variable. Kept apart so [`Zonker::zonk`] — whose whole job is to refuse
    /// exactly those — never reads one back as a success.
    opaque_memo: HashMap<Ty, RTy>,
    /// Rigid var id (`QuantVar::origin_id`) → its `Bound` index.
    quants: HashMap<i32, u32>,
    /// Next index handed to a rigid variable the current scheme did not
    /// quantify — a `let`-generalized body-local binder, whose `Bound` index
    /// no scheme fixes but which is polymorphic all the same.
    next_bound: u32,
}

impl<'e> Zonker<'e> {
    /// A zonker with no quantifier scope: every rigid variable is numbered in
    /// order of first appearance.
    pub fn new(eng: &'e InferEngine) -> Self {
        Zonker {
            eng,
            memo: HashMap::new(),
            opaque_memo: HashMap::new(),
            quants: HashMap::new(),
            next_bound: 0,
        }
    }

    /// A zonker whose rigid variables are numbered by `scheme`'s quantifier
    /// list, so `scheme.ty`'s `Bound(i)`s and the body's rigid vars agree.
    pub fn for_scheme(eng: &'e InferEngine, scheme: &Scheme) -> Self {
        let mut z = Zonker::new(eng);
        z.enter_scheme(scheme);
        z
    }

    /// Re-scope onto `scheme`. Drops the memo: cached nodes may embed `Bound`
    /// indices from the previous scheme, and those indices do not survive the
    /// move.
    pub fn enter_scheme(&mut self, scheme: &Scheme) {
        self.memo.clear();
        self.opaque_memo.clear();
        self.quants.clear();
        let qs = self.eng.quants_of(scheme.quantified);
        for (i, q) in qs.iter().enumerate() {
            if let Some(origin) = q.origin_id {
                self.quants.insert(origin, i as u32);
            }
        }
        // Off the quantifier *count*, not the map: a scheme that crossed
        // engines (every precompiled stdlib scheme) has `origin_id: None`
        // throughout, yet its closed `ty` still spells `Bound(0..n)`. Seeding
        // from `quants.len()` would mint `Bound(0)` for the first body-local
        // rigid var and silently alias the scheme's own first parameter.
        self.next_bound = qs.len() as u32;
    }

    /// The `Bound` index of a rigid variable, minting one if the current
    /// scheme did not quantify it.
    fn bound_index(&mut self, origin: i32) -> u32 {
        if let Some(i) = self.quants.get(&origin) {
            return *i;
        }
        let i = self.next_bound;
        self.next_bound += 1;
        self.quants.insert(origin, i);
        i
    }

    /// Resolve `t` and copy it into `pool`. An unsolved variable anywhere in
    /// the spine is reported, never invented.
    pub fn zonk(&mut self, pool: &mut ResolvedPool, t: Ty) -> Result<RTy, UnsolvedVar> {
        self.resolve::<Reject>(pool, t).map(|(r, _)| r)
    }

    /// As [`zonk`](Self::zonk), but each unsolved variable becomes its own
    /// opaque `Bound`: undetermined and rigidly polymorphic are the same
    /// operational fact — unknown representation, dispatch dynamically. Total,
    /// which is why a body's scratch types can use it (see the module docs).
    pub fn zonk_or_opaque(&mut self, pool: &mut ResolvedPool, t: Ty) -> RTy {
        match self.resolve::<Opaque>(pool, t) {
            Ok((r, _)) => r,
            // `Opaque::Err` is uninhabited: the compiler, not a comment, is
            // what rules this out.
            Err(never) => match never {},
        }
    }

    /// `(node, invented)` — `invented` is true when an unsolved variable was
    /// encoded as an opaque `Bound` anywhere in the spine.
    fn resolve<U: Unknown>(
        &mut self,
        pool: &mut ResolvedPool,
        t: Ty,
    ) -> Result<(RTy, bool), U::Err> {
        let root = self.eng.find_ref(t);
        if let Some(r) = self.memo.get(&root) {
            return Ok((*r, false));
        }
        if U::READS_OPAQUE_MEMO
            && let Some(r) = self.opaque_memo.get(&root)
        {
            return Ok((*r, true));
        }
        let (r, invented) = self.build::<U>(pool, root)?;
        // A node that invented a variable belongs only to the opaque memo: a
        // later strict `zonk` of the same type must still see the `Err`.
        if invented {
            self.opaque_memo.insert(root, r);
        } else {
            self.memo.insert(root, r);
        }
        Ok((r, invented))
    }

    /// `root` is already a representative: no `Link` can be its node. The bool
    /// reports whether an unsolved variable was invented anywhere beneath it.
    fn build<U: Unknown>(
        &mut self,
        pool: &mut ResolvedPool,
        root: Ty,
    ) -> Result<(RTy, bool), U::Err> {
        match self.eng.node(root) {
            TypeNode::Var(var_id) => match self.eng.root_generic_id(root) {
                Some(origin) => {
                    let i = self.bound_index(origin);
                    Ok((pool.mk_bound(i), false))
                }
                None => U::unsolved(self, pool, root, var_id),
            },
            // A `Scheme.ty` already carries closed `Bound` indices; they are
            // this scheme's, so they pass through unchanged.
            TypeNode::Bound(i) => Ok((pool.mk_bound(i), false)),
            TypeNode::Con { id, name, args } => {
                let (kids, inv) = self.zonk_children::<U>(pool, args)?;
                Ok((pool.mk_con(id, name, &kids), inv))
            }
            TypeNode::Fun { params, ret } => {
                let (ps, inv) = self.zonk_children::<U>(pool, params)?;
                let (r, inv_ret) = self.resolve::<U>(pool, ret)?;
                Ok((pool.mk_fun(&ps, r), inv || inv_ret))
            }
            TypeNode::Tuple { elems } => {
                let (es, inv) = self.zonk_children::<U>(pool, elems)?;
                Ok((pool.mk_tuple(&es), inv))
            }
        }
    }

    fn zonk_children<U: Unknown>(
        &mut self,
        pool: &mut ResolvedPool,
        sl: crate::types::ArenaSlice<crate::types::pool::Children>,
    ) -> Result<(SmallVec<[RTy; 4]>, bool), U::Err> {
        // Copied out of the engine's arena first: `resolve` reborrows `self`.
        let kids: SmallVec<[Ty; 4]> = self.eng.children_of(sl).into();
        let mut out: SmallVec<[RTy; 4]> = SmallVec::with_capacity(kids.len());
        let mut invented = false;
        for k in kids {
            let (r, inv) = self.resolve::<U>(pool, k)?;
            invented |= inv;
            out.push(r);
        }
        Ok((out, invented))
    }

    /// The resolved node `t` denotes, without copying it.
    pub fn peek(&self, t: Ty) -> TypeNode {
        self.eng.node(self.eng.find_ref(t))
    }
}

/// Resolve `t` against `eng` and copy it into `pool`.
///
/// Rigid variables are numbered in order of first appearance. When the caller
/// holds the `Scheme` that quantified them — and therefore cares that
/// `Bound(i)` lines up with `quantified[i]` — it should drive a
/// [`Zonker::for_scheme`] instead, which also amortises the memo across the
/// many types of one function body.
pub fn zonk(eng: &InferEngine, pool: &mut ResolvedPool, t: Ty) -> Result<RTy, UnsolvedVar> {
    Zonker::new(eng).zonk(pool, t)
}

/// A pool sized for the engine that will feed it. Convenience so callers do
/// not have to reach for `InferEngine::prim_ids` to keep `prim_of` honest.
pub fn pool_for(eng: &InferEngine) -> ResolvedPool {
    ResolvedPool::new(eng.prim_ids())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::type_def::TypeId;
    use crate::typed_ir::rty::ResolvedNode;
    use crate::types::{NO_STR, Prim, PrimIds, QuantVar, ValueKind, new_engine};

    /// Structural, not textual: the node the pool actually holds is a rigid
    /// `Bound`, which is the only thing an undetermined type may become.
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
        let r = zonk(&e, &mut pool, f).expect("concrete");
        assert_eq!(pool.arity(r), crate::core_ir::Arity(1));
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
        let r = zonk(&e, &mut pool, v).expect("solved");
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
        let r = z.zonk(&mut pool, tup).expect("concrete");
        let elems = pool.tuple_elems(r);
        assert_eq!(elems[0], elems[1], "shared spine must be memoised");
        // Int, Array(Int), (Array,Array) — three nodes, not five.
        assert_eq!(pool.len(), 3);
        // Re-zonking the same type is free.
        let again = z.zonk(&mut pool, tup).expect("concrete");
        assert_eq!(again, r);
        assert_eq!(pool.len(), 3);
    }

    #[test]
    fn a_link_and_its_target_share_one_node() {
        let mut e = engine();
        let int = e.mk_con(TypeId(1), "Int", &[]);
        let v = e.fresh_var();
        e.unify(v, int).expect("unify");
        let mut pool = pool_for(&e);
        let mut z = Zonker::new(&e);
        let a = z.zonk(&mut pool, v).expect("solved");
        let b = z.zonk(&mut pool, int).expect("solved");
        assert_eq!(a, b);
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn a_generalized_var_becomes_its_schemes_bound_index() {
        let mut e = engine();
        // fn(a, b) b  —  quantified [a, b]
        let a = e.fresh_var();
        let b = e.fresh_var();
        let f = e.mk_fun(&[a, b], b);
        let scheme = e.generalize_top(f);
        let mut pool = pool_for(&e);
        let mut z = Zonker::for_scheme(&e, &scheme);
        // The *live* vars still resolve to Generic roots; they must land on the
        // same Bound indices the scheme's closed type uses.
        let ra = z.zonk(&mut pool, a).expect("rigid");
        let rb = z.zonk(&mut pool, b).expect("rigid");
        assert_eq!(pool.node(ra), ResolvedNode::Bound(0));
        assert_eq!(pool.node(rb), ResolvedNode::Bound(1));
        // And the scheme's own closed type (which spells `Bound` directly)
        // agrees with them.
        let rs = z.zonk(&mut pool, scheme.ty).expect("closed");
        assert_eq!(pool.node(pool.fun_params(rs)[0]), ResolvedNode::Bound(0));
        assert_eq!(pool.node(pool.fun_ret(rs).unwrap()), ResolvedNode::Bound(1));
        // A rigid var is polymorphic, not missing: no heap cell is assumed.
        assert!(!pool.is_heap(ra));
        assert!(is_bound(pool.node(ra)));
    }

    #[test]
    fn a_body_local_rigid_var_gets_a_fresh_index() {
        let mut e = engine();
        // The enclosing fn quantifies exactly one var...
        let a = e.fresh_var();
        let f = e.mk_fun(&[a], a);
        let scheme = e.generalize_top(f);
        // ...while `let xs = []` inside the body generalizes its own element
        // var (compiler.rs's `generalize(final_ty)` at every `let`). That var
        // is rigid but not in the fn's quantifier list.
        e.enter_level();
        let elem = e.fresh_var();
        let arr = e.mk_con(TypeId(4), "Array", &[elem]);
        e.leave_level();
        let _local = e.generalize(arr);

        let mut pool = pool_for(&e);
        let mut z = Zonker::for_scheme(&e, &scheme);
        let ra = z.zonk(&mut pool, a).expect("rigid");
        let rarr = z.zonk(&mut pool, arr).expect("rigid element");
        assert_eq!(pool.node(ra), ResolvedNode::Bound(0));
        assert_eq!(
            pool.node(pool.con_arg(rarr, 0).unwrap()),
            ResolvedNode::Bound(1),
            "a body-local rigid var must not collide with the scheme's"
        );
    }

    /// A scheme that crossed engines — every precompiled stdlib scheme, which
    /// `al/build.rs` emits with `origin_id = None` on every quantifier — still
    /// spells `Bound(0..n)` in its closed type. Fresh indices must start past
    /// them, or a body-local type silently becomes the scheme's parameter.
    #[test]
    fn a_crossed_engine_schemes_bound_indices_are_never_reused() {
        let mut e = engine();
        // fn(a, b) b, closed, with no origin on either quantifier.
        let b0 = e.mk_bound(0);
        let b1 = e.mk_bound(1);
        let f = e.mk_fun(&[b0, b1], b1);
        let q = QuantVar {
            constraint: None,
            name: NO_STR,
            origin_id: None,
        };
        let scheme = Scheme {
            quantified: e.push_quants(&[q, q]),
            ty: f,
            kind: ValueKind::Local,
            def: None,
        };
        // A body-local `let`-generalized rigid var, and an unsolved var.
        e.enter_level();
        let local = e.fresh_var();
        e.leave_level();
        let _ = e.generalize(local);
        let unsolved = e.fresh_var();

        let mut pool = pool_for(&e);
        let mut z = Zonker::for_scheme(&e, &scheme);
        let rs = z.zonk(&mut pool, scheme.ty).expect("closed");
        assert_eq!(pool.node(pool.fun_params(rs)[0]), ResolvedNode::Bound(0));
        assert_eq!(pool.node(pool.fun_params(rs)[1]), ResolvedNode::Bound(1));

        let rl = z.zonk(&mut pool, local).expect("rigid");
        assert_eq!(
            pool.node(rl),
            ResolvedNode::Bound(2),
            "a body-local rigid var must not alias an unmapped quantifier"
        );
        let ro = z.zonk_or_opaque(&mut pool, unsolved);
        assert_eq!(pool.node(ro), ResolvedNode::Bound(3));
        assert_ne!(rl, ro);
    }

    /// The opaque walk keeps the spine it was handed: `Array(?v)` stays an
    /// `Array` (a heap cell), it does not degrade to the bare `Bound` of the
    /// variable buried inside it.
    #[test]
    fn an_opaque_spine_is_never_replaced_by_its_buried_var() {
        let mut e = engine();
        let v = e.fresh_var();
        let arr = e.mk_con(TypeId(4), "Array", &[v]);
        let mut pool = pool_for(&e);
        let mut z = Zonker::new(&e);
        let r = z.zonk_or_opaque(&mut pool, arr);
        assert!(matches!(pool.node(r), ResolvedNode::Con { .. }));
        assert!(pool.is_heap(r));
        assert!(is_bound(pool.node(pool.con_arg(r, 0).expect("Array(_)"))));
    }

    #[test]
    fn an_unsolved_var_is_reported_not_invented() {
        let mut e = engine();
        let v = e.fresh_var();
        let arr = e.mk_con(TypeId(4), "Array", &[v]);
        let mut pool = pool_for(&e);
        let err = zonk(&e, &mut pool, arr).expect_err("unsolved");
        // The failure names the buried variable, not the spine it was under.
        assert_eq!(err.ty, e.find_ref(v));
        assert_ne!(err.ty, arr);
    }

    /// The empirical answer to the spec's open question. `generalize_top`
    /// closes over the *signature*; a variable that only ever appears inside
    /// the body is not visited and stays `Unbound`. So a well-typed program
    /// really can hand `zonk` an unsolved variable, and callers elaborating
    /// bodies must have a policy for it rather than calling it a bug.
    #[test]
    fn a_well_typed_body_can_leave_an_unbound_var() {
        let mut e = engine();
        let int = e.mk_con(TypeId(1), "Int", &[]);
        // fn f(n) { array.length([]); n }   with  array.length : fn(Array(a)) Int
        e.enter_level();
        let n = e.fresh_var();
        // `[]`'s element unifies with the instantiated `a` of the callee; both
        // sides are unbound and nothing ever determines them.
        let elem = e.fresh_var();
        let empty = e.mk_con(TypeId(4), "Array", &[elem]);
        let callee_a = e.fresh_var();
        let callee_arg = e.mk_con(TypeId(4), "Array", &[callee_a]);
        e.unify(callee_arg, empty).expect("well typed");
        e.leave_level();
        let f = e.mk_fun(&[n], n);
        let scheme = e.generalize_top(f);

        let mut pool = pool_for(&e);
        let mut z = Zonker::for_scheme(&e, &scheme);
        // The parameter generalized; the element type did not.
        assert!(z.zonk(&mut pool, n).is_ok());
        let err = z
            .zonk(&mut pool, empty)
            .expect_err("body-only var stays unbound after generalize_top");
        assert_eq!(e.find_ref(err.ty), e.find_ref(elem));

        // ...and the elaborator's policy for it: an undetermined type is
        // operationally the same as a polymorphic one.
        let opaque = z.zonk_or_opaque(&mut pool, empty);
        assert!(is_bound(
            pool.node(pool.con_arg(opaque, 0).expect("Array(_)"))
        ));
        assert!(
            pool.is_heap(opaque),
            "Array is a heap cell whatever it holds"
        );
        let r_int = z.zonk(&mut pool, int).expect("Int");
        assert_eq!(pool.prim_of(r_int), Some(Prim::Int));

        // The opaque node is quarantined: asking strictly again still fails.
        assert!(z.zonk(&mut pool, empty).is_err());
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
        let r = z.zonk_or_opaque(&mut pool, outer);
        let t = pool.con_arg(r, 0).expect("Array(_)");
        assert!(matches!(pool.node(t), ResolvedNode::Tuple { elems } if elems.len == 2));
        assert!(is_bound(pool.node(pool.tuple_elem(t, 0).unwrap())));
        assert_eq!(
            pool.prim_of(pool.tuple_elem(t, 1).unwrap()),
            Some(Prim::Int)
        );
        assert!(pool.is_heap(r));
        // Two unrelated unknowns are two types, not one.
        let ru = z.zonk_or_opaque(&mut pool, u);
        let rw = z.zonk_or_opaque(&mut pool, w);
        assert_eq!(ru, pool.tuple_elem(t, 0).unwrap());
        assert_ne!(pool.node(ru), pool.node(rw));
    }

    #[test]
    fn entering_a_scheme_drops_the_previous_bound_numbering() {
        let mut e = engine();
        let a = e.fresh_var();
        let b = e.fresh_var();
        let f = e.mk_fun(&[a, b], b);
        let s1 = e.generalize_top(f);
        let s2 = e.generalize_top(b);
        let mut pool = pool_for(&e);
        let mut z = Zonker::for_scheme(&e, &s1);
        let in_s1 = z.zonk(&mut pool, b).expect("rigid");
        assert_eq!(pool.node(in_s1), ResolvedNode::Bound(1));
        z.enter_scheme(&s2);
        let in_s2 = z.zonk(&mut pool, b).expect("rigid");
        assert_eq!(pool.node(in_s2), ResolvedNode::Bound(0));
    }
}
