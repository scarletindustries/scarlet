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
//! alters inference or codegen.
//!
//! Module identity is interned to a [`ModuleId`] (`Copy`) rather than carried
//! as a `Vec<String>` so a `DefId` can ride on `Copy` types and survive
//! incremental recompiles independent of any one inference engine's arenas.

use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;

use indexmap::IndexMap;

use crate::diagnostic::Diagnostic;
use crate::module::{ModulePath, path_key};
use crate::span::Span;

pub mod rename;

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

    /// This kind as an LSP `SymbolKind` number (the wire enum used by
    /// `textDocument/documentSymbol` and `workspace/symbol`).
    pub fn lsp_symbol_kind(self) -> i32 {
        match self {
            EntityKind::Function => 12,
            EntityKind::Value => 13,
            EntityKind::Constant => 14,
            EntityKind::Constructor => 9,
            EntityKind::Type => 23,
            EntityKind::ModuleAlias => 2,
            EntityKind::Field => 8,
        }
    }
}

/// How a name is being used at an occurrence site. Mirrors Gleam's
/// reference-kind set, adapted to al's import syntax.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReferenceKind {
    /// `module.member` — qualified through an import alias.
    Qualified,
    /// A bare `name` resolved through the local/value environment.
    Unqualified,
    /// The module path in an `import a/b` declaration.
    Import,
    /// The `as` alias in `import a/b as c` (binds a `ModuleAlias`).
    Alias,
    /// The defining occurrence itself (a `fn`/`type`/`let` name at its
    /// declaration site). Lets goto-def on a declaration resolve to itself
    /// and lets find-references include the definition.
    Definition,
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
    by_key: HashMap<String, ModuleId>,
}

impl ModuleInterner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Intern `path`, returning its stable id (creating one on first sight).
    pub fn intern(&mut self, path: &ModulePath) -> ModuleId {
        let key = path_key(path);
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
        self.by_key.get(&path_key(path)).copied()
    }

    /// Look up by the `a/b/c` path key directly (what `ModuleTable` keys on).
    pub fn lookup_key(&self, key: &str) -> Option<ModuleId> {
        self.by_key.get(key).copied()
    }

    /// Resolve an id back to its path (for translating a `DefId` to a file
    /// URI in the LSP).
    pub fn path(&self, id: ModuleId) -> Option<&ModulePath> {
        self.paths.get(id.0 as usize)
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

/// A declared name: its canonical id, the source name as written, the span of
/// the declaring identifier, its doc comment, visibility, and entity kind.
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
    pub is_pub: bool,
    pub alias_of: Option<DefId>,
}

