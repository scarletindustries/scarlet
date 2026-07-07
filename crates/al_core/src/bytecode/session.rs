//! The LSP/workspace layer over [`Compiler`]: incremental recompilation and
//! the reference-graph queries answered from it.
//!
//! Split from `compiler.rs` along the seam its own file map already named:
//!
//! - **Reference-graph scaffolding** (`RawRef` → [`HoverFact`]): every name
//!   occurrence is buffered with its live `Ty` during the pass and lowered
//!   into the workspace `ReferenceGraph` once all unifications have settled.
//! - **Incremental recompilation** ([`Watermark`] / `reset_to`): a snapshot
//!   is six lengths, a rollback is six truncations.
//! - **[`IncrementalSession`]**: owns a `Compiler` across edits, invalidates
//!   cached modules, answers hover/goto-def/find-refs/rename from the
//!   reference graph.
//!
//! `compiler.rs` keeps the pass itself; this file owns the state that
//! survives *between* passes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use super::Program;
use super::compiler::{CompileResult, Compiler, new_compiler};
use crate::ast;
use crate::diagnostic::has_errors;
use crate::module::{self, ModulePath};
use crate::reference::{Definition, ModuleId, ModuleReferences, ReferenceGraph};
use crate::span::Span;
use crate::type_def::{Type, TypeId};
use crate::types::{DefinitionLocation, EnginePoolWatermark, EnvWatermark, Ty};

// ============================================================================
// Reference-graph collection scaffolding.
//
// Every name occurrence is buffered as a `RawRef` during the typecheck/infer
// pass holding the *live* `Ty` (resolution is deferred until all unifications
// have settled). At finalize the buffer is lowered into the workspace
// `reference::ReferenceGraph` (name→definition identity) plus a `HoverFact`
// table that joins the resolved type back in — the graph is deliberately
// inference-free, so the session layer owns the type join. This fully
// replaces the old flat `TypePosition` path.
// ============================================================================

#[derive(Debug, Clone)]
pub(super) struct RawRef {
    pub(super) span: Span,
    pub(super) name: String,
    pub(super) ty: Ty,
    pub(super) doc: Option<String>,
    /// Interned id of the module this occurrence was recorded in. Resolved
    /// once at `record` time (memoised), so `finalize_references` builds the
    /// `HoverFact` without re-interning the path per occurrence.
    pub(super) module: ModuleId,
}

/// Resolved type at one occurrence span, used by `IncrementalSession::hover`
/// to join an inferred type onto the graph's identity-only result. The
/// reference graph is deliberately inference-free, so the session layer owns
/// this type join.
#[derive(Debug, Clone)]
pub struct HoverFact {
    pub module: ModuleId,
    pub span: Span,
    pub name: String,
    pub ty: Type,
    pub doc: Option<String>,
}

// ============================================================================
// Incremental recompilation
// ============================================================================

/// Full snapshot of every append-only compiler structure, captured at module
/// boundaries so an `IncrementalSession` can roll back exactly to that point.
/// Ordered so `min()` over a set of watermarks picks the earliest-compiled
/// one; see [`ord_key`](Self::ord_key) for the comparison key.
#[derive(Debug, Clone, Copy, Default)]
pub struct Watermark {
    pub engine: EnginePoolWatermark,
    pub env: EnvWatermark,
    pub code: usize,
    pub functions: usize,
    pub constants: usize,
    pub local_count: i32,
}

