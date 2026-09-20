use indexmap::IndexMap;

use super::infer::{ArenaSlice, Scheme, StrId, Ty, pool};
use crate::type_def::TypeId;
use scarlet_syntax::span::Span;

/// What a definition *is*, stamped on every [`DefinitionLocation`]. Drives LSP
/// symbol kinds and the unused/dead-code rules. Lives here and is re-exported
/// by `scarlet_core::reference` so the inference env and the reference graph share
/// one kind vocabulary with no conversion seam. Must stay `Copy` so `Scheme`
/// stays `Copy` and the precompiled stdlib can emit `&'static [Scheme]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntityKind {
    /// A `let`/parameter/match binder — a local value.
    Value,
    /// A top-level `fn` (module function or `@vm` intrinsic).
    Function,
    /// A top-level `const`.
    Constant,
    /// A data constructor of a `type`.
    Constructor,
    /// A nominal `type` (custom, alias, or external).
    Type,
    /// The local binding introduced by `import a/b` or `import a/b as c`,
    /// used to resolve qualified `c.member` accesses back to the import.
    ModuleAlias,
    /// A labelled field of a constructor variant.
    Field,
}

impl EntityKind {
    /// Human-readable noun, for the hover panel and the unused-code hint.
    pub fn noun(self) -> &'static str {
        match self {
            EntityKind::Value => "value",
            EntityKind::Function => "function",
            EntityKind::Constant => "constant",
            EntityKind::Constructor => "constructor",
            EntityKind::Type => "type",
            EntityKind::ModuleAlias => "module",
            EntityKind::Field => "field",
        }
    }

    /// Whether goto-definition on a target of this kind should navigate. A
    /// `ModuleAlias` spans the whole `import`, so jumping to it is a no-op;
    /// the final path segment is handled as an `Import` occurrence instead.
    pub fn is_navigable(self) -> bool {
        !matches!(self, EntityKind::ModuleAlias)
    }
}

/// The canonical definition site of a name. `module` is an `ArenaSlice`, never
/// a `Vec<String>`, so this struct and the `Scheme` embedding it stay `Copy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DefinitionLocation {
    line: i32,
    column: i32,
    end_col: i32,
    /// → `InferEngine.str_slices`: the owning module's path segments.
    pub module: ArenaSlice<pool::StrSlices>,
    pub entity: EntityKind,
}

impl DefinitionLocation {
    pub fn new(sp: Span, module: ArenaSlice<pool::StrSlices>, entity: EntityKind) -> Self {
        Self {
            line: sp.start_line,
            column: sp.start_column,
            end_col: sp.end_column,
            module,
            entity,
        }
    }

    /// The declaring-name span this location was built from. A declaring
    /// identifier is always single-line, so this reconstructs exactly what
    /// [`Self::new`] was handed, keeping a definition's `DefId` equal to the
    /// one every occurrence targets.
    pub fn span(&self) -> Span {
        Span {
            start_line: self.line,
            start_column: self.column,
            end_line: self.line,
            end_column: self.end_col,
        }
    }
}

/// A type parameter on a nominal type. `name` is for diagnostics only.
#[derive(Debug, Clone, Copy)]
pub struct TypeParam {
    pub name: StrId,
    /// Engine-local var id, minted by the hydrator at Pass 1. For types loaded
    /// from a `StaticStdlib` this is the build-time engine's id: dangling in
    /// the live engine, but never matched, since `close_body` rewrote every
    /// body ref to `Bound(idx)` before flattening.
    pub id: i32,
}

/// A field of a constructor variant in template form. `ty` holds `Var(id)`
/// refs to the owning type's `TypeParam` ids, or `Bound(idx)` after
/// `close_body`, so callers substitute arguments through the inference engine
/// without round-tripping through `type_def::Type`.
#[derive(Debug, Clone, Copy)]
pub struct VariantField {
    pub label: StrId,
    pub ty: Ty,
}

/// One constructor of a custom type. `fields` → `InferEngine.variant_fields`.
#[derive(Debug, Clone, Copy)]
pub struct Variant {
    pub name: StrId,
    pub fields: ArenaSlice<pool::VariantFields>,
}

