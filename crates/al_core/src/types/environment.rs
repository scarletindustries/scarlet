use indexmap::IndexMap;

use super::infer::{ArenaSlice, Scheme, StrId, Ty};
use crate::type_def::TypeId;

/// What a definition denotes, stamped on every [`DefinitionLocation`]. This is
/// the exact enum the reference graph keys [`DefId`](crate::reference::DefId)
/// on — re-exported from [`crate::reference`] rather than redefined so the
/// inference env and the graph share one kind vocabulary with no conversion
/// seam between them. It must stay `Copy` (it is — see the definition) so
/// `DefinitionLocation`, and therefore `Scheme`, stays `Copy` and the
/// precompiled stdlib keeps emitting `&'static [Scheme]` directly.
pub use crate::reference::EntityKind;

/// The canonical definition site of a name: its source span, the interned path
/// of the module that owns it, and what kind of entity it is. `module` is an
/// `ArenaSlice` into `InferEngine.str_slices` (never a `Vec<String>`) so this
/// struct — and `Scheme`, which embeds it — stays `Copy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DefinitionLocation {
    pub line: i32,
    pub column: i32,
    pub end_col: i32,
    /// → `InferEngine.str_slices`: the owning module's path segments.
    pub module: ArenaSlice,
    pub entity: EntityKind,
}

/// A type parameter on a nominal type. `id` is the Generic var id minted by the
/// hydrator at Pass 1; `name` is kept for diagnostics and display.
#[derive(Debug, Clone, Copy)]
pub struct TypeParam {
    pub name: StrId,
    /// Engine-local var id. `-1` for types loaded from a `StaticStdlib`, where
    /// `close_body` has rewritten body refs to `Bound(idx)` so the id is never
    /// consulted.
    pub id: i32,
}

/// A field of a constructor variant in template form. `ty` is a `Ty` containing
/// `Var(id)` references to the owning type's `TypeParam` ids (or `Bound(idx)`
/// after `close_body`), so callers substitute concrete arguments via the
/// inference engine without round-tripping through `type_def::Type`.
#[derive(Debug, Clone, Copy)]
pub struct VariantField {
    pub label: StrId,
    pub ty: Ty,
}

/// One constructor of a custom type. `fields` → `InferEngine.variant_fields`.
#[derive(Debug, Clone, Copy)]
pub struct Variant {
    pub name: StrId,
    pub fields: ArenaSlice,
}

/// The body of a registered type. Separated from the head so that Pass 1 can
/// register all heads (allocating ids and arities) before any body is hydrated
/// in Pass 4, which is what makes mutually-recursive type declarations work.
#[derive(Debug, Clone, Copy)]
pub enum TypeBody {
    /// Head registered but body not yet hydrated. Reading variants/target in
    /// this state is a compiler bug.
    Unresolved,
    /// `type Name(params) { Ctor(label Type, ...) ... }` — `variants` →
    /// `InferEngine.variants`.
    Custom { variants: ArenaSlice },
    /// `type Name(params) = Target`
    Alias { target: Ty },
    /// `pub type Name` with no body — host-backed.
    External,
}

/// Canonical record for a user-defined nominal type. `name` is the canonical
/// name as declared (which may differ from the local binding under
/// `import {X as Y}`).
///
/// `Copy` so the precompiled stdlib emits `&'static [TypeInfo]` directly.
/// `module`/`type_params` slice into engine pools; `name` is a `StrId`.
#[derive(Debug, Clone, Copy)]
pub struct TypeInfo {
    pub id: TypeId,
    pub name: StrId,
    /// → `InferEngine.str_slices`
    pub module: ArenaSlice,
    /// → `InferEngine.type_params`
    pub type_params: ArenaSlice,
    pub body: TypeBody,
}

impl TypeInfo {
    /// Number of type parameters. Used by the hydrator's arity check when a
    /// type is referenced in an annotation.
    pub fn arity(&self) -> usize {
        self.type_params.len as usize
    }

