//! The LSP/workspace layer over [`Compiler`]: incremental recompilation and
//! the reference-graph queries answered from it.
//!
//! The rule the whole file exists to keep: an index must never outlive the
//! arena that minted it. [`Compiler::reset_to`] destructures [`Watermark`]
//! exhaustively so a new arena cannot join the snapshot without its rewind
//! being written.
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use super::compiler::{CompileResult, Compiler, new_compiler};
use crate::ast;
use crate::module::{self, ModulePath};
use crate::reference::{ModuleId, ModuleReferences, ReferenceGraph, ReferenceGraphBuilder};
use crate::span::Span;
use crate::tivec::Idx;
use crate::type_def::{Type, TypeId};
use crate::types::{EnginePoolWatermark, EnvWatermark, Ty, ValueKind};

/// One buffered name occurrence, holding the *live* `Ty`: resolution is
/// deferred until all unifications have settled.
#[derive(Debug, Clone)]
pub(super) struct RawRef {
    pub(super) span: Span,
    pub(super) name: String,
    pub(super) ty: Ty,
    pub(super) doc: Option<String>,
    /// Interned at `record` time so `finalize_references` need not re-intern
    /// the path per occurrence.
    pub(super) module: ModuleId,
}

/// One name a module offers its importers, as a name list (REPL completion)
/// wants it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Export {
    pub name: String,
    /// A function's parameter names, or a constructor's field labels, in
    /// order. Empty for anything else.
    pub params: Vec<String>,
}

/// Resolved type at one occurrence span. The reference graph is identity-only,
/// so `IncrementalSession::hover` joins the inferred type from here.
#[derive(Debug, Clone)]
pub struct HoverFact {
    module: ModuleId,
    span: Span,
    name: String,
    ty: Type,
    doc: Option<String>,
}

/// Snapshot of every append-only compiler structure, captured at module
/// boundaries so an `IncrementalSession` can roll back to that point.
///
/// Pick between two watermarks with [`earlier`](Self::earlier) /
/// [`later`](Self::later), never `Ord::min`/`max` — those drop an `env`
/// payload on a tie. Adding a field obliges you to rewind it in
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
    /// Comparison key. Every field is an append-only pool length or a monotone
    /// counter, so an earlier watermark compares `<=` a later one. `env` is
    /// excluded: it is a rollback payload, not a position, so its field set can
    /// change without perturbing this ordering. Equal keys can therefore hide
    /// different env payloads, which is why `earlier`/`later` merge on ties.
    fn ord_key(&self) -> (EnginePoolWatermark, usize, usize, i32) {
        // Exhaustive destructure: a new field must be consciously placed in or
        // out of the ordering.
        let Watermark {
            engine,
            env: _,
            functions,
            constants,
            local_count,
        } = *self;
        (engine, functions, constants, local_count)
    }

    /// The earlier-compiled of two watermarks, order-independently. Use this,
    /// not `Ord::min`: on an `ord_key` tie `min` picks by argument order and
    /// silently discards one env payload, which can under-truncate the env and
    /// leave stale entries. This keeps the field-wise deeper rollback instead.
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

    /// `earlier`'s mirror, keeping the field-wise shallower env on a tie. Used
    /// to clamp a rewind to the seed floor: truncating the env past the seed's
    /// payload would dangle stdlib bindings whose pools survived.
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

/// Field-wise `min` of two env rollback payloads: the deeper rollback in
/// every dimension, which is the conservative direction for
/// `TypeEnv::truncate_to`. Written out field by field so a new `EnvWatermark`
/// field fails to compile until its merge direction is chosen.
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