/// The body of a registered type. Separate from the head so Pass 1 can register
/// every head before Pass 4 hydrates any body, which is what makes
/// mutually-recursive type declarations work.
#[derive(Debug, Clone, Copy)]
pub enum TypeBody {
    /// Head registered, body not yet hydrated. Reading variants/target in this
    /// state is a compiler bug.
    Unresolved,
    /// `type Name(params) { Ctor(label Type, ...) ... }`
    Custom {
        variants: ArenaSlice<pool::Variants>,
        /// Whether a module other than the declaring one may name these
        /// constructors: `pub` and not `opaque`. Lives on the body rather
        /// than the head because only a body with constructors can answer
        /// it, and travels with `TypeInfo` into module interfaces and the
        /// precompiled stdlib blob so an importer reads the same answer the
        /// declaring module computed.
        ctors_public: bool,
        /// `@exhaustive` was declared on this type: a match on it may not use
        /// a wildcard/bare-binder arm to stand in for variants it does not
        /// name (T-148). Same reasons as `ctors_public` for living here
        /// rather than the head: only a body with variants can be collapsed,
        /// and a precompiled stdlib type must answer the same as its source.
        must_name_variants: bool,
    },
    /// `type Name(params) = Target`
    Alias { target: Ty },
    /// `pub type Name` with no body — host-backed.
    External,
}

/// Canonical record for a user-defined nominal type. `name` is the name as
/// declared, which may differ from the local binding under `import {X as Y}`.
/// `Copy` so the precompiled stdlib emits `&'static [TypeInfo]` directly.
#[derive(Debug, Clone, Copy)]
pub struct TypeInfo {
    pub id: TypeId,
    pub name: StrId,
    /// → `InferEngine.str_slices`
    pub(crate) module: ArenaSlice<pool::StrSlices>,
    /// → `InferEngine.type_params`
    pub type_params: ArenaSlice<pool::TypeParams>,
    pub(crate) body: TypeBody,
}

impl TypeInfo {
    /// Number of type parameters, for the hydrator's arity check.
    pub fn arity(&self) -> usize {
        self.type_params.len as usize
    }

    /// The variant slice when the body is `Custom`.
    pub fn variants(&self) -> Option<ArenaSlice<pool::Variants>> {
        match self.body {
            TypeBody::Custom { variants, .. } => Some(variants),
            _ => None,
        }
    }

    /// Whether importers may name this type's constructors, or `None` when
    /// the type has none to name (alias, external, or not yet hydrated).
    ///
    /// The three states are all distinct to a caller that builds values by
    /// constructor: an alias must be looked through, an external type has no
    /// representation at all, and a hydrated-but-private body must be
    /// refused. Collapsing "no constructors" into `false` would let the
    /// first two be reported as the third.
    pub fn ctors_public(&self) -> Option<bool> {
        match self.body {
            TypeBody::Custom { ctors_public, .. } => Some(ctors_public),
            _ => None,
        }
    }

    /// Whether `@exhaustive` was declared on this type (T-148). `false`,
    /// never `None`, for anything without variants to name — an alias or an
    /// external type has nothing a wildcard arm could collapse.
    pub fn must_name_variants(&self) -> bool {
        matches!(
            self.body,
            TypeBody::Custom {
                must_name_variants: true,
                ..
            }
        )
    }
}

/// One in-place replacement of an entry that already existed. The flat maps
/// roll back by truncating to a length, which cannot undo a replace:
/// `IndexMap::insert` keeps the entry at its original, possibly pre-watermark,
/// index. Without this journal an entry file declaring `type Parsed` would
/// clobber the seeded stdlib `Parsed` for the rest of an LSP session.
/// `truncate_to` replays these newest-first before truncating.
#[derive(Debug, Clone)]
enum Overwrite {
    /// A root-scope `define` that replaced an existing entry. Defines inside an
    /// open scope roll back through `scope_undo` instead.
    Binding(String, Scheme),
    TypeInfo(String, TypeInfo),
    TypeInfoById(TypeId, TypeInfo),
    Definition(String, DefinitionLocation),
    Doc(String, String),
}