impl Definition {
    pub fn new(defid: DefId, name: impl Into<String>, doc: Option<String>, is_pub: bool) -> Self {
        Definition {
            defid,
            name: name.into(),
            doc,
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

// ============================================================================
// Span geometry
// ============================================================================

/// Half-open containment: `[start, end)` in (line, column) order. A
/// `point_span(l, c)` (end column `c + 1`) therefore contains exactly column
/// `c` on line `l`.
pub fn span_contains(s: &Span, line: i32, col: i32) -> bool {
    let start = (s.start_line, s.start_column);
    let end = (s.end_line, s.end_column);
    let p = (line, col);
    start <= p && p < end
}

/// A monotone "width" key used to pick the tightest of several spans containing
/// a point. Ordered lexicographically as `(line-span, col-span)`, so any
/// single-line span sorts before any multi-line one; exact width is irrelevant,
/// only the ordering is.
pub fn span_width(s: &Span) -> (i32, i32) {
    (s.end_line - s.start_line, s.end_column - s.start_column)
}

/// Whether `inner` lies fully inside `outer` (both half-open, `(line, col)`
/// order). Used to tell an imported item's *binding* occurrence — which the
/// compiler records inside the `import` declaration's span — apart from a real
/// *use* of that imported name elsewhere in the module.
fn span_within(inner: &Span, outer: &Span) -> bool {
    let o = (
        (outer.start_line, outer.start_column),
        (outer.end_line, outer.end_column),
    );
    let i = (
        (inner.start_line, inner.start_column),
        (inner.end_line, inner.end_column),
    );
    o.0 <= i.0 && i.1 <= o.1
}

/// The smallest `Span` covering both `a` and `b` (`(line, col)` order). Used to
/// recover an `import` declaration's full lexical extent from its constituent
/// occurrences.
fn span_union(a: &Span, b: &Span) -> Span {
    let (start_line, start_column) = std::cmp::min(
        (a.start_line, a.start_column),
        (b.start_line, b.start_column),
    );
    let (end_line, end_column) =
        std::cmp::max((a.end_line, a.end_column), (b.end_line, b.end_column));
    Span {
        start_line,
        start_column,
        end_line,
        end_column,
    }
}

// ============================================================================
// ModuleReferences — per-module storage
// ============================================================================

/// Everything the reference graph knows about one module: its declared
/// definitions and every name occurrence inside it.
///
/// `occurrence_owner` is index-aligned with `occurrences`: it records the
/// definition each occurrence is lexically nested in (the enclosing `fn`/
/// `const`/`type`, `None` for a bare top-level expression). It is the
/// definition→definition edge channel the workspace reachability hook walks
/// for dead-code analysis; it is deliberately *not* a field of `Reference`,
/// which stays exactly `{ span, kind, target }`.
#[derive(Debug, Clone)]
pub struct ModuleReferences {
    module: ModuleId,
    definitions: IndexMap<DefId, Definition>,
    occurrences: Vec<Reference>,
    occurrence_owner: Vec<Option<DefId>>,
    /// Declared name → the defs declared under it in this module.
    name_to_defs: HashMap<String, Vec<DefId>>,
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
            definitions: IndexMap::new(),
            occurrences: Vec::new(),
            occurrence_owner: Vec::new(),
            name_to_defs: HashMap::new(),
        }
    }

    pub fn module(&self) -> ModuleId {
        self.module
    }

    /// Register a declared name. Re-declaring the same `DefId` overwrites the
    /// previous record (last write wins, matching the existing flat env).
    pub fn add_definition(&mut self, def: Definition) {
        let defid = def.defid;
        if self.definitions.insert(defid, def.clone()).is_none() {
            self.name_to_defs
                .entry(def.name.clone())
                .or_default()
                .push(defid);
        }
    }

    /// Record an occurrence. `owner` is the definition this occurrence is
    /// nested within, used for dead-code reachability.
    pub fn add_reference(&mut self, owner: Option<DefId>, reference: Reference) {
        self.occurrences.push(reference);
        self.occurrence_owner.push(owner);
    }

    pub fn definitions(&self) -> impl Iterator<Item = &Definition> {
        self.definitions.values()
    }

    pub fn definition(&self, id: DefId) -> Option<&Definition> {
        self.definitions.get(&id)
    }

    pub fn occurrences(&self) -> &[Reference] {
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

    /// The tightest thing the cursor is on: walk occurrences then definitions,
    /// gate by [`span_contains`], and keep the narrowest [`span_width`] match
    /// (the first wins on a tie). A position on a declaration's own name
    /// resolves to itself with kind [`ReferenceKind::Definition`], even when no
    /// explicit `Definition` occurrence was recorded. Backs both
    /// [`resolve_position`](Self::resolve_position) (which keeps only the
    /// `target`) and the rename layer (which also needs the matched `range` and
    /// `kind`).
    pub(crate) fn cursor_hit(&self, line: i32, col: i32) -> Option<CursorHit> {
        let occurrences = self.occurrences.iter().map(|o| (o.span, o.target, o.kind));
        let definitions = self
            .definitions
            .values()
            .map(|d| (d.span(), d.defid, ReferenceKind::Definition));
        occurrences
            .chain(definitions)
            .filter(|(s, ..)| span_contains(s, line, col))
            .min_by_key(|(s, ..)| span_width(s))
            .map(|(span, target, kind)| CursorHit {
                target,
                range: span,
                kind,
            })
    }
}

// ============================================================================
// ReferenceGraph — workspace scope
// ============================================================================

/// The workspace-wide reference graph: the module-path interner, every
/// module's [`ModuleReferences`], and the derived workspace reverse index
/// (`DefId` → every occurrence across *all* modules) used by find-references,
/// rename, and the unused/dead-code diagnostics.
///
/// Derived indexes are rebuilt wholesale from the live module set whenever the
/// set changes ([`rebuild`](Self::rebuild)), so an evicted module can never
/// leave a dangling reverse edge.
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
    pub fn new() -> Self {
        Self::default()
    }

    // --- Module identity ---

    pub fn intern_module(&mut self, path: &ModulePath) -> ModuleId {
        self.interner.intern(path)
    }

    pub fn module_id(&self, path: &ModulePath) -> Option<ModuleId> {
        self.interner.lookup(path)
    }

    pub fn module_id_by_key(&self, key: &str) -> Option<ModuleId> {
        self.interner.lookup_key(key)
    }

    pub fn module_path(&self, id: ModuleId) -> Option<&ModulePath> {
        self.interner.path(id)
    }

    // --- Mutation ---

    /// Install (or replace) a module's references and rebuild the workspace
    /// reverse index.
    #[cfg(test)]
    pub fn insert_module(&mut self, refs: ModuleReferences) {
        self.insert_module_deferred(Rc::new(refs));
        self.rebuild();
    }

    /// Install (or replace) a module's references *without* rebuilding the
    /// reverse index. For bulk population (see `Compiler::build_reference_graph`)
    /// where every loaded module is inserted back-to-back: insert them all, then
    /// call [`Self::rebuild`] exactly once. `rebuild` is a pure function of the
    /// live module set, so the deferred path yields a byte-identical graph in
    /// O(total occurrences) instead of O(modules x total occurrences) repeated
    /// full scans on every incremental `check`.
    ///
    /// Takes an `Rc`: an unchanged module's persisted refs are re-inserted on
    /// every `check`, so the caller hands over a shared pointer and this is an
    /// O(1) refcount bump rather than an O(module occurrences+defs) deep clone.
    pub(crate) fn insert_module_deferred(&mut self, refs: Rc<ModuleReferences>) {
        self.modules.insert(refs.module(), refs);
    }

    pub fn module_refs(&self, id: ModuleId) -> Option<&ModuleReferences> {
        self.modules.get(&id).map(|m| &**m)
    }

    /// Recompute the workspace reverse index from the live module set. O(total
    /// occurrences); correctness over cleverness — a full rebuild trivially
    /// guarantees no stale edge survives a module eviction.
    pub fn rebuild(&mut self) {
        let mut refs_by_def: HashMap<DefId, Vec<ResolvedRef>> = HashMap::new();
        for (&module, mr) in &self.modules {
            for occ in mr.occurrences() {
                refs_by_def
                    .entry(occ.target)
                    .or_default()
                    .push(ResolvedRef {
                        module,
                        span: occ.span,
                        kind: occ.kind,
                    });
            }
        }
        self.refs_by_def = refs_by_def;
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

    // --- Reachability hooks (unused-import / dead-code) ---

    /// Reachability roots: every `pub` definition. The non-LSP consumer adds
    /// the entry module's top-level definitions to this set before walking.
    fn roots(&self) -> impl Iterator<Item = DefId> + '_ {
        self.all_defs().filter(|d| d.is_pub).map(|d| d.defid)
    }

    /// Definition→definition edges: an edge `(a, b)` means definition `a`
    /// contains an occurrence that resolves to definition `b`.
    fn edges(&self) -> impl Iterator<Item = (DefId, DefId)> + '_ {
        self.modules.values().flat_map(|mr| {
            mr.occurrences
                .iter()
                .zip(mr.occurrence_owner.iter())
                .filter_map(|(occ, owner)| owner.map(|o| (o, occ.target)))
        })
    }

    /// The set of definitions reachable from `roots()` plus `extra_roots`,
    /// walking [`edges`](Self::edges). A definition only self-referenced
    /// (recursive but otherwise unused) is *not* made reachable by its own
    /// edge, so it is correctly reported as dead.
    fn reachable(&self, extra_roots: Vec<DefId>) -> HashSet<DefId> {
        let mut adj: HashMap<DefId, Vec<DefId>> = HashMap::new();
        for (from, to) in self.edges() {
            adj.entry(from).or_default().push(to);
        }
        let mut seen: HashSet<DefId> = HashSet::new();
        let mut queue: VecDeque<DefId> = VecDeque::new();
        for r in self.roots().chain(extra_roots) {
            if seen.insert(r) {
                queue.push_back(r);
            }
        }
        while let Some(d) = queue.pop_front() {
            if let Some(next) = adj.get(&d) {
                for &n in next {
                    if seen.insert(n) {
                        queue.push_back(n);
                    }
                }
            }
        }
        seen
    }
}

