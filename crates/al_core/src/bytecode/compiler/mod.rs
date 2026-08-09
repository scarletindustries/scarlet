//! AST → [`Program`]: Hindley-Milner type inference plus bytecode emission.
//!
//! A function body is typechecked by the [`Compiler::compile_expr`] walk, then
//! lowered to [`crate::core_ir`] ANF, run through Core→Core passes (Perceus
//! reuse, later mode inference) and emitted by `core_ir::emit`. Type erasure
//! happens exactly once, at Core→bytecode. See [`Compiler::compile_fn_body`]
//! and `docs/core-ir-spec.md`.
//!
//! Module top level is the exception: declarations are mutually recursive and
//! order-free, so `analysis.rs` runs its multi-pass declaration analysis first
//! and hands each body back to this file. Pattern typechecking lives in
//! `patterns.rs`, the Core IR bridge impls in `bridges.rs`.
//!
//! # Invariants
//!
//! - Every constant `Value` is built through the `const_*` helpers, never a
//!   bare `Value` constructor, so all constants live in the program's frozen
//!   area and stay valid on any thread for the program's life.
//! - Everything a compile appends to stays append-only between module
//!   boundaries. That is what makes `Watermark` rollback pure truncation.
//! - Local scoping is undo-log based: popping a scope replays only the
//!   bindings it actually shadowed, never a map snapshot.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use super::peephole::fuse;
use super::session::{RawRef, Watermark};
use super::{Function, Op, PreludeBindings, Program, TypeRef, Value, op, op_arg};
use crate::ast;
use crate::core_ir::CoreFn;
use crate::diagnostic::{Diagnostic, DiagnosticCode, has_errors};
use crate::frozen::{FrozenBuilder, FrozenConst};
use crate::tivec::Idx;
use crate::typed_ir::slots::{SlotError, slot_labeled};
use crate::typed_ir::{
    self, CaptureIdx, Denotation, ElabCtx, FnTable, FrameSlot, GlobalSlot, OrShape, PreludeTys,
    RTy, ResolvedPool, TempTys, TypedExpr, TypedFn, TypedProgram, WalkStep, Zonker, pool_for,
};
use indexmap::IndexMap;
use smallvec::SmallVec;

use crate::module::{
    self, CachedModule, ModuleInterface, ModuleKey, ModuleOrigin, ModulePath, ModuleSource,
    ModuleTable, ResolveError, source_hash,
};
use crate::reference::{
    DefId, Definition, DefinitionKind, ModuleId, ModuleInterner, ModuleReferences, Reference,
    ReferenceGraph, ReferenceKind,
};
use crate::span::Span;
use crate::type_def::TypeId;
use crate::types::{
    AnnotationContext, ArenaSlice, Constraint, DefinitionLocation, EntityKind, Hydrator,
    InferEngine, MatchFunTypeError, NullaryPrim, Pat, PatternBindings, PatternSink, Scheme, StrId,
    Ty, TypeEnv, TypeInfo, TypeNode, UsefulnessMatrix, ValueKind, mono, new_engine, new_env, pool,
};

mod bridges;
mod patterns;
#[cfg(test)]
mod tests;

/// The proof the elaborator demands before it will run. Its own module so
/// that the private field is unforgeable from the rest of this module.
mod clean {
    use crate::diagnostic::{Diagnostic, has_errors};

    /// Proof, as of the moment it was minted, that the module being compiled
    /// has produced no error diagnostic.
    ///
    /// No pass past the typecheck walk has a poison arm, so rather than teach
    /// each one to recognise poison the pipeline is made unreachable from a
    /// poisoned module at its first gate. `typed_ir::elaborate_body`/
    /// `elaborate_toplevel` are the only constructors of a
    /// [`TypedFn`](crate::typed_ir::TypedFn), hence of the `TypedProgram`
    /// `lower` consumes, and
    /// [`Compiler::elaborate_then_materialize`](super::Compiler::elaborate_then_materialize)
    /// is their only caller here. It consumes a `CleanModule`, and
    /// [`CleanModule::mint`] is the only way to make one.
    ///
    /// Neither `Copy` nor `Clone`, and taken by value: an elaboration can
    /// itself append diagnostics, so each one re-proves this.
    #[must_use]
    pub(super) struct CleanModule {
        _priv: (),
    }

    impl CleanModule {
        /// `Some` iff `diagnostics` carries no error — the same predicate
        /// `CompileResult::success` reports.
        pub(super) fn mint(diagnostics: &[Diagnostic]) -> Option<Self> {
            (!has_errors(diagnostics)).then_some(Self { _priv: () })
        }
    }
}
use clean::CleanModule;

/// Why a nominal `.field` lookup failed.
enum FieldMismatch {
    /// Receiver's type body has no variants at all (alias, opaque, builtin).
    NotNominal,
    /// Variant list is empty, so no field can exist.
    NoVariants,
    MissingOn(StrId),
    PositionDiffers {
        variant: StrId,
        at: usize,
        expected: usize,
    },
}

/// The bytecode `Program` and the Core IR it came from. A `check` builds one
/// too — its function table is mode-independent, pinned by
/// `crates/al/tests/check_parity.rs` — but its bodies are never emitted.
#[derive(Debug)]
pub struct Emitted {
    pub program: Program,
    /// Lowered Core IR (typed ANF). Golden-snapshotted by `crates/al/tests/core_ir.rs`.
    pub core: crate::core_ir::CoreProgram,
}

#[derive(Debug)]
pub struct CompileResult {
    /// `None` when no program was built at all: the incremental (LSP) check
    /// path, or a compile whose stdlib seed failed. Consumers that run or
    /// disassemble must unwrap the absence explicitly.
    pub emitted: Option<Emitted>,
    pub diagnostics: Vec<Diagnostic>,
    /// Workspace reference graph: the one source of truth for goto-def,
    /// find-refs, rename, symbols and dead-code. Shared so the owning
    /// `IncrementalSession` can keep querying it after handing this back.
    pub references: Rc<ReferenceGraph>,
}

impl CompileResult {
    /// Whether the compile succeeded: whether `diagnostics` carries no error.
    /// Derived, not stored, so no construction site can disagree with it.
    pub fn success(&self) -> bool {
        !has_errors(&self.diagnostics)
    }
}

/// An enclosing function frame's locals, moved here whole by `enter_fn_frame`
/// and moved back by `finish_fn_frame`, so a frame push/pop is O(1).
#[derive(Clone)]
pub(super) struct Scope {
    locals: HashMap<StrId, LocalSlot>,
}

/// A live local binding: its stack slot, and the block-scope depth it was bound
/// at. A rebinding at a strictly shallower depth is inherited from an enclosing
/// scope, so shadowing must allocate a fresh slot to keep the outer value.
#[derive(Clone, Copy, Debug)]
pub(super) struct LocalSlot {
    pub(super) slot: i32,
    pub(super) depth: u32,
    /// This name lives in the entry frame, so a nested body loads it with
    /// `PushGlobal <slot>` instead of capturing it. Decided once at bind time
    /// by [`Compiler::binds_a_global`]; `resolve_variable` must read this
    /// rather than re-derive it from `depth`.
}

/// One top-level `fn`/`const` declaration of the module being compiled, as the
/// toplevel elaboration needs it: where it sits in the module block, and which
/// entry-frame slot the check walk gave it. Nothing maps the name to a slot.
#[derive(Clone, Copy, Debug)]
pub(super) struct ToplevelDecl {
    /// Index of the declaration's node in the module block's `body`.
    pub(super) node: usize,
    /// Interned declaration name. Read only to resolve a forward reference to
    /// this decl from inside the toplevel spine; the answer is its `slot`.
    pub(super) name: StrId,
    /// The entry-frame slot Pass 3 allocated for the declaration.
    pub(super) slot: GlobalSlot,
}

/// Compiler state snapshotted on entry to a nested function body and restored
/// by `finish_fn_frame` once its bytecode and closure have been emitted.
struct FnFrame {
    /// `undo_log`/`scope_marks` lengths at frame entry. `locals` is restored
    /// wholesale from `outer_scopes`, so the inner frame's undo entries must be
    /// discarded here, never replayed against the parent map.
    undo_base: usize,
    marks_base: usize,
    local_count: i32,
    captures: HashMap<StrId, i32>,
    capture_names: Vec<StrId>,
    rigid_ids: HashSet<i32>,
    binding: Option<StrId>,
    jump_over: i32,
    /// The enclosing frame's [`Compiler::frame_closures`], parked for the
    /// duration of the inner walk so the inner frame accumulates only its own.
    closures: Vec<ClosureSite>,
}

/// Per-module compiler state parked by `enter_module_frame` and restored by
/// `leave_module_frame`.
struct ModuleFrame {
    module: ModulePath,
    module_key: ModuleKey,
    module_path_slice: Option<ArenaSlice<pool::StrSlices>>,
    imported_qualifiers: HashMap<String, ModuleKey>,
    base_dir: Option<PathBuf>,
    module_refs: ModuleReferences,
}

/// Compiler frame state parked by `enter_elab_frame` while a [`DeferredBody`]'s
/// snapshot is swapped in for its elaboration, restored by `leave_elab_frame`.
struct ElabFrame {
    outer_scopes: Vec<Scope>,
    locals: HashMap<StrId, LocalSlot>,
    captures: HashMap<StrId, i32>,
    capture_names: Vec<StrId>,
    rigid_ids: HashSet<i32>,
    current_binding: Option<StrId>,
    frame_closures: Vec<ClosureSite>,
}

pub struct Compiler {
    pub(super) program: Program,
    /// Append handle to `program`'s frozen area. Every constant `Value` is
    /// built through this (the `const_*` helpers), never a bare `Value`
    /// constructor, so enum names and field labels share the area's canonical
    /// interned allocations.
    frozen: FrozenBuilder,
    /// `Value::to_bits` → constant-pool index. `frozen` interns heap constants
    /// and immediates encode by value, so equal constants have equal bits.
    /// Lookups re-validate against the live pool, so `reset_to`'s truncate
    /// needs no paired invalidation — a stale entry just misses.
    const_dedup: HashMap<u64, i32>,
    pub(super) locals: HashMap<StrId, LocalSlot>,
    /// Scoped-symbol-table undo log. Every mutation of `locals` inside an open
    /// block scope appends `(name, previous entry)`; `pop_local_scope` unwinds
    /// back to the entering scope's mark. Costs O(bindings actually shadowed)
    /// rather than a full `locals` snapshot per scope.
    pub(super) undo_log: Vec<(StrId, Option<LocalSlot>)>,
    /// `undo_log` length captured at each `push_local_scope`.
    pub(super) scope_marks: Vec<usize>,
    /// Per-scope unused-binding tracking: a let/param/match name that does not
    /// start with `_`, mapped to its definition span. Anything left when the
    /// frame pops is an error. Frames move in lockstep with `scope_marks` and
    /// the fn frames, so a use inside a nested closure can mark an outer
    /// binding by walking the whole stack.
    pub(super) unused: Vec<HashMap<StrId, Span>>,
    pub(super) outer_scopes: Vec<Scope>,
    pub(super) local_count: i32,
    /// Entry-frame slot → `program.functions` index for every top-level `fn`
    /// already compiled. A hit lets the Core emit use `CallKnown` (immediate
    /// `func_idx`, no callee pushed); a miss — a forward ref within an SCC, or
    /// a non-fn global — falls back to `PushGlobal; Call`.
    pub(super) global_to_func: HashMap<GlobalSlot, crate::core_ir::FuncIdx>,
    /// This module's own top-level `fn`/`const` declarations, in Pass 5
    /// This module's own top-level `fn`/`const` declarations, in Pass 5
    /// SCC-visit order (leaves first). Cleared to entry-file scope at
    /// `code_mark`.
    ///
    /// The elaborator walks the module block's decl nodes in this order, not
    /// source order, so a forward-referenced `const` is stored before it is
    /// read. Definition and use read the `slot` off the same record, so they
    /// cannot disagree.
    pub(super) toplevel_decls: Vec<ToplevelDecl>,
    /// Entry-frame slots for module-scope binds that are not declarations —
    /// top-level `let`s and destructured names — in binding order. Unlike
    /// `locals` this is not unwound by `pop_local_scope`, so it survives
    /// `analyse_module`'s scope pop and reaches the toplevel elaboration, which
    /// drains it in the order it was filled. A queue, not a map: a name can be
    /// rebound and each binding needs its own slot. Cleared to entry-file scope
    /// at `code_mark`.
    pub(super) toplevel_binds: VecDeque<GlobalSlot>,
    /// True only while `analyse_module` walks a module's own statement list —
    /// the one walk whose bindings the toplevel elaboration mirrors. The queue
    /// above is positional, so any other walk feeding it would hand the
    /// elaborator a slot belonging to a different binding. Being a global does
    /// not say this on its own: imports, prelude slot seeds and declarations
    /// are globals bound outside the statement walk, each reaching the
    /// elaborator by its own route.
    pub(super) walking_module_statements: bool,
    /// Memo for [`ElabCtx::resolve_rty`], keyed by union-find root. Cleared in
    /// `elaborate`, with the pool it indexes.
    rty_cache: HashMap<Ty, RTy>,
    pub(super) captures: HashMap<StrId, i32>,
    pub(super) capture_names: Vec<StrId>,
    pub(super) current_binding: Option<StrId>,
    /// One-shot self-name for the next `enter_fn_frame(None)`, so a
    /// `name = fn(...)` lambda can self-recurse without the enclosing fn's
    /// binding leaking into unrelated (e.g. HOF-arg) lambdas.
    pub(super) next_fn_self_name: Option<StrId>,
    /// The definition whose body is being compiled: the `owner` of every
    /// reference-graph occurrence emitted while it is set, which is the def→def
    /// edge channel the dead-code reachability walk follows. `None` at module
    /// top level so genuine executed code roots its references. Spans nested
    /// lambdas, which have no `DefId` of their own.
    pub(super) current_owner: Option<DefId>,
    pub(super) engine: InferEngine,
    /// Generic var ids rigid for the body being checked (a `fn` annotation's
    /// type parameters). `instantiate` callers pass it so those ids are not
    /// freshened inside the body.
    /// pass this so those ids are not freshened inside the body.
    pub(super) rigid_ids: HashSet<i32>,
    pub(super) recorded: Vec<RawRef>,
    /// Reference-graph collector for the module currently being analysed.
    /// `compile_module_body` moves the finished set into that module's
    /// `CachedModule`. Reset by `reset_to` like `recorded`.
    pub(super) module_refs: ModuleReferences,
    check_only: bool,
    /// Native compile-at-load accounting: how many bodies the `AL_NATIVE` mode
    /// selected and how long the hook spent on them. Summarised against the
    /// 100ms unit budget under `AL_NATIVE_DEBUG`.
    native_stats: super::native::UnitStats,
    /// Whether to buffer per-occurrence `RawRef`s and resolve them into
    /// `HoverFact`s in `finalize_references`. Only the LSP consumes those, so
    /// `compile`/`check` leave this `false` and skip the whole O(occurrences)
    /// resolve pass. The reference graph is built separately via `module_refs`
    /// and is unaffected.
    pub(super) collect_hover_facts: bool,
    pub(super) module_table: ModuleTable,
    /// Append-only `ModulePath` ↔ `ModuleId` interner backing every `DefId`.
    /// Persistent across `reset_to` so a `DefId` minted in one `check` still
    /// resolves after later modules are added or evicted.
    pub(super) ref_interner: ModuleInterner,
    pub(super) current_module: ModulePath,
    /// `current_module`'s canonical cache key, kept in lockstep with it so no
    /// per-module bookkeeping re-derives a key from an unresolved path.
    pub(super) current_module_key: ModuleKey,
    /// Memo of `current_module` interned into `engine.str_slices`; cleared
    /// whenever `current_module` is swapped.
    pub(super) module_path_slice: Option<ArenaSlice<pool::StrSlices>>,
    /// Memo of a module-path `ArenaSlice` → its interned `ModuleId`. The
    /// `str_slices` pool is append-only within a compile, so a given slice
    /// always denotes the same path. Cleared in `reset_to`, the one point the
    /// pool is rewound.
    pub(super) defid_module_memo: HashMap<ArenaSlice<pool::StrSlices>, ModuleId>,
    /// Qualifier name in this file → the imported module's canonical key.
    pub(super) imported_qualifiers: HashMap<String, ModuleKey>,
    /// Canonical module key → the import path as the user wrote it. Identity is
    /// the resolved file, but diagnostics must still say `./lib`, not
    /// `/private/var/.../lib`.
    pub(super) module_display: HashMap<ModuleKey, String>,
    pub(super) base_dir: Option<PathBuf>,
    pub(super) prelude: PreludeBindings,
    /// Names user code may not redefine: every type and constructor the prelude
    /// exports, derived from `al.al` rather than mirrored in Rust. `@vm`
    /// functions are excluded, so `println` is shadowable. `BTreeSet` so the
    /// blob's `RESERVED` slice is sorted, reproducible and binary-searchable.
    pub(super) reserved: BTreeSet<String>,
    /// The build-time static stdlib, consulted on a runtime-map miss. `None` on
    /// the from-source path (`register_prelude`, `precompile_stdlib`, or the
    /// LSP editing the stdlib).
    static_stdlib: Option<&'static crate::static_ir::StaticStdlib>,
    /// Lowered Core IR accumulated during this compile, moved into
    /// [`Emitted::core`] at the end of [`compile_impl`].
    pub(super) core: crate::core_ir::CoreProgram,
    /// The `fn(...) {...}` expressions written directly inside the frame being
    /// walked, in the order the walk closed them.
    ///
    /// The frame owns its sites, not the compiler: `enter_fn_frame` parks the
    /// enclosing list, `compile_fn_body` moves the walked frame's list into its
    /// [`ParkedBody`], and `elaborate_deferred` swaps it back in for exactly
    /// that body's elaboration. A `HashMap<Span, _>` on the compiler could not
    /// do this: a `Span` carries no file id, so one module's lambda would
    /// answer for another's at the same offset.
    pub(super) frame_closures: Vec<ClosureSite>,
    /// The type the check walk inferred for every expression it entered, in
    /// entry order, one region per elaboration unit.
    ///
    /// The elaborator consumes it positionally ([`Elab::take_ty`]), never by
    /// span: a `Span` carries no file id, and re-deriving the types instead
    /// would reinstantiate constructor and module-function schemes into
    /// unresolved vars. The invariant is a traversal one — both walks enter the
    /// same expressions in the same order. The one skip that is a decision
    /// rather than a syntactic fact (is `name.field` a module member?) is
    /// recorded as a [`WalkStep::Qualified`], because the env it depends on has
    /// moved on by the time a deferred body is elaborated. `elaborate_fn`
    /// asserts the region is fully consumed.
    ///
    /// This is the region currently being filled; enclosing bodies' regions are
    /// parked in [`Compiler::walk_tys_stack`]. What is left when no body is
    /// open is the module toplevel's own region. Cleared by `reset_to`, whose
    /// engine rewind would otherwise leave dangling `Ty` indices.
    pub(super) walk_tys: Vec<WalkStep>,
    /// The regions of the bodies enclosing the one being walked, innermost last.
    /// Parked, not indexed: only [`Compiler::walk_tys`] is ever written to.
    pub(super) walk_tys_stack: Vec<Vec<WalkStep>>,
    /// Bodies whose typecheck walk has finished but whose Core pipeline is
    /// deferred until the enclosing declaration group has been generalized.
    /// See [`Self::begin_deferred_elaboration`].
    deferred_bodies: Vec<DeferredBody>,
    /// Nesting depth of open deferral regions. The parked bodies drain when it
    /// returns to zero.
    defer_depth: u32,
    /// `(bodies parked so far, the value env those bodies saw)`, set by
    /// [`Self::pin_deferred_env`] at the end of the declaration walk.
    ///
    /// A `DeferredBody` snapshots the frame but not `self.env`, and
    /// `resolve_name` re-queries the live env at drain time. The drain runs
    /// after the toplevel `let` walk and `TypeEnv::define` overwrites in place,
    /// so without this pin a toplevel `let` would re-point a name a declared
    /// body already resolved (`println = 5` after `fn g() { println(x) }`
    /// turned the callee into a self-call).
    deferred_env_pin: Option<(usize, TypeEnv)>,
    /// Native-backend hook fired once per lowered body; see [`NativeHook`].
    /// `None` on every other path.
    native_hook: Option<NativeHook>,
}

/// One body, elaborated into a whole-module [`TypedProgram`] the Core pipeline
/// can consume.
///
/// `FuncIdx` indexes both `TypedProgram::fns` and `program.functions`, so `fns`
/// is padded up to `program.functions.len()` before the walk. `eta_base` is
/// that padding length: `fns[eta_base..]` are exactly the eta wrappers the walk
/// appended, and `program.functions[eta_base..]` their reserved entries.
struct Elaborated {
    program: TypedProgram,
    eta_base: usize,
}

/// One body, all the way through `lower`: the `CoreFn` and the arena its `RTy`s
/// index (`perceus` reads it, `emit` does not).
///
/// A module toplevel's entry-frame slot pinnings ride the IR itself — `lower`
/// copies `TypedBind::global` onto each module-scope `Let`'s `CoreBind` and
/// `emit_toplevel` reads them back — so there is no side table to desync from
/// the `PushGlobal <slot>` already baked into every emitted fn body.
struct LoweredBody {
    core: CoreFn,
    pool: ResolvedPool,
}