/// One by-name mutation made while a module frame was open, replayed
/// newest-first by [`TypeEnv::pop_module_frame`]. `None` records an insertion
/// of a new key (removed on pop — the entry is always the map's last, so
/// index order is never perturbed); `Some` an in-place overwrite (restored).
///
/// A module's names — its imports, its own type heads, the mangled
/// `qualifier.Name` keys — are visible only while that module compiles.
/// Without this log they would stay in the flat maps for the rest of the
/// compile, letting a later module resolve a name it never imported (which
/// `scarlet check` then accepts where a single-file LSP check errors). The
/// by-id type registry is deliberately NOT covered: ids are the global
/// identity that already-hydrated annotations carry across modules.
#[derive(Debug, Clone)]
enum FrameUndo {
    Binding(String, Option<Scheme>),
    TypeInfo(String, Option<TypeInfo>),
    Definition(String, Option<DefinitionLocation>),
    Doc(String, Option<String>),
}

#[derive(Debug, Clone)]
pub struct TypeEnv {
    /// Flat name → scheme map. Nested scopes live in the `scope_undo` log below
    /// rather than a stack of per-scope maps, so an empty scope allocates
    /// nothing and `lookup` is one hash probe. Same pattern as
    /// `Compiler::locals`.
    ///
    /// Every map here is private: an overwrite MUST go through the journaling
    /// mutators (`define`, `store_type_info`, `store_definition`, `store_doc`)
    /// or `truncate_to` cannot restore the clobbered entry.
    bindings: IndexMap<String, Scheme>,
    /// For every `define` with a scope open, `(entry index, prior value)`.
    /// `pop_scope` replays newest-first: `Some` restores in place, `None` pops
    /// the entry, which is always the last one, so index order is never
    /// perturbed.
    scope_undo: Vec<(usize, Option<Scheme>)>,
    /// `scope_undo.len()` captured at each `push_scope`.
    scope_marks: Vec<usize>,
    /// Type lookup by SOURCE name. Annotation resolution only, where lexical
    /// shadowing is the correct semantics. Exhaustiveness, field access and
    /// hover must go through `type_info_by_id` instead.
    type_info: IndexMap<String, TypeInfo>,
    /// Type lookup by NOMINAL id, the identity carried in `TypeNode::Con`. Ids
    /// are allocator-unique, so no name collision can overwrite an entry here.
    type_info_by_id: IndexMap<TypeId, TypeInfo>,
    definitions: IndexMap<String, DefinitionLocation>,
    docs: IndexMap<String, String>,
    /// Replay log of in-place overwrites; see [`Overwrite`].
    journal: Vec<Overwrite>,
    /// Replay log of by-name mutations made inside module frames; see
    /// [`FrameUndo`].
    frame_undo: Vec<FrameUndo>,
    /// `(frame_undo.len(), scope_undo.len())` captured at each
    /// `push_module_frame`. The `scope_undo` mark exists for the session's
    /// throwaway import scope: a define made *during* the frame but attributed
    /// to a scope that was already open at frame entry still belongs to the
    /// frame, so `pop_module_frame` drains `scope_undo` down to it.
    frame_marks: Vec<(usize, usize)>,
    next_type_id: TypeId,
}

impl TypeEnv {
    pub fn next_type_id(&self) -> TypeId {
        self.next_type_id
    }
    pub fn set_next_type_id(&mut self, id: TypeId) {
        self.next_type_id = id;
    }

    pub fn watermark(&self) -> EnvWatermark {
        // A `None` frame-undo entry is a key that will be removed when its
        // module frame pops, so it is transient exactly like a `None`
        // scope-undo entry: subtract both to get each map's persistent size.
        let frame_inserts = |f: fn(&FrameUndo) -> bool| {
            self.frame_undo
                .iter()
                .filter(|u| {
                    f(u) && matches!(
                        u,
                        FrameUndo::Binding(_, None)
                            | FrameUndo::TypeInfo(_, None)
                            | FrameUndo::Definition(_, None)
                            | FrameUndo::Doc(_, None)
                    )
                })
                .count()
        };
        EnvWatermark {
            // Every `None` undo entry is a key appended while a nested scope
            // was open, so subtracting them gives `bindings.len()` as it would
            // be after popping all scopes: the persistent root layer.
            root_scope: self.bindings.len()
                - self.scope_undo.iter().filter(|(_, p)| p.is_none()).count()
                - frame_inserts(|u| matches!(u, FrameUndo::Binding(..))),
            type_info: self.type_info.len()
                - frame_inserts(|u| matches!(u, FrameUndo::TypeInfo(..))),
            type_info_by_id: self.type_info_by_id.len(),
            definitions: self.definitions.len()
                - frame_inserts(|u| matches!(u, FrameUndo::Definition(..))),
            docs: self.docs.len() - frame_inserts(|u| matches!(u, FrameUndo::Doc(..))),
            journal: self.journal.len(),
            next_type_id: self.next_type_id,
        }
    }

