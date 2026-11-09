use indexmap::IndexMap;

use super::infer::{ArenaSlice, StrId, Ty, pool};
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

/// Every nominal type the compile has registered, by id: the identity
/// `TypeNode::Con` carries. What a type *name* means is a module's own
/// business; this only answers what a type *is*.
///
/// It only grows, except that a type's body is filled in once, while the
/// module that declares it compiles. A watermark is taken before a module
/// compiles, so rolling back to one is truncation by length.
#[derive(Debug, Clone)]
pub struct TypeRegistry {
    types: IndexMap<TypeId, TypeInfo>,
    next_type_id: TypeId,
}

impl TypeRegistry {
    pub fn next_type_id(&self) -> TypeId {
        self.next_type_id
    }
    pub fn set_next_type_id(&mut self, id: TypeId) {
        self.next_type_id = id;
    }

    pub fn watermark(&self) -> RegistryWatermark {
        RegistryWatermark {
            types: self.types.len(),
            next_type_id: self.next_type_id,
        }
    }

    pub fn truncate_to(&mut self, w: &RegistryWatermark) {
        self.types.truncate(w.types);
        self.next_type_id = w.next_type_id;
    }

    /// Pass 1: register a type's head with an `Unresolved` body and give it an
    /// id. `set_type_body` fills in the body once every head in the module is
    /// visible, which is what lets type bodies refer to each other.
    pub fn register_type_head(
        &mut self,
        name: StrId,
        module: ArenaSlice<pool::StrSlices>,
        type_params: ArenaSlice<pool::TypeParams>,
    ) -> TypeId {
        let id = self.next_type_id;
        self.next_type_id.0 += 1;
        self.types.insert(
            id,
            TypeInfo {
                id,
                name,
                module,
                type_params,
                body: TypeBody::Unresolved,
            },
        );
        id
    }

    /// Passes 2 and 4: attach a hydrated body to a head that
    /// [`register_type_head`](Self::register_type_head) registered. Panics if
    /// the head was never registered, because that's a bug in the order of
    /// passes, not a user error.
    #[allow(clippy::panic)]
    pub fn set_type_body(&mut self, id: TypeId, body: TypeBody) {
        let entry = self
            .types
            .get_mut(&id)
            .unwrap_or_else(|| panic!("set_type_body: type id {id} head not registered"));
        entry.body = body;
    }

    /// The type with nominal id `id`.
    pub fn lookup_type_info_by_id(&self, id: TypeId) -> Option<TypeInfo> {
        self.types.get(&id).copied()
    }
}

/// Rollback payload for [`TypeRegistry::truncate_to`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegistryWatermark {
    pub types: usize,
    pub next_type_id: TypeId,
}

/// Hand-written, not derived, so `next_type_id` matches [`new_registry`]'s
/// starting id of 1: a derive would manufacture the `TypeId::NONE` sentinel.
impl Default for RegistryWatermark {
    fn default() -> Self {
        RegistryWatermark {
            types: 0,
            next_type_id: TypeId(1),
        }
    }
}

pub fn new_registry() -> TypeRegistry {
    TypeRegistry {
        types: IndexMap::new(),
        next_type_id: TypeId(1),
    }
}

/// The candidate closest to `name` by edit distance, if any is close enough
/// to be what was meant. Ties go to the earliest candidate.
pub fn closest_name<'a>(name: &str, candidates: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    let mut best: Option<&str> = None;
    // This is rustc's rule: accept an edit distance up to max(len, 3) / 3.
    // `best_dist` is an exclusive bound, hence the `+ 1`.
    let mut best_dist = std::cmp::max(name.chars().count(), 3) / 3 + 1;
    for candidate in candidates {
        let dist = levenshtein(name, candidate);
        if dist < best_dist {
            best_dist = dist;
            best = Some(candidate);
        }
    }
    best
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
