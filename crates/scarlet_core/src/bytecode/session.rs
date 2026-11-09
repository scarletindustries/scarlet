//! The LSP and workspace layer over [`Compiler`]. It handles incremental
//! recompilation and answers reference-graph queries from it.
//!
//! This whole file exists to keep one rule: an index must never outlive the
//! arena that minted it. [`Compiler::reset_to`] destructures [`Watermark`] in
//! full, so a new arena can't join the snapshot until someone writes its
//! rewind.
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use super::compiler::{CompileResult, Compiler, Importer, ModuleChecker, new_compiler};
use crate::ast;
use crate::module::{self, ModulePath};
use crate::reference::{ModuleId, ModuleReferences, ReferenceGraph, ReferenceGraphBuilder};
use crate::span::Span;
use crate::tivec::Idx;
use crate::type_def::{Type, TypeId};
use crate::types::{EnginePoolWatermark, EnvWatermark, Ty, ValueKind};

/// One buffered name occurrence. It holds the *live* `Ty`, because we only
/// resolve it once every unification has settled.
#[derive(Debug, Clone)]
pub(super) struct RawRef {
    pub(super) span: Span,
    pub(super) name: String,
    pub(super) ty: Ty,
    pub(super) doc: Option<String>,
    /// Interned when `record` runs, so `finalize_references` doesn't have to
    /// intern the path again for each occurrence.
    pub(super) module: ModuleId,
}

/// One name a module offers its importers, in the form a name list such as REPL
/// completion wants it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Export {
    pub name: String,
    /// A function's parameter names, or a constructor's field labels, in
    /// order. Empty for anything else.
    pub params: Vec<String>,
}

/// The resolved type at one occurrence's span. The reference graph only holds
/// identities, so `IncrementalSession::hover` gets the inferred type from here.
#[derive(Debug, Clone)]
pub struct HoverFact {
    module: ModuleId,
    span: Span,
    name: String,
    ty: Type,
    doc: Option<String>,
}

/// A snapshot of every compiler structure that only grows, taken at module
/// boundaries so an `IncrementalSession` can roll back to that point.
///
/// Pick between two watermarks with [`earlier`](Self::earlier) or
/// [`later`](Self::later), never `Ord::min` or `max`, because those drop an
/// `env` payload on a tie. If you add a field, you have to rewind it in
/// [`Compiler::reset_to`].
#[derive(Debug, Clone, Copy, Default)]
pub struct Watermark {
    engine: EnginePoolWatermark,
    env: EnvWatermark,
    functions: usize,
    constants: usize,
    local_count: i32,
}

impl Watermark {
    /// A watermark `n` functions in, for ordering tests.
    #[cfg(test)]
    pub(crate) fn at(functions: usize) -> Self {
        Watermark {
            functions,
            ..Watermark::default()
        }
    }

    /// The key watermarks compare by. Every field is the length of a pool that
    /// only grows or a counter that only goes up, so an earlier watermark
    /// compares `<=` a later one. `env` is left out because it is data for the
    /// rollback, not a position, so its fields can change without changing this
    /// order. That means equal keys can hide different env payloads, which is
    /// why `earlier` and `later` merge them on a tie.
    fn ord_key(&self) -> (EnginePoolWatermark, usize, usize, i32) {
        // Destructure every field, so a new one has to be put in or left out of
        // the order on purpose.
        let Watermark {
            engine,
            env: _,
            functions,
            constants,
            local_count,
        } = *self;
        (engine, functions, constants, local_count)
    }

    /// Whichever of two watermarks was compiled first, whatever order you pass
    /// them in. Use this, not `Ord::min`. On an `ord_key` tie, `min` picks by
    /// argument order and quietly throws away one env payload, which can leave
    /// the env not truncated far enough with stale entries in it. This keeps
    /// the deeper rollback of each field instead.
    pub(crate) fn earlier(self, other: Self) -> Self {
        match self.ord_key().cmp(&other.ord_key()) {
            std::cmp::Ordering::Less => self,
            std::cmp::Ordering::Greater => other,
            std::cmp::Ordering::Equal => Watermark {
                env: env_field_min(self.env, other.env),
                ..self
            },
        }
    }

