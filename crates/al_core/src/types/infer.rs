use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;

use super::environment::{DefinitionLocation, TypeBody, TypeEnv, TypeParam, Variant, VariantField};
use crate::diagnostic::Diagnostic;
use crate::span::Span;
use crate::type_def::{
    FieldDef, PrimitiveKind, Type, TypeId, prim_names as pn, t_array, t_float, t_int, t_string,
    t_tuple, t_var,
};

// ============================================================================
// Constraints (Elm-style constrained type variables)
// ============================================================================

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

    /// Whether a rigid generic carrying `generic` already satisfies *this*
    /// requirement. True iff every concrete type the generic admits is also
    /// admitted here (the generic's allowed set ⊆ this constraint's), so a
    /// constrained var may safely link to the generic without discarding the
    /// constraint. A generic with no constraint satisfies nothing: an
    /// unconstrained rigid type variable carries no numeric/addable guarantee.
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

/// A concrete primitive type the inference engine has resolved a `Ty` to.
/// Used by codegen to pick type-specialized opcodes; unbound/constrained
/// vars and every other constructor map to `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prim {
    Int,
    Float,
    String,
}

// ============================================================================
// Ty / TypeNode — arena-based type representation
// ============================================================================

/// Index into `InferEngine.nodes`. A `Ty` is meaningful only relative to the
/// engine that minted it (or, for static-stdlib types, the engine seeded from
/// those static arrays). Unlike the previous owned `InferType` tree, `Ty` is
/// `Copy` so threading types through inference is pointer-sized everywhere.
pub type Ty = u32;

/// Index into `InferEngine.strings`.
pub type StrId = u32;

/// Sentinel `StrId` meaning "no string". Used where a field is logically
/// `Option<String>` but the struct must stay `Copy` and const-constructible.
pub const NO_STR: StrId = u32::MAX;

/// Half-open `[start, start+len)` into `InferEngine.children`. `len` is `u16`
/// — no AL type has more than 65 535 type-arguments, parameters, or tuple
/// elements; keeping `TypeNode` at 12 bytes matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ArenaSlice {
    pub start: u32,
    pub len: u16,
}

impl ArenaSlice {
    pub const EMPTY: ArenaSlice = ArenaSlice { start: 0, len: 0 };
    #[inline]
    pub fn range(self) -> std::ops::Range<usize> {
        self.start as usize..(self.start as usize + self.len as usize)
    }
}

/// One node of a type, stored flat in the engine's arena. `Copy` and
/// const-constructible so the static stdlib emits a `&'static [TypeNode]`
/// directly — there is no separate "static IR" mirror.
#[derive(Debug, Clone, Copy)]
pub enum TypeNode {
    /// A unification variable, indexing into `InferEngine.vars`. Only valid
    /// within the engine that minted it.
    Var(i32),
    /// A bound (quantified) variable, indexing into the enclosing
    /// `Scheme.quantified`. Appears only inside a `Scheme.ty` — `instantiate`
    /// substitutes every `Bound` away before the type enters live inference,
    /// so `unify`/`find`/`occurs` never see one.
    Bound(u32),
    /// `Name(args...)` — a nominal type application.
    ///
    /// `id` is the type's registered nominal identity (`TypeInfo.id`,
    /// allocated once per declaration); `name` → `engine.strings` is carried
    /// for display only. Unification and every semantic lookup
    /// (exhaustiveness, field access, Option/Result detection) go through
    /// `id`, never the name: two types that happen to share a name — a user's
    /// `type Parsed` next to `al/http/h1.Parsed` — are different types and
    /// must neither unify nor answer for each other's variants.
    Con {
        id: TypeId,
        name: StrId,
        args: ArenaSlice,
    },
    /// `fn(params...) ret`.
    Fun { params: ArenaSlice, ret: Ty },
    /// `(elems...)`.
    Tuple { elems: ArenaSlice },
}

// ============================================================================
// TyVarState - union-find backing store
// ============================================================================

#[derive(Debug, Clone, Copy)]
enum TyVarState {
    Unbound {
        level: i32,
        constraint: Option<Constraint>,
    },
    /// Substitution edge. `ty` is an arena index, so path compression is a
    /// single `u32` write — no `Rc`/clone.
    Link { ty: Ty },
    /// A rigid quantified variable produced by `generalize`. The `id` is the
    /// originating var id and serves as the stable identity across
    /// instantiation. `constraint` is carried so `instantiate` can re-mint a
    /// constrained fresh var.
    Generic {
        id: i32,
        constraint: Option<Constraint>,
    },
}

// ============================================================================
// ValueKind - what a name in the value environment refers to
// ============================================================================

