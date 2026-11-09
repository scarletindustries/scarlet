use indexmap::IndexMap;

use super::infer::{ArenaSlice, Scheme, StrId, Ty, pool};
use crate::type_def::TypeId;
use scarlet_syntax::span::Span;

/// What kind of thing a definition *is*, stored on every
/// [`DefinitionLocation`]. LSP symbol kinds and the unused-code and dead-code
/// rules read it. It lives here and `scarlet_core::reference` re-exports it, so
/// the inference environment and the reference graph share one set of kinds
/// with nothing to convert. It must stay `Copy` so `Scheme` stays `Copy` and
/// the precompiled stdlib can emit `&'static [Scheme]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntityKind {
    /// A local value, bound by `let`, a parameter or a match pattern.
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
    /// The noun for this kind, for the hover panel and the unused-code hint.
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

    /// Whether go-to-definition on a target of this kind should jump. A
    /// `ModuleAlias` spans the whole `import`, so jumping to it would go
    /// nowhere. The last path segment is handled as an `Import` occurrence
    /// instead.
    pub fn is_navigable(self) -> bool {
        !matches!(self, EntityKind::ModuleAlias)
    }
}

/// Where a name is canonically defined. `module` is an `ArenaSlice`, never a
/// `Vec<String>`, so this struct and the `Scheme` that holds it stay `Copy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DefinitionLocation {
    line: i32,
    column: i32,
    end_col: i32,
    /// The owning module's path segments, stored in `InferEngine.str_slices`.
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

    /// The span of the declaring name this location was built from. A
    /// declaring name always sits on one line, so this rebuilds exactly what
    /// [`Self::new`] was given. That keeps a definition's `DefId` equal to the
    /// one every use of it points at.
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
    /// The engine's id for this variable, which the hydrator hands out in
    /// pass 1. For types loaded from a `StaticStdlib`, it's the id from the
    /// engine that built the stdlib. That id points at nothing in the live
    /// engine, but nothing matches on it, because `close_body` rewrote every
    /// reference in the body to `Bound(idx)` before flattening.
    pub id: i32,
}

/// A field of a constructor variant, in template form. `ty` refers to the
/// owning type's parameters as `Var(id)` with their `TypeParam` ids, or as
/// `Bound(idx)` after `close_body`. So callers can substitute arguments through
/// the inference engine without converting to `type_def::Type` and back.
#[derive(Debug, Clone, Copy)]
pub struct VariantField {
    pub label: StrId,
    pub ty: Ty,
}

/// One constructor of a custom type. `fields` indexes
/// `InferEngine.variant_fields`.
#[derive(Debug, Clone, Copy)]
pub struct Variant {
    pub name: StrId,
    pub fields: ArenaSlice<pool::VariantFields>,
}

/// The body of a registered type. It's separate from the head so pass 1 can
/// register every head before pass 4 hydrates any body, which is what lets
/// type declarations refer to each other.
#[derive(Debug, Clone, Copy)]
pub enum TypeBody {
    /// The head is registered but the body isn't hydrated yet. Reading the
    /// variants or the target in this state is a compiler bug.
    Unresolved,
    /// `type Name(params) { Ctor(label Type, ...) ... }`
    Custom {
        variants: ArenaSlice<pool::Variants>,
        /// Whether a module other than the declaring one may name these
        /// constructors, meaning the type is `pub` and not `opaque`. It lives
        /// on the body, not the head, because only a body with constructors
        /// can answer it. It travels with `TypeInfo` into module interfaces
        /// and the precompiled stdlib blob, so an importer reads the same
        /// answer the declaring module worked out.
        ctors_public: bool,
    },
    /// `type Name(params) = Target`
    Alias { target: Ty },
    /// `pub type Name` with no body. The host provides its values.
    External,
}

/// The canonical record of a user-defined nominal type. `name` is the name as
/// declared, which can differ from the local binding under `import {X as Y}`.
/// It's `Copy` so the precompiled stdlib can emit `&'static [TypeInfo]`
/// directly.
#[derive(Debug, Clone, Copy)]
pub struct TypeInfo {
    pub id: TypeId,
    pub name: StrId,
    /// Indexes `InferEngine.str_slices`.
    pub(crate) module: ArenaSlice<pool::StrSlices>,
    /// Indexes `InferEngine.type_params`.
    pub type_params: ArenaSlice<pool::TypeParams>,
    pub(crate) body: TypeBody,
}

impl TypeInfo {
    /// How many type parameters the type takes, for the hydrator's arity
    /// check.
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
    /// A caller that builds values by constructor treats all three states
    /// differently. It must look through an alias, an external type has no
    /// representation at all, and a hydrated but private body must be refused.
    /// Folding "no constructors" into `false` would let the first two be
    /// reported as the third.
    pub fn ctors_public(&self) -> Option<bool> {
        match self.body {
            TypeBody::Custom { ctors_public, .. } => Some(ctors_public),
            _ => None,
        }
    }
}