/// Per-body hook into the native (Cranelift) backend, called once for every
/// lowered body at the only point where the body's post-perceus [`CoreFn`] and
/// the [`ResolvedPool`] its `RTy`s index are both alive. The pool is per-body
/// and dies with the [`LoweredBody`], so a post-pass over [`Emitted::core`]
/// would resolve types through a dropped arena — native codegen must hang off
/// this seam or not run at all.
///
/// The body's [`FuncIdx`](crate::core_ir::FuncIdx) is passed explicitly: it is
/// the key the native entry table, closure dispatch and the perf map share.
/// The hook only observes, so it cannot reserve or reorder `Function` entries.
///
/// Fires for declared bodies and for the eta wrappers elaboration mints; never
/// for module toplevels or `__main__`, never under `check_only`, and only for
/// the bodies the process-wide `AL_NATIVE` mode selects. Installing a hook
/// makes `compile_impl` re-lower the whole stdlib from source instead of
/// seeding the blob, which ships post-emit bytecode with no `CoreFn` to hand
/// over, so the hook sees every body in the program.
///
/// A caller-installed callback so the driver crate — which owns the VM, the
/// runtime shims and the JIT finalize step — decides what each fire does. This
/// compiler hands over `(FuncIdx, CoreFn, pool)` and never learns what a
/// backend is.
pub type NativeHook = Box<dyn FnMut(crate::core_ir::FuncIdx, &CoreFn, &ResolvedPool)>;

/// Placeholder a [`Compiler::walk_tys`] slot holds between its reservation and
/// its fill. No real `Ty` can take this value: it indexes the engine's node
/// arena, which never reaches `u32::MAX` entries.
const WALK_TY_PENDING: Ty = Ty(u32::MAX);

/// A region with no unfilled slot left. A `WALK_TY_PENDING` reaching the
/// elaborator would index the engine's node arena out of bounds.
fn walk_region_is_filled(region: &[WalkStep]) -> bool {
    !region.contains(&WalkStep::Ty(WALK_TY_PENDING))
}

/// A `close_walk_region` with no matching `open_walk_region`. Aborts in release
/// too: restoring the wrong region hands a body another body's types.
#[allow(clippy::panic)]
#[cold]
#[inline(never)]
fn walk_region_underflow() -> ! {
    panic!(
        "internal compiler error: close_walk_region without matching open_walk_region. \
         Please report this as a compiler bug."
    )
}

/// Toplevel elaboration left module-scope slots queued: the check walk and the
/// elaborator disagreed about which statements bind at module scope. Aborts in
/// release too — every later bind would land in the wrong global slot.
#[allow(clippy::panic)]
#[cold]
#[inline(never)]
fn unclaimed_toplevel_slots(n: usize) -> ! {
    panic!(
        "internal compiler error: toplevel elaboration left {n} module-scope slot(s) unclaimed. \
         Please report this as a compiler bug."
    )
}

/// A `Function` was pushed while the elaborator walked, so the `FuncIdx` the
/// next `FnTable::push` mints no longer names it. Aborts in release too: every
/// eta wrapper reserved after the stray push would call the wrong function.
#[allow(clippy::panic)]
#[cold]
#[inline(never)]
fn function_reserved_during_elaboration() -> ! {
    panic!(
        "internal compiler error: a `Function` was reserved while the elaborator walked. \
         Please report this as a compiler bug."
    )
}

/// A `fn(...) {...}` expression the walk closed over: the `Function` reserved
/// for its body, and the names its frame captured, in the order
/// `Function::capture_count` counts them and `PushCapture` indexes them.
///
/// Lives in the *enclosing* frame's [`Compiler::frame_closures`], since that is
/// the frame whose elaboration builds the `Atom::Closure`: the capture values
/// are loads in the enclosing scope, not the lambda's.
pub(super) struct ClosureSite {
    /// Which lambda. The AST has no node ids, so a span is a node's only
    /// identity; unique within one body, which is the only scope a site is ever
    /// looked up in.
    at: Span,
    func_idx: crate::core_ir::FuncIdx,
    captures: Vec<StrId>,
}

/// A function body parked between its typecheck walk and its elaboration.
///
/// The walk fixes everything about the body except its types: which `Function`
/// slot it owns, which names it captured and at which indices, where its
/// jump-over sits. Types keep moving until the whole SCC has been generalized,
/// so `lower` runs last, off this record. Built whole in
/// [`Compiler::finish_fn_frame`]; no field is ever back-patched.
struct DeferredBody {
    name: StrId,
    param_binds: Vec<(StrId, Ty)>,
    /// Cloned, not borrowed: `Compiler` carries no `'ast` lifetime and the
    /// deferral outlives the `&ast::Expression`. Spans and shape are preserved,
    /// so `closures` and `walk_tys` still line up with it.
    body: ast::Expression,
    body_ty: Ty,
    /// This body's walk region, consumed positionally by the elaborator. See
    /// [`Compiler::walk_tys`].
    walk_tys: Vec<WalkStep>,
    /// The lambdas written directly inside this body. Swapped into
    /// [`Compiler::frame_closures`] for its elaboration and dropped with it.
    closures: Vec<ClosureSite>,
    /// `local_count` watermark after the params were bound; `Function.locals`
    /// is this maxed with Core's own slot allocation.
    param_slots: i32,
    /// The placeholder `Function` this body fills in. Reserved in both modes:
    /// `check_only` still records it on a [`ClosureSite`] and in
    /// `global_to_func`, and `lower` still bakes it into
    /// `Atom::Closure`/`CallKnown`.
    func_idx: crate::core_ir::FuncIdx,
    /// Address of `enter_fn_frame`'s jump-over. Patched once every body parked
    /// in the region is emitted, so the enclosing stream skips the whole run.
    jump_over: i32,
    /// Frame state `resolve_name` needs: a capture must land on the same
    /// `Denotation::capture` index the walk gave it, and a self-reference on
    /// the same self denotation.
    captures: HashMap<StrId, i32>,
    capture_names: Vec<StrId>,
    rigid_ids: HashSet<i32>,
    binding: Option<StrId>,
    /// The enclosing frames' locals exactly as `resolve_variable` saw them
    /// during the walk (module scope at index 0). Restored wholesale at
    /// elaboration time.
    ///
    /// Truncating this to the module scope is not sound: `current_binding`
    /// short-circuits before a capture is recorded, so a recursive local lambda
    /// whose self-name shadows a module-scope decl would re-resolve to
    /// `SelfGlobal(module_slot)` and push the wrong value where the walk
    /// emitted `PushSelf`.
    outer_scopes: Vec<Scope>,
    /// Type-env entries `resolve_name` would otherwise miss: the enclosing
    /// frames' scopes are long popped by elaboration time. Only two names can
    /// reach it from outside the module scope — a captured binding, and the
    /// frame's own self-name. Re-pushed as one scope around the `lower` call.
    capture_env: Vec<(String, Scheme)>,
}

/// The walk half of a [`DeferredBody`], carried by value from
/// [`Compiler::compile_fn_body`] to [`Compiler::finish_fn_frame`], which adds
/// the frame half (captures, `func_idx`, jump-over) and pushes the complete
/// record.
///
/// Being the only thing `compile_fn_body` can return is the point: the phase
/// boundary is a type, not a convention. The typecheck walk cannot emit a
/// function body because no `code_start` exists yet for anyone to spell.
struct ParkedBody {
    name: StrId,
    param_binds: Vec<(StrId, Ty)>,
    body: ast::Expression,
    body_ty: Ty,
    walk_tys: Vec<WalkStep>,
    closures: Vec<ClosureSite>,
    param_slots: i32,
}

/// A name resolved to a constructor: what `type_ctor_pattern` needs to slot and
/// type the pattern's arguments against.
struct CtorLookup {
    type_name: StrId,
    arity: usize,
    field_labels: ArenaSlice<pool::StrSlices>,
    scheme: Scheme,
}

impl CtorLookup {
    /// The lookup for `scheme` when it names a constructor; `None` otherwise.
    fn from_scheme(scheme: Scheme) -> Option<Self> {
        match scheme.kind {
            ValueKind::Constructor {
                type_name,
                arity,
                field_labels,
                ..
            } => Some(CtorLookup {
                type_name,
                arity: arity as usize,
                field_labels,
                scheme,
            }),
            _ => None,
        }
    }
}

/// Which toplevel `append_toplevel_init` is emitting. The two differ only in
/// what happens to the toplevel's tail value and Core body.
enum TopKind {
    /// `__main__`: the tail stays on the stack for `Halt`, and the Core body is
    /// kept on `self.core`.
    Entry,
    /// An imported module: its toplevel runs for effect, so the tail is popped.
    Module,
}

/// Outcome of resolving a `module.member` shape against the imported
/// qualifiers. Three-state because a non-qualified shape is recoverable (the
/// caller falls through to its general path) whereas a failed member lookup has
/// already been diagnosed and must short-circuit.
enum QualifiedMember<'a> {
    /// Not a `qualifier.member` shape (or the qualifier is not imported).
    NotQualified,
    /// A qualified shape whose member lookup failed; `lookup_module_member`
    /// has already emitted the diagnostic.
    LookupFailed,
    Resolved {
        module_key: ModuleKey,
        member_name: &'a str,
        member_span: Span,
        scheme: Scheme,
        /// Whether the member has a runtime binding (an entry-frame slot).
        has_binding: bool,
    },
}

/// A callee that resolved to a plain name or a qualified `module.member`.
struct ResolvedCallee<'a> {
    name: &'a str,
    span: Span,
    scheme: Scheme,
    /// Whether the callee has a runtime binding (see `Compiler::has_binding`).
    /// The module a qualified callee resolved through. Diagnostics render it
    /// with `Compiler::module_name` — the user-visible name, never the key.
}

struct CompiledBody {
    iface: ModuleInterface,
    watermark: Watermark,
    /// Reference-graph data for this module's body, moved into its
    /// `CachedModule` by `load_module`. The module's type-id range is both
    /// reserved and recorded inside `compile_module_body`, so its start is
    /// deliberately not threaded back out here.
}

pub fn compile(
    expr: &ast::Expression,
    base_dir: Option<&Path>,
    pre: Option<&'static crate::static_ir::StaticStdlib>,
) -> CompileResult {
    compile_impl(expr, base_dir, false, None, pre, None)
}

/// [`compile`] with a [`NativeHook`] installed for the duration; see
/// [`NativeHook`] for the contract. The caller publishes whatever the hook
/// compiled into the emitted program's [`NativeTable`](super::NativeTable)
/// after this returns, once the function list is final.
pub fn compile_with_native(
    expr: &ast::Expression,
    base_dir: Option<&Path>,
    pre: Option<&'static crate::static_ir::StaticStdlib>,
    native_hook: NativeHook,
) -> CompileResult {
    compile_impl(expr, base_dir, false, None, pre, Some(native_hook))
}

pub fn check(
    expr: &ast::Expression,
    base_dir: Option<&Path>,
    pre: Option<&'static crate::static_ir::StaticStdlib>,
) -> CompileResult {
    compile_impl(expr, base_dir, true, None, pre, None)
}

/// Analyse a file *as* a specific stdlib module, so editing `src/std/**/*.al`
/// does not report `@vm is stdlib-only` / `Result is reserved`. Always
/// check-only and always from source: the precompiled blob is stale.
pub fn check_as_module(
    expr: &ast::Expression,
    base_dir: Option<&Path>,
    module: ModulePath,
) -> CompileResult {
    compile_impl(expr, base_dir, true, Some(module), None, None)
}

pub(crate) fn new_compiler(base_dir: Option<&Path>, check_only: bool) -> Compiler {
    let mut ref_interner = ModuleInterner::new();
    let main_refs = ModuleReferences::new(ref_interner.intern(&module::main_module()));
    let program = Program::default();
    // The builder appends to the area the emitted `Program` anchors, so the
    // constants built during this compile stay frozen for the program's life.
    let frozen = program.frozen.builder();
    Compiler {
        program,
        frozen,
        const_dedup: HashMap::new(),
        locals: HashMap::new(),
        undo_log: vec![],
        scope_marks: vec![],
        unused: vec![],
        outer_scopes: vec![],
        local_count: 0,
        global_to_func: HashMap::new(),
        toplevel_decls: Vec::new(),
        toplevel_binds: VecDeque::new(),
        walking_module_statements: false,
        rty_cache: HashMap::new(),
        captures: HashMap::new(),
        capture_names: vec![],
        current_binding: None,
        next_fn_self_name: None,
        current_owner: None,
        engine: new_engine(),
        env: new_env(),
        rigid_ids: HashSet::new(),
        recorded: vec![],
        module_refs: main_refs,
        check_only,
        collect_hover_facts: false,
        module_table: ModuleTable::new(),
        module_display: HashMap::new(),
        ref_interner,
        current_module: module::main_module(),
        current_module_key: ModuleKey::main(),
        module_path_slice: None,
        defid_module_memo: HashMap::new(),
        imported_qualifiers: HashMap::new(),
        base_dir: base_dir.map(|p| p.to_path_buf()),
        prelude: PreludeBindings::default(),
        reserved: BTreeSet::new(),
        static_stdlib: None,
        core: crate::core_ir::CoreProgram::default(),
        frame_closures: Vec::new(),
        walk_tys: Vec::new(),
        walk_tys_stack: Vec::new(),
        deferred_bodies: Vec::new(),
        defer_depth: 0,
        deferred_env_pin: None,
        native_hook: None,
        native_stats: super::native::UnitStats::default(),
    }
}

impl Compiler {
    /// The `DefId` of the `Type` named `name` declared in module `path`, read
    /// from the reference graph's own definitions.
    ///
    /// Not from `env.definitions`: every record `type Foo {..}` registers a
    /// same-named `Constructor` that overwrites the `Foo => Type` entry, so
    /// that lookup yields the constructor and no type occurrence is ever
    /// recorded. The graph keeps both under one name as distinct `DefId`s.
    ///
    /// Same-module lookups read the live collector; a cross-module type reads
    /// the imported module's persisted `module_refs`. `None` for
    /// static/hydrated stdlib modules, whose type declaration spans are not
    /// persisted, so stdlib type goto-def does not work.
    fn type_defid_in_module(&self, path: &ModulePath, name: &str) -> Option<DefId> {
        fn pick(mr: &ModuleReferences, name: &str) -> Option<DefId> {
            mr.defs_named(name)
                .iter()
                .copied()
                .find(|d| d.entity == EntityKind::Type)
        }
        if *path == self.current_module {
            pick(&self.module_refs, name)
        } else {
            pick(self.module_table.module_refs_by_path(path)?, name)
        }
    }
}

/// The head of a constructor pattern: `NotFound`, or `io.NotFound` reached
/// through a module qualifier.
struct CtorHead<'a> {
    qualifier: Option<&'a ast::Identifier>,
    name: &'a ast::Identifier,
}

fn compile_impl(
    expr: &ast::Expression,
    base_dir: Option<&Path>,
    check_only: bool,
    as_module: Option<ModulePath>,
    pre: Option<&'static crate::static_ir::StaticStdlib>,
    native_hook: Option<NativeHook>,
) -> CompileResult {
    let mut c = new_compiler(base_dir, check_only);
    c.native_hook = native_hook;
    // `off` drops the hook outright so no body can fire it.
    if super::native::config().mode == super::native::NativeMode::Off {
        c.native_hook = None;
    }
    if let Some(m) = as_module.clone() {
        // `as_module` is always a stdlib path (see `check_as_module`), whose
        // written form is its canonical identity.
        c.current_module_key = ModuleKey::for_stdlib(&m);
        c.current_module = m;
    }

    // Analysing the prelude file itself must not load the prelude on top of it,
    // or every type becomes a redefinition of itself. Other stdlib modules
    // still need it for `Result`/`Nil`/etc.
    let is_prelude_self = as_module.as_deref() == Some(module::al_prelude().as_slice());
    if !is_prelude_self {
        match pre {
            Some(s) if c.native_hook.is_none() => c.seed_static(s),
            // A native hook must see stdlib bodies, and a body is only visible
            // at its own `lower`: the blob ships post-emit bytecode with no
            // `CoreFn`/`ResolvedPool` to hand over. So re-lower the whole
            // stdlib from source. Eager (every module, not just the entry's
            // imports) so the function table matches the seeded one entry for
            // entry — the pipeline is deterministic, so this reproduces the
            // blob's program prefix, numbering included.
            Some(_) => {
                c.register_prelude();
                let at = Span::DUMMY;
                for path in module::stdlib::all_modules() {
                    if has_errors(&c.engine.diagnostics) {
                        break;
                    }
                    c.load_module(&ast::ImportPath::canonical(path), at);
                }
                // Reset the memos that would otherwise leak across the
                // stdlib/user boundary. A seeded compile starts with both
                // empty, so leaving them populated would let user code dedup
                // constants against stdlib pool entries and resolve stdlib
                // callees to `CallKnown` where the seeded program reads a
                // module-init global slot: same semantics, different bytecode.
                c.const_dedup.clear();
                c.global_to_func.clear();
            }
            None => c.register_prelude(),
        }
        if has_errors(&c.engine.diagnostics) {
            // The seed itself failed: the user's program was never compiled,
            // so there is nothing to hand back — not even a partial one.
            return CompileResult {
                emitted: None,
                diagnostics: c.engine.diagnostics,
                references: Rc::new(ReferenceGraph::new()),
            };
        }
    }
    // `__main__` must include the module-init code already emitted for seeded
    // stdlib functions, so it starts at 0. The from-source path emits no init
    // code (al.al has only types and @vm fns), so 0 == code.len() there too.
    let main_start = 0i32;
    // Marks for the Core re-emit below. They must sit after `process_imports`,
    // which compiles imported modules into this same program and entry frame,
    // and before this file's own `analyse_module`, so they cover exactly the
    // entry file's output and never an imported module's init.
    let code_mark;
    let slot_base;
    let top_ty;
    if let ast::Expression::BlockExpression(block) = expr {
        c.process_imports(block);
        c.bump_type_ids_past_reserved();
        code_mark = c.program.code.len();
        slot_base = c.local_count;
        // Every imported module's bindings are already live in `[0, code_mark)`
        // and are not re-emitted by the toplevel Core, so drop them or a
        // shadowing entry-file bind dequeues an import's slot.
        c.toplevel_binds.clear();
        c.toplevel_decls.clear();
        // Opened here, not inside `analyse_module`, so the schemes it defines
        // stay visible through the toplevel elaboration below — otherwise a
        // `type` decl followed by a ctor use bails on `unbound callee`. Popped
        // after it.
        c.env.push_scope();
        c.analyse_module(block, None);
        top_ty = c.ty_nil();
    } else {
        code_mark = c.program.code.len();
        slot_base = c.local_count;
        c.env.push_scope();
        // `analyse_module` opens a local scope around a module's statements, so
        // nested blocks bind as frame temps rather than globals. A bare
        // expression has no statement walk, so open the same scope here: without
        // it a `let` in the expression's first nested block would resolve as a
        // global whose slot `bind_local` never queued and nothing ever pins.
        c.push_local_scope();
        // The same phase boundary `analyse_module` puts around its walk: a bare
        // expression can still contain lambdas, and none may lower or emit
        // while the expression is being typechecked.
        c.begin_deferred_elaboration();
        top_ty = c.compile_expr(expr);
        c.end_deferred_elaboration();
        c.pop_local_scope();
        // The expression's outermost block is the first the elaborator sees, so
        // it is elaborated as though it were a module toplevel and will try to
        // drain the queue. Nothing queued by this compile belongs to it.
        c.toplevel_binds.clear();
        c.toplevel_decls.clear();
    }

    // Elaborate the module toplevel, lower it into Core and re-emit the entry
    // frame from it, so `__main__`'s bytecode is Core-derived like every other
    // body (docs/core-ir-spec.md §Pipeline step 3). Fn bodies already reference
    // sibling decls via `PushGlobal <slot>`; `ElabCtx::global_slot` stamped the
    // same slots onto each `TypedBind`, `lower` copied them onto the toplevel
    // `Let`s, and `emit_toplevel` pins those `Let`s to them, so the entry-frame
    // `StoreLocal`s line up.
    //
    // Elaboration and lowering run under `check_only` too — only the emit half
    // is skipped — so `al check` reports a well-typed program the front half
    // cannot handle. Drained either way, so a check that bailed leaves nothing
    // behind for the next compile on this `Compiler`.
    let walk_tys = c.take_toplevel_walk_tys();
    if let Some(clean) = c.clean_module() {
        let name = c.engine.intern("__main__");
        // The wrappers land ahead of `append_toplevel_init`'s `base`, inside
        // `[code_mark, base)` — the region the entry frame's first `Jump` hops over.
        let lowered = c.elaborate_then_materialize(clean, None, |c, pool, fns| {
            // A module block was walked statement-by-statement, so nothing
            // recorded a type for the block itself; a bare expression was
            // entered by `compile_expr`, so its region starts with its own.
            match expr {
                ast::Expression::BlockExpression(block) => {
                    typed_ir::elaborate_toplevel(c, pool, fns, name, block, top_ty, &walk_tys)
                }
                _ => typed_ir::elaborate_body(c, pool, fns, name, &[], expr, top_ty, &walk_tys),
            }
        });
        c.frame_closures.clear();
        if !check_only {
            c.append_toplevel_init(lowered, code_mark, slot_base, TopKind::Entry);
            c.core.consts = c.program.constants.clone();
        }
    }
    c.env.pop_scope();

    c.emit(Op::Halt);

    c.program.functions.push(Function {
        name: "__main__".into(),
        arity: 0,
        locals: c.local_count,
        capture_count: 0,
        code_start: main_start,
        code_len: c.program.code.len() as i32 - main_start,
    });
    c.program.entry = c.program.functions.len() as i32 - 1;
    // The function list is final: size the native-entry table against it so the
    // backend can publish bodies keyed by the same `FuncIdx`. Empty = interpret.
    c.program.native = super::NativeTable::new(c.program.functions.len());

    if !check_only {
        // Jump operands are frame-relative, so fusion needs `functions` to know
        // which frame owns each instruction; the entry frame owns the rest.
        fuse(&mut c.program.code, &c.program.functions);
        c.native_stats.log_summary(c.program.code.len());
    }

    let (references, _facts) = c.finalize_references();
    CompileResult {
        emitted: Some(Emitted {
            program: c.program,
            core: c.core,
        }),
        diagnostics: c.engine.diagnostics,
        references,
    }
}

