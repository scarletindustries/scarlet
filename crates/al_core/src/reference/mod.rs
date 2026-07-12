//! The workspace reference graph.
//!
//! Every name occurrence in the program is recorded as a [`Reference`] (a
//! source [`Span`] + a [`ReferenceKind`]) resolved to the canonical
//! [`Definition`] it points at, identified by a [`DefId`]. Per-module data
//! lives in [`ModuleReferences`]; the workspace-wide [`ReferenceGraph`] owns
//! the module-path interner plus the forward (position → def) and reverse
//! (def → every occurrence) indexes the LSP and the unused/dead-code
//! diagnostics query.
//!
//! This module is the data model + index/query layer only. Population happens
//! during the existing typecheck/infer pass (other units) by building
//! `ModuleReferences` and handing them to a `ReferenceGraph`; the graph never
//! alters inference or codegen. The unused-import / dead-code analysis that
//! consumes the index lives in [`unused`], and the rename planner in
//! [`rename`].
//!
//! Module identity is interned to a [`ModuleId`] (`Copy`) rather than carried
//! as a `Vec<String>` so a `DefId` can ride on `Copy` types and survive
//! incremental recompiles independent of any one inference engine's arenas.

use std::cell::OnceCell;
use std::collections::HashMap;
use std::rc::Rc;

use indexmap::IndexMap;

use crate::module::{ModuleKey, ModulePath};
use crate::span::Span;

pub mod rename;
pub mod unused;
pub mod uri;

pub use uri::{ModuleUriError, module_uri, path_to_uri};

// ============================================================================
// EntityKind / ReferenceKind
// ============================================================================

/// What a definition *is*. Drives LSP symbol kinds and the
/// unused/dead-code rules (e.g. a `ModuleAlias` is "unused" when no
/// `Qualified` reference targets it).
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
    /// Human-readable noun for this kind, used in the hover panel and the
    /// unused/dead-code hint message.
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
    /// `ModuleAlias` definition spans the whole `import` declaration, so
    /// resolving it would be a no-op self-jump; the final path segment is
    /// handled separately as an `Import` occurrence.
    pub fn is_navigable(self) -> bool {
        !matches!(self, EntityKind::ModuleAlias)
    }
}

/// How a name is being used at an occurrence site. Mirrors Gleam's
/// reference-kind set, adapted to al's import syntax.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReferenceKind {
    /// `module.member` — qualified through an import alias.
    Qualified,
    /// The `module` half of a `module.member` access: the identifier that
    /// names an import alias rather than a value. Targets the `ModuleAlias`
    /// definition so hover / goto-def on the qualifier can reach the imported
    /// module. Not a use site — a qualifier never evaluates its target — but
    /// it is the direct alias→use edge the unused-import check reads: an
    /// alias is live iff one of these occurrences exists outside its
    /// declaration (see `ReferenceGraph::has_real_use` in [`unused`]).
    Qualifier,
    /// A bare `name` resolved through the local/value environment.
    Unqualified,
    /// The module path in an `import a/b` declaration.
    Import,
    /// The `as` alias in `import a/b as c` (binds a `ModuleAlias`).
    Alias,
    /// The item-binding token inside an import list — the `x` of
    /// `import a/b.{x}`. A reference site for find-references/rename,
    /// never an evaluating use.
    ImportItem,
    /// The defining occurrence itself (a `fn`/`type`/`let` name at its
    /// declaration site). Lets goto-def on a declaration resolve to itself
    /// and lets find-references include the definition.
    Definition,
}

impl ReferenceKind {
    /// Whether this occurrence is a real *use* in code — a qualified or
    /// unqualified name that evaluates the target — as opposed to a binding
    /// occurrence that merely declares it (`Import` path, `Alias` binding,
    /// `ImportItem` binding token, `Definition` self-occurrence) or names a
    /// module (`Qualifier`). Drives find-references, the xref reverse index,
    /// and the unused/dead-code use check.
    pub fn is_use_site(self) -> bool {
        matches!(self, ReferenceKind::Qualified | ReferenceKind::Unqualified)
    }

    /// Whether find-references should list this occurrence. A superset of
    /// [`is_use_site`](Self::is_use_site): the `b` of `b.add(..)` is a genuine
    /// textual reference to the import alias `b`, so it belongs in the result
    /// even though it is not a *use* for liveness purposes (a `Qualifier` never
    /// evaluates its target — see [`ReferenceKind::Qualifier`]). Likewise the
    /// `x` of `import a/b.{x}` (an `ImportItem`) spells the imported symbol's
    /// name, so find-references and rename must hit the token even though
    /// merely importing it evaluates nothing. Binding occurrences
    /// (`Import`/`Alias`/`Definition`) stay out: the declaration is added
    /// separately, under `includeDeclaration`.
    pub fn is_reference_site(self) -> bool {
        self.is_use_site() || matches!(self, ReferenceKind::Qualifier | ReferenceKind::ImportItem)
    }
}

// ============================================================================
// ModuleId + interner
// ============================================================================

/// Interned identity of a module path. `Copy`, stable for the lifetime of the
/// owning [`ReferenceGraph`], independent of inference-engine arenas so it
/// rides safely on `Copy` types and survives incremental recompiles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ModuleId(pub u32);

/// Append-only bijection between [`ModulePath`] and [`ModuleId`]. Ids are
/// assigned in first-seen order and never reused, so a `DefId` minted in one
/// compile still resolves after later modules are added or evicted.
#[derive(Debug, Default, Clone)]
pub struct ModuleInterner {
    paths: Vec<ModulePath>,
    by_key: HashMap<ModuleKey, ModuleId>,
}