// ============================================================================
// Unused / dead-code Hint diagnostics (the non-LSP reachability consumer)
// ============================================================================

impl ReferenceGraph {
    /// Targets of the entry module's executed top-level body — *use*
    /// occurrences (`Qualified`/`Unqualified`) with no enclosing definition
    /// (`owner == None`). That code runs whenever the program runs, so anything
    /// it names is a live root in addition to the `pub` API surface (see
    /// [`roots`](Self::roots)).
    ///
    /// The kind filter is load-bearing: the populator records, with
    /// `owner == None`, a `ReferenceKind::Definition` self-occurrence for
    /// *every* top-level fn/const/type and an `Import`/`Alias` occurrence for
    /// every import. Rooting on `owner.is_none()` alone would make every
    /// top-level definition its own reachability root, so no private definition
    /// could ever be reported as dead. Only a real qualified/unqualified use in
    /// the executed body is a root.
    fn entry_toplevel_roots(&self, entry: ModuleId) -> Vec<DefId> {
        match self.modules.get(&entry) {
            Some(mr) => mr
                .occurrences
                .iter()
                .zip(mr.occurrence_owner.iter())
                .filter_map(|(occ, owner)| {
                    (owner.is_none()
                        && matches!(
                            occ.kind,
                            ReferenceKind::Unqualified | ReferenceKind::Qualified
                        ))
                    .then_some(occ.target)
                })
                .collect(),
            None => Vec::new(),
        }
    }

    /// The reachable set under the full dead-code root rule: every `pub`
    /// definition (workspace-wide) plus everything the entry module's
    /// top-level body references, closed over `definition -> reference` edges.
    /// A self-only-recursive definition is *not* made reachable by its own
    /// edge, so it is still reported as dead.
    pub fn reachable_from_entry(&self, entry: ModuleId) -> HashSet<DefId> {
        self.reachable(self.entry_toplevel_roots(entry))
    }

    /// Whether `def` is used as a real (qualified/unqualified) reference
    /// anywhere. An import declaration's own `Import`/`Alias`/`Definition`
    /// occurrences never count as a use.
    ///
    /// For a `ModuleAlias` this also keeps the import alive when an
    /// *unqualified selective* item it introduced is used (`import a/b.{used}`
    /// then `used()`) or when a *plain qualified* member is used (`import a/b`
    /// then `b.foo()`). The compiler records a selective item-binding as an
    /// `Unqualified` occurrence and a `b.foo()` call as a `Qualified`
    /// occurrence whose `target` is the *remote* imported `DefId`, not this
    /// alias — so neither use ever points back at the alias and the direct
    /// check alone spuriously reports the import as unused. This is al's
    /// equivalent of Gleam's `module_name_to_node` +
    /// `register_module_reference`, which add a `use -> import` edge so an
    /// imported name's use keeps its import reachable. al has no such explicit
    /// edge, so the link is recovered structurally:
    ///
    /// * selective `import a/b.{item}`: every item-binding occurrence sits
    ///   inside the `import` declaration's span, so any imported target
    ///   referenced from *outside* that declaration is a genuine use; and
    /// * plain `import a/b` + `b.foo()`: the use is a `Qualified` occurrence
    ///   resolving into the *imported* module, so a `Qualified` occurrence —
    ///   outside the declaration — resolving into the module *this* import
    ///   brings in keeps the qualified import alive. The imported module is
    ///   recovered from the declaration's own `Import` occurrence (whose target
    ///   is owned by that module); matching on it rather than on "any other
    ///   module" stops one used qualified import from masking a second,
    ///   genuinely-unused one.
    fn has_real_use(&self, def: DefId) -> bool {
        let direct = self.references_to(def).iter().any(|r| {
            matches!(
                r.kind,
                ReferenceKind::Qualified | ReferenceKind::Unqualified
            )
        });
        if direct || def.entity != EntityKind::ModuleAlias {
            return direct;
        }
        let Some(mr) = self.modules.get(&def.module) else {
            return false;
        };
        // The `import ...` declaration's lexical extent. `process_import`
        // records the alias's `ModuleAlias` *definition* span as the full
        // declaration only for a *non-aliased* import; for `import a/b as
        // c.{item}` it is just the `as c` identifier, because the parser emits
        // `as c` *before* `.{item}`. Deriving the boundary from that narrow
        // span alone would leave every item-binding occurrence — recorded at
        // `item.name.span`, after `as c` — *outside* it, so a used selective
        // item could no longer keep its import alive. Recover the real extent
        // by unioning the alias-name span with this declaration's own
        // occurrences (the `Import` path segment, the `Alias` name, and the
        // item bindings). Imports precede all other code and each sits on its
        // own line, so an occurrence on the alias's line belongs to this
        // import; a genuine *use* lives in a later statement, on a later line,
        // and is correctly left outside the recovered span.
        let mut imp_span = def.span;
        let decl_line = imp_span.start_line;
        for o in mr.occurrences() {
            if o.span.start_line == decl_line {
                imp_span = span_union(&imp_span, &o.span);
            }
        }
        // Targets introduced by this import: occurrences nested inside the
        // declaration that resolve somewhere other than the alias itself.
        let imported: HashSet<DefId> = mr
            .occurrences()
            .iter()
            .filter(|o| o.target != def && span_within(&o.span, &imp_span))
            .map(|o| o.target)
            .collect();
        // Selective `import a/b.{item}`: used iff one of those imported targets
        // has a qualified/unqualified occurrence *outside* the declaration — a
        // real use in code, not the binding occurrence itself. al imports bind
        // only within the importing module, so the use is intra-module.
        let selective_item_used = !imported.is_empty()
            && mr.occurrences().iter().any(|o| {
                imported.contains(&o.target)
                    && matches!(
                        o.kind,
                        ReferenceKind::Qualified | ReferenceKind::Unqualified
                    )
                    && !span_within(&o.span, &imp_span)
            });
        if selective_item_used {
            return true;
        }
        // Plain `import a/b` + `b.foo()`: the qualified use is a `Qualified`
        // occurrence whose target is the *remote* member def (owned by the
        // imported module), recorded outside the declaration — it never points
        // back at the alias. The alias->imported-module link is the
        // declaration's own `Import` occurrence: recorded at the module-name
        // path segment (so inside `imp_span`) with a `target` owned by the
        // *imported* module. A qualified use keeps *this* import alive only when
        // it resolves into that same module; checking merely "some other module"
        // would let one used qualified import mask a second, genuinely-unused
        // one (two plain imports, only one used). (Only `Qualified` — selective
        // items are `Unqualified` and handled above.)
        let imported_modules: HashSet<ModuleId> = mr
            .occurrences()
            .iter()
            .filter(|o| o.kind == ReferenceKind::Import && span_within(&o.span, &imp_span))
            .map(|o| o.target.module)
            .collect();
        mr.occurrences().iter().any(|o| {
            o.kind == ReferenceKind::Qualified
                && o.target.module != def.module
                && imported_modules.contains(&o.target.module)
                && !span_within(&o.span, &imp_span)
        })
    }