/// The compiler state `precompile_stdlib` freezes into the static blob.
pub(crate) struct CompilerParts {
    pub(crate) program: Program,
    pub(crate) prelude: PreludeBindings,
    pub(crate) reserved: BTreeSet<String>,
    pub(crate) next_type_id: TypeId,
    pub(crate) local_count: i32,
}

impl Compiler {
    pub(crate) fn diagnostics(&self) -> &[Diagnostic] {
        &self.engine.diagnostics
    }
    pub(crate) fn take_module_table(&mut self) -> IndexMap<String, ModuleInterface> {
        std::mem::take(&mut self.module_table).into_loaded()
    }
    pub(crate) fn take_type_info(&mut self) -> IndexMap<String, crate::types::TypeInfo> {
        self.env.take_type_info()
    }
    /// Tear down for `precompile_stdlib`: program + scalars, plus the engine
    /// (whose arena `flatten` snapshots).
    pub(crate) fn into_parts(self) -> (CompilerParts, InferEngine) {
        (
            CompilerParts {
                program: self.program,
                prelude: self.prelude,
                reserved: self.reserved,
                next_type_id: self.env.next_type_id(),
                local_count: self.local_count,
            },
            self.engine,
        )
    }

    /// Seed from the build-time static stdlib: copy code/functions/constants
    /// (the VM mutates around them, so they must be owned), hydrate the prelude
    /// `TypeInfo`s and constructor `Scheme`s into root scope, and keep the blob
    /// so non-prelude modules hydrate lazily on first import. Nothing is parsed
    /// or deserialized.
    pub(crate) fn seed_static(&mut self, s: &'static crate::static_ir::StaticStdlib) {
        debug_assert!(self.program.code.is_empty());
        self.static_stdlib = Some(s);
        self.module_table.set_static_fallback(s);
        self.prelude = s.prelude.clone();
        self.engine.set_prim_ids(self.prelude.prim_ids());
        self.env.set_next_type_id(s.next_type_id);
        // The static type arena is the live arena's prefix: every `Ty`/
        // `ArenaSlice` in a stdlib scheme or typeinfo indexes into it.
        self.engine.seed_arena(crate::types::ArenaSeed {
            nodes: s.nodes,
            children: s.children,
            strings: s.str_pool,
            quants: s.quants,
            str_slices: s.str_slices,
            type_params: s.type_params,
            variant_fields: s.variant_fields,
            variants: s.variants,
        });

        let (code, functions, constants) = s.hydrate_program(&mut self.frozen);
        self.program.code = code;
        self.program.functions = functions;
        self.program.constants = constants;
        self.local_count = s.local_count;
        for slot in 0..s.local_count {
            let id = self.engine.intern(&format!("__pre{}", slot));
            self.bind_local(id, slot);
        }

        // Copied eagerly so both the by-name map (annotation resolution) and
        // the by-id registry (exhaustiveness, field access, hover) hit without
        // hydration. Seeded below the session watermark, so never truncated and
        // never overwritten in place.
        for (name, idx) in s.typeinfo_by_name {
            // The env is fresh here, so nothing is overwritten or journaled.
            // fresh here, so no overwrite occurs and nothing is journaled.
            self.env.store_type_info(name, ti);
        }

        // Re-export prelude constructor/@vm schemes into root scope.
        let key = ModuleKey::prelude();
        if let Some(iface) = self.module_table.get_or_hydrate(&key) {
            let pairs: Vec<_> = iface
                .values
                .iter()
                .map(|(n, ev)| (n.clone(), ev.scheme))
                .collect();
            for (name, scheme) in pairs {
                self.env.define(&name, scheme);
            }
        }
    }

    /// Reserved-name check covering both the runtime set (from-source path)
    /// and the static sorted slice (`seed_static` path).
    pub(super) fn is_reserved(&self, name: &str) -> bool {
        if self.reserved.contains(name) {
            return true;
        }
        if let Some(s) = self.static_stdlib {
            return s.reserved.binary_search(&name).is_ok();
        }
        false
    }

    // Codegen primitives never consult `check_only`. The frame scaffolding —
    // jump-over, trailing `Ret`, `Function` entry — is laid down in every mode,
    // so a `func_idx` denotes the same function under `al check` as under
    // `al build`. `check_only` truncates the pipeline in exactly one place
    // (`elaborate_body` returns before `perceus`/`emit`) and skips the toplevel
    // init and the peephole pass. It is a suffix of the compile, not a second
    // compilation mode.

    #[inline]
    fn emit(&mut self, o: Op) {
        self.program.code.push(op(o));
    }

    fn current_addr(&self) -> i32 {
        self.program.code.len() as i32
    }

    /// Pool `v`, deduplicating against the existing pool.
    ///
    /// The `const_dedup` memo survives `IncrementalSession::reset_to`, which
    /// truncates `program.constants` without clearing it, so every hit is
    /// re-validated against the live pool and a stale entry just misses.
    fn add_constant(&mut self, v: Value) -> i32 {
        let bits = v.to_bits();
        if let Some(&idx) = self.const_dedup.get(&bits)
            && let Some(slot) = self.program.constants.get(idx as usize)
            && slot.to_bits() == bits
        {
            return idx;
        }
        self.program.constants.push(v);
        let idx = self.program.constants.len() as i32 - 1;
        self.const_dedup.insert(bits, idx);
        idx
    }

    /// Pool a frozen Int constant.
    fn const_int(&mut self, i: i64) -> i32 {
        let v = self.frozen.int(i).into_value();
        self.add_constant(v)
    }

    /// Pool a frozen string constant (interned: one allocation per contents).
    fn const_str(&mut self, s: &str) -> i32 {
        let v = self.frozen.str(s).into_value();
        self.add_constant(v)
    }

    /// Pool a frozen binary constant of `bit_len` bits.
    fn const_binary(&mut self, bytes: Vec<u8>, bit_len: u64) -> i32 {
        let v = self.frozen.binary_bits(bytes, bit_len).into_value();
        self.add_constant(v)
    }

    pub(super) fn get_or_create_local(&mut self, name: &str) -> i32 {
        let id = self.engine.intern(name);
        if let Some(entry) = self.locals.get(&id).copied() {
            // A slot bound at a strictly shallower depth is inherited from an
            // enclosing scope, so shadowing must allocate a fresh one to keep
            // the outer value alive past the block.
            let inherited =
                !self.scope_marks.is_empty() && entry.depth < self.scope_marks.len() as u32;
            // A module-scope rebind also needs a fresh slot: top-level bindings
            // live in the entry frame and a closure that captured one reads
            // that slot at call time, so reusing it would overwrite what the
            // closure observes. Nested scopes capture by value at
            // `MakeClosure` time, so same-scope reuse is safe there.
            let module_scope = self.outer_scopes.is_empty();
            if !inherited && !module_scope {
                return entry.slot;
            }
        }
        let idx = self.alloc_temp();
        self.bind_local(id, idx);
        idx
    }

    /// Bind `name` to `slot` in the current scope without publishing the slot to
    /// the toplevel elaboration. This is the whole of binding for anything whose
    /// slot reaches the elaborator another way: a top-level declaration (via its
    /// [`ToplevelDecl`]) and every binding inside a function frame.
    ///
    /// The displaced entry goes on the undo log so `pop_local_scope` can restore
    /// it. Outside any open block scope there is nothing to unwind, so the log
    /// entry is skipped.
    fn bind_local_raw(&mut self, name: StrId, slot: i32) {
        let entry = LocalSlot {
            slot,
            depth: self.scope_marks.len() as u32,
            is_global: self.binds_a_global(),
        };
        let prev = self.locals.insert(name, entry);
        if !self.scope_marks.is_empty() {
            self.undo_log.push((name, prev));
        }
    }

    /// Whether a binding made now lands in the entry frame — the compiler's one
    /// definition of "global".
    ///
    /// No function frame is open, and we are at the module's own local-scope
    /// depth: `analyse_module` opens exactly one scope, imports and prelude slot
    /// seeds bind below it at depth 0, and every nested block at module level
    /// pushes another mark. The bare-expression entry path opens a matching
    /// scope of its own. Both halves of the global question —
    /// [`Self::bind_local`]'s queue and `resolve_variable`'s `PushGlobal` — are
    /// derived from this and recorded on [`LocalSlot::is_global`], so they
    /// cannot drift apart.
    fn binds_a_global(&self) -> bool {
        self.outer_scopes.is_empty() && self.scope_marks.len() <= 1
    }

    /// [`Self::bind_local_raw`], plus: queue a module-scope binding's slot for
    /// the toplevel elaboration.
    ///
    /// The queue outlives `analyse_module`'s `pop_local_scope`, which the
    /// `locals` entry does not. Only bindings made by the module's own statement
    /// walk qualify. That is not a second definition of "global": the queue is
    /// positional, and the other globals — imports, prelude seeds, declarations
    /// — reach the elaborator by another route and must not be dequeued.
    fn bind_local(&mut self, name: StrId, slot: i32) {
        self.bind_local_raw(name, slot);
        if self.walking_module_statements && self.binds_a_global() {
            self.toplevel_binds.push_back(GlobalSlot(slot));
        }
    }

    /// Allocate the entry-frame slot for a top-level `fn`/`const` declaration.
    ///
    /// Never reuses an existing binding's slot — a module-scope rebind must not
    /// alias a slot a closure already captured by reference — and never queues
    /// it: a declaration's slot travels on its own [`ToplevelDecl`].
    pub(super) fn alloc_decl_slot(&mut self, name: &str) -> GlobalSlot {
        let id = self.engine.intern(name);
        let idx = self.alloc_temp();
        self.bind_local_raw(id, idx);
        GlobalSlot(idx)
    }

    /// Take the entry-frame slot [`Self::bind_local`] queued for the next
    /// module-scope `let`/destructured binding. Positional, not by name: both
    /// walks visit the module's statements in source order, so a rebound name
    /// (`x = 1; f = fn() x; x = 2`) hands each binding its own slot. The caller
    /// stamps the slot onto the binding it is building, and no later pass maps a
    /// name back to a slot.
    ///
    /// A fn body's elaboration also walks an outermost block, and a `let` there
    /// must not take a module-scope slot. Only a module toplevel runs with no
    /// enclosing frame.
    pub(super) fn take_global_slot(&mut self) -> Option<GlobalSlot> {
        if !self.outer_scopes.is_empty() {
            return None;
        }
        self.toplevel_binds.pop_front()
    }

    #[inline]
    pub(super) fn alloc_temp(&mut self) -> i32 {
        let idx = self.local_count;
        self.local_count += 1;
        idx
    }

    pub(super) fn push_local_scope(&mut self) {
        self.scope_marks.push(self.undo_log.len());
        self.unused.push(HashMap::new());
    }

    pub(super) fn pop_local_scope(&mut self) {
        if let Some(mark) = self.scope_marks.pop() {
            // Unwind in reverse bind order so a name shadowed twice in one
            // scope lands back on its pre-scope entry.
            debug_assert!(
                mark <= self.undo_log.len(),
                "unbalanced push/pop_local_scope"
            );
            for (name, prev) in self.undo_log.drain(mark..).rev() {
                match prev {
                    Some(entry) => {
                        self.locals.insert(name, entry);
                    }
                    None => {
                        self.locals.remove(&name);
                    }
                }
            }
        }
        self.pop_unused_scope();
    }

    /// Open a lexical block scope: pushes a fresh type-env scope and a local
    /// undo mark in lockstep. Must be matched by `pop_block_scope`.
    fn push_block_scope(&mut self) {
        self.env.push_scope();
        self.push_local_scope();
    }

    fn pop_block_scope(&mut self) {
        self.pop_local_scope();
        self.env.pop_scope();
    }

    /// Record a let/param/match binding for unused-variable checking. Names
    /// starting with `_` are exempt. A same-name binding already tracked in this
    /// scope is reported now: it is shadowed and can no longer be referenced.
    pub(super) fn track_binding(&mut self, name: &str, sp: Span) {
        if name.starts_with('_') {
            return;
        }
        let id = self.engine.intern(name);
        let prev = self.unused.last_mut().and_then(|s| s.insert(id, sp));
        if let Some(prev_sp) = prev {
            self.unused_binding(name, prev_sp);
        }
    }

    /// Mark a name as used, searching innermost scope outward so a closure
    /// referencing an enclosing binding clears it there.
    fn mark_used(&mut self, name: StrId) {
        for scope in self.unused.iter_mut().rev() {
            if scope.remove(&name).is_some() {
                return;
            }
        }
    }

    fn pop_unused_scope(&mut self) {
        let Some(scope) = self.unused.pop() else {
            return;
        };
        let mut leftover: Vec<(StrId, Span)> = scope.into_iter().collect();
        leftover.sort_by_key(|(_, sp)| (sp.start_line, sp.start_column));
        for (id, sp) in leftover {
            let name = self.engine.str(id).to_string();
            self.unused_binding(&name, sp);
        }
    }

    /// Where a name lives in the current frame, as the [`Denotation`] the typed
    /// IR consumes. `None` when the frame does not bind it: a constructor, a
    /// builtin, or a declaration whose `locals` entry `analyse_module` unwound.
    fn resolve_variable(&mut self, name: StrId) -> Option<Denotation> {
        self.mark_used(name);
        if let Some(entry) = self.locals.get(&name) {
            return Some(Denotation::slot(FrameSlot(entry.slot)));
        }
        if let Some(idx) = self.captures.get(&name) {
            debug_assert!(*idx >= 0, "capture index is a Vec index");
            return Some(Denotation::capture(CaptureIdx(*idx)));
        }
        // Innermost-first so inner bindings shadow outer ones. Index 0 is the
        // entry frame, and its module-scope locals are the program's globals:
        // `StoreLocal` publishes them, so a nested fn loads them with
        // `PushGlobal` at call time instead of capturing by value. That is what
        // makes mutually-recursive top-level fns work.
        //
        // A local bound in a nested block/if/match scope at module level is not
        // a global: it is an ordinary entry-frame temp whose slot the toplevel
        // Core emit assigns, and nothing guarantees it is stored before a
        // `PushGlobal` of it runs. Which of the two a name is was decided by
        // [`Self::binds_a_global`] and recorded on the binding; re-deriving it
        // here from `depth` is how the loader and the binder came to disagree.
        for (i, scope) in self.outer_scopes.iter().enumerate().rev() {
            if let Some(entry) = scope.locals.get(&name) {
                let is_global = i == 0 && entry.is_global;
                if self.current_binding == Some(name) {
                    // A top-level fn's self-name resolves to its entry-frame
                    // slot so a value load emits `PushGlobal`; `PushSelf` would
                    // read the sentinel captures a `CallKnown` frame carries.
                    return Some(if is_global {
                        Denotation::self_toplevel_fn(GlobalSlot(entry.slot))
                    } else {
                        Denotation::self_closure()
                    });
                }
                if is_global {
                    return Some(self.global_denotation(GlobalSlot(entry.slot)));
                }
                let capture_idx = self.capture_names.len() as i32;
                self.captures.insert(name, capture_idx);
                self.capture_names.push(name);
                return Some(Denotation::capture(CaptureIdx(capture_idx)));
            }
        }
        None
    }

    /// A module-scope value at `slot`: a known top-level `fn` if one has been
    /// registered for that slot, an opaque global otherwise.
    fn global_denotation(&self, slot: GlobalSlot) -> Denotation {
        match self.global_to_func.get(&slot) {
            Some(&func_idx) => Denotation::known_fn(slot, func_idx),
            None => Denotation::global(slot),
        }
    }

    /// The denotation of one of this module's top-level declarations, read off
    /// the same [`ToplevelDecl`] record its elaborated definition is pinned to.
    fn decl_denotation(&self, name: StrId) -> Option<Denotation> {
        let decl = self.toplevel_decls.iter().find(|d| d.name == name)?;
        Some(self.global_denotation(decl.slot))
    }

    pub(super) fn error(&mut self, msg: String, sp: Span) {
        self.engine
            .diagnostics
            .push(Diagnostic::error(sp, DiagnosticCode::TypeError, msg));
    }

    fn module_error(&mut self, msg: String, sp: Span) {
        self.engine
            .diagnostics
            .push(Diagnostic::error(sp, DiagnosticCode::ModuleError, msg));
    }

    /// Mint the proof the elaborator demands, if the module has earned it.
    /// `None` means some subtree is poisoned and nothing may be elaborated; the
    /// user has already been told why.
    fn clean_module(&self) -> Option<CleanModule> {
        CleanModule::mint(&self.engine.diagnostics)
    }

    pub(super) fn note(&mut self, msg: String, sp: Span) {
        self.engine
            .diagnostics
            .push(Diagnostic::hint(sp, DiagnosticCode::RelatedLocation, msg));
    }

    fn unused_binding(&mut self, name: &str, sp: Span) {
        self.engine.diagnostics.push(Diagnostic::error(
            sp,
            DiagnosticCode::UnusedBinding,
            format!("'{name}' is unused; prefix with '_' to ignore"),
        ));
    }

    /// Probe the doc map for `name`, but only while collecting hover facts.
    /// On the non-LSP path `record` discards the doc, so skip the clone.
    fn doc_if_collecting(&self, name: &str) -> Option<String> {
        if self.collect_hover_facts {
            self.env.lookup_doc(name)
        } else {
            None
        }
    }

    pub(super) fn record(&mut self, name: &str, ty: Ty, sp: Span, doc: Option<String>) {
        // Only the LSP consumes hover facts. Elsewhere they are resolved and
        // discarded, so skip the `to_string` + `RawRef` buffering entirely.
        if !self.collect_hover_facts {
            return;
        }
        let module = self.current_module_id();
        self.recorded.push(RawRef {
            span: sp,
            name: name.to_string(),
            ty,
            doc,
            module,
        });
    }

    /// Resolve a module-path `ArenaSlice` to its stable `ModuleId`, memoised on
    /// the `Copy` slice. The `str_slices` pool is append-only within a compile,
    /// so a given slice always denotes the same path; the memo is dropped in
    /// `reset_to`, the one point the pool is rewound.
    fn module_id_of_slice(&mut self, sl: ArenaSlice<pool::StrSlices>) -> ModuleId {
        if let Some(&id) = self.defid_module_memo.get(&sl) {
            return id;
        }
        let path = self.engine.strs_of(sl);
        let id = self.ref_interner.intern(&path);
        self.defid_module_memo.insert(sl, id);
        id
    }

    /// The interned `ModuleId` of `current_module`, via the memoised path slice,
    /// so the hot recording path neither clones nor re-interns it.
    fn current_module_id(&mut self) -> ModuleId {
        let sl = self.current_module_slice();
        self.module_id_of_slice(sl)
    }

    /// Resolve an env-side [`DefinitionLocation`] to a stable reference-graph
    /// [`DefId`]. The module slice goes through the persistent `ref_interner` so
    /// the id survives incremental recompiles, and the declaring span is
    /// reconstructed single-line so a definition's `DefId` is bit-identical to
    /// the one every occurrence of it targets.
    pub(super) fn defid_of(&mut self, dl: DefinitionLocation) -> DefId {
        let module = self.module_id_of_slice(dl.module);
        DefId::new(module, dl.span(), dl.entity)
    }

    /// The [`DefId`] to stash in `current_owner` while a top-level
    /// `fn`/`const`/`type` body is compiled. Every reference inside that body,
    /// nested lambdas included, is attributed to this owner.
    pub(super) fn owner_defid(&mut self, name_span: Span, entity: EntityKind) -> DefId {
        let module = self.current_module_slice();
        self.defid_of(DefinitionLocation::new(name_span, module, entity))
    }

    /// Record a resolved name occurrence in the current module's collector,
    /// attributed to `current_owner` (`None` for genuine top-level executed
    /// code). The owner is the def→def edge the dead-code reachability walk
    /// follows; without it every reference in a body looks like executed code,
    /// so anything it names becomes a live root. Recording-only: never touches
    /// inference, schemes, slots or diagnostics.
    fn record_ref(&mut self, occ: Span, kind: ReferenceKind, target: DefId) {
        self.module_refs
            .add_reference(self.current_owner, Reference::new(occ, kind, target));
    }