    pub fn truncate_to(&mut self, w: &EnvWatermark) {
        // Invalidation runs between compiles, when every module frame has
        // been left; unwind defensively so the by-length truncations below
        // never see a transient frame entry. Scopes first: they nest inside
        // frames, so their entries are the newer ones.
        debug_assert!(
            self.frame_marks.is_empty(),
            "truncate_to inside a module frame"
        );
        // Unwind the undo log fully so `bindings` holds only root-scope state.
        self.scope_marks.clear();
        for (idx, prev) in self.scope_undo.drain(..).rev() {
            match prev {
                Some(s) => self.bindings[idx] = s,
                None => {
                    debug_assert_eq!(idx + 1, self.bindings.len());
                    self.bindings.pop();
                }
            }
        }
        self.frame_marks.clear();
        self.unwind_frame_undo(0);
        // Newest-first, so the oldest value of a multiply-overwritten key wins.
        // A restored entry above the truncation point is removed again below.
        while self.journal.len() > w.journal {
            match self.journal.pop() {
                Some(Overwrite::Binding(name, s)) => {
                    self.bindings.insert(name, s);
                }
                Some(Overwrite::TypeInfo(name, ti)) => {
                    self.type_info.insert(name, ti);
                }
                Some(Overwrite::TypeInfoById(id, ti)) => {
                    self.type_info_by_id.insert(id, ti);
                }
                Some(Overwrite::Definition(name, dl)) => {
                    self.definitions.insert(name, dl);
                }
                Some(Overwrite::Doc(name, doc)) => {
                    self.docs.insert(name, doc);
                }
                None => break,
            }
        }
        self.bindings.truncate(w.root_scope);
        self.type_info.truncate(w.type_info);
        self.type_info_by_id.truncate(w.type_info_by_id);
        self.definitions.truncate(w.definitions);
        self.docs.truncate(w.docs);
        self.next_type_id = w.next_type_id;
    }
}

/// Rollback payload for [`TypeEnv::truncate_to`]. Deliberately not `Ord`:
/// `Watermark`'s ordering key excludes it, so this field set can change without
/// perturbing which module `Watermark::earlier` picks on invalidation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnvWatermark {
    pub root_scope: usize,
    pub type_info: usize,
    pub type_info_by_id: usize,
    pub definitions: usize,
    pub docs: usize,
    pub journal: usize,
    pub next_type_id: TypeId,
}

/// Hand-written, not derived, so `next_type_id` matches [`new_env`]'s starting
/// id of 1: a derive would manufacture the `TypeId::NONE` sentinel here.
impl Default for EnvWatermark {
    fn default() -> Self {
        EnvWatermark {
            root_scope: 0,
            type_info: 0,
            type_info_by_id: 0,
            definitions: 0,
            docs: 0,
            journal: 0,
            next_type_id: TypeId(1),
        }
    }
}

pub fn new_env() -> TypeEnv {
    TypeEnv {
        bindings: IndexMap::new(),
        scope_undo: Vec::new(),
        scope_marks: Vec::new(),
        type_info: IndexMap::new(),
        type_info_by_id: IndexMap::new(),
        definitions: IndexMap::new(),
        docs: IndexMap::new(),
        journal: Vec::new(),
        frame_undo: Vec::new(),
        frame_marks: Vec::new(),
        next_type_id: TypeId(1),
    }
}

impl TypeEnv {
    pub fn push_scope(&mut self) {
        self.scope_marks.push(self.scope_undo.len());
    }

    pub fn pop_scope(&mut self) {
        debug_assert!(!self.scope_marks.is_empty(), "unbalanced pop_scope");
        if let Some(mark) = self.scope_marks.pop() {
            for (idx, prev) in self.scope_undo.drain(mark..).rev() {
                match prev {
                    Some(s) => self.bindings[idx] = s,
                    None => {
                        debug_assert_eq!(idx + 1, self.bindings.len());
                        self.bindings.pop();
                    }
                }
            }
        }
    }