/// One in-place replacement of an entry that already existed. The flat maps
/// roll back by truncating to a length, which can't undo a replacement,
/// because `IndexMap::insert` keeps the entry at its original index, which may
/// be below the watermark. Without this journal, an entry file that declares
/// `type Parsed` would overwrite the stdlib's `Parsed` for the rest of an LSP
/// session. `truncate_to` replays these newest first before it truncates.
#[derive(Debug, Clone)]
enum Overwrite {
    /// A root-scope `define` that replaced an existing entry. A define inside
    /// an open scope rolls back through `scope_undo` instead.
    Binding(String, Scheme),
    TypeInfo(String, TypeInfo),
    TypeInfoById(TypeId, TypeInfo),
    Definition(String, DefinitionLocation),
    Doc(String, String),
}

/// One change by name made while a module frame was open.
/// [`TypeEnv::pop_module_frame`] replays these newest first. `None` records a
/// new key, which the pop removes. By then it's always the map's last entry, so
/// index order never changes. `Some` records an overwrite, which the pop
/// restores.
///
/// A module's names (its imports, its own type heads and the mangled
/// `qualifier::Name` keys) are visible only while that module compiles.
/// Without this log they'd stay in the flat maps for the rest of the compile.
/// A later module could then resolve a name it never imported, and
/// `scarlet check` would accept code that a single-file LSP check rejects. The
/// by-id type registry is deliberately NOT covered, because ids are the global
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
    /// A flat map from name to scheme. Nested scopes live in the `scope_undo`
    /// log below instead of a stack of maps, one per scope, so an empty scope
    /// allocates nothing and `lookup` is one hash probe. `Compiler::locals`
    /// works the same way.
    ///
    /// Every map here is private. An overwrite MUST go through the methods
    /// that journal it (`define`, `store_type_info`, `store_definition`,
    /// `store_doc`), or `truncate_to` can't restore the entry it replaced.
    bindings: IndexMap<String, Scheme>,
    /// For every `define` made while a scope is open, the entry's index and
    /// its previous value. `pop_scope` replays these newest first. `Some`
    /// restores the value in place, and `None` pops the entry, which is always
    /// the last one, so index order never changes.
    scope_undo: Vec<(usize, Option<Scheme>)>,
    /// `scope_undo.len()` as it was at each `push_scope`.
    scope_marks: Vec<usize>,
    /// Looks types up by SOURCE name. Only annotation resolution uses it,
    /// because lexical shadowing is the right behaviour there.
    /// Exhaustiveness, field access and hover must use `type_info_by_id`
    /// instead.
    type_info: IndexMap<String, TypeInfo>,
    /// Looks types up by NOMINAL id, the identity `TypeNode::Con` carries.
    /// Each id is handed out once, so no clash of names can overwrite an entry
    /// here.
    type_info_by_id: IndexMap<TypeId, TypeInfo>,
    definitions: IndexMap<String, DefinitionLocation>,
    docs: IndexMap<String, String>,
    /// The log of in-place overwrites to replay. See [`Overwrite`].
    journal: Vec<Overwrite>,
    /// The log of changes by name made inside module frames, to replay. See
    /// [`FrameUndo`].
    frame_undo: Vec<FrameUndo>,
    /// `(frame_undo.len(), scope_undo.len())` as they were at each
    /// `push_module_frame`. The `scope_undo` mark is there for the session's
    /// throwaway import scope. A define made *during* the frame, but recorded
    /// against a scope that was already open when the frame began, still
    /// belongs to the frame, so `pop_module_frame` drains `scope_undo` down to
    /// the mark.
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
        // A `None` frame-undo entry is a key that's removed when its module
        // frame pops, so it's temporary in the same way as a `None` scope-undo
        // entry. Subtract both to get the size each map keeps.
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
            // Every `None` undo entry is a key added while a nested scope was
            // open. Subtracting them gives `bindings.len()` as it would be
            // after popping every scope, which is the root layer that stays.
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
        // Invalidation runs between compiles, after every module frame has
        // closed. We unwind anyway, so the truncations by length below never
        // see a leftover frame entry. Scopes go first, because they nest
        // inside frames, so their entries are the newer ones.
        debug_assert!(
            self.frame_marks.is_empty(),
            "truncate_to inside a module frame"
        );
        // Unwind the whole undo log, so `bindings` holds only the root scope.
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
        // Replay newest first, so when a key was overwritten several times its
        // oldest value wins. A restored entry above the truncation point is
        // removed again below.
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

/// The point [`TypeEnv::truncate_to`] rolls back to. It's deliberately not
/// `Ord`. `Watermark`'s ordering key leaves it out, so these fields can change
/// without changing which module `Watermark::earlier` picks on invalidation.
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