    /// The mirror of `earlier`, which keeps the shallower env of each field on
    /// a tie. We use it to stop a rewind at the seed. Truncating the env below
    /// the seed's payload would leave dangling stdlib bindings whose pools
    /// survived.
    fn later(self, other: Self) -> Self {
        match self.ord_key().cmp(&other.ord_key()) {
            std::cmp::Ordering::Less => other,
            std::cmp::Ordering::Greater => self,
            std::cmp::Ordering::Equal => Watermark {
                env: env_field_max(self.env, other.env),
                ..self
            },
        }
    }
}

/// The `min` of two env rollback payloads, field by field. That is the deeper
/// rollback in every field, which is the safe direction for
/// `TypeEnv::truncate_to`. It is written out one field at a time so a new
/// `EnvWatermark` field won't compile until someone picks which way it merges.
fn env_field_min(a: EnvWatermark, b: EnvWatermark) -> EnvWatermark {
    EnvWatermark {
        root_scope: a.root_scope.min(b.root_scope),
        type_info: a.type_info.min(b.type_info),
        type_info_by_id: a.type_info_by_id.min(b.type_info_by_id),
        definitions: a.definitions.min(b.definitions),
        docs: a.docs.min(b.docs),
        journal: a.journal.min(b.journal),
        next_type_id: a.next_type_id.min(b.next_type_id),
    }
}

/// The mirror of [`env_field_min`], which is the shallower rollback in every
/// field.
fn env_field_max(a: EnvWatermark, b: EnvWatermark) -> EnvWatermark {
    EnvWatermark {
        root_scope: a.root_scope.max(b.root_scope),
        type_info: a.type_info.max(b.type_info),
        type_info_by_id: a.type_info_by_id.max(b.type_info_by_id),
        definitions: a.definitions.max(b.definitions),
        docs: a.docs.max(b.docs),
        journal: a.journal.max(b.journal),
        next_type_id: a.next_type_id.max(b.next_type_id),
    }
}

impl PartialEq for Watermark {
    fn eq(&self, other: &Self) -> bool {
        self.ord_key() == other.ord_key()
    }
}
impl Eq for Watermark {}
impl Ord for Watermark {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.ord_key().cmp(&other.ord_key())
    }
}
impl PartialOrd for Watermark {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Compiler {
    pub(crate) fn watermark(&self) -> Watermark {
        Watermark {
            engine: self.engine.pool_watermark(),
            env: self.env.watermark(),
            functions: self.fns.len(),
            constants: self.consts.len(),
            local_count: self.slots,
        }
    }

    /// Roll every pool and map back to `w`, and clear the state that only lasts
    /// for one compile. This leaves `module_table` alone, because the caller
    /// decides which cached modules survive with `ModuleTable::invalidate`.
    ///
    /// Every structure that survives the rewind must be truncated to `w`,
    /// filtered to indices that are still in bounds, or cleared, so no index
    /// outlives its arena. The full destructure below means a new snapshot
    /// field won't compile until someone writes its rewind.
    ///
    /// `w` must not be below the session's `seed`, because everything below it
    /// is the stdlib itself. Tearing down to `bare` when the prelude is the
    /// entry is the one deliberate exception, and it rebuilds the session
    /// afterwards. `IncrementalSession::rewind_to` enforces the floor.
    fn reset_to(&mut self, w: &Watermark) {
        let Watermark {
            engine,
            env,
            functions,
            constants,
            local_count,
        } = *w;

        self.engine.truncate_to(&engine);
        self.env.truncate_to(&env);
        // Push a throwaway scope for this compile's imports. The root scope
        // rewinds by length, which can't undo a `define` that overwrote a name
        // in place, so an import that shadows a prelude name must never touch
        // the root scope.
        self.env.push_scope();
        self.fns.truncate(functions);
        self.consts.truncate(constants);
        self.global_to_func.retain(|_, fi| fi.index() < functions);
        self.slots = local_count;

        // Drop the lowered toplevels, never truncate them. They are recorded
        // at the end of a module's compile, and the init code of a compile we
        // rewound must not run as part of the next one.
        self.inits.clear();
        self.entry_toplevel = None;
        self.main = None;

        self.recorded.clear();
        // We don't clear `ref_interner`, on purpose, so the `DefId`s in the
        // `CachedModule.module_refs` that survive keep resolving to the same
        // `ModuleId`s.
        let main_id = self.ref_interner.intern(&module::main_module());
        self.entry_refs = ModuleReferences::new(main_id);
        // `str_slices` was just rewound, so an `ArenaSlice` may now name a
        // different path than it did in the last compile.
        self.defid_module_memo.clear();
        self.module_table.unmark_all_loading();
    }