    /// `Hint` diagnostics for the entry module: unused private definitions and
    /// unused imports, decided by workspace reachability
    /// ([`reachable_from_entry`](Self::reachable_from_entry)).
    ///
    /// Only definitions *owned by* `entry` are reported, so prelude / `@vm` /
    /// standard-library definitions (which live in other modules, and are
    /// `pub` when they are intrinsics) are never flagged; `pub` items are never
    /// flagged; a self-only-recursive private definition *is* flagged (it is
    /// not reachable from any root). Output is ordered by source position so it
    /// flows stably through `publishDiagnostics` when appended to
    /// `CompileResult.diagnostics`.
    pub fn unused_diagnostics(&self, entry: ModuleId) -> Vec<Diagnostic> {
        let Some(mr) = self.modules.get(&entry) else {
            return Vec::new();
        };
        let reachable = self.reachable_from_entry(entry);
        let mut out: Vec<Diagnostic> = Vec::new();

        for def in mr.definitions.values() {
            if def.entity() == EntityKind::ModuleAlias {
                // Unused import: an import/alias binding that nothing uses
                // qualified or unqualified.
                if !self.has_real_use(def.defid) {
                    out.push(Diagnostic::hint(
                        def.span(),
                        format!("unused import `{}`", def.name),
                    ));
                }
                continue;
            }
            // Unused private definition: non-pub, unreachable, and a reportable
            // declaration kind. `Value` binders are covered by the unused-binding
            // check, `Constructor`/`Field` via their owning `Type`, `ModuleAlias`
            // as an unused import (above).
            if def.is_pub
                || !matches!(
                    def.entity(),
                    EntityKind::Function | EntityKind::Constant | EntityKind::Type
                )
                || reachable.contains(&def.defid)
            {
                continue;
            }
            out.push(Diagnostic::hint(
                def.span(),
                format!("unused {} `{}`", def.entity().noun(), def.name),
            ));
        }

        out.sort_by_key(|d| (d.span.start_line, d.span.start_column));
        out
    }