/// Written by hand, not derived, so `next_type_id` matches [`new_env`]'s first
/// id of 1. A derive would put the `TypeId::NONE` sentinel here.
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

    /// Open a module frame. Until the matching [`Self::pop_module_frame`],
    /// every change by name made outside an open scope is recorded as a
    /// [`FrameUndo`] and undone at the pop. See `FrameUndo` for why.
    pub fn push_module_frame(&mut self) {
        self.frame_marks
            .push((self.frame_undo.len(), self.scope_undo.len()));
    }

    /// Close the innermost module frame. This removes every by-name entry the
    /// module registered (its imports, its own type heads and top-level
    /// values, and the mangled `qualifier::Name` keys) and restores whatever
    /// they overwrote. Scopes opened inside the frame are balanced by now.
    /// Entries above the recorded `scope_undo` mark were recorded against a
    /// scope that was already open when the frame began, so they unwind with
    /// the frame.
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

    /// Replay `frame_undo` down to `mark`, newest first. By the time an
    /// inserted key's record is replayed, it's always the map's last entry, so
    /// removing it is a pop from the end and index order never changes.
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
        // Look the name up by reference first, so shadowing an existing name
        // never allocates a new key `String`.
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
            // Truncating by length can't undo an in-place replacement, so
            // journal it. This path is rare, because every define inside a
            // function body has a scope open.
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

    /// Insert or replace a type's info under `name`, and keep the by-id
    /// registry in step. Outside a module frame, an overwrite is journaled so
    /// `truncate_to` can restore it. Inside one, the by-name entry belongs to
    /// the frame instead, because a module's type names must not outlive its
    /// compile. The by-id entry is global either way.
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

    /// Pass 1: register a type's head with an `Unresolved` body and give it an
    /// id. `set_type_body` fills in the body once every head in the module is
    /// visible, which is what lets type bodies refer to each other.
    ///
    /// `name` MUST be the string `name_id` was interned from. The by-name and
    /// by-id registries are kept in step, so a mismatch would let the two
    /// lookups find different `TypeInfo`s.
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

    /// Passes 2 and 4: attach a hydrated body to a head that
    /// [`register_type_head`](Self::register_type_head) registered, found by
    /// nominal id. It goes through the by-id registry because names in the
    /// by-name map can be shadowed, so changing an entry through it could
    /// attach the body to whichever type of that name was analysed last.
    /// Panics if the head was never registered, because that's a bug in the
    /// order of passes, not a user error.
    #[allow(clippy::panic)]
    pub fn set_type_body(&mut self, id: TypeId, body: TypeBody) {
        let entry = self
            .type_info_by_id
            .get_mut(&id)
            .unwrap_or_else(|| panic!("set_type_body: type id {id} head not registered"));
        // The head may itself be a journaled entry from before the watermark.
        // Journaling its value from before the body keeps the order right:
        // head, then body, then restore.
        let old = *entry;
        entry.body = body;
        self.journal.push(Overwrite::TypeInfoById(id, old));
        let in_frame = !self.frame_marks.is_empty();
        for (name, by_name) in self.type_info.iter_mut().filter(|(_, ti)| ti.id == id) {
            let old_by_name = *by_name;
            by_name.body = body;
            // This follows `store_type_info`. A by-name entry changed inside a
            // module frame is restored when the frame pops, so journaling it
            // too would bring the name back on a later `truncate_to`.
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

    /// Every qualified spelling (`map::Map`) under which a type called `name`
    /// is in scope, sorted. An import registers each of its module's types
    /// under `qualifier::Name`, so these are exactly the spellings that would
    /// resolve where the bare `name` didn't.
    pub(crate) fn qualified_spellings(&self, name: &str) -> Vec<String> {
        let suffix = format!("::{name}");
        let mut out: Vec<String> = self
            .type_info
            .keys()
            .filter(|k| k.ends_with(&suffix))
            .cloned()
            .collect();
        out.sort();
        out
    }

    /// Look a type up by the nominal id `TypeNode::Con` carries. This is the
    /// only correct way to ask for a type's variants or fields, because in the
    /// by-name map, whichever type of that name was analysed last can shadow it.
    pub fn lookup_type_info_by_id(&self, id: TypeId) -> Option<TypeInfo> {
        self.type_info_by_id.get(&id).copied()
    }

    pub fn suggest_name(&self, name: &str) -> Option<String> {
        let mut best: Option<&String> = None;
        // This is rustc's rule: accept an edit distance up to max(len, 3) / 3.
        // `best_dist` is an exclusive bound, hence the `+ 1`.
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
    // Count chars, the same unit as `suggest_name`'s threshold. Counting bytes
    // would turn one substituted multi-byte char into several edits.
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

    // Truncating by length alone can't undo an in-place replacement.
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