    /// Convenience accessor for the variant slice when the body is `Custom`.
    pub fn variants(&self) -> Option<ArenaSlice> {
        match self.body {
            TypeBody::Custom { variants } => Some(variants),
            _ => None,
        }
    }
}

/// One in-place modification of a flat-map entry that already existed when it
/// was written. The flat maps (`type_info` / `definitions` / `docs`) roll back
/// by truncating to a recorded length, which cannot undo an insert that
/// REPLACED an existing entry — `IndexMap::insert` keeps the entry at its
/// original (possibly pre-watermark) index, so a later truncation never
/// reaches it. Without this journal, an entry file declaring `type Parsed`
/// would permanently clobber the seeded stdlib `Parsed` for the rest of an
/// LSP session. Every overwrite is recorded here and replayed (newest first)
/// by `truncate_to` before the length truncation.
#[derive(Debug, Clone)]
enum Overwrite {
    TypeInfo(String, TypeInfo),
    TypeInfoById(TypeId, TypeInfo),
    Definition(String, DefinitionLocation),
    Doc(String, String),
}

#[derive(Debug, Clone)]
pub struct TypeEnv {
    pub scopes: Vec<IndexMap<String, Scheme>>,
    /// Type lookup by SOURCE name — annotation resolution only, where lexical
    /// shadowing (an entry's `type Parsed` over the stdlib's) is the correct
    /// semantics. Semantic lookups (exhaustiveness, field access, hover
    /// resolution) must go through `type_info_by_id` instead.
    pub type_info: IndexMap<String, TypeInfo>,
    /// Type lookup by NOMINAL id — the identity carried in `TypeNode::Con`.
    /// Ids are allocator-unique, so entries here are never overwritten in
    /// place by a name collision; rollback is plain truncation.
    pub type_info_by_id: IndexMap<TypeId, TypeInfo>,
    pub definitions: IndexMap<String, DefinitionLocation>,
    pub docs: IndexMap<String, String>,
    /// Replay log of in-place overwrites; see [`Overwrite`].
    journal: Vec<Overwrite>,
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
        EnvWatermark {
            root_scope: self.scopes.first().map(|s| s.len()).unwrap_or(0),
            type_info: self.type_info.len(),
            type_info_by_id: self.type_info_by_id.len(),
            definitions: self.definitions.len(),
            docs: self.docs.len(),
            journal: self.journal.len(),
            next_type_id: self.next_type_id,
        }
    }

    pub fn truncate_to(&mut self, w: &EnvWatermark) {
        self.scopes.truncate(1);
        if let Some(root) = self.scopes.first_mut() {
            root.truncate(w.root_scope);
        }
        // Undo in-place overwrites first (newest first, so the oldest value of
        // a multiply-overwritten key wins), then truncate by length. A
        // restored entry that itself sits above the truncation point is
        // removed again by the truncation — the replay is still correct, just
        // transient.
        while self.journal.len() > w.journal {
            match self.journal.pop() {
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
        self.type_info.truncate(w.type_info);
        self.type_info_by_id.truncate(w.type_info_by_id);
        self.definitions.truncate(w.definitions);
        self.docs.truncate(w.docs);
        self.next_type_id = w.next_type_id;
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct EnvWatermark {
    pub root_scope: usize,
    pub type_info: usize,
    pub type_info_by_id: usize,
    pub definitions: usize,
    pub docs: usize,
    pub journal: usize,
    pub next_type_id: TypeId,
}

pub fn new_env() -> TypeEnv {
    TypeEnv {
        scopes: vec![IndexMap::new()],
        type_info: IndexMap::new(),
        type_info_by_id: IndexMap::new(),
        definitions: IndexMap::new(),
        docs: IndexMap::new(),
        journal: Vec::new(),
        next_type_id: TypeId(1),
    }
}

impl TypeEnv {
    pub fn push_scope(&mut self) {
        self.scopes.push(IndexMap::new());
    }

    pub fn pop_scope(&mut self) {
        if self.scopes.len() > 1 {
            self.scopes.pop();
        }
    }

    pub fn define(&mut self, name: &str, scheme: Scheme) {
        if !self.scopes.is_empty() {
            let last = self.scopes.len() - 1;
            self.scopes[last].insert(name.to_string(), scheme);
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
        for scope in self.scopes.iter().rev() {
            if let Some(scheme) = scope.get(name) {
                return Some(scheme);
            }
        }
        None
    }

    pub fn store_doc(&mut self, name: &str, doc: String) {
        if let Some(old) = self.docs.get(name) {
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
        if let Some(old) = self.definitions.get(name) {
            self.journal
                .push(Overwrite::Definition(name.to_string(), *old));
        }
        self.definitions.insert(name.to_string(), loc);
    }

    /// Insert (or rebind) a type's info under `name`, journaling any overwrite
    /// of an existing entry so `truncate_to` can restore it. Selective type
    /// imports re-bind through here on every check. The by-id registry is kept
    /// in lockstep.
    pub fn store_type_info(&mut self, name: &str, ti: TypeInfo) {
        if let Some(old) = self.type_info.get(name) {
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
        for scope in self.scopes.iter().rev() {
            if let Some(scheme) = scope.get(name)
                && let Some(def) = scheme.def
            {
                return Some(def);
            }
        }
        self.definitions.get(name).copied()
    }

    /// Pass 1: register a type's head (name, module, parameters) with an
    /// `Unresolved` body and allocate its id. Returns the id so the caller can
    /// populate `PreludeIds` or wire constructor schemes. The body is filled in
    /// later via `set_type_body` once all heads in the module are visible,
    /// which is what permits recursive and mutually-recursive type bodies.
    pub fn register_type_head(
        &mut self,
        name: &str,
        name_id: StrId,
        module: ArenaSlice,
        type_params: ArenaSlice,
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

    /// Pass 2/4: attach a hydrated body to a previously-registered head.
    /// Panics if the head was never registered, since that indicates a bug in
    /// the analysis pass ordering rather than a user error: Pass 1 registers a
    /// head for every type decl before any body is attached here.
    #[allow(clippy::panic)]
    pub fn set_type_body(&mut self, name: &str, body: TypeBody) {
        let entry = self
            .type_info
            .get_mut(name)
            .unwrap_or_else(|| panic!("set_type_body: '{name}' head not registered"));
        // Journal the pre-body value: the head may be a pre-watermark entry
        // (an overwritten one already journaled by `store_type_info`, in which
        // case this preserves the chain head→body→restore ordering).
        let old = *entry;
        entry.body = body;
        let updated = *entry;
        self.journal
            .push(Overwrite::TypeInfo(name.to_string(), old));
        // Mirror into the by-id registry (same journal discipline).
        if let Some(old_by_id) = self.type_info_by_id.get(&old.id) {
            self.journal
                .push(Overwrite::TypeInfoById(old.id, *old_by_id));
        }
        self.type_info_by_id.insert(updated.id, updated);
    }

    pub fn lookup_type_info(&self, name: &str) -> Option<TypeInfo> {
        self.type_info.get(name).copied()
    }

    /// Nominal lookup by the id carried in `TypeNode::Con`. This is the only
    /// correct way to answer "what are this type's variants/fields" — the
    /// by-name map can be shadowed by whatever same-named type was analysed
    /// most recently.
    pub fn lookup_type_info_by_id(&self, id: TypeId) -> Option<TypeInfo> {
        self.type_info_by_id.get(&id).copied()
    }

    pub fn suggest_name(&self, name: &str) -> Option<String> {
        let mut best: Option<&String> = None;
        let mut best_dist = 4usize;

        for candidate in self.scopes.iter().flat_map(|s| s.keys()) {
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
    let a = a.as_bytes();
    let b = b.as_bytes();
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
