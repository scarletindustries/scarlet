//! The LSP/workspace layer over [`Compiler`]: incremental recompilation and
//! the reference-graph queries answered from it.
//!
//! The rule the whole file exists to keep: an index must never outlive the
//! arena that minted it. [`Compiler::reset_to`] destructures [`Watermark`]
//! exhaustively so a new arena cannot join the snapshot without its rewind
//! being written.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use super::compiler::{CompileResult, Compiler, new_compiler};
use crate::ast;
use crate::module::{self, ModulePath};
use crate::reference::{
    Definition, DefinitionKind, EntityKind, ModuleId, ModuleReferences, ReferenceGraph,
    ReferenceGraphBuilder,
};
use crate::span::Span;
use crate::tivec::Idx;
use crate::type_def::{Type, TypeId};
use crate::types::{DefinitionLocation, EnginePoolWatermark, EnvWatermark, Ty};

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

/// Resolved type at one occurrence span. The reference graph is identity-only,
/// so `IncrementalSession::hover` joins the inferred type from here.
#[derive(Debug, Clone)]
pub struct HoverFact {
    pub module: ModuleId,
    pub span: Span,
    pub name: String,
    pub ty: Type,
    pub doc: Option<String>,
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
    pub engine: EnginePoolWatermark,
    pub env: EnvWatermark,
    pub code: usize,
    pub functions: usize,
    pub constants: usize,
    pub local_count: i32,
}

impl Watermark {
    /// Comparison key. Every field is an append-only pool length or a monotone
    /// counter, so an earlier watermark compares `<=` a later one. `env` is
    /// excluded: it is a rollback payload, not a position, so its field set can
    /// change without perturbing this ordering. Equal keys can therefore hide
    /// different env payloads, which is why `earlier`/`later` merge on ties.
    fn ord_key(&self) -> (EnginePoolWatermark, usize, usize, usize, i32) {
        // Exhaustive destructure: a new field must be consciously placed in or
        // out of the ordering.
        let Watermark {
            engine,
            env: _,
            code,
            functions,
            constants,
            local_count,
        } = *self;
        (engine, code, functions, constants, local_count)
    }