    /// Record an occurrence of `kind` at `occ` targeting the value's canonical
    /// definition, carried on its `Scheme.def`. No-op when the name has none;
    /// builtins own no definition, so their edges dangle harmlessly.
    fn record_value_use(
        &mut self,
        def: Option<DefinitionLocation>,
        occ: Span,
        kind: ReferenceKind,
    ) {
        if let Some(dl) = def {
            let target = self.defid_of(dl);
            self.record_ref(occ, kind, target);
        }
    }

    /// Record a `Qualifier` occurrence for the module alias a qualified member
    /// use was reached through, so hover and goto-def on the `b` of `b.add(..)`
    /// reach module `b`. Not a use site: the alias's liveness rides on the
    /// member's own `Qualified` occurrence, which the caller records. An unknown
    /// qualifier records nothing.
    fn record_qualifier_use(&mut self, qualifier: &ast::Identifier) {
        if let Some(alias) = self
            .module_refs
            .defs_named(&qualifier.name)
            .iter()
            .find(|d| d.entity == EntityKind::ModuleAlias)
            .copied()
        {
            self.record_ref(qualifier.span, ReferenceKind::Qualifier, alias);
        }
    }

    /// [`Self::record_value_use`] for type references. No-op when the module has
    /// no such type (e.g. static/hydrated stdlib modules).
    fn record_type_use(&mut self, path: &ModulePath, name: &str, occ: Span, kind: ReferenceKind) {
        if let Some(target) = self.type_defid_in_module(path, name) {
            self.record_ref(occ, kind, target);
        }
    }

    /// Record a declaration, plus one `Definition`-kind self-occurrence at the
    /// declaring name so find-references includes the declaration site.
    ///
    /// The `Definition` is last-write-wins on its `DefId` — docs and visibility
    /// settle on the last write — while the self-occurrence fires only on first
    /// sight. It is owned by the definition itself rather than `None`, so it is
    /// never mistaken for executed code and can never bootstrap the def's own
    /// reachability. Recording-only: never touches inference, schemes, slots or
    /// diagnostics.
    pub(super) fn emit_def(
        &mut self,
        dl: DefinitionLocation,
        name: &str,
        doc: Option<String>,
        is_pub: bool,
        kind: DefinitionKind,
    ) {
        let loc = self.defid_of(dl);
        let def = Definition::new(loc.module, loc.span, name, doc, is_pub, kind);
        let defid = def.defid;
        let first = self.module_refs.definition(defid).is_none();
        self.module_refs.add_definition(def);
        if first {
            self.module_refs.add_reference(
                Some(defid),
                Reference::new(defid.span, ReferenceKind::Definition, defid),
            );
        }
    }

    /// Register a local binder as an `EntityKind::Value` graph definition so
    /// goto-def / find-refs / hover resolve on it and its uses. LSP-only: `al
    /// run`/`al check` never query locals and the dead-code pass ignores
    /// `Value` defs, so the `collect_hover_facts` gate keeps this off the hot
    /// path.
    fn emit_value_def(&mut self, sp: Span, name: &str, doc: Option<String>) {
        if self.collect_hover_facts {
            let m = self.current_module_slice();
            self.emit_def(
                DefinitionLocation::new(sp, m, EntityKind::Value),
                name,
                doc,
                false,
                DefinitionKind::Value { alias_of: None },
            );
        }
    }

    /// Thin wrappers over the engine's `mk_con` for prelude types, keyed by the
    /// captured `PreludeBindings` so type identity, not the name string, is the
    /// source of truth. The nullary ones go through the engine's per-primitive
    /// cache, so a recompile mints one `Int` node rather than one per literal.
    #[inline]
    fn ty_prelude(&mut self, r: TypeRef, args: &[Ty]) -> Ty {
        self.engine.mk_con(r.id, r.name, args)
    }
    #[inline]
    fn ty_nullary(&mut self, slot: NullaryPrim, r: TypeRef) -> Ty {
        self.engine.nullary_con(slot, r.id, r.name)
    }
    fn ty_bool(&mut self) -> Ty {
        self.ty_nullary(NullaryPrim::Bool, self.prelude.bool)
    }
    fn ty_int(&mut self) -> Ty {
        self.ty_nullary(NullaryPrim::Int, self.prelude.int)
    }
    fn ty_string(&mut self) -> Ty {
        self.ty_nullary(NullaryPrim::String, self.prelude.string)
    }
    fn ty_nil(&mut self) -> Ty {
        self.ty_nullary(NullaryPrim::Nil, self.prelude.nil)
    }
    fn ty_array(&mut self, elem: Ty) -> Ty {
        self.ty_prelude(self.prelude.array, &[elem])
    }
    fn ty_binary(&mut self) -> Ty {
        self.ty_nullary(NullaryPrim::Binary, self.prelude.binary)
    }
    fn ty_option(&mut self, inner: Ty) -> Ty {
        self.ty_prelude(self.prelude.option, &[inner])
    }
    fn ty_result(&mut self, ok: Ty, err: Ty) -> Ty {
        self.ty_prelude(self.prelude.result, &[ok, err])
    }

    /// Hydrate a type annotation through `h`, recording any error and falling
    /// back to a fresh tyvar so inference can continue. Every type name the
    /// hydrator resolved is drained as an `Unqualified` occurrence, whether
    /// hydration succeeded or not: a nested name can resolve before a later
    /// sibling fails. The target is the owning module's canonical `Type`
    /// definition from the reference graph, never the constructor-clobbered
    /// `env.definitions`.
    pub(super) fn hydrate(&mut self, h: &mut Hydrator, t: &ast::TypeIdentifier) -> Ty {
        let result = h.type_from_ast(t, &self.env, &mut self.engine);
        for hit in h.take_type_refs() {
            let path = self.engine.strs_of(hit.module);
            let name = self.engine.str(hit.name).to_string();
            self.record_type_use(&path, &name, hit.span, ReferenceKind::Unqualified);
        }
        match result {
            Ok(ty) => ty,
            Err(d) => {
                self.engine.diagnostics.push(d);
                self.engine.fresh_var()
            }
        }
    }

    /// Like [`Self::hydrate`] for an optional annotation: `None` yields a fresh tyvar.
    pub(super) fn hydrate_opt(&mut self, h: &mut Hydrator, t: Option<&ast::TypeIdentifier>) -> Ty {
        match t {
            Some(t) => self.hydrate(h, t),
            None => self.engine.fresh_var(),
        }
    }

    /// Hydrate a `let x: T = ...` binding annotation, where any type-variable
    /// name must already be in scope (i.e. a parameter of the enclosing fn).
    fn hydrate_annotation(&mut self, t: &ast::TypeIdentifier) -> Ty {
        let mut h = Hydrator::new(AnnotationContext::Binding);
        self.hydrate(&mut h, t)
    }

    pub(super) fn compile_node(&mut self, node: &ast::Node) -> Ty {
        match node {
            ast::Node::Statement(s) => {
                self.compile_statement(s);
                self.ty_nil()
            }
            ast::Node::Expression(e) => self.compile_expr(e),
        }
    }

    fn compile_statement(&mut self, stmt: &ast::Statement) {
        match stmt {
            ast::Statement::VariableBinding(vb) => {
                let name = vb.identifier.name.clone();
                let name_id = self.engine.intern(&name);

                // A module-scope rebind gets a fresh slot, so allocate it
                // *after* the initializer compiles: otherwise the name would
                // already resolve to the new, uninitialised slot and `x = x + 1`
                // would read garbage. The prior binding stays in scope across
                // the init, which also drives `Self_` resolution for a
                // self-recursive lambda. A first binding still reserves its slot
                // up front, so such a lambda can resolve its own name through
                // the entry frame.
                let defer_slot = self.outer_scopes.is_empty() && self.locals.contains_key(&name_id);
                if !defer_slot {
                    self.get_or_create_local(&name);
                }

                let annot_ty = vb.typ.as_ref().map(|a| self.hydrate_annotation(a));

                self.engine.enter_level();
                let self_ty = self.engine.fresh_var();
                let init_is_fn = matches!(vb.init, ast::Expression::FunctionExpression(_));
                // Only pre-bind for recursive lambdas: exposing the name to any
                // other initializer lets `x = x` infer ⊥ and generalize to ∀A.A,
                // which is unsound.
                if init_is_fn {
                    self.env.define(&name, mono(self_ty));
                }

                // One-shot, consumed by the lambda's own `enter_fn_frame`.
                // Mutating `current_binding` here instead leaks into nested and
                // HOF-arg lambdas, mis-resolving their calls to the enclosing fn
                // as `Self_` and emitting `CallSelf` against the wrong frame.
                let saved_self = if init_is_fn {
                    self.next_fn_self_name.replace(name_id)
                } else {
                    self.next_fn_self_name.take()
                };
                let init_ty = self.compile_expr_with_hint(&vb.init, annot_ty);
                self.next_fn_self_name = saved_self;

                self.engine.unify_at(self_ty, init_ty, vb.init.span());
                self.engine.leave_level();

                let final_ty = if let Some(a) = annot_ty {
                    self.engine
                        .unify_at(a, init_ty, type_defining_span(&vb.init));
                    a
                } else {
                    init_ty
                };

                let scheme = self.engine.generalize(final_ty);
                let m = self.current_module_slice();
                self.env.define_at(
                    &name,
                    scheme,
                    DefinitionLocation::new(vb.identifier.span, m, EntityKind::Value),
                );
                self.track_binding(&name, vb.identifier.span);
                if let Some(doc) = &vb.doc {
                    self.env.store_doc(&name, doc.clone());
                }
                self.record(&name, final_ty, vb.identifier.span, vb.doc.clone());
                self.emit_value_def(vb.identifier.span, &name, vb.doc.clone());

                if defer_slot {
                    self.get_or_create_local(&name);
                }
            }
            ast::Statement::TupleDestructuringBinding(tdb) => {
                let init_ty = self.compile_expr(&tdb.init);

                let mut elem_vars: Vec<Ty> = Vec::with_capacity(tdb.patterns.len());
                for _ in 0..tdb.patterns.len() {
                    elem_vars.push(self.engine.fresh_var());
                }
                let tup = self.engine.mk_tuple(&elem_vars);
                self.engine.unify_at(tup, init_ty, tdb.init.span());

                let mut b = PatternBindings::new();
                for (i, pattern) in tdb.patterns.iter().enumerate() {
                    // No usefulness check runs here — the refutability check
                    // below is syntactic, so an ill-typed pattern is harmless.
                    let _ = self.type_pattern(pattern, elem_vars[i], &mut b.sink());
                }
                self.bind_pattern_initials(&b);

                for pattern in &tdb.patterns {
                    self.type_pattern_sizes(pattern);
                    if pattern_is_refutable(pattern) {
                        self.error(
                            "Destructuring binding pattern must be irrefutable".to_string(),
                            pattern.span(),
                        );
                    }
                }
            }
            ast::Statement::Declaration { decl, .. } => {
                // Top-level declarations are handled by `analyse_module`; reaching
                // one here means it's nested inside an expression block.
                let (kind, sp) = match decl.as_ref() {
                    ast::Declaration::Function(fd) => ("named function", fd.span),
                    ast::Declaration::Type(td) => ("type", td.span),
                    ast::Declaration::Const(cb) => ("const", cb.span),
                };
                self.error(
                    format!("{kind} declarations are only allowed at the top level"),
                    sp,
                );
            }
            ast::Statement::TypedDiscard(td) => {
                // `UpperIdent = expr`: assert the init has the named 0-arg type
                // and discard the value. If the name resolves to a constructor
                // but not a type (`Some = ...`), say so directly rather than
                // letting the hydrator report "Unknown type".
                let name = &td.ty_name.name;
                let expected = if self.env.lookup_type_info(name).is_none()
                    && matches!(
                        self.env.lookup(name).map(|s| s.kind),
                        Some(ValueKind::Constructor { .. })
                    ) {
                    self.error(format!("'{name}' is not a type"), td.ty_name.span);
                    self.engine.fresh_var()
                } else {
                    // As a no-arg annotation: the hydrator rejects unknown
                    // names and arity misuse, and records the type occurrence.
                    let ann = ast::TypeIdentifier {
                        kind: ast::TypeKind::NamedType(ast::NamedType {
                            identifier: td.ty_name.clone(),
                            type_args: Vec::new(),
                        }),
                        span: td.ty_name.span,
                    };
                    self.hydrate_annotation(&ann)
                };
                let init_ty = self.compile_expr_with_hint(&td.init, Some(expected));
                self.engine
                    .unify_at(expected, init_ty, type_defining_span(&td.init));
            }
            ast::Statement::CtorDestructuringBinding(cdb) => {
                // `Ctor(p1, ..) = expr`: an irrefutable single-arm destructure,
                // typed like a one-arm match and then required to be
                // exhaustive, so only single-constructor types qualify.
                let init_ty = self.compile_expr(&cdb.init);
                let pattern = cdb.as_pattern();

                let mut b = PatternBindings::new();
                let typed_ok = self.type_pattern(&pattern, init_ty, &mut b.sink());

                self.bind_pattern_initials(&b);
                self.type_pattern_sizes(&pattern);

                if typed_ok {
                    let resolved = self.engine.resolve(init_ty, Some(&self.env));
                    let mut um = UsefulnessMatrix::new(resolved);
                    let pat = um.lower(&pattern);
                    if let Some(missing) = um.find_missing(&[pat]) {
                        self.error(
                            format!(
                                "constructor destructuring binding must be irrefutable; \
                                 pattern does not cover {missing}"
                            ),
                            cdb.span,
                        );
                    }
                }
            }
            ast::Statement::ImportDeclaration(_) => {
                // Already resolved (or rejected) by `process_imports` before the
                // body walk.
            }
        }
    }

    pub(super) fn process_imports(&mut self, block: &ast::BlockExpression) {
        for node in &block.body {
            let ast::Node::Statement(stmt) = node else {
                break;
            };
            let ast::Statement::ImportDeclaration(imp) = stmt.as_ref() else {
                break;
            };
            self.process_import(imp);
        }
    }

    /// A cache-hit dependency reserved its type-id range without contributing to
    /// `next_type_id` this pass, so bump past every reserved block before the
    /// entry file allocates ids of its own.
    pub(super) fn bump_type_ids_past_reserved(&mut self) {
        let hw = self.module_table.id_high_water();
        let cur = self.env.next_type_id();
        if hw > cur {
            self.env.set_next_type_id(hw);
        }
    }