    /// Open a module frame: until the matching [`Self::pop_module_frame`],
    /// every by-name mutation made outside an open scope is recorded on
    /// [`FrameUndo`] and unwound at the pop. See `FrameUndo` for why.
    pub fn push_module_frame(&mut self) {
        self.frame_marks
            .push((self.frame_undo.len(), self.scope_undo.len()));
    }

    /// Close the innermost module frame, removing every by-name entry the
    /// module registered — its imports, its own type heads and toplevel
    /// values, the mangled `qualifier.Name` keys — and restoring what they
    /// overwrote. Scopes opened inside the frame are balanced by now; entries
    /// above the recorded `scope_undo` mark were attributed to a scope that
    /// was already open at frame entry, so they unwind with the frame.
    pub fn pop_module_frame(&mut self) {
        debug_assert!(!self.frame_marks.is_empty(), "unbalanced pop_module_frame");
        let Some((fmark, smark)) = self.frame_marks.pop() else {
            return;
        };
        for (idx, prev) in self.scope_undo.drain(smark..).rev() {
            match prev {
                Some(s) => self.bindings[idx] = s,
                None => {
                    debug_assert_eq!(idx + 1, self.bindings.len());
                    self.bindings.pop();
                }
            }
        }
        self.unwind_frame_undo(fmark);
    }

    fn in_module_frame(&self) -> bool {
        !self.frame_marks.is_empty()
    }

    /// Replay `frame_undo` down to `mark`, newest-first. An inserted key is
    /// always the map's last entry by the time its record is replayed, so
    /// removal is a tail pop and index order is never perturbed.
    fn unwind_frame_undo(&mut self, mark: usize) {
        for u in self.frame_undo.drain(mark..).rev() {
            match u {
                FrameUndo::Binding(name, Some(s)) => {
                    self.bindings.insert(name, s);
                }
                FrameUndo::Binding(name, None) => {
                    let popped = self.bindings.pop();
                    debug_assert_eq!(popped.map(|(n, _)| n), Some(name));
                }
                FrameUndo::TypeInfo(name, Some(ti)) => {
                    self.type_info.insert(name, ti);
                }
                FrameUndo::TypeInfo(name, None) => {
                    let popped = self.type_info.pop();
                    debug_assert_eq!(popped.map(|(n, _)| n), Some(name));
                }
                FrameUndo::Definition(name, Some(dl)) => {
                    self.definitions.insert(name, dl);
                }
                FrameUndo::Definition(name, None) => {
                    let popped = self.definitions.pop();
                    debug_assert_eq!(popped.map(|(n, _)| n), Some(name));
                }
                FrameUndo::Doc(name, Some(doc)) => {
                    self.docs.insert(name, doc);
                }
                FrameUndo::Doc(name, None) => {
                    let popped = self.docs.pop();
                    debug_assert_eq!(popped.map(|(n, _)| n), Some(name));
                }
            }
        }
    }

    pub fn define(&mut self, name: &str, scheme: Scheme) {
        // Probe by borrow first so shadowing an existing name never allocates
        // a fresh key `String`.
        let (idx, prev) = if let Some((idx, _, slot)) = self.bindings.get_full_mut(name) {
            (idx, Some(std::mem::replace(slot, scheme)))
        } else {
            let (idx, _) = self.bindings.insert_full(name.to_string(), scheme);
            (idx, None)
        };
        if !self.scope_marks.is_empty() {
            self.scope_undo.push((idx, prev));
        } else if self.in_module_frame() {
            self.frame_undo
                .push(FrameUndo::Binding(name.to_string(), prev));
        } else if let Some(prev) = prev {
            // By-length truncation cannot undo an in-place replace, so journal
            // it. Cold: every define inside a function body has a scope open.
            self.journal
                .push(Overwrite::Binding(name.to_string(), prev));
        }
    }

    pub fn define_at(&mut self, name: &str, scheme: Scheme, loc: DefinitionLocation) {
        self.define(
            name,
            Scheme {
                def: Some(loc),
                ..scheme
            },
        );
    }

    pub fn lookup(&self, name: &str) -> Option<&Scheme> {
        self.bindings.get(name)
    }

