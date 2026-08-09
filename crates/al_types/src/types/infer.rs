use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;

use indexmap::{IndexMap, IndexSet};
use smallvec::SmallVec;

use super::environment::{DefinitionLocation, TypeBody, TypeEnv, TypeParam, Variant, VariantField};
use crate::type_def::{
    FieldDef, PrimitiveKind, Type, TypeId, prim_names as pn, t_array, t_float, t_int, t_string,
    t_tuple, t_var,
};
use al_syntax::diagnostic::{Diagnostic, DiagnosticCode};
use al_syntax::span::Span;

/// An Elm-style constrained type variable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Constraint {
    Addable,
    Numeric,
}

impl Constraint {
    pub fn allowed_types(&self) -> &'static [Prim] {
        const ADDABLE: &[Prim] = &[Prim::Int, Prim::Float, Prim::String];
        const NUMERIC: &[Prim] = &[Prim::Int, Prim::Float];
        match self {
            Constraint::Addable => ADDABLE,
            Constraint::Numeric => NUMERIC,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Constraint::Addable => "addable",
            Constraint::Numeric => "numeric",
        }
    }

    /// Greatest lower bound: the constraint admitting exactly the types both
    /// admit, or `None` when their allowed sets are disjoint. Deliberately
    /// exhaustive over pairs so a new variant must spell out its intersections.
    pub fn intersect(self, other: Constraint) -> Option<Constraint> {
        match (self, other) {
            (Constraint::Addable, Constraint::Addable) => Some(Constraint::Addable),
            (Constraint::Numeric, Constraint::Numeric)
            | (Constraint::Numeric, Constraint::Addable)
            | (Constraint::Addable, Constraint::Numeric) => Some(Constraint::Numeric),
        }
    }

    /// Whether a rigid generic carrying `generic` satisfies this requirement,
    /// so a constrained var may link to it without discarding the constraint.
    /// An unconstrained generic satisfies nothing: it carries no guarantee.
    fn satisfied_by_generic(self, generic: Option<Constraint>) -> bool {
        match generic {
            Some(g) => {
                let required = self.allowed_types();
                g.allowed_types().iter().all(|t| required.contains(t))
            }
            None => false,
        }
    }
}

/// `type_def::PrimitiveKind` under the short name the codegen paths use.
pub use crate::type_def::PrimitiveKind as Prim;

/// Index into `InferEngine.nodes`. Meaningful only relative to the engine that
/// minted it, or the engine seeded from the static stdlib arrays.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Ty(pub u32);

impl Ty {
    /// Sentinel `Ty` meaning "no arena node".
    pub const NONE: Ty = Ty(u32::MAX);

    #[inline]
    pub const fn idx(self) -> usize {
        self.0 as usize
    }
}

/// Index into `InferEngine.strings`.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct StrId(pub u32);

impl StrId {
    /// Sentinel `StrId` meaning "no string", for fields that would be
    /// `Option<String>` if the struct did not have to stay const-constructible.
    pub const NONE: StrId = StrId(u32::MAX);

    #[inline]
    pub const fn idx(self) -> usize {
        self.0 as usize
    }
}

/// Cache slot for a nullary primitive `Con` node. `unify` compares `Con` by
/// nominal `TypeId`, never arena identity, so one node per primitive can be
/// shared across every literal instead of minting one per call.
#[derive(Debug, Clone, Copy)]
#[repr(usize)]
pub enum NullaryPrim {
    Int = 0,
    Float = 1,
    String = 2,
    Bool = 3,
    Nil = 4,
    Binary = 5,
}
const NULLARY_CACHE_LEN: usize = 6;

#[derive(Debug)]
struct NullaryCache([Ty; NULLARY_CACHE_LEN]);
impl Default for NullaryCache {
    fn default() -> Self {
        NullaryCache([Ty::NONE; NULLARY_CACHE_LEN])
    }
}

/// Markers naming which `InferEngine` side-pool an [`ArenaSlice`] indexes, so
/// handing a slice to the wrong pool is a compile error rather than a garbage
/// read.
pub mod pool {
    pub enum Children {}
    pub enum Quants {}
    pub enum StrSlices {}
    pub enum TypeParams {}
    pub enum VariantFields {}
    pub enum Variants {}
}

/// Half-open `[start, start+len)` into the `InferEngine` side-pool `P` (see
/// [`pool`]). `len` is `u16` to keep `TypeNode` at 12 bytes; no AL type has
/// more than 65535 type arguments, parameters, or tuple elements.
pub struct ArenaSlice<P> {
    pub start: u32,
    pub len: u16,
    _pool: PhantomData<P>,
}

// Manual impls: `#[derive]` would add spurious `P: Trait` bounds.
impl<P> Clone for ArenaSlice<P> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<P> Copy for ArenaSlice<P> {}
impl<P> PartialEq for ArenaSlice<P> {
    fn eq(&self, other: &Self) -> bool {
        self.start == other.start && self.len == other.len
    }
}
impl<P> Eq for ArenaSlice<P> {}
impl<P> Hash for ArenaSlice<P> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.start.hash(state);
        self.len.hash(state);
    }
}
impl<P> std::fmt::Debug for ArenaSlice<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArenaSlice")
            .field("start", &self.start)
            .field("len", &self.len)
            .finish()
    }
}

impl<P> ArenaSlice<P> {
    pub const EMPTY: Self = Self::new(0, 0);
    #[inline]
    pub const fn new(start: u32, len: u16) -> Self {
        Self {
            start,
            len,
            _pool: PhantomData,
        }
    }
    #[inline]
    pub fn range(self) -> std::ops::Range<usize> {
        self.start as usize..(self.start as usize + self.len as usize)
    }
}

/// One node of a type, stored flat in the engine's arena. `Copy` and
/// const-constructible so the static stdlib emits a `&'static [TypeNode]`.
#[derive(Debug, Clone, Copy)]
pub enum TypeNode {
    /// A unification variable, indexing into `InferEngine.vars`. Only valid
    /// within the engine that minted it.
    Var(i32),
    /// A quantified variable, indexing into the enclosing `Scheme.quantified`.
    /// Appears only inside a `Scheme.ty`: `instantiate` substitutes every
    /// `Bound` away, so `unify`/`find`/`occurs` never see one.
    Bound(u32),
    /// `Name(args...)`, a nominal type application.
    ///
    /// `name` is display only. Unification and every semantic lookup go
    /// through `id`, because two types sharing a name (a user's `type Parsed`
    /// next to `al/http/h1.Parsed`) must not unify or answer for each other.
    Con {
        id: TypeId,
        name: StrId,
        args: ArenaSlice<pool::Children>,
    },
    /// `fn(params...) ret`.
    Fun {
        params: ArenaSlice<pool::Children>,
        ret: Ty,
    },
    /// `(elems...)`.
    Tuple { elems: ArenaSlice<pool::Children> },
}

/// Union-find backing store for type variables.
#[derive(Debug, Clone, Copy)]
enum TyVarState {
    Unbound {
        level: i32,
        constraint: Option<Constraint>,
    },
    /// Substitution edge. `ty` is an arena index, so path compression is a
    /// single `u32` write.
    Link { ty: Ty },
    /// A rigid quantified variable produced by `generalize`. `id` is the
    /// originating var id, the stable identity across instantiation.
    Generic {
        id: i32,
        constraint: Option<Constraint>,
    },
}

/// What `vars[id]` holds when `id` came from `find()` returning `Var(id)`.
/// `Link` is unrepresentable, so no downstream match can invent handling for it.
#[derive(Debug, Clone, Copy)]
enum RootVarState {
    Unbound {
        level: i32,
        constraint: Option<Constraint>,
    },
    Generic {
        id: i32,
        constraint: Option<Constraint>,
    },
}

/// What a value name refers to. Carried on `Scheme` so resolving an identifier
/// yields its type and its provenance in one lookup. `Copy`, so the
/// precompiled stdlib can emit `&'static [Scheme]` with no runtime hydration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ValueKind {
    /// Ordinary `let`/param binding. Never instantiated (mono).
    #[default]
    Local,
    /// Top-level `fn` in a module. Generic; instantiated on use.
    ModuleFn,
    /// VM intrinsic registered from Rust. The `@vm(name)` string is resolved
    /// to an opcode at analysis time, so an unknown name is a compile error at
    /// the annotation rather than a codegen fallthrough.
    Builtin { op: al_vm::bytecode::Op },
    /// A data constructor. Carries enough to compile pattern-match and
    /// constructor-call without re-consulting the type env.
    Constructor {
        type_name: StrId,
        type_id: TypeId,
        variant_idx: u16,
        arity: u16,
        /// → `InferEngine.str_slices`
        field_labels: ArenaSlice<pool::StrSlices>,
    },
}

/// One quantified variable in a `Scheme`. `origin_id` records which engine var
/// it was generalized from, so while that body is being checked `instantiate`
/// hands back the same var instead of a fresh one (the `rigid_ids` mechanism).
/// It is engine-local, so `None` once a scheme crosses engines.
#[derive(Debug, Clone, Copy)]
pub struct QuantVar {
    pub constraint: Option<Constraint>,
    /// Display name; `StrId::NONE` when unset.
    pub name: StrId,
    pub origin_id: Option<i32>,
}