    /// [`unused_diagnostics`](Self::unused_diagnostics) keyed by module path;
    /// empty if `entry` was never interned into this graph.
    pub fn unused_diagnostics_for(&self, entry: &ModulePath) -> Vec<Diagnostic> {
        match self.module_id(entry) {
            Some(id) => self.unused_diagnostics(id),
            None => Vec::new(),
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostic::Severity;
    use crate::span::{point_span, range_span};

    fn mp(parts: &[&str]) -> ModulePath {
        parts.iter().map(|s| s.to_string()).collect()
    }

    fn def(module: ModuleId, line: i32, c0: i32, c1: i32, kind: EntityKind) -> DefId {
        DefId::new(module, range_span(line, c0, c1), kind)
    }

    fn main_graph() -> (ReferenceGraph, ModuleId, ModuleReferences) {
        let mut g = ReferenceGraph::new();
        let m = g.intern_module(&mp(&["main"]));
        let mr = ModuleReferences::new(m);
        (g, m, mr)
    }

    // ---- ModuleInterner ----

    #[test]
    fn interner_is_stable_and_bijective() {
        let mut it = ModuleInterner::new();
        let a = it.intern(&mp(&["main"]));
        let b = it.intern(&mp(&["al", "list"]));
        let a2 = it.intern(&mp(&["main"]));

        assert_eq!(a, a2, "same path re-interns to the same id");
        assert_ne!(a, b);
        assert_eq!(it.len(), 2, "distinct paths get distinct ids");
        assert_eq!(it.path(a), Some(&mp(&["main"])));
        assert_eq!(it.path(b), Some(&mp(&["al", "list"])));
        assert_eq!(it.lookup(&mp(&["al", "list"])), Some(b));
        assert_eq!(it.lookup_key("al/list"), Some(b));
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
        let s = range_span(5, 2, 6); // columns [2, 6) on line 5
        assert!(!span_contains(&s, 5, 1));
        assert!(span_contains(&s, 5, 2));
        assert!(span_contains(&s, 5, 5));
        assert!(!span_contains(&s, 5, 6), "end column is exclusive");
        assert!(!span_contains(&s, 4, 3));

        let p = point_span(7, 9);
        assert!(span_contains(&p, 7, 9));
        assert!(!span_contains(&p, 7, 10));
    }

    #[test]
    fn span_width_orders_tightest_first() {
        let narrow = range_span(1, 0, 3);
        let wide = range_span(1, 0, 30);
        let multiline = Span {
            start_line: 1,
            start_column: 0,
            end_line: 3,
            end_column: 0,
        };
        assert!(span_width(&narrow) < span_width(&wide));
        assert!(span_width(&wide) < span_width(&multiline));
    }

    // ---- ModuleReferences: definitions, reverse index, position lookup ----

    #[test]
    fn module_refs_intra_module_reverse_index() {
        let m = ModuleId(0);
        let mut mr = ModuleReferences::new(m);

        let foo = def(m, 1, 3, 6, EntityKind::Function);
        mr.add_definition(Definition::new(foo, "foo", Some("the foo".into()), true));

        // two uses of foo inside some other (owner) definition
        let owner = def(m, 10, 3, 7, EntityKind::Function);
        mr.add_reference(
            Some(owner),
            Reference::new(range_span(11, 4, 7), ReferenceKind::Unqualified, foo),
        );
        mr.add_reference(
            Some(owner),
            Reference::new(range_span(12, 4, 7), ReferenceKind::Unqualified, foo),
        );

        assert_eq!(mr.defs_named("foo"), &[foo]);
        assert_eq!(mr.definition(foo).map(|d| d.name.as_str()), Some("foo"));
        assert_eq!(mr.occurrences().len(), 2);
    }

    #[test]
    fn resolve_position_prefers_tightest_then_falls_back_to_def_name() {
        let m = ModuleId(0);
        let mut mr = ModuleReferences::new(m);

        let target = def(m, 1, 0, 3, EntityKind::Function);
        mr.add_definition(Definition::new(target, "fn", None, false));

        // A wide occurrence and a tighter one overlapping the same point.
        let wide = def(m, 9, 0, 1, EntityKind::Value);
        mr.add_reference(
            None,
            Reference::new(range_span(20, 0, 40), ReferenceKind::Unqualified, wide),
        );
        mr.add_reference(
            None,
            Reference::new(range_span(20, 10, 14), ReferenceKind::Unqualified, target),
        );

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
        let mut g = ReferenceGraph::new();
        let lib = g.intern_module(&mp(&["lib"]));
        let app = g.intern_module(&mp(&["app"]));

        // lib defines `helper` (pub) at line 1.
        let helper = def(lib, 1, 3, 9, EntityKind::Function);
        let mut lib_mr = ModuleReferences::new(lib);
        lib_mr.add_definition(Definition::new(helper, "helper", Some("doc".into()), true));
        g.insert_module(lib_mr);

        // app uses `helper` twice (qualified), inside app's `run`.
        let run = def(app, 1, 3, 6, EntityKind::Function);
        let mut app_mr = ModuleReferences::new(app);
        app_mr.add_definition(Definition::new(run, "run", None, true));
        app_mr.add_reference(
            Some(run),
            Reference::new(range_span(2, 4, 14), ReferenceKind::Qualified, helper),
        );
        app_mr.add_reference(
            Some(run),
            Reference::new(range_span(3, 4, 14), ReferenceKind::Qualified, helper),
        );
        g.insert_module(app_mr);

        (g, lib, app, helper)
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
        assert!(refs.iter().all(|r| r.kind == ReferenceKind::Qualified));
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
        // rebuilds a fresh graph from the live set via `insert_module_deferred`
        // + one `rebuild`, so a module is "evicted" purely by omission. Build
        // from only `lib`, omitting the `app` that referenced `helper`; since
        // `rebuild` is a pure function of the live module set, no reverse edge
        // into `helper` can survive.
        let mut g = ReferenceGraph::new();
        let lib = g.intern_module(&mp(&["lib"]));
        let helper = def(lib, 1, 3, 9, EntityKind::Function);
        let mut lib_mr = ModuleReferences::new(lib);
        lib_mr.add_definition(Definition::new(helper, "helper", Some("doc".into()), true));
        g.insert_module_deferred(Rc::new(lib_mr));
        g.rebuild();

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

    // ---- Reachability / dead-code ----

    #[test]
    fn edges_only_emitted_for_owned_occurrences() {
        let (mut g, m, mut mr) = main_graph();

        let a = def(m, 1, 0, 1, EntityKind::Function);
        let b = def(m, 2, 0, 1, EntityKind::Function);
        mr.add_definition(Definition::new(a, "a", None, true));
        mr.add_definition(Definition::new(b, "b", None, false));

        // Owned by `a` -> contributes an a->b edge.
        mr.add_reference(
            Some(a),
            Reference::new(range_span(1, 5, 6), ReferenceKind::Unqualified, b),
        );
        // Unowned top-level occurrence -> no edge.
        mr.add_reference(
            None,
            Reference::new(range_span(9, 0, 1), ReferenceKind::Unqualified, b),
        );
        g.insert_module(mr);

        let edges: Vec<(DefId, DefId)> = g.edges().collect();
        assert_eq!(edges, vec![(a, b)]);

        // `b` is reachable: `a` is a pub root and references it.
        assert!(g.reachable(Vec::new()).contains(&b));
    }

    #[test]
    fn reachable_closes_transitively_and_skips_self_only_recursion() {
        let (mut g, m, mut mr) = main_graph();

        // pub `a` -> private `b` -> private `deep`: a multi-hop chain whose
        // middle node `b` is not itself a root.
        let a = add_def(&mut mr, m, "a", 1, EntityKind::Function, true);
        let b = add_def(&mut mr, m, "b", 3, EntityKind::Function, false);
        let deep = add_def(&mut mr, m, "deep", 5, EntityKind::Function, false);
        // private `loopy`, only self-recursive.
        let loopy = add_def(&mut mr, m, "loopy", 7, EntityKind::Function, false);

        mr.add_reference(
            Some(a),
            Reference::new(range_span(2, 4, 5), ReferenceKind::Unqualified, b),
        );
        mr.add_reference(
            Some(b),
            Reference::new(range_span(4, 4, 8), ReferenceKind::Unqualified, deep),
        );
        mr.add_reference(
            Some(loopy),
            Reference::new(range_span(8, 4, 9), ReferenceKind::Unqualified, loopy),
        );
        g.insert_module(mr);

        let r = g.reachable(Vec::new());
        assert!(
            r.contains(&deep),
            "transitively reachable through a -> b -> deep"
        );
        assert!(
            !r.contains(&loopy),
            "a self-only-recursive private def is not rooted by its own edge"
        );
    }

    // ---- Unused / dead-code Hint diagnostics ----

    fn add_def(
        mr: &mut ModuleReferences,
        m: ModuleId,
        name: &str,
        line: i32,
        kind: EntityKind,
        is_pub: bool,
    ) -> DefId {
        let d = def(m, line, 3, 3 + name.len() as i32, kind);
        mr.add_definition(Definition::new(d, name, None, is_pub));
        d
    }

    #[track_caller]
    fn sole_unused(g: &ReferenceGraph, m: ModuleId) -> Diagnostic {
        let mut d = g.unused_diagnostics(m);
        assert_eq!(d.len(), 1, "expected exactly one unused diagnostic: {d:?}");
        d.pop().unwrap()
    }

    #[test]
    fn unused_diag_flags_private_fn_keeps_pub_and_used() {
        let (mut g, m, mut mr) = main_graph();
        let api = add_def(&mut mr, m, "api", 1, EntityKind::Function, true);
        let used = add_def(&mut mr, m, "used", 3, EntityKind::Function, false);
        let dead = add_def(&mut mr, m, "dead", 5, EntityKind::Function, false);
        // pub `api` calls private `used`; nobody calls `dead`.
        mr.add_reference(
            Some(api),
            Reference::new(range_span(2, 4, 8), ReferenceKind::Unqualified, used),
        );
        g.insert_module(mr);

        let d = sole_unused(&g, m);
        assert_eq!(d.severity, Severity::Hint);
        assert_eq!(d.message, "unused function `dead`");
        assert_eq!(d.span, dead.span);
    }

    #[test]
    fn unused_diag_entry_toplevel_body_keeps_def_live() {
        let (mut g, m, mut mr) = main_graph();
        let helper = add_def(&mut mr, m, "helper", 1, EntityKind::Function, false);
        let dead = add_def(&mut mr, m, "dead", 4, EntityKind::Function, false);
        // `helper` is only called by the executed top-level body (owner None).
        mr.add_reference(
            None,
            Reference::new(range_span(9, 0, 6), ReferenceKind::Unqualified, helper),
        );
        g.insert_module(mr);

        let d = sole_unused(&g, m);
        assert_eq!(d.message, "unused function `dead`");
        assert_eq!(d.span, dead.span);
    }

    #[test]
    fn unused_diag_self_recursive_only_is_flagged() {
        let (mut g, m, mut mr) = main_graph();
        let spin = add_def(&mut mr, m, "spin", 1, EntityKind::Function, false);
        // `spin` references only itself — a self edge never makes it reachable.
        mr.add_reference(
            Some(spin),
            Reference::new(range_span(2, 4, 8), ReferenceKind::Unqualified, spin),
        );
        g.insert_module(mr);

        assert_eq!(sole_unused(&g, m).message, "unused function `spin`");
    }

    #[test]
    fn unused_diag_only_reports_entry_module() {
        let (mut g, entry, mut em) = main_graph();
        let lib = g.intern_module(&mp(&["lib"]));

        add_def(&mut em, entry, "go", 1, EntityKind::Function, true);
        g.insert_module(em);

        // A private, unreachable def in another module must NOT surface when
        // diagnosing the entry module (prelude/@vm/library defs live here).
        let mut lm = ModuleReferences::new(lib);
        add_def(&mut lm, lib, "secret", 1, EntityKind::Function, false);
        g.insert_module(lm);

        assert!(g.unused_diagnostics(entry).is_empty());
    }

    #[test]
    fn unused_diag_unused_and_used_module_alias_import() {
        let (mut g, m, mut mr) = main_graph();
        let unused_imp = add_def(&mut mr, m, "io", 1, EntityKind::ModuleAlias, false);
        let used_imp = add_def(&mut mr, m, "fmt", 2, EntityKind::ModuleAlias, false);
        // `fmt` is used through a qualified access; `io` never is.
        mr.add_reference(
            None,
            Reference::new(range_span(5, 4, 12), ReferenceKind::Qualified, used_imp),
        );
        g.insert_module(mr);

        let d = sole_unused(&g, m);
        assert_eq!(d.message, "unused import `io`");
        assert_eq!(d.span, unused_imp.span);
    }

    #[test]
    fn unused_diag_import_with_only_decl_occurrences_is_flagged() {
        let (mut g, m, mut mr) = main_graph();
        let imp = add_def(&mut mr, m, "util", 1, EntityKind::ModuleAlias, false);
        // The import declaration's own occurrences are not "uses".
        mr.add_reference(
            None,
            Reference::new(range_span(1, 7, 11), ReferenceKind::Import, imp),
        );
        mr.add_reference(
            None,
            Reference::new(range_span(1, 7, 11), ReferenceKind::Definition, imp),
        );
        g.insert_module(mr);

        assert_eq!(sole_unused(&g, m).message, "unused import `util`");
    }

    #[test]
    fn unused_diag_unqualified_import_item_used_keeps_import_live() {
        // `import a/b.{used}` then `pub fn main() { used() }`. The item
        // binding records an `Unqualified` occurrence whose target is the
        // *remote* `used` def in `a/b` (not the alias), and so does the
        // `used()` call — the import must still be recognised as used.
        //
        // The boundary that tells the item *binding* (inside the declaration)
        // from the real *use* (outside it) is the alias's `ModuleAlias`
        // *definition* span, which production records as the full declaration
        // span. The `Import` *occurrence* covers only the final module-name
        // segment (`b`), too narrow to contain the binding — modelled here to
        // prove that narrowing does not regress unused detection.
        let (mut g, m, _) = main_graph();
        let lib = g.intern_module(&mp(&["a", "b"]));

        let build = |with_use: bool| {
            let mut mr = ModuleReferences::new(m);
            let alias = def(m, 1, 0, 18, EntityKind::ModuleAlias);
            mr.add_definition(Definition::new(alias, "b", None, false));
            let remote_used = def(lib, 1, 3, 7, EntityKind::Function);
            // The `Import` occurrence covers only the `b` path segment; the
            // `used` item-binding occurrence sits inside the declaration span.
            mr.add_reference(
                None,
                Reference::new(range_span(1, 9, 10), ReferenceKind::Import, alias),
            );
            mr.add_reference(
                None,
                Reference::new(
                    range_span(1, 12, 16),
                    ReferenceKind::Unqualified,
                    remote_used,
                ),
            );
            let main = add_def(&mut mr, m, "main", 3, EntityKind::Function, true);
            if with_use {
                // `pub fn main() { used() }` — a real use, outside the
                // declaration.
                mr.add_reference(
                    Some(main),
                    Reference::new(
                        range_span(3, 16, 20),
                        ReferenceKind::Unqualified,
                        remote_used,
                    ),
                );
            }
            mr
        };

        g.insert_module(build(true));
        assert!(
            g.unused_diagnostics(m).is_empty(),
            "an import whose unqualified item is used must not be flagged"
        );

        // Drop the use: the same import is now genuinely unused again, so
        // the check is still live (we didn't just disable it).
        g.insert_module(build(false));
        assert_eq!(sole_unused(&g, m).message, "unused import `b`");
    }

    #[test]
    fn unused_diag_aliased_selective_import_item_used_keeps_import_live() {
        // `import ./util as u.{empty}` then `println(empty())`. The parser
        // emits `as u` *before* `.{empty}`, so production records the alias's
        // `ModuleAlias` definition span as just the `u` identifier — the item
        // binding at `empty` (and any later use of it) sit *after* `u`. The
        // declaration extent must still be recovered so the binding counts as
        // inside the import and the `empty()` call counts as a real use; a
        // boundary taken from the `u` span alone would wrongly flag a used,
        // single-statement aliased+selective import as unused.
        //
        //   import ./util as u.{empty}
        //   0      ^9   ^13 ^17^20   ^25
        let (mut g, m, _) = main_graph();
        let lib = g.intern_module(&mp(&["util"]));

        // The alias `ModuleAlias` definition covers only the `as u` name.
        let alias = def(m, 1, 17, 18, EntityKind::ModuleAlias);

        let build = |with_use: bool| {
            let mut mr = ModuleReferences::new(m);
            mr.add_definition(Definition::new(alias, "u", None, false));
            let remote_empty = def(lib, 2, 7, 12, EntityKind::Function);
            // `Import` path segment (`util`) -> the imported module; `Alias`
            // name (`u`) -> this alias; `empty` item binding -> the remote
            // `empty`.
            mr.add_reference(
                None,
                Reference::new(
                    range_span(1, 9, 13),
                    ReferenceKind::Import,
                    def(lib, 1, 9, 13, EntityKind::ModuleAlias),
                ),
            );
            mr.add_reference(
                None,
                Reference::new(range_span(1, 17, 18), ReferenceKind::Alias, alias),
            );
            mr.add_reference(
                None,
                Reference::new(
                    range_span(1, 20, 25),
                    ReferenceKind::Unqualified,
                    remote_empty,
                ),
            );
            if with_use {
                // `println(empty())` on line 2 — a real use, outside the
                // declaration.
                mr.add_reference(
                    None,
                    Reference::new(
                        range_span(2, 8, 13),
                        ReferenceKind::Unqualified,
                        remote_empty,
                    ),
                );
            }
            mr
        };

        g.insert_module(build(true));
        assert!(
            g.unused_diagnostics(m).is_empty(),
            "a used item from an aliased selective import must not be flagged"
        );

        // Drop the use: the same import is now genuinely unused, so the check
        // is still live (the widened boundary did not just disable it).
        g.insert_module(build(false));
        let d = sole_unused(&g, m);
        assert_eq!(d.message, "unused import `u`");
        assert_eq!(d.span, alias.span);
    }

    #[test]
    fn unused_diag_constant_and_type_wording_and_ordering() {
        let (mut g, m, mut mr) = main_graph();
        // Declared out of source order to prove the output is position-sorted.
        add_def(&mut mr, m, "Color", 9, EntityKind::Type, false);
        add_def(&mut mr, m, "MAX", 2, EntityKind::Constant, false);
        g.insert_module(mr);

        let d = g.unused_diagnostics(m);
        let msgs: Vec<&str> = d.iter().map(|x| x.message.as_str()).collect();
        assert_eq!(msgs, vec!["unused constant `MAX`", "unused type `Color`"]);
    }

    #[test]
    fn unused_diag_value_binders_are_not_reported() {
        let (mut g, m, mut mr) = main_graph();
        // `Value` is the unused-binding checker's job, never a dead-code hint.
        add_def(&mut mr, m, "x", 1, EntityKind::Value, false);
        g.insert_module(mr);
        assert!(g.unused_diagnostics(m).is_empty());
    }

    #[test]
    fn unused_diag_cross_module_pub_keeps_private_live() {
        let (mut g, entry, mut em) = main_graph();
        let lib = g.intern_module(&mp(&["lib"]));

        // lib: pub `run` -> private `helper`.
        let mut lm = ModuleReferences::new(lib);
        let run = add_def(&mut lm, lib, "run", 1, EntityKind::Function, true);
        let helper = add_def(&mut lm, lib, "helper", 3, EntityKind::Function, false);
        lm.add_reference(
            Some(run),
            Reference::new(range_span(2, 4, 10), ReferenceKind::Unqualified, helper),
        );
        g.insert_module(lm);

        // entry top-level body calls `lib.run` qualified.
        em.add_reference(
            None,
            Reference::new(range_span(5, 0, 7), ReferenceKind::Qualified, run),
        );
        g.insert_module(em);

        assert!(g.unused_diagnostics(entry).is_empty());
        // `helper` is reachable via the pub `run` root, not dead.
        assert!(g.reachable_from_entry(entry).contains(&helper));
    }

    #[test]
    fn unused_diag_for_path_and_unknown_entry() {
        let (mut g, m, mut mr) = main_graph();
        add_def(&mut mr, m, "dead", 1, EntityKind::Function, false);
        g.insert_module(mr);

        assert_eq!(g.unused_diagnostics_for(&mp(&["main"])).len(), 1);
        // Never-interned path: empty, no panic.
        assert!(g.unused_diagnostics_for(&mp(&["ghost"])).is_empty());
        // Interned but never populated: empty.
        let bare = g.intern_module(&mp(&["bare"]));
        assert!(g.unused_diagnostics(bare).is_empty());
    }

    /// Mirrors production `Compiler::emit_def`, which records a
    /// `ReferenceKind::Definition` self-occurrence (`owner == None`,
    /// `target == the def`) for every top-level fn/const/type. Such
    /// self-occurrences must NOT be treated as entry-body reachability roots
    /// (the `entry_toplevel_roots` kind filter) — otherwise every top-level
    /// definition roots itself and no dead private code is ever reported.
    #[test]
    fn unused_diag_definition_self_occurrence_does_not_root_dead_code() {
        let (mut g, m, mut mr) = main_graph();

        let api = add_def(&mut mr, m, "api", 1, EntityKind::Function, true);
        let used = add_def(&mut mr, m, "used", 3, EntityKind::Function, false);
        let dead = add_def(&mut mr, m, "dead", 5, EntityKind::Function, false);

        // emit_def's self-occurrence for every top-level def (owner None).
        for d in [api, used, dead] {
            mr.add_reference(None, Reference::new(d.span, ReferenceKind::Definition, d));
        }
        // pub `api` calls private `used`; nobody calls `dead`.
        mr.add_reference(
            Some(api),
            Reference::new(range_span(2, 4, 8), ReferenceKind::Unqualified, used),
        );
        g.insert_module(mr);

        let d = sole_unused(&g, m);
        assert_eq!(d.message, "unused function `dead`");
        assert_eq!(d.span, dead.span);
    }

    /// Production shape of `import a/b` + `b.foo()`: the populator records the
    /// module-name segment as an `Import` occurrence whose target is owned by
    /// the *imported* module `a/b` (`DefId::new(imported_mid, path_span,
    /// ModuleAlias)`) — the alias->imported-module edge — and the call as a
    /// `Qualified` occurrence whose target is the *remote* `foo` def, also owned
    /// by `a/b`. Neither ever points back at the alias. The import must not be
    /// reported as unused, and dropping the use must flag it again.
    #[test]
    fn unused_diag_plain_qualified_import_recovers_use() {
        let (mut g, m, mut mr) = main_graph();
        let lib = g.intern_module(&mp(&["a", "b"]));

        // `a/b` defines pub `foo` (+ its emit_def self-occurrence).
        let foo = def(lib, 1, 3, 6, EntityKind::Function);
        let mut lib_mr = ModuleReferences::new(lib);
        lib_mr.add_definition(Definition::new(foo, "foo", None, true));
        lib_mr.add_reference(
            None,
            Reference::new(foo.span, ReferenceKind::Definition, foo),
        );
        g.insert_module(lib_mr);

        // The `Import` occurrence's target is owned by the imported module
        // `a/b`, not the importing module — this is the link `has_real_use`
        // recovers to tell which module the alias brings in.
        let import_seg = def(lib, 1, 7, 8, EntityKind::ModuleAlias);

        // main: `import a/b` then `pub fn run() { b.foo() }`.
        let alias = add_def(&mut mr, m, "b", 1, EntityKind::ModuleAlias, false);
        mr.add_reference(
            None,
            Reference::new(alias.span, ReferenceKind::Definition, alias),
        );
        mr.add_reference(
            None,
            Reference::new(range_span(1, 7, 8), ReferenceKind::Import, import_seg),
        );
        let run = add_def(&mut mr, m, "run", 3, EntityKind::Function, true);
        mr.add_reference(
            None,
            Reference::new(run.span, ReferenceKind::Definition, run),
        );
        // `b.foo()` — a `Qualified` occurrence targeting the *remote* `foo`.
        mr.add_reference(
            Some(run),
            Reference::new(range_span(3, 18, 21), ReferenceKind::Qualified, foo),
        );
        g.insert_module(mr);

        assert!(
            g.unused_diagnostics(m).is_empty(),
            "a plain qualified import whose member is used must not be flagged"
        );

        // Drop the use: the import is genuinely unused again and IS flagged
        // (the check is still live, not just disabled).
        let mut mr2 = ModuleReferences::new(m);
        let alias2 = add_def(&mut mr2, m, "b", 1, EntityKind::ModuleAlias, false);
        mr2.add_reference(
            None,
            Reference::new(alias2.span, ReferenceKind::Definition, alias2),
        );
        mr2.add_reference(
            None,
            Reference::new(range_span(1, 7, 8), ReferenceKind::Import, import_seg),
        );
        let run2 = add_def(&mut mr2, m, "run", 3, EntityKind::Function, true);
        mr2.add_reference(
            None,
            Reference::new(run2.span, ReferenceKind::Definition, run2),
        );
        g.insert_module(mr2);

        assert_eq!(sole_unused(&g, m).message, "unused import `b`");
    }

    /// Two plain qualified imports where only one is used: the unused one must
    /// still be flagged. The used import's cross-module `Qualified` occurrence
    /// must not mask the genuinely-unused sibling — the regression guarded here
    /// (`has_real_use` formerly kept *any* import alive on *any* cross-module
    /// qualified use, so `c.use()` wrongly suppressed `import a/b`'s hint).
    #[test]
    fn unused_diag_one_of_two_plain_qualified_imports_still_flagged() {
        let (mut g, m, mut mr) = main_graph();
        let lib_b = g.intern_module(&mp(&["a", "b"]));
        let lib_c = g.intern_module(&mp(&["a", "c"]));

        // `a/c` defines pub `used`; `a/b` defines pub `foo` (never used here).
        let used = def(lib_c, 1, 3, 7, EntityKind::Function);
        let mut c_mr = ModuleReferences::new(lib_c);
        c_mr.add_definition(Definition::new(used, "used", None, true));
        g.insert_module(c_mr);
        let foo = def(lib_b, 1, 3, 6, EntityKind::Function);
        let mut b_mr = ModuleReferences::new(lib_b);
        b_mr.add_definition(Definition::new(foo, "foo", None, true));
        g.insert_module(b_mr);

        // main:
        //   import a/b   (line 1, unused)
        //   import a/c   (line 2, used via c.used())
        //   pub fn run() { c.used() }
        let alias_b = def(m, 1, 7, 8, EntityKind::ModuleAlias);
        mr.add_definition(Definition::new(alias_b, "b", None, false));
        mr.add_reference(
            None,
            Reference::new(
                range_span(1, 7, 8),
                ReferenceKind::Import,
                def(lib_b, 1, 7, 8, EntityKind::ModuleAlias),
            ),
        );
        let alias_c = def(m, 2, 7, 8, EntityKind::ModuleAlias);
        mr.add_definition(Definition::new(alias_c, "c", None, false));
        mr.add_reference(
            None,
            Reference::new(
                range_span(2, 7, 8),
                ReferenceKind::Import,
                def(lib_c, 2, 7, 8, EntityKind::ModuleAlias),
            ),
        );
        let run = add_def(&mut mr, m, "run", 3, EntityKind::Function, true);
        // `c.used()` — a cross-module `Qualified` occurrence into `a/c` only.
        mr.add_reference(
            Some(run),
            Reference::new(range_span(3, 18, 24), ReferenceKind::Qualified, used),
        );
        g.insert_module(mr);

        assert_eq!(
            sole_unused(&g, m).message,
            "unused import `b`",
            "the unused `a/b` import must be flagged while the used `a/c` is not"
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
