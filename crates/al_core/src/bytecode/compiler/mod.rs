//! AST → [`Program`]: orchestrates Hindley–Milner type inference and bytecode
//! emission.
//!
//! Function bodies go through the Core IR pipeline: the fused
//! [`Compiler::compile_expr`] walk runs first for type inference (a type that
//! unification just resolved is immediately available so `lower` can pick
//! typed opcodes — `AddInt` over `Add` — and pattern codegen can consult
//! variant layouts), then the typechecked body is lowered to
//! [`crate::core_ir`] ANF, run through Core→Core passes (Perceus reuse, later
//! mode inference), and finally emitted by `core_ir::emit`. Type erasure
//! happens exactly once at Core→bytecode. See [`Compiler::compile_fn_body`]
//! and `docs/core-ir-spec.md`.
//!
//! Module top level is the exception: declarations are mutually
//! recursive and order-free, so `analysis.rs` runs its multi-pass declaration
//! analysis first and hands each body back to this file.
//!
//! # Map of the module
//!
//! - **This file** — [`Compiler`] + entry points ([`compile`] / [`check`] /
//!   [`check_as_module`]): all codegen, scoping, and inference state in one
//!   struct, documented field by field; the pass itself (`compile_node` /
//!   `compile_expr` and friends, one method per AST form); and the Core IR
//!   orchestration ([`Compiler::compile_fn_body`]), the `lower → perceus →
//!   emit` pipeline hook for function bodies.
//! - **`patterns.rs`** — pattern type-checking (no codegen):
//!   [`Compiler::type_pattern`] plus constructor lookup and argument
//!   slotting.
//! - **`bridges.rs`** — the `EmitCtx`/[`ElabCtx`] impls through which the
//!   compiler-agnostic Core IR passes speak to this compilation.
//! - **`tests.rs`** — the unit-test modules.
//!
//! The LSP/workspace layer that owns a `Compiler` across edits —
//! [`IncrementalSession`], [`Watermark`]/`reset_to`, and the reference-graph
//! finalization — lives in [`super::session`]; the peephole superinstruction
//! pass lives in [`super::peephole`].
//!
//! # Invariants
//!
//! - Every constant `Value` is built through the compiler's
//!   `FrozenBuilder` handle (the `const_*` helpers), never a bare `Value`
//!   constructor, so all constants live in the program's frozen area and
//!   stay valid for the program's life on any thread.
//! - Everything a compile appends to (inference pool, type env, code,
//!   functions, constants) stays append-only between module boundaries;
//!   that is what makes `Watermark` rollback pure truncation.
//! - Local scoping is undo-log based (`undo_log` / `scope_marks`): popping
//!   a scope replays only the bindings it actually shadowed, never a map
//!   snapshot.

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

    /// Proof — as of the moment it was minted — that the module being compiled
    /// has produced no error diagnostic.
    ///
    /// There is no error node anywhere past the typecheck walk:
    /// [`TypedExpr`](crate::typed_ir::TypedExpr) has no poison arm, `lower` has
    /// nothing to emit for a subtree the typechecker rejected, and `perceus`
    /// has no type to compute its shape from. Rather than teach every pass to
    /// recognise poison, the whole pipeline is made *unreachable* from a
    /// poisoned module at its **first** gate, which is the elaborator and not
    /// `lower`: `typed_ir::elaborate_body`/`elaborate_toplevel` are the only
    /// constructors of a [`TypedFn`](crate::typed_ir::TypedFn), hence of the
    /// [`TypedProgram`](crate::typed_ir::TypedProgram) that `lower` consumes,
    /// and [`Compiler::elaborate_then_materialize`](super::Compiler::elaborate_then_materialize)
    /// is the only place in this compiler that calls them. It consumes a
    /// `CleanModule`, and [`CleanModule::mint`] is the only way to make one.
    ///
    /// (`core_ir::lower::lower` itself takes no proof — it does not need
    /// one. Its argument *is* the proof: a `TypedProgram` cannot exist for a
    /// module that failed to typecheck, because the only way to build one runs
    /// through the gate above.)
    ///
    /// A body the walk rejected is therefore never elaborated; it is closed out
    /// empty, which is what the fused pipeline always did. In particular
    /// `Elab::poison` — which fires when the check walk left an expression with
    /// no recorded type, exactly what a rejected subtree looks like — cannot be
    /// reached from a module that already reported a type error, so a plain
    /// type error never turns into a compiler-bug report.
    ///
    /// Neither `Copy` nor `Clone`, and taken by value: cleanliness established
    /// before one elaboration says nothing about the state after it, since an
    /// elaboration can itself append diagnostics. Each one re-proves it.
    #[must_use]
    pub(super) struct CleanModule {
        _priv: (),
    }

    impl CleanModule {
        /// `Some` iff `diagnostics` carries no error — exactly the predicate
        /// `CompileResult::success` reports, so gating on this proof cannot
        /// change which diagnostics the user sees.
        pub(super) fn mint(diagnostics: &[Diagnostic]) -> Option<Self> {
            (!has_errors(diagnostics)).then_some(Self { _priv: () })
        }
    }
}
use clean::CleanModule;

/// Why a nominal `.field` lookup failed. Carries enough of the offending
/// variant for the typecheck path to render its diagnostics; lower just
/// discards it.
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

// ============================================================================
// CompileResult
// ============================================================================

/// The artifacts one compile materialises together: the bytecode `Program`
/// and the Core IR it was derived from. A `check` still carries one — the
/// function table it registers is mode-independent and pinned by
/// `crates/al/tests/check_parity.rs` — its bodies are simply never emitted.
#[derive(Debug)]
pub struct Emitted {
    pub program: Program,
    /// Lowered Core IR (typed ANF) for the whole program.
    /// Golden-snapshotted by `crates/al/tests/core_ir.rs`.
    pub core: crate::core_ir::CoreProgram,
}

#[derive(Debug)]
pub struct CompileResult {
    /// What the compile built, when it built anything. `None` means no
    /// program exists at all: the incremental (LSP) session's check path,
    /// which analyses without materialising a result, and a compile whose
    /// stdlib seed failed before the program was touched. Consumers that run
    /// or disassemble must unwrap the absence explicitly — handing a real
    /// `entry` index over empty code is unrepresentable.
    pub emitted: Option<Emitted>,
    pub diagnostics: Vec<Diagnostic>,
    /// Workspace reference graph (the single source of truth for goto-def /
    /// find-refs / rename / symbols / dead-code). An owned `Rc` handle so the
    /// owning `IncrementalSession` can keep answering queries against the same
    /// graph after the result is handed back.
    pub references: Rc<ReferenceGraph>,
    pub success: bool,
}

// ============================================================================
// Compiler — single-pass: HM type inference + bytecode emission
// ============================================================================

/// An enclosing function frame's locals, moved here wholesale by
/// `enter_fn_frame` and moved back by `finish_fn_frame`. Holding the map
/// itself (rather than a name→slot copy) makes the frame push/pop O(1).
#[derive(Clone)]
pub(super) struct Scope {
    locals: HashMap<StrId, LocalSlot>,
}

/// A live local binding: the stack slot it occupies plus the block-scope
/// nesting depth (`scope_marks.len()`) at which it was bound. The depth lets
/// `get_or_create_local` decide in O(1) whether a re-binding shadows an
/// enclosing scope (depth strictly shallower than the current scope ⇒ a fresh
/// slot must be allocated so the outer value survives the block) or simply
/// rebinds within the current scope (reuse the slot).
#[derive(Clone, Copy, Debug)]
pub(super) struct LocalSlot {
    pub(super) slot: i32,
    pub(super) depth: u32,
    /// Recorded by [`Compiler::binds_a_global`] at bind time: this name lives
    /// in the entry frame, so a nested body loads it with `PushGlobal <slot>`
    /// rather than capturing it. `resolve_variable` reads this field instead of
    /// re-deriving the predicate from `depth`, so the binder and the loader
    /// cannot disagree about which names are globals.
    pub(super) is_global: bool,
}

/// One top-level `fn`/`const` declaration of the module currently being
/// compiled, as the toplevel elaboration needs it.
///
/// Both facts the elaborator needs about a declaration live on this one record:
/// where it sits in the module block (so the spine can be scheduled in
/// dependency order) and which entry-frame slot the check walk already gave it
/// (so `TypedBind::global` pins the `StoreLocal` that the `PushGlobal`s already
/// emitted into fn bodies address). Nothing maps the *name* back to a slot.
#[derive(Clone, Copy, Debug)]
pub(super) struct ToplevelDecl {
    /// Index of the declaration's node in the module block's `body`.
    pub(super) node: usize,
    /// Interned declaration name. Read only to resolve a forward *reference*
    /// to this decl from inside the toplevel spine — the AST spells a name, so
    /// something must translate it — and the answer is this record's `slot`.
    pub(super) name: StrId,
    /// The entry-frame slot Pass 3 allocated for the declaration.
    pub(super) slot: GlobalSlot,
}