    /// Build a `CompileResult` by cloning, not taking, so the session can be
    /// used again. This rebuilds the workspace reference graph and the hover
    /// table.
    fn snapshot_result(&mut self) -> (CompileResult, Vec<HoverFact>) {
        let (references, facts) = self.finalize_references();
        (
            // A session that only checks emits no program, because the LSP only
            // reads the diagnostics and the graph.
            CompileResult::analysis_only(self.engine.diagnostics.clone(), references),
            facts,
        )
    }

    /// Build the workspace [`ReferenceGraph`] from the entry file's collector
    /// and the stored `module_refs` of every `CachedModule`. We rebuild it from
    /// scratch on each `check`, so an evicted module's reverse edges all go at
    /// once.
    fn build_reference_graph(&mut self) -> ReferenceGraph {
        // Intern every loaded module path plus main, so every module gets a
        // stable `ModuleId`.
        let loaded_paths: Vec<ModulePath> = self
            .module_table
            .loaded_modules()
            .map(|(_, cm)| cm.iface.path.clone())
            .collect();
        for p in &loaded_paths {
            self.ref_interner.intern(p);
        }
        self.ref_interner.intern(&module::main_module());

        // Copy the long-lived interner's id assignment. Interning in id order
        // gives the same ids, so the graph's `ModuleId`s match the ones already
        // baked into every `DefId`. Skipping one would renumber every later
        // module.
        let mut graph = ReferenceGraphBuilder::new();
        for p in self.ref_interner.paths() {
            graph.intern_module(p);
        }

        // Add every cached module's references. `finish()` builds the reverse
        // index once, not on each insert.
        for (_key, cm) in self.module_table.loaded_modules() {
            graph.insert(Rc::clone(cm.module_refs()));
        }

        // The entry file's own refs must be copied, because the collector is
        // used again for the next check.
        graph.insert(Rc::new(self.entry_refs.clone()));

        graph.finish()
    }