/// What a value name refers to. Carried on `Scheme` so that resolving an
/// identifier yields both its type and its provenance in one lookup.
///
/// `Copy` so that `Scheme` is `Copy`, which lets the precompiled stdlib emit
/// `&'static [Scheme]` directly with no runtime hydration. Strings are
/// `StrId`s and label lists are `ArenaSlice`s into `InferEngine.str_slices`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ValueKind {
    /// Ordinary `let`/param binding. Never instantiated (mono).
    #[default]
    Local,
    /// Top-level `fn` in a module. Generic; instantiated on use.
    ModuleFn,
    /// VM intrinsic registered from Rust (`println`, `net.listen`, ...).
    /// `op` is the dispatch key for `emit_builtin_op`, decoupled from the
    /// user-facing name so a builtin can be exposed as `net.read` while
    /// dispatching on `"tcp_read"`.
    Builtin { op: StrId },
    /// A data constructor. Carries enough to compile pattern-match and
    /// constructor-call without re-consulting the type env.
    Constructor {
        type_name: StrId,
        type_id: TypeId,
        variant_idx: u16,
        arity: u16,
        /// → `InferEngine.str_slices`
        field_labels: ArenaSlice,
    },
}

// ============================================================================
// Scheme - polymorphic type with quantified variables
// ============================================================================

/// One quantified variable in a `Scheme`. `origin_id` records which engine var
/// it was generalized from so that, while the body that owns those rigid vars
/// is being checked, `instantiate` can hand back the *same* var instead of a
/// fresh one (the `rigid_ids` mechanism). It is engine-local and meaningless
/// after a scheme crosses engines — `None` then, which is correct: a static
/// stdlib scheme is never the body-under-check's own.
#[derive(Debug, Clone, Copy)]
pub struct QuantVar {
    pub constraint: Option<Constraint>,
    /// Display name; `NO_STR` when unset.
    pub name: StrId,
    pub origin_id: Option<i32>,
}

#[derive(Debug, Clone, Copy)]
pub struct Scheme {
    /// Bound variables, in `Bound(i)` index order. → `InferEngine.quants`.
    pub quantified: ArenaSlice,
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

// ============================================================================
// Unification errors
// ============================================================================

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
        Diagnostic::error(span, message)
    }
}

fn could_not_unify(expected: Ty, given: Ty) -> UnifyError {
    UnifyError::CouldNotUnify {
        expected,
        given,
        situation: None,
    }
}

/// Re-express a child unification failure in terms of the enclosing types so
/// the user sees the whole shape, not just the leaf that disagreed.
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
        params: Vec<Ty>,
        ret: Ty,
    },
    NotFn {
        ty: Ty,
    },
}

// ============================================================================
// InferEngine - the HM inference engine
// ============================================================================

#[derive(Debug, Default)]
pub struct InferEngine {
    /// The type arena. `Ty` indexes into this.
    pub nodes: Vec<TypeNode>,
    /// Shared pool for `Con.args`/`Fun.params`/`Tuple.elems`. `ArenaSlice`
    /// indexes into this.
    pub children: Vec<Ty>,
    /// Interned strings. `StrId` indexes into this.
    pub strings: Vec<String>,
    string_intern: HashMap<String, StrId>,

    // ---- Pools backing the `Copy` data carried by `Scheme`/`TypeInfo`.
    // These exist so those structs can be `Copy` and const-constructible
    // (`&'static [Scheme]` in the precompiled stdlib) while still describing
    // variable-length data. All are append-only; indices are stable.
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
    /// Var id -> display name, stored as an interned `StrId` (Copy) so
    /// `instantiate` can record the originating quant's name without
    /// re-allocating the string on every scheme instantiation. Resolved to
    /// `&str` lazily via `str()` at display time.
    var_names: HashMap<i32, StrId>,
    /// Mirror of every value currently in `var_names`. Kept in lockstep with
    /// `var_names` (every insert mirrored, cleared together; no per-key removal
    /// exists) so `var_display_name` can test name collisions in O(1) instead
    /// of rebuilding a `HashSet` from `var_names.values()` on every call.
    used_names: HashSet<StrId>,
    next_name_uid: u64,
    /// Nominal ids of the primitive types the engine itself mints `Con` nodes
    /// for during literal inference (`icon_int`/`icon_float`/`icon_string`).
    /// Set once after the prelude is registered (or seeded) so engine-minted
    /// primitives carry the same identity as compiler-minted ones; defaults
    /// keep engine-only unit tests working without a prelude.
    prim_ids: PrimIds,
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

impl Default for PrimIds {
    fn default() -> Self {
        // Engine-only tests mint primitives before any prelude exists; these
        // placeholder ids are distinct from each other and from the 1-based
        // ids `register_type_head` allocates, and they are overwritten by
        // `set_prim_ids` as soon as a compiler owns the engine.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct EnginePoolWatermark {
    // Field order is significant: derived `Ord` compares lexicographically, and
    // pools grow monotonically together, so an earlier watermark compares `<`
    // a later one on `nodes` first. `ModuleTable::invalidate` relies on `min`
    // over watermarks picking the earliest-compiled module.
    pub nodes: usize,
    pub children: usize,
    pub strings: usize,
    pub quants: usize,
    pub str_slices: usize,
    pub type_params: usize,
    pub variant_fields: usize,
    pub variants: usize,
}

pub fn next_letter(uid: &mut u64) -> String {
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
    ($field:ident : $T:ty => $push:ident, $of:ident) => {
        pub fn $push(&mut self, items: &[$T]) -> ArenaSlice {
            Self::pool_slice(&mut self.$field, items)
        }
        #[inline]
        pub fn $of(&self, sl: ArenaSlice) -> &[$T] {
            &self.$field[sl.range()]
        }
    };
}

impl InferEngine {
    // --- Arena primitives ---