/// [`env_field_min`]'s mirror: the shallower rollback in every dimension.
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
            local_count: self.local_count,
        }
    }

    /// Roll every pool/map back to `w` and clear per-compile transient state.
    /// `module_table` is left untouched: the caller decides which cached
    /// modules survive via `ModuleTable::invalidate`.
    ///
    /// Every structure surviving the rewind must be truncated to `w`, filtered
    /// to in-bounds indices, or cleared, so no index outlives its arena. The
    /// exhaustive destructure below makes a new snapshot field fail to compile
    /// until its rewind is written.
    ///
    /// `w` must not be below the session's `seed`, because everything below it
    /// is the stdlib itself. The prelude-as-entry teardown to `bare` is the one
    /// deliberate exception, and it rebuilds the session afterwards.
    /// `IncrementalSession::rewind_to` is the clamp.
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
        // A throwaway scope for this compile's imports. The root scope rewinds
        // by length, which cannot undo an in-place `define` overwrite, so an
        // import that shadows a prelude name must never touch the root scope.
        self.env.push_scope();
        self.fns.truncate(functions);
        self.consts.truncate(constants);
        self.local_count = local_count;
        self.global_to_func.retain(|_, fi| fi.index() < functions);
        // Survivors are watermark-preserved entry-frame slots. Depth normalises
        // to 0 so the next opened scope treats them as inherited. The `StrId`
        // key must also survive the strings truncation: an aliased selective
        // import can bind a post-watermark key to a pre-watermark slot, and a
        // dangling key would collide with whatever re-interns at that index.
        self.locals.retain(|&k, v| {
            if v.slot < local_count && k.idx() < engine.strings {
                v.depth = 0;
                true
            } else {
                false
            }
        });

        // Lowered toplevels are dropped, never truncated: they are recorded
        // at the end of a module's compile, and a rewound compile's init code
        // must not run as part of the next one.
        self.inits.clear();
        self.entry_toplevel = None;
        self.main = None;

        // Recorded expression types hold `Ty` indices into the arena just
        // rewound; the next compile's typecheck walk re-records them all.
        self.walk_tys.clear();
        self.walk_tys_stack.clear();
        // A `ClosureSite` holds a `func_idx` and `StrId`s into pools truncated
        // above.
        self.frame_closures.clear();
        // `toplevel_binds` is positional: a survivor would hand the next
        // module-scope bind someone else's slot, silently.
        self.toplevel_decls.clear();
        self.toplevel_binds.clear();
        self.walking_module_statements = false;

        self.undo_log.clear();
        self.scope_marks.clear();
        self.unused.clear();
        // Queued entries hold `Ty` indices into the arena just rewound.
        self.nil_discards.clear();
        self.outer_scopes.clear();
        self.captures.clear();
        self.capture_names.clear();
        self.current_binding = None;
        self.next_fn_self_name = None;
        self.current_owner = None;
        self.rigid_ids.clear();
        self.recorded.clear();
        // `ref_interner` is deliberately not cleared, so `DefId`s in surviving
        // `CachedModule.module_refs` keep resolving to stable `ModuleId`s.
        let main_id = self.ref_interner.intern(&module::main_module());
        self.module_refs = ModuleReferences::new(main_id);
        self.imported_qualifiers.clear();
        self.current_module = module::main_module();
        self.current_module_key = module::ModuleKey::main();
        self.module_path_slice = None;
        // `str_slices` was just rewound, so an `ArenaSlice` may now denote a
        // different path than it did last compile.
        self.defid_module_memo.clear();
        self.module_table.unmark_all_loading();
        self.reset_module_frames();
    }

    /// Materialise a `CompileResult` by cloning, not taking, so the session can
    /// be reused. Rebuilds the workspace reference graph and the hover table.
    fn snapshot_result(&mut self) -> (CompileResult, Vec<HoverFact>) {
        let (references, facts) = self.finalize_references();
        (
            // A check-only session emits no program: the LSP reads only
            // diagnostics and the graph.
            CompileResult::analysis_only(self.engine.diagnostics.clone(), references),
            facts,
        )
    }

    /// Build the workspace [`ReferenceGraph`] from the entry file's collector
    /// and every `CachedModule`'s persisted `module_refs`. Built wholesale each
    /// `check` so an evicted module's reverse edges vanish coherently.
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

        // Mirror the persistent interner's id assignment: interning in id order
        // reproduces identical ids, so the graph's `ModuleId`s match the ones
        // already baked into every `DefId`. Skipping one would renumber every
        // later module.
        let mut graph = ReferenceGraphBuilder::new();
        for p in self.ref_interner.paths() {
            graph.intern_module(p);
        }

        // Every cached module's references. The reverse index is built once by
        // `finish()`, not per insert.
        for (_key, cm) in self.module_table.loaded_modules() {
            graph.insert(Rc::clone(cm.module_refs()));
        }

        // The entry file's own refs must be copied: the collector is reused for
        // the next check.
        graph.insert(Rc::new(self.module_refs.clone()));

        graph.finish()
    }

    /// Build the workspace [`ReferenceGraph`] and resolve the buffered
    /// occurrences into the [`HoverFact`] table.
    pub(super) fn finalize_references(&mut self) -> (Rc<ReferenceGraph>, Vec<HoverFact>) {
        let graph = self.build_reference_graph();
        // Only the LSP consumes hover facts; off that path `record` buffered
        // nothing, so skip the O(occurrences) resolve pass.
        if !self.collect_hover_facts {
            return (Rc::new(graph), Vec::new());
        }
        let raw = std::mem::take(&mut self.recorded);
        let mut facts: Vec<HoverFact> = Vec::with_capacity(raw.len());
        // Finalization never unifies, so `resolve` is a pure function of the
        // union-find representative and can be memoised on it. Many occurrences
        // share a canonical `Ty`, and each raw resolve appends arena nodes.
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

/// A reusable, check-only compiler for the LSP. Holds the seeded stdlib and a
/// cache of compiled user modules; each `check()` re-hashes cached module
/// sources, invalidates the changed ones and everything compiled after them,
/// truncates the arena to the surviving boundary, and recompiles the rest.
pub struct IncrementalSession {
    c: Compiler,
    seed: Watermark,
    /// Watermark before anything at all was seeded — the floor a
    /// prelude-as-entry check rewinds to, since the prelude cannot be checked
    /// on top of itself.
    bare: Watermark,
    /// Whether the prelude seed is currently in place. A prelude-as-entry
    /// check tears it down (`bare` rewind); the next ordinary check re-seeds.
    seeded: bool,
    /// Watermark immediately before the previous entry-body analysis, i.e.
    /// after every imported module had been compiled.
    last_entry: Option<Watermark>,
    /// Workspace reference graph, rebuilt wholesale at the end of every `check`
    /// so an invalidated module's reverse edges disappear coherently. Shared
    /// with the `CompileResult` handed back from `check`.
    graph: Rc<ReferenceGraph>,
    /// Resolved type per recorded occurrence, from the last `check`.
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

    /// A session that compiles the stdlib from the `.scrl` sources under
    /// `stdlib_root` (the in-repo `src/std`) instead of the embedded copy.
    /// Used when editing the stdlib itself: every `scarlet/...` module is then
    /// an ordinary on-disk `File` module — compiled, cached, hashed and
    /// invalidated exactly like user code — so the reference graph and hover
    /// facts carry full fidelity for stdlib sources.
    pub fn new_from_source(stdlib_root: std::path::PathBuf) -> Self {
        Self::with_stdlib_root(Some(stdlib_root))
    }

    fn with_stdlib_root(stdlib_root: Option<std::path::PathBuf>) -> Self {
        let mut c = new_compiler(None, true);
        c.collect_hover_facts = true;
        c.stdlib_source_root = stdlib_root;
        let bare = c.watermark();
        c.register_prelude();
        let seed = c.watermark();
        IncrementalSession {
            c,
            seed,
            bare,
            seeded: true,
            last_entry: None,
            graph: Rc::new(ReferenceGraph::new()),
            type_facts: Vec::new(),
        }
    }

    /// Check entries the way a REPL submits them: a script of statements,
    /// one fragment at a time, so module-scope statements are the point and
    /// a binding whose use has not been typed yet is not an error. See
    /// [`ModuleScope`](crate::bytecode::ModuleScope) and
    /// [`UnusedBindings`](crate::bytecode::UnusedBindings).
    pub fn as_repl(&mut self) {
        self.c.unused_bindings = crate::bytecode::UnusedBindings::Ignore;
        self.c.module_scope = crate::bytecode::ModuleScope::Script;
    }

    pub fn compile_count(&self) -> u32 {
        self.c.module_table.compile_count()
    }

    /// The one rewind path. `seed` is a hard floor: everything below it is the
    /// stdlib, compiled when the session started, and rewinding past it would
    /// drop the stdlib too. Clamping here rather than at each caller means a
    /// new rewind site cannot forget.
    fn rewind_to(&mut self, w: Watermark) {
        self.c.reset_to(&w.later(self.seed));
    }

    pub fn reference_graph(&self) -> &ReferenceGraph {
        self.graph.as_ref()
    }

    /// The canonical module path a cached user module was loaded under, found
    /// by its on-disk source path. Analysing the same file as the open entry
    /// would key it under `main` instead, so the LSP needs this to make a query
    /// inside an imported file resolve to the `DefId` its callers point at.
    pub fn module_path_for_source(&self, path: &Path) -> Option<&ModulePath> {
        self.c
            .module_table
            .user_modules()
            .find(|(_, cm)| cm.source_path() == Some(path))
            .map(|(_, cm)| &cm.iface.path)
    }

    /// Evict the cached module compiled from `path` and its dependents so the
    /// next `check()` recompiles them. Called from LSP `didChangeWatchedFiles`
    /// when a file changes on disk outside the editor.
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

    /// [`check`](Self::check), analysing the buffer *as* the given module
    /// rather than as `main`. Used by a from-source stdlib session so an open
    /// `src/std/**/*.scrl` file keeps its canonical `scarlet/...` identity: `@vm` is
    /// legal, prelude-redefinition rules apply, and every def/occurrence/hover
    /// fact is keyed under the module its importers reference.
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
        // Rewrite backpass sugar away before any typing pass, mirroring
        // `compile_impl`.
        let mut expr = expr.clone();
        crate::desugar::desugar_expression(&mut expr);
        let expr = &expr;

        // The prelude cannot be analysed on top of itself: tear the seed down
        // to `bare` and compile the buffer as the whole world. The next
        // ordinary check re-seeds below.
        let prelude_self = entry_module == module::scarlet_prelude();
        if prelude_self {
            self.c.module_table.invalidate_all();
            self.c.reset_to(&self.bare);
            self.seeded = false;
            self.last_entry = None;
        } else if !self.seeded {
            // A previous prelude-as-entry check tore the seed down. Rebuild
            // the session outright — byte-for-byte the fresh-session state,
            // with none of the partially-rewound world to reason about. Rare
            // (only after editing `scrl.scrl` and switching file), so the full
            // stdlib recompile it implies is acceptable.
            *self = Self::with_stdlib_root(self.c.stdlib_source_root.clone());
        }

        // The previous entry's contributions are dropped; cached modules' arena
        // state sits below this line and survives.
        let mut floor = self.last_entry.unwrap_or(self.seed);

        // Keys are collected first so the staleness scan can take
        // `&mut module_table` for its stat cache without overlapping the
        // `user_modules()` borrow. Modules baked at or below the seed (the
        // from-source prelude) are excluded: they cannot be rewound past, so a
        // change there is handled by the owner dropping the session instead.
        let seed = self.seed;
        let candidates: Vec<module::ModuleKey> = self
            .c
            .module_table
            .user_modules()
            .filter(|(_, cm)| cm.watermark().is_none_or(|w| w >= seed))
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
        self.c.base_dir = base_dir.map(|p| p.to_path_buf());

        // Entry identity. Set on every check — never left over from the
        // previous one — and the entry's reference collector is re-keyed to
        // match, so its defs land under the module id importers target.
        self.c.current_module = entry_module.clone();
        self.c.current_module_key = if prelude_self || module::is_stdlib(&entry_module) {
            module::ModuleKey::for_stdlib(&entry_module)
        } else {
            module::ModuleKey::main()
        };
        self.c.module_path_slice = None;
        let entry_mid = self.c.ref_interner.intern(&entry_module);
        self.c.module_refs = ModuleReferences::new(entry_mid);

        self.compile_entry(expr);

        // Overflow fallback: a recompiled module spilled past its reused id
        // range and may have collided with a sibling's block. Evict everything,
        // drop the id bases, and recompile once. Every module is now a fresh
        // allocation sized to current usage, so the flag cannot be re-raised
        // this pass — hence one pass, not a loop.
        if self.c.module_table.id_range_overflow()
            && let Some(w) = self.c.module_table.invalidate_all()
        {
            self.c.module_table.reset_id_bases();
            self.rewind_to(w);
            self.last_entry = None;
            self.compile_entry(expr);
        }
        let (result, facts) = self.c.snapshot_result();
        self.graph = result.references.clone();
        self.type_facts = facts;
        result
    }

    /// Compile the entry expression and capture its `last_entry` watermark.
    fn compile_entry(&mut self, expr: &ast::Expression) {
        if let ast::Expression::BlockExpression(block) = expr {
            // `env.type_info` is a flat map, not a scope stack, so a selective
            // `import m.{Type}` is not confined to the throwaway scope the way a
            // value binding is. Capture the env position before the imports run
            // so `last_entry` excludes them; otherwise a removed or renamed type
            // import keeps resolving to a stale `TypeInfo` with no diagnostic.
            // The journal position rolls back with it, which is what restores a
            // stdlib type the entry shadowed.
            let pre_import = self.c.env.watermark();
            self.c.process_imports(block);
            // The imports left their own module-scope binds on these positional
            // per-compile channels; a shadowing entry-file bind must not dequeue
            // an import's slot.
            self.c.toplevel_binds.clear();
            self.c.toplevel_decls.clear();
            // Must precede the watermark capture below.
            self.c.bump_type_ids_past_reserved();
            let mut wm = self.c.watermark();
            wm.env.type_info = pre_import.type_info;
            wm.env.journal = pre_import.journal;
            self.last_entry = Some(wm);
            self.c.env.push_scope();
            self.c.analyse_module(block, None);
            self.c.env.pop_scope();
        } else {
            self.last_entry = Some(self.c.watermark());
            // A bare-expression entry can still contain lambdas, and no body may
            // lower or emit during the typecheck walk. `analyse_module` brackets
            // its own walk; this path must bracket this one.
            self.c.begin_deferred_elaboration();
            self.c.compile_expr(expr);
            self.c.end_deferred_elaboration();
        }
    }

    /// Resolve a module key to its interned `ModuleId`; `None` means the entry
    /// (`main`) module. A key naming no interned module must resolve to `None`,
    /// never fall back to the entry, or a stale URI would answer queries with
    /// another file's facts.
    fn module_for(&self, module_key: Option<&module::ModuleKey>) -> Option<ModuleId> {
        match module_key {
            Some(key) => self.graph.module_id_by_key(key),
            None => self.graph.module_id_by_key(&module::ModuleKey::main()),
        }
    }

    /// The base of the type-id range reserved for module `key`, if one was
    /// allocated. Used by the incremental test harness to assert range reuse.
    pub fn module_id_base(&self, key: &module::ModuleKey) -> Option<TypeId> {
        self.c.module_table.id_base_of(key)
    }

    /// Name, inferred type and doc at a position. The tightest fact containing
    /// the cursor wins, mirroring `resolve_position`, so a nested sub-expr's
    /// type beats an enclosing one rather than whichever was recorded first.
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

    /// What the module at `path` exports, types first. A module no check has
    /// imported yet is compiled first, by checking an entry that imports it,
    /// so it lands in the cache through the same door as any other import.
    /// One that does not resolve or does not compile exports nothing.
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
        // One name, one entry: a record type names its single constructor
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

    /// Off a tie, `earlier`/`later` follow `Ord` and keep the winner's env.
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
    /// watermark, and drops every lowered toplevel: a rewound compile's init
    /// code must not run as part of the next one.
    #[test]
    fn reset_to_rewinds_functions_and_consts_and_drops_toplevels() {
        let mut c = new_compiler(None, false);
        let pool = Rc::new(crate::typed_ir::ResolvedPool::new(PrimIds::default()));
        let top = LoweredFn {
            module: crate::module::ModuleKey::main(),
            name: "m".to_string(),
            core: CoreFn {
                name: c.engine.intern("m"),
                params: Vec::new(),
                body: CoreExpr::Tail(Atom::Nil),
                ret_ty: RTy(0),
            },
            pool,
        };

        // Anchored to whatever `new_compiler` seeded, not a literal, so the
        // test survives that seed growing.
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