impl ModuleInterner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Intern `path`, returning its stable id (creating one on first sight).
    pub fn intern(&mut self, path: &ModulePath) -> ModuleId {
        let key = ModuleKey::of(path);
        if let Some(&id) = self.by_key.get(&key) {
            return id;
        }
        let id = ModuleId(self.paths.len() as u32);
        self.paths.push(path.clone());
        self.by_key.insert(key, id);
        id
    }

    /// Look up an already-interned path by value.
    pub fn lookup(&self, path: &ModulePath) -> Option<ModuleId> {
        self.by_key.get(&ModuleKey::of(path)).copied()
    }

    /// Look up by canonical key directly (what `ModuleTable` keys on).
    pub fn lookup_key(&self, key: &ModuleKey) -> Option<ModuleId> {
        self.by_key.get(key).copied()
    }

    /// Resolve an id back to its path (for translating a `DefId` to a file
    /// URI in the LSP).
    pub fn path(&self, id: ModuleId) -> Option<&ModulePath> {
        self.paths.get(id.0 as usize)
    }

    /// Every interned module in id order. Infallible by construction — it
    /// walks the dense id-indexed store directly — so a caller mirroring the
    /// first-seen id assignment cannot skip an id (and thereby renumber every
    /// later one) the way a fallible per-id lookup could.
    pub fn iter(&self) -> impl Iterator<Item = (ModuleId, &ModulePath)> {
        self.paths
            .iter()
            .enumerate()
            .map(|(i, p)| (ModuleId(i as u32), p))
    }

    /// Every interned path in id order: the `i`th item is `ModuleId(i)`'s.
    pub fn paths(&self) -> impl Iterator<Item = &ModulePath> {
        self.iter().map(|(_, p)| p)
    }

    pub fn len(&self) -> usize {
        self.paths.len()
    }

    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }
}

// ============================================================================
// DefId / Definition / Reference
// ============================================================================

/// Canonical identity of a definition: the module that owns it, the span of
/// its declaring name, and what kind of entity it is. `Copy` (so it can be
/// stamped onto `Scheme`/`TypeInfo`-style data) and usable as a map key.
///
/// `Hash`/`Eq` cover all three fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DefId {
    pub module: ModuleId,
    pub span: Span,
    pub entity: EntityKind,
}

impl DefId {
    pub fn new(module: ModuleId, span: Span, entity: EntityKind) -> Self {
        DefId {
            module,
            span,
            entity,
        }
    }
}

/// Kind-specific payload of a [`Definition`]. One variant per [`EntityKind`],
/// carrying exactly the data that kind — and only that kind — owns, so a
/// function cannot exist without its parameter names and a constant can never
/// carry a `ctor_of` edge. The flat-`Option` predecessor let any field be set
/// on any kind (and let a mistyped `DefId` silently drop the payload).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DefinitionKind {
    /// A `let`/parameter/match binder — a local value.
    Value,
    Function {
        /// The parameter names, for rendering `fn(path String)` in hover.
        /// Documentation only — see `module::ExportedValue::param_names`.
        param_names: Vec<String>,
    },
    Constant,
    Constructor {
        /// The `DefId` of the type that declares this constructor.
        ///
        /// Reachability follows it, so using `Config(..)` keeps `type Config`
        /// alive. Without it, a type whose only mention is its own constructor
        /// — every single-constructor `type Config { name String }` — was
        /// reported unused. It is NOT an occurrence: `find-references` on the
        /// type must not list the constructor's declaration, and renaming one
        /// must not rename the other — except a record-shorthand type, whose
        /// constructor shares the declaring token/span and so forms one rename
        /// class with it (see `rename::ReferenceGraph::rename_with`). `None`
        /// only for a definition synthesised from a hydrated stdlib interface,
        /// which carries no type `DefId`.
        ctor_of: Option<DefId>,
        /// The constructor's field labels, mirrored for hover (they also live
        /// in `ValueKind::Constructor.field_labels`).
        param_names: Vec<String>,
    },
    Type,
    ModuleAlias {
        /// Span of the whole `import ...` declaration this alias binds — the
        /// boundary that tells an item's binding occurrence (inside it) from a
        /// real use (outside it). Equals `defid.span` for a non-aliased
        /// import; for `import a/b as c.{item}` `defid.span` is only the `c`
        /// identifier while `decl_span` still covers the full statement.
        decl_span: Span,
        /// The module this alias imports — the alias→imported-module edge
        /// LSP module navigation follows (see
        /// [`ReferenceGraph::module_named_at`]).
        imports_module: Option<ModuleId>,
    },
    Field,
}

impl DefinitionKind {
    /// The [`EntityKind`] this payload belongs to; must agree with the
    /// `DefId.entity` of the definition carrying it.
    pub fn entity(&self) -> EntityKind {
        match self {
            DefinitionKind::Value => EntityKind::Value,
            DefinitionKind::Function { .. } => EntityKind::Function,
            DefinitionKind::Constant => EntityKind::Constant,
            DefinitionKind::Constructor { .. } => EntityKind::Constructor,
            DefinitionKind::Type => EntityKind::Type,
            DefinitionKind::ModuleAlias { .. } => EntityKind::ModuleAlias,
            DefinitionKind::Field => EntityKind::Field,
        }
    }
}

