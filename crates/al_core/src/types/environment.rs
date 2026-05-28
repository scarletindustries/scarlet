use indexmap::IndexMap;

use super::infer::{ArenaSlice, Scheme, StrId, Ty};

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
    pub id: i32,
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

#[derive(Debug, Clone)]
pub struct TypeEnv {
    pub scopes: Vec<IndexMap<String, Scheme>>,
    pub type_info: IndexMap<String, TypeInfo>,
    pub definitions: IndexMap<String, DefinitionLocation>,
    pub docs: IndexMap<String, String>,
    next_type_id: i32,
}

impl TypeEnv {
    pub fn next_type_id(&self) -> i32 {
        self.next_type_id
    }
    pub fn set_next_type_id(&mut self, id: i32) {
        self.next_type_id = id;
    }

    pub fn watermark(&self) -> EnvWatermark {
        EnvWatermark {
            root_scope: self.scopes.first().map(|s| s.len()).unwrap_or(0),
            type_info: self.type_info.len(),
            definitions: self.definitions.len(),
            docs: self.docs.len(),
            next_type_id: self.next_type_id,
        }
    }

    pub fn truncate_to(&mut self, w: &EnvWatermark) {
        self.scopes.truncate(1);
        if let Some(root) = self.scopes.first_mut() {
            root.truncate(w.root_scope);
        }
        self.type_info.truncate(w.type_info);
        self.definitions.truncate(w.definitions);
        self.docs.truncate(w.docs);
        self.next_type_id = w.next_type_id;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct EnvWatermark {
    pub root_scope: usize,
    pub type_info: usize,
    pub definitions: usize,
    pub docs: usize,
    pub next_type_id: i32,
}

pub fn new_env() -> TypeEnv {
    TypeEnv {
        scopes: vec![IndexMap::new()],
        type_info: IndexMap::new(),
        definitions: IndexMap::new(),
        docs: IndexMap::new(),
        next_type_id: 1,
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
        self.docs.insert(name.to_string(), doc);
    }

    pub fn store_doc_opt(&mut self, name: &str, doc: &Option<String>) {
        if let Some(d) = doc {
            self.docs.insert(name.to_string(), d.clone());
        }
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
    ) -> i32 {
        let id = self.next_type_id;
        self.next_type_id += 1;
        self.type_info.insert(
            name.to_string(),
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
        self.type_info
            .get_mut(name)
            .unwrap_or_else(|| panic!("set_type_body: '{name}' head not registered"))
            .body = body;
    }

    pub fn lookup_type_info(&self, name: &str) -> Option<TypeInfo> {
        self.type_info.get(name).copied()
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