    /// Build the workspace [`ReferenceGraph`] and resolve the buffered
    /// occurrences into the [`HoverFact`] table.
    pub(super) fn finalize_references(&mut self) -> (Rc<ReferenceGraph>, Vec<HoverFact>) {
        let graph = self.build_reference_graph();
        // Only the LSP uses hover facts. Anywhere else `record` buffered
        // nothing, so skip the resolve pass over every occurrence.
        if !self.collect_hover_facts {
            return (Rc::new(graph), Vec::new());
        }
        let raw = std::mem::take(&mut self.recorded);
        let mut facts: Vec<HoverFact> = Vec::with_capacity(raw.len());
        // Nothing here unifies, so `resolve` depends only on the union-find
        // representative and we can memoise on it. Many occurrences share a
        // canonical `Ty`, and each resolve on its own adds arena nodes.
        let mut memo: HashMap<Ty, Type> = HashMap::new();
        for r in raw {
            let rep = self.engine.find(r.ty);
            let ty = match memo.get(&rep) {
                Some(t) => t.clone(),
                None => {
                    let t = self.engine.resolve(r.ty, Some(&self.env));
                    memo.insert(rep, t.clone());
                    t
                }
            };
            facts.push(HoverFact {
                module: r.module,
                span: r.span,
                name: r.name,
                ty,
                doc: r.doc,
            });
        }
        (Rc::new(graph), facts)
    }
}

/// A reusable compiler for the LSP that only checks. It holds the seeded stdlib
/// and a cache of compiled user modules. Each `check()` hashes the cached
/// modules' sources again, invalidates the changed ones and everything compiled
/// after them, truncates the arena to where the survivors end, and recompiles
/// the rest.
pub struct IncrementalSession {
    c: Compiler,
    seed: Watermark,
    /// The watermark before anything was seeded at all. A check with the
    /// prelude as its entry rewinds to this, because the prelude can't be
    /// checked on top of itself.
    bare: Watermark,
    /// Whether the prelude seed is in place right now. A check with the prelude
    /// as its entry tears it down (by rewinding to `bare`), and the next
    /// ordinary check seeds it again.
    seeded: bool,
    /// Module bodies compiled while seeding, which [`Self::compile_count`]
    /// leaves out.
    seed_compiles: u32,
    /// The watermark just before the previous entry body was analysed, which is
    /// after every imported module was compiled.
    last_entry: Option<Watermark>,
    /// The workspace reference graph. It is rebuilt from scratch at the end of
    /// every `check`, so an invalidated module's reverse edges all go at once.
    /// The `CompileResult` that `check` returns shares it.
    graph: Rc<ReferenceGraph>,
    /// The resolved type of each recorded occurrence, from the last `check`.
    type_facts: Vec<HoverFact>,
}

impl Default for IncrementalSession {
    fn default() -> Self {
        Self::new()
    }
}

impl IncrementalSession {
    /// A session over the stdlib embedded in the binary.
    pub fn new() -> Self {
        Self::with_stdlib_root(None)
    }

    /// A session that compiles the stdlib from the `.sca` sources under
    /// `stdlib_root` (the `src/std` in this repo) instead of the embedded copy.
    /// Use it when editing the stdlib itself. Every `scarlet/...` module is
    /// then an ordinary `File` module on disk, compiled, cached, hashed and
    /// invalidated just like user code, so the reference graph and hover facts
    /// are complete for stdlib sources.
    pub fn new_from_source(stdlib_root: std::path::PathBuf) -> Self {
        Self::with_stdlib_root(Some(stdlib_root))
    }

    fn with_stdlib_root(stdlib_root: Option<std::path::PathBuf>) -> Self {
        let mut c = new_compiler(true);
        c.collect_hover_facts = true;
        c.stdlib_source_root = stdlib_root;
        let bare = c.watermark();
        c.register_prelude();
        let seed = c.watermark();
        let seed_compiles = c.module_table.compile_count();
        IncrementalSession {
            c,
            seed,
            bare,
            seeded: true,
            seed_compiles,
            last_entry: None,
            graph: Rc::new(ReferenceGraph::new()),
            type_facts: Vec::new(),
        }
    }

    /// Check entries the way a REPL sends them, as a script of statements one
    /// piece at a time. Statements at module scope are the whole point there,
    /// and a binding whose use hasn't been typed yet is not an error. See
    /// [`ModuleScope`](crate::bytecode::ModuleScope) and
    /// [`UnusedBindings`](crate::bytecode::UnusedBindings).
    pub fn as_repl(&mut self) {
        self.c.unused_bindings = crate::bytecode::UnusedBindings::Ignore;
        self.c.module_scope = crate::bytecode::ModuleScope::Script;
    }

    /// How many module bodies the session's checks have compiled, not counting
    /// cache hits or the prelude it compiled when it started. Only used for
    /// telemetry.
    pub fn compile_count(&self) -> u32 {
        self.c.module_table.compile_count() - self.seed_compiles
    }

    /// Every rewind goes through here. `seed` is a hard floor, because
    /// everything below it is the stdlib compiled when the session started, and
    /// rewinding past it would drop the stdlib too. Enforcing that here instead
    /// of at each caller means a new caller can't forget.
    fn rewind_to(&mut self, w: Watermark) {
        self.c.reset_to(&w.later(self.seed));
    }