/// A declared name: its canonical id, the source name as written, the span of
/// the declaring identifier, its doc comment, visibility, and the kind-specific
/// payload ([`DefinitionKind`]).
///
/// `alias_of` links an import-alias binding (the `Y` of `import a.{X as Y}`) to
/// the canonical definition it stands for (`X`). It is the goto-def/hover
/// *chain* edge: those queries follow it to the real declaration, while
/// find-references and rename stay anchored on this alias, so renaming `Y` does
/// not rewrite `X` (and vice versa). `None` for every ordinary definition.
#[derive(Debug, Clone)]
pub struct Definition {
    pub defid: DefId,
    pub name: String,
    pub doc: Option<String>,
    pub kind: DefinitionKind,
    pub is_pub: bool,
    pub alias_of: Option<DefId>,
}

impl Definition {
    /// Build a definition from its location and payload. The `DefId` is
    /// derived here — `DefId.entity` always equals `kind.entity()` — so a
    /// definition whose id disagrees with its payload is unrepresentable.
    pub fn new(
        module: ModuleId,
        span: Span,
        name: impl Into<String>,
        doc: Option<String>,
        is_pub: bool,
        kind: DefinitionKind,
    ) -> Self {
        Definition {
            defid: DefId::new(module, span, kind.entity()),
            name: name.into(),
            doc,
            kind,
            is_pub,
            alias_of: None,
        }
    }

    /// The span of the declaring identifier; lives on the [`DefId`].
    pub fn span(&self) -> Span {
        self.defid.span
    }

    /// What kind of entity this definition is; lives on the [`DefId`].
    pub fn entity(&self) -> EntityKind {
        self.defid.entity
    }

    /// A function's parameter names / a constructor's field labels, for
    /// rendering `fn(path String)` in hover. Empty for every other kind.
    pub fn param_names(&self) -> &[String] {
        match &self.kind {
            DefinitionKind::Function { param_names }
            | DefinitionKind::Constructor { param_names, .. } => param_names,
            _ => &[],
        }
    }

    /// For a constructor: the declaring type's `DefId` (see
    /// [`DefinitionKind::Constructor`]). `None` for every other kind.
    pub fn ctor_of(&self) -> Option<DefId> {
        match self.kind {
            DefinitionKind::Constructor { ctor_of, .. } => ctor_of,
            _ => None,
        }
    }

    /// The full extent of the declaration: the whole `import ...` statement
    /// for a module alias (see [`DefinitionKind::ModuleAlias`]), the declaring
    /// identifier for everything else.
    pub fn decl_span(&self) -> Span {
        match self.kind {
            DefinitionKind::ModuleAlias { decl_span, .. } => decl_span,
            _ => self.defid.span,
        }
    }

    /// For a module alias: the module it imports (see
    /// [`DefinitionKind::ModuleAlias`]). `None` for every other kind.
    pub fn imports_module(&self) -> Option<ModuleId> {
        match self.kind {
            DefinitionKind::ModuleAlias { imports_module, .. } => imports_module,
            _ => None,
        }
    }

    /// Whether this definition belongs on a symbol surface
    /// (`textDocument/documentSymbol`, `workspace/symbol`).
    ///
    /// `EntityKind::Value` covers `let`/parameter/match/destructure binders —
    /// local bindings recorded in the graph for goto-def/find-refs/rename. They
    /// are intentionally excluded here so the editor outline and the workspace
    /// symbol picker stay a list of a module's structural declarations rather
    /// than every local in every function body. This gates only the symbol
    /// projection; the underlying [`definitions`](ReferenceGraph::definitions)
    /// iteration that resolution and reachability walk is unaffected.
    pub fn is_symbol_listable(&self) -> bool {
        self.entity() != EntityKind::Value
    }
}

/// A single use of a name: where it occurs, how, and which definition it
/// resolves to.
#[derive(Debug, Clone, Copy)]
pub struct Reference {
    pub span: Span,
    pub kind: ReferenceKind,
    pub target: DefId,
}

impl Reference {
    pub fn new(span: Span, kind: ReferenceKind, target: DefId) -> Self {
        Reference { span, kind, target }
    }
}

/// An occurrence located at workspace scope: a [`Reference`] plus the module
/// it physically appears in (the reference's `target` says where it points;
/// this says where it *is*).
#[derive(Debug, Clone, Copy)]
pub struct ResolvedRef {
    pub module: ModuleId,
    pub span: Span,
    pub kind: ReferenceKind,
}

/// One recorded occurrence: the [`Reference`] plus the definition it is
/// lexically nested in (`owner` — the enclosing `fn`/`const`/`type`, `None`
/// for a bare top-level expression). The owner is the definition→definition
/// edge channel the workspace reachability hook walks for dead-code analysis;
/// it is deliberately *not* a field of `Reference`, which stays exactly
/// `{ span, kind, target }`.
#[derive(Debug, Clone, Copy)]
pub struct Occurrence {
    pub owner: Option<DefId>,
    pub reference: Reference,
}

// ============================================================================
// ModuleReferences — per-module storage
// ============================================================================