    pub fn store_doc(&mut self, name: &str, doc: String) {
        if self.in_module_frame() {
            self.frame_undo.push(FrameUndo::Doc(
                name.to_string(),
                self.docs.get(name).cloned(),
            ));
        } else if let Some(old) = self.docs.get(name) {
            self.journal
                .push(Overwrite::Doc(name.to_string(), old.clone()));
        }
        self.docs.insert(name.to_string(), doc);
    }

    pub fn store_doc_opt(&mut self, name: &str, doc: &Option<String>) {
        if let Some(d) = doc {
            self.store_doc(name, d.clone());
        }
    }

    /// Insert a definition location, journaling any overwrite of an existing
    /// entry so `truncate_to` can restore it.
    pub fn store_definition(&mut self, name: &str, loc: DefinitionLocation) {
        if self.in_module_frame() {
            self.frame_undo.push(FrameUndo::Definition(
                name.to_string(),
                self.definitions.get(name).copied(),
            ));
        } else if let Some(old) = self.definitions.get(name) {
            self.journal
                .push(Overwrite::Definition(name.to_string(), *old));
        }
        self.definitions.insert(name.to_string(), loc);
    }

    /// Insert or rebind a type's info under `name`, journaling any overwrite so
    /// `truncate_to` can restore it. Keeps the by-id registry in lockstep.
    /// Inside a module frame the by-name entry is frame-scoped instead — a
    /// module's type names must not outlive its compile — while the by-id
    /// entry stays global either way.
    pub fn store_type_info(&mut self, name: &str, ti: TypeInfo) {
        if self.in_module_frame() {
            self.frame_undo.push(FrameUndo::TypeInfo(
                name.to_string(),
                self.type_info.get(name).copied(),
            ));
        } else if let Some(old) = self.type_info.get(name) {
            self.journal
                .push(Overwrite::TypeInfo(name.to_string(), *old));
        }
        self.type_info.insert(name.to_string(), ti);
        if let Some(old) = self.type_info_by_id.get(&ti.id) {
            self.journal.push(Overwrite::TypeInfoById(ti.id, *old));
        }
        self.type_info_by_id.insert(ti.id, ti);
    }

    pub fn lookup_doc(&self, name: &str) -> Option<String> {
        self.docs.get(name).cloned()
    }

    pub fn lookup_definition(&self, name: &str) -> Option<DefinitionLocation> {
        if let Some(scheme) = self.bindings.get(name)
            && let Some(def) = scheme.def
        {
            return Some(def);
        }
        self.definitions.get(name).copied()
    }

    /// Pass 1: register a type's head with an `Unresolved` body and allocate
    /// its id. `set_type_body` fills the body once every head in the module is
    /// visible, which is what permits mutually-recursive type bodies.
    ///
    /// `name` MUST be the string `name_id` was interned from. The by-name and
    /// by-id registries are kept in lockstep, so a mismatch would let the two
    /// lookups resolve to different `TypeInfo`.
    pub fn register_type_head(
        &mut self,
        name: &str,
        name_id: StrId,
        module: ArenaSlice<pool::StrSlices>,
        type_params: ArenaSlice<pool::TypeParams>,
    ) -> TypeId {
        let id = self.next_type_id;
        self.next_type_id.0 += 1;
        self.store_type_info(
            name,
            TypeInfo {
                id,
                name: name_id,
                module,
                type_params,
                body: TypeBody::Unresolved,
            },
        );
        id
    }

    /// Pass 2/4: attach a hydrated body to a head registered by
    /// [`register_type_head`](Self::register_type_head), addressed by nominal
    /// id. Goes through the by-id registry because the by-name map is
    /// shadowable, so mutating through it could attach the body to whatever
    /// same-named type was analysed most recently. Panics if the head was never
    /// registered: that is a pass-ordering bug, not a user error.
    #[allow(clippy::panic)]
    pub fn set_type_body(&mut self, id: TypeId, body: TypeBody) {
        let entry = self
            .type_info_by_id
            .get_mut(&id)
            .unwrap_or_else(|| panic!("set_type_body: type id {id} head not registered"));
        // The head may itself be a journaled pre-watermark entry; journaling
        // the pre-body value preserves the head→body→restore ordering.
        let old = *entry;
        entry.body = body;
        self.journal.push(Overwrite::TypeInfoById(id, old));
        let in_frame = !self.frame_marks.is_empty();
        for (name, by_name) in self.type_info.iter_mut().filter(|(_, ti)| ti.id == id) {
            let old_by_name = *by_name;
            by_name.body = body;
            // Same routing as `store_type_info`: a by-name entry touched
            // inside a module frame is restored at frame pop, so journaling it
            // too would resurrect the name on a later `truncate_to`.
            if in_frame {
                self.frame_undo
                    .push(FrameUndo::TypeInfo(name.clone(), Some(old_by_name)));
            } else {
                self.journal
                    .push(Overwrite::TypeInfo(name.clone(), old_by_name));
            }
        }
    }