#[derive(Debug, Clone, Copy)]
pub struct Scheme {
    /// Bound variables, in `Bound(i)` index order. → `InferEngine.quants`.
    pub quantified: ArenaSlice<pool::Quants>,
    /// Closed type: contains `Bound` (and `Con`/`Fun`/`Tuple`), never `Var`.
    pub ty: Ty,
    pub kind: ValueKind,
    pub def: Option<DefinitionLocation>,
}

pub fn mono(ty: Ty) -> Scheme {
    Scheme {
        quantified: ArenaSlice::EMPTY,
        ty,
        kind: ValueKind::Local,
        def: None,
    }
}

#[derive(Debug, Clone, Copy)]
pub enum UnifyErrorSituation {
    FunctionArity { expected: usize, given: usize },
}

#[derive(Debug, Clone, Copy)]
pub enum UnifyError {
    CouldNotUnify {
        expected: Ty,
        given: Ty,
        situation: Option<UnifyErrorSituation>,
    },
    RecursiveType,
}

impl UnifyError {
    pub fn flip(self) -> Self {
        match self {
            UnifyError::CouldNotUnify {
                expected,
                given,
                situation,
            } => UnifyError::CouldNotUnify {
                expected: given,
                given: expected,
                situation,
            },
            UnifyError::RecursiveType => UnifyError::RecursiveType,
        }
    }

    pub fn into_diagnostic(self, engine: &mut InferEngine, span: Span) -> Diagnostic {
        let message = match self {
            UnifyError::CouldNotUnify {
                expected,
                given,
                situation,
            } => {
                let exp = engine.type_to_str(expected);
                let got = engine.type_to_str(given);
                let mut msg = format!("Type mismatch: expected '{}', got '{}'", exp, got);
                if let Some(UnifyErrorSituation::FunctionArity { expected, given }) = situation {
                    msg.push_str(&format!(
                        "\nExpected {} argument(s), but {} were supplied.",
                        expected, given
                    ));
                }
                msg
            }
            UnifyError::RecursiveType => "Infinite type detected".to_string(),
        };
        Diagnostic::error(span, DiagnosticCode::TypeError, message)
    }
}

fn could_not_unify(expected: Ty, given: Ty) -> UnifyError {
    UnifyError::CouldNotUnify {
        expected,
        given,
        situation: None,
    }
}

/// Re-express a child failure in terms of the enclosing types, so the user
/// sees the whole shape and not just the leaf that disagreed.
fn unify_enclosed(err: UnifyError, expected: Ty, given: Ty) -> UnifyError {
    match err {
        UnifyError::CouldNotUnify { situation, .. } => UnifyError::CouldNotUnify {
            expected,
            given,
            situation,
        },
        other => other,
    }
}

#[derive(Debug, Clone)]
pub enum MatchFunTypeError {
    IncorrectArity {
        expected: usize,
        given: usize,
        params: SmallVec<[Ty; 4]>,
        ret: Ty,
    },
    NotFn {
        ty: Ty,
    },
}

/// The Hindley-Milner inference engine.
#[derive(Debug, Default)]
pub struct InferEngine {
    /// The type arena. `Ty` indexes into this.
    pub nodes: Vec<TypeNode>,
    /// Shared pool for `Con.args`/`Fun.params`/`Tuple.elems`. `ArenaSlice`
    /// indexes into this.
    pub children: Vec<Ty>,
    /// Interned strings. `StrId` indexes into this.
    pub strings: IndexSet<String>,

    // Pools backing the variable-length data `Scheme`/`TypeInfo` carry while
    // staying `Copy` and const-constructible. Append-only; indices are stable.
    /// `Scheme.quantified` slices into this.
    pub quants: Vec<QuantVar>,
    /// `ValueKind::Constructor.field_labels` and `TypeInfo.module` slice into
    /// this. Each entry is itself a `StrId` into `strings`.
    pub str_slices: Vec<StrId>,
    /// `TypeInfo.type_params` slices into this.
    pub type_params: Vec<TypeParam>,
    /// `Variant.fields` slices into this.
    pub variant_fields: Vec<VariantField>,
    /// `TypeBody::Custom.variants` slices into this.
    pub variants: Vec<Variant>,

    vars: Vec<TyVarState>,
    next_var_id: i32,
    current_level: i32,
    /// Var id -> display name.
    var_names: HashMap<i32, StrId>,
    /// Mirror of `var_names`' values, so collision tests are O(1). Must be kept
    /// in lockstep: every insert mirrored, both cleared together.
    used_names: HashSet<StrId>,
    next_name_uid: u64,
    /// Nominal ids for the primitives the engine mints `Con` nodes for during
    /// literal inference. Set once the prelude is registered, so engine-minted
    /// primitives share identity with compiler-minted ones. The defaults keep
    /// engine-only unit tests working with no prelude.
    prim_ids: PrimIds,
    /// One cached arena `Ty` per nullary primitive. `truncate_to` clears
    /// entries past its watermark.
    nullary_cache: NullaryCache,
    pub diagnostics: Vec<Diagnostic>,
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

pub fn new_engine() -> InferEngine {
    InferEngine::default()
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct EnginePoolWatermark {
    // Field order is significant: derived `Ord` compares lexicographically, so
    // `nodes` decides. `ModuleTable::invalidate` relies on `min` over
    // watermarks picking the earliest-compiled module.
    pub nodes: usize,
    pub children: usize,
    pub strings: usize,
    pub quants: usize,
    pub str_slices: usize,
    pub type_params: usize,
    pub variant_fields: usize,
    pub variants: usize,
}

/// Every static pool slice handed to [`InferEngine::seed_arena`]. Named fields
/// rather than eight positional slices, so transposing two same-typed pools is
/// a compile error and not a corrupted arena prefix.
#[derive(Clone, Copy)]
pub struct ArenaSeed<'a> {
    pub nodes: &'a [TypeNode],
    pub children: &'a [Ty],
    pub strings: &'a [&'a str],
    pub quants: &'a [QuantVar],
    pub str_slices: &'a [StrId],
    pub type_params: &'a [TypeParam],
    pub variant_fields: &'a [VariantField],
    pub variants: &'a [Variant],
}

fn next_letter(uid: &mut u64) -> String {
    let alphabet_len: u64 = 26;
    let offset = b'a';
    let mut chars: Vec<char> = Vec::new();
    let mut rest = *uid;
    loop {
        let n = rest % alphabet_len;
        rest /= alphabet_len;
        chars.push((n as u8 + offset) as char);
        if rest == 0 {
            break;
        }
        rest -= 1;
    }
    *uid += 1;
    chars.into_iter().rev().collect()
}

/// Display name for `Bound(i)` when printed without a `Scheme` in hand.
fn bound_letter(index: u32) -> String {
    let mut uid = index as u64;
    next_letter(&mut uid)
}

macro_rules! side_pool {
    ($field:ident : $T:ty, $M:ty => $push:ident, $of:ident) => {
        pub fn $push(&mut self, items: &[$T]) -> ArenaSlice<$M> {
            Self::pool_slice(&mut self.$field, items)
        }
        #[inline]
        pub fn $of(&self, sl: ArenaSlice<$M>) -> &[$T] {
            &self.$field[sl.range()]
        }
    };
}

impl InferEngine {
    // --- Arena primitives ---

    #[inline]
    pub fn node(&self, t: Ty) -> TypeNode {
        self.nodes[t.idx()]
    }

    #[inline]
    pub fn children_of(&self, sl: ArenaSlice<pool::Children>) -> &[Ty] {
        &self.children[sl.range()]
    }

    #[inline]
    fn push(&mut self, n: TypeNode) -> Ty {
        let i = Ty(self.nodes.len() as u32);
        self.nodes.push(n);
        i
    }

    #[allow(clippy::expect_used)] // a >65535-child type is a compiler bug, not user error
    fn push_children(&mut self, kids: &[Ty]) -> ArenaSlice<pool::Children> {
        let start = self.children.len() as u32;
        let len = u16::try_from(kids.len())
            .expect("type has too many children for the arena (max 65535)");
        self.children.extend_from_slice(kids);
        ArenaSlice::new(start, len)
    }

    pub fn intern(&mut self, s: &str) -> StrId {
        if let Some(i) = self.strings.get_index_of(s) {
            return StrId(i as u32);
        }
        StrId(self.strings.insert_full(s.to_string()).0 as u32)
    }

    #[inline]
    pub fn str(&self, id: StrId) -> &str {
        &self.strings[id.idx()]
    }

    // --- Side-pool primitives ---