    #[inline]
    pub fn node(&self, t: Ty) -> TypeNode {
        self.nodes[t as usize]
    }

    #[inline]
    pub fn children_of(&self, sl: ArenaSlice) -> &[Ty] {
        &self.children[sl.range()]
    }

    #[inline]
    fn push(&mut self, n: TypeNode) -> Ty {
        let i = self.nodes.len() as Ty;
        self.nodes.push(n);
        i
    }

    fn push_children(&mut self, kids: &[Ty]) -> ArenaSlice {
        let start = self.children.len() as u32;
        self.children.extend_from_slice(kids);
        ArenaSlice {
            start,
            len: kids.len() as u16,
        }
    }

    pub fn intern(&mut self, s: &str) -> StrId {
        if let Some(&i) = self.string_intern.get(s) {
            return i;
        }
        let i = self.strings.len() as StrId;
        self.strings.push(s.to_string());
        self.string_intern.insert(s.to_string(), i);
        i
    }

    #[inline]
    pub fn str(&self, id: StrId) -> &str {
        &self.strings[id as usize]
    }

    // --- Side-pool primitives ---

    fn pool_slice<T: Copy>(pool: &mut Vec<T>, items: &[T]) -> ArenaSlice {
        let start = pool.len() as u32;
        pool.extend_from_slice(items);
        ArenaSlice {
            start,
            len: items.len() as u16,
        }
    }
    side_pool!(quants: QuantVar => push_quants, quants_of);
    side_pool!(str_slices: StrId => push_str_ids, str_ids_of);
    side_pool!(type_params: TypeParam => push_type_params, type_params_of);
    side_pool!(variant_fields: VariantField => push_variant_fields, variant_fields_of);
    side_pool!(variants: Variant => push_variants, variants_of);
    /// Resolve a `field_labels`/`module` slice (a slice of `StrId`s) to owned
    /// strings. Convenience for diagnostic formatting and codegen header
    /// constants; hot paths should iterate `str_ids_of` directly.
    pub fn strs_of(&self, sl: ArenaSlice) -> Vec<String> {
        self.str_ids_of(sl)
            .iter()
            .map(|&i| self.str(i).to_string())
            .collect()
    }
    /// Intern each string and push the ids as a contiguous slice.
    pub fn intern_slice<S: AsRef<str>>(&mut self, ss: &[S]) -> ArenaSlice {
        let start = self.str_slices.len() as u32;
        for s in ss {
            let id = self.intern(s.as_ref());
            self.str_slices.push(id);
        }
        ArenaSlice {
            start,
            len: ss.len() as u16,
        }
    }

    /// Snapshot every append-only pool length so a later `truncate_to` can
    /// roll the engine back to exactly this state. Transient inference state
    /// (`vars`, `var_names`, `next_var_id`, `current_level`, `diagnostics`) is
    /// fully reset rather than length-captured: stored `Scheme`s use `Bound`,
    /// never live `Var`s, so no var-id survives across calls.
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