/// Everything the reference graph knows about one module: its declared
/// definitions and every name occurrence inside it (each [`Occurrence`]
/// carrying its reference plus the definition it is nested in).
#[derive(Debug, Clone)]
pub struct ModuleReferences {
    module: ModuleId,
    /// The module's own doc comment (the `/** */` at line 0 of its source),
    /// verbatim. Rendered by LSP hover on an import path segment or a
    /// `q.member` qualifier.
    doc: Option<String>,
    definitions: IndexMap<DefId, Definition>,
    occurrences: Vec<Occurrence>,
    /// Declared name → the defs declared under it in this module.
    name_to_defs: HashMap<String, Vec<DefId>>,
    /// Source line → every occurrence/definition span touching that line,
    /// built lazily on the first [`cursor_hit`](Self::cursor_hit) and
    /// invalidated by any mutation. Makes a position query O(spans on the
    /// queried line) instead of O(all occurrences + all definitions); spans
    /// are identifier-sized, so a line holds a handful of entries.
    line_index: OnceCell<HashMap<i32, Vec<LineEntry>>>,
}

/// One candidate in the [`ModuleReferences::line_index`]. `rank` reproduces
/// the original linear-scan tie-break order — occurrences in insertion order,
/// then definitions in declaration order — so equal-width matches resolve
/// identically to the pre-index scan.
#[derive(Debug, Clone, Copy)]
struct LineEntry {
    span: Span,
    target: DefId,
    kind: ReferenceKind,
    rank: (u8, u32),
}

/// What the cursor is sitting on, as resolved by
/// [`ModuleReferences::cursor_hit`]: the matched definition (`target`), the
/// exact source `range` of the matched occurrence or declaration name, and the
/// `kind` of reference it was.
pub(crate) struct CursorHit {
    pub(crate) target: DefId,
    pub(crate) range: Span,
    pub(crate) kind: ReferenceKind,
}

impl ModuleReferences {
    pub fn new(module: ModuleId) -> Self {
        ModuleReferences {
            module,
            doc: None,
            definitions: IndexMap::new(),
            occurrences: Vec::new(),
            name_to_defs: HashMap::new(),
            line_index: OnceCell::new(),
        }
    }

    pub fn module(&self) -> ModuleId {
        self.module
    }

    pub fn set_doc(&mut self, doc: Option<String>) {
        self.doc = doc;
    }

    pub fn doc(&self) -> Option<&str> {
        self.doc.as_deref()
    }

    /// Register a declared name. Re-declaring the same `DefId` overwrites the
    /// previous record (last write wins, matching the existing flat env);
    /// `name_to_defs` is re-keyed when the overwrite changes the name so the
    /// index always matches the stored definitions.
    pub fn add_definition(&mut self, def: Definition) {
        let defid = def.defid;
        let name = def.name.clone();
        if let Some(old) = self.definitions.insert(defid, def) {
            if old.name != name {
                if let Some(bucket) = self.name_to_defs.get_mut(&old.name) {
                    bucket.retain(|d| *d != defid);
                    if bucket.is_empty() {
                        self.name_to_defs.remove(&old.name);
                    }
                }
                self.name_to_defs.entry(name).or_default().push(defid);
            }
        } else {
            self.name_to_defs.entry(name).or_default().push(defid);
        }
        self.line_index.take();
    }

    /// Record an occurrence. `owner` is the definition this occurrence is
    /// nested within, used for dead-code reachability.
    pub fn add_reference(&mut self, owner: Option<DefId>, reference: Reference) {
        self.occurrences.push(Occurrence { owner, reference });
        self.line_index.take();
    }

    pub fn definitions(&self) -> impl Iterator<Item = &Definition> {
        self.definitions.values()
    }

    pub fn definition(&self, id: DefId) -> Option<&Definition> {
        self.definitions.get(&id)
    }

    pub fn occurrences(&self) -> &[Occurrence] {
        &self.occurrences
    }

    /// The defs declared under `name` in this module (for symbol queries).
    pub fn defs_named(&self, name: &str) -> &[DefId] {
        self.name_to_defs.get(name).map_or(&[], Vec::as_slice)
    }

    /// Forward lookup: the definition a position resolves to. The tightest
    /// occurrence containing the point wins; failing that, the point may be
    /// sitting on a definition's own declaring name. `None` if nothing covers
    /// it.
    pub fn resolve_position(&self, line: i32, col: i32) -> Option<DefId> {
        self.cursor_hit(line, col).map(|h| h.target)
    }

    /// The tightest thing the cursor is on: consult the per-line index, gate
    /// by [`Span::contains`], and keep the narrowest [`Span::width`] match
    /// (occurrences beat definitions on a tie, in insertion order — the
    /// `rank` key preserves the original scan order). A position on a
    /// declaration's own name resolves to itself with kind
    /// [`ReferenceKind::Definition`], even when no explicit `Definition`
    /// occurrence was recorded. Backs both
    /// [`resolve_position`](Self::resolve_position) (which keeps only the
    /// `target`) and the rename layer (which also needs the matched `range` and
    /// `kind`).
    pub(crate) fn cursor_hit(&self, line: i32, col: i32) -> Option<CursorHit> {
        let index = self.line_index.get_or_init(|| self.build_line_index());
        index
            .get(&line)?
            .iter()
            .filter(|e| e.span.contains(line, col))
            .min_by_key(|e| (e.span.width(), e.rank))
            .map(|e| CursorHit {
                target: e.target,
                range: e.span,
                kind: e.kind,
            })
    }