    #[allow(clippy::expect_used)] // a >65535-item slice is a compiler bug, not user error
    fn pool_slice<T: Copy, P>(pool: &mut Vec<T>, items: &[T]) -> ArenaSlice<P> {
        let start = pool.len() as u32;
        let len = u16::try_from(items.len())
            .expect("type has too many children for the arena (max 65535)");
        pool.extend_from_slice(items);
        ArenaSlice::new(start, len)
    }
    side_pool!(quants: QuantVar, pool::Quants => push_quants, quants_of);
    side_pool!(str_slices: StrId, pool::StrSlices => push_str_ids, str_ids_of);
    side_pool!(type_params: TypeParam, pool::TypeParams => push_type_params, type_params_of);
    side_pool!(variant_fields: VariantField, pool::VariantFields => push_variant_fields, variant_fields_of);
    side_pool!(variants: Variant, pool::Variants => push_variants, variants_of);
    /// Resolve a slice of `StrId`s to owned strings. Hot paths should iterate
    /// `str_ids_of` instead.
    pub fn strs_of(&self, sl: ArenaSlice<pool::StrSlices>) -> Vec<String> {
        self.str_ids_of(sl)
            .iter()
            .map(|&i| self.str(i).to_string())
            .collect()
    }
    /// Intern each string and push the ids as a contiguous slice.
    #[allow(clippy::expect_used)] // a >65535-item slice is a compiler bug, not user error
    pub fn intern_slice<S: AsRef<str>>(&mut self, ss: &[S]) -> ArenaSlice<pool::StrSlices> {
        let start = self.str_slices.len() as u32;
        let len =
            u16::try_from(ss.len()).expect("type has too many children for the arena (max 65535)");
        for s in ss {
            let id = self.intern(s.as_ref());
            self.str_slices.push(id);
        }
        ArenaSlice::new(start, len)
    }

    /// Snapshot every append-only pool length so a later `truncate_to` can roll
    /// the engine back here. Transient state is fully reset rather than
    /// length-captured: stored `Scheme`s use `Bound`, never live `Var`s, so no
    /// var id survives across calls.
    pub fn pool_watermark(&self) -> EnginePoolWatermark {
        EnginePoolWatermark {
            nodes: self.nodes.len(),
            children: self.children.len(),
            strings: self.strings.len(),
            quants: self.quants.len(),
            str_slices: self.str_slices.len(),
            type_params: self.type_params.len(),
            variant_fields: self.variant_fields.len(),
            variants: self.variants.len(),
        }
    }

    /// No `TypeInfo` body may reference `vars` across a rewind: this clears
    /// every entry, so a body holding an open `Var(param_id)` would make a
    /// later `find` index a cleared or re-minted slot. Bodies are closed to
    /// `Bound(idx)` at registration by `close_body` and `precompile.rs`.
    pub fn truncate_to(&mut self, w: &EnginePoolWatermark) {
        self.nodes.truncate(w.nodes);
        let n = w.nodes as u32;
        for c in &mut self.nullary_cache.0 {
            if c.0 >= n {
                *c = Ty::NONE;
            }
        }
        self.children.truncate(w.children);
        self.strings.truncate(w.strings);
        self.quants.truncate(w.quants);
        self.str_slices.truncate(w.str_slices);
        self.type_params.truncate(w.type_params);
        self.variant_fields.truncate(w.variant_fields);
        self.variants.truncate(w.variants);

        self.vars.clear();
        self.next_var_id = 0;
        self.current_level = 0;
        self.var_names.clear();
        self.used_names.clear();
        self.next_name_uid = 0;
        self.diagnostics.clear();
    }

    /// Seed every arena/pool from static slices (the precompiled stdlib). Must
    /// be called before anything is minted so static indices stay valid.
    pub fn seed_arena(&mut self, seed: ArenaSeed<'_>) {
        debug_assert_eq!(self.pool_watermark(), EnginePoolWatermark::default());
        debug_assert!(self.vars.is_empty());
        self.nodes.extend_from_slice(seed.nodes);
        self.children.extend_from_slice(seed.children);
        for s in seed.strings {
            self.intern(s);
        }
        self.quants.extend_from_slice(seed.quants);
        self.str_slices.extend_from_slice(seed.str_slices);
        self.type_params.extend_from_slice(seed.type_params);
        self.variant_fields.extend_from_slice(seed.variant_fields);
        self.variants.extend_from_slice(seed.variants);
    }

    /// Wire the nominal ids of Int/Float/String. Called once by the compiler
    /// right after the prelude is registered or seeded.
    pub fn set_prim_ids(&mut self, ids: PrimIds) {
        self.prim_ids = ids;
        self.nullary_cache = NullaryCache::default();
    }

    /// [`PrimIds::prim_of`] against this engine's ids.
    fn as_prim(&self, id: TypeId) -> Option<Prim> {
        self.prim_ids.prim_of(id)
    }

    // --- Type constructors ---

    pub fn mk_con(&mut self, id: TypeId, name: &str, args: &[Ty]) -> Ty {
        let name = self.intern(name);
        let args = self.push_children(args);
        self.push(TypeNode::Con { id, name, args })
    }

    /// `mk_con(id, name, &[])` memoised per `slot`. Sound because `Con`
    /// identity is nominal; see [`NullaryPrim`].
    #[inline]
    pub fn nullary_con(&mut self, slot: NullaryPrim, id: TypeId, name: &str) -> Ty {
        let i = slot as usize;
        let cached = self.nullary_cache.0[i];
        if cached != Ty::NONE {
            debug_assert!(
                matches!(self.node(cached), TypeNode::Con { id: cid, .. } if cid == id),
                "nullary_con slot reused with a different TypeId"
            );
            return cached;
        }
        let t = self.mk_con(id, name, &[]);
        self.nullary_cache.0[i] = t;
        t
    }

    pub fn mk_con_id(&mut self, id: TypeId, name: StrId, args: &[Ty]) -> Ty {
        let args = self.push_children(args);
        self.push(TypeNode::Con { id, name, args })
    }

    pub fn mk_fun(&mut self, params: &[Ty], ret: Ty) -> Ty {
        let params = self.push_children(params);
        self.push(TypeNode::Fun { params, ret })
    }

    pub fn mk_tuple(&mut self, elems: &[Ty]) -> Ty {
        let elems = self.push_children(elems);
        self.push(TypeNode::Tuple { elems })
    }

    pub fn mk_bound(&mut self, index: u32) -> Ty {
        self.push(TypeNode::Bound(index))
    }

    pub fn icon_int(&mut self) -> Ty {
        let id = self.prim_ids.int;
        self.nullary_con(NullaryPrim::Int, id, pn::INT)
    }
    pub fn icon_float(&mut self) -> Ty {
        let id = self.prim_ids.float;
        self.nullary_con(NullaryPrim::Float, id, pn::FLOAT)
    }
    pub fn icon_string(&mut self) -> Ty {
        let id = self.prim_ids.string;
        self.nullary_con(NullaryPrim::String, id, pn::STRING)
    }

    // --- Annotation name tracking ---

    pub fn name_var(&mut self, t: Ty, name: StrId) {
        if let TypeNode::Var(id) = self.node(t) {
            self.used_names.insert(name);
            self.var_names.insert(id, name);
        }
    }

    /// Mint a fresh base-26 display name, skipping anything already claimed by
    /// `used_names` or the caller's in-flight `taken` set. The winner joins
    /// both sets before returning.
    fn mint_display_name(&mut self, taken: &mut HashSet<StrId>) -> StrId {
        loop {
            let candidate = next_letter(&mut self.next_name_uid);
            let sid = self.intern(&candidate);
            if !self.used_names.contains(&sid) && !taken.contains(&sid) {
                self.used_names.insert(sid);
                taken.insert(sid);
                return sid;
            }
        }
    }

    /// The display name for a var id, minted and remembered on first call so
    /// later calls are stable.
    fn var_display_name(&mut self, id: i32) -> String {
        if let Some(&name) = self.var_names.get(&id) {
            return self.str(name).to_string();
        }
        let sid = self.mint_display_name(&mut HashSet::new());
        self.var_names.insert(id, sid);
        self.str(sid).to_string()
    }

    // --- Fresh variables ---

    fn alloc_var(&mut self, state: TyVarState) -> (Ty, i32) {
        let id = self.next_var_id;
        self.vars.push(state);
        self.next_var_id += 1;
        (self.push(TypeNode::Var(id)), id)
    }

    pub fn fresh_var(&mut self) -> Ty {
        self.alloc_var(TyVarState::Unbound {
            level: self.current_level,
            constraint: None,
        })
        .0
    }

    pub fn fresh_constrained_var(&mut self, c: Constraint) -> Ty {
        self.alloc_var(TyVarState::Unbound {
            level: self.current_level,
            constraint: Some(c),
        })
        .0
    }

    /// Mint a fresh rigid generic variable. The hydrator uses this so repeated
    /// occurrences of one type parameter share an identity that still cannot be
    /// solved to a concrete type while checking the body.
    pub fn fresh_generic_var(&mut self) -> (Ty, i32) {
        let id = self.next_var_id;
        self.alloc_var(TyVarState::Generic {
            id,
            constraint: None,
        })
    }

    /// The representative of `t`, without path compression. Same answer as
    /// [`find`](Self::find), but `&self`, so a read-only consumer can resolve
    /// types while holding the engine immutably.
    // Two independent let-else exits; a single while-let cannot express both.
    #[allow(clippy::while_let_loop)]
    pub fn find_ref(&self, t: Ty) -> Ty {
        let mut root = t;
        loop {
            let TypeNode::Var(id) = self.node(root) else {
                break;
            };
            let TyVarState::Link { ty } = self.vars[id as usize] else {
                break;
            };
            root = ty;
        }
        root
    }