    pub fn truncate_to(&mut self, w: &EnginePoolWatermark) {
        self.nodes.truncate(w.nodes);
        self.children.truncate(w.children);
        self.strings.truncate(w.strings);
        let n = w.strings as u32;
        self.string_intern.retain(|_, &mut id| id < n);
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
    #[allow(clippy::too_many_arguments)]
    pub fn seed_arena(
        &mut self,
        nodes: &[TypeNode],
        children: &[Ty],
        strings: &[&str],
        quants: &[QuantVar],
        str_slices: &[StrId],
        type_params: &[TypeParam],
        variant_fields: &[VariantField],
        variants: &[Variant],
    ) {
        debug_assert!(self.nodes.is_empty() && self.children.is_empty() && self.strings.is_empty());
        self.nodes.extend_from_slice(nodes);
        self.children.extend_from_slice(children);
        for s in strings {
            self.intern(s);
        }
        self.quants.extend_from_slice(quants);
        self.str_slices.extend_from_slice(str_slices);
        self.type_params.extend_from_slice(type_params);
        self.variant_fields.extend_from_slice(variant_fields);
        self.variants.extend_from_slice(variants);
    }

    /// Wire the nominal ids of Int/Float/String so engine-minted literal
    /// types carry the same identity as compiler-minted ones. Called once by
    /// the compiler right after the prelude is registered or seeded.
    pub fn set_prim_ids(&mut self, ids: PrimIds) {
        self.prim_ids = ids;
    }

    /// Map a nominal type id to the corresponding primitive, if it is one.
    /// Identity is by id, never name — a user's `type Int { }` is not `Int`.
    fn as_prim(&self, id: TypeId) -> Option<Prim> {
        if id == self.prim_ids.int {
            Some(Prim::Int)
        } else if id == self.prim_ids.float {
            Some(Prim::Float)
        } else if id == self.prim_ids.string {
            Some(Prim::String)
        } else {
            None
        }
    }

    // --- Type constructors ---

    pub fn mk_con(&mut self, id: TypeId, name: &str, args: &[Ty]) -> Ty {
        let name = self.intern(name);
        let args = self.push_children(args);
        self.push(TypeNode::Con { id, name, args })
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
        self.mk_con(id, pn::INT, &[])
    }
    pub fn icon_float(&mut self) -> Ty {
        let id = self.prim_ids.float;
        self.mk_con(id, pn::FLOAT, &[])
    }
    pub fn icon_string(&mut self) -> Ty {
        let id = self.prim_ids.string;
        self.mk_con(id, pn::STRING, &[])
    }

    // --- Annotation name tracking ---

    pub fn name_var(&mut self, t: Ty, name: StrId) {
        if let TypeNode::Var(id) = self.node(t) {
            self.used_names.insert(name);
            self.var_names.insert(id, name);
        }
    }

    /// Return the display name for a var id. If unset, mint a fresh base-26
    /// name (skipping any already taken) and remember it so subsequent calls
    /// are stable.
    fn var_display_name(&mut self, id: i32) -> String {
        if let Some(&name) = self.var_names.get(&id) {
            return self.str(name).to_string();
        }
        loop {
            let candidate = next_letter(&mut self.next_name_uid);
            let sid = self.intern(&candidate);
            if !self.used_names.contains(&sid) {
                self.used_names.insert(sid);
                self.var_names.insert(id, sid);
                return candidate;
            }
        }
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

    /// Mint a fresh rigid generic variable. Used by the hydrator when reading
    /// type-parameter annotations so that distinct textual occurrences of the
    /// same parameter share one identity but cannot be solved to a concrete
    /// type while checking the body.
    pub fn fresh_generic_var(&mut self) -> (Ty, i32) {
        let id = self.next_var_id;
        self.alloc_var(TyVarState::Generic {
            id,
            constraint: None,
        })
    }

    // --- Union-find: find with path compression ---

    pub fn find(&mut self, t: Ty) -> Ty {
        let TypeNode::Var(id) = self.node(t) else {
            return t;
        };
        let idx = id as usize;
        let TyVarState::Link { ty } = self.vars[idx] else {
            return t;
        };
        let rep = self.find(ty);
        self.vars[idx] = TyVarState::Link { ty: rep };
        rep
    }

    /// If `t` currently resolves (via union-find) to a concrete `Int`, `Float`
    /// or `String` constructor, return which one. Returns `None` for unbound
    /// vars (including constrained `numeric`/`addable` vars that haven't been
    /// unified with a concrete type yet) and for every other constructor.
    /// Read-only over the union-find aside from `find`'s path compression.
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

    // --- Occurs check + level adjustment ---
    //
    // For sound level-based generalization (Rémy/Didier style), when binding a
    // var at level L to a type T we must also lower the level of every unbound
    // var inside T to min(its_level, L). Otherwise an inner var that becomes
    // observable from an outer scope can be wrongly quantified at the inner
    // level. Generic vars carry no level and are skipped (already terminal).

    fn occurs_and_adjust(&mut self, var_id: i32, var_level: i32, t: Ty) -> bool {
        let r = self.find(t);
        match self.node(r) {
            TypeNode::Var(id) => {
                if id == var_id {
                    return true;
                }
                if let TyVarState::Unbound { level, constraint } = self.vars[id as usize]
                    && level > var_level
                {
                    self.vars[id as usize] = TyVarState::Unbound {
                        level: var_level,
                        constraint,
                    };
                }
                false
            }
            // `Bound` only exists inside stored schemes; `instantiate`
            // substitutes it away before any value reaches unification.
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

    fn occurs_slice(&mut self, var_id: i32, var_level: i32, sl: ArenaSlice) -> bool {
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
                // Nominal identity: same registered type id. Names play no
                // part — two `Parsed`s from different modules are different
                // types and must not unify.
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
            _ => Err(could_not_unify(fa, fb)),
        }
    }

    fn unify_children(
        &mut self,
        a: ArenaSlice,
        b: ArenaSlice,
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

    /// Convenience wrapper for callers that just want a diagnostic pushed on
    /// failure rather than handling `UnifyError` themselves. Returns `true` on
    /// success for drop-in compatibility with the previous bool-returning API.
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
        let state = self.vars[var_id as usize];
        match state {
            TyVarState::Link { .. } => Err(could_not_unify(var_ty, ty)),
            TyVarState::Generic {
                id,
                constraint: gen_c,
            } => {
                // A rigid generic may only absorb an unbound var (which then
                // links to the same generic) or an identical generic.
                if let TypeNode::Var(other_id) = self.node(ty) {
                    match self.vars[other_id as usize] {
                        TyVarState::Unbound {
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
                        TyVarState::Generic { id: other_gid, .. } if other_gid == id => {
                            return Ok(());
                        }
                        _ => {}
                    }
                }
                Err(could_not_unify(var_ty, ty))
            }
            TyVarState::Unbound { level, constraint } => {
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
                let other_state = self.vars[other_id as usize];
                match other_state {
                    TyVarState::Unbound {
                        level: other_level,
                        constraint: other_constraint,
                    } => {
                        if let Some(other_c) = other_constraint {
                            let allowed_a = constraint.allowed_types();
                            let allowed_b = other_c.allowed_types();
                            if !allowed_a.iter().any(|t| allowed_b.contains(t)) {
                                return Err(could_not_unify(var_ty, resolved));
                            }
                            let winner = if allowed_a.len() <= allowed_b.len() {
                                constraint
                            } else {
                                other_c
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
                    TyVarState::Generic {
                        constraint: gen_c, ..
                    } => {
                        // Linking the constrained var to a rigid generic
                        // discards the constraint, so the generic must already
                        // guarantee it. An unconstrained generic does not.
                        if !constraint.satisfied_by_generic(gen_c) {
                            return Err(could_not_unify(var_ty, resolved));
                        }
                    }
                    TyVarState::Link { .. } => {}
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

    /// If `ty` is (or can become) a function of the given arity, return its
    /// parameter and return types. An unbound var is pre-linked to a fresh
    /// `fn(a0..aN) -> r` so the caller can proceed and let unification refine
    /// it. A function of the wrong arity yields `IncorrectArity` carrying the
    /// real params/ret so the caller can still type-check what it can.
    pub fn match_fun_type(
        &mut self,
        ty: Ty,
        arity: usize,
    ) -> Result<(Vec<Ty>, Ty), MatchFunTypeError> {
        let resolved = self.find(ty);
        if let TypeNode::Var(id) = self.node(resolved) {
            match self.vars[id as usize] {
                TyVarState::Unbound { .. } => {
                    let params: Vec<Ty> = (0..arity).map(|_| self.fresh_var()).collect();
                    let ret = self.fresh_var();
                    let fn_ty = self.mk_fun(&params, ret);
                    self.vars[id as usize] = TyVarState::Link { ty: fn_ty };
                    return Ok((params, ret));
                }
                TyVarState::Generic { .. } => {
                    return Err(MatchFunTypeError::NotFn { ty: resolved });
                }
                TyVarState::Link { .. } => {}
            }
        }
        if let TypeNode::Fun { params, ret } = self.node(resolved) {
            let params: Vec<Ty> = self.children_of(params).to_vec();
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

    // --- Generalization ---
    //
    // Walks the type, flips every Unbound var at level > current to Generic,
    // and returns the collected ids as the scheme's quantified set. Display
    // names (a, b, c, ...) are assigned at this point so they are stable
    // across all later printings of the scheme.

    pub fn generalize(&mut self, ty: Ty) -> Scheme {
        self.generalize_impl(ty, false, ValueKind::Local)
    }

    /// Module-scope generalization (Gleam's `generalise`): every remaining
    /// Unbound var becomes Generic regardless of level. Used in pass 5 after a
    /// top-level body has been fully inferred so its scheme is closed.
    pub fn generalize_top(&mut self, ty: Ty) -> Scheme {
        self.generalize_impl(ty, true, ValueKind::ModuleFn)
    }

    fn generalize_impl(&mut self, ty: Ty, ignore_level: bool, kind: ValueKind) -> Scheme {
        let mut ids: Vec<i32> = Vec::new();
        self.collect_generalizable(ty, ignore_level, &mut ids);
        self.assign_names(&ids);
        // Close the scheme: rewrite each generalized Var to its Bound index and
        // snapshot its constraint + display name. After this the closed type
        // contains no engine-local references, so the scheme can be moved
        // between engines (precompiled stdlib) without dragging `vars`.
        let quantified: Vec<QuantVar> = ids
            .iter()
            .map(|id| {
                let constraint = match self.vars[*id as usize] {
                    TyVarState::Generic { constraint, .. } => constraint,
                    TyVarState::Unbound { constraint, .. } => constraint,
                    TyVarState::Link { .. } => None,
                };
                let name = self.var_names.get(id).copied().unwrap_or(NO_STR);
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

    /// Replace every `Var(id)` whose representative is a `Generic` listed in
    /// `ids` with `Bound(idx)` of its position, fully resolving links along
    /// the way.
    fn close_over(&mut self, ty: Ty, ids: &[i32]) -> Ty {
        self.rewrite(ty, &mut |e, n| match n {
            TypeNode::Var(id) => match e.vars[id as usize] {
                TyVarState::Generic { id: gid, .. } => ids
                    .iter()
                    .position(|&i| i == gid)
                    .map(|ix| e.mk_bound(ix as u32)),
                _ => None,
            },
            _ => None,
        })
    }

    /// Structural type rewrite: resolve `ty`, give `leaf` first refusal at
    /// every node, and otherwise rebuild Con/Fun/Tuple by recursing into their
    /// children. Var/Bound that `leaf` declines fall through to the resolved
    /// node unchanged. Shared spine of close_over / open_with /
    /// substitute_type_vars / close_body, which differ only at the leaves.
    /// Subtrees in which nothing was rewritten are returned as-is, so fresh
    /// arena nodes are only built along paths that actually changed.
    fn rewrite(&mut self, ty: Ty, leaf: &mut impl FnMut(&mut Self, TypeNode) -> Option<Ty>) -> Ty {
        self.rewrite_node(ty, leaf).0
    }

    /// Core of `rewrite`. The bool reports whether the result differs from the
    /// input `ty` — resolving a union-find link counts as a change, so parents
    /// rebuild with `find`-resolved children and rewritten output never
    /// contains live link-vars. `false` means the result is exactly `ty`.
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
                    // Params untouched: reuse their existing child slice (the
                    // arena is append-only, so it stays valid) and only mint
                    // the Fun node itself.
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
    /// unchanged. Iterates the child range by index — the arena is append-only
    /// with stable indices, so no host copy of the slice is needed — and only
    /// allocates the result Vec once a child actually changes.
    fn rewrite_children(
        &mut self,
        sl: ArenaSlice,
        leaf: &mut impl FnMut(&mut Self, TypeNode) -> Option<Ty>,
    ) -> Option<Vec<Ty>> {
        let mut kids: Option<Vec<Ty>> = None;
        for (off, i) in sl.range().enumerate() {
            let k = self.children[i];
            let (rk, changed) = self.rewrite_node(k, leaf);
            if let Some(kids) = &mut kids {
                kids.push(rk);
            } else if changed {
                let mut v = Vec::with_capacity(sl.len as usize);
                let start = sl.start as usize;
                v.extend_from_slice(&self.children[start..start + off]);
                v.push(rk);
                kids = Some(v);
            }
        }
        kids
    }

    fn assign_names(&mut self, quantified: &[i32]) {
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
            loop {
                let candidate = next_letter(&mut self.next_name_uid);
                let sid = self.intern(&candidate);
                if !taken.contains(&sid) {
                    taken.insert(sid);
                    self.used_names.insert(sid);
                    self.var_names.insert(*qvar, sid);
                    break;
                }
            }
        }
    }

    fn collect_generalizable(&mut self, ty: Ty, ignore_level: bool, quantified: &mut Vec<i32>) {
        let r = self.find(ty);
        match self.node(r) {
            TypeNode::Var(id) => {
                let state = self.vars[id as usize];
                match state {
                    TyVarState::Unbound { level, constraint }
                        if ignore_level || level > self.current_level =>
                    {
                        self.vars[id as usize] = TyVarState::Generic { id, constraint };
                        if !quantified.contains(&id) {
                            quantified.push(id);
                        }
                    }
                    TyVarState::Generic { id: gid, .. } if !quantified.contains(&gid) => {
                        quantified.push(gid);
                    }
                    _ => {}
                }
            }
            TypeNode::Con { args, .. } => self.collect_slice(args, ignore_level, quantified),
            TypeNode::Fun { params, ret } => {
                self.collect_slice(params, ignore_level, quantified);
                self.collect_generalizable(ret, ignore_level, quantified);
            }
            TypeNode::Tuple { elems } => self.collect_slice(elems, ignore_level, quantified),
            TypeNode::Bound(_) => {}
        }
    }

    fn collect_slice(&mut self, sl: ArenaSlice, ignore_level: bool, quantified: &mut Vec<i32>) {
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
        // One slot per bound var, decided up front: the original rigid Var if
        // we are inside that body (recursive self-reference), otherwise a fresh
        // Unbound carrying the constraint and display name. `QuantVar` is
        // `Copy`, so read each one out of the pool by index before invoking
        // `&mut self` methods — no temp `Vec<QuantVar>` needed to dodge the
        // borrow.
        let start = scheme.quantified.start as usize;
        let count = scheme.quantified.len as usize;
        let mut subst: Vec<Ty> = Vec::with_capacity(count);
        for i in 0..count {
            let q = self.quants[start + i];
            let slot = if let Some(id) = q.origin_id
                && rigid_ids.contains(&id)
            {
                self.push(TypeNode::Var(id))
            } else {
                let fresh = match q.constraint {
                    Some(c) => self.fresh_constrained_var(c),
                    None => self.fresh_var(),
                };
                if q.name != NO_STR {
                    self.name_var(fresh, q.name);
                }
                fresh
            };
            subst.push(slot);
        }
        self.open_with(scheme.ty, &subst)
    }

    fn open_with(&mut self, ty: Ty, subst: &[Ty]) -> Ty {
        self.rewrite(ty, &mut |_, n| match n {
            TypeNode::Bound(i) => Some(subst[i as usize]),
            _ => None,
        })
    }

    // --- Type-parameter substitution / closing ---

    /// Substitute the type-parameters in a `TypeInfo` body template at a
    /// concrete instantiation `args` (positional, same order as `params`).
    /// Templates may reference parameters either as `Var(id == params[i].id)`
    /// (open form, while the originating engine is alive) or as `Bound(i)`
    /// (closed form, after `close_body` — used by precompiled stdlib types).
    /// Both resolve to `args[i]`.
    pub fn substitute_type_vars(&mut self, ty: Ty, params: ArenaSlice, args: &[Ty]) -> Ty {
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

    /// Rewrite a `TypeInfo` body from open form (`Var(param_id)`) to closed
    /// form (`Bound(idx)`) so it no longer references engine-local var ids.
    /// Idempotent.
    pub fn close_body(&mut self, ty: Ty, params: ArenaSlice) -> Ty {
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
                let constraint = match self.vars[id as usize] {
                    TyVarState::Unbound { constraint, .. } => constraint,
                    TyVarState::Generic { constraint, .. } => constraint,
                    TyVarState::Link { .. } => None,
                };
                if let Some(c) = constraint {
                    return t_var(c.name());
                }
                t_var(self.var_display_name(id))
            }
            TypeNode::Con { id, name, args } => {
                let name = self.str(name).to_string();
                let args: Vec<Ty> = self.children_of(args).to_vec();
                self.resolve_icon(id, &name, &args, env, path)
            }
            TypeNode::Fun { params, ret } => {
                let params: Vec<Type> = self
                    .children_of(params)
                    .to_vec()
                    .into_iter()
                    .map(|p| self.resolve_inner(p, env, path))
                    .collect();
                let ret_t = self.resolve_inner(ret, env, path);
                Type::Function {
                    params,
                    ret: Box::new(ret_t),
                }
            }
            TypeNode::Tuple { elems } => {
                let elements: Vec<Type> = self
                    .children_of(elems)
                    .to_vec()
                    .into_iter()
                    .map(|e| self.resolve_inner(e, env, path))
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
        args: &[Ty],
        env: Option<&TypeEnv>,
        path: &mut ResolvePath,
    ) -> Type {
        // Int/Float/String are declared as external in the prelude (so the env
        // has them for arity checks and go-to-def), but their resolved `Type`
        // must be `Primitive` so exhaustiveness treats them as infinite-ctor.
        // `Array` similarly resolves structurally so `[h, ..t]` patterns work.
        // `Bool` is NOT here — it is a real two-variant `Named` type and falls
        // through to the env lookup below. Matched by nominal id, not name: a
        // user-declared `type Int { }` is a distinct type and must not resolve
        // as `Primitive`.
        if id == self.prim_ids.int {
            return t_int();
        } else if id == self.prim_ids.float {
            return t_float();
        } else if id == self.prim_ids.string {
            return t_string();
        } else if id == self.prim_ids.array {
            let elem = args
                .first()
                .map(|&a| self.resolve_inner(a, env, path))
                .unwrap_or_else(|| t_var("a"));
            return t_array(elem);
        }

        // Resolve the type's variant info by its NOMINAL id — the identity
        // carried in the Con node — never by name: a same-named type declared
        // by whatever file the LSP analysed last must not answer for this one.
        let info = env.and_then(|e| e.lookup_type_info_by_id(id));

        // Resolve the type arguments first — each on its own copy of the path so
        // one argument's expansion can't leak into the next — then key the
        // recursion guard on the resolved *instance* (`id` + argument shape), not
        // the bare nominal id. A type's recursion always flows through its
        // variant fields (expanded below with the current instance on the path),
        // never through its arguments, so the arguments need not see the current
        // instance. Keying on the instance lets a finite re-nesting of the same
        // nominal type with distinct arguments — `Option(Option(Int))` — expand,
        // while a genuine self-recursive field — `List(t)` inside `List(t)` — is
        // still cut off. (Keying on the bare id collapses these two cases: it
        // takes the inner `Option` for a recursive occurrence, leaves it
        // variant-less, and so reports nested matches as non-exhaustive.)
        let resolved_args: Vec<Type> = args
            .iter()
            .map(|&a| {
                let mut branch = path.clone();
                self.resolve_inner(a, env, &mut branch)
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
                                        let s =
                                            self.substitute_type_vars(f.ty, info.type_params, args);
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
                    let s = self.substitute_type_vars(target, info.type_params, args);
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
        self.diagnostics.push(Diagnostic::error(s, message));
    }

    pub fn type_to_str(&mut self, ty: Ty) -> String {
        self.resolve(ty, None).to_string()
    }
}

/// How many times a single nominal type may re-occur along one `resolve` path
/// before the recursion guard cuts off. [`ResolvePath::would_recurse`] already
/// cuts off a *genuine* recursive occurrence (same id and same arguments) the
/// first time it repeats; this bound additionally terminates *non-uniform*
/// recursive types — e.g. `type Nest(t) { More(Nest((t, t))) Done }` — whose
/// argument grows at every level and so never produces a repeating instance
/// key. Cutting off only ever makes the exhaustiveness checker stricter (a
/// variant-less `Named` lowers to an infinite-constructor type), never unsound,
/// so the limit sits far above any realistic nesting depth.
const MAX_NOMINAL_RECURRENCE: usize = 16;

/// The nominal-type instances currently being expanded on the active `resolve`
/// path. Each entry pairs a type id with a structural key for its resolved
/// arguments, so the guard can tell a finite re-nesting (`Option(Option(Int))`,
/// distinct keys) from a true recursive occurrence (`List(t)` inside `List(t)`,
/// identical keys).
#[derive(Clone, Default)]
struct ResolvePath {
    stack: Vec<(TypeId, String)>,
}

impl ResolvePath {
    /// Whether expanding `id` with argument shape `args_key` would recurse:
    /// either that exact instance is already on the path, or this nominal type
    /// has already recurred [`MAX_NOMINAL_RECURRENCE`] times without repeating.
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

/// Structural key for a constructor's resolved type arguments, used to identify
/// a nominal-type instance in [`ResolvePath`]. It records each argument's shape
/// (head constructors and nesting) but never descends into a `Named`'s variant
/// table: `(id, type_args)` already pins the instance, and the variants are
/// exactly what resolution is in the middle of computing. Type variables
/// collapse to one token — distinguishing them could only make the guard cut
/// off *less*, so merging them keeps it conservative.
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

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::type_def::{t_array, t_int, t_string};

    fn no_rigid() -> HashSet<i32> {
        HashSet::new()
    }

    /// Mint a rigid generic variable carrying constraint `c`, exactly as
    /// `generalize` produces from a constrained body var (it flips the unbound
    /// var to `Generic` in place, preserving the constraint).
    fn constrained_generic(e: &mut InferEngine, c: Constraint) -> Ty {
        e.enter_level();
        let v = e.fresh_constrained_var(c);
        e.leave_level();
        let _ = e.generalize(v);
        v
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
        assert!(matches!(
            e.unify(n, s),
            Err(UnifyError::CouldNotUnify { .. })
        ));
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
        // A constrained var must NOT silently link to an unconstrained rigid
        // generic: doing so discards the numeric/addable requirement, which let
        // a function doing arithmetic on an annotated polymorphic param pass
        // type-checking and never propagated the constraint to its scheme.
        let mut e = new_engine();
        let n = e.fresh_constrained_var(Constraint::Numeric);
        let (g, _) = e.fresh_generic_var();
        assert!(matches!(
            e.unify(n, g),
            Err(UnifyError::CouldNotUnify { .. })
        ));
    }

    #[test]
    fn constraint_not_dropped_when_generic_is_expected() {
        // Symmetric path: a rigid generic on the `expected` side absorbing a
        // constrained var must apply the same check.
        let mut e = new_engine();
        let (g, _) = e.fresh_generic_var();
        let a = e.fresh_constrained_var(Constraint::Addable);
        assert!(matches!(
            e.unify(g, a),
            Err(UnifyError::CouldNotUnify { .. })
        ));
    }

    #[test]
    fn constrained_var_links_to_satisfying_generic() {
        // A `numeric` generic already guarantees `addable` (Int/Float is a
        // subset of Int/Float/String), so an addable var may link to it.
        let mut e = new_engine();
        let numeric_generic = constrained_generic(&mut e, Constraint::Numeric);
        let addable = e.fresh_constrained_var(Constraint::Addable);
        assert!(e.unify(addable, numeric_generic).is_ok());
        assert!(e.diagnostics.is_empty());
    }

    #[test]
    fn constrained_var_rejects_insufficient_generic() {
        // An `addable` generic may still be a String, so it does not satisfy a
        // `numeric` requirement.
        let mut e = new_engine();
        let addable_generic = constrained_generic(&mut e, Constraint::Addable);
        let numeric = e.fresh_constrained_var(Constraint::Numeric);
        assert!(matches!(
            e.unify(numeric, addable_generic),
            Err(UnifyError::CouldNotUnify { .. })
        ));
    }

    #[test]
    fn unify_mismatched_cons_produces_error() {
        let mut e = new_engine();
        let i = e.icon_int();
        let s = e.icon_string();
        assert!(matches!(
            e.unify(i, s),
            Err(UnifyError::CouldNotUnify { .. })
        ));
    }

    #[test]
    fn unify_at_pushes_diagnostic() {
        use crate::span::point_span;
        let mut e = new_engine();
        let i = e.icon_int();
        let s = e.icon_string();
        assert!(!e.unify_at(i, s, point_span(1, 1)));
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
        // A monomorphic fn whose param/ret vars are Linked to Int must close
        // to a scheme with no live Var nodes: stored Schemes outlive
        // `truncate_to`'s `vars.clear()`, so any surviving var id would read
        // an unrelated fresh var (or panic OOB) on the next compile.
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
        // A linked var next to an already-resolved sibling: the rebuild must
        // not re-copy the original (link-bearing) child slots.
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