    fn process_import(&mut self, imp: &ast::ImportDeclaration) {
        // `canon` is the module's identity: the file it resolved to. Every
        // downstream key must use it, never `imp.path`, which is only how this
        // file spelled it.
        let Some((canon, key)) = self.load_module(&imp.path, imp.span) else {
            return;
        };
        self.module_display
            .insert(key.clone(), imp.path.to_string());

        // The default qualifier is the last module-name segment; relative
        // markers live in `path.leading`, so they can't be picked up here.
        let qualifier = imp
            .alias
            .as_ref()
            .map(|a| a.name.clone())
            .unwrap_or_else(|| {
                imp.path
                    .names
                    .last()
                    .cloned()
                    .unwrap_or_else(|| key.as_str().to_string())
            });

        // The qualifier binding is a `ModuleAlias` definition; the final
        // module-name segment is an `Import` occurrence and the `as` alias an
        // `Alias` occurrence, both targeting it, so qualified uses, find-refs
        // and rename resolve back to this import. The `Import` occ sits at
        // `imp.path_span` — the final segment only — so the `import` keyword and
        // earlier segments are not clickable. The alias `Definition` also stores
        // the declaration span and imported `ModuleId` so unused-import
        // detection reads them directly.
        let alias_span = imp.alias.as_ref().map_or(imp.span, |a| a.span);
        let cur_path = self.current_module.clone();
        let cur_mid = self.ref_interner.intern(&cur_path);
        let imported_mid = self.ref_interner.intern(&canon);
        let alias_defid = DefId::new(cur_mid, alias_span, EntityKind::ModuleAlias);
        self.module_refs.add_definition(Definition::new(
            cur_mid,
            alias_span,
            qualifier.clone(),
            None,
            false,
            DefinitionKind::ModuleAlias {
                decl_span: imp.span,
                imports_module: Some(imported_mid),
            },
        ));
        // Targets a `DefId` owned by the *imported* module, so goto-def on the
        // `b` in `import a/b` lands on `b`'s file rather than self-jumping to
        // the alias binding. find-references, rename and the unused-import rule
        // all key off the alias `Definition` above and are unaffected.
        self.module_refs.add_reference(
            None,
            Reference::new(
                imp.path_span,
                ReferenceKind::Import,
                DefId::new(imported_mid, imp.path_span, EntityKind::ModuleAlias),
            ),
        );
        if let Some(a) = imp.alias.as_ref() {
            self.module_refs.add_reference(
                None,
                Reference::new(a.span, ReferenceKind::Alias, alias_defid),
            );
        }

        self.imported_qualifiers.insert(qualifier, key.clone());

        // Collected as owned values while `iface` borrows `module_table`, then
        // turned into `DefId`s once that borrow has ended.
        let mut item_refs: Vec<(Span, ReferenceKind, DefinitionLocation)> = Vec::new();
        let mut type_item_refs: Vec<(Span, ReferenceKind, String)> = Vec::new();
        for item in &imp.items {
            let local_name = item
                .alias
                .as_ref()
                .map(|a| a.name.clone())
                .unwrap_or_else(|| item.name.name.clone());
            let Some(iface) = self.module_table.get(&key) else {
                continue;
            };
            // A record `type Foo {..}` exports both a same-named constructor
            // value and the type itself. They are not mutually exclusive: bind
            // and record each independently, or the type branch is unreachable
            // for every record type because the value always matches first.
            let val = iface.values.get(&item.name.name).cloned();
            let typ = iface.types.get(&item.name.name).cloned();
            let is_private = iface.private_names.contains(&item.name.name);
            if let Some(ev) = val.clone() {
                let vdef = ev.scheme.def;
                // The `X` token of `{X}` / `{X as Y}` always names the imported
                // The `X` of `{X}` / `{X as Y}` always names the imported
                // symbol, so it targets X's canonical def: goto-def chains to
                // the real declaration and renaming X rewrites it. A binding
                // token, not an evaluating use, hence `ImportItem`.
                    item_refs.push((item.name.span, ReferenceKind::ImportItem, dl));
                }
                // `{X as Y}` introduces a new local name. Y gets its own
                // `DefId` so its rename class is separate from X's: sharing X's
                // made renaming X rewrite Y's binder and every Y use, and made
                // renaming a Y use escape into X's module. Type, kind and slot
                // are still inherited from X. The `Value` entity keeps Y off the
                // dead-code surface and out of the symbol outline.
                if let Some(a) = item.alias.as_ref() {
                    let module = self.current_module_slice();
                    let alias_dl = DefinitionLocation::new(a.span, module, EntityKind::Value);
                    self.env.define(
                        &local_name,
                        Scheme {
                            def: Some(alias_dl),
                            ..ev.scheme
                        },
                    );
                    let alias_defid = self.defid_of(alias_dl);
                    let canonical = vdef.map(|dl| self.defid_of(dl));
                    // goto-def / hover on Y chains to X via `alias_of`; rename
                    // and find-references stay on this alias.
                    let def = Definition::new(
                        alias_defid.module,
                        alias_defid.span,
                        local_name.clone(),
                        None,
                        false,
                        DefinitionKind::Value {
                            alias_of: canonical,
                        },
                    );
                    self.module_refs.add_definition(def);
                    item_refs.push((a.span, ReferenceKind::Alias, alias_dl));
                } else {
                    self.env.define(&local_name, ev.scheme);
                }
                if let Some(slot) = ev.local_slot {
                    let id = self.engine.intern(&local_name);
                    self.bind_local(id, slot.0);
                }
            }
            if let Some(ti) = typ {
                // Bound unconditionally so annotation hydration resolves it
                // through this module's env rather than residual global
                // `type_info` from the dependency's own compile. Its canonical
                // `Type` `DefId` is resolved after the loop from the imported
                // module's reference graph.
                self.env.store_type_info(&local_name, ti);
                type_item_refs.push((
                    item.name.span,
                    ReferenceKind::ImportItem,
                    item.name.name.clone(),
                ));
                if let Some(a) = item.alias.as_ref() {
                    type_item_refs.push((a.span, ReferenceKind::Alias, item.name.name.clone()));
                }
            }
            if val.is_none() && typ.is_none() {
                if is_private {
                    self.error(
                        format!(
                            "'{}' is private in module '{}'",
                            item.name.name,
                            self.module_name(&key)
                        ),
                        item.name.span,
                    );
                } else {
                    self.error(
                        format!(
                            "Module '{}' has no member '{}'",
                            self.module_name(&key),
                            item.name.name
                        ),
                        item.name.span,
                    );
                }
            }
        }
        for (occ, kind, dl) in item_refs {
            let target = self.defid_of(dl);
            self.record_ref(occ, kind, target);
        }
        for (occ, kind, tyname) in type_item_refs {
            // The declaring module is the *resolved* file: looking it up under
            // `imp.path` as written would miss the cache for a relative import
            // and silently record nothing.
            self.record_type_use(&canon, &tyname, occ, kind);
        }
    }

    /// How to name a module in a diagnostic: the path the importer wrote
    /// (`./lib`), falling back to the module's own last segment. Never the
    /// canonical key — that is an identity, not a name.
    pub(super) fn module_name(&self, key: &ModuleKey) -> String {
        self.module_display.get(key).cloned().unwrap_or_else(|| {
            let k = key.as_str();
            k.rsplit('/').next().unwrap_or(k).to_string()
        })
    }

    /// Load the module `path` names, relative to the importing module's
    /// directory, and return its canonical identity. Resolution happens before
    /// the cache is consulted and the key comes from the file we resolved to,
    /// never from `path` as written: keying on the written path made `./b` mean
    /// one module program-wide.
    pub(crate) fn load_module(
        &mut self,
        path: &ast::ImportPath,
        at: Span,
    ) -> Option<(ModulePath, ModuleKey)> {
        let resolved = match module::resolve(path, self.base_dir.as_deref()) {
            Ok(r) => r,
            // These two get richer, import-syntax-aware guidance than
            // `ResolveError`'s shared Display wording provides.
            Err(ResolveError::BareName(p)) => {
                self.module_error(
                    format!(
                        "Unknown module '{p}' — package imports are not yet supported; \
                         use a relative path like `./{p}`"
                    ),
                    at,
                );
                return None;
            }
            Err(ResolveError::NoBaseDir) => {
                self.module_error(
                    "Relative imports are not allowed without a file context (e.g. REPL)"
                        .to_string(),
                    at,
                );
                return None;
            }
            Err(e) => {
                self.module_error(e.to_string(), at);
                return None;
            }
        };

        // The identity of an on-disk module is the file it resolved to;
        // `resolve` already minted the canonical path + key from it.
        let module::ResolvedModule { source, canon, key } = resolved;
        let importer = self.current_module_key.clone();
        if self.module_table.get_or_hydrate(&key).is_some() {
            self.module_table.record_dependent(&key, &importer);
            return Some((canon, key));
        }
        if self.module_table.is_loading(&key) {
            self.module_error(format!("Import cycle detected at module '{key}'"), at);
            return None;
        }

        let (text, child_base, source_path): (String, Option<PathBuf>, Option<PathBuf>) =
            match source {
                ModuleSource::Embedded(s) => (s.to_string(), None, None),
                ModuleSource::File(p) => match self.module_table.read_source(&p) {
                    Ok(t) => (t, p.parent().map(|d| d.to_path_buf()), Some(p)),
                    Err(e) => {
                        self.module_error(format!("Failed to read module '{key}': {e}"), at);
                        return None;
                    }
                },
            };

        let hash = source_hash(&text);
        let mut sc = crate::scanner::new_scanner(text);
        let parser = crate::parser::new_parser(&mut sc);
        let parsed = parser.parse_program();
        for d in parsed.diagnostics {
            // The span is `at` — the import site in the *importing* module —
            // so provenance is left for the importer's own stamping pass.
            self.engine.diagnostics.push(Diagnostic {
                span: at,
                severity: d.severity,
                code: d.code,
                message: format!("In module '{key}': {}", d.message),
                source: None,
            });
        }

        self.module_table.mark_loading(&key);
        let (body, imports) = self.compile_module_body(
            canon.clone(),
            key.clone(),
            &parsed.ast,
            parsed.doc,
            child_base,
        );
        let refs = Rc::new(body.refs);
        // Captured after this module's dependencies have loaded but before its
        // own body added anything. Only on-disk modules are ever invalidated, so
        // only they carry one.
        let origin = match source_path {
            Some(path) => ModuleOrigin::File {
                source_hash: hash,
                stat: None,
                watermark: body.watermark,
                path,
                refs,
            },
            None => ModuleOrigin::Embedded { refs },
        };
        self.module_table.bump_compile_count();
        self.module_table.insert_cached(
            key.clone(),
            CachedModule {
                iface: body.iface,
                origin,
                dependents: HashSet::new(),
            },
        );
        for imp in imports {
            self.module_table.record_dependent(&imp, &key);
        }
        self.module_table.record_dependent(&key, &importer);
        Some((canon, key))
    }

    /// Snapshot the enclosing module's per-module state and swap in fresh state
    /// for compiling `path`'s body, including a fresh `ModuleReferences`
    /// collector for `leave_module_frame` to hand back.
    fn enter_module_frame(
        &mut self,
        path: ModulePath,
        key: ModuleKey,
        base_dir: Option<PathBuf>,
    ) -> ModuleFrame {
        let mid = self.ref_interner.intern(&path);
        ModuleFrame {
            module: std::mem::replace(&mut self.current_module, path),
            module_key: std::mem::replace(&mut self.current_module_key, key),
            module_path_slice: self.module_path_slice.take(),
            imported_qualifiers: std::mem::take(&mut self.imported_qualifiers),
            base_dir: std::mem::replace(&mut self.base_dir, base_dir),
            module_refs: std::mem::replace(&mut self.module_refs, ModuleReferences::new(mid)),
        }
    }

    /// Restore the snapshotted per-module state and return the just-compiled
    /// module's collected references.
    fn leave_module_frame(&mut self, old: ModuleFrame) -> ModuleReferences {
        let ModuleFrame {
            module,
            module_key,
            module_path_slice,
            imported_qualifiers,
            base_dir,
            module_refs,
        } = old;
        self.current_module = module;
        self.current_module_key = module_key;
        self.module_path_slice = module_path_slice;
        self.imported_qualifiers = imported_qualifiers;
        self.base_dir = base_dir;
        std::mem::replace(&mut self.module_refs, module_refs)
    }

    fn compile_module_body(
        &mut self,
        path: ModulePath,
        key: ModuleKey,
        block: &ast::BlockExpression,
        doc: Option<String>,
        base_dir: Option<PathBuf>,
    ) -> (CompiledBody, Vec<ModuleKey>) {
        let mut iface = ModuleInterface::new(path.clone());
        iface.doc = doc.clone();
        // Diagnostics pushed between here and `leave_module_frame` have spans in
        // this module's text, not the entry file's.
        let diag_mark = self.engine.diagnostics.len();
        let old = self.enter_module_frame(path, key.clone(), base_dir);
        // The collector installed above is this module's, so its doc goes on it.
        self.module_refs.set_doc(doc);

        self.process_imports(block);
        let imports: Vec<ModuleKey> = self.imported_qualifiers.values().cloned().collect();
        // Reserve (or reuse) this module's stable 256-aligned type-id range
        // *before* the watermark is captured, so the watermark's `next_type_id`
        // is `base`. On invalidation `reset_to` restores it from that watermark,
        // putting the module back at its own range start, so editing one module
        // never shifts another's ids. Deps have already reserved their ranges,
        // so the floor sits past every stdlib and dependency id.
        let floor = self.env.next_type_id();
        let reservation = self.module_table.id_base_for(&key, floor);
        let base = reservation.base();
        self.env.set_next_type_id(base);
        let watermark = self.watermark();
        // Reset so they carry only this module's binds, not the dependencies'
        // that `process_imports` just compiled.
        self.toplevel_binds.clear();
        self.toplevel_decls.clear();
        let code_mark = self.program.code.len();
        let slot_base = self.local_count;
        self.env.push_scope();
        self.analyse_module(block, Some(&mut iface));
        self.emit_module_init(&key, block, code_mark, slot_base);
        self.env.pop_scope();
        // Record real type-id consumption, so `id_high_water` tracks usage and a
        // spill past `MODULE_TYPE_ID_RANGE` raises `id_range_overflow`.
        let used = self.env.next_type_id().0 - base.0;
        reservation.note_usage(&mut self.module_table, used);

        let refs = self.leave_module_frame(old);
        // Stamp provenance on this module's diagnostics. Dependencies compiled
        // inside our range already stamped theirs, so only unstamped ones are
        // ours.
        for d in &mut self.engine.diagnostics[diag_mark..] {
            if d.source.is_none() {
                d.source = Some(key.clone());
            }
        }
        (
            CompiledBody {
                iface,
                watermark,
                refs,
            },
            imports,
        )
    }

    /// Perceus-optimise a just-lowered toplevel and append its Core-derived
    /// entry-frame init. Shared by `__main__` and every imported module; see
    /// [`TopKind`] for how the two callers differ.
    ///
    /// The analysis pass already laid the toplevel's function bodies down in
    /// `[code_mark, base)`, each behind a jump-over. Overwriting the first of
    /// those with `Jump base` skips the whole run in one hop and lands on the
    /// init. The bodies keep their `Function.code_start` addresses and stay
    /// reachable via `CallKnown`; truncating instead would orphan all of them.
    fn append_toplevel_init(
        &mut self,
        lowered: LoweredBody,
        code_mark: usize,
        slot_base: i32,
        kind: TopKind,
    ) {
        use crate::core_ir::{emit, perceus};
        // The elaboration drained the queue the check walk filled. A leftover
        // means the two walks disagreed about which statements bind at module
        // scope, which silently mis-slots every bind after that point.
        if !self.toplevel_binds.is_empty() {
            unclaimed_toplevel_slots(self.toplevel_binds.len());
        }
        let LoweredBody { core: top, pool } = lowered;
        // Perceus so temporaries passed into calls are moved. `emit_toplevel`
        // suppresses the resulting `Drop`/`Reuse` for the pinned globals, whose
        // last use in the toplevel is not their last use in the program.
        let top = perceus::perceus(&pool, top);
        if std::env::var("CORE_DBG").is_ok() {
            eprintln!("=== {}\n{top}", self.engine.str(top.name));
        }
        let base = self.program.code.len() as i32;
        let mut out = emit::emit_toplevel(&top.body, slot_base, self);
        // A function body links by plain append because its block starts at its
        // own `Function.code_start`. This one does not: it is spliced into the
        // entry frame, whose `code_start` is 0, but starts at `base`. The VM
        // resolves jumps as `0 + operand`, so rebase by `base`. The one place an
        // operand is ever rewritten.
        emit::relocate(&mut out.code, base);
        self.local_count = self.local_count.max(out.locals);
        if code_mark < base as usize {
            self.program.code[code_mark] = op_arg(Op::Jump, base);
        }
        self.program.code.extend(out.code);
        match kind {
            TopKind::Module => self.program.code.push(op(Op::Pop)),
            TopKind::Entry => self.core.toplevel = top.body,
        }
    }

    /// Elaborate and lower a just-analysed module toplevel to Core and append
    /// its entry-frame init, exactly as `compile_impl` does for `__main__`.
    fn emit_module_init(
        &mut self,
        key: &ModuleKey,
        block: &ast::BlockExpression,
        code_mark: usize,
        slot_base: i32,
    ) {
        // Taken, not merely read: the next module's toplevel has `Span`s from its
        // own file and must not find one of ours at the same offset. Dropped on
        // the early return too — an unelaborated toplevel has no reader.
        let sites = std::mem::take(&mut self.frame_closures);
        // This module's toplevel walk region, drained whether or not it is
        // elaborated: the next module's walk must start from an empty one.
        let walk_tys = self.take_toplevel_walk_tys();
        let Some(clean) = self.clean_module() else {
            return;
        };
        self.frame_closures = sites;
        let name = self.engine.intern(key.as_str());
        let nil = self.ty_nil();
        // Elaborated and lowered under `check_only` too (emit is not), so a
        // toplevel the front half cannot handle is a diagnostic rather than a
        // crash at `al run`. The wrappers go down before anyone reads an
        // address, here `append_toplevel_init`'s `base`.
        let lowered = self.elaborate_then_materialize(clean, None, |c, pool, fns| {
            typed_ir::elaborate_toplevel(c, pool, fns, name, block, nil, &walk_tys)
        });
        self.frame_closures.clear();
        if !self.check_only {
            self.append_toplevel_init(lowered, code_mark, slot_base, TopKind::Module);
        }
    }

    /// Look up `qualifier.member` for an imported module. Returns the scheme and,
    /// when the member has a runtime binding, its entry-frame slot.
    fn lookup_module_member(
        &mut self,
        module_key: &ModuleKey,
        member: &str,
        member_span: Span,
    ) -> Option<(Scheme, Option<GlobalSlot>)> {
        let Some(iface) = self.module_table.get_or_hydrate(module_key) else {
            self.module_error(format!("Module '{module_key}' is not loaded"), member_span);
            return None;
        };
        if let Some(ev) = iface.values.get(member).cloned() {
            return Some((ev.scheme, ev.local_slot));
        }
        let private = iface.private_names.contains(member);
        let name = self.module_name(module_key);
        if private {
            self.error(
                format!("'{member}' is private in module '{name}'"),
                member_span,
            );
        } else {
            self.error(
                format!("Module '{name}' has no member '{member}'"),
                member_span,
            );
        }
        None
    }

    /// Typecheck `expr`, appending the type it inferred to the current walk
    /// region in entry order.
    ///
    /// Must stay total over expressions: the elaborator reads this instead of
    /// re-deriving a type, and enters exactly the expressions this walk entered,
    /// in the same order. See [`Compiler::walk_tys`].
    pub(super) fn compile_expr(&mut self, expr: &ast::Expression) -> Ty {
        let slot = self.reserve_walk_ty();
        let ty = self.compile_expr_inner(expr);
        self.fill_walk_ty(slot, ty);
        ty
    }

    /// Claim this expression's position in the current region before its
    /// children claim theirs, so the region is a pre-order listing.
    fn reserve_walk_ty(&mut self) -> usize {
        self.walk_tys.push(WalkStep::Ty(WALK_TY_PENDING));
        self.walk_tys.len() - 1
    }

    /// Fill the slot [`Self::reserve_walk_ty`] claimed. Indexes rather than
    /// probes: a slot outside the current region is the desync this table exists
    /// to prevent, and swallowing the write would let a `WALK_TY_PENDING` reach
    /// the elaborator.
    fn fill_walk_ty(&mut self, slot: usize, ty: Ty) {
        self.walk_tys[slot] = WalkStep::Ty(ty);
    }

    /// Record the check walk's `module.member` verdict for the `left.right`
    /// shape. `Elab::qualified` consumes it and must not re-derive it.
    fn record_walk_qualified(&mut self, qualified: bool) {
        self.walk_tys.push(WalkStep::Qualified(qualified));
    }

    /// Park the enclosing body's region and start a fresh one for the body about
    /// to be walked.
    fn open_walk_region(&mut self) {
        let outer = std::mem::take(&mut self.walk_tys);
        self.walk_tys_stack.push(outer);
    }

    /// Close the region [`Self::open_walk_region`] opened, returning it and
    /// restoring the enclosing body's.
    fn close_walk_region(&mut self) -> Vec<WalkStep> {
        let Some(outer) = self.walk_tys_stack.pop() else {
            walk_region_underflow();
        };
        let region = std::mem::replace(&mut self.walk_tys, outer);
        debug_assert!(
            walk_region_is_filled(&region),
            "walk region closed with an unfilled slot"
        );
        region
    }

    /// Take the module toplevel's region: everything typed outside a function
    /// body. One elaboration drains it, so the next module starts empty.
    fn take_toplevel_walk_tys(&mut self) -> Vec<WalkStep> {
        debug_assert!(
            self.walk_tys_stack.is_empty(),
            "a function body's walk region leaked into the module toplevel's"
        );
        let region = std::mem::take(&mut self.walk_tys);
        debug_assert!(
            walk_region_is_filled(&region),
            "module toplevel walk region has an unfilled slot"
        );
        region
    }

    fn compile_expr_inner(&mut self, expr: &ast::Expression) -> Ty {
        match expr {
            ast::Expression::BinaryLiteral(bl) => self.compile_binary_literal(bl),
            ast::Expression::BlockExpression(be) => {
                self.push_block_scope();
                let mut last_ty = self.ty_nil();

                for (i, node) in be.body.iter().enumerate() {
                    let is_last = i == be.body.len() - 1;
                    let ty = self.compile_node(node);

                    if is_last {
                        last_ty = ty;
                    } else if matches!(node, ast::Node::Expression(_)) {
                        let resolved = self.engine.find(ty);
                        let is_nil = matches!(
                            self.engine.node(resolved),
                            TypeNode::Con { id, .. } if self.prelude.nil.is(id)
                        );
                        let is_var = matches!(self.engine.node(resolved), TypeNode::Var(_));
                        if !is_nil && !is_var {
                            let ty_str = self.engine.type_to_str(ty);
                            self.error(
                                format!(
                                    "Expression of type '{ty_str}' must be consumed. Assign it to a variable or use '_ = ...' to discard"
                                ),
                                node.span(),
                            );
                        }
                    }
                }

                self.pop_block_scope();
                last_ty
            }
            ast::Expression::NumberLiteral(nl) => {
                // `const_number` is the single source of the int/float split and
                // of the overflow diagnostic; the pooled `Value` itself is
                // re-interned by the Core emit.
                let v = self.const_number(nl);
                if v.is_float() {
                    self.engine.icon_float()
                } else {
                    self.ty_int()
                }
            }
            ast::Expression::StringLiteral(_) => self.ty_string(),
            ast::Expression::InterpolatedString(is) => {
                for part in &is.parts {
                    if let ast::InterpPart::Expr(e) = part {
                        self.compile_expr(e);
                    }
                }
                self.ty_string()
            }
            ast::Expression::Identifier(id) => self.compile_identifier(id),
            ast::Expression::BinaryExpression(be) => self.compile_binary(be),
            ast::Expression::UnaryExpression(ue) => {
                let inner_ty = self.compile_expr(&ue.expression);
                match ue.op {
                    ast::UnaryOp::Not => {
                        let b = self.ty_bool();
                        self.engine.unify_at(b, inner_ty, ue.expression.span());
                        self.ty_bool()
                    }
                    ast::UnaryOp::Neg => {
                        let result = self.engine.fresh_constrained_var(Constraint::Numeric);
                        self.engine.unify_at(result, inner_ty, ue.expression.span());
                        result
                    }
                }
            }
            ast::Expression::IfExpression(ie) => {
                let cond_ty = self.compile_expr(&ie.condition);
                let b = self.ty_bool();
                self.engine.unify_at(b, cond_ty, ie.condition.span());
                let then_ty = self.compile_expr(&ie.body);
                let else_ty = self.compile_expr(&ie.else_body);

                let ret = self.engine.fresh_var();
                self.engine
                    .unify_at(ret, then_ty, type_defining_span(&ie.body));
                self.engine
                    .unify_at(ret, else_ty, type_defining_span(&ie.else_body));
                ret
            }
            ast::Expression::MatchExpression(me) => self.compile_match(me),
            ast::Expression::ArrayExpression(ae) => self.compile_array(ae),
            ast::Expression::TupleExpression(te) => {
                let mut elem_tys: Vec<Ty> = Vec::with_capacity(te.elements.len());
                for elem in &te.elements {
                    elem_tys.push(self.compile_expr(elem));
                }
                self.engine.mk_tuple(&elem_tys)
            }
            ast::Expression::ArrayIndexExpression(aie) => {
                let arr_ty = self.compile_expr(&aie.expression);
                let elem_var = self.engine.fresh_var();
                let arr_expected = self.ty_array(elem_var);
                self.engine
                    .unify_at(arr_expected, arr_ty, aie.expression.span());

                if let ast::Expression::RangeExpression(r) = aie.index.as_ref() {
                    let start_ty = self.compile_expr(&r.start);
                    let end_ty = self.compile_expr(&r.end);
                    let int_t = self.ty_int();
                    self.engine.unify_at(int_t, start_ty, r.start.span());
                    self.engine.unify_at(int_t, end_ty, r.end.span());
                    self.ty_array(elem_var)
                } else {
                    let idx_ty = self.compile_expr(&aie.index);
                    let int_t = self.ty_int();
                    self.engine.unify_at(int_t, idx_ty, aie.index.span());
                    self.ty_option(elem_var)
                }
            }
            ast::Expression::RangeExpression(re) => {
                let start_ty = self.compile_expr(&re.start);
                let end_ty = self.compile_expr(&re.end);
                let int_t = self.ty_int();
                self.engine.unify_at(int_t, start_ty, re.start.span());
                self.engine.unify_at(int_t, end_ty, re.end.span());
                let i = self.ty_int();
                self.ty_array(i)
            }
            ast::Expression::FunctionExpression(fe) => {
                self.engine.enter_level();
                let ty = self.compile_function_common(
                    &fe.params,
                    &fe.body,
                    fe.return_type.as_ref(),
                    None,
                    fe.span,
                );
                self.engine.leave_level();
                ty
            }
            ast::Expression::FunctionCallExpression(fc) => self.compile_call(fc),
            ast::Expression::PropertyAccessExpression(pa) => self.compile_property_access(pa),
            ast::Expression::OrExpression(oe) => self.compile_or(oe),
            ast::Expression::ErrorNode(err) => self.error_node(err),
        }
    }

    /// An `ErrorNode` stands in for a region the parser could not read, so there
    /// is nothing to check and nothing to elaborate. The parser already reported
    /// it, but the entry file's parse diagnostics never enter
    /// `engine.diagnostics`, so restate it here — and only when nothing else has
    /// already denied the module its [`CleanModule`] proof, which keeps the
    /// diagnostic set a user sees identical.
    fn error_node(&mut self, err: &ast::ErrorNode) -> Ty {
        if self.clean_module().is_some() {
            self.engine.diagnostics.push(Diagnostic::error(
                err.span,
                DiagnosticCode::ParseError,
                err.message.clone(),
            ));
        }
        self.engine.fresh_var()
    }

    /// Compile an expression with an optional expected-type hint. Consulted only
    /// for fn-literal arguments, so an unannotated lambda param gets a concrete
    /// type immediately instead of "cannot access field on unknown type".
    pub(super) fn compile_expr_with_hint(
        &mut self,
        expr: &ast::Expression,
        hint: Option<Ty>,
    ) -> Ty {
        if let Some(h) = hint
            && let ast::Expression::FunctionExpression(fe) = expr
        {
            let resolved = self.engine.find(h);
            if let TypeNode::Fun { params, .. } = self.engine.node(resolved)
                && params.len as usize == fe.params.len()
            {
                let params: Vec<Ty> = self.engine.children_of(params).to_vec();
                // This path bypasses `compile_expr`, so claim the lambda's walk
                // slot here, before its body opens a region of its own, or the
                // elaborator's next `take_ty` reads the following expression.
                let slot = self.reserve_walk_ty();
                self.engine.enter_level();
                let ty = self.compile_function_common(
                    &fe.params,
                    &fe.body,
                    fe.return_type.as_ref(),
                    Some(params.clone()),
                    fe.span,
                );
                self.engine.leave_level();
                self.fill_walk_ty(slot, ty);
                return ty;
            }
        }
        self.compile_expr(expr)
    }

    /// Does this value reference have a runtime binding? Resolving one carries
    /// the usual `mark_used` and capture side effects; constructors and builtins
    /// have no plain runtime binding.
    fn has_binding(&mut self, name: &str, kind: &ValueKind) -> bool {
        match kind {
            ValueKind::Local | ValueKind::ModuleFn => {
                let id = self.engine.intern(name);
                self.resolve_variable(id).is_some()
            }
            _ => false,
        }
    }

    fn compile_identifier(&mut self, expr: &ast::Identifier) -> Ty {
        let name = &expr.name;

        let Some(scheme) = self.env.lookup(name) else {
            if let Some(suggestion) = self.env.suggest_name(name) {
                self.error(
                    format!(
                        "Unknown identifier '{}'. Did you mean '{}'?",
                        name, suggestion
                    ),
                    expr.span,
                );
            } else {
                self.error(format!("Unknown identifier '{}'", name), expr.span);
            }
            return self.engine.fresh_var();
        };

        let ty = self.engine.instantiate(scheme, &self.rigid_ids);
        let kind = scheme.kind;
        let def = scheme.def;
        // `record` discards this off the LSP path, so skip the probe there.
        let doc = self.doc_if_collecting(name);
        self.record(name, ty, expr.span, doc);
        self.record_value_use(def, expr.span, ReferenceKind::Unqualified);

        let bound = self.has_binding(name, &kind);
        self.check_named_value(name, None, expr.span, kind, bound);
        ty
    }

    fn no_runtime_binding(&mut self, name: &str, qualifier: Option<&str>, sp: Span) {
        let msg = match qualifier {
            Some(m) => format!("'{name}' in module '{m}' has no runtime binding"),
            None => format!("'{name}' has a type but no runtime binding here"),
        };
        self.error(msg, sp);
    }

    /// Diagnose a named value used in value position. Constructors and builtins
    /// are legal — the elaborator synthesises a nullary build or an eta wrapper
    /// — but a `Local`/`ModuleFn` with no runtime binding is an error.
    fn check_named_value(
        &mut self,
        name: &str,
        qualifier: Option<&str>,
        sp: Span,
        kind: ValueKind,
        has_binding: bool,
    ) {
        match kind {
            ValueKind::Constructor { .. } | ValueKind::Builtin { .. } => {}
            ValueKind::Local | ValueKind::ModuleFn => {
                if !has_binding {
                    self.no_runtime_binding(name, qualifier, sp);
                }
            }
        }
    }

    fn compile_binary(&mut self, expr: &ast::BinaryExpression) -> Ty {
        use ast::BinaryOp;
        let (constraint, is_cmp) = match expr.op {
            BinaryOp::And | BinaryOp::Or => {
                let l_ty = self.compile_expr(&expr.left);
                let b = self.ty_bool();
                self.engine.unify_at(b, l_ty, expr.left.span());
                let r_ty = self.compile_expr(&expr.right);
                self.engine.unify_at(b, r_ty, expr.right.span());
                return self.ty_bool();
            }
            BinaryOp::Eq | BinaryOp::Ne => {
                let l_ty = self.compile_expr(&expr.left);
                let r_ty = self.compile_expr(&expr.right);
                self.engine.unify_at(l_ty, r_ty, expr.span);
                return self.ty_bool();
            }
            BinaryOp::Add => (Constraint::Addable, false),
            BinaryOp::Sub => (Constraint::Numeric, false),
            BinaryOp::Mul => (Constraint::Numeric, false),
            BinaryOp::Div => (Constraint::Numeric, false),
            BinaryOp::Mod => (Constraint::Numeric, false),
            BinaryOp::Lt | BinaryOp::Gt | BinaryOp::Le | BinaryOp::Ge => {
                (Constraint::Numeric, true)
            }
        };
        let l_ty = self.compile_expr(&expr.left);
        let r_ty = self.compile_expr(&expr.right);
        let operand = self.engine.fresh_constrained_var(constraint);
        self.engine.unify_at(operand, l_ty, expr.left.span());
        self.engine.unify_at(operand, r_ty, expr.right.span());
        if is_cmp { self.ty_bool() } else { operand }
    }

    /// Type-check a `<<v:size:kind, ..>>` literal. Each segment's value and its
    /// runtime size are checked; the encoding itself is Core's job.
    fn compile_binary_literal(&mut self, bl: &ast::BinaryLiteral) -> Ty {
        for seg in &bl.segments {
            let vty = self.compile_expr(&seg.value);
            let expected = match seg.spec {
                ast::BinSpec::Int { .. } => self.ty_int(),
                ast::BinSpec::Binary { .. } => self.ty_binary(),
                ast::BinSpec::Utf8 => self.ty_string(),
            };
            self.engine.unify_at(expected, vty, seg.value.span());
            self.type_seg_size(seg.spec.size_expr());
        }
        self.ty_binary()
    }

    /// A segment's size expression is a runtime `Int`, not a binding.
    fn type_seg_size(&mut self, size: Option<&ast::Expression>) {
        if let Some(e) = size {
            let ety = self.compile_expr(e);
            let int_ty = self.ty_int();
            self.engine.unify_at(int_ty, ety, e.span());
        }
    }

    /// Compile a run of non-spread array elements against the element type var.
    fn compile_array_run(&mut self, elems: &[ast::ArrayElement], elem_var: Ty) {
        for elem in elems {
            if let ast::ArrayElement::Expression(e) = elem {
                let ty = self.compile_expr(e);
                self.engine.unify_at(elem_var, ty, type_defining_span(e));
            }
        }
    }

    fn compile_array(&mut self, expr: &ast::ArrayExpression) -> Ty {
        let elem_var = self.engine.fresh_var();
        let mut run_start = 0usize;
        for (i, e) in expr.elements.iter().enumerate() {
            let ast::ArrayElement::SpreadElement(spread) = e else {
                continue;
            };
            self.compile_array_run(&expr.elements[run_start..i], elem_var);
            run_start = i + 1;
            let inner = &spread.expression;
            let spread_ty = self.compile_expr(inner);
            let expected = self.ty_array(elem_var);
            self.engine.unify_at(expected, spread_ty, inner.span());
        }
        self.compile_array_run(&expr.elements[run_start..], elem_var);
        self.ty_array(elem_var)
    }

    fn compile_call(&mut self, expr: &ast::FunctionCallExpression) -> Ty {
        // Resolve the callee first, without emitting anything, so we can dispatch
        // on `ValueKind` and push its known parameter types into function-literal
        // arguments.
        if let Some(ResolvedCallee {
            name,
            span: name_span,
            scheme,
            has_binding,
            qualifier,
        }) = self.resolve_simple_callee(&expr.callee)
        {
            let inst_ty = self.engine.instantiate(&scheme, &self.rigid_ids);
            // Hover-only; the graph occurrence, with the right
            // Unqualified/Qualified kind, is emitted in `resolve_simple_callee`.
            let doc = self.doc_if_collecting(name);
            self.record(name, inst_ty, name_span, doc);

            return match scheme.kind {
                ValueKind::Constructor {
                    arity,
                    field_labels,
                    ..
                } => self.compile_ctor_call(
                    name,
                    arity as usize,
                    field_labels,
                    inst_ty,
                    &expr.arguments,
                    expr.span,
                ),
                ValueKind::Builtin { .. } => {
                    self.compile_positional_args(inst_ty, &expr.arguments, expr.span)
                }
                ValueKind::Local | ValueKind::ModuleFn => {
                    let ret = self.compile_positional_args(inst_ty, &expr.arguments, expr.span);
                    if !has_binding {
                        let module = qualifier.as_ref().map(|k| self.module_name(k));
                        self.no_runtime_binding(name, module.as_deref(), name_span);
                    }
                    ret
                }
            };
        }

        // Arbitrary callee expression: compile it first so we have its type
        // for argument hints.
        let callee_ty = self.compile_expr(&expr.callee);
        self.compile_positional_args(callee_ty, &expr.arguments, expr.span)
    }

    /// Type-check a positional argument list against a callee type. Labelled and
    /// spread arguments are rejected: only constructor calls support them.
    fn compile_positional_args(
        &mut self,
        callee_ty: Ty,
        args: &[ast::CallArg],
        call_span: Span,
    ) -> Ty {
        let (params, ret) = match self.engine.match_fun_type(callee_ty, args.len()) {
            Ok(pr) => pr,
            Err(MatchFunTypeError::IncorrectArity {
                expected,
                given,
                params,
                ret,
            }) => {
                self.error(
                    format!("Expected {} argument(s), got {}", expected, given),
                    call_span,
                );
                (params, ret)
            }
            Err(MatchFunTypeError::NotFn { ty }) => {
                let s = self.engine.type_to_str(ty);
                self.error(
                    format!(
                        "This value of type '{}' is not a function and cannot be called",
                        s
                    ),
                    call_span,
                );
                for arg in args {
                    self.compile_call_arg_value(arg);
                }
                return self.engine.fresh_var();
            }
        };

        for (i, arg) in args.iter().enumerate() {
            let hint = params.get(i).cloned();
            let (arg_ty, arg_span) = match arg {
                ast::CallArg::Positional(e) => (self.compile_expr_with_hint(e, hint), e.span()),
                ast::CallArg::Labeled { label, value } => {
                    self.error(
                        "Labelled arguments are only allowed in constructor calls".to_string(),
                        label.span,
                    );
                    (self.compile_expr_with_hint(value, hint), value.span())
                }
                ast::CallArg::Spread(e) => {
                    self.error(
                        "Spread arguments are only allowed in constructor record-update calls"
                            .to_string(),
                        e.span(),
                    );
                    (self.compile_expr(e), e.span())
                }
            };
            if let Some(p) = hint {
                self.engine.unify_at(p, arg_ty, arg_span);
            }
        }

        ret
    }

    fn compile_call_arg_value(&mut self, arg: &ast::CallArg) -> Ty {
        match arg {
            ast::CallArg::Positional(e) => self.compile_expr(e),
            ast::CallArg::Labeled { value, .. } => self.compile_expr(value),
            ast::CallArg::Spread(e) => self.compile_expr(e),
        }
    }

    /// Resolve a `module.member` shape against the imported qualifiers, note
    /// whether it has a runtime binding, and record the `Qualified` value-use.
    /// Shared by the callee and value positions, which diverge only on the
    /// failure modes — see [`QualifiedMember`].
    ///
    /// The verdict goes into the walk region on the way out: whether the
    /// qualifier was entered turns on `self.env` as it stands here, and a
    /// deferred body is elaborated long after, so the elaborator cannot
    /// re-derive it. `Elab::qualified` consumes exactly one verdict per call.
    fn resolve_qualified_member<'a>(
        &mut self,
        pa: &'a ast::PropertyAccessExpression,
    ) -> QualifiedMember<'a> {
        let out = self.resolve_qualified_member_inner(pa);
        // `LookupFailed` also skipped the qualifier — but it emitted a
        // diagnostic, so no `CleanModule` proof and no elaboration.
        self.record_walk_qualified(!matches!(out, QualifiedMember::NotQualified));
        out
    }

    fn resolve_qualified_member_inner<'a>(
        &mut self,
        pa: &'a ast::PropertyAccessExpression,
    ) -> QualifiedMember<'a> {
        let ast::Expression::Identifier(left) = pa.left.as_ref() else {
            return QualifiedMember::NotQualified;
        };
        // Any value binding of the same name — a decl, a parameter, a `let`, a
        // match binder — shadows an import's qualifier, and `env` holds exactly
        // those, so a hit means `left.member` is an ordinary field access.
        // Checked before the `imported_qualifiers` probe so the shadowed case
        // records no occurrence either.
        if self.env.lookup(&left.name).is_some() {
            return QualifiedMember::NotQualified;
        }
        let Some(module_key) = self.imported_qualifiers.get(&left.name).cloned() else {
            return QualifiedMember::NotQualified;
        };
        let ast::PropertyKey::Field(member) = &pa.right else {
            return QualifiedMember::NotQualified;
        };
        let Some((scheme, slot)) =
            self.lookup_module_member(&module_key, &member.name, member.span)
        else {
            return QualifiedMember::LookupFailed;
        };
        // The member is not in unqualified scope, so resolve the occurrence
        // through the imported scheme's canonical `def`.
        self.record_value_use(scheme.def, member.span, ReferenceKind::Qualified);
        self.record_qualifier_use(left);
        QualifiedMember::Resolved {
            module_key,
            member_name: &member.name,
            member_span: member.span,
            scheme,
            has_binding: slot.is_some(),
        }
    }

    /// Best-effort resolution of a callee that is a plain name or `module.name`.
    /// Anything else falls through to the arbitrary-expression path.
    fn resolve_simple_callee<'a>(
        &mut self,
        callee: &'a ast::Expression,
    ) -> Option<ResolvedCallee<'a>> {
        match callee {
            ast::Expression::Identifier(id) => {
                let scheme = *self.env.lookup(&id.name)?;
                // This path bypasses `compile_identifier`, so it is the genuine
                // unqualified-use seam for calls.
                self.record_value_use(scheme.def, id.span, ReferenceKind::Unqualified);
                Some(ResolvedCallee {
                    name: &id.name,
                    span: id.span,
                    scheme,
                    has_binding,
                    qualifier: None,
                })
            }
            // Qualified callee `module.member()`. Both failure modes mean "no
            // simple callee"; a failed lookup was already diagnosed.
            ast::Expression::PropertyAccessExpression(pa) => {
                match self.resolve_qualified_member(pa) {
                    QualifiedMember::NotQualified | QualifiedMember::LookupFailed => None,
                    QualifiedMember::Resolved {
                        module_key,
                        member_name,
                        member_span,
                        scheme,
                        has_binding,
                    } => Some(ResolvedCallee {
                        name: member_name,
                        span: member_span,
                        scheme,
                        has_binding,
                        qualifier: Some(module_key),
                    }),
                }
            }
            _ => None,
        }
    }

    /// Compile a call whose callee is a data constructor: positional, labelled
    /// and `..base` record-update forms, reordered into field-declaration order.
    #[allow(clippy::too_many_arguments)]
    fn compile_ctor_call(
        &mut self,
        variant_name: &str,
        arity: usize,
        field_labels_sl: ArenaSlice<pool::StrSlices>,
        inst_ty: Ty,
        args: &[ast::CallArg],
        call_span: Span,
    ) -> Ty {
        // The instantiated scheme is `Fun` for arity > 0 and `Con` for arity 0.
        let r = self.engine.find(inst_ty);
        let (param_tys, result_ty) = match self.engine.node(r) {
            TypeNode::Fun { params, ret } => (self.engine.children_of(params).to_vec(), ret),
            _ => (vec![], r),
        };

        // Partition into a spread base (at most one) and the explicitly-supplied
        // fields, keyed by declared position. Each expression keeps its
        // source-argument index, because the walk sub-regions below are spliced
        // back by that index.
        let mut spread: Option<(usize, &ast::Expression)> = None;
        for (i, arg) in args.iter().enumerate() {
            if let ast::CallArg::Spread(e) = arg {
                if spread.is_some() {
                    self.error(
                        "Constructor call may have at most one spread".to_string(),
                        e.span(),
                    );
                }
                spread = Some((i, e));
            }
        }
        let indexed: Vec<(Option<&ast::Identifier>, (usize, &ast::Expression), Span)> = args
            .iter()
            .enumerate()
            .filter_map(|(i, a)| match a {
                ast::CallArg::Positional(e) => Some((None, (i, e), e.span())),
                ast::CallArg::Labeled { label, value } => {
                    Some((Some(label), (i, value), label.span))
                }
                ast::CallArg::Spread(_) => None,
            })
            .collect();
        let (by_pos, _) = self.slot_ctor_args(
            variant_name,
            arity,
            field_labels_sl,
            indexed.iter().map(|&(l, ref pair, sp)| (l, pair, sp)),
            if spread.is_some() {
                None
            } else {
                Some((call_span, ""))
            },
        );

        // Two orders have to hold at once, so the walk splits them.
        //
        // Checking runs spread-first then in declared-field order: the spread's
        // `unify_at` solves `result_ty` before any hint is pushed into a
        // function-literal argument, and diagnostics come out in field order.
        //
        // Recording is source order, because the elaborator spills the arguments
        // into `Let`s in source order and the walk region is consumed
        // positionally. So each argument is checked into a sub-region of its own
        // and the sub-regions are spliced back in source order below. An
        // argument `slot_ctor_args` did not place is already an arity or label
        // error; it stays unchecked and contributes no region.
        let slots = &by_pos[..(field_labels_sl.len as usize).min(by_pos.len())];

        // All-positional: `args[i]` is already in slot `i`, so the two orders
        // coincide and no sub-region is needed.
        if args
            .iter()
            .all(|a| matches!(a, ast::CallArg::Positional(_)))
        {
            for (i, slot_expr) in slots.iter().enumerate() {
                let Some(&(_, value)) = *slot_expr else {
                    continue;
                };
                let expected = param_tys.get(i).copied();
                let ty = self.compile_expr_with_hint(value, expected);
                if let Some(p) = expected {
                    self.engine.unify_at(p, ty, value.span());
                }
            }
            return result_ty;
        }

        let mut regions: Vec<Vec<WalkStep>> = vec![Vec::new(); args.len()];

        // Record-update: the base is unified with the constructor's result
        // type; the per-field projection is Core's job.
        if let Some((ai, e)) = spread {
            self.open_walk_region();
            let base_ty = self.compile_expr(e);
            regions[ai] = self.close_walk_region();
            self.engine.unify_at(result_ty, base_ty, e.span());
        }

        for (i, slot_expr) in slots.iter().enumerate() {
            let Some(&(ai, value)) = *slot_expr else {
                continue;
            };
            let expected = param_tys.get(i).copied();
            self.open_walk_region();
            let ty = self.compile_expr_with_hint(value, expected);
            regions[ai] = self.close_walk_region();
            if let Some(p) = expected {
                self.engine.unify_at(p, ty, value.span());
            }
        }

        for region in regions {
            self.walk_tys.extend(region);
        }

        result_ty
    }

    fn compile_property_access(&mut self, expr: &ast::PropertyAccessExpression) -> Ty {
        // `module.member` access; qualified calls go through `compile_call`.
        match self.resolve_qualified_member(expr) {
            QualifiedMember::NotQualified => {}
            QualifiedMember::LookupFailed => return self.engine.fresh_var(),
            QualifiedMember::Resolved {
                module_key,
                member_name,
                member_span,
                scheme,
                has_binding,
            } => {
                let ty = self.engine.instantiate(&scheme, &self.rigid_ids);
                self.record(member_name, ty, member_span, None);
                // Only reaches a diagnostic on the no-runtime-binding path, so
                // the common case skips the display-name lookup.
                let module = (!has_binding).then(|| self.module_name(&module_key));
                self.check_named_value(
                    member_name,
                    module.as_deref(),
                    member_span,
                    scheme.kind,
                    has_binding,
                );
                return ty;
            }
        }

        let left_ty = self.compile_expr(&expr.left);

        match &expr.right {
            // `tuple.N` index.
            ast::PropertyKey::TupleIndex(num) => {
                let index = match num.value.parse::<i32>() {
                    Ok(i) => i,
                    Err(_) => {
                        self.error(
                            format!("Tuple index must be an integer, got '{}'", num.value),
                            num.span,
                        );
                        return self.engine.fresh_var();
                    }
                };

                let resolved = self.engine.find(left_ty);
                if let TypeNode::Tuple { elems } = self.engine.node(resolved) {
                    let elements = self.engine.children_of(elems);
                    if (index as usize) < elements.len() {
                        return elements[index as usize];
                    }
                    self.error(
                        format!(
                            "Tuple index {} out of bounds (tuple has {} elements)",
                            index,
                            elements.len()
                        ),
                        num.span,
                    );
                    return self.engine.fresh_var();
                }
                self.error(
                    format!("Cannot index .{} on non-tuple type", index),
                    num.span,
                );
                self.engine.fresh_var()
            }
            // `value.field` — labelled field shared by every variant.
            ast::PropertyKey::Field(field) => {
                self.compile_field_access(left_ty, &field.name, field.span)
            }
        }
    }

    /// compile_field_access error path: report and yield a fresh tyvar.
    fn field_access_bail(&mut self, msg: String, span: Span) -> Ty {
        self.error(msg, span);
        self.engine.fresh_var()
    }

    /// Single source of truth for the nominal `.field` lookup: the slot index a
    /// `TupleIndex` must address, plus the field's instantiated type. A field is
    /// projectable only when every variant carries that label at the same
    /// position and at a unifiable type.
    ///
    /// `unify_span` is `Some` on the typecheck path, which still has to unify
    /// the per-variant field types; `lower` passes `None`.
    fn field_in_variants(
        &mut self,
        info: TypeInfo,
        type_args: &[Ty],
        field_id: StrId,
        unify_span: Option<Span>,
    ) -> Result<(usize, Ty), FieldMismatch> {
        let variants = info.variants().ok_or(FieldMismatch::NotNominal)?;
        let mut found: Option<(usize, Ty)> = None;
        // Copied out by index rather than `to_vec()`ing the slices, to survive
        // the `&mut engine` calls in the loop body.
        for vi in 0..variants.len as usize {
            let v = self.engine.variants_of(variants)[vi];
            let hit = self
                .engine
                .variant_fields_of(v.fields)
                .iter()
                .enumerate()
                .find_map(|(i, f)| (f.label == field_id).then_some((i, f.ty)));
            let Some((i, fty)) = hit else {
                return Err(FieldMismatch::MissingOn(v.name));
            };
            let substituted = self
                .engine
                .substitute_type_vars(fty, info.type_params, type_args);
            match found {
                Some((prev, existing)) => {
                    if let Some(span) = unify_span {
                        self.engine.unify_at(existing, substituted, span);
                    }
                    if prev != i {
                        return Err(FieldMismatch::PositionDiffers {
                            variant: v.name,
                            at: i,
                            expected: prev,
                        });
                    }
                }
                None => found = Some((i, substituted)),
            }
        }
        found.ok_or(FieldMismatch::NoVariants)
    }

    /// Project `.field` out of a value. The runtime variant is not statically
    /// known, so every variant of the receiver type must carry that label at the
    /// same type. Shares [`Compiler::field_in_variants`] with `lower`, so the
    /// approved slot index and the emitted `TupleIndex` cannot disagree.
    fn compile_field_access(&mut self, receiver_ty: Ty, field: &str, field_span: Span) -> Ty {
        let resolved = self.engine.find(receiver_ty);
        let (type_id, type_name, type_args) = match self.engine.node(resolved) {
            TypeNode::Con { id, name, args } => (
                id,
                self.engine.str(name).to_string(),
                self.engine.children_of(args).to_vec(),
            ),
            TypeNode::Var(_) => {
                return self.field_access_bail(
                    format!(
                        "Cannot access field '{}' on a value of unknown type — add a type annotation",
                        field
                    ),
                    field_span,
                );
            }
            _ => {
                let s = self.engine.type_to_str(resolved);
                return self.field_access_bail(
                    format!("Type '{}' has no field '{}'", s, field),
                    field_span,
                );
            }
        };

        // Nominal: the variants come from the type the `Con` node identifies,
        // never from whatever same-named type a flat name lookup finds first.
        let Some(info) = self.env.lookup_type_info_by_id(type_id) else {
            return self.field_access_bail(
                format!("Type '{}' has no field '{}'", type_name, field),
                field_span,
            );
        };

        let field_id = self.engine.intern(field);
        let result_ty = match self.field_in_variants(info, &type_args, field_id, Some(field_span)) {
            Ok((_, ty)) => ty,
            // A variant list with no variants at all: nothing to project, and
            // the empty type was already diagnosed at its declaration.
            Err(FieldMismatch::NoVariants) => self.engine.fresh_var(),
            Err(FieldMismatch::NotNominal) => {
                return self.field_access_bail(
                    format!("Type '{}' has no accessible fields", type_name),
                    field_span,
                );
            }
            Err(FieldMismatch::MissingOn(variant)) => {
                let variant = self.engine.str(variant).to_string();
                return self.field_access_bail(
                    format!(
                        "Field '{}' is not present on every variant of '{}' (missing on '{}')",
                        field, type_name, variant
                    ),
                    field_span,
                );
            }
            Err(FieldMismatch::PositionDiffers {
                variant,
                at,
                expected,
            }) => {
                let variant = self.engine.str(variant).to_string();
                return self.field_access_bail(
                    format!(
                        "Field '{}' is not at the same position in every variant of '{}' (position {} in '{}', expected {})",
                        field, type_name, at, variant, expected
                    ),
                    field_span,
                );
            }
        };
        let qualified = format!("{}.{}", type_name, field);
        let doc = self.doc_if_collecting(&qualified);
        self.record(&qualified, result_ty, field_span, doc);
        // `Type.field` is a dotted key in `env.definitions`, which no local can
        // shadow, so goto-def / find-refs / rename and the field-level dead-code
        // walk all keep working.
        let fdef = self.env.lookup_definition(&qualified);
        self.record_value_use(fdef, field_span, ReferenceKind::Unqualified);
        result_ty
    }

    fn compile_or(&mut self, expr: &ast::OrExpression) -> Ty {
        let left_ty = self.compile_expr(&expr.expression);
        let resolved = self.engine.find(left_ty);

        let success_var = self.engine.fresh_var();

        // The `Con` node carries a nominal id, so a user-defined type that
        // happens to be called "Option"/"Result" can never match.
        let lhs_type_id = match self.engine.node(resolved) {
            TypeNode::Con { id, .. } => id,
            _ => TypeId::NONE,
        };

        // Option and Result differ only in the expected ICon and whether the
        // failure case carries a bindable payload (`err_var`).
        let err_var = if self.prelude.option.is(lhs_type_id) {
            if let Some(recv) = &expr.receiver {
                self.error(
                    "'or' on an Option does not bind a value (the failure case carries nothing)"
                        .to_string(),
                    recv.span,
                );
            }
            None::<Ty>
        } else if self.prelude.result.is(lhs_type_id) {
            Some(self.engine.fresh_var())
        } else {
            let s = self.engine.type_to_str(left_ty);
            self.error(
                format!(
                    "'or' requires the left side to be Option(_) or Result(_, _), got '{}'",
                    s
                ),
                expr.expression.span(),
            );
            // Still type-check the body so errors do not cascade.
            self.push_block_scope();
            let _ = self.compile_expr(&expr.body);
            self.pop_block_scope();
            return self.engine.fresh_var();
        };

        let expected = match &err_var {
            Some(e) => self.ty_result(success_var, *e),
            None => self.ty_option(success_var),
        };
        self.engine
            .unify_at(expected, left_ty, expr.expression.span());

        // Failure branch, with the `Err` payload bound to the receiver if any.
        self.push_block_scope();
        if let Some(e) = err_var
            && let Some(recv) = &expr.receiver
        {
            self.get_or_create_local(&recv.name);
            self.register_local_binding(&recv.name, e, recv.span);
        }
        let body_ty = self.compile_expr(&expr.body);
        self.pop_block_scope();
        self.engine
            .unify_at(success_var, body_ty, type_defining_span(&expr.body));

        success_var
    }

    /// Typecheck a function body inside a live `enter_fn_frame`/
    /// `finish_fn_frame` pair with params already bound, and park it. This is the
    /// seam between the typecheck walk and the Core IR pipeline, and it is a
    /// hand-off, not a call: the walk produces no bytecode at all.
    ///
    /// The parked body goes through `elaborate_body`→`lower`→`perceus`→`emit` in
    /// pass 6, once the whole module has been walked, so `lower` reads solved
    /// types rather than vars inference has not yet pinned down. There is no
    /// error path: the elaborator covers every form a [`CleanModule`] can
    /// contain. Elaboration and lowering run under `check_only` too — they are
    /// the only passes that prove a well-typed program is compilable.
    ///
    /// Returns `(body_ty, parked)`; see [`ParkedBody`].
    fn compile_fn_body(
        &mut self,
        params: &[ast::FunctionParameter],
        param_tys: &[Ty],
        body: &ast::Expression,
    ) -> (Ty, ParkedBody) {
        // Nested closures re-enter this fn via `compile_function_common` and
        // park their own bodies, reserving their `Function` entries first.
        let param_slots = self.local_count;
        // This body's own walk region. A nested lambda opens another one, so its
        // types never land in ours.
        self.open_walk_region();
        let body_ty = self.compile_expr(body);
        let walk_tys = self.close_walk_region();
        // The slots the walk reserved for pattern binds are dead. Rewind to the
        // param watermark so `Function.locals` reflects only Core's allocation.
        self.local_count = param_slots;
        debug_assert!(
            self.defer_depth > 0,
            "every function body must be walked inside a deferral region: \
             the Core pipeline runs after the walk, never during it"
        );
        // An ill-typed body is parked like any other and closed out empty at
        // drain time, where no `CleanModule` can be minted for it.
        let name = self
            .current_binding
            .unwrap_or_else(|| self.engine.intern("__anon__"));
        let param_binds: Vec<(StrId, Ty)> = params
            .iter()
            .zip(param_tys)
            .map(|(p, &ty)| (self.engine.intern(&p.identifier.name), ty))
            .collect();
        let parked = ParkedBody {
            name,
            param_binds,
            body: body.clone(),
            body_ty,
            walk_tys,
            // Taken here rather than in `finish_fn_frame`, which has already
            // handed the enclosing frame its own list back.
            closures: std::mem::take(&mut self.frame_closures),
            param_slots,
        };
        (body_ty, parked)
    }

    /// Elaborate one already-typechecked body, lower the whole `TypedProgram` it
    /// produces, and write the bodies of every eta wrapper the elaborator
    /// appended to it.
    ///
    /// **The one door into the Core pipeline.** The only caller of
    /// `typed_ir::elaborate_body`/`elaborate_toplevel`, which are the only
    /// constructors of the [`TypedProgram`] that `lower`, `perceus` and `emit`
    /// consume. Consuming a [`CleanModule`] here therefore closes the whole
    /// pipeline to a module that reported an error — not by convention, but
    /// because a poisoned module cannot produce the value the passes take.
    ///
    /// `at` is `Some(func_idx)` for a function body, whose reserved `Function`
    /// slot fixes its [`FuncIdx`](crate::core_ir::FuncIdx); `None` for a module
    /// toplevel, which has no index.
    ///
    /// The wrappers are written before the caller reads `current_addr()` as
    /// `base`, because `base` becomes the body's `Function.code_start`, which
    /// the VM adds to every jump operand — so it must name the body's first
    /// instruction. Each wrapper jumps over itself, so the stream falls through.
    fn elaborate_then_materialize(
        &mut self,
        _clean: CleanModule,
        at: Option<crate::core_ir::FuncIdx>,
        build: impl FnOnce(&mut Self, &mut ResolvedPool, &mut FnTable) -> TypedFn,
    ) -> LoweredBody {
        let Elaborated { program, eta_base } = self.elaborate(at, build);
        let crate::core_ir::CoreProgram {
            mut fns, toplevel, ..
        } = crate::core_ir::lower::lower(&program);
        let wrappers = fns.split_off(eta_base);
        self.materialize_eta_wrappers(&program.pool, eta_base, wrappers);
        let core = match at {
            Some(func_idx) => fns.swap_remove(func_idx.index()),
            None => CoreFn {
                name: program.toplevel.name,
                params: Vec::new(),
                body: toplevel,
                ret_ty: program.toplevel.ret,
            },
        };
        LoweredBody {
            core,
            pool: program.pool,
        }
    }

    /// Elaborate one body into a whole-module [`TypedProgram`], reserving a
    /// `Function` entry for every eta wrapper the walk minted.
    ///
    /// `TypedProgram::fns` and `program.functions` are both `FuncIdx`-indexed and
    /// must agree, so `fns` is padded up to `program.functions.len()` before the
    /// walk and each wrapper appended past that point gets its `Function`
    /// reserved here, in order. Nothing else may push a `Function` while the
    /// walk runs.
    fn elaborate(
        &mut self,
        at: Option<crate::core_ir::FuncIdx>,
        build: impl FnOnce(&mut Self, &mut ResolvedPool, &mut FnTable) -> TypedFn,
    ) -> Elaborated {
        let eta_base = self.program.functions.len();
        let mut pool = pool_for(&self.engine);
        // `RTy`s name nodes of `pool`, so the memo dies with the previous one.
        self.rty_cache.clear();
        let temps = TempTys::intern(self, &mut pool);
        let nil_ty = self.ty_nil();
        let nil = PreludeTys::resolve_rty(self, &mut pool, nil_ty);
        // Padding for the `fns` entries an earlier body owns, so the next
        // `FnTable::push` lands on the `Function` reserved for it below.
        let filler = || TypedFn {
            name: crate::types::StrId::NONE,
            params: Vec::new(),
            ret: nil,
            body: TypedExpr::Nil { ty: nil },
            binds: 0,
        };
        let mut fns = FnTable::new();
        for _ in 0..eta_base {
            fns.push(filler());
        }

        let code_before = self.program.code.len();
        let built = build(self, &mut pool, &mut fns);
        debug_assert_eq!(
            self.program.code.len(),
            code_before,
            "the elaborator must not append to `program.code`"
        );

        if self.program.functions.len() != eta_base {
            function_reserved_during_elaboration();
        }
        for w in fns.tail_from(crate::core_ir::FuncIdx::from_usize(eta_base)) {
            let arity = w.params.len() as i32;
            self.program.functions.push(Function {
                name: self.engine.str(w.name).into(),
                arity,
                locals: arity,
                capture_count: 0,
                code_start: 0,
                code_len: 0,
            });
        }

        let toplevel = match at {
            Some(func_idx) => {
                fns[func_idx] = built;
                filler()
            }
            None => built,
        };
        Elaborated {
            program: TypedProgram {
                fns: fns.into_vec(),
                toplevel,
                // Nothing downstream reads this: the elaborator pooled every
                // constant straight into `program.constants`, which is the pool
                // `emit`'s operands and the VM both address.
                consts: Vec::new(),
                pool,
                temps,
            },
            eta_base,
        }
    }

    /// Perceus and emit the eta wrappers `fns[base..]`, back-filling the
    /// `Function` entries [`Self::elaborate`] reserved for them. They go down
    /// ahead of the body that named them, each behind a `Jump` over itself.
    fn materialize_eta_wrappers(
        &mut self,
        pool: &ResolvedPool,
        base: usize,
        wrappers: Vec<CoreFn>,
    ) {
        use crate::core_ir::{emit, perceus};
        for (i, w) in wrappers.into_iter().enumerate() {
            let w = perceus::perceus(pool, w);
            // Wrappers own real `Function` slots and are `CallKnown` targets, so
            // they are native candidates like any declared body. Guarded on
            // `check_only` here, unlike `elaborate_body`, because a check still
            // materializes wrappers.
            let wrapper_idx = crate::core_ir::FuncIdx::from_usize(base + i);
            if !self.check_only
                && let Some(hook) = self.native_hook.as_mut()
                && super::native::config().includes(wrapper_idx)
            {
                let native_t0 = std::time::Instant::now();
                hook(wrapper_idx, &w, pool);
                self.native_stats.record(native_t0.elapsed());
                super::native::log_selected(wrapper_idx, self.engine.str(w.name));
            }
            let jump_over = self.current_addr();
            self.program.code.push(op_arg(Op::Jump, 0));
            let body_start = self.current_addr();
            let out = emit::emit(&w, self);
            self.program.code.extend(out.code);
            self.emit(Op::Ret);
            let end = self.current_addr();
            self.program.code[jump_over as usize].operand = end;
            let f = &mut self.program.functions[base + i];
            f.locals = f.arity.max(out.locals);
            f.code_start = body_start;
            f.code_len = end - body_start;
        }
    }

    /// Run the Core pipeline (elaborate→`lower`→`perceus`→`emit`) over one
    /// already typechecked body and append its bytecode. Runs in pass 6;
    /// `func_idx` is the placeholder `Function` the walk reserved and this fills
    /// in. Under `check_only` the pipeline stops after `lower`, leaving the
    /// `Function` unfilled for the caller to close out.
    ///
    /// The `CleanModule` is the caller's proof that nothing in the module failed
    /// to typecheck: a poisoned body has no types to elaborate.
    #[allow(clippy::too_many_arguments)]
    fn elaborate_body(
        &mut self,
        clean: CleanModule,
        name: StrId,
        param_binds: &[(StrId, Ty)],
        body: &ast::Expression,
        body_ty: Ty,
        walk_tys: &[WalkStep],
        param_slots: i32,
        func_idx: crate::core_ir::FuncIdx,
    ) {
        use crate::core_ir::{emit, perceus};
        // The elaborator's eta wrappers are written by the helper, ahead of this.
        let LoweredBody { core, pool, .. } =
            self.elaborate_then_materialize(clean, Some(func_idx), |c, pool, fns| {
                typed_ir::elaborate_body(c, pool, fns, name, param_binds, body, body_ty, walk_tys)
            });
        if self.check_only {
            return;
        }
        let core = perceus::perceus(&pool, core);
        if std::env::var("CORE_DBG").is_ok() {
            eprintln!("=== {}\n{core}", self.engine.str(name));
        }
        // The native seam: this body's `RTy`s index `pool`, which dies with this
        // call — see [`NativeHook`]. Post-perceus, so the hook sees the same
        // Core IR, Drops and reuse tokens included, that `emit` consumes.
        if let Some(hook) = self.native_hook.as_mut()
            && super::native::config().includes(func_idx)
        {
            let native_t0 = std::time::Instant::now();
            hook(func_idx, &core, &pool);
            self.native_stats.record(native_t0.elapsed());
            super::native::log_selected(func_idx, self.engine.str(name));
        }
        self.core.fns.push(core.clone());
        // A plain append: `emit`'s jump operands are relative to `code[0]` of the
        // block, and `code[0]` lands at `base`, which is exactly the
        // `Function.code_start` the VM adds back. Nothing may push an instruction
        // between here and the `extend` below, or `code_start` would no longer
        // name the block's first instruction.
        let base = self.current_addr();
        let out = emit::emit(&core, self);
        self.program.code.extend(out.code);
        // The walk reserved the `Function` entry but could not fill these fields,
        // nor write the `Ret`.
        self.program.code.push(op(Op::Ret));
        let end = self.current_addr();
        let f = &mut self.program.functions[func_idx.index()];
        f.locals = param_slots.max(out.locals);
        f.code_start = base;
        f.code_len = end - base - 1;
    }

    /// Open the elaboration phase boundary: every function body walked until the
    /// matching [`Self::end_deferred_elaboration`] is parked instead of
    /// elaborated, and the Core pipeline runs over all of them at once there.
    ///
    /// Exactly two places open one, and between them they cover every body the
    /// compiler walks: `analyse_module` (around all of pass 5) and
    /// `compile_impl`'s bare-expression program.
    ///
    /// The boundary is whole-module and not per-SCC because that is what
    /// `lower(p: &TypedProgram) -> CoreProgram` needs to exist, and because a
    /// body's types are only final once its SCC has been generalized: after
    /// `generalize_top` an unsolved body var has a `Generic` root rather than an
    /// `Unbound` one, so `lower` never observes a var about to be quantified out
    /// from under it. It is not an opcode win — `compile_binary` already unifies
    /// both operands during the walk.
    ///
    /// Deferral does move code addresses and `program.functions` ordering, so
    /// `al build`'s output is not byte-identical to the fused compiler's. What
    /// does not move is which `Function` slot a declared body owns — the
    /// property `tests/check_parity.rs` pins.
    pub(super) fn begin_deferred_elaboration(&mut self) {
        self.defer_depth += 1;
    }

    /// Freeze the value env for every body parked so far, restored around their
    /// elaboration in [`Self::end_deferred_elaboration`].
    ///
    /// Called once, between the declaration walk and the toplevel `let`/bare-
    /// expression walk. A `DeferredBody`'s frame snapshot fixes only the load a
    /// free name lowers to; its `Ty` and `ValueKind` still come from `self.env`
    /// at drain time, and a toplevel `let` rebinds in place. Bodies parked after
    /// this point need the live env and keep it: they may reference an earlier
    /// toplevel bind reachable only through `env`.
    pub(super) fn pin_deferred_env(&mut self) {
        // One pin per drain. An imported module is compiled before its importer
        // opens a region, so `analyse_module` never nests inside another's
        // deferral and a second pin would index a region not being closed.
        debug_assert_eq!(self.defer_depth, 1, "pin outside the module's own region");
        debug_assert!(self.deferred_env_pin.is_none(), "deferred env pinned twice");
        if self.deferred_bodies.is_empty() {
            return;
        }
        self.deferred_env_pin = Some((self.deferred_bodies.len(), self.env.clone()));
    }

    /// Close the region opened by [`Self::begin_deferred_elaboration`] and, at
    /// depth zero, run the Core pipeline over every parked body in walk order —
    /// innermost closure first, exactly the order the fused pipeline emitted
    /// them in.
    ///
    /// Every jump-over is patched after the whole run, not per body: the bodies
    /// are emitted contiguously here, so there is no `J_a, body_a, J_b, body_b`
    /// chain to hop along and each `J` skips the entire run.
    pub(super) fn end_deferred_elaboration(&mut self) {
        self.defer_depth -= 1;
        if self.defer_depth > 0 {
            return;
        }
        let bodies = std::mem::take(&mut self.deferred_bodies);
        let jumps: Vec<i32> = bodies.iter().map(|d| d.jump_over).collect();
        // Bodies parked before `pin_deferred_env` elaborate against the env the
        // declaration walk saw; the rest against the live one.
        let (pinned_upto, mut live_env) = match self.deferred_env_pin.take() {
            Some((n, pinned)) => (n, Some(std::mem::replace(&mut self.env, pinned))),
            None => (0, None),
        };
        for (i, d) in bodies.into_iter().enumerate() {
            if i == pinned_upto
                && let Some(live) = live_env.take()
            {
                self.env = live;
            }
            // Re-proved per body: an internal error raised while lowering one
            // body poisons the module for the next.
            match self.clean_module() {
                // A decl in the module failed to typecheck, so there is no typed
                // IR to lower and the parked bodies may reference unresolved
                // names. Leave them empty.
                None => self.close_empty_deferred(d.func_idx, d.param_slots),
                Some(clean) => self.elaborate_deferred(d, clean),
            }
        }
        // Every parked body was pre-pin: hand the live env back. Lowering reads
        // the env, never writes it, so the pinned copy is dropped unexamined.
        if let Some(live) = live_env {
            self.env = live;
        }
        let end = self.current_addr();
        for j in jumps {
            self.program.code[j as usize].operand = end;
        }
    }

    fn elaborate_deferred(&mut self, mut d: DeferredBody, clean: CleanModule) {
        let saved = self.enter_elab_frame(&mut d);
        self.elaborate_body(
            clean,
            d.name,
            &d.param_binds,
            &d.body,
            d.body_ty,
            &d.walk_tys,
            d.param_slots,
            d.func_idx,
        );
        self.leave_elab_frame(saved);
        // `check_only` stops the pipeline before `emit`, so the reserved
        // `Function` still has to be closed out.
        if self.check_only {
            self.close_empty_deferred(d.func_idx, d.param_slots);
        }
    }

    /// Swap a [`DeferredBody`]'s frame snapshot into the compiler for its
    /// elaboration, parking the current state in the returned [`ElabFrame`].
    ///
    /// `resolve_name` reads the frame and must reach the same answers the walk
    /// did. The whole `outer_scopes` chain is restored, not just the module
    /// scope: `resolve_variable` short-circuits on `current_binding` while
    /// scanning it, so `captures` alone cannot answer a self-reference.
    fn enter_elab_frame(&mut self, d: &mut DeferredBody) -> ElabFrame {
        self.env.push_scope();
        for (name, scheme) in &d.capture_env {
            self.env.define(name, *scheme);
        }
        ElabFrame {
            outer_scopes: std::mem::replace(
                &mut self.outer_scopes,
                std::mem::take(&mut d.outer_scopes),
            ),
            locals: std::mem::take(&mut self.locals),
            captures: std::mem::replace(&mut self.captures, std::mem::take(&mut d.captures)),
            capture_names: std::mem::replace(
                &mut self.capture_names,
                std::mem::take(&mut d.capture_names),
            ),
            rigid_ids: std::mem::replace(&mut self.rigid_ids, std::mem::take(&mut d.rigid_ids)),
            current_binding: std::mem::replace(&mut self.current_binding, d.binding.take()),
            // The lambdas this body wrote. `ElabCtx::closure` only asks about
            // nodes in the body being elaborated, so the enclosing frame's sites
            // (and the module toplevel's) are safe from being read by it.
            frame_closures: std::mem::replace(
                &mut self.frame_closures,
                std::mem::take(&mut d.closures),
            ),
        }
    }

    fn leave_elab_frame(&mut self, saved: ElabFrame) {
        self.env.pop_scope();
        let ElabFrame {
            outer_scopes,
            locals,
            captures,
            capture_names,
            rigid_ids,
            current_binding,
            frame_closures,
        } = saved;
        self.outer_scopes = outer_scopes;
        self.locals = locals;
        self.captures = captures;
        self.capture_names = capture_names;
        self.rigid_ids = rigid_ids;
        self.current_binding = current_binding;
        self.frame_closures = frame_closures;
    }

    /// Give a parked body that never elaborated the same shape an ill-typed one
    /// gets: a bare `Ret` and a zero-length `Function`. Its jump-over is patched
    /// with the rest of the region's, in [`Self::end_deferred_elaboration`].
    fn close_empty_deferred(&mut self, func_idx: crate::core_ir::FuncIdx, param_slots: i32) {
        let base = self.current_addr();
        self.program.code.push(op(Op::Ret));
        let f = &mut self.program.functions[func_idx.index()];
        f.locals = param_slots;
        f.code_start = base;
        f.code_len = 0;
    }

    /// Compile a `fn(...) { ... }` expression. `param_hints` is `Some` when the
    /// lambda is passed directly to a call site with known parameter types; an
    /// unannotated param then takes the hint rather than a fresh var, so the body
    /// can immediately do field access.
    ///
    /// The [`ClosureSite`] recorded under `span` belongs to the frame this runs
    /// in — the one whose elaboration builds the `Atom::Closure` and evaluates
    /// its captures.
    fn compile_function_common(
        &mut self,
        params: &[ast::FunctionParameter],
        body: &ast::Expression,
        return_annot: Option<&ast::TypeIdentifier>,
        param_hints: Option<Vec<Ty>>,
        span: Span,
    ) -> Ty {
        let saved = self.enter_fn_frame(None);

        // A lambda's annotation type-variables share scope with each other but
        // are local to the lambda; mint them via a fresh hydrator.
        let mut hydrator = Hydrator::new(AnnotationContext::Signature);
        let mut param_tys: Vec<Ty> = Vec::with_capacity(params.len());
        for (i, param) in params.iter().enumerate() {
            let p_ty = match &param.typ {
                Some(annot) => self.hydrate(&mut hydrator, annot),
                None => match &param_hints {
                    Some(h) if i < h.len() => h[i],
                    _ => self.engine.fresh_var(),
                },
            };
            param_tys.push(p_ty);
            self.bind_param(param, p_ty);
        }
        // Any tyvars the hydrator minted are rigid for the body.
        self.rigid_ids.extend(hydrator.rigid_ids().iter().copied());

        let (body_ty, body_emit) = self.compile_fn_body(params, &param_tys, body);
        let ret_ty = match return_annot {
            Some(rt) => {
                let annot_ty = self.hydrate(&mut hydrator, rt);
                self.engine
                    .unify_at(annot_ty, body_ty, type_defining_span(body));
                annot_ty
            }
            None => body_ty,
        };

        let (func_idx, captures) = self.finish_fn_frame(saved, "__anon__", params.len(), body_emit);
        // `finish_fn_frame` restored the enclosing frame, so this lands in that
        // frame's site list — the one its elaboration will read.
        self.frame_closures.push(ClosureSite {
            at: span,
            func_idx,
            captures,
        });
        self.engine.mk_fun(&param_tys, ret_ty)
    }

    /// Compile a top-level `fn` whose parameter and return types were already
    /// hydrated in Pass 3. `hydrator` carries the rigid generic ids, so
    /// `instantiate` leaves annotated type variables intact inside the body, and
    /// the body's inferred type is unified against the preregistered shape rather
    /// than the annotations being re-read.
    ///
    /// `global_slot` is threaded from Pass 3 so `global_to_func` is keyed by the
    /// same slot the caller emits `StoreLocal` for, rather than trusting
    /// `self.locals[name]` to still hold it.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn compile_declared_function(
        &mut self,
        name: &str,
        global_slot: GlobalSlot,
        params: &[ast::FunctionParameter],
        body: &ast::Expression,
        param_tys: Vec<Ty>,
        ret_ty: Ty,
        hydrator: &Hydrator,
    ) -> Ty {
        let saved = self.enter_fn_frame(Some(name));
        self.rigid_ids = hydrator.rigid_ids().clone();
        for (param, p_ty) in params.iter().zip(param_tys.iter()) {
            self.bind_param(param, *p_ty);
        }

        let (body_ty, body_emit) = self.compile_fn_body(params, &param_tys, body);
        self.engine
            .unify_at(ret_ty, body_ty, type_defining_span(body));

        // `global_to_func` is how the module toplevel's elaboration finds this
        // fn's body, off the same slot it stores into, in every mode — so the
        // index must be real under `check_only` too. Take it from
        // `finish_fn_frame` rather than re-deriving it from
        // `program.functions.len()`, which matches only by accident.
        let (func_idx, _) = self.finish_fn_frame(saved, name, params.len(), body_emit);
        self.global_to_func.insert(global_slot, func_idx);
        self.engine.mk_fun(&param_tys, ret_ty)
    }

    /// Snapshot the enclosing frame's codegen state, push a fresh inner frame,
    /// emit the jump-over placeholder that lets execution skip the embedded body,
    /// and open a new type-env scope.
    fn enter_fn_frame(&mut self, binding: Option<&str>) -> FnFrame {
        let binding_id = binding.map(|n| self.engine.intern(n));
        // Moved as-is, so a nested lambda resolves the enclosing fn's name
        // through `outer_scopes[0]` at the right slot; depth is irrelevant for
        // outer-scope resolution. `finish_fn_frame` moves it back.
        self.outer_scopes.push(Scope {
            locals: std::mem::take(&mut self.locals),
        });
        // Lets the enclosing code stream skip the embedded `Function` body.
        // Pushed under `check_only` too, which is what makes `jump_over` — and
        // every `Function` index downstream of it — mode-independent.
        let jump_over = self.current_addr();
        self.program.code.push(op_arg(Op::Jump, 0));
        self.env.push_scope();
        self.unused.push(HashMap::new());
        FnFrame {
            undo_base: self.undo_log.len(),
            marks_base: self.scope_marks.len(),
            local_count: std::mem::replace(&mut self.local_count, 0),
            captures: std::mem::take(&mut self.captures),
            capture_names: std::mem::take(&mut self.capture_names),
            rigid_ids: self.rigid_ids.clone(),
            binding: match binding_id {
                Some(id) => self.current_binding.replace(id),
                None => {
                    // A lambda has no self-name unless it is the RHS of a
                    // `name = fn(...)` binding, which stashed that name in
                    // `next_fn_self_name`. Inheriting the enclosing fn's
                    // `current_binding` would make a HOF-arg lambda's call to
                    // the enclosing fn emit `CallSelf` against its own frame.
                    std::mem::replace(&mut self.current_binding, self.next_fn_self_name.take())
                }
            },
            jump_over,
            closures: std::mem::take(&mut self.frame_closures),
        }
    }

    /// Register a freshly-typed local binding into the type environment, the
    /// reference graph and the hover record. These must stay in lockstep, so
    /// every local binder — params, pattern names, the `or`-receiver — funnels
    /// through here after reserving its slot with `get_or_create_local`.
    fn register_local_binding(&mut self, name: &str, ty: Ty, sp: Span) {
        let m = self.current_module_slice();
        self.env.define_at(
            name,
            mono(ty),
            DefinitionLocation::new(sp, m, EntityKind::Value),
        );
        self.track_binding(name, sp);
        self.record(name, ty, sp, None);
        self.emit_value_def(sp, name, None);
    }

    fn bind_param(&mut self, p: &ast::FunctionParameter, ty: Ty) {
        self.get_or_create_local(&p.identifier.name);
        self.register_local_binding(&p.identifier.name, ty, p.identifier.span);
    }

    /// Close out a function body: reserve its `Function` slot, combine the walk
    /// half ([`ParkedBody`]) with the frame state that just became final into a
    /// complete [`DeferredBody`], and restore the enclosing frame and type-env.
    /// Returns the `func_idx` and captured names so `compile_function_common` can
    /// record a [`ClosureSite`].
    ///
    /// No bytecode is written here. The body's `Ret`, its `code_start`/`locals`
    /// and its jump-over patch all belong to pass 6.
    fn finish_fn_frame(
        &mut self,
        saved: FnFrame,
        name: &str,
        arity: usize,
        parked: ParkedBody,
    ) -> (crate::core_ir::FuncIdx, Vec<StrId>) {
        // Taken before the enclosing frame's are moved back over them: the parked
        // body needs them at elaboration time to resolve its captures and
        // self-reference exactly as the walk did.
        let frame_binding = std::mem::replace(&mut self.current_binding, saved.binding);
        let frame_rigids = std::mem::replace(&mut self.rigid_ids, saved.rigid_ids);
        let frame_captures = std::mem::replace(&mut self.captures, saved.captures);
        // The inner frame's sites left with its `DeferredBody`; hand the
        // enclosing frame its list back so this lambda can be recorded into it.
        self.frame_closures = saved.closures;
        self.env.pop_scope();
        self.pop_unused_scope();

        let captured = std::mem::replace(&mut self.capture_names, saved.capture_names);
        // Reserved now, so the `func_idx` the `ClosureSite` and `global_to_func`
        // are about to record is the one the elaborated body fills in. Pass 6 can
        // still move only `locals`, `code_start` and `code_len`.
        self.program.functions.push(Function {
            name: name.into(),
            arity: arity as i32,
            locals: 0,
            capture_count: captured.len() as i32,
            code_start: 0,
            code_len: 0,
        });
        let func_idx = crate::core_ir::FuncIdx::from_usize(self.program.functions.len() - 1);
        // Read after `env.pop_scope()`: a captured name is bound in an enclosing
        // frame's scope, still open here but gone by elaboration time.
        let capture_env = captured
            .iter()
            .chain(frame_binding.iter())
            .filter_map(|&n| {
                let s = self.engine.str(n).to_string();
                self.env.lookup(&s).map(|&sc| (s, sc))
            })
            .collect();
        let ParkedBody {
            name: body_name,
            param_binds,
            body,
            body_ty,
            walk_tys,
            closures,
            param_slots,
        } = parked;
        self.deferred_bodies.push(DeferredBody {
            name: body_name,
            param_binds,
            body,
            body_ty,
            walk_tys,
            closures,
            param_slots,
            func_idx,
            jump_over: saved.jump_over,
            captures: frame_captures,
            capture_names: captured.clone(),
            rigid_ids: frame_rigids,
            binding: frame_binding,
            // Read before the `outer_scopes.pop()` below: the exact scope chain
            // `resolve_variable` walked.
            outer_scopes: self.outer_scopes.clone(),
            capture_env,
        });

        // `enter_fn_frame` pushed the enclosing frame's locals; move them back.
        if let Some(scope) = self.outer_scopes.pop() {
            self.locals = scope.locals;
        }
        // `locals` is restored wholesale, so the inner frame's undo entries must
        // be dropped, not replayed against the parent.
        self.undo_log.truncate(saved.undo_base);
        self.scope_marks.truncate(saved.marks_base);
        self.local_count = saved.local_count;

        // Load-bearing: `resolve_variable` promotes a name this body captured
        // from further out into the enclosing frame's own capture set, so
        // transitive captures chain outwards.
        for &cap_name in &captured {
            let _ = self.resolve_variable(cap_name);
        }
        (func_idx, captured)
    }

    fn compile_match(&mut self, m: &ast::MatchExpression) -> Ty {
        let subject_ty = self.compile_expr(&m.subject);
        let result_ty = self.engine.fresh_var();
        let mut any_pattern_err = false;
        let mut b = PatternBindings::new();

        for arm in &m.arms {
            self.push_block_scope();

            b.clear();
            if !self.type_pattern(&arm.pattern, subject_ty, &mut b.sink()) {
                any_pattern_err = true;
            }
            self.bind_pattern_initials(&b);
            self.type_pattern_sizes(&arm.pattern);

            if let Some(guard) = &arm.guard {
                let guard_ty = self.compile_expr(guard);
                let bool_ty = self.ty_bool();
                self.engine
                    .unify_at(bool_ty, guard_ty, type_defining_span(guard));
            }

            let body_ty = self.compile_expr(&arm.body);
            self.engine
                .unify_at(result_ty, body_ty, type_defining_span(&arm.body));

            self.pop_block_scope();
        }

        if !any_pattern_err {
            let resolved_subj = self.engine.resolve(subject_ty, Some(&self.env));
            let mut um = UsefulnessMatrix::new(resolved_subj);
            let all_pats: Vec<Pat> = m.arms.iter().map(|arm| um.lower(&arm.pattern)).collect();
            // Guarded arms don't contribute to exhaustiveness (the guard may be
            // false) and don't make later arms unreachable.
            let unguarded = m
                .arms
                .iter()
                .zip(&all_pats)
                .filter(|(arm, _)| arm.guard.is_none())
                .map(|(_, p)| p);
            if let Some(missing) = um.find_missing(unguarded) {
                self.error(
                    format!("Match is not exhaustive, missing: {}", missing),
                    m.subject.span(),
                );
            }
            for (i, arm) in m.arms.iter().enumerate() {
                if !um.is_useful(&all_pats[i]) {
                    self.error(
                        "This pattern is unreachable; a previous pattern matches the same values"
                            .to_string(),
                        arm.pattern.span(),
                    );
                }
                if arm.guard.is_none() {
                    um.push(&all_pats[i]);
                }
            }
        }

        result_ty
    }

    /// Bind a pattern's freshly-typed names: a local slot each, through
    /// [`Self::register_local_binding`].
    fn bind_pattern_initials(&mut self, b: &PatternBindings) {
        for (name, (ty, sp)) in b.bindings() {
            self.get_or_create_local(name);
            self.register_local_binding(name, *ty, *sp);
        }
    }

    /// Type-check the runtime size expression of every `<<..>>` segment in `p`.
    /// They are operands, not binders, and a later segment's size may name an
    /// earlier segment's binding (`<<n:8, body:bytes(n)>>`), so this runs after
    /// [`Self::bind_pattern_initials`].
    fn type_pattern_sizes(&mut self, p: &ast::Pattern) {
        let mut sizes: Vec<&ast::Expression> = Vec::new();
        p.for_each_binder(ast::OrAlternatives::All, &mut |b| {
            if let ast::PatternBinder::SizeExpr(e) = b {
                sizes.push(e);
            }
        });
        for e in sizes {
            self.type_seg_size(Some(e));
        }
    }

    /// Single source of truth turning a numeric literal's source text into a
    /// constant `Value`. On overflow or malformed input it emits a diagnostic and
    /// returns a kind-preserving zero, reachable only on that error branch, so it
    /// can never masquerade as a valid literal `0`.
    fn const_number(&mut self, n: &ast::NumberLiteral) -> Value {
        match number_literal_value(&n.value, &mut self.frozen) {
            Ok(v) => v.into_value(),
            Err(e) => {
                self.error(e.message(&n.value), n.span);
                e.recovery(&mut self.frozen).into_value()
            }
        }
    }
}