    /// `Some(gid)` when `t`'s representative is a rigid `Generic` variable.
    /// `gid` is the id `QuantVar::origin_id` records, so a caller holding the
    /// scheme can map the variable onto its `Bound` index.
    pub fn root_generic_id(&self, t: Ty) -> Option<i32> {
        let TypeNode::Var(id) = self.node(self.find_ref(t)) else {
            return None;
        };
        match self.vars[id as usize] {
            TyVarState::Generic { id, .. } => Some(id),
            TyVarState::Unbound { .. } | TyVarState::Link { .. } => None,
        }
    }

    /// The nominal ids registered by `set_prim_ids`. Handed to a
    /// `ResolvedPool` so it answers `prim_of` the same way the engine does.
    pub fn prim_ids(&self) -> PrimIds {
        self.prim_ids
    }

    pub fn find(&mut self, t: Ty) -> Ty {
        // Iterative, not recursive: a long chain would blow the native stack.
        let root = self.find_ref(t);
        let mut cur = t;
        while cur != root {
            let TypeNode::Var(id) = self.node(cur) else {
                break;
            };
            let idx = id as usize;
            let TyVarState::Link { ty: next } = self.vars[idx] else {
                break;
            };
            self.vars[idx] = TyVarState::Link { ty: root };
            cur = next;
        }
        root
    }

    /// State of a root var. `id` must have come from `find()` returning
    /// `Var(id)`; a `Link` here means that invariant is broken.
    fn root_var(&self, id: i32) -> RootVarState {
        match self.vars[id as usize] {
            TyVarState::Unbound { level, constraint } => {
                RootVarState::Unbound { level, constraint }
            }
            TyVarState::Generic { id, constraint } => RootVarState::Generic { id, constraint },
            TyVarState::Link { .. } => {
                debug_assert!(false, "root_var on Link: find() invariant broken");
                RootVarState::Unbound {
                    level: 0,
                    constraint: None,
                }
            }
        }
    }

    /// Which primitive `t` resolves to, if any. `None` for an unbound var, a
    /// constrained but unsolved var, and every other constructor.
    pub fn resolved_prim(&mut self, t: Ty) -> Option<Prim> {
        let rep = self.find(t);
        if let TypeNode::Con { id, .. } = self.node(rep) {
            self.as_prim(id)
        } else {
            None
        }
    }

    // --- Level management ---

    pub fn enter_level(&mut self) {
        self.current_level += 1;
    }

    pub fn leave_level(&mut self) {
        self.current_level -= 1;
    }

    // Level-based generalization: binding a var at level L to a type T must
    // also lower every unbound var inside T to min(its_level, L). Otherwise an
    // inner var observable from an outer scope gets quantified at the wrong
    // level. Generic vars carry no level and are skipped.
    fn occurs_and_adjust(&mut self, var_id: i32, var_level: i32, t: Ty) -> bool {
        let r = self.find(t);
        match self.node(r) {
            TypeNode::Var(id) => {
                if id == var_id {
                    return true;
                }
                if let RootVarState::Unbound { level, constraint } = self.root_var(id)
                    && level > var_level
                {
                    self.vars[id as usize] = TyVarState::Unbound {
                        level: var_level,
                        constraint,
                    };
                }
                false
            }
            TypeNode::Bound(_) => {
                debug_assert!(false, "Bound in live inference (occurs_and_adjust)");
                false
            }
            TypeNode::Con { args, .. } => self.occurs_slice(var_id, var_level, args),
            TypeNode::Fun { params, ret } => {
                self.occurs_slice(var_id, var_level, params)
                    || self.occurs_and_adjust(var_id, var_level, ret)
            }
            TypeNode::Tuple { elems } => self.occurs_slice(var_id, var_level, elems),
        }
    }

    fn occurs_slice(
        &mut self,
        var_id: i32,
        var_level: i32,
        sl: ArenaSlice<pool::Children>,
    ) -> bool {
        for i in sl.range() {
            let kid = self.children[i];
            if self.occurs_and_adjust(var_id, var_level, kid) {
                return true;
            }
        }
        false
    }

    // --- Unification ---

    pub fn unify(&mut self, expected: Ty, given: Ty) -> Result<(), UnifyError> {
        let fa = self.find(expected);
        let fb = self.find(given);
        let na = self.node(fa);
        let nb = self.node(fb);

        if let (TypeNode::Var(ia), TypeNode::Var(ib)) = (na, nb)
            && ia == ib
        {
            return Ok(());
        }

        if let TypeNode::Var(id) = na {
            return self.unify_var(id, fa, fb);
        }
        if matches!(nb, TypeNode::Var(_)) {
            return self.unify(fb, fa).map_err(UnifyError::flip);
        }

        match (na, nb) {
            (
                TypeNode::Con {
                    id: ia, args: aa, ..
                },
                TypeNode::Con {
                    id: ib, args: ab, ..
                },
            ) => {
                // Nominal identity only. Two `Parsed`s from different modules
                // are different types and must not unify.
                if ia != ib || aa.len != ab.len {
                    return Err(could_not_unify(fa, fb));
                }
                self.unify_children(aa, ab, fa, fb)
            }
            (
                TypeNode::Fun {
                    params: pa,
                    ret: ra,
                },
                TypeNode::Fun {
                    params: pb,
                    ret: rb,
                },
            ) => {
                if pa.len != pb.len {
                    return Err(UnifyError::CouldNotUnify {
                        expected: fa,
                        given: fb,
                        situation: Some(UnifyErrorSituation::FunctionArity {
                            expected: pa.len as usize,
                            given: pb.len as usize,
                        }),
                    });
                }
                self.unify_children(pa, pb, fa, fb)?;
                self.unify(ra, rb).map_err(|e| unify_enclosed(e, fa, fb))
            }
            (TypeNode::Tuple { elems: ea }, TypeNode::Tuple { elems: eb }) => {
                if ea.len != eb.len {
                    return Err(could_not_unify(fa, fb));
                }
                self.unify_children(ea, eb, fa, fb)
            }
            (TypeNode::Bound(_), _) | (_, TypeNode::Bound(_)) => {
                debug_assert!(false, "Bound in live inference (unify)");
                Err(could_not_unify(fa, fb))
            }
            _ => Err(could_not_unify(fa, fb)),
        }
    }

    fn unify_children(
        &mut self,
        a: ArenaSlice<pool::Children>,
        b: ArenaSlice<pool::Children>,
        outer_a: Ty,
        outer_b: Ty,
    ) -> Result<(), UnifyError> {
        for (ia, ib) in a.range().zip(b.range()) {
            let (ka, kb) = (self.children[ia], self.children[ib]);
            self.unify(ka, kb)
                .map_err(|e| unify_enclosed(e, outer_a, outer_b))?;
        }
        Ok(())
    }

    /// `unify`, pushing a diagnostic on failure. `true` on success.
    pub fn unify_at(&mut self, expected: Ty, given: Ty, span: Span) -> bool {
        match self.unify(expected, given) {
            Ok(()) => true,
            Err(e) => {
                let d = e.into_diagnostic(self, span);
                self.diagnostics.push(d);
                false
            }
        }
    }

    fn unify_var(&mut self, var_id: i32, var_ty: Ty, ty: Ty) -> Result<(), UnifyError> {
        match self.root_var(var_id) {
            RootVarState::Generic {
                id,
                constraint: gen_c,
            } => {
                // A rigid generic may only absorb an unbound var (which then
                // links to the same generic) or an identical generic.
                if let TypeNode::Var(other_id) = self.node(ty) {
                    match self.root_var(other_id) {
                        RootVarState::Unbound {
                            constraint: other_c,
                            ..
                        } => {
                            // Absorbing the var drops its constraint, so the
                            // generic must already guarantee it.
                            if let Some(req) = other_c
                                && !req.satisfied_by_generic(gen_c)
                            {
                                return Err(could_not_unify(var_ty, ty));
                            }
                            self.vars[other_id as usize] = TyVarState::Link { ty: var_ty };
                            return Ok(());
                        }
                        RootVarState::Generic { id: other_gid, .. } if other_gid == id => {
                            return Ok(());
                        }
                        RootVarState::Generic { .. } => {}
                    }
                }
                Err(could_not_unify(var_ty, ty))
            }
            RootVarState::Unbound { level, constraint } => {
                if self.occurs_and_adjust(var_id, level, ty) {
                    return Err(UnifyError::RecursiveType);
                }
                if let Some(c) = constraint {
                    return self.unify_constrained(var_id, var_ty, c, level, ty);
                }
                self.vars[var_id as usize] = TyVarState::Link { ty };
                Ok(())
            }
        }
    }