    /// Build the line → candidate-spans index `cursor_hit` queries. A span is
    /// registered on every line it touches (multi-line spans are rare —
    /// occurrences and declaring names are identifier-sized), so any span
    /// containing a point is reachable from that point's line bucket.
    fn build_line_index(&self) -> HashMap<i32, Vec<LineEntry>> {
        let mut index: HashMap<i32, Vec<LineEntry>> = HashMap::new();
        let occurrences = self
            .occurrences
            .iter()
            .map(|o| (o.reference.span, o.reference.target, o.reference.kind))
            .enumerate()
            .map(|(i, (span, target, kind))| (span, target, kind, (0u8, i as u32)));
        let definitions = self
            .definitions
            .values()
            .map(|d| (d.span(), d.defid, ReferenceKind::Definition))
            .enumerate()
            .map(|(i, (span, target, kind))| (span, target, kind, (1u8, i as u32)));
        for (span, target, kind, rank) in occurrences.chain(definitions) {
            for line in span.start_line..=span.end_line {
                index.entry(line).or_default().push(LineEntry {
                    span,
                    target,
                    kind,
                    rank,
                });
            }
        }
        index
    }
}

// ============================================================================
// ReferenceGraph — workspace scope
// ============================================================================

/// Accumulates the workspace module set — interned module paths plus each
/// module's [`ModuleReferences`] — and seals it into an immutable
/// [`ReferenceGraph`] via [`finish`](Self::finish), which computes the
/// workspace reverse index exactly once, from the final module set.
///
/// The graph exposes no mutation, so "index out of sync with the module set"
/// is unrepresentable: there is no rebuild step to forget and no way to
/// insert a module after the index was computed. Eviction is by omission —
/// each `check` builds a fresh graph from the live module set, so an evicted
/// module can never leave a dangling reverse edge.
#[derive(Debug, Default)]
pub struct ReferenceGraphBuilder {
    interner: ModuleInterner,
    modules: IndexMap<ModuleId, Rc<ModuleReferences>>,
}

impl ReferenceGraphBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Intern `path`, returning its stable id (creating one on first sight).
    pub fn intern_module(&mut self, path: &ModulePath) -> ModuleId {
        self.interner.intern(path)
    }

    /// Install (or replace) a module's references.
    ///
    /// Takes an `Rc`: an unchanged module's persisted refs are re-inserted on
    /// every `check` (see `build_reference_graph`), so the caller hands over a
    /// shared pointer and this is an O(1) refcount bump rather than an
    /// O(module occurrences+defs) deep clone.
    pub fn insert(&mut self, refs: Rc<ModuleReferences>) {
        self.modules.insert(refs.module(), refs);
    }

    /// Test-only sugar over [`Self::insert`] for freshly built refs.
    #[cfg(test)]
    pub(crate) fn insert_module(&mut self, refs: ModuleReferences) {
        self.insert(Rc::new(refs));
    }

    /// Compute the workspace reverse index from the final module set and seal
    /// the graph. O(total occurrences); correctness over cleverness — building
    /// the whole index from the whole set trivially guarantees no stale edge
    /// survives a module eviction, and doing it once here costs O(total
    /// occurrences) instead of O(modules x total occurrences) repeated full
    /// scans had each insert re-indexed.
    pub fn finish(self) -> ReferenceGraph {
        let mut refs_by_def: HashMap<DefId, Vec<ResolvedRef>> = HashMap::new();
        for (&module, mr) in &self.modules {
            for occ in mr.occurrences() {
                refs_by_def
                    .entry(occ.reference.target)
                    .or_default()
                    .push(ResolvedRef {
                        module,
                        span: occ.reference.span,
                        kind: occ.reference.kind,
                    });
            }
        }
        ReferenceGraph {
            interner: self.interner,
            modules: self.modules,
            refs_by_def,
        }
    }
}

/// The workspace-wide reference graph: the module-path interner, every
/// module's [`ModuleReferences`], and the derived workspace reverse index
/// (`DefId` → every occurrence across *all* modules) used by find-references,
/// rename, and the unused/dead-code diagnostics.
///
/// Immutable once built — construction goes through [`ReferenceGraphBuilder`],
/// whose `finish` computes the reverse index from the final module set, so the
/// index can never be stale.
#[derive(Debug, Default)]
pub struct ReferenceGraph {
    interner: ModuleInterner,
    /// `Rc` so a `build_reference_graph` that re-inserts every unchanged
    /// module's persisted refs on each `check` is an O(modules) refcount
    /// bump, not an O(total workspace occurrences+defs) deep copy.
    modules: IndexMap<ModuleId, Rc<ModuleReferences>>,
    refs_by_def: HashMap<DefId, Vec<ResolvedRef>>,
}

impl ReferenceGraph {
    /// The empty graph (placeholder until a real one is built).
    pub fn new() -> Self {
        Self::default()
    }

    // --- Module identity ---

    pub fn module_id(&self, path: &ModulePath) -> Option<ModuleId> {
        self.interner.lookup(path)
    }

    pub fn module_id_by_key(&self, key: &ModuleKey) -> Option<ModuleId> {
        self.interner.lookup_key(key)
    }

    pub fn module_path(&self, id: ModuleId) -> Option<&ModulePath> {
        self.interner.path(id)
    }

    /// Every interned module, in interning order.
    pub fn modules(&self) -> impl Iterator<Item = (ModuleId, &ModulePath)> {
        self.interner.iter()
    }

    pub fn module_refs(&self, id: ModuleId) -> Option<&ModuleReferences> {
        self.modules.get(&id).map(|m| &**m)
    }

    // --- Query API ---

    /// The definition identified by `id`, looked up in its owning module.
    pub fn definition(&self, id: DefId) -> Option<&Definition> {
        self.modules.get(&id.module)?.definition(id)
    }