/// Whether matching `p` can fail on a value of its own type. Only wildcards,
/// bare names and tuples of those are irrefutable: a constructor pattern is
/// refutable even for a single-variant type (the tag is still tested), and an
/// or-pattern is refutable exactly when its last alternative is.
fn pattern_is_refutable(p: &ast::Pattern) -> bool {
    match p {
        ast::Pattern::Wildcard { .. } | ast::Pattern::Var { .. } => false,
        ast::Pattern::Tuple { elements, .. } => elements.iter().any(pattern_is_refutable),
        ast::Pattern::Or { first, rest, .. } => pattern_is_refutable(rest.last().unwrap_or(first)),
        _ => true,
    }
}

/// The span that defines an expression's type for error reporting: a block's
/// last node, recursively, so a return-type or branch mismatch points at the
/// value-producing sub-expression rather than the whole `{ ... }`.
pub fn type_defining_span(expr: &ast::Expression) -> Span {
    match expr {
        ast::Expression::BlockExpression(b) => match b.body.last() {
            Some(ast::Node::Expression(e)) => type_defining_span(e),
            Some(n) => n.span(),
            None => b.span,
        },
        _ => expr.span(),
    }
}

/// Why a numeric literal's source text could not be turned into a `Value`.
///
/// Scanner output is always `-?[0-9]+(\.[0-9]+)?`, so only the integer branch
/// can really fail, on i64 overflow. `InvalidFloat` is kept so the parse is
/// total over every `&str` rather than depending on that lexical invariant.
enum NumLitError {
    IntOutOfRange,
    InvalidFloat,
}