    fn unify_constrained(
        &mut self,
        var_id: i32,
        var_ty: Ty,
        constraint: Constraint,
        level: i32,
        ty: Ty,
    ) -> Result<(), UnifyError> {
        let resolved = self.find(ty);
        match self.node(resolved) {
            TypeNode::Var(other_id) => {
                match self.root_var(other_id) {
                    RootVarState::Unbound {
                        level: other_level,
                        constraint: other_constraint,
                    } => {
                        if let Some(other_c) = other_constraint {
                            let Some(winner) = constraint.intersect(other_c) else {
                                return Err(could_not_unify(var_ty, resolved));
                            };
                            self.vars[other_id as usize] = TyVarState::Unbound {
                                level: other_level.min(level),
                                constraint: Some(winner),
                            };
                        } else {
                            self.vars[other_id as usize] = TyVarState::Unbound {
                                level: other_level,
                                constraint: Some(constraint),
                            };
                        }
                    }
                    RootVarState::Generic {
                        constraint: gen_c, ..
                    } => {
                        // Linking discards the constraint, so the generic must
                        // already guarantee it. An unconstrained one does not.
                        if !constraint.satisfied_by_generic(gen_c) {
                            return Err(could_not_unify(var_ty, resolved));
                        }
                    }
                }
            }
            TypeNode::Con { id, .. } => match self.as_prim(id) {
                Some(p) if constraint.allowed_types().contains(&p) => {}
                _ => return Err(could_not_unify(var_ty, resolved)),
            },
            _ => return Err(could_not_unify(var_ty, resolved)),
        }
        self.vars[var_id as usize] = TyVarState::Link { ty: resolved };
        Ok(())
    }

    /// If `ty` is, or can become, a function of the given arity, return its
    /// parameter and return types. An unbound var goes through `unify` rather
    /// than a raw `Link` write, so the fresh vars are lowered to the var's own
    /// level and any constraint on it is enforced (a numeric var is not a
    /// function). Wrong arity yields `IncorrectArity` carrying the real
    /// params/ret so the caller can keep checking.
    pub fn match_fun_type(
        &mut self,
        ty: Ty,
        arity: usize,
    ) -> Result<(SmallVec<[Ty; 4]>, Ty), MatchFunTypeError> {
        let resolved = self.find(ty);
        if let TypeNode::Var(id) = self.node(resolved) {
            return match self.root_var(id) {
                RootVarState::Unbound { .. } => {
                    let params: SmallVec<[Ty; 4]> = (0..arity).map(|_| self.fresh_var()).collect();
                    let ret = self.fresh_var();
                    let fn_ty = self.mk_fun(&params, ret);
                    match self.unify(resolved, fn_ty) {
                        Ok(()) => Ok((params, ret)),
                        Err(_) => Err(MatchFunTypeError::NotFn { ty: resolved }),
                    }
                }
                RootVarState::Generic { .. } => Err(MatchFunTypeError::NotFn { ty: resolved }),
            };
        }
        if let TypeNode::Fun { params, ret } = self.node(resolved) {
            let params: SmallVec<[Ty; 4]> = self.children_of(params).into();
            if params.len() != arity {
                return Err(MatchFunTypeError::IncorrectArity {
                    expected: params.len(),
                    given: arity,
                    params,
                    ret,
                });
            }
            return Ok((params, ret));
        }
        Err(MatchFunTypeError::NotFn { ty: resolved })
    }

    /// Flip every Unbound var at level > current to Generic and quantify over
    /// them. Display names are assigned here so they stay stable across every
    /// later printing of the scheme.
    pub fn generalize(&mut self, ty: Ty) -> Scheme {
        self.generalize_impl(ty, false, ValueKind::Local)
    }

    /// Module-scope generalization: every remaining Unbound var becomes
    /// Generic regardless of level. Run once a top-level body is fully
    /// inferred, so its scheme is closed.
    pub fn generalize_top(&mut self, ty: Ty) -> Scheme {
        self.generalize_impl(ty, true, ValueKind::ModuleFn)
    }

    fn generalize_impl(&mut self, ty: Ty, ignore_level: bool, kind: ValueKind) -> Scheme {
        let mut ids: IndexSet<i32> = IndexSet::new();
        self.collect_generalizable(ty, ignore_level, &mut ids);
        self.assign_names(&ids);
        // Close the scheme: each generalized Var becomes a Bound index. After
        // this the type holds no engine-local reference, so the scheme can move
        // between engines without dragging `vars`.
        let quantified: Vec<QuantVar> = ids
            .iter()
            .map(|id| {
                let constraint = match self.root_var(*id) {
                    RootVarState::Generic { constraint, .. }
                    | RootVarState::Unbound { constraint, .. } => constraint,
                };
                let name = self.var_names.get(id).copied().unwrap_or(StrId::NONE);
                QuantVar {
                    constraint,
                    name,
                    origin_id: Some(*id),
                }
            })
            .collect();
        let closed_ty = self.close_over(ty, &ids);
        Scheme {
            quantified: self.push_quants(&quantified),
            ty: closed_ty,
            kind,
            def: None,
        }
    }

    /// Replace every `Var(id)` whose representative is a `Generic` in `ids`
    /// with `Bound(idx)` of its position.
    fn close_over(&mut self, ty: Ty, ids: &IndexSet<i32>) -> Ty {
        self.rewrite(ty, &mut |e, n| match n {
            TypeNode::Var(id) => match e.root_var(id) {
                RootVarState::Generic { id: gid, .. } => {
                    ids.get_index_of(&gid).map(|ix| e.mk_bound(ix as u32))
                }
                RootVarState::Unbound { .. } => None,
            },
            _ => None,
        })
    }

    /// Structural type rewrite: `leaf` gets first refusal at every node, and
    /// Con/Fun/Tuple are otherwise rebuilt from rewritten children. Shared
    /// spine of close_over / open_with / substitute_type_vars / close_body,
    /// which differ only at the leaves. Unchanged subtrees are returned as-is.
    fn rewrite(&mut self, ty: Ty, leaf: &mut impl FnMut(&mut Self, TypeNode) -> Option<Ty>) -> Ty {
        self.rewrite_node(ty, leaf).0
    }

    /// Core of `rewrite`. The bool says whether the result differs from `ty`.
    /// Resolving a union-find link counts as a change, so parents rebuild with
    /// resolved children and the output never contains live link-vars.
    fn rewrite_node(
        &mut self,
        ty: Ty,
        leaf: &mut impl FnMut(&mut Self, TypeNode) -> Option<Ty>,
    ) -> (Ty, bool) {
        let r = self.find(ty);
        let n = self.node(r);
        if let Some(out) = leaf(self, n) {
            return (out, out != ty);
        }
        match n {
            TypeNode::Var(_) | TypeNode::Bound(_) => (r, r != ty),
            TypeNode::Con { args, .. } if args.len == 0 => (r, r != ty),
            TypeNode::Con { id, name, args } => match self.rewrite_children(args, leaf) {
                Some(kids) => (self.mk_con_id(id, name, &kids), true),
                None => (r, r != ty),
            },
            TypeNode::Fun { params, ret } => {
                let kids = self.rewrite_children(params, leaf);
                let (new_ret, ret_changed) = self.rewrite_node(ret, leaf);
                match kids {
                    Some(kids) => (self.mk_fun(&kids, new_ret), true),
                    // Params untouched: the arena is append-only, so their
                    // existing child slice stays valid.
                    None if ret_changed => (
                        self.push(TypeNode::Fun {
                            params,
                            ret: new_ret,
                        }),
                        true,
                    ),
                    None => (r, r != ty),
                }
            }
            TypeNode::Tuple { elems } => match self.rewrite_children(elems, leaf) {
                Some(kids) => (self.mk_tuple(&kids), true),
                None => (r, r != ty),
            },
        }
    }

    /// Rewrite each child of `sl`, returning `None` when every child came back
    /// unchanged. The result buffer is only materialised once a child changes.
    fn rewrite_children(
        &mut self,
        sl: ArenaSlice<pool::Children>,
        leaf: &mut impl FnMut(&mut Self, TypeNode) -> Option<Ty>,
    ) -> Option<SmallVec<[Ty; 4]>> {
        let mut kids: Option<SmallVec<[Ty; 4]>> = None;
        for (off, i) in sl.range().enumerate() {
            let k = self.children[i];
            let (rk, changed) = self.rewrite_node(k, leaf);
            if let Some(kids) = &mut kids {
                kids.push(rk);
            } else if changed {
                let mut v: SmallVec<[Ty; 4]> = SmallVec::with_capacity(sl.len as usize);
                let start = sl.start as usize;
                v.extend_from_slice(&self.children[start..start + off]);
                v.push(rk);
                kids = Some(v);
            }
        }
        kids
    }

    fn assign_names(&mut self, quantified: &IndexSet<i32>) {
        let mut taken: HashSet<StrId> = HashSet::new();
        for qvar in quantified {
            if let Some(&name) = self.var_names.get(qvar) {
                taken.insert(name);
            }
        }
        for qvar in quantified {
            if self.var_names.contains_key(qvar) {
                continue;
            }
            let sid = self.mint_display_name(&mut taken);
            self.var_names.insert(*qvar, sid);
        }
    }