    pub fn lookup_type_info(&self, name: &str) -> Option<TypeInfo> {
        self.type_info.get(name).copied()
    }

    /// Every qualified spelling (`map.Map`) under which a type named `name`
    /// is in scope, sorted. An import registers each of its module's types
    /// under `qualifier.Name`, so these are exactly the spellings that would
    /// resolve where the bare `name` did not.
    pub(crate) fn qualified_spellings(&self, name: &str) -> Vec<String> {
        let suffix = format!(".{name}");
        let mut out: Vec<String> = self
            .type_info
            .keys()
            .filter(|k| k.ends_with(&suffix))
            .cloned()
            .collect();
        out.sort();
        out
    }

    /// Nominal lookup by the id carried in `TypeNode::Con`. The only correct
    /// way to ask for a type's variants or fields: the by-name map can be
    /// shadowed by whatever same-named type was analysed most recently.
    pub fn lookup_type_info_by_id(&self, id: TypeId) -> Option<TypeInfo> {
        self.type_info_by_id.get(&id).copied()
    }

    pub fn suggest_name(&self, name: &str) -> Option<String> {
        let mut best: Option<&String> = None;
        // rustc's heuristic: accept distance <= max(len, 3) / 3. `best_dist` is
        // an exclusive bound, hence the `+ 1`.
        let mut best_dist = std::cmp::max(name.chars().count(), 3) / 3 + 1;

        for candidate in self.bindings.keys() {
            let dist = levenshtein(name, candidate);
            if dist < best_dist {
                best_dist = dist;
                best = Some(candidate);
            }
        }

        best.cloned()
    }
}

fn levenshtein(a: &str, b: &str) -> usize {
    // Char-based, matching `suggest_name`'s threshold unit. A byte-wise DP
    // would count one multi-byte substitution as several edits.
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }

    let mut prev: Vec<usize> = vec![0; b.len() + 1];
    let mut curr: Vec<usize> = vec![0; b.len() + 1];

    for (j, p) in prev.iter_mut().enumerate() {
        *p = j;
    }

    for i in 1..=a.len() {
        curr[0] = i;
        for j in 1..=b.len() {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            let del = prev[j] + 1;
            let ins = curr[j - 1] + 1;
            let sub = prev[j - 1] + cost;
            let mut min = del;
            if ins < min {
                min = ins;
            }
            if sub < min {
                min = sub;
            }
            curr[j] = min;
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::super::infer::mono;
    use super::*;

    // By-length truncation alone cannot reach an in-place replace.
    #[test]
    fn truncate_to_restores_overwritten_root_binding() {
        let mut env = new_env();
        env.define("x", mono(Ty(1)));
        let w = env.watermark();

        env.define("x", mono(Ty(2))); // no scope open: root-scope overwrite
        assert_eq!(env.lookup("x").unwrap().ty, Ty(2));

        env.truncate_to(&w);
        assert_eq!(
            env.lookup("x").unwrap().ty,
            Ty(1),
            "clobbered root binding must be restored"
        );
    }

    // An overwrite inside an open scope rolls back through `scope_undo` and
    // must NOT be double-restored by the journal.
    #[test]
    fn scoped_overwrite_still_rolls_back_via_scope_undo() {
        let mut env = new_env();
        env.define("x", mono(Ty(1)));
        let w = env.watermark();

        env.push_scope();
        env.define("x", mono(Ty(2)));
        env.pop_scope();
        assert_eq!(env.lookup("x").unwrap().ty, Ty(1));

        env.truncate_to(&w);
        assert_eq!(env.lookup("x").unwrap().ty, Ty(1));
    }
}