impl NumLitError {
    fn message(&self, src: &str) -> String {
        match self {
            NumLitError::IntOutOfRange => format!(
                "integer literal '{src}' out of range for Int (must be between {} and {})",
                i64::MIN,
                i64::MAX
            ),
            NumLitError::InvalidFloat => format!("invalid number literal '{src}'"),
        }
    }

    /// Value to substitute so codegen can continue after the diagnostic.
    /// Kind-preserving, so the compile does not cascade into spurious errors.
    fn recovery(&self, frozen: &mut FrozenBuilder) -> FrozenConst {
        match self {
            NumLitError::IntOutOfRange => frozen.int(0),
            NumLitError::InvalidFloat => frozen.float(0.0),
        }
    }
}

/// Parse a numeric literal's source text into a constant `Value`, built through
/// the frozen builder like every other program constant.
///
/// Total: the partiality is in the return type, so no caller can obtain a
/// fabricated `Value`. The only `Value`-producing path is
/// [`Compiler::const_number`], which emits a diagnostic before recovering.
fn number_literal_value(s: &str, frozen: &mut FrozenBuilder) -> Result<FrozenConst, NumLitError> {
    if s.contains('.') {
        s.parse()
            .map(|f| frozen.float(f))
            .map_err(|_| NumLitError::InvalidFloat)
    } else {
        s.parse()
            .map(|i| frozen.int(i))
            .map_err(|_| NumLitError::IntOutOfRange)
    }
}