    fn collect_generalizable(
        &mut self,
        ty: Ty,
        ignore_level: bool,
        quantified: &mut IndexSet<i32>,
    ) {
        let r = self.find(ty);
        match self.node(r) {
            TypeNode::Var(id) => match self.root_var(id) {
                RootVarState::Unbound { level, constraint }
                    if ignore_level || level > self.current_level =>
                {
                    self.vars[id as usize] = TyVarState::Generic { id, constraint };
                    quantified.insert(id);
                }
                RootVarState::Generic { id: gid, .. } => {
                    quantified.insert(gid);
                }
                _ => {}
            },
            TypeNode::Con { args, .. } => self.collect_slice(args, ignore_level, quantified),
            TypeNode::Fun { params, ret } => {
                self.collect_slice(params, ignore_level, quantified);
                self.collect_generalizable(ret, ignore_level, quantified);
            }
            TypeNode::Tuple { elems } => self.collect_slice(elems, ignore_level, quantified),
            TypeNode::Bound(_) => {}
        }
    }

    fn collect_slice(
        &mut self,
        sl: ArenaSlice<pool::Children>,
        ignore_level: bool,
        quantified: &mut IndexSet<i32>,
    ) {
        for i in sl.range() {
            let k = self.children[i];
            self.collect_generalizable(k, ignore_level, quantified);
        }
    }

    // --- Instantiation ---

    pub fn instantiate(&mut self, scheme: &Scheme, rigid_ids: &HashSet<i32>) -> Ty {
        if scheme.quantified.len == 0 {
            return scheme.ty;
        }
        // One slot per bound var: the original rigid Var when this is a
        // recursive self-reference, otherwise a fresh Unbound carrying the
        // constraint and display name.
        let start = scheme.quantified.start as usize;
        let count = scheme.quantified.len as usize;
        let mut stack = [Ty(0); 4];
        let mut heap: Vec<Ty>;
        let subst: &mut [Ty] = if count <= stack.len() {
            &mut stack[..count]
        } else {
            heap = vec![Ty(0); count];
            &mut heap[..]
        };
        for (i, slot) in subst.iter_mut().enumerate() {
            let q = self.quants[start + i];
            *slot = if let Some(id) = q.origin_id
                && rigid_ids.contains(&id)
            {
                self.push(TypeNode::Var(id))
            } else {
                let fresh = match q.constraint {
                    Some(c) => self.fresh_constrained_var(c),
                    None => self.fresh_var(),
                };
                if q.name != StrId::NONE {
                    self.name_var(fresh, q.name);
                }
                fresh
            };
        }
        self.open_with(scheme.ty, subst)
    }

    fn open_with(&mut self, ty: Ty, subst: &[Ty]) -> Ty {
        self.rewrite(ty, &mut |_, n| match n {
            TypeNode::Bound(i) => Some(subst[i as usize]),
            _ => None,
        })
    }

    // --- Type-parameter substitution / closing ---

    /// Substitute a `TypeInfo` body template's type parameters at `args`,
    /// positional in `params` order. A template references a parameter either
    /// as `Var(params[i].id)` (open) or `Bound(i)` (closed, after
    /// `close_body`); both resolve to `args[i]`.
    pub fn substitute_type_vars(
        &mut self,
        ty: Ty,
        params: ArenaSlice<pool::TypeParams>,
        args: &[Ty],
    ) -> Ty {
        self.rewrite(ty, &mut |e, n| match n {
            TypeNode::Var(id) => e
                .type_params_of(params)
                .iter()
                .position(|p| p.id == id)
                .and_then(|i| args.get(i).copied()),
            TypeNode::Bound(i) => args.get(i as usize).copied(),
            _ => None,
        })
    }

    /// As `substitute_type_vars`, but the arguments stay in the children pool,
    /// so an `ArenaSlice` caller needs no temporary `Vec<Ty>`.
    fn substitute_type_vars_slice(
        &mut self,
        ty: Ty,
        params: ArenaSlice<pool::TypeParams>,
        args: ArenaSlice<pool::Children>,
    ) -> Ty {
        self.rewrite(ty, &mut |e, n| match n {
            TypeNode::Var(id) => {
                let idx = e.type_params_of(params).iter().position(|p| p.id == id);
                idx.and_then(|i| e.children_of(args).get(i).copied())
            }
            TypeNode::Bound(i) => e.children_of(args).get(i as usize).copied(),
            _ => None,
        })
    }

    /// Rewrite a `TypeInfo` body from `Var(param_id)` to `Bound(idx)` so it no
    /// longer references engine-local var ids. Idempotent.
    pub fn close_body(&mut self, ty: Ty, params: ArenaSlice<pool::TypeParams>) -> Ty {
        self.rewrite(ty, &mut |e, n| match n {
            TypeNode::Var(id) => e
                .type_params_of(params)
                .iter()
                .position(|p| p.id == id)
                .map(|i| e.mk_bound(i as u32)),
            _ => None,
        })
    }

    // --- Resolution: Ty -> type_def::Type ---

    pub fn resolve(&mut self, ty: Ty, env: Option<&TypeEnv>) -> Type {
        let mut path = ResolvePath::default();
        self.resolve_inner(ty, env, &mut path)
    }

    fn resolve_inner(&mut self, ty: Ty, env: Option<&TypeEnv>, path: &mut ResolvePath) -> Type {
        let r = self.find(ty);
        match self.node(r) {
            TypeNode::Var(id) => {
                let constraint = match self.root_var(id) {
                    RootVarState::Unbound { constraint, .. }
                    | RootVarState::Generic { constraint, .. } => constraint,
                };
                if let Some(c) = constraint {
                    return t_var(c.name());
                }
                t_var(self.var_display_name(id))
            }
            TypeNode::Con { id, name, args } => {
                let name = self.str(name).to_string();
                self.resolve_icon(id, &name, args, env, path)
            }
            TypeNode::Fun { params, ret } => {
                let params: Vec<Type> = params
                    .range()
                    .map(|i| {
                        let p = self.children[i];
                        self.resolve_inner(p, env, path)
                    })
                    .collect();
                let ret_t = self.resolve_inner(ret, env, path);
                Type::Function {
                    params,
                    ret: Box::new(ret_t),
                }
            }
            TypeNode::Tuple { elems } => {
                let elements: Vec<Type> = elems
                    .range()
                    .map(|i| {
                        let e = self.children[i];
                        self.resolve_inner(e, env, path)
                    })
                    .collect();
                t_tuple(elements)
            }
            TypeNode::Bound(i) => t_var(bound_letter(i)),
        }
    }

    fn resolve_icon(
        &mut self,
        id: TypeId,
        name: &str,
        args: ArenaSlice<pool::Children>,
        env: Option<&TypeEnv>,
        path: &mut ResolvePath,
    ) -> Type {
        // Int/Float/String are declared external in the prelude, but must
        // resolve to `Primitive` so exhaustiveness treats them as having
        // infinitely many constructors. `Array` resolves structurally so
        // `[h, ..t]` patterns work. `Bool` deliberately falls through to the
        // env lookup: it is a real two-variant `Named` type.
        match self.as_prim(id) {
            Some(Prim::Int) => return t_int(),
            Some(Prim::Float) => return t_float(),
            Some(Prim::String) => return t_string(),
            None => {}
        }
        if id == self.prim_ids.array {
            let elem = args
                .range()
                .next()
                .map(|i| {
                    let a = self.children[i];
                    self.resolve_inner(a, env, path)
                })
                .unwrap_or_else(|| t_var("a"));
            return t_array(elem);
        }

        // By nominal id, never by name: a same-named type from whatever file
        // the LSP analysed last must not answer for this one.
        let info = env.and_then(|e| e.lookup_type_info_by_id(id));

        // The recursion guard keys on the resolved instance (id plus argument
        // shape), not the bare id. Keying on the id alone takes the inner
        // `Option` of `Option(Option(Int))` for a recursive occurrence, leaves
        // it variant-less, and reports nested matches as non-exhaustive.
        // Recursion flows through variant fields, never arguments, so the
        // arguments only need the path so far.
        let mark = path.stack.len();
        let resolved_args: Vec<Type> = args
            .range()
            .map(|i| {
                let a = self.children[i];
                let r = self.resolve_inner(a, env, path);
                path.stack.truncate(mark);
                r
            })
            .collect();

        let args_key = type_args_key(&resolved_args);

        if let Some(info) = info
            && !path.would_recurse(id, &args_key)
        {
            match info.body {
                TypeBody::Custom { variants } => {
                    let vs: Vec<Variant> = self.variants_of(variants).to_vec();
                    path.enter(id, args_key);
                    let variants: IndexMap<String, Vec<FieldDef>> = vs
                        .into_iter()
                        .map(|v| {
                            let fields: Vec<VariantField> =
                                self.variant_fields_of(v.fields).to_vec();
                            (
                                self.str(v.name).to_string(),
                                fields
                                    .into_iter()
                                    .map(|f| {
                                        let s = self.substitute_type_vars_slice(
                                            f.ty,
                                            info.type_params,
                                            args,
                                        );
                                        FieldDef {
                                            label: self.str(f.label).to_string(),
                                            ty: self.resolve_inner(s, env, path),
                                        }
                                    })
                                    .collect(),
                            )
                        })
                        .collect();
                    path.exit();
                    return Type::Named {
                        id,
                        name: name.to_string(),
                        type_args: resolved_args,
                        variants,
                    };
                }
                TypeBody::Alias { target } => {
                    let s = self.substitute_type_vars_slice(target, info.type_params, args);
                    path.enter(id, args_key);
                    let resolved = self.resolve_inner(s, env, path);
                    path.exit();
                    return resolved;
                }
                TypeBody::Unresolved | TypeBody::External => {}
            }
        }

        Type::Named {
            id,
            name: name.to_string(),
            type_args: resolved_args,
            variants: IndexMap::new(),
        }
    }

