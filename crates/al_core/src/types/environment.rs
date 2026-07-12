use indexmap::IndexMap;

use super::infer::{ArenaSlice, Scheme, StrId, Ty, pool};
use crate::span::Span;
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
    /// identifier is always single-line, so the reconstructed span is exactly
    /// the one [`Self::new`] was handed — keeping a definition's `DefId` equal
    /// to the `DefId` every occurrence of it targets.
    pub fn span(&self) -> Span {
        Span {
            start_line: self.line,
            start_column: self.column,
            end_line: self.line,
            end_column: self.end_col,
        }
    }
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
    pub fields: ArenaSlice<pool::VariantFields>,
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
    Custom {
        variants: ArenaSlice<pool::Variants>,
    },
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
    pub module: ArenaSlice<pool::StrSlices>,
    /// → `InferEngine.type_params`
    pub type_params: ArenaSlice<pool::TypeParams>,
    pub body: TypeBody,
}

impl TypeInfo {
    /// Number of type parameters. Used by the hydrator's arity check when a
    /// type is referenced in an annotation.
    pub fn arity(&self) -> usize {
        self.type_params.len as usize
    }

    /// Convenience accessor for the variant slice when the body is `Custom`.
    pub fn variants(&self) -> Option<ArenaSlice<pool::Variants>> {
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
    /// A root-scope `define` that replaced an existing entry. Defines inside
    /// an open scope roll back through `scope_undo` instead.
    Binding(String, Scheme),
    TypeInfo(String, TypeInfo),
    TypeInfoById(TypeId, TypeInfo),
    Definition(String, DefinitionLocation),
    Doc(String, String),
}

#[derive(Debug, Clone)]
pub struct TypeEnv {
    /// Flat name → scheme map. Nested lexical scopes are implemented via the
    /// `scope_undo`/`scope_marks` undo log below rather than a stack of
    /// per-scope `IndexMap`s, so `push_scope`/`pop_scope` allocate nothing for
    /// the common empty scope and `lookup` is a single hash probe instead of
    /// O(scope depth). Same pattern as `Compiler::locals`.
    ///
    /// All five maps below are private: every overwrite MUST go through the
    /// journaling mutators (`define`, `store_type_info`, `store_definition`,
    /// `store_doc`) or `truncate_to` cannot restore the clobbered entry.
    bindings: IndexMap<String, Scheme>,
    /// For every `define` while at least one scope is open, `(entry index,
    /// value before this define)`. `pop_scope` replays entries above the top
    /// mark newest-first: `Some` restores the shadowed value in place; `None`
    /// pops the entry (which is always the last — new keys are appended, and
    /// reverse replay removes them in reverse append order, so `pop()` is
    /// exact and index order is never perturbed).
    scope_undo: Vec<(usize, Option<Scheme>)>,
    /// `scope_undo.len()` captured at each `push_scope`.
    scope_marks: Vec<usize>,
    /// Type lookup by SOURCE name — annotation resolution only, where lexical
    /// shadowing (an entry's `type Parsed` over the stdlib's) is the correct
    /// semantics. Semantic lookups (exhaustiveness, field access, hover
    /// resolution) must go through `type_info_by_id` instead.
    type_info: IndexMap<String, TypeInfo>,
    /// Type lookup by NOMINAL id — the identity carried in `TypeNode::Con`.
    /// Ids are allocator-unique, so entries here are never overwritten in
    /// place by a name collision; rollback is plain truncation.
    type_info_by_id: IndexMap<TypeId, TypeInfo>,
    definitions: IndexMap<String, DefinitionLocation>,
    docs: IndexMap<String, String>,
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
            // Root-scope binding count: every `None` undo entry is a key that
            // was appended while a nested scope was open, so subtracting them
            // yields exactly what `bindings.len()` would be after popping all
            // scopes — i.e. the persistent root layer `truncate_to` truncates.
            root_scope: self.bindings.len()
                - self.scope_undo.iter().filter(|(_, p)| p.is_none()).count(),
            type_info: self.type_info.len(),
            type_info_by_id: self.type_info_by_id.len(),
            definitions: self.definitions.len(),
            docs: self.docs.len(),
            journal: self.journal.len(),
            next_type_id: self.next_type_id,
        }
    }

    pub fn truncate_to(&mut self, w: &EnvWatermark) {
        // Discard all nested scopes: unwind the undo log fully so `bindings`
        // holds exactly the root-scope state, then truncate that by length.
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
        // Undo in-place overwrites first (newest first, so the oldest value of
        // a multiply-overwritten key wins), then truncate by length. A
        // restored entry that itself sits above the truncation point is
        // removed again by the truncation — the replay is still correct, just
        // transient.
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
/// [`Watermark`](crate::bytecode::Watermark)'s ordering key excludes this
/// field so `EnvWatermark`'s field set can change without silently perturbing
/// which cached module `Watermark::earlier` picks during invalidation (on an
/// ordering tie, `earlier`/`later` merge this payload field-wise instead).
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

/// The empty-env watermark. Hand-written rather than derived so
/// `next_type_id` matches [`new_env`]'s starting id of 1 — `TypeId` has no
/// `Default` precisely because a derive would silently manufacture the
/// `TypeId::NONE` sentinel here.
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

    pub fn define(&mut self, name: &str, scheme: Scheme) {
        // Probe by borrow first so shadowing an existing name (params, `let`
        // rebinds, match-arm vars) never allocates a fresh key `String`.
        let (idx, prev) = if let Some((idx, _, slot)) = self.bindings.get_full_mut(name) {
            (idx, Some(std::mem::replace(slot, scheme)))
        } else {
            let (idx, _) = self.bindings.insert_full(name.to_string(), scheme);
            (idx, None)
        };
        if !self.scope_marks.is_empty() {
            self.scope_undo.push((idx, prev));
        } else if let Some(prev) = prev {
            // Root-scope overwrite: `truncate_to`'s by-length truncation
            // cannot undo an in-place replace, so journal it exactly like the
            // flat maps do. Cold path — only root-level shadowing (e.g. a
            // module redefining a prelude name) reaches here; every define
            // inside a function body has a scope open.
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
        if let Some(scheme) = self.bindings.get(name)
            && let Some(def) = scheme.def
        {
            return Some(def);
        }
        self.definitions.get(name).copied()
    }

    /// Pass 1: register a type's head (name, module, parameters) with an
    /// `Unresolved` body and allocate its id. Returns the id so the caller can
    /// populate `PreludeIds` or wire constructor schemes. The body is filled in
    /// later via `set_type_body` once all heads in the module are visible,
    /// which is what permits recursive and mutually-recursive type bodies.
    ///
    /// `name` MUST be the string that `name_id` was interned from
    /// (`engine.str(name_id) == name`): the by-name and by-id registries are
    /// kept in lockstep by [`store_type_info`](Self::store_type_info), so a
    /// mismatch would let by-name and by-id lookups resolve to different
    /// `TypeInfo`.
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

    /// Pass 2/4: attach a hydrated body to a previously-registered head,
    /// addressed by the nominal id [`register_type_head`](Self::register_type_head)
    /// returned. Resolution goes through the by-id registry — the by-name map
    /// is shadowable, so mutating through it could attach a body to whatever
    /// same-named type was analysed most recently. Any by-name entry that
    /// still refers to this id is mirrored (same journal discipline).
    /// Panics if the head was never registered, since that indicates a bug in
    /// the analysis pass ordering rather than a user error: Pass 1 registers a
    /// head for every type decl before any body is attached here.
    #[allow(clippy::panic)]
    pub fn set_type_body(&mut self, id: TypeId, body: TypeBody) {
        let entry = self
            .type_info_by_id
            .get_mut(&id)
            .unwrap_or_else(|| panic!("set_type_body: type id {id} head not registered"));
        // Journal the pre-body value: the head may be a pre-watermark entry
        // (an overwritten one already journaled by `store_type_info`, in which
        // case this preserves the chain head→body→restore ordering).
        let old = *entry;
        entry.body = body;
        self.journal.push(Overwrite::TypeInfoById(id, old));
        for (name, by_name) in self.type_info.iter_mut().filter(|(_, ti)| ti.id == id) {
            let old_by_name = *by_name;
            by_name.body = body;
            self.journal
                .push(Overwrite::TypeInfo(name.clone(), old_by_name));
        }
    }

    pub fn lookup_type_info(&self, name: &str) -> Option<TypeInfo> {
        self.type_info.get(name).copied()
    }

    /// Unjournaled escape hatch for `precompile_stdlib`'s teardown: moves the
    /// by-name type registry out wholesale so `flatten` can snapshot it.
    /// Journaling is deliberately skipped — the env is being consumed and no
    /// `truncate_to` can ever run against it afterwards.
    pub(crate) fn take_type_info(&mut self) -> IndexMap<String, TypeInfo> {
        std::mem::take(&mut self.type_info)
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
        // Length-proportional threshold (rustc's heuristic): a 2-char typo
        // shouldn't suggest a 3-edits-away name. `best_dist` is the exclusive
        // upper bound, so `+1` yields "accept dist ≤ max(len,3)/3".
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
    // Char-based, not byte-based, so distances are in the same unit as
    // `suggest_name`'s `chars().count()` threshold (rustc's heuristic is
    // char-based). Byte-wise DP would count a multi-byte char substitution
    // as several edits and overshoot the threshold.
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

    // A root-scope `define` that overwrites an existing root binding is
    // journaled and undone by `truncate_to` — by-length truncation alone
    // cannot reach an in-place replace.
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

    // The same overwrite inside an open scope rolls back through `scope_undo`
    // and must NOT be double-restored by the journal.
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