/// Compiler state snapshotted on entry to a nested function body and restored
/// by `finish_fn_frame` once its bytecode and closure have been emitted.
struct FnFrame {
    /// `undo_log`/`scope_marks` lengths at frame entry. A nested body's
    /// param/local bindings live in the *enclosing* scope's mark range (no
    /// `push_local_scope` wraps bare params), so `finish_fn_frame` truncates
    /// back to these — `locals` is restored wholesale from `outer_scopes`, so
    /// the inner frame's undo entries must be discarded, never replayed
    /// against the parent map.
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

/// Per-module compiler state snapshotted on entry to a submodule body by
/// `enter_module_frame` and restored by `leave_module_frame`. Mirrors
/// `FnFrame`: adding a per-module field means one line here plus one
/// swap/restore in the enter/leave pair, not two hand-written
/// `mem::replace` calls to remember at both ends of `compile_module_body`.
struct ModuleFrame {
    module: ModulePath,
    module_key: ModuleKey,
    module_path_slice: Option<ArenaSlice<pool::StrSlices>>,
    imported_qualifiers: HashMap<String, ModuleKey>,
    base_dir: Option<PathBuf>,
    module_refs: ModuleReferences,
}

/// Compiler frame state parked by `enter_elab_frame` while a [`DeferredBody`]'s
/// snapshot is swapped in for its elaboration, and restored wholesale by
/// `leave_elab_frame`. Mirrors [`ModuleFrame`]: adding a field the elaboration
/// must see means one line here plus one swap/restore in the enter/leave pair,
/// not two hand-written `mem::replace` calls to keep paired at both ends of
/// `elaborate_deferred`.
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
    // --- Codegen state ---
    pub(super) program: Program,
    /// Append handle to `program`'s frozen area.
    /// Every constant `Value` the compiler builds — literals, enum
    /// construction/match headers, folded binaries — is constructed through
    /// this builder (the `const_*` helpers below), never via bare `Value`
    /// constructors, so enum names and field labels from compile-time
    /// constants all point at the area's canonical interned allocations.
    frozen: FrozenBuilder,
    /// `Value::to_bits` → constant-pool index, so `add_constant` returns the
    /// existing slot for a repeated literal instead of pushing a duplicate.
    /// `frozen` interns heap constants and immediates encode by value, so
    /// identical constants have identical bits. Self-validating on lookup
    /// (index in-bounds and slot bits still match) so `reset_to`'s pool
    /// truncate needs no paired invalidation — a stale entry just misses.
    const_dedup: HashMap<u64, i32>,
    pub(super) locals: HashMap<StrId, LocalSlot>,
    /// Scoped-symbol-table undo log. Every mutation of `locals` made inside an
    /// open block scope appends `(name, previous entry)`; `pop_local_scope`
    /// unwinds back to the entering scope's mark, restoring (or removing) each
    /// touched name. Replaces snapshotting the whole `locals` map on every
    /// block/if-arm/match-arm/lambda entry: cost is now O(bindings actually
    /// shadowed) instead of O(scopes × live locals).
    pub(super) undo_log: Vec<(StrId, Option<LocalSlot>)>,
    /// `undo_log` length captured at each `push_local_scope`; `pop_local_scope`
    /// unwinds the log down to the popped mark.
    pub(super) scope_marks: Vec<usize>,
    /// Per-scope unused-binding tracking. Each frame maps a let/param/match
    /// name (that doesn't start with `_`) to its definition span; `mark_used`
    /// removes the entry on first reference. Anything left when the frame is
    /// popped is reported as an error. Frames are pushed/popped in lockstep
    /// with `scope_marks` (blocks) and `enter_fn_frame`/`finish_fn_frame`
    /// (params), so a use inside a nested closure marks the outer binding by
    /// walking the whole stack.
    pub(super) unused: Vec<HashMap<StrId, Span>>,
    pub(super) outer_scopes: Vec<Scope>,
    pub(super) local_count: i32,
    /// Entry-frame slot → `program.functions` index for every top-level `fn`
    /// that has already been compiled. Populated by `compile_declared_function`
    /// after `finish_fn_frame` assigns the `func_idx`; consulted by
    /// `resolve_variable` to choose between [`Denotation::known_fn`] and
    /// [`Denotation::global`], which lets the Core emit call a known top-level
    /// fn with `CallKnown` (immediate `func_idx`, no callee value pushed)
    /// instead of the `PushGlobal; Call` dynamic path. A miss (forward ref
    /// within an SCC, or a non-fn global) falls back to `Call`.
    pub(super) global_to_func: HashMap<GlobalSlot, crate::core_ir::FuncIdx>,
    /// This module's own top-level `fn`/`const` declarations, in Pass 5
    /// SCC-visit order (leaves first). Cleared to entry-file scope at
    /// `code_mark`.
    ///
    /// One record serves both halves of the toplevel elaboration. The
    /// [`ToplevelDecl::node`] index *schedules* the spine: the elaborator walks
    /// the module block's decl nodes in this order, not source order, so a
    /// forward-referenced `const` is stored before it is read. The
    /// [`ToplevelDecl::slot`] on that same record is the entry-frame slot the
    /// decl's `TypedBind::global` is pinned to, and the one an intra-SCC
    /// forward *reference* loads with `PushGlobal`. Definition and use read the
    /// same field of the same record, so they cannot disagree.
    pub(super) toplevel_decls: Vec<ToplevelDecl>,
    /// Entry-frame slots for module-scope binds that are *not* declarations —
    /// top-level `let`s and destructured names — in binding order. Unlike
    /// [`Self::locals`] this is not unwound by `pop_local_scope`, so it
    /// survives `analyse_module`'s scope pop and reaches the toplevel
    /// elaboration, which drains it in the order it was filled: both walks
    /// visit the module's statements in source order. A queue rather than a
    /// map because a name may be rebound (`x = 10; f = fn() x; x = 20`) and
    /// each binding needs its own slot — the closures compiled in between
    /// address distinct slots via `PushGlobal` — so a name is not an identity
    /// here, and nothing downstream ever maps one back to a slot. Cleared to
    /// entry-file scope at `code_mark`.
    pub(super) toplevel_binds: VecDeque<GlobalSlot>,
    /// True only while `analyse_module` walks a module's own statement list,
    /// the single walk whose bindings the toplevel elaboration mirrors. The
    /// queue above is positional, so *any* other walk that fed it would hand
    /// the elaborator a slot belonging to a different binding. Being a global
    /// ([`Compiler::binds_a_global`]) does not say this on its own: imports,
    /// the prelude's slot seeds and `fn`/`const` declarations are all globals
    /// bound outside the statement walk, and each reaches the elaborator by a
    /// route of its own.
    pub(super) walking_module_statements: bool,
    /// Memo for [`ElabCtx::resolve_rty`], keyed by union-find root: one pool
    /// node per solved type, for the whole `TypedProgram` the current
    /// `elaborate` call is building. Cleared there, with the pool.
    rty_cache: HashMap<Ty, RTy>,
    pub(super) captures: HashMap<StrId, i32>,
    pub(super) capture_names: Vec<StrId>,
    pub(super) current_binding: Option<StrId>,
    /// One-shot self-name handed to the next `enter_fn_frame(None)` so a
    /// `name = fn(...)` binding's lambda can self-recurse, without leaking the
    /// enclosing fn's binding into unrelated (e.g. HOF-arg) lambdas.
    pub(super) next_fn_self_name: Option<StrId>,
    /// The definition whose body is currently being compiled, used as the
    /// `owner` of every reference-graph occurrence emitted while it is set
    /// (the def→def edge channel for dead-code reachability). `None` at module
    /// top level so genuine executed code roots its references; set/restored
    /// around each `fn`/`const`/`type` body in the single-pass traversal and
    /// spans nested lambdas (a lambda has no `DefId`, so its body's references
    /// belong to the enclosing definition).
    pub(super) current_owner: Option<DefId>,
    // --- Type inference state ---
    pub(super) engine: InferEngine,
    pub(super) env: TypeEnv,
    /// Generic var ids that are rigid for the body currently being checked
    /// (the type parameters from a `fn`'s annotation). `instantiate` callers
    /// pass this so those ids are not freshened inside the body.
    pub(super) rigid_ids: HashSet<i32>,
    pub(super) recorded: Vec<RawRef>,
    /// Transient reference-graph collector for the module *currently* being
    /// analysed (entry file, or a submodule between the save/restore dance in
    /// `compile_module_body`). The typecheck/infer pass emits definitions and
    /// references into this; `compile_module_body` moves the finished set into
    /// the module's `CachedModule`. Reset by `reset_to` like `recorded`.
    pub(super) module_refs: ModuleReferences,
    check_only: bool,
    /// Whether to buffer per-occurrence `RawRef`s and resolve them into the
    /// `HoverFact` table in `finalize_references`. Only `IncrementalSession`
    /// (the LSP) consumes `HoverFact`s; the free `compile`/`check` entry points
    /// discard them, so they leave this `false` and the whole O(occurrences)
    /// resolve pass (a deep `Type`-tree build + `HashSet` alloc per occurrence)
    /// plus the per-occurrence `name.to_string()` is skipped. The reference
    /// graph (goto-def/find-refs/dead-code) is built independently via
    /// `module_refs` and is unaffected.
    pub(super) collect_hover_facts: bool,
    // --- Module state ---
    pub(super) module_table: ModuleTable,
    /// Append-only `ModulePath` ↔ `ModuleId` interner backing every `DefId`.
    /// Persistent across `reset_to`/incremental recompiles (like
    /// `module_table`) so a `DefId` minted in one `check` still resolves after
    /// later modules are added or evicted; ids are reused across recompiles.
    pub(super) ref_interner: ModuleInterner,
    pub(super) current_module: ModulePath,
    /// `current_module`'s canonical cache key, maintained in lockstep with it
    /// (swapped by `enter_module_frame`, reset alongside it) so per-module
    /// bookkeeping never re-derives a key from a possibly-unresolved path.
    pub(super) current_module_key: ModuleKey,
    /// Memo of `current_module` interned into `engine.str_slices`; invalidated
    /// (set to `None`) whenever `current_module` is swapped in
    /// `enter_module_frame`.
    pub(super) module_path_slice: Option<ArenaSlice<pool::StrSlices>>,
    /// Memo of a module-path `ArenaSlice` → its interned `ModuleId`. The
    /// `str_slices` pool is append-only within a compile, so a given
    /// `ArenaSlice` always denotes the same path (hence the same id); cleared
    /// in `reset_to`, the single point where the pool is rewound. Lets
    /// `defid_of`/`record` skip the per-occurrence `strs_of` + key-join
    /// allocations and string-hash probe on the per-keystroke recompile.
    pub(super) defid_module_memo: HashMap<ArenaSlice<pool::StrSlices>, ModuleId>,
    /// Qualifier name in this file → the imported module's canonical key.
    pub(super) imported_qualifiers: HashMap<String, ModuleKey>,
    /// canonical module key -> the import path as the user wrote it. Identity
    /// is the resolved file; diagnostics must still say `./lib`, not
    /// `/private/var/.../lib`.
    pub(super) module_display: HashMap<ModuleKey, String>,
    pub(super) base_dir: Option<PathBuf>,
    pub(super) prelude: PreludeBindings,
    /// Names user code may not redefine. Populated from the prelude module's
    /// interface (every exported type and constructor) so the set is derived
    /// from `al.al` rather than mirrored in Rust. `@vm` functions are
    /// deliberately excluded — `println` is shadowable.
    /// `BTreeSet` so `precompile_stdlib`'s snapshot iterates sorted — the
    /// blob's `RESERVED` slice must be reproducible (and binary-searchable).
    pub(super) reserved: BTreeSet<String>,
    /// When the binary is built with the static stdlib, this is the
    /// `&'static StaticStdlib` handle; lazy-hydrate fallthrough lookups consult
    /// it on a runtime-map miss. `None` for the from-source path
    /// (`register_prelude`, `precompile_stdlib`, LSP-editing-stdlib).
    static_stdlib: Option<&'static crate::static_ir::StaticStdlib>,
    /// Lowered Core IR accumulated during this compile. Each function body
    /// [`Self::compile_fn_body`] routes through the `lower→perceus→emit`
    /// pipeline pushes its post-perceus [`crate::core_ir::CoreFn`] into
    /// `core.fns`, and [`compile_impl`] lowers the module toplevel into
    /// `core.toplevel`. Moved into [`CompileResult::core`] at the end of
    /// [`compile_impl`].
    pub(super) core: crate::core_ir::CoreProgram,
    /// The `fn(...) {...}` expressions written *directly inside the frame being
    /// walked* — one [`ClosureSite`] each, in the order the walk closed them.
    ///
    /// The frame owns its sites, not the compiler: [`Self::enter_fn_frame`]
    /// parks the enclosing frame's list and hands the new frame an empty one,
    /// [`Self::compile_fn_body`] moves the walked frame's list into its
    /// [`ParkedBody`], and [`Self::elaborate_deferred`] swaps that list back
    /// in for exactly the elaboration of that body. Entry-frame sites (a lambda
    /// in a toplevel `let` or `const`) accumulate here across the module walk
    /// and are dropped once that module's toplevel has been elaborated, so no
    /// two modules' sites are ever live at once.
    ///
    /// That ownership is what a `HashMap<Span, _>` on the compiler could not
    /// have: a `Span` carries no file id, so a global table let one module's
    /// lambda answer for another's at the same offset, and its `func_idx`/
    /// `StrId`s outlived the arenas they indexed (`reset_to`).
    pub(super) frame_closures: Vec<ClosureSite>,
    /// The type the check walk inferred for every expression it entered, in
    /// entry order, one region per elaboration unit.
    ///
    /// The elaborator consumes it *positionally* — [`Elab::take_ty`] pops the
    /// next entry as it enters the next expression — rather than looking a span
    /// up. Re-deriving the type instead would reinstantiate constructor and
    /// module-function schemes, producing unresolved vars where the walk had a
    /// concrete `Con`; keying on `Span` instead let one module's expression
    /// answer for another's at the same line/column, since a `Span` carries no
    /// file id.
    ///
    /// The invariant is a traversal one, not a naming one: `compile_expr`
    /// reserves a slot on entry (so the order is pre-order) and the elaborator
    /// enters exactly the expressions the walk entered, in the same order. Both
    /// walks skip the same nodes — the `RangeExpression` of a slice, a
    /// `module.member` qualifier, a simple callee — and both visit constructor
    /// arguments in source order. The one skip that is a *decision* rather than
    /// a syntactic fact — is `name.field` a module member? — is recorded as a
    /// [`WalkStep::Qualified`] rather than re-derived, because the env it
    /// depends on has moved on by the time a deferred body is elaborated.
    /// `elaborate_fn` asserts the region is fully consumed, so a divergence
    /// cannot pass silently.
    ///
    /// This is the region currently being filled. A function body opens its own
    /// ([`Self::compile_fn_body`], which hands it to that body's
    /// [`ParkedBody`]) and parks the enclosing one in
    /// [`Compiler::walk_tys_stack`], so a nested lambda's types never mix into
    /// the enclosing body's region. What is left when no body is open is the
    /// module toplevel's own region, drained by whoever elaborates that
    /// toplevel. Cleared by `reset_to`, whose engine rewind would otherwise
    /// leave dangling `Ty` indices.
    pub(super) walk_tys: Vec<WalkStep>,
    /// The regions of the bodies enclosing the one being walked, innermost last.
    /// Parked, not indexed: only [`Compiler::walk_tys`] is ever written to.
    pub(super) walk_tys_stack: Vec<Vec<WalkStep>>,
    /// Function bodies whose typecheck walk has finished but whose Core
    /// pipeline (`lower`→`perceus`→`emit`) is deferred until the enclosing
    /// declaration group has been generalized. See
    /// [`Self::begin_deferred_elaboration`].
    deferred_bodies: Vec<DeferredBody>,
    /// Nesting depth of open deferral regions. `compile_fn_body` runs the Core
    /// pipeline inline when this is zero (REPL entries, top-level `let`s and
    /// bare expressions, which are walked outside `analyse_module`'s Pass 5).
    defer_depth: u32,
    /// `(number of bodies parked so far, the value env as those bodies saw it)`,
    /// set by [`Self::pin_deferred_env`] at the end of the declaration walk.
    ///
    /// A `DeferredBody` snapshots the *frame* (`outer_scopes`, `captures`, the
    /// self-name) but not `self.env`, and `resolve_name` re-queries the live
    /// env at drain time for a name's `Ty` and `ValueKind`. The drain now runs
    /// after the toplevel `let` walk, and `TypeEnv::define` overwrites a
    /// same-scope binding in place, so without this pin a toplevel `let` would
    /// re-point a name a declared body already resolved to a builtin,
    /// constructor or sibling decl (`println = 5` after `fn g() { println(x) }`
    /// turned the callee into a self-call).
    deferred_env_pin: Option<(usize, TypeEnv)>,
}

/// One body, elaborated into a whole-module [`TypedProgram`] that the Core
/// pipeline can consume.
///
/// The elaborator mints a `FuncIdx` for every eta wrapper it needs by pushing
/// onto its `FnTable`, and `FuncIdx` is also this compiler's index into
/// `program.functions` (it is what `Atom::Closure` and `CallKnown` carry). So
/// `fns` is padded up to `program.functions.len()` before the walk starts, and
/// each wrapper the walk appends past that point gets its `Function` reserved
/// here in the same order. `eta_base` is the padding length: `fns[eta_base..]`
/// are exactly the wrappers, and `program.functions[eta_base..]` their reserved
/// entries.
struct Elaborated {
    program: TypedProgram,
    eta_base: usize,
}

/// One body, all the way through `lower`: the `CoreFn` itself and the arena
/// its `RTy`s index (`perceus` reads it, `emit` does not).
///
/// A module toplevel's entry-frame slot pinnings ride the IR itself: `lower`
/// copied `TypedBind::global` — stamped by `ElabCtx::global_slot` during
/// elaboration — onto each module-scope `Let`'s `CoreBind`, and
/// `emit_toplevel` reads them back off the body it emits via
/// `CoreExpr::toplevel_globals`. So a def/use mismatch against the
/// `PushGlobal <slot>` already baked into every emitted fn body is
/// unspellable: there is no side table to desync from the binds it describes.
struct LoweredBody {
    core: CoreFn,
    pool: ResolvedPool,
}

/// The value a [`Compiler::walk_tys`] slot holds between its reservation (on
/// entering an expression) and its fill (on leaving it). No `Ty` ever takes this
/// value: it indexes the engine's node arena, which cannot reach `u32::MAX`
/// entries. A slot can only still hold it if `compile_expr_inner` unwound.
const WALK_TY_PENDING: Ty = u32::MAX;

/// A region with no unfilled slot left in it. Checked wherever a region leaves
/// the compiler for the elaborator: a `WALK_TY_PENDING` that got that far would
/// index the engine's node arena out of bounds.
fn walk_region_is_filled(region: &[WalkStep]) -> bool {
    !region.contains(&WalkStep::Ty(WALK_TY_PENDING))
}

/// A `close_walk_region` with no matching `open_walk_region` — a broken walk
/// invariant. Aborts, in release as well as debug: restoring the wrong region
/// would silently hand a body another body's types — the desync
/// [`WalkStep`] exists to make impossible.
#[allow(clippy::panic)]
#[cold]
#[inline(never)]
fn walk_region_underflow() -> ! {
    panic!(
        "internal compiler error: close_walk_region without matching open_walk_region. \
         Please report this as a compiler bug."
    )
}

/// A `fn(...) {...}` expression the walk closed over: the `Function` its body
/// was reserved at, and the names its frame captured — in the order
/// `Function::capture_count` counts them and `PushCapture` indexes them.
///
/// Lives in the enclosing frame's [`Compiler::frame_closures`], because that is
/// the frame whose elaboration has to build the `Atom::Closure`: the capture
/// *values* are loads in the enclosing scope, not the lambda's.
pub(super) struct ClosureSite {
    /// Which lambda. The AST carries no node id, so a span is the only identity
    /// a node has; within one body it is a unique one, and a site is only ever
    /// looked up while elaborating the body it was recorded in.
    at: Span,
    func_idx: crate::core_ir::FuncIdx,
    captures: Vec<StrId>,
}