    // --- Error helpers ---

    pub fn error_at_span(&mut self, message: String, s: Span) {
        self.diagnostics
            .push(Diagnostic::error(s, DiagnosticCode::TypeError, message));
    }

    pub fn type_to_str(&mut self, ty: Ty) -> String {
        self.resolve(ty, None).to_string()
    }
}

/// How many times one nominal type may re-occur along a `resolve` path before
/// the guard cuts off. A genuine recursive occurrence is caught by matching
/// instance keys; this bound also terminates non-uniform recursive types like
/// `type Nest(t) { More(Nest((t, t))) Done }`, whose argument grows forever.
/// Cutting off only makes exhaustiveness stricter, never unsound.
const MAX_NOMINAL_RECURRENCE: usize = 16;

/// The nominal-type instances being expanded on the active `resolve` path.
/// Pairing the id with a key for its arguments is what tells a finite
/// re-nesting apart from a true recursive occurrence.
#[derive(Default)]
struct ResolvePath {
    stack: Vec<(TypeId, String)>,
}

impl ResolvePath {
    /// Whether that exact instance is already on the path, or this nominal type
    /// has recurred [`MAX_NOMINAL_RECURRENCE`] times without repeating.
    fn would_recurse(&self, id: TypeId, args_key: &str) -> bool {
        let mut count = 0usize;
        for (i, k) in &self.stack {
            if *i == id {
                if k == args_key {
                    return true;
                }
                count += 1;
            }
        }
        count >= MAX_NOMINAL_RECURRENCE
    }

    fn enter(&mut self, id: TypeId, args_key: String) {
        self.stack.push((id, args_key));
    }

    fn exit(&mut self) {
        self.stack.pop();
    }
}

/// Structural key identifying a nominal-type instance in [`ResolvePath`]. It
/// must not descend into a `Named`'s variant table: `(id, type_args)` already
/// pins the instance, and the variants are what resolution is mid-computing.
/// Type variables collapse to one token, which keeps the guard conservative.
fn type_args_key(args: &[Type]) -> String {
    let mut out = String::new();
    for a in args {
        write_type_key(a, &mut out);
        out.push(',');
    }
    out
}