    /// Follow an import-alias binding (`Definition::alias_of`) to the canonical
    /// definition it stands for — e.g. the `Y` of `import a.{X as Y}` resolves
    /// to `X`. This is the goto-def/hover chain; find-references and rename use
    /// the raw `DefId` so the alias and its target stay separate rename classes.
    /// Bounded against a pathological alias-of-alias cycle; returns `id`
    /// unchanged when it names no alias.
    pub fn canonical(&self, id: DefId) -> DefId {
        let mut cur = id;
        for _ in 0..16 {
            match self.definition(cur).and_then(|d| d.alias_of) {
                Some(next) if next != cur => cur = next,
                _ => break,
            }
        }
        cur
    }

    /// goto-definition: resolve a position to the definition it points at
    /// (following the occurrence to its `target`, possibly cross-module, and
    /// chaining through an import alias to the real declaration).
    pub fn definition_at(&self, module: ModuleId, line: i32, col: i32) -> Option<&Definition> {
        let target = self.modules.get(&module)?.resolve_position(line, col)?;
        self.definition(self.canonical(target))
    }

    /// The doc comment of `module` — the `/** */` at line 0 of its source.
    pub fn module_doc(&self, module: ModuleId) -> Option<&str> {
        self.modules.get(&module)?.doc()
    }

    /// The *module* a position names, if any: the final path segment of an
    /// `import a/b` (an [`ReferenceKind::Import`] occurrence, whose target is
    /// owned by the imported module), or the `q` of a `q.member` access (a
    /// [`ReferenceKind::Qualifier`] occurrence targeting the `ModuleAlias`
    /// binding, which records the module it imports).
    ///
    /// Every other position inside an `import` declaration — the keyword, a
    /// non-final path segment, the `as` alias binding — falls only under the
    /// alias `Definition`'s whole-declaration span, resolves as
    /// [`ReferenceKind::Definition`]/[`ReferenceKind::Alias`], and yields
    /// `None` so those stay non-navigable.
    pub fn module_named_at(&self, module: ModuleId, line: i32, col: i32) -> Option<ModuleId> {
        let hit = self.modules.get(&module)?.cursor_hit(line, col)?;
        match hit.kind {
            ReferenceKind::Import => Some(hit.target.module),
            ReferenceKind::Qualifier => self.definition(hit.target)?.imports_module(),
            _ => None,
        }
    }

    /// The raw `DefId` a position resolves to, without crossing to the owning
    /// module's record.
    pub fn def_id_at(&self, module: ModuleId, line: i32, col: i32) -> Option<DefId> {
        self.modules.get(&module)?.resolve_position(line, col)
    }

    /// find-references: every occurrence of `id` across the whole workspace.
    pub fn references_to(&self, id: DefId) -> &[ResolvedRef] {
        self.refs_by_def.get(&id).map_or(&[], Vec::as_slice)
    }

    /// documentSymbol: every definition declared in `module`.
    pub fn defs_in(&self, module: ModuleId) -> impl Iterator<Item = &Definition> {
        self.modules
            .get(&module)
            .into_iter()
            .flat_map(|m| m.definitions())
    }