/// A function body parked between its typecheck walk and its elaboration.
///
/// The walk fixes everything about the body except its *types*: which
/// `Function` slot it owns, which names it captured and at which indices, and
/// where its jump-over placeholder sits. Types keep moving until the whole SCC
/// has been inferred and generalized, so `lower` — the pass that reads them —
/// runs last, off this record.
///
/// Built whole in exactly one place, [`Compiler::finish_fn_frame`], from the
/// walk half ([`ParkedBody`]) plus the frame half that only becomes final
/// there. No field is ever back-patched.
struct DeferredBody {
    name: StrId,
    param_binds: Vec<(StrId, Ty)>,
    /// Cloned rather than borrowed: `Compiler` carries no `'ast` lifetime and
    /// the deferral outlives the `&ast::Expression` `compile_fn_body` is
    /// handed. The clone preserves spans, so this body's own
    /// [`DeferredBody::closures`] still key against it, and preserves shape, so
    /// [`DeferredBody::walk_tys`] still lines up with it.
    body: ast::Expression,
    body_ty: Ty,
    /// This body's walk region: the type of every expression the check walk
    /// entered while walking `body`, in entry order, the root's first. The
    /// elaborator consumes it positionally. See [`Compiler::walk_tys`].
    walk_tys: Vec<WalkStep>,
    /// The lambdas written directly inside this body, moved out of
    /// [`Compiler::frame_closures`] the moment the walk of this body finished.
    /// Swapped back in for its elaboration and dropped with it.
    closures: Vec<ClosureSite>,
    /// `local_count` watermark after the params were bound; `Function.locals`
    /// is this maxed with Core's own slot allocation.
    param_slots: i32,
    /// Index of the placeholder `Function` this body fills in. Reserved by
    /// `finish_fn_frame` in *both* modes: a [`ClosureSite`] and `global_to_func`
    /// record it, and `lower` bakes it into `Atom::Closure`/`CallKnown`, all
    /// of which happen under `check_only` too.
    func_idx: crate::core_ir::FuncIdx,
    /// Address of `enter_fn_frame`'s jump-over. Patched by
    /// `end_deferred_elaboration` once *every* body parked in the region has
    /// been emitted, so the enclosing stream skips the whole run.
    jump_over: i32,
    /// The frame state `resolve_name` needs: a captured name must resolve to
    /// the same `Denotation::capture` index the walk assigned it, and a
    /// self-reference to the same self-closure/self-toplevel-fn denotation.
    captures: HashMap<StrId, i32>,
    capture_names: Vec<StrId>,
    rigid_ids: HashSet<i32>,
    binding: Option<StrId>,
    /// The enclosing frames' locals, exactly as `resolve_variable` saw them
    /// during the walk (module scope at index 0, then one entry per
    /// intervening fn frame). Restored wholesale at elaboration time.
    ///
    /// Truncating this to the module scope is *not* sound, even though every
    /// name reached through an intervening frame was captured by the walk:
    /// `current_binding == Some(name)` short-circuits before the capture is
    /// recorded, so a recursive local lambda whose self-name shadows a
    /// module-scope decl would re-resolve to `SelfGlobal(module_slot)` — a
    /// `PushGlobal` of the wrong value where the walk emitted `PushSelf`.
    outer_scopes: Vec<Scope>,
    /// Type-env entries `lower`'s `resolve_name` would otherwise miss: the
    /// enclosing frames' scopes are long popped by elaboration time. Only two
    /// classes of name can reach `resolve_name` from outside the module scope —
    /// a captured binding, and the frame's own self-name (`current_binding`,
    /// which resolves to `Self_`/`SelfGlobal` rather than being captured).
    /// Everything else the body mentions is either bound by `lower` itself
    /// (params, `let`s, pattern binds) or lives at module scope, which is still
    /// open. Re-pushed as one scope around the `lower` call.
    capture_env: Vec<(String, Scheme)>,
}

/// The walk half of a [`DeferredBody`]: everything [`Compiler::compile_fn_body`]
/// knows about the body it just typechecked, carried by value to the matching
/// [`Compiler::finish_fn_frame`], which adds the frame half (captures,
/// `func_idx`, jump-over) and pushes the complete `DeferredBody`.
///
/// There is exactly one outcome, and that is the point: the typecheck walk
/// cannot emit a function body, because `compile_fn_body` has nothing to hand
/// `finish_fn_frame` but this record. The phase boundary is a type, not a
/// convention — no `code_start` exists yet for anyone to spell. The deferred
/// body appends its code, fills the `Function` entry and patches the
/// jump-over, in `analyse_module`'s pass 6.
struct ParkedBody {
    name: StrId,
    param_binds: Vec<(StrId, Ty)>,
    body: ast::Expression,
    body_ty: Ty,
    walk_tys: Vec<WalkStep>,
    closures: Vec<ClosureSite>,
    param_slots: i32,
}

/// A name resolved to a constructor, as `type_ctor_pattern` needs it: the
/// facts to slot and type the pattern's arguments against. Produced by
/// `Compiler::lookup_ctor` / `lookup_ctor_qualified`.
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

/// Which toplevel `append_toplevel_init` is emitting: the entry file's or an
/// imported module's. They differ only in what happens to the toplevel's tail
/// value and Core body.
enum TopKind {
    /// `__main__`: the tail value stays on the stack for `Halt`, and the Core
    /// body is kept on `self.core` for consumers of the entry program's IR.
    Entry,
    /// An imported module: its toplevel runs for effect, so the tail is popped.
    Module,
}

/// Outcome of resolving a `module.member` property-access shape against the
/// imported qualifiers. Three-state because the callee and value positions
/// diverge on the failure modes: a non-qualified shape is recoverable (the
/// caller falls through to its general path), whereas a failed member lookup
/// has already been diagnosed and must short-circuit.
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

struct CompiledBody {
    iface: ModuleInterface,
    watermark: Watermark,
    /// Reference-graph data collected while this module's body was analysed;
    /// moved into the module's `CachedModule` by `load_module`. The module's
    /// stable 256-aligned type-id range is allocated *and* its usage recorded
    /// inside `compile_module_body` itself (via `id_base_for`/`note_id_usage`),
    /// so the range start is deliberately not threaded back out here.
    refs: ModuleReferences,
}

pub fn compile(
    expr: &ast::Expression,
    base_dir: Option<&Path>,
    pre: Option<&'static crate::static_ir::StaticStdlib>,
) -> CompileResult {
    compile_impl(expr, base_dir, false, None, pre)
}

pub fn check(
    expr: &ast::Expression,
    base_dir: Option<&Path>,
    pre: Option<&'static crate::static_ir::StaticStdlib>,
) -> CompileResult {
    compile_impl(expr, base_dir, true, None, pre)
}

/// Analyse a file *as* a specific stdlib module (used when editing
/// `src/std/**/*.al` inside the AL repo so the LSP/CLI doesn't report
/// `@vm is stdlib-only` / `Result is reserved`). Always check-only and always
/// from source (the user is editing it; the precompiled blob is stale).
pub fn check_as_module(
    expr: &ast::Expression,
    base_dir: Option<&Path>,
    module: ModulePath,
) -> CompileResult {
    compile_impl(expr, base_dir, true, Some(module), None)
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
    }
}