    pub fn reference_graph(&self) -> &ReferenceGraph {
        self.graph.as_ref()
    }

    /// The canonical module path a cached user module was loaded under, found
    /// by its source path on disk. Analysing the same file as the open entry
    /// would key it under `main` instead. So the LSP needs this to make a query
    /// inside an imported file resolve to the `DefId` its callers point at.
    pub fn module_path_for_source(&self, path: &Path) -> Option<&ModulePath> {
        self.c
            .module_table
            .user_modules()
            .find(|(_, cm)| cm.source_path() == Some(path))
            .map(|(_, cm)| &cm.iface.path)
    }

    /// Evict the cached module compiled from `path`, and everything that
    /// depends on it, so the next `check()` recompiles them. The LSP calls this
    /// from `didChangeWatchedFiles` when a file changes on disk outside the
    /// editor.
    pub fn invalidate_path(&mut self, path: &Path) {
        self.c.module_table.clear_overlay(path);
        let key = self
            .c
            .module_table
            .user_modules()
            .find(|(_, cm)| cm.source_path() == Some(path))
            .map(|(k, _)| k.clone());
        if let Some(k) = key
            && let Some(w) = self.c.module_table.invalidate(&k)
        {
            let floor = self
                .last_entry
                .map_or(w, |le| le.earlier(w))
                .later(self.seed);
            self.last_entry = Some(floor);
        }
    }

    pub fn set_overlay(&mut self, path: PathBuf, text: String) {
        self.c.module_table.set_overlay(path, text);
    }

    pub fn check(&mut self, expr: &ast::Expression, base_dir: Option<&Path>) -> CompileResult {
        self.check_impl(expr, base_dir, module::main_module())
    }

    /// [`check`](Self::check), but analysing the buffer *as* the given module
    /// instead of as `main`. A session that compiles the stdlib from source
    /// uses this, so an open `src/std/**/*.sca` file keeps its canonical
    /// `scarlet/...` identity. Then `@vm` is allowed, the prelude-redefinition
    /// rules apply, and every def, occurrence and hover fact is keyed under the
    /// module its importers refer to.
    pub fn check_as(
        &mut self,
        expr: &ast::Expression,
        base_dir: Option<&Path>,
        module: ModulePath,
    ) -> CompileResult {
        self.check_impl(expr, base_dir, module)
    }