    /// workspace/symbol: every definition in the workspace.
    pub fn all_defs(&self) -> impl Iterator<Item = &Definition> {
        self.modules.values().flat_map(|m| m.definitions())
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
fn mp(parts: &[&str]) -> ModulePath {
    parts.iter().map(|s| s.to_string()).collect()
}

#[cfg(test)]
fn def(module: ModuleId, line: i32, c0: i32, c1: i32, kind: EntityKind) -> DefId {
    DefId::new(module, crate::span::Span::single_line(line, c0, c1), kind)
}

/// Test-only default payload for `defid`'s entity: empty parameter names, no
/// `ctor_of`/`imports_module` edges, `decl_span` equal to the declaring span.
#[cfg(test)]
pub(crate) fn stub_kind(defid: DefId) -> DefinitionKind {
    match defid.entity {
        EntityKind::Value => DefinitionKind::Value,
        EntityKind::Function => DefinitionKind::Function {
            param_names: Vec::new(),
        },
        EntityKind::Constant => DefinitionKind::Constant,
        EntityKind::Constructor => DefinitionKind::Constructor {
            ctor_of: None,
            param_names: Vec::new(),
        },
        EntityKind::Type => DefinitionKind::Type,
        EntityKind::ModuleAlias => DefinitionKind::ModuleAlias {
            decl_span: defid.span,
            imports_module: None,
        },
        EntityKind::Field => DefinitionKind::Field,
    }
}

#[cfg(test)]
fn add_ref(
    mr: &mut ModuleReferences,
    owner: Option<DefId>,
    (line, c0, c1): (i32, i32, i32),
    kind: ReferenceKind,
    target: DefId,
) {
    mr.add_reference(
        owner,
        Reference::new(crate::span::Span::single_line(line, c0, c1), kind, target),
    );
}

#[cfg(test)]
fn main_graph() -> (ReferenceGraphBuilder, ModuleId, ModuleReferences) {
    let mut g = ReferenceGraphBuilder::new();
    let m = g.intern_module(&mp(&["main"]));
    let mr = ModuleReferences::new(m);
    (g, m, mr)
}

#[cfg(test)]
fn add_def(
    mr: &mut ModuleReferences,
    m: ModuleId,
    name: &str,
    line: i32,
    kind: EntityKind,
    is_pub: bool,
) -> DefId {
    let d = def(m, line, 3, 3 + name.len() as i32, kind);
    mr.add_definition(Definition::new(
        d.module,
        d.span,
        name,
        None,
        is_pub,
        stub_kind(d),
    ));
    d
}

#[cfg(test)]
mod tests {
    use super::{ReferenceKind as K, *};
    use std::collections::HashSet;

    // ---- ModuleInterner ----

    #[test]
    fn interner_is_stable_and_bijective() {
        let mut it = ModuleInterner::new();
        let a = it.intern(&mp(&["main"]));
        let b = it.intern(&mp(&["al", "array"]));
        let a2 = it.intern(&mp(&["main"]));

        assert_eq!(a, a2, "same path re-interns to the same id");
        assert_ne!(a, b);
        assert_eq!(it.len(), 2, "distinct paths get distinct ids");
        assert_eq!(it.path(a), Some(&mp(&["main"])));
        assert_eq!(it.path(b), Some(&mp(&["al", "array"])));
        assert_eq!(it.lookup(&mp(&["al", "array"])), Some(b));
        assert_eq!(
            it.lookup_key(&crate::module::ModuleKey::for_stdlib(&mp(&["al", "array"]))),
            Some(b)
        );
        assert_eq!(it.lookup(&mp(&["nope"])), None);
        assert_eq!(it.path(ModuleId(99)), None);
    }

    // ---- DefId hashing / equality ----

    #[test]
    fn defid_hash_eq_consistent() {
        let m = ModuleId(3);
        let d1 = def(m, 1, 0, 4, EntityKind::Function);
        let d2 = def(m, 1, 0, 4, EntityKind::Function);
        let d3 = def(m, 1, 0, 4, EntityKind::Type); // entity differs
        let d4 = def(m, 2, 0, 4, EntityKind::Function); // span differs

        assert_eq!(d1, d2);
        assert_ne!(d1, d3);
        assert_ne!(d1, d4);

        let mut set: HashSet<DefId> = HashSet::new();
        set.insert(d1);
        assert!(set.contains(&d2), "equal DefIds hash to the same bucket");
        assert!(!set.contains(&d3));
        assert!(!set.contains(&d4));
    }

    // ---- Span geometry ----

    #[test]
    fn span_containment_is_half_open() {
        let s = Span::single_line(5, 2, 6); // columns [2, 6) on line 5
        assert!(!s.contains(5, 1));
        assert!(s.contains(5, 2));
        assert!(s.contains(5, 5));
        assert!(!s.contains(5, 6), "end column is exclusive");
        assert!(!s.contains(4, 3));

        let p = Span::point(7, 9);
        assert!(p.contains(7, 9));
        assert!(!p.contains(7, 10));
    }

    #[test]
    fn span_width_orders_tightest_first() {
        let narrow = Span::single_line(1, 0, 3);
        let wide = Span::single_line(1, 0, 30);
        let multiline = Span {
            start_line: 1,
            start_column: 0,
            end_line: 3,
            end_column: 0,
        };
        assert!(narrow.width() < wide.width());
        assert!(wide.width() < multiline.width());
    }

    // ---- ModuleReferences: definitions, reverse index, position lookup ----

    #[test]
    fn module_refs_intra_module_reverse_index() {
        let m = ModuleId(0);
        let mut mr = ModuleReferences::new(m);

        let foo = def(m, 1, 3, 6, EntityKind::Function);
        mr.add_definition(Definition::new(
            foo.module,
            foo.span,
            "foo",
            Some("the foo".into()),
            true,
            stub_kind(foo),
        ));

        // two uses of foo inside some other (owner) definition
        let owner = def(m, 10, 3, 7, EntityKind::Function);
        add_ref(&mut mr, Some(owner), (11, 4, 7), K::Unqualified, foo);
        add_ref(&mut mr, Some(owner), (12, 4, 7), K::Unqualified, foo);

        assert_eq!(mr.defs_named("foo"), &[foo]);
        assert_eq!(mr.definition(foo).map(|d| d.name.as_str()), Some("foo"));
        assert_eq!(mr.occurrences().len(), 2);
    }

    #[test]
    fn resolve_position_prefers_tightest_then_falls_back_to_def_name() {
        let m = ModuleId(0);
        let mut mr = ModuleReferences::new(m);

        let target = def(m, 1, 0, 3, EntityKind::Function);
        mr.add_definition(Definition::new(
            target.module,
            target.span,
            "fn",
            None,
            false,
            stub_kind(target),
        ));

        // A wide occurrence and a tighter one overlapping the same point.
        let wide = def(m, 9, 0, 1, EntityKind::Value);
        add_ref(&mut mr, None, (20, 0, 40), K::Unqualified, wide);
        add_ref(&mut mr, None, (20, 10, 14), K::Unqualified, target);

        // Inside the tight span -> tightest wins.
        assert_eq!(mr.resolve_position(20, 12), Some(target));
        // Inside only the wide span -> wide.
        assert_eq!(mr.resolve_position(20, 2), Some(wide));
        // On the definition's own name -> the definition itself.
        assert_eq!(mr.resolve_position(1, 1), Some(target));
        // Nowhere.
        assert_eq!(mr.resolve_position(99, 0), None);
    }

    // ---- ReferenceGraph: cross-module reverse index + queries ----

    fn two_module_graph() -> (ReferenceGraph, ModuleId, ModuleId, DefId) {
        let mut g = ReferenceGraphBuilder::new();
        let lib = g.intern_module(&mp(&["lib"]));
        let app = g.intern_module(&mp(&["app"]));

        // lib defines `helper` (pub) at line 1.
        let helper = def(lib, 1, 3, 9, EntityKind::Function);
        let mut lib_mr = ModuleReferences::new(lib);
        lib_mr.add_definition(Definition::new(
            helper.module,
            helper.span,
            "helper",
            Some("doc".into()),
            true,
            stub_kind(helper),
        ));
        g.insert_module(lib_mr);

        // app uses `helper` twice (qualified), inside app's `run`.
        let run = def(app, 1, 3, 6, EntityKind::Function);
        let mut app_mr = ModuleReferences::new(app);
        app_mr.add_definition(Definition::new(
            run.module,
            run.span,
            "run",
            None,
            true,
            stub_kind(run),
        ));
        add_ref(&mut app_mr, Some(run), (2, 4, 14), K::Qualified, helper);
        add_ref(&mut app_mr, Some(run), (3, 4, 14), K::Qualified, helper);
        g.insert_module(app_mr);

        (g.finish(), lib, app, helper)
    }

    #[test]
    fn graph_cross_module_goto_def_and_find_refs() {
        let (g, _lib, app, helper) = two_module_graph();

        // goto-def from a use in `app` lands on the `lib` definition.
        let d = g
            .definition_at(app, 2, 8)
            .expect("position resolves to a definition");
        assert_eq!(d.name, "helper");
        assert_eq!(d.defid, helper);
        assert_eq!(d.doc.as_deref(), Some("doc"));

        // hover surfaces the same definition record.
        assert_eq!(g.definition_at(app, 3, 8).map(|d| d.defid), Some(helper));

        // find-references returns both occurrences, both attributed to `app`.
        let refs = g.references_to(helper);
        assert_eq!(refs.len(), 2);
        assert!(refs.iter().all(|r| r.module == app));
        assert!(refs.iter().all(|r| r.kind == K::Qualified));
    }

    #[test]
    fn graph_symbol_queries() {
        let (g, lib, _app, helper) = two_module_graph();

        let lib_syms: Vec<&str> = g.defs_in(lib).map(|d| d.name.as_str()).collect();
        assert_eq!(lib_syms, vec!["helper"]);

        let mut all: Vec<&str> = g.all_defs().map(|d| d.name.as_str()).collect();
        all.sort_unstable();
        assert_eq!(all, vec!["helper", "run"]);

        assert_eq!(
            g.definition(helper).map(|d| d.name.as_str()),
            Some("helper")
        );
    }

    #[test]
    fn omitting_a_referencing_module_leaves_no_dangling_reverse_edges() {
        // Production never eagerly removes a module: `build_reference_graph`
        // builds a fresh graph from the live set via `ReferenceGraphBuilder`,
        // so a module is "evicted" purely by omission. Build from only `lib`,
        // omitting the `app` that referenced `helper`; since `finish` computes
        // the index as a pure function of the final module set, no reverse
        // edge into `helper` can survive.
        let mut g = ReferenceGraphBuilder::new();
        let lib = g.intern_module(&mp(&["lib"]));
        let helper = def(lib, 1, 3, 9, EntityKind::Function);
        let mut lib_mr = ModuleReferences::new(lib);
        lib_mr.add_definition(Definition::new(
            helper.module,
            helper.span,
            "helper",
            Some("doc".into()),
            true,
            stub_kind(helper),
        ));
        g.insert(Rc::new(lib_mr));
        let g = g.finish();

        assert_eq!(
            g.references_to(helper).len(),
            0,
            "no reverse edge may dangle when the referencing module is omitted"
        );
        // `helper`'s own definition still resolvable from lib.
        assert_eq!(
            g.definition(helper).map(|d| d.name.as_str()),
            Some("helper")
        );
    }

    #[test]
    fn value_binders_are_excluded_from_symbol_surfaces() {
        // Local `let`/param/match/destructure binders are recorded as
        // `EntityKind::Value` definitions so goto-def / find-refs / rename
        // resolve on them. They must NOT reach the documentSymbol outline or
        // the workspace/symbol picker, which list a module's structural
        // declarations only. The single chokepoint is
        // `Definition::is_symbol_listable`, applied by every symbol projection.
        let (mut g, m, mut mr) = main_graph();
        let func = add_def(&mut mr, m, "compute", 1, EntityKind::Function, true);
        let local = add_def(&mut mr, m, "tmp", 2, EntityKind::Value, false);
        g.insert_module(mr);
        let g = g.finish();

        // The predicate: only the local `Value` is unlistable.
        assert!(
            g.definition(func)
                .expect("fn def present")
                .is_symbol_listable()
        );
        assert!(
            !g.definition(local)
                .expect("value def present")
                .is_symbol_listable()
        );

        // The underlying iteration stays UNFILTERED — resolution and
        // reachability (goto-def / find-refs / rename / dead-code) still walk
        // the `Value` binder.
        let raw: Vec<&str> = g.defs_in(m).map(|d| d.name.as_str()).collect();
        assert!(raw.contains(&"compute") && raw.contains(&"tmp"), "{raw:?}");

        // The symbol projection — what all four documentSymbol/workspaceSymbol
        // sites apply — keeps the function and drops the local.
        let surfaced: Vec<&str> = g
            .defs_in(m)
            .filter(|d| d.is_symbol_listable())
            .map(|d| d.name.as_str())
            .collect();
        assert_eq!(surfaced, vec!["compute"]);
    }
}