impl Compiler {
    /// The `DefId` of the `Type` named `name` declared in module `path`, read
    /// from the reference graph's own definitions — the single source of truth.
    ///
    /// It must *not* go through the flat `env.definitions`: every record
    /// `type Foo {..}` registers a same-named `Constructor` that overwrites the
    /// earlier `Foo => Type` entry (last-write-wins `IndexMap`), so once the
    /// defining module is fully analysed that lookup yields the constructor and
    /// no type occurrence is ever recorded. The graph keeps `Type` and
    /// `Constructor` as distinct `DefId`s under the same name, so `defs_named`
    /// resolves the type unambiguously.
    ///
    /// Same-module lookups read the live collector (the type def is emitted
    /// when its declaration is visited, before any annotation that names it is
    /// hydrated); a cross-module type reads the imported module's persisted
    /// `module_refs`. `None` for static/hydrated stdlib modules, whose type
    /// declaration spans are not persisted (a pre-existing limitation: stdlib
    /// type goto-def is out of this path's scope).
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
/// through a module qualifier. One thing, so it travels as one argument.
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
) -> CompileResult {
    let mut c = new_compiler(base_dir, check_only);
    if let Some(m) = as_module.clone() {
        // `as_module` is always a stdlib path (see `check_as_module`), whose
        // written form is its canonical identity.
        c.current_module_key = ModuleKey::for_stdlib(&m);
        c.current_module = m;
    }

    // When analysing the prelude file itself, don't load the prelude on top of
    // it (that would make every type a redefinition of itself). Other stdlib
    // modules still need the prelude for `Result`/`Nil`/etc.
    let is_prelude_self = as_module.as_deref() == Some(module::al_prelude().as_slice());
    if !is_prelude_self {
        match pre {
            Some(s) => c.seed_static(s),
            None => c.register_prelude(),
        }
        if has_errors(&c.engine.diagnostics) {
            // The seed itself failed: the user's program was never compiled,
            // so there is nothing to hand back — not even a partial one.
            return CompileResult {
                emitted: None,
                diagnostics: c.engine.diagnostics,
                references: Rc::new(ReferenceGraph::new()),
                success: false,
            };
        }
    }
    // `__main__` must include any module-init code already emitted (the
    // `MakeClosure`/`StoreLocal` sequences for pure-AL stdlib functions seeded
    // by `seed_precompiled`), so it starts at 0. With the from-source path
    // `register_prelude` emits no init code (al.al has only types and @vm fns)
    // so 0 == code.len() there too.
    let main_start = 0i32;
    // Marks for the Core re-emit below. They must sit AFTER `process_imports`
    // (which recursively compiles imported modules into the same `program` /
    // entry frame) and BEFORE this file's own `analyse_module`, so the range
    // `[code_mark, ..)` and slots `[slot_base, ..)` cover exactly the entry
    // file's fused-walk output — never an imported module's init.
    let code_mark;
    let slot_base;
    let top_ty;
    if let ast::Expression::BlockExpression(block) = expr {
        c.process_imports(block);
        c.bump_type_ids_past_reserved();
        code_mark = c.program.code.len();
        slot_base = c.local_count;
        // `toplevel_binds` accumulated every imported module's bindings during
        // `process_imports`; those slots are already live in `[0, code_mark)`
        // and are not re-emitted by the toplevel Core, so drop them so a
        // shadowing entry-file bind doesn't dequeue an import's slot.
        c.toplevel_binds.clear();
        c.toplevel_decls.clear();
        // Entry-file env scope: opened here (not inside `analyse_module`) so
        // the constructor/fn/const schemes it defines stay visible through
        // the toplevel elaboration below — otherwise a `type` decl followed
        // by a ctor use bails on `unbound callee` and the module toplevel
        // never gets Core. Popped after it.
        c.env.push_scope();
        c.analyse_module(block, None);
        top_ty = c.ty_nil();
    } else {
        code_mark = c.program.code.len();
        slot_base = c.local_count;
        c.env.push_scope();
        // `analyse_module` opens a local scope around a module's statements, so
        // its nested blocks bind at depth 2 and are frame temps, not globals.
        // A bare expression has no statement walk, so open the same scope here:
        // without it a `let` in the expression's first nested block (an `if`
        // arm) would sit at the module's own depth, `resolve_variable` would
        // call it a global, and a lambda would read a `PushGlobal` slot that
        // `bind_local` never queued and the elaborator therefore never pins.
        c.push_local_scope();
        // The same phase boundary `analyse_module` puts around its walk: a bare
        // expression program can still contain lambdas, and none of them may
        // lower or emit while the expression is being typechecked.
        c.begin_deferred_elaboration();
        top_ty = c.compile_expr(expr);
        c.end_deferred_elaboration();
        c.pop_local_scope();
        // A bare expression has no module statement walk, but its outermost
        // *block* (an `if` arm, say) is still the first one the elaborator
        // sees, so it is elaborated as though it were a module toplevel and
        // will try to drain the queue. Nothing this compile queued belongs to
        // it: seeds and imports bind no name the expression can rebind, and no
        // walk above ran with `walking_module_statements` set.
        c.toplevel_binds.clear();
        c.toplevel_decls.clear();
    }

    // Elaborate the module toplevel, lower it into Core and re-emit the entry
    // frame from it, so `__main__`'s bytecode is Core-derived like every other
    // function body (docs/core-ir-spec.md §Pipeline step 3). The elaborator
    // handles top-level `fn`/`const`/`type`/`import` decls, so it covers real
    // modules. Fn bodies were already emitted during `analyse_module` and
    // reference sibling decls via `PushGlobal <slot>` where `<slot>` is Pass 3's
    // decl-first allocation; `ElabCtx::global_slot` stamped the corresponding
    // slot onto each `TypedBind`, `lower` copied it onto each toplevel `Let`'s
    // `CoreBind`, and `emit_toplevel` pins those `Let`s to the same slots (read
    // back via `CoreExpr::toplevel_globals`) so the entry-frame `StoreLocal`s
    // line up.
    //
    // Elaboration and lowering run under `check_only` too — only the emit half
    // is skipped. A well-typed program the front half cannot handle has to be
    // reported by `al check`, not left to blow up at `al run`.
    // Drained whether or not the entry elaborates, so a `check` that bailed
    // leaves nothing behind for the next compile on this `Compiler`.
    let walk_tys = c.take_toplevel_walk_tys();
    if let Some(clean) = c.clean_module() {
        let name = c.engine.intern("__main__");
        // The wrappers land ahead of `append_toplevel_init`'s `base`, inside
        // `[code_mark, base)` — the region the entry frame's first `Jump` hops over.
        let lowered = c.elaborate_then_materialize(clean, None, |c, pool, fns| {
            // A module block was walked statement-by-statement, so nothing
            // recorded a type for the block itself; a bare expression *was*
            // entered by `compile_expr`, so its region starts with its own.
            match expr {
                ast::Expression::BlockExpression(block) => {
                    typed_ir::elaborate_toplevel(c, pool, fns, name, block, top_ty, &walk_tys)
                }
                _ => typed_ir::elaborate_body(c, pool, fns, name, &[], expr, top_ty, &walk_tys),
            }
        });
        // The entry frame's own lambdas have been read; the last consumer of
        // `frame_closures` in this compile is done with it.
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

    if !check_only {
        // Jump operands are frame-relative, so fusion has to know which frame
        // owns each instruction: `functions` is that map, and the entry frame
        // owns everything the bodies do not.
        fuse(&mut c.program.code, &c.program.functions);
    }

    let (references, _facts) = c.finalize_references();
    let success = !has_errors(&c.engine.diagnostics);
    CompileResult {
        emitted: Some(Emitted {
            program: c.program,
            core: c.core,
        }),
        diagnostics: c.engine.diagnostics,
        references,
        success,
    }
}

/// The compiler state `precompile_stdlib` freezes into the static blob:
/// the compiled program plus the scalars a fresh compiler needs to resume
/// from it.
pub(crate) struct CompilerParts {
    pub(crate) program: Program,
    pub(crate) prelude: PreludeBindings,
    pub(crate) reserved: BTreeSet<String>,
    pub(crate) next_type_id: TypeId,
    pub(crate) local_count: i32,
}

impl Compiler {
    // ========================================================================
    // Precompile accessors (used by `precompile.rs` only)
    // ========================================================================

    pub(crate) fn diagnostics(&self) -> &[Diagnostic] {
        &self.engine.diagnostics
    }
    pub(crate) fn take_module_table(&mut self) -> IndexMap<String, ModuleInterface> {
        std::mem::take(&mut self.module_table).into_loaded()
    }
    pub(crate) fn take_type_info(&mut self) -> IndexMap<String, crate::types::TypeInfo> {
        std::mem::take(&mut self.env.type_info)
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

    /// Seed from the build-time static stdlib. Per-compile work here is
    /// deliberately minimal: copy code/functions/constants (the VM mutates
    /// around them so they must be owned), hydrate the dozen prelude
    /// `TypeInfo`s and constructor `Scheme`s into root scope, and remember the
    /// `&'static StaticStdlib` so `module_table.get_or_hydrate` can lazily
    /// pull non-prelude stdlib modules on first import. Nothing is parsed or
    /// deserialized.
    pub(crate) fn seed_static(&mut self, s: &'static crate::static_ir::StaticStdlib) {
        debug_assert!(self.program.code.is_empty());
        self.static_stdlib = Some(s);
        self.module_table.set_static_fallback(s);
        self.prelude = s.prelude.clone();
        self.engine.set_prim_ids(self.prelude.prim_ids());
        self.env.set_next_type_id(s.next_type_id);
        // The static type arena IS the live arena's prefix — every `Ty`/
        // `ArenaSlice` in stdlib schemes/typeinfos indexes into it. memcpy
        // every pool + intern strings.
        self.engine.seed_arena(
            s.nodes,
            s.children,
            s.str_pool,
            s.quants,
            s.str_slices,
            s.type_params,
            s.variant_fields,
            s.variants,
        );

        let (code, functions, constants) = s.hydrate_program(&mut self.frozen);
        self.program.code = code;
        self.program.functions = functions;
        self.program.constants = constants;
        self.local_count = s.local_count;
        for slot in 0..s.local_count {
            let id = self.engine.intern(&format!("__pre{}", slot));
            self.bind_local(id, slot);
        }

        // Stdlib type-infos: copy eagerly so both the by-name map (annotation
        // resolution) and the by-id registry (nominal lookups: exhaustiveness,
        // field access, hover) hit without hydration. Seeded below the session
        // watermark, so they are never truncated and never overwritten in
        // place (a colliding user type gets its own id and its own entry).
        for (name, idx) in s.typeinfo_by_name {
            let ti = s.typeinfos[idx.0 as usize];
            self.env.type_info.insert((*name).to_string(), ti);
            self.env.type_info_by_id.insert(ti.id, ti);
        }

        // Re-export prelude constructor/@vm schemes (Some/None/Ok/Err/True/...)
        // into root scope.
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

    // ========================================================================
    // Codegen primitives. None of them consult `check_only`. The frame
    // scaffolding — the jump-over, the trailing `Ret`, the `Function` entry —
    // is laid down in every mode, so a `func_idx` denotes the same function
    // under `al check` as under `al build`. `check_only` truncates the
    // pipeline at exactly one place (`elaborate_body` returns before
    // `perceus`/`emit`) and skips the toplevel init and the peephole pass. It
    // is a suffix of the compile, not a second compilation mode.
    // ========================================================================

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
    /// truncates `program.constants` without clearing it. Every hit is therefore
    /// re-validated against the live pool (`constants.get(idx)` plus a
    /// `to_bits()` compare): a stale entry either falls out of range or fails
    /// the compare, and the constant is re-pooled.
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

    // Frozen constant-pool helpers: every constant `Value` is built through
    // `self.frozen` and then pooled.

    /// Pool a frozen Int constant.
    fn const_int(&mut self, i: i64) -> i32 {
        let v = self.frozen.int(i).into_value();
        self.add_constant(v)
    }

    /// Pool a frozen string constant (interned: every pool entry with the
    /// same contents shares one frozen allocation).
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
            // Reuse only if this slot was bound in the *current* block scope.
            // If it was bound at a strictly shallower depth it is inherited
            // from an enclosing scope, and shadowing must allocate a fresh
            // slot so the outer value survives the block.
            let inherited =
                !self.scope_marks.is_empty() && entry.depth < self.scope_marks.len() as u32;
            // At module scope (no enclosing fn frame) a re-binding must also
            // get a fresh slot. Top-level bindings live in the entry frame, so
            // a closure that captured one resolves it to `Global(slot)` and
            // reads that slot at call time; reusing the slot for the shadow
            // would overwrite the value the closure observes. AL is immutable-
            // with-shadowing — each binding is distinct — so the earlier
            // binding must survive. Nested scopes capture by value at
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

    /// Bind `name` to `slot` in the current scope without publishing the slot
    /// to the toplevel elaboration. This is the whole of binding for anything
    /// the elaborator learns a slot for by another route: a top-level
    /// declaration (whose slot travels on its [`ToplevelDecl`]) and every
    /// binding inside a function frame.
    ///
    /// The displaced entry goes on the undo log so `pop_local_scope` can
    /// restore it. Outside any open block scope there is nothing to unwind, so
    /// the log entry is skipped (the binding is captured by the next scope's
    /// mark instead).
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

    /// Whether a binding made *now* lands in the entry frame — the one
    /// definition of "global" the compiler has.
    ///
    /// `outer_scopes.is_empty()` says no function frame is open, and
    /// `scope_marks.len() <= 1` says we are at the module's own local-scope
    /// depth: `analyse_module` opens exactly one scope, imports and the
    /// prelude's slot seeds bind below it at depth 0, and every nested
    /// block/`if`/`match` at module level pushes another mark. Both halves of
    /// the global question — [`Self::bind_local`]'s queue and
    /// `resolve_variable`'s `PushGlobal` — are derived from this, recorded once
    /// on [`LocalSlot::is_global`], so they cannot drift apart.
    ///
    /// Note the bare-expression entry path (`compile` on a non-`BlockExpression`)
    /// opens a matching scope of its own, so a `let` inside one of its nested
    /// blocks is a frame temp there too.
    fn binds_a_global(&self) -> bool {
        self.outer_scopes.is_empty() && self.scope_marks.len() <= 1
    }

    /// [`Self::bind_local_raw`], plus: if this is a module-scope binding,
    /// queue its slot for the toplevel elaboration.
    ///
    /// The queue outlives `analyse_module`'s `pop_local_scope`, which the
    /// `locals` entry does not. Only bindings made by the module's own
    /// statement walk ([`Self::walking_module_statements`]) at the module's own
    /// local-scope depth qualify. The extra `walking_module_statements` term is
    /// not a second definition of "global": the queue is *positional*, and the
    /// other globals — imports, prelude seeds, `fn`/`const` declarations —
    /// reach the elaborator by another route (already-stored slots, or their
    /// own [`ToplevelDecl`]) and must not be dequeued as if they were the
    /// module's `let`s.
    fn bind_local(&mut self, name: StrId, slot: i32) {
        self.bind_local_raw(name, slot);
        if self.walking_module_statements && self.binds_a_global() {
            self.toplevel_binds.push_back(GlobalSlot(slot));
        }
    }

    /// Allocate the entry-frame slot for a top-level `fn`/`const` declaration.
    ///
    /// Unlike [`Self::get_or_create_local`] this never reuses an existing
    /// binding's slot — a module-scope rebind must not alias the slot a
    /// closure already captured by reference — and it does not queue the slot
    /// on [`Self::toplevel_binds`]: a declaration's slot reaches the
    /// elaborator on its own [`ToplevelDecl`] record instead.
    pub(super) fn alloc_decl_slot(&mut self, name: &str) -> GlobalSlot {
        let id = self.engine.intern(name);
        let idx = self.alloc_temp();
        self.bind_local_raw(id, idx);
        GlobalSlot(idx)
    }

    /// Take the entry-frame slot [`Self::bind_local`] queued for the next
    /// module-scope `let`/destructured binding. Consumed in binding order, and
    /// the toplevel elaboration visits those bindings in exactly the order the
    /// check walk bound them — both walk the module's statements in source
    /// order — so a rebound name (`x = 1; f = fn() x; x = 2`) hands each
    /// binding its own slot. No name is involved: the caller stamps the slot
    /// onto the binding it is building ([`crate::typed_ir::TypedBind::global`],
    /// then `CoreBind::global`), and no later pass maps a name back to a slot.
    ///
    /// Per-fn body elaboration also walks an outermost block; a `let x` there
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
            // Unwind in reverse bind order so a name shadowed twice in the
            // same scope lands back on its pre-scope entry. `mark` is a length
            // captured at the matching push, so it never exceeds the log.
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
    /// starting with `_` are exempt. If the same name was already tracked in
    /// this scope and never used, that earlier binding is reported now (it's
    /// been shadowed and can no longer be referenced).
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
    /// builtin, or (at a module toplevel) a declaration whose `locals` entry
    /// `analyse_module` has already unwound.
    fn resolve_variable(&mut self, name: StrId) -> Option<Denotation> {
        self.mark_used(name);
        if let Some(entry) = self.locals.get(&name) {
            return Some(Denotation::slot(FrameSlot(entry.slot)));
        }
        if let Some(idx) = self.captures.get(&name) {
            debug_assert!(*idx >= 0, "capture index is a Vec index");
            return Some(Denotation::capture(CaptureIdx(*idx)));
        }
        // Search enclosing scopes innermost-first so inner bindings shadow
        // outer ones. The bottom of the stack (index 0) is always the entry
        // frame, and its *module-scope* locals are the program's globals:
        // `StoreLocal` publishes them (see `Op::StoreLocal` in the VM), so a
        // nested fn loads them with `PushGlobal` at call time instead of
        // capturing by value. This is what makes mutually-recursive top-level
        // fns work — the sibling's slot is read at call time, not at
        // `MakeClosure` time.
        //
        // A local bound in a nested block/if/match scope at module level is
        // *not* a global: it is an ordinary entry-frame temp whose slot the
        // toplevel Core emit assigns for itself (only module-scope-depth binds
        // are queued on `toplevel_binds`, see `bind_local`), and nothing
        // guarantees it is stored before a `PushGlobal` of it runs. Such a
        // name is captured by value like any other enclosing local — exactly
        // as it would be inside a fn body. Which of the two a name is was
        // decided once, by [`Self::binds_a_global`], and recorded on the
        // binding; re-deriving it here from `depth` is how the loader and the
        // binder came to disagree on the bare-expression entry path.
        for (i, scope) in self.outer_scopes.iter().enumerate().rev() {
            if let Some(entry) = scope.locals.get(&name) {
                let is_global = i == 0 && entry.is_global;
                if self.current_binding == Some(name) {
                    // A top-level fn's self-name resolves to its entry-frame
                    // slot so a value load emits `PushGlobal`; `PushSelf` would
                    // read the sentinel `captures` a `CallKnown` frame carries.
                    // `Denotation` fixes both halves together.
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

    /// The denotation of one of *this module's* top-level declarations, read
    /// off the same [`ToplevelDecl`] record whose `slot` the elaborated
    /// definition is pinned to. A linear scan over the module's own decls: a
    /// handful, walked once per forward reference.
    fn decl_denotation(&self, name: StrId) -> Option<Denotation> {
        let decl = self.toplevel_decls.iter().find(|d| d.name == name)?;
        Some(self.global_denotation(decl.slot))
    }

    // ========================================================================
    // Diagnostics & LSP recording
    // ========================================================================

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
    /// `None` means some subtree is poisoned and nothing may be elaborated: the
    /// user has already been told why, and a body the checker rejected is
    /// closed out empty rather than handed to `typed_ir::elaborate_body`.
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
        // Hover facts are only consumed by `IncrementalSession` (the LSP). On
        // the free `compile`/`check` path they are resolved then discarded, so
        // skip the `name.to_string()` + `RawRef` buffering entirely there.
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
    /// so a given slice always denotes the same path (hence the same id); the
    /// memo is dropped in `reset_to`, the single point the pool is rewound.
    /// Replaces a per-occurrence `strs_of` (`Vec<String>` + a `String` per
    /// segment) plus `ref_interner.intern` (a `ModuleKey::of` joined `String`
    /// and a string-hash probe) with one `ArenaSlice`-keyed probe on a cache
    /// hit.
    fn module_id_of_slice(&mut self, sl: ArenaSlice<pool::StrSlices>) -> ModuleId {
        if let Some(&id) = self.defid_module_memo.get(&sl) {
            return id;
        }
        let path = self.engine.strs_of(sl);
        let id = self.ref_interner.intern(&path);
        self.defid_module_memo.insert(sl, id);
        id
    }

    /// The interned `ModuleId` of `current_module`, reusing the memoised
    /// module-path slice so the hot recording path neither clones the
    /// `ModulePath` nor re-interns it.
    fn current_module_id(&mut self) -> ModuleId {
        let sl = self.current_module_slice();
        self.module_id_of_slice(sl)
    }

    /// Resolve an env-side [`DefinitionLocation`] (declaring span + module
    /// `ArenaSlice` + [`EntityKind`]) to a stable reference-graph [`DefId`].
    /// The module slice is re-interned through the persistent `ref_interner`
    /// so the id survives incremental recompiles, and the declaring span is
    /// reconstructed single-line so a definition's `DefId` is bit-identical to
    /// the `DefId` every occurrence of it targets.
    pub(super) fn defid_of(&mut self, dl: DefinitionLocation) -> DefId {
        let module = self.module_id_of_slice(dl.module);
        DefId::new(module, dl.span(), dl.entity)
    }

    /// The [`DefId`] to stash in `current_owner` while a top-level
    /// `fn`/`const`/`type` body is compiled. Every reference emitted inside
    /// that body — including inside nested lambdas, which have no `DefId` of
    /// their own — is attributed to this owner: the def→def edge channel the
    /// dead-code reachability walk follows.
    pub(super) fn owner_defid(&mut self, name_span: Span, entity: EntityKind) -> DefId {
        let module = self.current_module_slice();
        self.defid_of(DefinitionLocation::new(name_span, module, entity))
    }

    /// Record a resolved name occurrence in the current module's collector,
    /// attributed to the definition whose body is currently being compiled
    /// (`current_owner`; `None` for genuine top-level executed code). The
    /// owner is the def→def edge channel the dead-code reachability walk
    /// follows: without it every reference in a `fn`/`const`/`type` body
    /// looks like top-level executed code, so anything it names is treated as
    /// a live root and no transitively-dead private definition is ever
    /// reported. Recording-only: never touches inference, schemes, slots, or
    /// diagnostics.
    fn record_ref(&mut self, occ: Span, kind: ReferenceKind, target: DefId) {
        self.module_refs
            .add_reference(self.current_owner, Reference::new(occ, kind, target));
    }

    /// Resolve a value name's canonical [`DefinitionLocation`] (carried on its
    /// `Scheme.def`) to a [`DefId`] and record an occurrence of `kind` at
    /// `occ`. No-op when the name has no recorded definition; builtins behave
    /// as external targets (harmless — they own no definition in the graph so
    /// the edge dangles and never affects reachability).
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
    /// use was reached through. The qualifier names the import, not a value:
    /// the occurrence targets the `ModuleAlias` binding so hover and goto-def
    /// on the `b` of `b.add(..)` reach module `b`. Non-use-site, so
    /// find-references / rename / unused-import are unaffected: the alias's
    /// liveness rides on the member's own `Qualified` occurrence, which the
    /// caller records. The alias `Definition` was registered under the
    /// qualifier's name by `process_import`, so no extra bookkeeping is needed
    /// to find it; an unknown qualifier records nothing.
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

    /// Resolve a type name in `path`'s module to its canonical `Type`
    /// definition and record an occurrence of `kind` at `occ`. No-op when the
    /// module has no such type (e.g. static/hydrated stdlib modules). Mirror of
    /// [`Self::record_value_use`] for type references.
    fn record_type_use(&mut self, path: &ModulePath, name: &str, occ: Span, kind: ReferenceKind) {
        if let Some(target) = self.type_defid_in_module(path, name) {
            self.record_ref(occ, kind, target);
        }
    }

    /// Record a declaration in the current module's reference collector, plus
    /// a single `Definition`-kind self-occurrence at the declaring name so
    /// find-references includes the declaration site. The `Definition` record
    /// is last-write-wins on its `DefId` (a definition may be re-emitted as
    /// more is learned about it — docs/visibility settle on the last write);
    /// the self-occurrence is emitted only on first sight so the declaration
    /// is not double-counted. The self-occurrence is owned by the definition
    /// itself (not `None`) so it is never mistaken for top-level executed code
    /// — a self-edge can never bootstrap a def's own reachability, so a dead
    /// definition stays dead. Recording-only: never touches inference,
    /// schemes, slots, or diagnostics.
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
    /// goto-def / find-refs / hover resolve on it and its uses. LSP-only: the
    /// graph is built on every compile, but `al run`/`al check` never query
    /// locals, and `EntityKind::Value` defs are ignored by the dead-code pass,
    /// so the `collect_hover_facts` gate keeps this off the hot path.
    fn emit_value_def(&mut self, sp: Span, name: &str, doc: Option<String>) {
        if self.collect_hover_facts {
            let m = self.current_module_slice();
            self.emit_def(
                DefinitionLocation::new(sp, m, EntityKind::Value),
                name,
                doc,
                false,
                DefinitionKind::Value,
            );
        }
    }

    /// Thin wrappers over the engine's `mk_con` for prelude types, fed by the
    /// captured `PreludeBindings` so the type identity (not just the name
    /// string) is the source of truth where it matters. The nullary ones go
    /// through the engine's per-primitive cache so a recompile mints one `Int`
    /// node instead of one per literal.
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
    /// hydrator resolved is drained as an `Unqualified` occurrence of that
    /// type's declaration — whether hydration succeeded or not, since a nested
    /// name can resolve before a later sibling fails. The target is the owning
    /// module's canonical `Type` definition (resolved through the reference
    /// graph, not the constructor-clobbered `env.definitions`), so an
    /// occurrence and its declaration share one `DefId`. Recording-only: the
    /// returned `Ty` is byte-for-byte what the old code returned.
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

    // ========================================================================
    // Node dispatch
    // ========================================================================

    pub(super) fn compile_node(&mut self, node: &ast::Node) -> Ty {
        match node {
            ast::Node::Statement(s) => {
                self.compile_statement(s);
                self.ty_nil()
            }
            ast::Node::Expression(e) => self.compile_expr(e),
        }
    }

    // ========================================================================
    // Statements
    // ========================================================================

    fn compile_statement(&mut self, stmt: &ast::Statement) {
        match stmt {
            ast::Statement::VariableBinding(vb) => {
                let name = vb.identifier.name.clone();
                let name_id = self.engine.intern(&name);

                // At module scope a re-binding gets a fresh slot (see
                // `get_or_create_local`), so the slot must be allocated *after*
                // the initializer compiles — otherwise the name would already
                // resolve to the new, still-uninitialised slot and an init that
                // reads the prior binding (e.g. `x = x + 1`) would read garbage.
                // The prior binding stays in scope across the init, which also
                // drives `Self_` resolution for a self-recursive lambda. A first
                // binding still reserves its slot up front so a self-recursive
                // lambda can resolve its own (not-yet-bound) name through the
                // entry frame.
                let defer_slot = self.outer_scopes.is_empty() && self.locals.contains_key(&name_id);
                if !defer_slot {
                    self.get_or_create_local(&name);
                }

                let annot_ty = vb.typ.as_ref().map(|a| self.hydrate_annotation(a));

                self.engine.enter_level();
                let self_ty = self.engine.fresh_var();
                let init_is_fn = matches!(vb.init, ast::Expression::FunctionExpression(_));
                // Only pre-bind for recursive lambdas. For any other init, exposing
                // the name to its own initializer lets `x = x` infer ⊥ and generalize
                // to ∀A.A, which is unsound.
                if init_is_fn {
                    self.env.define(&name, mono(self_ty));
                }

                // Deliver the self-name to the lambda's own frame via a one-shot
                // consumed by its `enter_fn_frame` — NOT by mutating
                // `current_binding` here, which leaks into nested / HOF-arg
                // lambdas and mis-resolves their calls to the enclosing fn as
                // Self_ (emitting CallSelf against the wrong, live frame).
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
                // and discard the value. A bare `UpperIdent` here names a
                // *type*, not a value; if the name resolves to a constructor
                // but not a type (`Some = ...`, `Ok = ...`), say so directly
                // rather than letting the hydrator report "Unknown type".
                let name = &td.ty_name.name;
                let expected = if self.env.lookup_type_info(name).is_none()
                    && matches!(
                        self.env.lookup(name).map(|s| s.kind),
                        Some(ValueKind::Constructor { .. })
                    ) {
                    self.error(format!("'{name}' is not a type"), td.ty_name.span);
                    self.engine.fresh_var()
                } else {
                    // Hydrate the name as a no-arg annotation — the hydrator
                    // rejects unknown names and arity misuse, and records the
                    // type occurrence for the reference graph.
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
                // `Ctor(p1, ..) = expr` — an irrefutable single-arm destructure.
                // Lowered like a one-arm match: type the pattern against the
                // init, bind its names, then require the single pattern to be
                // exhaustive (so only single-constructor types qualify; a
                // multi-constructor or nested-optional pattern is refutable).
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
                // Imports are processed at the start of compilation, before
                // walking the body. By the time we reach one here it's already
                // been resolved (or rejected) — nothing left to do.
            }
        }
    }

    // ========================================================================
    // Modules
    // ========================================================================

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

    /// A cache-hit dependency reserved its id range but contributed nothing to
    /// `next_type_id` this pass, so bump the entry file past every reserved
    /// block before it allocates any of its own type ids.
    pub(super) fn bump_type_ids_past_reserved(&mut self) {
        let hw = self.module_table.id_high_water();
        let cur = self.env.next_type_id();
        if hw > cur {
            self.env.set_next_type_id(hw);
        }
    }

    fn process_import(&mut self, imp: &ast::ImportDeclaration) {
        // `canon` is the module's identity — the file it resolved to. Every
        // downstream key (module table, reference interner, qualifier map) must
        // use it, never `imp.path`, which is only how *this* file spelled it.
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

        // Reference graph: the qualifier binding is a `ModuleAlias`
        // definition; the final module-name path segment is an `Import`
        // occurrence and the `as` alias an `Alias` occurrence, both targeting
        // it so qualified `q.member` uses, find-references, and rename resolve
        // back to this import (and the unused-import reachability rule can see
        // it). The `Import` occ is recorded at `imp.path_span` — the final
        // segment only — so the `import` keyword and earlier path segments
        // (`al` / `/` in `import al/string`) are not clickable. The alias
        // `Definition` also stores the full declaration span (`decl_span`) and
        // the imported `ModuleId` (`imports_module`) so unused-import detection
        // reads both directly instead of recovering them from occurrence
        // geometry.
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
        // The module-name segment points at the *imported* module, so its
        // `Import` occurrence targets a `DefId` owned by that module: goto-def
        // on the `b` in `import a/b` lands on `b`'s file, not the importing
        // alias's own binding (a no-op self-jump). find-references / rename / the
        // unused-import rule all key off the alias `Definition` above, so
        // retargeting only the occurrence changes nothing for them.
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

        // Per-item occurrences resolved through the imported module's
        // interface. Collected with owned `DefinitionLocation`s / type names
        // while `iface` borrows `module_table`, then turned into `DefId`s and
        // recorded once that borrow has ended.
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
            // A record `type Foo {..}` exports *both* a same-named constructor
            // value and the type itself. They are not mutually exclusive: bind
            // and record each independently so importing the name serves
            // value- *and* type-level resolution (goto-def / find-refs /
            // rename) for both. Resolving the type via an `else if` after the
            // value branch — as the old code did — made it unreachable for
            // every record type, since the constructor value always matched
            // first.
            let val = iface.values.get(&item.name.name).cloned();
            let typ = iface.types.get(&item.name.name).cloned();
            let is_private = iface.private_names.contains(&item.name.name);
            if let Some(ev) = val.clone() {
                let vdef = ev.scheme.def;
                // The `X` token of `{X}` / `{X as Y}` always names the imported
                // symbol, so it targets X's canonical def — goto-def on it chains
                // to the real declaration, and renaming X rewrites it. It is a
                // binding token, not an evaluating use, hence `ImportItem`.
                if let Some(dl) = vdef {
                    item_refs.push((item.name.span, ReferenceKind::ImportItem, dl));
                }
                // An aliased item `{X as Y}` introduces a *new* local name Y.
                // Mint Y its own DefId so its rename class is separate from X's:
                // sharing X's def made renaming X rewrite Y's binder and every Y
                // use to X's new name (name capture / broken compile), and made
                // renaming a Y use escape into X's module. Y's env binding and the
                // `Y` alias token target this local def; type/kind/slot are still
                // inherited from X. The `Value` entity keeps it off the dead-code
                // surface (the import's own unused-ness is reported on the
                // `ModuleAlias`) and out of the symbol outline.
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
                    let mut def = Definition::new(
                        alias_defid.module,
                        alias_defid.span,
                        local_name.clone(),
                        None,
                        false,
                        DefinitionKind::Value,
                    );
                    // goto-def / hover on Y chains to X's real declaration; rename
                    // and find-references stay on this alias.
                    def.alias_of = canonical;
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
                // Bind the imported type unconditionally so annotation
                // hydration resolves it through this module's env rather than
                // residual global `type_info` left over from the dependency's
                // own compile. Its canonical `Type` `DefId` is resolved
                // (module-aware, after the loop) from the imported module's
                // reference graph, never the constructor-clobbered
                // `env.definitions`.
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
            // The type's declaring module is the *resolved* file: looking it
            // up under `imp.path` as written would miss the cache for a
            // relative import and silently record nothing.
            self.record_type_use(&canon, &tyname, occ, kind);
        }
    }

    /// Load the module `path` names, relative to the *importing* module's
    /// directory, and return its canonical identity.
    ///
    /// Resolution happens **before** the cache is consulted, and the key comes
    /// from the file we resolved to — never from `path` as written. Keying on
    /// the written path made `./b` mean one module program-wide: a `sub/mid.al`
    /// importing `./b` silently received the root's `b.al`. See
    /// `module::file_module_path`.
    /// How to name a module in a diagnostic: the path the importer wrote
    /// (`./lib`), falling back to the module's own last segment. Never the
    /// canonical key — that is an identity, not a name.
    pub(super) fn module_name(&self, key: &ModuleKey) -> String {
        self.module_display.get(key).cloned().unwrap_or_else(|| {
            let k = key.as_str();
            k.rsplit('/').next().unwrap_or(k).to_string()
        })
    }

    pub(crate) fn load_module(
        &mut self,
        path: &ast::ImportPath,
        at: Span,
    ) -> Option<(ModulePath, ModuleKey)> {
        let resolved = match module::resolve(path, self.base_dir.as_deref()) {
            Ok(r) => r,
            Err(ResolveError::FileNotFound(p)) => {
                self.module_error(format!("file not found: {}", p.display()), at);
                return None;
            }
            Err(ResolveError::NoSuchStdlibModule(p)) => {
                self.module_error(format!("no such stdlib module {}", p.join("/")), at);
                return None;
            }
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
        let mut parser = crate::parser::new_parser(&mut sc);
        let parsed = parser.parse_program();
        for d in parsed.diagnostics {
            self.engine.diagnostics.push(Diagnostic {
                span: at,
                severity: d.severity,
                code: d.code,
                message: format!("In module '{key}': {}", d.message),
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
        // The watermark is captured *after* this module's own dependencies have
        // loaded (compile_module_body does that first) but reflects the arena
        // state immediately *before* this module's body added anything. Only
        // on-disk modules are ever invalidated, so only they carry it.
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

    /// Snapshot the enclosing module's per-module state into a `ModuleFrame`
    /// and swap in fresh state for compiling `path`'s body. A fresh
    /// `ModuleReferences` collector is installed (the typecheck pass populates
    /// it; `leave_module_frame` moves the result out for the caller to store
    /// on the module's `CachedModule`). `module_path_slice` is a memo of
    /// `current_module`, so it is cleared rather than carried over.
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
    /// module's collected references (the `module_refs` value that was live
    /// between enter and leave).
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
        let old = self.enter_module_frame(path, key.clone(), base_dir);
        // The fresh collector installed above belongs to this module, so its
        // doc goes here rather than on the enclosing module's.
        self.module_refs.set_doc(doc);

        self.process_imports(block);
        let imports: Vec<ModuleKey> = self.imported_qualifiers.values().cloned().collect();
        // Reserve (or reuse) this module's stable 256-aligned type-id range
        // *before* the watermark is captured, so the captured watermark's
        // `env.next_type_id == base`. On invalidation `reset_to`/`truncate_to`
        // restores `next_type_id` from that watermark, putting the module back
        // at its own range start — editing one module never shifts another's
        // ids. Deps have already loaded (and reserved their ranges) via the
        // `process_imports` recursion above, so the floor (current
        // `next_type_id`) already sits past every stdlib + dependency id.
        // `key` is the same key `load_module` caches this module under.
        let floor = self.env.next_type_id();
        let reservation = self.module_table.id_base_for(&key, floor);
        let base = reservation.base();
        self.env.set_next_type_id(base);
        let watermark = self.watermark();
        // `toplevel_binds`/`toplevel_decls` drive the Core toplevel lower
        // below; reset them so they carry only *this* module's binds, not
        // those of the dependencies `process_imports` just compiled.
        self.toplevel_binds.clear();
        self.toplevel_decls.clear();
        let code_mark = self.program.code.len();
        let slot_base = self.local_count;
        self.env.push_scope();
        self.analyse_module(block, Some(&mut iface));
        self.emit_module_init(&key, block, code_mark, slot_base);
        self.env.pop_scope();
        // Record actual type-id consumption so `id_high_water` tracks real
        // usage and a reused-range spill past `MODULE_TYPE_ID_RANGE` raises
        // `id_range_overflow` for the invalidate-all/reset recovery path.
        let used = self.env.next_type_id().0 - base.0;
        reservation.note_usage(&mut self.module_table, used);

        let refs = self.leave_module_frame(old);
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
    /// entry-frame init. Shared by `__main__` and every imported module.
    ///
    /// The analysis pass laid the toplevel's function bodies down in
    /// `[code_mark, base)`; each is preceded by a jump-over so the enclosing
    /// stream skips it. Overwriting the first of those with `Jump base` skips
    /// the whole run in one hop and lands on the Core-derived init, which then
    /// falls through to the next region. Bodies keep the addresses their
    /// `Function.code_start` recorded and stay reachable via `CallKnown`;
    /// truncating instead would orphan every one of them.
    ///
    /// See [`TopKind`] for how the two callers differ in their handling of the
    /// toplevel's tail value.
    fn append_toplevel_init(
        &mut self,
        lowered: LoweredBody,
        code_mark: usize,
        slot_base: i32,
        kind: TopKind,
    ) {
        use crate::core_ir::{emit, perceus};
        // The elaboration that produced `lowered` drained the queue the check
        // walk filled. A leftover means the two walks disagreed about which
        // statements bind at module scope — the failure the old name-keyed map
        // could not represent, and one that silently mis-slots every bind after
        // the point of disagreement.
        debug_assert!(
            self.toplevel_binds.is_empty(),
            "toplevel elaboration left {} module-scope slot(s) unclaimed",
            self.toplevel_binds.len()
        );
        let LoweredBody { core: top, pool } = lowered;
        // Perceus runs so *temporaries* passed into calls are moved (rc==1 in
        // the callee → its own reuse fires); `emit_toplevel` suppresses the
        // resulting `Drop`/`Reuse` for the pinned globals, whose last use in
        // the toplevel is not their last use in the program.
        let top = perceus::perceus(&pool, top);
        if std::env::var("CORE_DBG").is_ok() {
            eprintln!("=== {}\n{top}", self.engine.str(top.name));
        }
        let base = self.program.code.len() as i32;
        let mut out = emit::emit_toplevel(&top.body, slot_base, self);
        // A function body links by plain append because its block starts at its
        // own `Function.code_start`. This block does not: it is spliced into the
        // *entry* frame, whose `code_start` is 0 (it must run the module-init
        // code that precedes every body), and it starts at `base`. The VM
        // resolves its jumps as `0 + operand`, so rebase them by `base` here.
        // This is the one place an operand is ever rewritten.
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
        // This module's toplevel lambdas, and nobody else's. Taken (not merely
        // read) so the next module's toplevel — whose `Span`s are numbered from
        // its own file — cannot find one of ours at the same offset. Dropped on
        // the early return too: an unelaborated toplevel has no reader.
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
        // Elaborated and lowered even under `check_only` (emit is not) so a
        // module toplevel the front half cannot handle is a diagnostic rather
        // than a crash at `al run`. Same rule as `elaborate_body`: the wrappers
        // go down before anyone reads an address, here
        // `append_toplevel_init`'s `base`.
        let lowered = self.elaborate_then_materialize(clean, None, |c, pool, fns| {
            typed_ir::elaborate_toplevel(c, pool, fns, name, block, nil, &walk_tys)
        });
        self.frame_closures.clear();
        if !self.check_only {
            self.append_toplevel_init(lowered, code_mark, slot_base, TopKind::Module);
        }
    }

    /// Look up a `qualifier.member` reference where `qualifier` is an imported
    /// module name. Returns the value's scheme and (if it has a runtime
    /// binding) its slot in the entry frame.
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

    // ========================================================================
    // Expressions
    // ========================================================================

    /// Typecheck `expr`, appending the type it inferred to the current walk
    /// region in *entry* order.
    ///
    /// The recording is what lets the elaborator — which walks the same AST
    /// straight after this one, on the same engine — use the typechecker's
    /// answer rather than re-deriving one. It must stay total over expressions:
    /// every `Expression` the elaborator can reach is typed here, and the two
    /// walks enter them in the same order (see [`Compiler::walk_tys`]).
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

    /// Fill the slot [`Self::reserve_walk_ty`] claimed. `compile_expr_inner` may
    /// have opened and closed a nested region (a lambda body), but it always
    /// puts the one it found back, so the slot still addresses it.
    ///
    /// Indexes rather than probes: a slot outside the current region is the
    /// region desync this table exists to make impossible, and swallowing the
    /// write would let a `WALK_TY_PENDING` reach the elaborator.
    fn fill_walk_ty(&mut self, slot: usize, ty: Ty) {
        self.walk_tys[slot] = WalkStep::Ty(ty);
    }

    /// Record the check walk's `module.member` verdict for the `left.right`
    /// shape it is looking at. Consumed by `Elab::qualified`, which must not
    /// re-derive it — see [`WalkStep::Qualified`].
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

    /// Take the module toplevel's region: everything the walk typed outside a
    /// function body. Every module's walk fills it and exactly one elaboration
    /// drains it, so it is empty again for the next.
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
                // `const_number` is the single source of the int/float split
                // (and of the overflow diagnostic); the pooled `Value` itself
                // is re-interned by the Core emit.
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

    /// An `ErrorNode` stands in for a region the parser could not read, so it
    /// has no meaning to check and none to elaborate. Every parser site that
    /// builds one also records the error, and the entry file's parse
    /// diagnostics never enter `engine.diagnostics` — so restate it here, but
    /// only when nothing else has already denied the module its [`CleanModule`]
    /// proof. That keeps the diagnostic set a user sees identical (the CLI
    /// stops before checking a file that failed to parse; the LSP publishes
    /// only the parse errors for such a buffer; an imported module's parse
    /// errors are folded in before its body is walked) while making the one
    /// unelaborable form in the language a checked error rather than a bug
    /// report from a later pass.
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

    /// Compile an expression with an optional expected-type hint. The hint is
    /// only consulted for fn-literal arguments where the parameter type is
    /// known and can be pushed down so unannotated lambda params get a
    /// concrete type immediately (avoiding "cannot access field on unknown
    /// type" inside the body).
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
                // This path bypasses `compile_expr`, so claim the lambda's slot
                // in the walk region here — before its body opens a region of
                // its own — or the elaborator's next `take_ty` reads the type of
                // whatever expression follows.
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

    // ========================================================================
    // Identifier
    // ========================================================================

    /// Does this value reference have a runtime binding? `Local`/`ModuleFn`
    /// resolve the variable (with the usual `mark_used` + capture side
    /// effects), while constructors and builtins have no plain runtime binding.
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
        // `record` discards everything on the non-LSP path (see its early
        // return); skip the doc map probe + clone entirely there.
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

    /// Diagnose a named value used in value position. Constructors are legal
    /// (the Core emit synthesises a nullary build or an eta wrapper); builtins
    /// are not, and a `Local`/`ModuleFn` with no runtime binding is an error.
    fn check_named_value(
        &mut self,
        name: &str,
        qualifier: Option<&str>,
        sp: Span,
        kind: ValueKind,
        has_binding: bool,
    ) {
        match kind {
            ValueKind::Constructor { .. } => {}
            ValueKind::Builtin { .. } => self.error_builtin_as_value(name, sp),
            ValueKind::Local | ValueKind::ModuleFn => {
                if !has_binding {
                    self.no_runtime_binding(name, qualifier, sp);
                }
            }
        }
    }

    fn error_builtin_as_value(&mut self, name: &str, sp: Span) {
        self.error(
            format!("'{}' is a builtin and cannot be used as a value", name),
            sp,
        );
    }

    // ========================================================================
    // Binary ops
    // ========================================================================

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

    // ========================================================================
    // Binary literals  ( `<<v:size:kind, ..>>` )
    // ========================================================================

    /// Type-check a `<<v:size:kind, ..>>` literal. Every segment's value (and
    /// its runtime size expression) is checked against the kind's expected
    /// type; the encoding itself is Core's job.
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

    // ========================================================================
    // Array
    // ========================================================================

    /// Compile a run of non-spread array elements, unifying each with the
    /// array's element type variable.
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

    // ========================================================================
    // Function call
    // ========================================================================

    fn compile_call(&mut self, expr: &ast::FunctionCallExpression) -> Ty {
        // Resolve the callee to a name + scheme + load action without emitting
        // anything yet. This lets us dispatch on `ValueKind` for constructors
        // and builtins, and use the callee's known type to push hints into
        // function-literal arguments.
        if let Some((name, name_span, scheme, has_binding)) =
            self.resolve_simple_callee(&expr.callee)
        {
            let inst_ty = self.engine.instantiate(&scheme, &self.rigid_ids);
            // Hover-only here; the graph occurrence (with the correct
            // Unqualified/Qualified kind) is emitted in `resolve_simple_callee`.
            // Skip the doc map probe + clone on the non-LSP path, where
            // `record` discards it (see its early return).
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
                        self.no_runtime_binding(name, None, name_span);
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

    /// Type-check and emit a positional argument list against a callee type
    /// using `match_fun_type`. Labeled and spread arguments are rejected here
    /// since only constructor calls support them.
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

    /// Resolve a `module.member` property-access shape against the imported
    /// qualifiers: look the member up, note whether it has a runtime binding,
    /// and record the `Qualified` value-use. Shared by the callee position
    /// (`resolve_simple_callee`) and the value position
    /// (`compile_property_access`), which diverge only on how they treat the
    /// two failure modes — see [`QualifiedMember`].
    ///
    /// The verdict is recorded into the walk region on the way out: whether the
    /// qualifier was entered is the one traversal decision the elaborator cannot
    /// re-derive, because it turns on `self.env` as it stands *here* and a
    /// deferred body is elaborated long after. Both call sites go through this
    /// wrapper, and `Elab::qualified` consumes exactly one verdict per call, so
    /// the two walks cannot disagree.
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
        // An import binds its qualifier at module scope; any value binding of
        // the same name — a top-level decl, a parameter, a `let`, a match
        // binder — shadows it. `env` holds exactly those bindings (the
        // qualifier itself never enters it), so a hit here means `left` names a
        // value and `left.member` is an ordinary field access, not a qualified
        // module member. Checked before the `imported_qualifiers` probe so the
        // shadowed case records no `Qualified`/`Qualifier` occurrence either.
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

    /// Best-effort resolution of a callee expression that is a plain name (or
    /// `module.name`). Returns the looked-up scheme and whether the callee has
    /// a runtime binding. Anything else falls through to the
    /// arbitrary-expression path.
    fn resolve_simple_callee<'a>(
        &mut self,
        callee: &'a ast::Expression,
    ) -> Option<(&'a str, Span, Scheme, bool)> {
        match callee {
            ast::Expression::Identifier(id) => {
                let scheme = *self.env.lookup(&id.name)?;
                let has_binding = self.has_binding(&id.name, &scheme.kind);
                // Unqualified callee `name()` — the genuine unqualified-use
                // seam for calls (this path bypasses `compile_identifier`).
                // `record` in `compile_call` is hover-only.
                self.record_value_use(scheme.def, id.span, ReferenceKind::Unqualified);
                Some((&id.name, id.span, scheme, has_binding))
            }
            // Qualified callee `module.member()`. Both failure modes mean "no
            // simple callee" (a failed lookup has already been diagnosed by
            // `resolve_qualified_member`).
            ast::Expression::PropertyAccessExpression(pa) => {
                match self.resolve_qualified_member(pa) {
                    QualifiedMember::NotQualified | QualifiedMember::LookupFailed => None,
                    QualifiedMember::Resolved {
                        member_name,
                        member_span,
                        scheme,
                        has_binding,
                        ..
                    } => Some((member_name, member_span, scheme, has_binding)),
                }
            }
            _ => None,
        }
    }

    /// Compile a call where the callee is a data constructor. Handles
    /// positional, labelled, and `..base` (record-update) argument forms,
    /// reordering into field-declaration order before emission.
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
        // The instantiated scheme is `IFun{params, ret}` for arity>0 or `ICon`
        // for arity==0. Extract param/result types.
        let r = self.engine.find(inst_ty);
        let (param_tys, result_ty) = match self.engine.node(r) {
            TypeNode::Fun { params, ret } => (self.engine.children_of(params).to_vec(), ret),
            _ => (vec![], r),
        };

        // Partition the arguments into a spread base (at most one) and a map
        // of explicitly-supplied fields keyed by their declared position. Each
        // expression is paired with its source-argument index so a slotted
        // value structurally remembers which `args[i]` it came from — the
        // walk sub-regions below are spliced back by that index.
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
        // *Checking* runs spread-first and then in declared-field order, as it
        // always has: the spread's `unify_at` solves `result_ty` (and with it
        // the constructor's type params) before any `compile_expr_with_hint`
        // pushes a parameter type into a function-literal argument, and the
        // diagnostics a mistyped argument raises come out in field order.
        //
        // *Recording* is source order: the elaborator's `ctor_call` spills the
        // arguments into `Let`s in source order before reordering them, and the
        // walk region is consumed positionally. So each argument is checked into
        // a sub-region of its own, and the sub-regions are spliced back in
        // source order below.
        //
        // An argument `slot_ctor_args` did not place is an arity or label error
        // it has already reported — left unchecked, and contributing no region,
        // exactly as before.
        let slots = &by_pos[..(field_labels_sl.len as usize).min(by_pos.len())];

        // All-positional: `slot_ctor_args` placed `args[i]` in slot `i`, so the
        // two orders already coincide and no sub-region is needed. This is every
        // ctor call that does not name a field or spread a base.
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

    // ========================================================================
    // Property access
    // ========================================================================

    fn compile_property_access(&mut self, expr: &ast::PropertyAccessExpression) -> Ty {
        // `module.member` qualified access (no call — calls are handled in
        // compile_call via resolve_simple_callee).
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
                self.check_named_value(
                    member_name,
                    Some(module_key.as_str()),
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
    /// `TupleIndex` must address, plus the field's instantiated type. A field
    /// is only projectable when EVERY variant carries that label at the same
    /// position and at a unifiable type.
    ///
    /// `unify_span` is `Some` on the typecheck path, where the per-variant
    /// field types still have to be unified (and any mismatch reported); the
    /// lower path passes `None` because typecheck has already run.
    fn field_in_variants(
        &mut self,
        info: TypeInfo,
        type_args: &[Ty],
        field_id: StrId,
        unify_span: Option<Span>,
    ) -> Result<(usize, Ty), FieldMismatch> {
        let variants = info.variants().ok_or(FieldMismatch::NotNominal)?;
        let mut found: Option<(usize, Ty)> = None;
        // Variant/VariantField are Copy and the pools are append-only, so copy
        // each entry out by index instead of `to_vec()`ing the slices to
        // survive the `&mut engine` calls in the loop body.
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

    /// Project `.field` out of a value. Because the runtime value's variant is
    /// not statically known, the field is only accessible if EVERY variant of
    /// the receiver type carries a label of that name and at the same type.
    ///
    /// Delegates the walk to [`Compiler::field_in_variants`], which lower also
    /// uses, so the typecheck-approved slot index and the emitted `TupleIndex`
    /// can never disagree.
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

        // Nominal lookup: the receiver's variants come from the type the Con
        // node identifies, never from whatever same-named type a flat name
        // lookup would find first.
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
        // `Type.field` is a dotted key in `env.definitions` (no local can
        // shadow it), so resolving it here is safe and keeps goto-def /
        // find-references / rename working for record fields, and feeds the
        // field-level dead-code reachability walk.
        let fdef = self.env.lookup_definition(&qualified);
        self.record_value_use(fdef, field_span, ReferenceKind::Unqualified);
        result_ty
    }

    // ========================================================================
    // Or expression (Option/Result unwrapping)
    // ========================================================================

    fn compile_or(&mut self, expr: &ast::OrExpression) -> Ty {
        let left_ty = self.compile_expr(&expr.expression);
        let resolved = self.engine.find(left_ty);

        let success_var = self.engine.fresh_var();

        // Determine which prelude type the LHS is. The Con node carries its
        // nominal id, so user-defined types that happen to be called
        // "Option"/"Result" can never match.
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
            // Best-effort recovery: still type-check the body so downstream
            // errors are not cascaded.
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

        // Failure branch: the recovery body, with the `Err` payload bound to
        // the receiver when there is one.
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

    // ========================================================================
    // Functions
    // ========================================================================

    /// Typecheck a function body inside a live `enter_fn_frame`/
    /// `finish_fn_frame` pair with params already bound, and park it. This is
    /// the single seam between the typecheck [`Self::compile_expr`] walk and
    /// the Core IR pipeline, and it is a *hand-off*, not a call: the walk is
    /// where inference, diagnostics, hover facts and reference-graph collection
    /// happen, and it produces no bytecode at all.
    ///
    /// The parked body is handed to `typed_ir::elaborate_body`→
    /// [`core_ir::lower`](crate::core_ir::lower)→`perceus`→`emit` in pass 6,
    /// once the whole module has been walked. There is no fallback path and no
    /// error path: the elaborator covers every form a [`CleanModule`] can
    /// contain, so it returns a [`TypedFn`] rather than a `Result`.
    ///
    /// Elaboration and lowering run under `check_only` too (they just stop
    /// before `emit`): they are the only passes that can prove a well-typed
    /// program is actually compilable, so `al check` must not skip them.
    ///
    /// The Core pipeline does not run here at all: the body is parked in
    /// `deferred_bodies` and elaborated once the whole module has been walked
    /// (`analyse_module`'s pass 6), so `lower` reads solved types rather than
    /// the vars inference has not yet pinned down, and sees the module as a
    /// whole rather than one SCC at a time.
    ///
    /// Returns `(body_ty, parked)`; see [`ParkedBody`].
    fn compile_fn_body(
        &mut self,
        params: &[ast::FunctionParameter],
        param_tys: &[Ty],
        body: &ast::Expression,
    ) -> (Ty, ParkedBody) {
        // Typecheck walk. Nested closures encountered here re-enter this fn
        // (via `compile_function_common`) and park their own bodies + reserve
        // their `Function` entries before we reserve ours.
        let param_slots = self.local_count;
        // This body's own walk region: its expressions, and none of the
        // enclosing frame's. A nested lambda opens another one here, so its
        // types never land in ours.
        self.open_walk_region();
        let body_ty = self.compile_expr(body);
        let walk_tys = self.close_walk_region();
        // The walk still bumped `local_count` for the pattern binds it
        // reserved; those slots are dead. Rewind to the param watermark so
        // `Function.locals` reflects only Core's own slot allocation.
        self.local_count = param_slots;
        debug_assert!(
            self.defer_depth > 0,
            "every function body must be walked inside a deferral region: \
             the Core pipeline runs after the walk, never during it"
        );
        // An ill-typed body is parked like any other and closed out empty at
        // drain time, where `CleanModule` cannot be minted for it: the
        // elaborator is never shown a subtree the checker rejected.
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
            // Everything the walk of *this* body just recorded: the lambdas
            // written directly inside it. Taken here rather than in
            // `finish_fn_frame`, which has already handed the enclosing frame
            // its own list back.
            closures: std::mem::take(&mut self.frame_closures),
            param_slots,
        };
        (body_ty, parked)
    }

    /// Elaborate one already-typechecked body, lower the whole `TypedProgram`
    /// it produces, and write the bodies of every eta-wrapper the elaborator
    /// appended to it.
    ///
    /// **The one door into the Core pipeline.** It is the only place in this
    /// compiler that calls `typed_ir::elaborate_body`/`elaborate_toplevel`, and
    /// those two are the only constructors of a [`TypedFn`] — so they are the
    /// only constructors of the [`TypedProgram`] that `lower`, `perceus` and
    /// `emit` consume. Consuming a [`CleanModule`] here therefore closes the
    /// whole pipeline to a module that reported an error: not by convention,
    /// but because a poisoned module cannot produce the value the passes take.
    /// (`lower` needs no proof of its own for exactly that reason.)
    ///
    /// `at` says where the elaborated function belongs in the program it is
    /// lowered as part of: `Some(func_idx)` for a function body, whose reserved
    /// `Function` slot fixes its [`FuncIdx`](crate::core_ir::FuncIdx); `None`
    /// for a module toplevel, which is `TypedProgram::toplevel` and has no
    /// index. Either way the returned [`LoweredBody`] carries its `CoreFn`.
    ///
    /// The wrappers are written *before* the caller reads `current_addr()` as
    /// `base`: they are the only instructions an elaboration can add ahead of
    /// its own body, and `base` becomes the body's `Function.code_start`, which
    /// the VM adds to every frame-relative jump operand `emit` produced. So
    /// `base` must name the body's first instruction. Each wrapper is
    /// self-contained — a `Jump` over its own body, then the body — so the
    /// surrounding stream falls straight through it. The elaborator itself
    /// cannot touch `program.code`; the debug assertion below pins that.
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
    /// `TypedProgram::fns` is `FuncIdx`-indexed and so is `program.functions`,
    /// so the two must agree: `fns` is padded up to `program.functions.len()`
    /// before the walk (`FnTable::push` mints each `FuncIdx` in append order),
    /// and each wrapper appended past that point gets its `Function` reserved
    /// here, in order. Nothing else may push a `Function` while the walk runs.
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
        // The `fns` entries an earlier body already owns. Never lowered into
        // anything the caller reads — they exist so the next `FnTable::push`
        // lands on the `Function` reserved for it below.
        let filler = || TypedFn {
            name: crate::types::NO_STR,
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

        debug_assert_eq!(
            self.program.functions.len(),
            eta_base,
            "nothing may reserve a `Function` while the elaborator walks: \
             the `FuncIdx` the next `FnTable::push` mints would stop naming it"
        );
        for w in &fns[eta_base..] {
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
                fns[func_idx.index()] = built;
                filler()
            }
            None => built,
        };
        Elaborated {
            program: TypedProgram {
                fns: fns.into_vec(),
                toplevel,
                // `lower` copies this verbatim into `CoreProgram::consts` and
                // nothing downstream of here reads it: the elaborator pooled
                // every constant straight into `program.constants` through
                // `ElabCtx::add_const`, and that is the pool `emit`'s operands
                // and the VM both address.
                consts: Vec::new(),
                pool,
                temps,
            },
            eta_base,
        }
    }

    /// Perceus and emit the eta wrappers `fns[base..]`, back-filling the
    /// `Function` entries [`Self::elaborate`] reserved for them.
    ///
    /// They go down ahead of the body that named them, each behind a `Jump`
    /// over itself, exactly where the old request-and-synthesise path put them.
    fn materialize_eta_wrappers(
        &mut self,
        pool: &ResolvedPool,
        base: usize,
        wrappers: Vec<CoreFn>,
    ) {
        use crate::core_ir::{emit, perceus};
        for (i, w) in wrappers.into_iter().enumerate() {
            let w = perceus::perceus(pool, w);
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
    /// already typechecked body and append its bytecode.
    ///
    /// Runs in pass 6, after the whole module has been walked; the only caller
    /// is [`Self::elaborate_deferred`]. `func_idx` is the placeholder
    /// `Function` the walk reserved and this fills in. Under `check_only` the
    /// pipeline stops after `lower` — nothing is emitted and the `Function`
    /// stays unfilled; the caller closes it out.
    ///
    /// The `CleanModule` is the caller's proof that this body typechecked (that
    /// nothing in the module failed to): a poisoned body has no types to
    /// elaborate.
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
        // The eta-wrappers the elaborator minted are written by the helper,
        // ahead of this body.
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
        self.core.fns.push(core.clone());
        // Linking a body is a plain append: `emit`'s jump operands are relative
        // to `code[0]` of the block, and `code[0]` lands at `base`, which is
        // exactly the `Function.code_start` the VM adds back. Nothing may push
        // an instruction between here and the `extend` below, or `code_start`
        // would no longer name the block's first instruction. The elaborator
        // cannot — it appends eta-wrappers to `TypedProgram::fns` rather than
        // synthesising them — and the one caller that still writes ahead of the
        // body, `materialize_eta_wrappers`, has already run.
        let base = self.current_addr();
        let out = emit::emit(&core, self);
        self.program.code.extend(out.code);
        // The frame is long closed, so this body owns its whole tail: the
        // `Ret`, and every field of the `Function` entry the walk reserved but
        // could not fill.
        self.program.code.push(op(Op::Ret));
        let end = self.current_addr();
        let f = &mut self.program.functions[func_idx.index()];
        f.locals = param_slots.max(out.locals);
        f.code_start = base;
        f.code_len = end - base - 1;
    }

    /// Open the elaboration phase boundary: every function body walked until
    /// the matching [`Self::end_deferred_elaboration`] is parked instead of
    /// elaborated, and the Core pipeline runs over all of them at once there.
    ///
    /// Exactly two places open one, and between them they cover every body the
    /// compiler ever walks: `analyse_module` (around passes 5's whole walk —
    /// the SCC loop *and* the toplevel lets and bare expressions after it) and
    /// `compile_impl`'s bare-expression program. `compile_fn_body` asserts it
    /// is inside one; the assertion is belt-and-braces, since [`ParkedBody`]
    /// leaves it nothing else to return.
    ///
    /// It is a whole-module boundary and not a per-SCC one because that is what
    /// `lower(p: &TypedProgram) -> CoreProgram` needs to exist: emit cannot be
    /// a step of the typecheck walk if the walk's product is the module.
    ///
    /// A body's types are also only final once its SCC has been inferred,
    /// `leave_level` has run and `generalize_top` has quantified what stayed
    /// free — draining at the end of the module is strictly later than that.
    ///
    /// What this buys, precisely: after `generalize_top`, a body var that
    /// inference never solved has a `Generic` root rather than an `Unbound`
    /// one, so `lower` can never observe a var that is about to be quantified
    /// out from under it. That is what keeps zonking honest — a `Generic`
    /// root maps to a bound index instead of an invented opaque `Bound`.
    /// It is not, measurably, an opcode win: on the T0 workload set the opcode
    /// mix is unchanged (0 typed ops recovered), because `compile_binary`
    /// already unifies both operands during the walk. The cases the reorder
    /// newly resolves — a var pinned by a later SCC sibling, or by the
    /// post-body `unify_at(ret_ty, body_ty)` — exist but move no opcodes there.
    ///
    /// Deferral *does* move code addresses and `program.functions` ordering
    /// relative to the fused pipeline: the module's jump-overs now all precede
    /// all of its bodies, and an eta-wrapper `Function` is pushed after its
    /// owner's (reserved during the walk) rather than before it. Both are
    /// self-consistent — every operand referring to them is computed after the
    /// fact — but `al build`'s output is not byte-identical to the fused
    /// compiler's, and an opcode histogram cannot see the difference. What does
    /// *not* move is which `Function` slot a declared body owns: those are
    /// reserved by `finish_fn_frame` during the walk, in walk order, in both
    /// `check` and `build` — the property `tests/check_parity.rs` pins.
    pub(super) fn begin_deferred_elaboration(&mut self) {
        self.defer_depth += 1;
    }

    /// Freeze the value env for every body parked so far, to be restored around
    /// their elaboration in [`Self::end_deferred_elaboration`].
    ///
    /// Called once, between the declaration walk and the toplevel `let`/bare-
    /// expression walk. The frame snapshot a `DeferredBody` carries fixes only
    /// the *load* a free name lowers to; its `Ty` and `ValueKind` still come
    /// from `self.env` at drain time, and a toplevel `let` rebinds in place.
    /// Bodies parked *after* this point (lambdas bound by those very `let`s)
    /// need the live env and keep it: they may reference an earlier toplevel
    /// bind that `resolve_variable` resolves to a global rather than a
    /// capture, so it is reachable only through `env`.
    pub(super) fn pin_deferred_env(&mut self) {
        // One pin per drain: an imported module is compiled by `process_imports`
        // *before* its importer opens a region, so `analyse_module` never nests
        // inside another's deferral and a second pin would name body indices
        // from a region that is not the one being closed.
        debug_assert_eq!(self.defer_depth, 1, "pin outside the module's own region");
        debug_assert!(self.deferred_env_pin.is_none(), "deferred env pinned twice");
        if self.deferred_bodies.is_empty() {
            return;
        }
        self.deferred_env_pin = Some((self.deferred_bodies.len(), self.env.clone()));
    }

    /// Close the region opened by [`Self::begin_deferred_elaboration`] and, at
    /// depth zero, run the Core pipeline over every parked body in walk order
    /// — innermost closure first, exactly the order the fused pipeline emitted
    /// them in. This is the module's whole `lower`→`perceus`→`emit` phase: one
    /// loop, after the typecheck walk, over every body the walk produced.
    ///
    /// Every jump-over is patched *after* the whole run, not per body: the
    /// bodies are emitted contiguously here, long after the walk pushed the
    /// `Jump` placeholders, so there is no `J_a, body_a, J_b, body_b` chain to
    /// hop along any more. Each `J` skips the entire run.
    pub(super) fn end_deferred_elaboration(&mut self) {
        self.defer_depth -= 1;
        if self.defer_depth > 0 {
            return;
        }
        let bodies = std::mem::take(&mut self.deferred_bodies);
        let jumps: Vec<i32> = bodies.iter().map(|d| d.jump_over).collect();
        // Bodies parked before `pin_deferred_env` (the declaration walk's)
        // elaborate against the env that walk saw; the rest (lambdas the
        // toplevel `let` walk parked) against the live one. See the pin's docs.
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
            // Re-proved per body, not once for the run: `elaborate_deferred`
            // consumes the proof, and an internal error raised while lowering
            // one body poisons the module for the next.
            match self.clean_module() {
                // A decl in the module failed to typecheck: the parked bodies
                // may reference names inference never resolved, and there is no
                // typed IR to lower. Leave them empty, exactly as the fused
                // pipeline left an ill-typed body empty.
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
    /// `resolve_name` reads the frame: a captured name must land on the
    /// `Denotation::capture` index the walk assigned it, a module-scope decl
    /// must still look like a global, and the frame's self-name must resolve
    /// to the same self-closure/self-toplevel-fn shape. The last of those is not
    /// answered by `captures` — `resolve_variable` short-circuits on
    /// `current_binding` while scanning `outer_scopes` — so the whole chain
    /// is restored, not just the module scope.
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
            // The lambdas this body wrote. `ElabCtx::closure` only ever asks
            // about a node in the body it is elaborating, so this is the whole
            // universe of answers for the elaboration — and the enclosing
            // frame's sites (or the module toplevel's, still waiting to be
            // elaborated) are safe from being read by it.
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

    /// Give a parked body that never elaborated the same shape the inline path
    /// gives an ill-typed one: a bare `Ret` and a zero-length `Function`. The
    /// jump-over is patched with the rest of the region's, in
    /// [`Self::end_deferred_elaboration`].
    fn close_empty_deferred(&mut self, func_idx: crate::core_ir::FuncIdx, param_slots: i32) {
        let base = self.current_addr();
        self.program.code.push(op(Op::Ret));
        let f = &mut self.program.functions[func_idx.index()];
        f.locals = param_slots;
        f.code_start = base;
        f.code_len = 0;
    }

    /// Compile a `fn(...) { ... }` expression. `param_hints` is `Some` when
    /// the lambda is being passed directly to a call site whose parameter
    /// types are known; in that case any unannotated parameter is given the
    /// hinted type rather than a fresh var so the body can immediately do
    /// field access etc.
    ///
    /// `span` is the lambda's own span: the [`ClosureSite`] recorded under it
    /// belongs to the frame this runs in — the one whose elaboration builds the
    /// `Atom::Closure` and evaluates its captures.
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
    /// hydrated in Pass 3. The supplied `hydrator` carries the rigid generic
    /// ids for any annotated type variables so that `instantiate` inside the
    /// body leaves them intact, and we unify the body's inferred type against
    /// the preregistered shape rather than re-reading the annotations.
    ///
    /// `global_slot` is this fn's entry-frame slot from `Prepared::Fn`,
    /// threaded from Pass 3 so `global_to_func` is keyed by the same slot the
    /// caller emits `StoreLocal` for — no reliance on `self.locals[name]`
    /// still holding that slot by the time this runs.
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
        // fn's body (`ElabCtx::fn_of_global`, off the same slot it stores into),
        // in every mode — so the index has to be a real one under `check_only`
        // too.
        // `finish_fn_frame` reserved it; take it from there rather than
        // re-deriving it from `program.functions.len()`, which is only the
        // same number by accident of nothing else having pushed since.
        let (func_idx, _) = self.finish_fn_frame(saved, name, params.len(), body_emit);
        self.global_to_func.insert(global_slot, func_idx);
        self.engine.mk_fun(&param_tys, ret_ty)
    }

    /// Snapshot the enclosing frame's codegen state, push a fresh inner frame,
    /// emit the jump-over placeholder that lets execution skip the embedded
    /// body, and open a new type-env scope. Param binding emits no bytecode so
    /// `func_start` here equals the address after params are bound.
    fn enter_fn_frame(&mut self, binding: Option<&str>) -> FnFrame {
        let binding_id = binding.map(|n| self.engine.intern(n));
        // The enclosing frame's locals already map any preallocated
        // entry-frame slots (analysis.rs Pass 3); moving the map into
        // `outer_scopes` as-is lets a nested lambda resolve the enclosing fn
        // name through outer_scopes[0] at the right slot (depth is irrelevant
        // for outer-scope resolution). `finish_fn_frame` moves it back.
        self.outer_scopes.push(Scope {
            locals: std::mem::take(&mut self.locals),
        });
        // The jump-over lets the enclosing code stream (the module's
        // fn-body run, or an outer body chaining past a nested closure's
        // body) skip the embedded `Function` body. Pushed in `check_only` too:
        // it is what makes `jump_over` — and therefore every `Function` index
        // downstream of it — mode-independent.
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
                    // A lambda / fn-expression has no self-name UNLESS it is the
                    // RHS of a `name = fn(...)` binding, which stashes that name
                    // in `next_fn_self_name` (consumed one-shot here). Inheriting
                    // the enclosing fn's `current_binding` is wrong: a HOF-arg
                    // lambda calling the enclosing fn would mis-resolve to Self_
                    // and emit CallSelf against the lambda's own live frame.
                    std::mem::replace(&mut self.current_binding, self.next_fn_self_name.take())
                }
            },
            jump_over,
            closures: std::mem::take(&mut self.frame_closures),
        }
    }

    /// Register a freshly-typed local binding into the type environment, the
    /// dead-code/reference graph, and the hover record, then emit its
    /// value-def. These steps must stay in lockstep, so every local-binder
    /// site (params, pattern names, the `or`-receiver) funnels through here
    /// after reserving its slot with `get_or_create_local`.
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

    /// Close out a function body: reserve its `Function` slot, combine the
    /// walk half ([`ParkedBody`]) with the frame state that just became final
    /// into the complete [`DeferredBody`], and restore the enclosing frame
    /// and type-env. Returns the assigned `func_idx` and captured-name set so
    /// `compile_function_common` can record a [`ClosureSite`] in the enclosing
    /// frame.
    ///
    /// No bytecode is written here. The body's `Ret`, its `code_start`/`locals`
    /// and its jump-over patch all belong to pass 6, which runs after the walk.
    fn finish_fn_frame(
        &mut self,
        saved: FnFrame,
        name: &str,
        arity: usize,
        parked: ParkedBody,
    ) -> (crate::core_ir::FuncIdx, Vec<StrId>) {
        // The frame's own name/rigids/captures, taken before the enclosing
        // frame's are moved back over them: a parked body needs them at
        // elaboration time to resolve its captures and self-reference exactly
        // as the walk did.
        let frame_binding = std::mem::replace(&mut self.current_binding, saved.binding);
        let frame_rigids = std::mem::replace(&mut self.rigid_ids, saved.rigid_ids);
        let frame_captures = std::mem::replace(&mut self.captures, saved.captures);
        // The inner frame's own sites left with its `DeferredBody`
        // (`compile_fn_body`); hand the enclosing frame its list back so the
        // lambda this closes out can be recorded into it.
        self.frame_closures = saved.closures;
        self.env.pop_scope();
        self.pop_unused_scope();

        let captured = std::mem::replace(&mut self.capture_names, saved.capture_names);
        // Reserve the body's `Function` slot now, so the `func_idx` that
        // the `ClosureSite` and `global_to_func` are about to record is the one
        // the elaborated body fills in. `locals`, `code_start` and
        // `code_len` are the only fields pass 6 can still move.
        self.program.functions.push(Function {
            name: name.into(),
            arity: arity as i32,
            locals: 0,
            capture_count: captured.len() as i32,
            code_start: 0,
            code_len: 0,
        });
        let func_idx = crate::core_ir::FuncIdx::from_usize(self.program.functions.len() - 1);
        // Read after `env.pop_scope()`: a captured name is by
        // definition bound in an *enclosing* frame's scope, which is
        // still open here but gone by elaboration time.
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
            // Read before the `outer_scopes.pop()` below: this is the exact
            // scope chain `resolve_variable` walked, and nothing mutates an
            // outer frame's map while an inner frame is open.
            outer_scopes: self.outer_scopes.clone(),
            capture_env,
        });

        // `enter_fn_frame` pushed the enclosing frame's locals; move them back.
        if let Some(scope) = self.outer_scopes.pop() {
            self.locals = scope.locals;
        }
        // `locals` is restored wholesale, so the inner frame's undo entries
        // (params bound in the enclosing mark range, never wrapped by a
        // `push_local_scope`) must be dropped, not replayed against the parent.
        self.undo_log.truncate(saved.undo_base);
        self.scope_marks.truncate(saved.marks_base);
        self.local_count = saved.local_count;

        // Re-resolving each capture in the *enclosing* frame is load-bearing:
        // `resolve_variable` promotes a name this body captured from further
        // out into the enclosing frame's own capture set, so transitive
        // captures chain outwards.
        for &cap_name in &captured {
            let _ = self.resolve_variable(cap_name);
        }
        (func_idx, captured)
    }

    // ========================================================================
    // Pattern matching
    // ========================================================================

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

    /// Bind a pattern's freshly-typed names (populated by `type_pattern`):
    /// reserve a local slot for each and funnel it through
    /// [`Self::register_local_binding`].
    fn bind_pattern_initials(&mut self, b: &PatternBindings) {
        for (name, (ty, sp)) in b.bindings() {
            self.get_or_create_local(name);
            self.register_local_binding(name, *ty, *sp);
        }
    }

    /// Type-check the runtime size expressions of every `<<..>>` segment in
    /// `p`. They are operands, not binders, and a later segment's size may
    /// name an earlier segment's binding (`<<n:8, body:bytes(n)>>`), so this
    /// runs *after* [`Self::bind_pattern_initials`] has brought the pattern's
    /// names into scope.
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
    /// constant `Value`. On i64 overflow / malformed input it emits a real
    /// diagnostic via [`Compiler::error`] and then returns a kind-preserving
    /// recovery zero. The recovery value is reachable ONLY on the
    /// post-diagnostic error branch, so it can never masquerade as a valid
    /// literal `0`: the compile has already failed.
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

// ============================================================================
// Free helpers
// ============================================================================

/// Whether matching `p` can fail on a value of its own type. Only wildcards,
/// bare names, and tuples of those are irrefutable; a constructor pattern is
/// refutable even for a single-variant type (the tag is still tested), and an
/// or-pattern is refutable exactly when its last alternative is. Used by the
/// destructuring-binding statements, which require an irrefutable pattern.
fn pattern_is_refutable(p: &ast::Pattern) -> bool {
    match p {
        ast::Pattern::Wildcard { .. } | ast::Pattern::Var { .. } => false,
        ast::Pattern::Tuple { elements, .. } => elements.iter().any(pattern_is_refutable),
        ast::Pattern::Or { first, rest, .. } => pattern_is_refutable(rest.last().unwrap_or(first)),
        _ => true,
    }
}

/// The span that "defines" the type of an expression for error reporting:
/// for a block, the last node (recursively); otherwise the expression's own
/// span. This focuses a return-type or branch-type mismatch on the actual
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
/// The scanner only ever produces numeric literal text matching
/// `[0-9]+(\.[0-9]+)?`, and the parser only prepends a leading `-` for
/// negative pattern literals, so the source is always
/// `-?[0-9]+(\.[0-9]+)?`. The integer branch can therefore fail only on
/// i64 overflow; the float branch effectively never fails for scanner
/// output, but `InvalidFloat` is kept so the function is total over every
/// `&str` rather than silently depending on that lexical invariant.
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

    /// Value to substitute so codegen can continue after the diagnostic has
    /// been emitted. Kind-preserving so the inferred type still matches user
    /// intent and the compile doesn't cascade into spurious type errors.
    fn recovery(&self, frozen: &mut FrozenBuilder) -> FrozenConst {
        match self {
            NumLitError::IntOutOfRange => frozen.int(0),
            NumLitError::InvalidFloat => frozen.float(0.0),
        }
    }
}

/// Parse a numeric literal's source text into a constant `Value`, built
/// through the frozen builder like every other program constant.
///
/// Total: the partiality is lifted into the return type so no caller can
/// obtain a fabricated `Value` for out-of-domain input. The only
/// `Value`-producing path for callers is [`Compiler::const_number`], which
/// emits a diagnostic before recovering.
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