fn write_type_key(t: &Type, out: &mut String) {
    match t {
        Type::Primitive { kind } => out.push(match kind {
            PrimitiveKind::Int => 'i',
            PrimitiveKind::Float => 'f',
            PrimitiveKind::String => 's',
        }),
        Type::Var { .. } => out.push('_'),
        Type::Array { element } => {
            out.push('[');
            write_type_key(element, out);
            out.push(']');
        }
        Type::Tuple { elements } => {
            out.push('(');
            for e in elements {
                write_type_key(e, out);
                out.push(',');
            }
            out.push(')');
        }
        Type::Function { params, ret } => {
            out.push('{');
            for p in params {
                write_type_key(p, out);
                out.push(',');
            }
            out.push_str("->");
            write_type_key(ret, out);
            out.push('}');
        }
        Type::Named { id, type_args, .. } => {
            out.push('N');
            out.push_str(&id.0.to_string());
            out.push('<');
            for a in type_args {
                write_type_key(a, out);
                out.push(',');
            }
            out.push('>');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::type_def::{t_array, t_int, t_string};

    fn no_rigid() -> HashSet<i32> {
        HashSet::new()
    }

    /// Mint a rigid generic carrying constraint `c`, as `generalize` produces
    /// from a constrained body var.
    fn constrained_generic(e: &mut InferEngine, c: Constraint) -> Ty {
        e.enter_level();
        let v = e.fresh_constrained_var(c);
        e.leave_level();
        let _ = e.generalize(v);
        v
    }

    #[track_caller]
    fn assert_cannot_unify(e: &mut InferEngine, a: Ty, b: Ty) {
        assert!(matches!(
            e.unify(a, b),
            Err(UnifyError::CouldNotUnify { .. })
        ));
    }

    #[test]
    fn next_letter_sequence() {
        let mut uid = 0u64;
        assert_eq!(next_letter(&mut uid), "a");
        assert_eq!(next_letter(&mut uid), "b");
        let mut uid = 25u64;
        assert_eq!(next_letter(&mut uid), "z");
        assert_eq!(next_letter(&mut uid), "aa");
        assert_eq!(next_letter(&mut uid), "ab");
    }

    #[test]
    fn identity_generalizes_to_forall_a() {
        let mut e = new_engine();
        e.enter_level();
        let x = e.fresh_var();
        let id_ty = e.mk_fun(&[x], x);
        e.leave_level();
        let scheme = e.generalize(id_ty);
        assert_eq!(scheme.quantified.len, 1);
        assert_eq!(e.type_to_str(scheme.ty), "fn(a) a");
    }

    #[test]
    fn let_polymorphism() {
        let mut e = new_engine();
        e.enter_level();
        let x = e.fresh_var();
        let id_ty = e.mk_fun(&[x], x);
        e.leave_level();
        let scheme = e.generalize(id_ty);

        let inst1 = e.instantiate(&scheme, &no_rigid());
        let int_t = e.icon_int();
        let TypeNode::Fun { params, ret } = e.node(inst1) else {
            panic!("expected Fun")
        };
        assert!(e.unify(e.children_of(params)[0], int_t).is_ok());
        assert_eq!(e.type_to_str(ret), "Int");

        let inst2 = e.instantiate(&scheme, &no_rigid());
        let str_t = e.icon_string();
        let TypeNode::Fun { params, ret } = e.node(inst2) else {
            panic!("expected Fun")
        };
        assert!(e.unify(e.children_of(params)[0], str_t).is_ok());
        assert_eq!(e.type_to_str(ret), "String");

        assert!(e.diagnostics.is_empty());
    }

    #[test]
    fn occurs_check_rejects_infinite_type() {
        let mut e = new_engine();
        let a = e.fresh_var();
        let b = e.fresh_var();
        let fn_ty = e.mk_fun(&[a], b);
        let res = e.unify(a, fn_ty);
        assert!(matches!(res, Err(UnifyError::RecursiveType)));
    }

    #[test]
    fn numeric_constraint_rejects_string() {
        let mut e = new_engine();
        let n = e.fresh_constrained_var(Constraint::Numeric);
        let s = e.icon_string();
        assert_cannot_unify(&mut e, n, s);
    }

    #[test]
    fn numeric_constraint_accepts_int() {
        let mut e = new_engine();
        let n = e.fresh_constrained_var(Constraint::Numeric);
        let i = e.icon_int();
        assert!(e.unify(n, i).is_ok());
        assert!(e.diagnostics.is_empty());
        assert_eq!(e.type_to_str(n), "Int");
    }

    #[test]
    fn addable_intersect_numeric_yields_numeric() {
        let mut e = new_engine();
        let a = e.fresh_constrained_var(Constraint::Addable);
        let n = e.fresh_constrained_var(Constraint::Numeric);
        assert!(e.unify(a, n).is_ok());
        assert_eq!(e.type_to_str(a), "numeric");
    }

    #[test]
    fn constraint_not_dropped_against_unconstrained_generic() {
        // Linking would discard the requirement, letting a function do
        // arithmetic on an annotated polymorphic param.
        let mut e = new_engine();
        let n = e.fresh_constrained_var(Constraint::Numeric);
        let (g, _) = e.fresh_generic_var();
        assert_cannot_unify(&mut e, n, g);
    }

    #[test]
    fn constraint_not_dropped_when_generic_is_expected() {
        // Symmetric path: the generic on the `expected` side.
        let mut e = new_engine();
        let (g, _) = e.fresh_generic_var();
        let a = e.fresh_constrained_var(Constraint::Addable);
        assert_cannot_unify(&mut e, g, a);
    }

    #[test]
    fn constrained_var_links_to_satisfying_generic() {
        // Int/Float is a subset of Int/Float/String, so numeric guarantees
        // addable.
        let mut e = new_engine();
        let numeric_generic = constrained_generic(&mut e, Constraint::Numeric);
        let addable = e.fresh_constrained_var(Constraint::Addable);
        assert!(e.unify(addable, numeric_generic).is_ok());
        assert!(e.diagnostics.is_empty());
    }

    #[test]
    fn constrained_var_rejects_insufficient_generic() {
        // An addable generic may still be a String.
        let mut e = new_engine();
        let addable_generic = constrained_generic(&mut e, Constraint::Addable);
        let numeric = e.fresh_constrained_var(Constraint::Numeric);
        assert_cannot_unify(&mut e, numeric, addable_generic);
    }

    #[test]
    fn unify_mismatched_cons_produces_error() {
        let mut e = new_engine();
        let i = e.icon_int();
        let s = e.icon_string();
        assert_cannot_unify(&mut e, i, s);
    }

    #[test]
    fn unify_at_pushes_diagnostic() {
        let mut e = new_engine();
        let i = e.icon_int();
        let s = e.icon_string();
        assert!(!e.unify_at(i, s, Span::point(1, 1)));
        assert_eq!(e.diagnostics.len(), 1);
        assert!(e.diagnostics[0].message.contains("Type mismatch"));
    }

    #[test]
    fn instantiate_gives_fresh_vars() {
        let mut e = new_engine();
        e.enter_level();
        let x = e.fresh_var();
        e.leave_level();
        let scheme = e.generalize(x);
        let i1 = e.instantiate(&scheme, &no_rigid());
        let i2 = e.instantiate(&scheme, &no_rigid());
        let int_t = e.icon_int();
        let str_t = e.icon_string();
        assert!(e.unify(i1, int_t).is_ok());
        assert!(e.unify(i2, str_t).is_ok());
        assert!(e.diagnostics.is_empty());
    }

    #[test]
    fn rigid_generic_stays_rigid() {
        let mut e = new_engine();
        e.enter_level();
        let x = e.fresh_var();
        e.leave_level();
        let scheme = e.generalize(x);
        let rigid: HashSet<i32> = e
            .quants_of(scheme.quantified)
            .iter()
            .filter_map(|q| q.origin_id)
            .collect();
        let inst = e.instantiate(&scheme, &rigid);
        let int_t = e.icon_int();
        assert!(e.unify(inst, int_t).is_err());
    }

    #[test]
    fn generalize_top_ignores_level() {
        let mut e = new_engine();
        let x = e.fresh_var();
        let scheme = e.generalize_top(x);
        assert_eq!(scheme.quantified.len, 1);
        assert!(matches!(scheme.kind, ValueKind::ModuleFn));
    }

    fn assert_no_live_vars(e: &InferEngine, ty: Ty) {
        match e.node(ty) {
            TypeNode::Var(id) => panic!("live Var({id}) survived close_over"),
            TypeNode::Bound(_) => {}
            TypeNode::Con { args, .. } => {
                for i in args.range() {
                    assert_no_live_vars(e, e.children[i]);
                }
            }
            TypeNode::Fun { params, ret } => {
                for i in params.range() {
                    assert_no_live_vars(e, e.children[i]);
                }
                assert_no_live_vars(e, ret);
            }
            TypeNode::Tuple { elems } => {
                for i in elems.range() {
                    assert_no_live_vars(e, e.children[i]);
                }
            }
        }
    }

    #[test]
    fn generalize_resolves_linked_vars_in_scheme() {
        // Stored Schemes outlive `truncate_to`'s `vars.clear()`, so a surviving
        // var id would read an unrelated fresh var on the next compile.
        let mut e = new_engine();
        e.enter_level();
        let p = e.fresh_var();
        let r = e.fresh_var();
        let int_t = e.icon_int();
        e.unify(p, int_t).unwrap();
        e.unify(r, int_t).unwrap();
        let f = e.mk_fun(&[p], r);
        e.leave_level();
        let scheme = e.generalize_top(f);
        assert_no_live_vars(&e, scheme.ty);
    }

    #[test]
    fn generalize_resolves_linked_vars_among_unchanged_siblings() {
        // The rebuild must not re-copy the original link-bearing child slots.
        let mut e = new_engine();
        e.enter_level();
        let v = e.fresh_var();
        let int_t = e.icon_int();
        e.unify(v, int_t).unwrap();
        let str_t = e.icon_string();
        let tup = e.mk_tuple(&[str_t, v]);
        let tup2 = e.mk_tuple(&[v, str_t]);
        e.leave_level();
        let s1 = e.generalize_top(tup);
        let s2 = e.generalize_top(tup2);
        assert_no_live_vars(&e, s1.ty);
        assert_no_live_vars(&e, s2.ty);
    }

    #[test]
    fn constrained_generic_reinstantiates_constrained() {
        let mut e = new_engine();
        e.enter_level();
        let n = e.fresh_constrained_var(Constraint::Numeric);
        e.leave_level();
        let scheme = e.generalize(n);
        let inst = e.instantiate(&scheme, &no_rigid());
        let s = e.icon_string();
        assert!(e.unify(inst, s).is_err());
        let inst2 = e.instantiate(&scheme, &no_rigid());
        let i = e.icon_int();
        assert!(e.unify(inst2, i).is_ok());
    }

    #[test]
    fn match_fun_type_on_unbound() {
        let mut e = new_engine();
        let v = e.fresh_var();
        let (params, _ret) = e.match_fun_type(v, 2).expect("should pre-link");
        assert_eq!(params.len(), 2);
        let resolved = e.find(v);
        assert!(matches!(e.node(resolved), TypeNode::Fun { .. }));
    }

    #[test]
    fn match_fun_type_rejects_constrained_var() {
        // A numeric/addable var used as a callee must be a type error, not a
        // silent link to a Fun type that erases the constraint.
        let mut e = new_engine();
        let n = e.fresh_constrained_var(Constraint::Numeric);
        assert!(matches!(
            e.match_fun_type(n, 1),
            Err(MatchFunTypeError::NotFn { .. })
        ));
    }

    #[test]
    fn match_fun_type_binds_fresh_vars_at_callee_level() {
        // Calling through a var bound at an outer let level from a deeper
        // scope: the fresh param/ret vars must be lowered to the callee's
        // level, so generalizing back at that level does not quantify them.
        let mut e = new_engine();
        e.enter_level();
        let callee = e.fresh_var();
        e.enter_level();
        let (params, ret) = e.match_fun_type(callee, 1).expect("should bind");
        e.leave_level();
        let param_scheme = e.generalize(params[0]);
        assert_eq!(
            param_scheme.quantified.len, 0,
            "fresh param var tied to outer callee must not be generalized"
        );
        let ret_scheme = e.generalize(ret);
        assert_eq!(ret_scheme.quantified.len, 0);
    }

    #[test]
    fn match_fun_type_arity_mismatch() {
        let mut e = new_engine();
        let i = e.icon_int();
        let f = e.mk_fun(&[i], i);
        let res = e.match_fun_type(f, 2);
        assert!(matches!(
            res,
            Err(MatchFunTypeError::IncorrectArity {
                expected: 1,
                given: 2,
                ..
            })
        ));
    }

    #[test]
    fn match_fun_type_not_fn() {
        let mut e = new_engine();
        let i = e.icon_int();
        assert!(matches!(
            e.match_fun_type(i, 1),
            Err(MatchFunTypeError::NotFn { .. })
        ));
    }

    #[test]
    fn flip_swaps_expected_given() {
        let mut e = new_engine();
        let i = e.icon_int();
        let s = e.icon_string();
        let err = UnifyError::CouldNotUnify {
            expected: i,
            given: s,
            situation: None,
        };
        if let UnifyError::CouldNotUnify {
            expected, given, ..
        } = err.flip()
        {
            assert_eq!(expected, s);
            assert_eq!(given, i);
        } else {
            panic!();
        }
    }

    #[test]
    fn resolve_primitives() {
        let mut e = new_engine();
        let i = e.icon_int();
        assert_eq!(e.resolve(i, None), t_int());
        let s = e.icon_string();
        let arr_id = e.prim_ids.array;
        let arr = e.mk_con(arr_id, pn::ARRAY, &[s]);
        assert_eq!(e.resolve(arr, None), t_array(t_string()));
    }

    #[test]
    fn resolve_unbound_var_gets_base26_name() {
        let mut e = new_engine();
        let v = e.fresh_var();
        assert_eq!(e.resolve(v, None), t_var("a"));
        assert_eq!(e.resolve(v, None), t_var("a"));
        let v2 = e.fresh_var();
        assert_eq!(e.resolve(v2, None), t_var("b"));
    }

    #[test]
    fn unify_adjusts_levels_to_prevent_unsound_generalization() {
        let mut e = new_engine();
        e.enter_level();
        let outer = e.fresh_var();
        e.enter_level();
        let inner = e.fresh_var();
        assert!(e.unify(inner, outer).is_ok());
        e.leave_level();
        let scheme = e.generalize(inner);
        assert_eq!(
            scheme.quantified.len, 0,
            "var tied to outer scope must not be generalized"
        );
    }

    #[test]
    fn unify_adjusts_levels_through_constructors() {
        let mut e = new_engine();
        e.enter_level();
        let outer = e.fresh_var();
        e.enter_level();
        let inner = e.fresh_var();
        let arr_inner = e.mk_con(TypeId(-7), pn::ARRAY, &[inner]);
        let arr_outer = e.mk_con(TypeId(-7), pn::ARRAY, &[outer]);
        assert!(e.unify(arr_inner, arr_outer).is_ok());
        e.leave_level();
        let scheme = e.generalize(inner);
        assert_eq!(scheme.quantified.len, 0);
    }
}