impl Watermark {
    /// The "earlier/later" comparison key. Every field here is the length of
    /// an append-only pool (or a monotone counter), so a watermark captured
    /// earlier compares `<=` one captured later regardless of which entry the
    /// tuple happens to differ on first — `ModuleTable::invalidate` relies on
    /// `min` picking the earliest-compiled module. `env` is deliberately
    /// excluded: it is a rollback payload for `TypeEnv::truncate_to`, and
    /// keeping it out means `EnvWatermark`'s field set can grow or reorder
    /// without any risk of perturbing this ordering.
    fn ord_key(&self) -> (EnginePoolWatermark, usize, usize, usize, i32) {
        (
            self.engine,
            self.code,
            self.functions,
            self.constants,
            self.local_count,
        )
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
    pub fn watermark(&self) -> Watermark {
        Watermark {
            engine: self.engine.pool_watermark(),
            env: self.env.watermark(),
            code: self.program.code.len(),
            functions: self.program.functions.len(),
            constants: self.program.constants.len(),
            local_count: self.local_count,
        }
    }

    /// Roll every pool/map back to `w` and clear per-compile transient state so
    /// the compiler is exactly as it was when `w` was captured. `module_table`
    /// is left untouched: the caller (`IncrementalSession`) decides which
    /// cached modules survive via `ModuleTable::invalidate`.
    pub fn reset_to(&mut self, w: &Watermark) {
        self.engine.truncate_to(&w.engine);
        self.env.truncate_to(&w.env);
        // The rewind above leaves only the persistent root scope, which holds
        // the immutable prelude seed (plus cached-module state below the
        // watermark). Layer a fresh, throwaway scope on top for this compile's
        // imports. `truncate_to` rewinds the root scope by *length*, so an
        // in-place `define` overwrite (an `IndexMap::insert`) below the
        // watermark could never be undone: a selective import that shadows a
        // shadowable prelude name (e.g. `import ./lib.{println}`) would clobber
        // the prelude binding in the root scope and the by-length rollback could
        // not restore it, corrupting the prelude for the rest of the session.
        // Keeping imports in this discard-and-rebuild layer means they never
        // mutate the root scope, so the rollback stays exact.
        self.env.push_scope();
        self.program.code.truncate(w.code);
        self.program.functions.truncate(w.functions);
        self.program.constants.truncate(w.constants);
        self.local_count = w.local_count;
        // Survivors are watermark-preserved entry-frame slots (e.g. `__pre*`,
        // imports). Scope state is fully cleared, so normalise their depth to 0
        // ("pre-existing, outermost"): the next opened scope then treats them as
        // inherited bindings, exactly as the old full-snapshot restore did.
        self.locals.retain(|_, v| {
            if v.slot < w.local_count {
                v.depth = 0;
                true
            } else {
                false
            }
        });

        self.undo_log.clear();
        self.scope_marks.clear();
        self.or_bind_overrides.clear();
        self.unused.clear();
        self.outer_scopes.clear();
        self.captures.clear();
        self.capture_names.clear();
        self.current_binding = None;
        self.next_fn_self_name = None;
        self.in_tail_position = false;
        self.current_owner = None;
        self.rigid_ids.clear();
        self.recorded.clear();
        // The transient reference collector is per-compile like `recorded`.
        // `ref_interner` is deliberately *not* cleared (persistent like
        // `module_table`) so `DefId`s in surviving `CachedModule.module_refs`
        // keep resolving to stable `ModuleId`s across recompiles.
        let main_id = self.ref_interner.intern(&module::main_module());
        self.module_refs = ModuleReferences::new(main_id);
        self.imported_qualifiers.clear();
        self.current_module = module::main_module();
        self.module_path_slice = None;
        // The `str_slices` pool was just rewound, so an `ArenaSlice` may now
        // denote a different path than it did last compile — drop the memo.
        self.defid_module_memo.clear();
        self.module_table.unmark_all_loading();
    }

    /// Materialise a `CompileResult` by *cloning* (not taking) so the session
    /// can be reused, alongside the hover-type table the session answers
    /// `hover` from. The workspace reference graph is rebuilt here and handed
    /// back inside the result so the owning session can keep querying it.
    fn snapshot_result(&mut self) -> (CompileResult, Vec<HoverFact>) {
        let (references, facts) = self.finalize_references();
        let success = !has_errors(&self.engine.diagnostics);
        (
            CompileResult {
                // The session is check-only and its consumer (the LSP) reads
                // only `diagnostics` + the reference graph, never `program`.
                // Cloning the seeded, hydrated stdlib `Program`
                // (code+functions+constants, each constant string a fresh `Rc`
                // bump) on every keystroke is pure waste, so hand back an empty
                // program. The cheap `Copy` scalar metadata is preserved in
                // case a future consumer inspects it.
                program: Program {
                    entry: self.program.entry,
                    ..Program::default()
                },
                diagnostics: self.engine.diagnostics.clone(),
                references,
                success,
            },
            facts,
        )
    }

    /// Synthesise reference-graph [`Definition`]s for a static/hydrated stdlib
    /// module straight from its exported values' `Scheme.def`. The precompiled
    /// stdlib already carries the real declaration span on every exported
    /// value, so goto-def into `al/*` lands on the true source location without
    /// serialising a separate static reference blob (the spec's sanctioned lazy
    /// alternative).
    ///
    /// Every `DefId` — and the owning [`ModuleReferences`] container — is keyed
    /// through [`Compiler::defid_of`], the exact computation a *populated* use
    /// of the same name bakes into its occurrence target. The two therefore
    /// share one canonical `DefId` and `ReferenceGraph::definition()` resolves
    /// across the module boundary even when the precompiled `Scheme.def.module`
    /// path spelling differs from the `ModuleTable` key. Returns `None` for a
    /// value-less (e.g. types-only) interface so the caller can skip an
    /// otherwise-empty graph rebuild. Types are intentionally not synthesised:
    /// `TypeInfo` has no declaration span, so type goto-def into the stdlib is
    /// served by the populated path, not this fallback.
    fn synth_refs_from_interface(
        &mut self,
        defs: &[(String, DefinitionLocation)],
    ) -> Option<ModuleReferences> {
        // The container must be keyed by the same `ModuleId` the use side
        // bakes into the occurrence target (`defid_of`), so that
        // `definition()`'s `modules.get(&target.module)` lands here.
        let mid = self.defid_of(defs.first()?.1).module;
        let mut mr = ModuleReferences::new(mid);
        for (name, dl) in defs {
            let defid = self.defid_of(*dl);
            mr.add_definition(Definition::new(defid, name.clone(), None, true));
        }
        Some(mr)
    }

    /// Build the workspace [`ReferenceGraph`] wholesale from the entry file's
    /// transient collector, every from-source `CachedModule`'s persisted
    /// `module_refs`, and — for static/hydrated stdlib modules that carry none
    /// — `Definition`s synthesised from the hydrated interface. The stdlib's
    /// precompiled `Scheme.def` already holds the real declaration span, so
    /// goto-def into `al/*` lands correctly without serialising a separate
    /// static reference blob. Built wholesale each `check` so an evicted
    /// module's reverse edges vanish coherently.
    fn build_reference_graph(&mut self) -> ReferenceGraph {
        // 1. Intern every loaded module path plus the entry/main module so a
        //    synthesised stdlib def gets a stable `ModuleId`.
        let loaded_paths: Vec<ModulePath> = self
            .module_table
            .loaded_modules()
            .map(|(_, cm)| cm.iface.path.clone())
            .collect();
        for p in &loaded_paths {
            self.ref_interner.intern(p);
        }
        self.ref_interner.intern(&module::main_module());

        // 2. Mirror the persistent interner's first-seen id assignment into the
        //    graph: interning in id order reproduces identical ids, so the
        //    graph's `ModuleId`s match the ones already baked into every
        //    persisted / freshly-collected `DefId`.
        let mut graph = ReferenceGraph::new();
        for i in 0..self.ref_interner.len() as u32 {
            if let Some(p) = self.ref_interner.path(ModuleId(i)).cloned() {
                graph.intern_module(&p);
            }
        }

        // 3. Every cached module's references; for static/hydrated stdlib
        //    modules (no collected refs) synthesise definitions from the
        //    interface so cross-module goto-def into `al/*` resolves. Insert
        //    deferred: a single `rebuild()` (step 5) is a pure function of the
        //    final module set, so M back-to-back inserts cost O(total
        //    occurrences) once instead of M full workspace rescans on every
        //    incremental `check`. The persisted refs are shared via `Rc`, so
        //    re-inserting an unchanged module is a refcount bump rather than a
        //    deep copy of its occurrences/definitions every keystroke.
        let mut synth_inputs: Vec<Vec<(String, DefinitionLocation)>> = Vec::new();
        for (_key, cm) in self.module_table.loaded_modules() {
            match cm.module_refs() {
                Some(mr) => graph.insert_module_deferred(Rc::clone(mr)),
                None => {
                    let defs: Vec<(String, DefinitionLocation)> = cm
                        .iface
                        .values
                        .iter()
                        .filter_map(|(name, ev)| ev.scheme.def.map(|dl| (name.clone(), dl)))
                        .collect();
                    if !defs.is_empty() {
                        synth_inputs.push(defs);
                    }
                }
            }
        }
        // Resolve each synthesised def's `ModuleId` through `defid_of` — a
        // `&mut self` borrow that cannot overlap the `module_table` iteration
        // above — so the synthesised `DefId` is bit-identical to the one a
        // cross-module use of the same name records as its occurrence target.
        for defs in &synth_inputs {
            if let Some(synth) = self.synth_refs_from_interface(defs) {
                graph.insert_module_deferred(Rc::new(synth));
            }
        }

        // 4. The entry/open file's freshly-collected references. This is the
        //    one unavoidable copy — the edited buffer's own refs, O(edited
        //    file) — since the collector is reused for the next check.
        graph.insert_module_deferred(Rc::new(self.module_refs.clone()));

        // 5. Materialise the workspace reverse index exactly once.
        graph.rebuild();
        graph
    }

    /// Build the workspace [`ReferenceGraph`] and resolve the buffered
    /// occurrences into the [`HoverFact`] table. The graph carries name→def
    /// identity only; the resolved `Type` for hover is joined back in here
    /// (the "session layer") since the graph is deliberately inference-free.
    pub(super) fn finalize_references(&mut self) -> (Rc<ReferenceGraph>, Vec<HoverFact>) {
        let graph = self.build_reference_graph();
        // Only the LSP consumes the `HoverFact` table. On the free
        // `compile`/`check` path `record` buffered nothing, so the O(occurrences)
        // resolve pass below is pure waste — skip it and hand back the graph
        // only (tests read `CompileResult.references`, never the facts).
        if !self.collect_hover_facts {
            return (Rc::new(graph), Vec::new());
        }
        let raw = std::mem::take(&mut self.recorded);
        let mut facts: Vec<HoverFact> = Vec::with_capacity(raw.len());
        // The engine is frozen here (finalization never unifies), so
        // `resolve` is a pure function of the union-find representative.
        // Many occurrences share a canonical `Ty` (every use of a variable,
        // every monomorphic call to a fn); resolving each one re-clones all
        // variants/fields and re-runs `substitute_type_vars` (which appends
        // arena nodes). Memoise on the representative and clone the cached
        // `Type` for duplicates instead.
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

// ============================================================================
// IncrementalSession
// ============================================================================

/// A reusable, check-only compiler for the LSP. Holds the seeded stdlib and a
/// cache of compiled user modules; on each `check()` it re-hashes cached
/// module sources, invalidates the changed ones (and everything compiled
/// after them), truncates the arena back to the surviving boundary, and
/// recompiles only what's needed.
pub struct IncrementalSession {
    c: Compiler,
    seed: Watermark,
    /// Watermark immediately before the previous entry-body analysis, i.e.
    /// after every imported module had been compiled. The next `check()`
    /// truncates here first (discarding only the previous entry's own heap
    /// contributions) before deciding whether any module needs invalidating.
    last_entry: Option<Watermark>,
    /// Workspace reference graph, rebuilt from scratch at the end of every
    /// `check` by merging the entry file's freshly-collected references with
    /// every surviving cached module's `ModuleReferences`. Rebuilding wholesale
    /// (rather than mutating in place) is what makes an invalidated module's
    /// reverse edges disappear coherently. An `Rc` so the same graph instance
    /// is shared with the `CompileResult` handed back from `check`.
    graph: Rc<ReferenceGraph>,
    /// Resolved type per recorded occurrence, from the last `check`. The graph
    /// is identity-only, so `hover` joins the inferred type from here.
    type_facts: Vec<HoverFact>,
}

impl IncrementalSession {
    pub fn new(stdlib: &'static crate::static_ir::StaticStdlib) -> Self {
        let mut c = new_compiler(None, true);
        // The LSP is the only consumer of `HoverFact`s; enable the
        // per-occurrence resolve pass for this session only.
        c.collect_hover_facts = true;
        c.seed_static(stdlib);
        let seed = c.watermark();
        IncrementalSession {
            c,
            seed,
            last_entry: None,
            graph: Rc::new(ReferenceGraph::new()),
            type_facts: Vec::new(),
        }
    }

    pub fn compile_count(&self) -> u32 {
        self.c.module_table.compile_count()
    }

    pub fn reference_graph(&self) -> &ReferenceGraph {
        self.graph.as_ref()
    }

    /// The canonical module path a cached *user* module was loaded under,
    /// located by its on-disk source path. An open file that some other file
    /// imports is keyed in the workspace graph under this path (e.g.
    /// `["." , "lib"]`) — the identity every caller's reverse edge targets —
    /// whereas analysing it as the open entry would key it under the bare
    /// `main` module. The LSP uses this to resolve a position query driven from
    /// an imported file's own declaration to the same `DefId` its callers point
    /// at. `None` when no cached module came from `path`.
    pub fn module_path_for_source(&self, path: &Path) -> Option<&ModulePath> {
        self.c
            .module_table
            .user_modules()
            .find(|(_, cm)| cm.source_path() == Some(path))
            .map(|(_, cm)| &cm.iface.path)
    }

    /// Evict the cached module compiled from `path` (and its dependents) so the
    /// next `check()` recompiles it. Called from LSP `didChangeWatchedFiles`
    /// when a file changes on disk outside the editor. Drops any overlay for
    /// the path so disk content is re-read, and lowers `last_entry` to the
    /// evicted module's watermark so the arena is truncated correctly.
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
            let floor = self.last_entry.map_or(w, |le| le.min(w)).max(self.seed);
            self.last_entry = Some(floor);
        }
    }

    pub fn set_overlay(&mut self, path: PathBuf, text: String) {
        self.c.module_table.set_overlay(path, text);
    }

    pub fn check(&mut self, expr: &ast::Expression, base_dir: Option<&Path>) -> CompileResult {
        // 1. Drop the previous entry's contributions; cached modules' arena
        //    state is below this line and survives intact.
        let mut floor = self.last_entry.unwrap_or(self.seed);

        // 2. Detect which cached user modules changed and invalidate them.
        //    `check` runs per LSP keystroke, so the unchanged-file fast path
        //    must not read+hash every dependency: `source_changed` stat-gates
        //    on `(mtime, len)` and only falls through to a full read+hash when
        //    that tuple moved. Each invalidate cascades through dependents and
        //    evicts later-compiled modules, returning the earliest watermark
        //    touched. (Keys are collected first so the staleness scan can take
        //    `&mut module_table` for its stat cache without overlapping the
        //    `user_modules()` borrow.)
        let candidates: Vec<String> = self
            .c
            .module_table
            .user_modules()
            .map(|(k, _)| k.clone())
            .collect();
        let dirty: Vec<String> = candidates
            .into_iter()
            .filter(|k| self.c.module_table.source_changed(k))
            .collect();
        for k in dirty {
            if let Some(w) = self.c.module_table.invalidate(&k) {
                floor = floor.min(w);
            }
        }
        floor = floor.max(self.seed);

        // 3. Truncate to the surviving boundary and recompile.
        self.c.reset_to(&floor);
        self.c.base_dir = base_dir.map(|p| p.to_path_buf());

        self.compile_entry(expr);

        // Overflow fallback (rare path; strictly flag-guarded so the common
        // case is zero-cost). A recompiled module reused its assigned id range
        // but spilled past `MODULE_TYPE_ID_RANGE`, so it may have collided with
        // a sibling's already-assigned block. Evict every user module, drop
        // every `id_base` assignment, truncate back to the earliest evicted
        // watermark, and recompile the entry once. Every module is now a fresh
        // (non-reused) allocation, so ranges are re-sized to current usage and
        // the overflow flag cannot be re-raised this pass — hence a single
        // pass, not a loop.
        if self.c.module_table.id_range_overflow()
            && let Some(w) = self.c.module_table.invalidate_all()
        {
            self.c.module_table.reset_id_bases();
            let floor = w.max(self.seed);
            self.c.reset_to(&floor);
            self.last_entry = None;
            self.compile_entry(expr);
        }
        // `snapshot_result` rebuilds the workspace graph wholesale (merging the
        // entry file's freshly-collected references with every surviving cached
        // module) so an invalidated module leaves no dangling reverse edge, and
        // resolves the hover-type table. The session shares the same graph Rc
        // and keeps the type table to answer LSP queries.
        let (result, facts) = self.c.snapshot_result();
        self.graph = result.references.clone();
        self.type_facts = facts;
        result
    }

    /// Compile the entry expression and capture its `last_entry` watermark.
    /// Shared by [`Self::check`]'s normal path and its id-range-overflow
    /// fallback.
    fn compile_entry(&mut self, expr: &ast::Expression) {
        if let ast::Expression::BlockExpression(block) = expr {
            // `env.type_info` is a flat map, not a scope stack, so a selective
            // `import m.{Type}` binding written by `process_imports` is not
            // confined to the throwaway scope the way a value binding is. Record
            // the env watermark *before* the entry's imports run so the
            // `last_entry` watermark excludes them; otherwise the binding folds
            // into the watermark and the next check's `reset_to` preserves it,
            // leaving a removed or renamed type import still resolving to a
            // stale `TypeInfo` with no diagnostic. The entry re-binds whatever
            // it imports from the (persistent) cached module interfaces on
            // every check, so rolling this map back to the pre-import position
            // discards only re-derivable lookup state — never the engine arena
            // those interfaces point into. The journal position rolls back with
            // it: an entry type that shadowed a seeded stdlib name (`type
            // Parsed = ...` over al/http/h1's `Parsed`) overwrote that entry
            // in-place below the watermark, and replaying the journal is what
            // restores the stdlib value on the next check.
            let pre_import = self.c.env.watermark();
            self.c.process_imports(block);
            // Must precede the `last_entry` watermark capture below so the
            // seed reflects the bumped id position.
            self.c.bump_type_ids_past_reserved();
            // The watermark records the persistent root scope (prelude seed)
            // plus the bumped id position; the entry's imports live in the
            // throwaway scope `reset_to` layered on top (value bindings) or are
            // rewound to the pre-import type_info/journal position (type
            // bindings), so they — like the entry body analysed below — are
            // discarded and rebuilt next time.
            let mut wm = self.c.watermark();
            wm.env.type_info = pre_import.type_info;
            wm.env.journal = pre_import.journal;
            self.last_entry = Some(wm);
            self.c.analyse_module(block, None);
        } else {
            self.last_entry = Some(self.c.watermark());
            self.c.compile_expr(expr);
        }
    }

    /// Resolve a `ModulePath` key / file URI to its interned `ModuleId`,
    /// falling back to the entry (`main`) module so a single-open-file LSP
    /// caller can omit the path.
    fn module_for(&self, module_or_uri: &str) -> Option<ModuleId> {
        self.graph.module_id_by_key(module_or_uri).or_else(|| {
            self.graph
                .module_id_by_key(&module::path_key(&module::main_module()))
        })
    }

    /// The base of the type-id range reserved for module `key`, if one was
    /// allocated. Used by the incremental test harness to assert range reuse.
    pub fn module_id_base(&self, key: &str) -> Option<TypeId> {
        self.c.module_table.id_base_of(key)
    }

    /// hover: name + inferred type + doc. The reference graph is identity-only,
    /// so the inferred `Type` is joined from the session's hover-type table.
    /// The tightest fact containing the cursor wins (min span-width, mirroring
    /// `resolve_position`) so a nested sub-expr's type beats an enclosing one
    /// rather than whichever was recorded first.
    pub fn hover(
        &self,
        module_or_uri: &str,
        line: i32,
        col: i32,
    ) -> Option<(String, Type, Option<String>)> {
        let m = self.module_for(module_or_uri)?;
        let f = self
            .type_facts
            .iter()
            .filter(|f| f.module == m && f.span.contains(line, col))
            .min_by_key(|f| f.span.width())?;
        Some((f.name.clone(), f.ty.clone(), f.doc.clone()))
    }
}