    /// The earlier-compiled of two watermarks, order-independently. Use this,
    /// not `Ord::min`: on an `ord_key` tie `min` picks by argument order and
    /// silently discards one env payload, which can under-truncate the env and
    /// leave stale entries. This keeps the field-wise deeper rollback instead.
    pub fn earlier(self, other: Self) -> Self {
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
    pub fn later(self, other: Self) -> Self {
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

/// One definition synthesised for a static/hydrated stdlib module from its
/// interface's exported values.
struct SynthDef {
    name: String,
    location: DefinitionLocation,
    doc: Option<String>,
    /// Function parameter names / constructor field labels, for hover.
    param_names: Vec<String>,
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

    /// Roll every pool/map back to `w` and clear per-compile transient state.
    /// `module_table` is left untouched: the caller decides which cached
    /// modules survive via `ModuleTable::invalidate`.
    ///
    /// Every structure surviving the rewind must be truncated to `w`, filtered
    /// to in-bounds indices, or cleared, so no index outlives its arena. The
    /// exhaustive destructure below makes a new snapshot field fail to compile
    /// until its rewind is written.
    ///
    /// `w` must not be below the watermark captured right after `seed_static`:
    /// that prefix is memcpy'd out of the stdlib blob and every `Ty`/`StrId`/
    /// `ArenaSlice` frozen into `.rodata` indexes into it.
    /// `IncrementalSession::rewind_to` is the clamp.
    pub fn reset_to(&mut self, w: &Watermark) {
        let Watermark {
            engine,
            env,
            code,
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
        self.program.code.truncate(code);
        self.program.functions.truncate(functions);
        self.program.constants.truncate(constants);
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

        // Lowered Core IR is cleared, never truncated. A `CoreFn`'s `ConstId`s
        // index `core.consts`, which is assigned wholesale rather than appended
        // to, and its types index a `ResolvedPool` the elaborator builds fresh
        // per compile and drops after emit. Neither has a length to rewind to,
        // so no `core_fns` watermark would mean anything.
        let crate::core_ir::CoreProgram {
            fns,
            consts,
            toplevel,
        } = &mut self.core;
        fns.clear();
        consts.clear();
        *toplevel = crate::core_ir::CoreProgram::default().toplevel;

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
    }

    /// Materialise a `CompileResult` by cloning, not taking, so the session can
    /// be reused. Rebuilds the workspace reference graph and the hover table.
    fn snapshot_result(&mut self) -> (CompileResult, Vec<HoverFact>) {
        let (references, facts) = self.finalize_references();
        (
            CompileResult {
                // A check-only session emits no program: the LSP reads only
                // diagnostics and the graph, and cloning the hydrated stdlib
                // `Program` per keystroke would be pure waste.
                emitted: None,
                diagnostics: self.engine.diagnostics.clone(),
                references,
            },
            facts,
        )
    }

    /// Synthesise reference-graph [`Definition`]s for a static/hydrated stdlib
    /// module from its exported values' `Scheme.def`, which already carries the
    /// real declaration span. `None` for a value-less interface.
    ///
    /// Every `DefId` and the owning container go through
    /// [`Compiler::defid_of`], the same computation a populated use of the name
    /// bakes into its occurrence target, so both share one canonical `DefId`
    /// even when the precompiled `Scheme.def.module` spelling differs from the
    /// `ModuleTable` key. Types are not synthesised: `TypeInfo` has no
    /// declaration span.
    fn synth_refs_from_interface(
        &mut self,
        defs: &[SynthDef],
        doc: Option<&str>,
    ) -> Option<ModuleReferences> {
        let mid = self.defid_of(defs.first()?.location).module;
        let mut mr = ModuleReferences::new(mid);
        mr.set_doc(doc.map(str::to_string));
        for sd in defs {
            let defid = self.defid_of(sd.location);
            // A constructor's declaring-type `DefId` is not serialised, so its
            // `ctor_of` edge is absent. Harmless: the dead-code pass that walks
            // it only reports the entry module.
            let kind = match defid.entity {
                EntityKind::Function => DefinitionKind::Function {
                    param_names: sd.param_names.clone(),
                },
                EntityKind::Constructor => DefinitionKind::Constructor {
                    ctor_of: None,
                    param_names: sd.param_names.clone(),
                },
                EntityKind::Constant => DefinitionKind::Constant,
                EntityKind::Value => DefinitionKind::Value { alias_of: None },
                EntityKind::Type => DefinitionKind::Type,
                EntityKind::Field => DefinitionKind::Field,
                EntityKind::ModuleAlias => DefinitionKind::ModuleAlias {
                    decl_span: defid.span,
                    imports_module: None,
                },
            };
            mr.add_definition(Definition::new(
                defid.module,
                defid.span,
                sd.name.clone(),
                sd.doc.clone(),
                true,
                kind,
            ));
        }
        Some(mr)
    }

    /// Build the workspace [`ReferenceGraph`] from the entry file's collector,
    /// every from-source `CachedModule`'s persisted `module_refs`, and
    /// definitions synthesised from hydrated stdlib interfaces. Built wholesale
    /// each `check` so an evicted module's reverse edges vanish coherently.
    fn build_reference_graph(&mut self) -> ReferenceGraph {
        // Intern every loaded module path plus main, so a synthesised stdlib
        // def gets a stable `ModuleId`.
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

        // Every cached module's references; hydrated stdlib modules carry none,
        // so their definitions are synthesised from the interface below. The
        // reverse index is built once by `finish()`, not per insert.
        let mut synth_inputs: Vec<(Vec<SynthDef>, Option<String>)> = Vec::new();
        for (_key, cm) in self.module_table.loaded_modules() {
            match cm.module_refs() {
                Some(mr) => graph.insert(Rc::clone(mr)),
                None => {
                    let defs: Vec<SynthDef> = cm
                        .iface
                        .values
                        .iter()
                        .filter_map(|(name, ev)| {
                            ev.scheme.def.map(|dl| SynthDef {
                                name: name.clone(),
                                location: dl,
                                doc: ev.doc.clone(),
                                param_names: ev.param_names.clone(),
                            })
                        })
                        .collect();
                    if !defs.is_empty() {
                        synth_inputs.push((defs, cm.iface.doc.clone()));
                    }
                }
            }
        }
        // Deferred out of the loop above: `defid_of` takes `&mut self`, which
        // cannot overlap the `module_table` iteration.
        for (defs, doc) in &synth_inputs {
            if let Some(synth) = self.synth_refs_from_interface(defs, doc.as_deref()) {
                graph.insert(Rc::new(synth));
            }
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

impl IncrementalSession {
    pub fn new(stdlib: &'static crate::static_ir::StaticStdlib) -> Self {
        let mut c = new_compiler(None, true);
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

    /// The one rewind path. `seed` is a hard floor: everything below it is the
    /// stdlib blob memcpy'd out of `.rodata`, and the `Ty`/`StrId`/`ArenaSlice`
    /// indices frozen into the binary cannot be re-minted. Clamping here rather
    /// than at each caller means a new rewind site cannot forget.
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
        // The previous entry's contributions are dropped; cached modules' arena
        // state sits below this line and survives.
        let mut floor = self.last_entry.unwrap_or(self.seed);

        // Keys are collected first so the staleness scan can take
        // `&mut module_table` for its stat cache without overlapping the
        // `user_modules()` borrow.
        let candidates: Vec<module::ModuleKey> = self
            .c
            .module_table
            .user_modules()
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
        self.rewind_to(floor);
        self.c.base_dir = base_dir.map(|p| p.to_path_buf());

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
}

#[cfg(test)]
mod tests {
    use super::Watermark;
    use crate::bytecode::Value;
    use crate::bytecode::compiler::new_compiler;
    use crate::core_ir::{Atom, ConstId, CoreExpr, CoreFn};
    use crate::type_def::TypeId;
    use crate::typed_ir::RTy;
    use crate::types::EnvWatermark;

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
            code: 10,
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

    /// The resolved-type pool is compile-local, so a `CoreFn` cannot outlive
    /// the compile that lowered it: `reset_to` must clear `core.fns` outright
    /// rather than truncate it, and `Watermark` carries no field for it.
    #[test]
    fn reset_to_clears_lowered_core_fns_because_the_pool_is_compile_local() {
        let mut c = new_compiler(None, false);
        let name = c.engine.intern("f");
        let lowered = |name| CoreFn {
            name,
            params: Vec::new(),
            body: CoreExpr::Tail(Atom::Const(ConstId(0))),
            ret_ty: RTy(0),
        };

        // Lowered below the watermark: a length-based rewind would preserve it.
        c.core.fns.push(lowered(name));
        let w = c.watermark();
        c.core.fns.push(lowered(name));

        c.reset_to(&w);

        assert!(
            c.core.fns.is_empty(),
            "core.fns must be cleared, not truncated: {} lowered bodies survived \
             the rewind holding RTys into a pool that no longer exists",
            c.core.fns.len()
        );
    }

    /// `core.consts` is assigned wholesale, not appended to, so it has no
    /// watermark. Rewinding it to another pool's length would leave a stale
    /// prefix no surviving `ConstId` was minted against.
    #[test]
    fn reset_to_clears_core_consts_rather_than_truncating_to_another_pool() {
        let mut c = new_compiler(None, false);

        // Anchored to whatever `new_compiler` seeded, not a literal, so the
        // test survives that seed growing.
        let base = c.program.constants.len();
        c.program.constants.push(Value::bool(true));
        let w = c.watermark();
        assert_eq!(w.constants, base + 1);

        // A compile then grows `program.constants` and clones it wholesale.
        c.program.constants.push(Value::bool(false));
        c.program.constants.push(Value::nil());
        c.core.consts = c.program.constants.clone();

        c.reset_to(&w);

        assert_eq!(
            c.program.constants.len(),
            base + 1,
            "program.constants rewinds to its own watermark"
        );
        assert!(
            c.core.consts.is_empty(),
            "core.consts must be cleared, not truncated to program.constants.len() \
             ({} entries survived) — a stale prefix is indexed by no live ConstId",
            c.core.consts.len()
        );
    }
}