    fn check_impl(
        &mut self,
        expr: &ast::Expression,
        base_dir: Option<&Path>,
        entry_module: ModulePath,
    ) -> CompileResult {
        // Remove the backpass sugar before any typing pass, as `compile_impl`
        // does.
        let mut expr = expr.clone();
        crate::desugar::desugar_expression(&mut expr);
        let expr = &expr;

        // The prelude can't be analysed on top of itself. So tear the seed down
        // to `bare` and compile the buffer as the whole world. The next
        // ordinary check seeds it again below.
        let prelude_self = entry_module == module::scarlet_prelude();
        if prelude_self {
            self.c.module_table.invalidate_all();
            self.c.reset_to(&self.bare);
            self.seeded = false;
            self.last_entry = None;
        } else if !self.seeded {
            // An earlier check with the prelude as its entry tore the seed
            // down. Rebuild the session from scratch, so its state is exactly a
            // fresh session's and we don't have to reason about a half-rewound
            // world. This only happens after you edit `scarlet.sca` and switch
            // file, so the full stdlib recompile it costs is fine.
            *self = Self::with_stdlib_root(self.c.stdlib_source_root.clone());
        }

        // Drop what the previous entry added. The cached modules' arena state
        // sits below this line and survives.
        let mut floor = self.last_entry.unwrap_or(self.seed);

        // Collect the keys first, so the scan for stale files can take
        // `&mut module_table` for its stat cache without overlapping the
        // `user_modules()` borrow. Modules baked in at or below the seed (the
        // prelude, when compiled from source) are left out. They can't be
        // rewound past, so the owner handles a change there by dropping the
        // session.
        let seed = self.seed;
        let candidates: Vec<module::ModuleKey> = self
            .c
            .module_table
            .user_modules()
            .filter(|(_, cm)| cm.watermark >= seed)
            .map(|(k, _)| k.clone())
            .collect();
        let dirty: Vec<module::ModuleKey> = candidates
            .into_iter()
            .filter(|k| self.c.module_table.source_changed(k))
            .collect();
        for k in dirty {
            if let Some(w) = self.c.module_table.invalidate(&k) {
                floor = floor.earlier(w);
            }
        }
        if !prelude_self {
            self.rewind_to(floor);
        }

        // Work out the entry's identity from this check alone, so its defs land
        // under the module id its importers target.
        let entry_key = if prelude_self || module::is_stdlib(&entry_module) {
            module::ModuleKey::for_stdlib(&entry_module)
        } else {
            module::ModuleKey::main()
        };

        self.compile_entry(expr, &entry_module, &entry_key, base_dir);

        // This is the fallback for overflow. A recompiled module spilled past
        // its reused id range and may have collided with another module's
        // range. So evict everything, drop the id bases and recompile once.
        // Every module now gets a fresh range that fits its current usage, so
        // the flag can't be set again on this pass. That's why this runs once
        // and isn't a loop.
        if self.c.module_table.id_range_overflow()
            && let Some(w) = self.c.module_table.invalidate_all()
        {
            self.c.module_table.reset_id_bases();
            self.rewind_to(w);
            self.last_entry = None;
            self.compile_entry(expr, &entry_module, &entry_key, base_dir);
        }
        let (result, facts) = self.c.snapshot_result();
        self.graph = result.references.clone();
        self.type_facts = facts;
        result
    }

    /// Compile the entry expression and capture its `last_entry` watermark.
    fn compile_entry(
        &mut self,
        expr: &ast::Expression,
        path: &ModulePath,
        key: &module::ModuleKey,
        base_dir: Option<&Path>,
    ) {
        let importer = Importer { dir: base_dir, key };
        let loaded = self.c.load_entry_imports(expr, &importer);
        // What the entry imports sits below this line and survives the next
        // check. What the entry adds itself sits above it.
        self.last_entry = Some(self.c.watermark());
        let mut checker = ModuleChecker::new(&mut self.c, path.clone(), key.clone());
        checker.c.env.push_scope();
        checker.check_entry(expr, loaded);
        checker.c.env.pop_scope();
        self.c.entry_refs = checker.finish();
    }

    /// Resolve a module key to its interned `ModuleId`, where `None` means the
    /// entry (`main`) module. A key that names no interned module must resolve
    /// to `None` and never fall back to the entry, or a stale URI would answer
    /// queries with another file's facts.
    fn module_for(&self, module_key: Option<&module::ModuleKey>) -> Option<ModuleId> {
        match module_key {
            Some(key) => self.graph.module_id_by_key(key),
            None => self.graph.module_id_by_key(&module::ModuleKey::main()),
        }
    }

    /// The start of the type-id range reserved for module `key`, if one was
    /// allocated. The incremental test harness uses this to check that ranges
    /// are reused.
    pub fn module_id_base(&self, key: &module::ModuleKey) -> Option<TypeId> {
        self.c.module_table.id_base_of(key)
    }

    /// The name, inferred type and doc at a position. The tightest fact that
    /// contains the cursor wins, as in `resolve_position`, so the type of a
    /// nested expression beats the one around it, whichever was recorded first.
    pub fn hover(
        &self,
        module_key: Option<&module::ModuleKey>,
        line: i32,
        col: i32,
    ) -> Option<(String, Type, Option<String>)> {
        let m = self.module_for(module_key)?;
        let f = self
            .type_facts
            .iter()
            .filter(|f| f.module == m && f.span.contains(line, col))
            .min_by_key(|f| f.span.width())?;
        Some((f.name.clone(), f.ty.clone(), f.doc.clone()))
    }

    /// What the module at `path` exports, types first. If no check has imported
    /// the module yet, we compile it first by checking an entry that imports
    /// it, so it reaches the cache the same way as any other import. A module
    /// that doesn't resolve or doesn't compile exports nothing.
    pub fn exports(&mut self, path: &ModulePath) -> Vec<Export> {
        let key = module::ModuleKey::of(path);
        if self.c.module_table.get(&key).is_none() {
            let mut scanner = crate::scanner::new_scanner(format!("import {key}\n"));
            let parsed = crate::parser::new_parser(&mut scanner).parse_program();
            if crate::diagnostic::has_errors(&parsed.diagnostics) {
                return Vec::new();
            }
            self.check(&ast::Expression::BlockExpression(parsed.ast), None);
        }
        let Some(iface) = self.c.module_table.get(&key) else {
            return Vec::new();
        };
        let engine = &self.c.engine;
        let types = iface.types.keys().map(|name| Export {
            name: name.clone(),
            params: Vec::new(),
        });
        let values = iface.values.iter().map(|(name, ev)| {
            let params = match ev.scheme.kind {
                ValueKind::ModuleFn { param_labels } | ValueKind::Builtin { param_labels, .. } => {
                    engine.strs_of(param_labels)
                }
                ValueKind::Constructor { field_labels, .. } => engine.strs_of(field_labels),
                ValueKind::Local => Vec::new(),
            };
            Export {
                name: name.clone(),
                params,
            }
        });
        // Give each name one entry. A record type names its only constructor
        // after itself.
        let mut seen = HashSet::new();
        types
            .chain(values)
            .filter(|e| seen.insert(e.name.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use super::{Export, IncrementalSession, Watermark};
    use crate::bytecode::compiler::new_compiler;
    use crate::core_ir::{Atom, Const, CoreExpr, CoreFn, LoweredFn};
    use crate::type_def::TypeId;
    use crate::typed_ir::RTy;
    use crate::types::{EnvWatermark, PrimIds};

    /// Watermarks with equal `ord_key` can carry different env payloads.
    /// `earlier` must be symmetric and merge them toward the deeper rollback.
    #[test]
    fn earlier_merges_env_payloads_on_ord_key_ties() {
        let env_a = EnvWatermark {
            root_scope: 3,
            type_info: 9,
            type_info_by_id: 2,
            definitions: 8,
            docs: 1,
            journal: 7,
            next_type_id: TypeId(4),
        };
        let env_b = EnvWatermark {
            root_scope: 5,
            type_info: 6,
            type_info_by_id: 2,
            definitions: 4,
            docs: 2,
            journal: 3,
            next_type_id: TypeId(9),
        };
        let a = Watermark {
            env: env_a,
            ..Watermark::default()
        };
        let b = Watermark {
            env: env_b,
            ..Watermark::default()
        };
        assert_eq!(a, b, "test premise: equal ord_key despite differing envs");

        let want_min = EnvWatermark {
            root_scope: 3,
            type_info: 6,
            type_info_by_id: 2,
            definitions: 4,
            docs: 1,
            journal: 3,
            next_type_id: TypeId(4),
        };
        assert_eq!(a.earlier(b).env, want_min);
        assert_eq!(b.earlier(a).env, want_min);

        let want_max = EnvWatermark {
            root_scope: 5,
            type_info: 9,
            type_info_by_id: 2,
            definitions: 8,
            docs: 2,
            journal: 7,
            next_type_id: TypeId(9),
        };
        assert_eq!(a.later(b).env, want_max);
        assert_eq!(b.later(a).env, want_max);
    }

    /// When there's no tie, `earlier` and `later` follow `Ord` and keep the
    /// winner's env.
    #[test]
    fn earlier_and_later_follow_ord_when_keys_differ() {
        let older = Watermark::default();
        let newer = Watermark {
            functions: 10,
            env: EnvWatermark {
                root_scope: 4,
                ..EnvWatermark::default()
            },
            ..Watermark::default()
        };
        assert_eq!(older.earlier(newer).env, older.env);
        assert_eq!(newer.earlier(older).env, older.env);
        assert_eq!(older.later(newer).env, newer.env);
        assert_eq!(newer.later(older).env, newer.env);
    }

    /// `reset_to` rewinds the function table and the constant pool to the
    /// watermark, and drops every lowered toplevel, because the init code of a
    /// compile we rewound must not run as part of the next one.
    #[test]
    fn reset_to_rewinds_functions_and_consts_and_drops_toplevels() {
        let mut c = new_compiler(false);
        let pool = Rc::new(crate::typed_ir::ResolvedPool::new(PrimIds::default()));
        let top = LoweredFn::new(
            crate::module::ModuleKey::main().to_string(),
            "m".to_string(),
            CoreFn {
                params: Vec::new(),
                body: CoreExpr::Tail(Atom::Nil),
                ret_ty: RTy(0),
            },
            pool,
        );

        // Measure from whatever `new_compiler` seeded, not a fixed number, so
        // the test still works when the seed grows.
        let (fns, consts) = (c.fns.len(), c.consts.len());
        c.fns.push(None);
        c.consts.push(Const::Int(1));
        let w = c.watermark();
        c.fns.push(None);
        c.consts.push(Const::Int(2));
        c.inits.push(top);

        c.reset_to(&w);

        assert_eq!(
            c.fns.len(),
            fns + 1,
            "function slots rewind to the watermark"
        );
        assert_eq!(c.consts.len(), consts + 1, "consts rewind to the watermark");
        assert!(c.inits.is_empty(), "a rewound compile's inits are dropped");
    }

    fn path(s: &str) -> crate::module::ModulePath {
        s.split('/').map(str::to_string).collect()
    }

    fn export<'a>(exports: &'a [Export], name: &str) -> &'a Export {
        exports
            .iter()
            .find(|e| e.name == name)
            .unwrap_or_else(|| panic!("{name} is not exported"))
    }

    #[test]
    fn a_modules_functions_export_with_their_parameter_names() {
        let mut session = IncrementalSession::new();
        let exports = session.exports(&path("scarlet/string"));
        assert_eq!(
            export(&exports, "replace").params,
            ["s", "pattern", "replacement"]
        );
        // `@vm` functions carry their names the same way.
        assert_eq!(export(&exports, "length").params, ["s"]);
    }

    /// The prelude is compiled when the session starts, so it is listed
    /// without another check running.
    #[test]
    fn the_prelude_exports_without_a_check() {
        let mut session = IncrementalSession::new();
        let before = session.compile_count();
        let exports = session.exports(&path("scarlet"));
        assert_eq!(export(&exports, "println").params, ["x"]);
        assert_eq!(session.compile_count(), before);
    }

    #[test]
    fn a_constructor_exports_with_its_field_labels() {
        let mut session = IncrementalSession::new();
        let exports = session.exports(&path("scarlet/http"));
        assert_eq!(export(&exports, "Fixed").params, ["len", "b"]);
        assert!(export(&exports, "Post").params.is_empty());
        // A record's constructor shares its type's name and is listed once.
        let responses = exports.iter().filter(|e| e.name == "Response").count();
        assert_eq!(responses, 1);
    }

    /// The prelude compiles when the session starts, not in a check.
    #[test]
    fn a_new_session_counts_no_compiles() {
        assert_eq!(IncrementalSession::new().compile_count(), 0);
    }

    #[test]
    fn an_unknown_module_exports_nothing() {
        let mut session = IncrementalSession::new();
        assert!(session.exports(&path("scarlet/nope")).is_empty());
    }

    #[test]
    fn the_stdlib_lists_nested_modules_and_the_prelude() {
        let modules = crate::module::stdlib_modules();
        assert!(modules.contains(&path("scarlet")));
        assert!(modules.contains(&path("scarlet/net/tls")));
        assert!(modules.is_sorted());
    }
}
