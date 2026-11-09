//! AST → core IR: orchestrates Hindley–Milner type inference, elaboration to
//! the typed IR, lowering to core IR and Perceus. The output is a
//! [`Program`]: every function body, every module's top level, and `main`.
//!
//! Function bodies go through the Core IR pipeline: the fused
//! [`Compiler::compile_expr`] walk runs first for type inference (a type that
//! unification just resolved is immediately available so elaboration can pick
//! typed operations — `IntAdd` over `Add` — and pattern lowering can consult
//! variant layouts), then the typechecked body is elaborated, lowered to
//! [`crate::core_ir`] ANF and run through Core→Core passes (Perceus reuse,
//! later mode inference). See [`Compiler::compile_fn_body`] and
//! `docs/core-ir-spec.md`.
//!
//! Module top level is the exception: declarations are mutually
//! recursive and order-free, so `analysis.rs` runs its multi-pass declaration
//! analysis first and hands each body back to this file.
//!
//! # Map of the module
//!
//! - **This file** — [`Compiler`] + entry points ([`compile`] / [`check`] /
//!   [`check_as_module`]): scoping and inference state in one struct,
//!   documented field by field; the pass itself (`compile_node` /
//!   `compile_expr` and friends, one method per AST form); and the Core IR
//!   orchestration ([`Compiler::compile_fn_body`]).
//! - **`patterns.rs`** — pattern type-checking: [`Compiler::type_pattern`]
//!   plus constructor lookup and argument slotting.
//! - **`bridges.rs`** — the [`ElabCtx`] impl through which the elaborator
//!   speaks to this compilation.
//! - **`tests.rs`** — the unit-test modules.
//!
//! The LSP/workspace layer that owns a `Compiler` across edits —
//! [`IncrementalSession`], [`Watermark`]/`reset_to`, and the reference-graph
//! finalization — lives in [`super::session`].
//!
//! # Invariants
//!
//! - Everything a compile appends to (inference pool, type env, function
//!   table, constants) stays append-only between module boundaries; that is
//!   what makes `Watermark` rollback pure truncation.
//! - Each body being walked has a frame, and each block in it a map of its
//!   own names (`frame.rs`): closing a block drops its map.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use super::session::{RawRef, Watermark};
use super::{PreludeBindings, TypeRef};
use crate::ast;
use crate::core_ir::{
    Abi, Const, ConstId, CoreFn, FuncIdx, HttpTypes, IoErrors, JsonTypes, LoweredFn, Program,
    Radix, TypeNames, VariantNames, VariantRef,
};
use crate::diagnostic::{Diagnostic, DiagnosticCode, has_errors};
use crate::tivec::{Idx, TiVec};
use crate::typed_ir::checked::{
    BinderId, CArg, CArm, CArrayElem, CBinPatSeg, CBinSeg, CCallee, CExpr, CInterpPart, CKind,
    CNode, CPat, CPatKind, CRest, CWidth,
};
use crate::typed_ir::slots::{SlotError, slot_labeled};
use crate::typed_ir::{
    self, ClosureFact, Denotation, ElabCtx, FnTable, GlobalSlot, NameAt, NameFact, OrShape,
    PreludeTys, RTy, ResolvedPool, TempTys, TypedExpr, TypedFn, TypedProgram, Zonker, pool_for,
};
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
    AnnotationContext, ArenaSlice, Bound, Constraint, DefinitionLocation, EntityKind, Hydrator,
    InferEngine, MatchFunTypeError, NullaryPrim, Pat, PatternBindings, PatternSink, Scheme, StrId,
    Ty, TypeEnv, TypeInfo, TypeNode, UsefulnessMatrix, ValueKind, mono, new_engine, new_env, pool,
    unroll_depth,
};

mod bridges;
pub(super) mod frame;
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
    /// empty, which is what the fused pipeline always did. In particular the
    /// elaborator's abort on a `CKind::Error` node — exactly what a rejected
    /// subtree looks like in the checked tree — cannot be reached from a
    /// module that already reported a type error, so a plain type error never
    /// turns into a compiler-bug report.
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

/// Proof that the entry frame's toplevel was spliced into the code: the
/// user's program is actually in the `Program`, not just the stdlib init that
/// precedes it.
///
/// Minted only by [`Compiler::record_toplevel`] on its `Entry` path, and the
/// only way to reach [`CompileResult::into_runnable`]. Without it, a compile
/// that rejected the module could still hand back a program that runs the
/// stdlib and nothing else — which does not fail, it silently evaluates to
/// whatever the globals were pre-filled with. A caller cannot tell that apart
/// from a real result, and one did not.
#[must_use]
struct EntryToplevel {
    _priv: (),
}

/// Where [`Compiler::resolve_variable`] found a name.
enum Place {
    /// Bound in the function being compiled, by this binding. The elaborator
    /// knows it by its own `BindingId`, so it never asks where it is.
    ThisFunction(BinderId),
    /// Reached from outside the function: a capture, a global, or the
    /// function itself.
    Outside(Denotation),
}

/// Why a nominal `.field` lookup failed. Carries enough of the offending
/// variant for the typecheck path to render its diagnostics; lower just
/// discards it.
enum FieldMismatch {
    /// Receiver's type body has no variants at all (alias, opaque, builtin).
    NotNominal,
    /// Variant list is empty, so no field can exist.
    NoVariants,
    /// A variant with no field of that name, so a value of it has nothing to
    /// read.
    MissingOn(StrId),
}

/// A field every variant of a type declares, found by its name.
struct SharedField {
    /// Its type, which every variant agrees on.
    ty: Ty,
    /// Where it sits in each variant, by variant index. Variants may disagree:
    /// a read looks the field up by name, not by position.
    slots: SmallVec<[u32; 4]>,
}

// ============================================================================
// CompileResult
// ============================================================================

#[derive(Debug)]
/// Whether a compilation unit reports the bindings it never uses.
///
/// A file is a whole program, so a binding nothing reads is a mistake worth
/// reporting. A REPL entry is a fragment of a session — the line that uses the
/// name has not been typed yet — so the same check would reject exactly the
/// entries the user meant.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum UnusedBindings {
    #[default]
    Report,
    Ignore,
}

/// What a module's top level may contain.
///
/// A program is declarations — `import`, `fn`, `type`, `const` — and starts
/// at `pub fn main()`; statements at module scope are rejected, in the entry
/// file exactly as in an imported module. The REPL is the one place code is
/// entered a statement at a time, and its bindings have to persist from one
/// entry to the next, which is what module-scope binds are; so it compiles
/// in [`Script`](ModuleScope::Script) mode, and nothing else does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ModuleScope {
    #[default]
    DeclarationsOnly,
    Script,
}

pub struct CompileResult {
    /// The program, when the compile built one. `None` for a check, for a
    /// module that failed, and for the incremental (LSP) session.
    ///
    /// Private, and reachable only through [`into_runnable`](Self::into_runnable),
    /// which also demands the [`EntryToplevel`] witness.
    program: Option<Program>,
    /// Set iff the entry toplevel was spliced. See [`EntryToplevel`].
    entry: Option<EntryToplevel>,
    pub diagnostics: Vec<Diagnostic>,
    /// Workspace reference graph (the single source of truth for goto-def /
    /// find-refs / rename / symbols / dead-code). An owned `Rc` handle so the
    /// owning `IncrementalSession` can keep answering queries against the same
    /// graph after the result is handed back.
    pub(crate) references: Rc<ReferenceGraph>,
}

impl CompileResult {
    /// Whether the compile succeeded — definitionally, whether `diagnostics`
    /// carries no error. Derived rather than stored so no construction site
    /// can disagree with the diagnostics it hands back.
    pub fn success(&self) -> bool {
        !has_errors(&self.diagnostics)
    }

    /// The program to run, and the only way to reach one.
    ///
    /// `None` unless the compiler recorded the user's toplevel — which a
    /// check-only compile and a rejected module both skip. The answer comes
    /// from that recording itself ([`EntryToplevel`]), not from re-reading
    /// `diagnostics`, so a caller that filters a diagnostic away cannot turn
    /// a compile that never lowered into a run.
    pub fn into_runnable(self) -> Option<Program> {
        let _recorded = self.entry?;
        self.program
    }

    /// A compile that materialised no program: a check-only session, or a
    /// stdlib seed that failed before the user's program was touched.
    pub(crate) fn analysis_only(
        diagnostics: Vec<Diagnostic>,
        references: Rc<ReferenceGraph>,
    ) -> Self {
        CompileResult {
            program: None,
            entry: None,
            diagnostics,
            references,
        }
    }
}

// ============================================================================
// Compiler — single-pass: HM type inference + elaboration
// ============================================================================

/// A live local binding: the stack slot it occupies, and what a use of it
/// means.
#[derive(Clone, Copy, Debug)]
pub(super) struct LocalSlot {
    pub(super) slot: i32,
    /// This name lives in the entry frame, so a nested body loads it with
    /// `PushGlobal <slot>` instead of capturing it. Decided once at bind time
    /// by [`Compiler::binds_a_global`]; `resolve_variable` must read this
    /// rather than re-derive it from where the binding sits.
    is_global: bool,
    /// The binding a use of the name in this frame means. A slot reserved
    /// before its binding is complete (a `let` whose init is still being
    /// walked, a pattern name left of a size expression) keeps the binding
    /// the name meant before; `None` when there was none in this frame.
    binder: Option<BinderId>,
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
    /// The declaration's binder in the module toplevel. Read only to resolve
    /// a forward *reference* to this decl from inside the toplevel spine, and
    /// the answer is this record's `slot`.
    pub(super) binder: BinderId,
    /// The entry-frame slot Pass 3 allocated for the declaration.
    pub(super) slot: GlobalSlot,
}

/// Per-module compiler state snapshotted on entry to a submodule body by
/// `enter_module_frame` and restored by `leave_module_frame`. Adding a
/// per-module field means one line here plus one swap/restore in the
/// enter/leave pair, not two hand-written `mem::replace` calls to remember at
/// both ends of `compile_module_body`.
struct ModuleFrame {
    module: ModulePath,
    module_key: ModuleKey,
    module_path_slice: Option<ArenaSlice<pool::StrSlices>>,
    imported_qualifiers: HashMap<String, ModuleKey>,
    base_dir: Option<PathBuf>,
    module_refs: ModuleReferences,
    /// What to close the module's imports with: the module base it replaced
    /// in the toplevel frame. `None` when the module's namespace was not
    /// scoped (prelude registration). Recorded at enter so a
    /// `retain_namespaces` toggle can never unbalance the pop.
    scoped: Option<usize>,
}

// `bool_cluster` fires on the four bool fields below, and the answer is that
// all 16 states are legal: there is no illegal combination for an enum to
// name. They are knobs on four unrelated subsystems — pipeline truncation,
// hover-fact collection, namespace scoping, and a marker for one transient
// walk — and both corners are reachable. All unset is an ordinary `compile`;
// all set is `IncrementalSession::new_from_source`, which builds a
// `check_only` compiler, sets `collect_hover_facts`, then calls
// `register_prelude`, which runs under `with_retained_namespaces`, inside
// which `analyse_module` sets `walking_module_statements`. The mode bools in
// this struct that did carry an invariant are already enums — see
// [`UnusedBindings`] and [`ModuleScope`]; these four carry none between them.
#[cfg_attr(mordant, allow(bool_cluster))]
pub struct Compiler {
    // --- Lowered program ---
    /// Every function this compile reserved, indexed by [`FuncIdx`], and filled
    /// once its body is lowered. A slot stays `None` under `check_only` and
    /// in a module that failed; [`Self::finish_program`] only builds a
    /// [`Program`] when every one is filled.
    pub(super) fns: TiVec<FuncIdx, Option<LoweredFn>>,
    /// Each imported module's lowered top level, in the order they finished
    /// compiling: a module after everything it imports.
    pub(super) inits: Vec<LoweredFn>,
    /// The entry file's lowered top level, once recorded.
    pub(super) entry_toplevel: Option<LoweredFn>,
    /// `pub fn main`, when the entry file has a usable one.
    pub(super) main: Option<FuncIdx>,
    /// Every constant this compile pooled, indexed by [`ConstId`]. Handed to
    /// [`CoreProgram::consts`](crate::core_ir::CoreProgram) wholesale once the
    /// program is lowered.
    pub(super) consts: Vec<Const>,
    /// Each pooled constant → its `ConstId`, so `add_constant` returns the
    /// existing id for a repeated literal instead of pushing a duplicate.
    /// Self-validating on lookup (the id is in bounds and still names that
    /// constant) so `reset_to`'s pool truncate needs no paired invalidation —
    /// a stale entry just misses.
    const_dedup: HashMap<Const, ConstId>,
    /// The bodies the walk is inside, the current one first.
    pub(super) frames: frame::Frames,
    /// Stdlib bootstrap (prelude registration) compiles at root on purpose: the flat by-name residue it leaves *is*
    /// the ambient stdlib namespace. While set, `enter_module_frame` skips
    /// namespace scoping. User modules always compile with it unset.
    retain_namespaces: bool,
    /// `_`-prefixed pattern binders whose discarded type must be re-examined
    /// once inference for the module is final: discarding a value the type
    /// system knows is `Nil` hides nothing, so the explicit `Nil` pattern is
    /// required instead. Drained by [`Compiler::check_nil_discards`].
    pub(super) nil_discards: Vec<(String, Ty, Span)>,
    /// Global slot → [`FuncIdx`] for every top-level `fn`
    /// that has already been compiled. Populated by `compile_declared_function`
    /// after `finish_fn_frame` assigns the `func_idx`; consulted by
    /// `resolve_variable` to choose between [`Denotation::known_fn`] and
    /// [`Denotation::global`], which lets Core call a known top-level fn
    /// directly (`Callee::Known`) instead of loading it and calling the value.
    /// A miss (forward ref within an SCC, or a non-fn global) falls back to the
    /// value call.
    pub(super) global_to_func: HashMap<GlobalSlot, crate::core_ir::FuncIdx>,
    /// This module's own top-level `fn`/`const` declarations, in Pass 5
    /// SCC-visit order (leaves first). Cleared to entry-file scope once the
    /// imports are compiled.
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
    /// The names the binding being checked has bound at module scope, with
    /// their entry-frame slots. [`Compiler::compile_statement`] moves them onto
    /// that binding's [`CNode::Binding`], so a slot travels with the binding
    /// it belongs to and nothing pairs them up by order later.
    pub(super) binding_globals: Vec<(StrId, GlobalSlot)>,
    /// True only while `analyse_module` walks a module's own statement list.
    /// Being a global ([`Compiler::binds_a_global`]) does not say this on its
    /// own: imports, the prelude's slot seeds and `fn`/`const` declarations
    /// are all globals bound outside the statement walk, and each reaches the
    /// elaborator by a route of its own.
    pub(super) walking_module_statements: bool,
    /// Memo for [`ElabCtx::resolve_rty`], keyed by union-find root: one pool
    /// node per solved type, for the whole `TypedProgram` the current
    /// `elaborate` call is building. Cleared there, with the pool.
    rty_cache: HashMap<Ty, RTy>,
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
    /// Whether closing a block reports the names nothing read. See
    /// [`UnusedBindings`].
    pub(super) unused_bindings: UnusedBindings,
    /// Whether module-scope statements are accepted in the entry. See
    /// [`ModuleScope`]; imported modules never accept them.
    pub(super) module_scope: ModuleScope,
    /// Memo for [`Compiler::restricted_generalization_cons`]: the TypeIds
    /// whose parameters must not generalize at a value binding. Stdlib type
    /// ids are stable across `reset_to` (the watermark sits past the static
    /// stdlib), so the memo survives a session's rewinds.
    restricted_gen_cons: Option<HashSet<TypeId>>,
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
    module_display: HashMap<ModuleKey, String>,
    pub(super) base_dir: Option<PathBuf>,
    /// When set, `scarlet/...` imports resolve to `.sca` files under this directory
    /// (the in-repo `src/std`) instead of the embedded snapshot, so a session
    /// editing the stdlib compiles it like any other on-disk module tree.
    pub(super) stdlib_source_root: Option<PathBuf>,
    pub(super) prelude: PreludeBindings,
    /// Names user code may not redefine. Populated from the prelude module's
    /// interface (every exported type and constructor) so the set is derived
    /// from `scarlet.sca` rather than mirrored in Rust. `@vm` functions are
    /// deliberately excluded — `println` is shadowable.
    pub(super) reserved: BTreeSet<String>,
    /// Function bodies whose typecheck walk has finished but whose Core
    /// pipeline (`lower`→`perceus`) is deferred until the enclosing
    /// declaration group has been generalized. See
    /// [`Self::begin_deferred_elaboration`].
    deferred_bodies: Vec<DeferredBody>,
    /// Nesting depth of open deferral regions. `compile_fn_body` runs the Core
    /// pipeline inline when this is zero (REPL entries, top-level `let`s and
    /// bare expressions, which are walked outside `analyse_module`'s Pass 5).
    defer_depth: u32,
}

/// One body, elaborated into a whole-module [`TypedProgram`] that the Core
/// pipeline can consume.
///
/// The elaborator mints a `FuncIdx` for every eta wrapper it needs by pushing
/// onto its `FnTable`, and `FuncIdx` is also this compiler's index into
/// [`Compiler::fns`] (it is what `Atom::Closure` and `Callee::Known` carry).
/// So `fns` is padded up to `Compiler::fns.len()` before the walk starts, and
/// each wrapper the walk appends past that point gets its slot reserved here
/// in the same order. `eta_base` is the padding length: `fns[eta_base..]` are
/// exactly the wrappers.
struct Elaborated {
    program: TypedProgram,
    eta_base: usize,
}

/// One body, all the way through `lower`: the `CoreFn` itself and the arena
/// its `RTy`s index.
///
/// A module toplevel's global-slot pinnings ride the IR itself: `lower` copied
/// `TypedBind::global` — stamped by `ElabCtx::global_slot` during elaboration
/// — onto each module-scope `Let`'s `CoreBind`. So a def/use mismatch against
/// the `Load::Global` every function body reads is unspellable: there is no
/// side table to desync from the binds it describes.
struct LoweredBody {
    /// The function's name, from the `TypedFn` it was lowered from.
    name: StrId,
    core: CoreFn,
    pool: Rc<ResolvedPool>,
}

/// A module-scope slot was bound outside any binding statement, so no
/// binding carries it to the elaborator and its global is never stored.
/// Aborts, in release as well as debug: only a compiler bug binds one there.
#[allow(clippy::panic)]
#[cold]
#[inline(never)]
fn unclaimed_toplevel_slots(n: usize) -> ! {
    panic!(
        "internal compiler error: {n} module-scope slot(s) bound outside a binding. \
         Report this as a compiler bug."
    )
}

/// A function slot was reserved but no body ever filled it, so the program
/// would call a function that does not exist. Aborts: only a compiler bug can
/// leave one behind.
#[allow(clippy::panic)]
#[cold]
#[inline(never)]
fn function_never_lowered(idx: usize) -> ! {
    panic!(
        "internal compiler error: function fn#{idx} was reserved but never lowered. \
         Report this as a compiler bug."
    )
}

/// A lowered body names a constructor of a type the compile never registered
/// with constructors. Lowering only names constructors the checker resolved,
/// so only a compiler bug gets here.
#[allow(clippy::panic)]
#[cold]
#[inline(never)]
fn constructor_of_no_type(id: TypeId) -> ! {
    panic!(
        "internal compiler error: a constructor of type {} has no declared variants. \
         Report this as a compiler bug.",
        id.0
    )
}

/// Something reserved a function while the elaborator walked — the `FuncIdx`
/// the next `FnTable::push` mints would stop naming it. Aborts, in release as
/// well as debug: every eta wrapper reserved after the stray push would
/// silently call the wrong function.
#[allow(clippy::panic)]
#[cold]
#[inline(never)]
fn function_reserved_during_elaboration() -> ! {
    panic!(
        "internal compiler error: a function was reserved while the elaborator walked. \
         Report this as a compiler bug."
    )
}

/// A function body parked between its typecheck walk and its elaboration.
///
/// The walk fixes everything about the body except its *types*: which
/// function slot it owns, and which names it captured and at which indices.
/// Types keep moving until the whole SCC
/// has been inferred and generalized, so `lower` — the pass that reads them —
/// runs last, off this record.
///
/// Built whole in exactly one place, [`Compiler::finish_fn_frame`], from the
/// walk half ([`ParkedBody`]) plus the frame half that only becomes final
/// there. No field is ever back-patched.
struct DeferredBody {
    name: StrId,
    param_binds: Vec<(StrId, BinderId, Ty)>,
    /// What the check walk built for the body: every node with the type it
    /// inferred, and every name and lambda with what it resolved to.
    body: CExpr,
    body_ty: Ty,
    /// The function slot this body fills in. Reserved by `finish_fn_frame` in
    /// *both* modes: a [`ClosureFact`] and `global_to_func` record it, and
    /// `lower` bakes it into `Atom::Closure`/`Callee::Known`, all of which
    /// happen under `check_only` too.
    func_idx: FuncIdx,
}

/// The walk half of a [`DeferredBody`]: everything [`Compiler::compile_fn_body`]
/// knows about the body it just typechecked, carried by value to the matching
/// [`Compiler::finish_fn_frame`], which adds the frame half (captures,
/// `func_idx`) and pushes the complete `DeferredBody`.
///
/// There is exactly one outcome, and that is the point: the typecheck walk
/// cannot lower a function body, because `compile_fn_body` has nothing to hand
/// `finish_fn_frame` but this record. The phase boundary is a type, not a
/// convention. The deferred body is lowered and fills its function slot in
/// `analyse_module`'s pass 6.
struct ParkedBody {
    name: StrId,
    param_binds: Vec<(StrId, BinderId, Ty)>,
    body: CExpr,
    body_ty: Ty,
}

/// A name resolved to a constructor, as `type_ctor_pattern` needs it: the
/// facts to slot and type the pattern's arguments against. Produced by
/// `Compiler::lookup_ctor` / `lookup_ctor_qualified`.
struct CtorLookup {
    variant: VariantRef,
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
                type_id,
                variant_idx,
                arity,
                field_labels,
                ..
            } => Some(CtorLookup {
                variant: VariantRef {
                    type_id,
                    variant_idx,
                },
                type_name,
                arity: arity as usize,
                field_labels,
                scheme,
            }),
            _ => None,
        }
    }
}

/// Which toplevel [`Compiler::record_toplevel`] is recording: the entry file's
/// or an imported module's.
enum TopKind {
    /// `__main__`: the program's own toplevel, and the `main` it starts at when
    /// it has one. A script (the REPL) has no `main`, and its toplevel's tail
    /// value is what it prints.
    Entry { entered: Option<FuncIdx> },
    /// An imported module: its toplevel runs for effect.
    Module,
}

/// A `module::member` access whose member the check walk found.
struct QualifiedMember<'a> {
    module_key: ModuleKey,
    member_name: &'a str,
    member_span: Span,
    scheme: Scheme,
    /// Whether the member has a runtime binding (an entry-frame slot).
    has_binding: bool,
    at: NameAt,
}

/// A callee that resolved to a plain name or a qualified `module::member`:
/// the looked-up scheme plus everything `compile_call` needs to dispatch on
/// the scheme's `ValueKind` and render diagnostics about the callee.
struct ResolvedCallee<'a> {
    name: &'a str,
    span: Span,
    scheme: Scheme,
    /// Whether the callee has a runtime binding (see `Compiler::name_at`).
    has_binding: bool,
    at: NameAt,
    /// The module a qualified callee resolved through. Diagnostics render it
    /// with `Compiler::module_name` — the user-visible name, never the key.
    qualifier: Option<ModuleKey>,
}

struct CompiledBody {
    iface: ModuleInterface,
    watermark: Watermark,
    /// Reference-graph data for this module's body, moved into its
    /// `CachedModule` by `load_module`. The module's type-id range is both
    /// reserved and recorded inside `compile_module_body`, so its start is
    /// deliberately not threaded back out here.
    refs: ModuleReferences,
}

/// Everything one compile can be asked to do differently. Combinations are
/// named by the entry points below rather than spelled at call sites, so a
/// new knob does not mean a new argument on five functions.
#[derive(Default)]
pub struct CompileOptions<'a> {
    /// Directory relative imports resolve against. `None` rejects them.
    pub base_dir: Option<&'a Path>,
    /// Analyse without emitting a program.
    pub check_only: bool,
    /// Analyse the buffer *as* this module rather than as `main`.
    pub as_module: Option<ModulePath>,
    /// Whether bindings nothing reads are reported. See [`UnusedBindings`].
    pub unused_bindings: UnusedBindings,
    /// Whether the entry may contain module-scope statements. See
    /// [`ModuleScope`].
    pub module_scope: ModuleScope,
}

impl<'a> CompileOptions<'a> {
    /// Emit a program from `base_dir`: what a file compile is. Every other
    /// entry point below is this with one field changed.
    pub fn new(base_dir: Option<&'a Path>) -> Self {
        CompileOptions {
            base_dir,
            ..Self::default()
        }
    }
}

pub fn compile(expr: &ast::Expression, base_dir: Option<&Path>) -> CompileResult {
    compile_with(expr, CompileOptions::new(base_dir))
}

pub fn check(expr: &ast::Expression, base_dir: Option<&Path>) -> CompileResult {
    compile_with(
        expr,
        CompileOptions {
            check_only: true,
            ..CompileOptions::new(base_dir)
        },
    )
}

/// Analyse a file *as* a specific stdlib module (used when editing
/// `src/std/**/*.sca` inside the Scarlet repo so the LSP/CLI doesn't report
/// `@vm is stdlib-only` / `Result is reserved`). Always check-only.
pub fn check_as_module(
    expr: &ast::Expression,
    base_dir: Option<&Path>,
    module: ModulePath,
) -> CompileResult {
    compile_with(
        expr,
        CompileOptions {
            check_only: true,
            as_module: Some(module),
            ..CompileOptions::new(base_dir)
        },
    )
}

pub(crate) fn new_compiler(base_dir: Option<&Path>, check_only: bool) -> Compiler {
    let mut ref_interner = ModuleInterner::new();
    let main_refs = ModuleReferences::new(ref_interner.intern(&module::main_module()));
    Compiler {
        fns: TiVec::new(),
        inits: Vec::new(),
        entry_toplevel: None,
        main: None,
        consts: Vec::new(),
        const_dedup: HashMap::new(),
        frames: frame::Frames::toplevel(HashMap::new(), 0),
        retain_namespaces: false,
        nil_discards: vec![],
        global_to_func: HashMap::new(),
        toplevel_decls: Vec::new(),
        binding_globals: Vec::new(),
        walking_module_statements: false,
        rty_cache: HashMap::new(),
        next_fn_self_name: None,
        current_owner: None,
        engine: new_engine(),
        env: new_env(),
        recorded: vec![],
        module_refs: main_refs,
        check_only,
        collect_hover_facts: false,
        unused_bindings: UnusedBindings::Report,
        module_scope: ModuleScope::default(),
        restricted_gen_cons: None,
        module_table: ModuleTable::new(),
        module_display: HashMap::new(),
        ref_interner,
        current_module: module::main_module(),
        current_module_key: ModuleKey::main(),
        module_path_slice: None,
        defid_module_memo: HashMap::new(),
        imported_qualifiers: HashMap::new(),
        base_dir: base_dir.map(|p| p.to_path_buf()),
        stdlib_source_root: None,
        prelude: PreludeBindings::default(),
        reserved: BTreeSet::new(),
        deferred_bodies: Vec::new(),
        defer_depth: 0,
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

/// A constructor pattern as written: `NotFound(path)`, or `io::NotFound(..)`
/// reached through a module qualifier.
struct CtorPattern<'a> {
    qualifier: Option<&'a ast::Identifier>,
    name: &'a ast::Identifier,
    args: &'a [ast::PatternArg],
    /// Ends in `..`, so fields it leaves out match anything.
    rest: bool,
    span: Span,
}

/// What the check walk built for an entry: a module block's nodes, or a bare
/// expression.
enum CheckedEntry {
    Module(Vec<CNode>, Span),
    Expr(CExpr),
}

/// The one compile. Every entry point above is a named [`CompileOptions`].
pub fn compile_with(expr: &ast::Expression, options: CompileOptions<'_>) -> CompileResult {
    let CompileOptions {
        base_dir,
        check_only,
        as_module,
        unused_bindings,
        module_scope,
    } = options;

    // Backpass sugar is rewritten away here, before any typing pass, so
    // inference and lowering never see a `Statement::Backpass`.
    let mut expr = expr.clone();
    crate::desugar::desugar_expression(&mut expr);
    let expr = &expr;

    let mut c = new_compiler(base_dir, check_only);
    c.unused_bindings = unused_bindings;
    c.module_scope = module_scope;
    if let Some(m) = as_module.clone() {
        // `as_module` is always a stdlib path (see `check_as_module`), whose
        // written form is its canonical identity.
        c.current_module_key = ModuleKey::for_stdlib(&m);
        c.current_module = m;
        // The user is editing stdlib source, so sibling `scarlet/...` imports must
        // resolve to the on-disk files next to it, not the stale embedded
        // snapshot.
        c.stdlib_source_root = base_dir.and_then(module::find_stdlib_root);
    }

    // When analysing the prelude file itself, don't load the prelude on top of
    // it (that would make every type a redefinition of itself). Other stdlib
    // modules still need the prelude for `Result`/`Nil`/etc.
    let is_prelude_self = as_module.as_deref() == Some(module::scarlet_prelude().as_slice());
    if !is_prelude_self {
        c.register_prelude();
        if has_errors(&c.engine.diagnostics) {
            // The prelude itself failed: the user's program was never compiled,
            // so there is nothing to hand back — not even a partial one.
            return CompileResult::analysis_only(
                c.engine.diagnostics,
                Rc::new(ReferenceGraph::new()),
            );
        }
    }
    let top_ty;
    let checked;
    // The `main` a program starts at; `None` for a check, a script, a
    // module, or a program with no usable `main` (which is then not clean).
    let mut entered = None;
    if let ast::Expression::BlockExpression(block) = expr {
        c.process_imports(block);
        c.bump_type_ids_past_reserved();
        c.toplevel_decls.clear();
        // Entry-file env scope: opened here (not inside `analyse_module`) so
        // the constructor/fn/const schemes it defines stay visible through
        // the toplevel elaboration below — otherwise a `type` decl followed
        // by a ctor use bails on `unbound callee` and the module toplevel
        // never gets Core. Popped after it.
        c.env.push_scope();
        let nodes = c.analyse_module(block, None);
        checked = CheckedEntry::Module(nodes, block.span);
        top_ty = c.ty_nil();
        if as_module.is_none() && module_scope == ModuleScope::DeclarationsOnly {
            entered = c.program_entry(block, check_only, expr.span());
        }
    } else {
        c.env.push_scope();
        // `analyse_module` opens a local scope around a module's statements, so
        // its nested blocks bind at depth 2 and are frame temps, not globals.
        // A bare expression has no statement walk, so open the same scope here:
        // without it a `let` in the expression's first nested block (an `if`
        // arm) would sit at the module's own depth, `resolve_variable` would
        // call it a global, and a lambda would load a global slot that
        // `bind_local` never queued and the elaborator therefore never pins.
        c.push_local_scope();
        // The same phase boundary `analyse_module` puts around its walk: a bare
        // expression program can still contain lambdas, and none of them may
        // lower while the expression is being typechecked.
        c.begin_deferred_elaboration();
        let body = c.compile_expr(expr);
        top_ty = body.ty;
        checked = CheckedEntry::Expr(body);
        c.end_deferred_elaboration();
        c.check_nil_discards();
        c.pop_local_scope();
        // A bare expression has no module statement walk, but its outermost
        // *block* (an `if` arm, say) is still the first one the elaborator
        // sees, so it is elaborated as though it were a module toplevel.
        // Nothing this compile declared belongs to it.
        c.toplevel_decls.clear();
    }

    // Elaborate the module toplevel and lower it into Core, like every other
    // function body (docs/core-ir-spec.md §Pipeline step 3). The elaborator
    // handles top-level `fn`/`const`/`type`/`import` decls, so it covers real
    // modules. Fn bodies were already lowered during `analyse_module` and read
    // sibling decls with `Load::Global(slot)`, where `slot` is Pass 3's
    // decl-first allocation; `ElabCtx::global_slot` stamped the same slot onto
    // each `TypedBind`, and `lower` copied it onto each toplevel `Let`'s
    // `CoreBind`, so definition and use agree.
    //
    // Elaboration and lowering run under `check_only` too. A well-typed
    // program the front half cannot handle has to be reported by
    // `scarlet check`, not left to blow up at `scarlet run`.
    // Stays `None` unless the recording below actually happens, which is what
    // keeps a rejected or check-only compile from handing back a `Program`
    // that runs the stdlib init and nothing else.
    let mut entry = None;
    if let Some(clean) = c.clean_module() {
        let name = c.engine.intern("__main__");
        let lowered = c.elaborate_then_materialize(clean, None, |c, pool, fns| match &checked {
            CheckedEntry::Module(nodes, span) => {
                typed_ir::elaborate_toplevel(c, pool, fns, name, nodes, *span, top_ty)
            }
            CheckedEntry::Expr(body) => {
                typed_ir::elaborate_body(c, pool, fns, name, &[], body, top_ty, true)
            }
        });
        if !check_only {
            entry = c.record_toplevel(lowered, TopKind::Entry { entered });
        }
    }
    c.env.pop_scope();

    let program = match entry {
        Some(_) => c.finish_program(),
        None => None,
    };
    let (references, _facts) = c.finalize_references();
    CompileResult {
        program,
        entry,
        diagnostics: c.engine.diagnostics,
        references,
    }
}

impl Compiler {
    pub(super) fn is_reserved(&self, name: &str) -> bool {
        self.reserved.contains(name)
    }

    /// The type constructors whose parameters must stay monomorphic when a
    /// binding's type is generalized — the relaxed value restriction. A
    /// `Subject`'s argument names what a live mailbox carries, so
    /// quantifying it would let one queue be sent one type and received as
    /// another. Resolved by module identity, never by name alone, so a user
    /// type called `Subject` is unaffected. An unresolvable module yields an
    /// empty set, which is sound: no value of the type can occur in a
    /// program that never loaded it.
    pub(crate) fn restricted_generalization_cons(&mut self) -> HashSet<TypeId> {
        if let Some(memo) = &self.restricted_gen_cons {
            return memo.clone();
        }
        let process: ModulePath = ["scarlet", "process"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let found = self
            .module_table
            .get(&ModuleKey::of(&process))
            .and_then(|iface| iface.types.get("Subject").map(|et| et.info.id));
        match found {
            Some(id) => {
                let set = HashSet::from([id]);
                // Memoize only a hit: a module unresolvable now (one this
                // compile has not loaded yet) may load later in this session.
                self.restricted_gen_cons = Some(set.clone());
                set
            }
            None => HashSet::new(),
        }
    }

    /// Assemble the [`Program`] once the entry toplevel is recorded. Every
    /// reserved function slot is filled by then; an empty one is a function
    /// nothing lowered, which only a compiler bug leaves behind.
    fn finish_program(&mut self) -> Option<Program> {
        let toplevel = self.entry_toplevel.take()?;
        let mut fns = TiVec::new();
        for (i, slot) in std::mem::take(&mut self.fns)
            .into_vec()
            .into_iter()
            .enumerate()
        {
            match slot {
                Some(f) => {
                    fns.push(f);
                }
                None => function_never_lowered(i),
            }
        }
        let inits = std::mem::take(&mut self.inits);
        let abi = Abi {
            some: self.prelude.some().into(),
            none: self.prelude.none().into(),
            ok: self.prelude.ok().into(),
            err: self.prelude.err().into(),
            radix: self.radix(),
            io: self.io_errors(),
            json: self.json_types(),
            http: self.http_types(),
        };
        let types = self.type_names((&fns).into_iter().chain(&inits).chain([&toplevel]), &abi);
        Some(Program {
            fns,
            consts: self.consts.clone(),
            inits,
            toplevel,
            main: self.main,
            globals: self.frames.current.slots as u32,
            types,
            abi,
        })
    }

    /// `scarlet/binary.Radix`'s constructors, by name, when the program loaded
    /// that module. A renamed constructor leaves it `None`, and the built-ins
    /// that read one then stop, rather than read the wrong base.
    fn radix(&self) -> Option<Radix> {
        let variant = self.stdlib_variants(&["scarlet", "binary"], "Radix")?;
        Some(Radix {
            dec: variant("Dec")?,
            hex: variant("Hex")?,
        })
    }

    /// `scarlet/io.IoError`'s constructors, by name, as [`Self::radix`].
    fn io_errors(&self) -> Option<IoErrors> {
        let variant = self.stdlib_variants(&["scarlet", "io"], "IoError")?;
        Some(IoErrors {
            not_found: variant("NotFound")?,
            permission_denied: variant("PermissionDenied")?,
            already_exists: variant("AlreadyExists")?,
            not_a_directory: variant("NotADirectory")?,
            is_a_directory: variant("IsADirectory")?,
            read_only_filesystem: variant("ReadOnlyFilesystem")?,
            filesystem_loop: variant("FilesystemLoop")?,
            file_too_large: variant("FileTooLarge")?,
            storage_full: variant("StorageFull")?,
            quota_exceeded: variant("QuotaExceeded")?,
            unaligned_binary: variant("UnalignedBinary")?,
            errno: variant("Errno")?,
        })
    }

    /// `scarlet/json`'s `Doc`, `ParseError` and `Json` constructors, by name,
    /// as [`Self::radix`].
    fn json_types(&self) -> Option<JsonTypes> {
        let module = ["scarlet", "json"];
        let doc = self.stdlib_variants(&module, "Doc")?;
        let parse_error = self.stdlib_variants(&module, "ParseError")?;
        let json = self.stdlib_variants(&module, "Json")?;
        Some(JsonTypes {
            doc: doc("Doc")?,
            parse_error: parse_error("ParseError")?,
            null: json("Null")?,
            boolean: json("Boolean")?,
            integer: json("Integer")?,
            real: json("Real")?,
            str: json("Str")?,
            list: json("List")?,
            object: json("Object")?,
            number: json("Number")?,
        })
    }

    /// `scarlet/http/h1`'s and `scarlet/http/headers`' constructors, by name,
    /// as [`Self::radix`].
    fn http_types(&self) -> Option<HttpTypes> {
        let h1 = ["scarlet", "http", "h1"];
        let header = self.stdlib_variants(&["scarlet", "http", "headers"], "Header")?;
        let version = self.stdlib_variants(&h1, "Version")?;
        let flags = self.stdlib_variants(&h1, "HeadFlags")?;
        let conn = self.stdlib_variants(&h1, "ConnTokens")?;
        let parsed = self.stdlib_variants(&h1, "Parsed")?;
        let response = self.stdlib_variants(&h1, "ParsedResponse")?;
        let bad = self.stdlib_variants(&h1, "BadResponse")?;
        let framing = self.stdlib_variants(&h1, "Framing")?;
        let chunk = self.stdlib_variants(&h1, "ChunkBody")?;
        Some(HttpTypes {
            header: header("Header")?,
            http10: version("Http10")?,
            http11: version("Http11")?,
            head_flags: flags("HeadFlags")?,
            conn_neither: conn("ConnNeither")?,
            conn_close: conn("ConnClose")?,
            conn_keep_alive: conn("ConnKeepAlive")?,
            conn_both: conn("ConnBoth")?,
            parsed_done: parsed("Done")?,
            parsed_need_more: parsed("NeedMore")?,
            parsed_bad: parsed("Bad")?,
            response_done: response("ResponseDone")?,
            response_need_more: response("ResponseNeedMore")?,
            response_bad: response("ResponseBad")?,
            bad_status_line: bad("BadStatusLine")?,
            bad_version: bad("BadVersion")?,
            bad_field: bad("BadField")?,
            head_too_large: bad("HeadTooLarge")?,
            bad_framing: bad("BadFraming")?,
            no_body: framing("NoBody")?,
            length: framing("Length")?,
            chunked: framing("Chunked")?,
            framing_invalid: framing("Invalid")?,
            chunked_done: chunk("ChunkedDone")?,
            chunked_need_more: chunk("ChunkedNeedMore")?,
            chunked_bad: chunk("ChunkedBad")?,
        })
    }

    /// The constructors of the stdlib type `module.name`, looked up by their
    /// names, when the program loaded that module.
    fn stdlib_variants(
        &self,
        module: &[&str],
        name: &str,
    ) -> Option<impl Fn(&str) -> Option<VariantRef>> {
        let path: ModulePath = module.iter().map(|s| s.to_string()).collect();
        let id = self
            .module_table
            .get(&ModuleKey::of(&path))?
            .types
            .get(name)?
            .info
            .id;
        let names = self.names_of_type(id);
        Some(move |name: &str| {
            let idx = names.variants.iter().position(|v| v.name == name)?;
            Some(VariantRef {
                type_id: id,
                variant_idx: u16::try_from(idx).ok()?,
            })
        })
    }

    /// The names of every type a constructor in `bodies`, or in `abi`,
    /// belongs to.
    fn type_names<'a>(
        &self,
        bodies: impl Iterator<Item = &'a LoweredFn>,
        abi: &Abi,
    ) -> BTreeMap<TypeId, TypeNames> {
        let mut ids: BTreeSet<TypeId> = abi.variants().iter().map(|v| v.type_id).collect();
        for f in bodies {
            f.core.body.for_each_variant(|v| {
                ids.insert(v.type_id);
            });
        }
        ids.into_iter()
            .map(|id| (id, self.names_of_type(id)))
            .collect()
    }

    fn names_of_type(&self, id: TypeId) -> TypeNames {
        let Some((info, variants)) = self
            .env
            .lookup_type_info_by_id(id)
            .and_then(|info| Some((info, info.variants()?)))
        else {
            constructor_of_no_type(id)
        };
        let str = |s| self.engine.str(s).to_string();
        TypeNames {
            name: str(info.name),
            variants: self
                .engine
                .variants_of(variants)
                .iter()
                .map(|v| VariantNames {
                    name: str(v.name),
                    fields: self
                        .engine
                        .variant_fields_of(v.fields)
                        .iter()
                        .map(|f| str(f.label))
                        .collect(),
                })
                .collect(),
        }
    }

    /// Pool `c`, deduplicating against the existing pool.
    ///
    /// The `const_dedup` memo survives `IncrementalSession::reset_to`, which
    /// truncates `consts` without clearing it. Every hit is therefore
    /// re-validated against the live pool: a stale entry either falls out of
    /// range or names a different constant, and `c` is re-pooled.
    fn add_constant(&mut self, c: Const) -> ConstId {
        if let Some(&id) = self.const_dedup.get(&c)
            && self.consts.get(id.0 as usize) == Some(&c)
        {
            return id;
        }
        let id = ConstId(self.consts.len() as u32);
        self.consts.push(c.clone());
        self.const_dedup.insert(c, id);
        id
    }

    fn const_int(&mut self, i: i64) -> ConstId {
        self.add_constant(Const::Int(i))
    }

    fn const_str(&mut self, s: &str) -> ConstId {
        self.add_constant(Const::String(s.to_string()))
    }

    /// Pool a binary constant of `bit_len` bits.
    fn const_binary(&mut self, bytes: Vec<u8>, bit_len: u64) -> ConstId {
        self.add_constant(Const::Binary { bytes, bit_len })
    }

    fn get_or_create_local(&mut self, name: &str) -> i32 {
        let id = self.engine.intern(name);
        if let Some((entry, inherited)) = self.frames.current.lookup(id) {
            // Reuse only if this slot was bound in the *current* block. One
            // inherited from an enclosing block must survive it, so shadowing
            // allocates a fresh slot.
            // At module scope (no enclosing fn frame) a re-binding must also
            // get a fresh slot. Top-level bindings live in the entry frame, so
            // a closure that captured one resolves it to `Global(slot)` and
            // reads that slot at call time; reusing the slot for the shadow
            // would overwrite the value the closure observes. Scarlet is immutable-
            // with-shadowing — each binding is distinct — so the earlier
            // binding must survive. Nested scopes capture by value at
            // `MakeClosure` time, so same-scope reuse is safe there.
            let module_scope = self.frames.at_toplevel();
            if !inherited && !module_scope {
                return entry.slot;
            }
        }
        let idx = self.frames.current.alloc_slot();
        self.bind_local(id, idx);
        idx
    }

    /// Bind `name` to `slot` in the current scope without publishing the slot
    /// to the toplevel elaboration. This is the whole of binding for anything
    /// the elaborator learns a slot for by another route: a top-level
    /// declaration (whose slot travels on its [`ToplevelDecl`]) and every
    /// binding inside a function frame.
    ///
    /// The new entry keeps the binding the name meant until
    /// [`Self::publish_local`] gives it its own.
    fn bind_local_raw(&mut self, name: StrId, slot: i32) {
        let is_global = self.binds_a_global();
        let frame = &mut self.frames.current;
        let entry = LocalSlot {
            slot,
            is_global,
            binder: frame.lookup(name).and_then(|(e, _)| e.binder),
        };
        frame.insert(name, entry);
    }

    /// Whether a binding made *now* lands in the entry frame — the one
    /// definition of "global" the compiler has.
    ///
    /// `at_toplevel` says no function frame is open, and a module depth of at
    /// most 1 says we are in the module's own statements: `analyse_module`
    /// opens exactly one block, imports and the prelude's slot seeds bind
    /// below it, and every nested block/`if`/`match` at module level opens
    /// another. Both halves of
    /// the global question — [`Self::bind_local`]'s queue and
    /// `resolve_variable`'s `PushGlobal` — are derived from this, recorded once
    /// on [`LocalSlot::is_global`], so they cannot drift apart.
    ///
    /// Note the bare-expression entry path (`compile` on a non-`BlockExpression`)
    /// opens a matching scope of its own, so a `let` inside one of its nested
    /// blocks is a frame temp there too.
    fn binds_a_global(&self) -> bool {
        self.frames.at_toplevel() && self.frames.current.module_depth() <= 1
    }

    /// [`Self::bind_local_raw`], plus: if this is a module-scope binding,
    /// queue its slot for the toplevel elaboration.
    ///
    /// The queue outlives `analyse_module`'s `pop_local_scope`, which the
    /// name's block does not. Only bindings made by the module's own
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
            self.binding_globals.push((name, GlobalSlot(slot)));
        }
    }

    /// Allocate the entry-frame slot for a top-level `fn`/`const` declaration.
    ///
    /// Unlike [`Self::get_or_create_local`] this never reuses an existing
    /// binding's slot — a module-scope rebind must not alias the slot a
    /// closure already captured by reference — and it does not queue the slot
    /// on [`Self::binding_globals`]: a declaration's slot reaches the
    /// elaborator on its own [`ToplevelDecl`] record instead.
    pub(super) fn alloc_decl_slot(&mut self, name: &str) -> (GlobalSlot, BinderId) {
        let id = self.engine.intern(name);
        let idx = self.frames.current.alloc_slot();
        self.bind_local_raw(id, idx);
        (GlobalSlot(idx), self.publish_local(id))
    }

    /// Make `name`'s slot, already reserved, mean a new binding from here on,
    /// and return that binding's [`BinderId`]. Uses walked before this, and
    /// the binding's own init, still mean whatever the name meant before.
    fn publish_local(&mut self, name: StrId) -> BinderId {
        let frame = &mut self.frames.current;
        let binder = frame.mint_binder();
        if let Some(entry) = frame.lookup_mut(name) {
            entry.binder = Some(binder);
        }
        binder
    }

    pub(super) fn push_local_scope(&mut self) {
        self.frames.current.open_block();
    }

    pub(super) fn pop_local_scope(&mut self) {
        if let Some(block) = self.frames.current.close_block() {
            self.report_unused(block);
        }
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
    /// this block and never used, that earlier binding is reported now (it's
    /// been shadowed and can no longer be referenced).
    fn track_binding(&mut self, name: &str, sp: Span) {
        if name.starts_with('_') {
            return;
        }
        let id = self.engine.intern(name);
        if let Some(prev_sp) = self.frames.current.track(id, sp) {
            self.unused_binding(name, prev_sp);
        }
    }

    /// Report each name `block` bound that nothing read, in source order.
    fn report_unused(&mut self, block: frame::Block) {
        let Some(unused) = block.unused else {
            return;
        };
        let mut leftover: Vec<(StrId, Span)> = unused.into_iter().collect();
        leftover.sort_by_key(|(_, sp)| (sp.start_line, sp.start_column));
        for (id, sp) in leftover {
            let name = self.engine.str(id).to_string();
            self.unused_binding(&name, sp);
        }
    }

    /// Where a name in scope lives. `None` when nothing in scope binds it: a
    /// constructor, a builtin, or a name whose only binding is not made yet.
    fn resolve_variable(&mut self, name: StrId) -> Option<Place> {
        self.frames.mark_used(name);
        // A slot reserved for a binding not yet made is not in scope.
        let frame = &self.frames.current;
        if let Some(binder) = frame.lookup(name).and_then(|(e, _)| e.binder) {
            return Some(Place::ThisFunction(binder));
        }
        if let Some(idx) = frame.captures.get(name) {
            return Some(Place::Outside(Denotation::capture(idx)));
        }
        // Search enclosing scopes innermost-first so inner bindings shadow
        // outer ones. The bottom of the stack (index 0) is always the entry
        // frame, and its *module-scope* locals are the program's globals, so a
        // nested fn loads them (`Load::Global`) at call time instead of
        // capturing by value. This is what makes mutually-recursive top-level
        // fns work — the sibling's slot is read at call time, not when the
        // closure is made.
        //
        // A local bound in a nested block/if/match scope at module level is
        // *not* a global: it is an ordinary toplevel temp (only
        // module-scope-depth binds land on `binding_globals`, see
        // `bind_local`), and nothing guarantees it is stored before a global
        // load of it runs. Such a
        // name is captured by value like any other enclosing local — exactly
        // as it would be inside a fn body. Which of the two a name is was
        // decided once, by [`Self::binds_a_global`], and recorded on the
        // binding; re-deriving it here from `depth` is how the loader and the
        // binder came to disagree on the bare-expression entry path.
        for (i, outer) in self.frames.outer().iter().enumerate().rev() {
            if let Some((entry, _)) = outer.lookup(name) {
                let is_global = i == 0 && entry.is_global;
                if frame.self_name == Some(name) {
                    // A top-level fn's self-name resolves to its entry-frame
                    // slot so a value load emits `PushGlobal`; `PushSelf` would
                    // read the sentinel `captures` a `CallKnown` frame carries.
                    // `Denotation` fixes both halves together.
                    return Some(Place::Outside(if is_global {
                        Denotation::self_toplevel_fn(GlobalSlot(entry.slot))
                    } else {
                        Denotation::self_closure()
                    }));
                }
                if is_global {
                    return Some(Place::Outside(
                        self.global_denotation(GlobalSlot(entry.slot)),
                    ));
                }
                let idx = self.frames.current.captures.add(name);
                return Some(Place::Outside(Denotation::capture(idx)));
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
    fn decl_denotation(&self, binder: BinderId) -> Option<Denotation> {
        let decl = self.toplevel_decls.iter().find(|d| d.binder == binder)?;
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
        if self.unused_bindings == UnusedBindings::Ignore {
            return;
        }
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
        self.ty_nullary(NullaryPrim::Bool, self.prelude.bool())
    }
    fn ty_int(&mut self) -> Ty {
        self.ty_nullary(NullaryPrim::Int, self.prelude.int())
    }
    fn ty_string(&mut self) -> Ty {
        self.ty_nullary(NullaryPrim::String, self.prelude.string())
    }
    fn ty_nil(&mut self) -> Ty {
        self.ty_nullary(NullaryPrim::Nil, self.prelude.nil())
    }
    fn ty_array(&mut self, elem: Ty) -> Ty {
        self.ty_prelude(self.prelude.array(), &[elem])
    }
    fn ty_binary(&mut self) -> Ty {
        self.ty_nullary(NullaryPrim::Binary, self.prelude.binary())
    }
    fn ty_option(&mut self, inner: Ty) -> Ty {
        self.ty_prelude(self.prelude.option(), &[inner])
    }
    fn ty_result(&mut self, ok: Ty, err: Ty) -> Ty {
        self.ty_prelude(self.prelude.result(), &[ok, err])
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
            let kind = match hit.qualifier {
                Some(_) => ReferenceKind::Qualified,
                None => ReferenceKind::Unqualified,
            };
            self.record_type_use(&path, &name, hit.span, kind);
            // A `module::Type` occurrence names the import through its
            // qualifier, exactly like a `module.value` use: record the
            // `Qualifier` occurrence so the import counts as used and the
            // qualifier resolves under hover / goto-def.
            if let Some((qid, qspan)) = hit.qualifier {
                let q = ast::Identifier {
                    name: self.engine.str(qid).to_string(),
                    span: qspan,
                };
                self.record_qualifier_use(&q);
            }
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

    pub(super) fn compile_node(&mut self, node: &ast::Node) -> (Ty, CNode) {
        match node {
            ast::Node::Statement(s) => {
                let n = self.compile_statement(s);
                (self.ty_nil(), n)
            }
            ast::Node::Expression(e) => {
                let e = self.compile_expr(e);
                (e.ty, CNode::Expr(e))
            }
        }
    }

    // ========================================================================
    // Statements
    // ========================================================================

    fn compile_statement(&mut self, stmt: &ast::Statement) -> CNode {
        match stmt {
            ast::Statement::Binding(b) => {
                let mark = self.binding_globals.len();
                let (pattern, init) = match b.name() {
                    Some(ident) => {
                        let (init, binder) = self.compile_name_binding(b, ident);
                        let pattern = CPat {
                            ty: init.ty,
                            span: ident.span,
                            kind: CPatKind::Bind {
                                name: self.engine.intern(&ident.name),
                                binder,
                            },
                        };
                        (pattern, init)
                    }
                    None => self.compile_destructuring(b),
                };
                let globals = self.binding_globals.split_off(mark);
                CNode::Binding {
                    pattern,
                    init,
                    globals,
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
                CNode::Nothing
            }
            // Imports are processed at the start of compilation, before
            // walking the body. By the time we reach one here it's already
            // been resolved (or rejected) — nothing left to do.
            ast::Statement::ImportDeclaration(_) => CNode::Nothing,
            ast::Statement::Backpass(bp) => {
                // Desugared away before compilation; one can only survive a
                // parser rejection (e.g. at module top level), which already
                // errored. Type the call so its own diagnostics still surface.
                CNode::Effect(self.compile_expr(&bp.call))
            }
        }
    }

    /// `x = e` or `x Type = e`. Returns the init and the binding's binder.
    fn compile_name_binding(
        &mut self,
        vb: &ast::Binding,
        ident: &ast::Identifier,
    ) -> (CExpr, BinderId) {
        let name = ident.name.clone();
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
        let defer_slot = self.frames.at_toplevel() && self.frames.current.lookup(name_id).is_some();
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
        // consumed by its `enter_fn_frame`. Setting it on the current frame
        // would leak into nested / HOF-arg lambdas and mis-resolve their
        // calls to the enclosing fn as Self_ (emitting CallSelf against the
        // wrong, live frame).
        let saved_self = if init_is_fn {
            self.next_fn_self_name.replace(name_id)
        } else {
            self.next_fn_self_name.take()
        };
        let init = self.compile_expr_with_hint(&vb.init, annot_ty);
        let init_ty = init.ty;
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

        let restricted = self.restricted_generalization_cons();
        let scheme = self.engine.generalize_restricted(final_ty, &restricted);
        let m = self.current_module_slice();
        self.env.define_at(
            &name,
            scheme,
            DefinitionLocation::new(ident.span, m, EntityKind::Value),
        );
        self.track_binding(&name, ident.span);
        if let Some(doc) = &vb.doc {
            self.env.store_doc(&name, doc.clone());
        }
        self.record(&name, final_ty, ident.span, vb.doc.clone());
        self.emit_value_def(ident.span, &name, vb.doc.clone());

        if defer_slot {
            self.get_or_create_local(&name);
        }
        (init, self.publish_local(name_id))
    }

    /// `(a, b) = e`, `Point(x:, ..) = e`: typed like a one-arm `match`, and the
    /// one arm has to cover every value, so only a pattern that cannot fail
    /// is a binding.
    fn compile_destructuring(&mut self, b: &ast::Binding) -> (CPat, CExpr) {
        let init = self.compile_expr(&b.init);
        let init_ty = init.ty;
        let mut pb = PatternBindings::new();
        let (pattern, typed_ok) =
            self.type_pattern(&b.pattern, init_ty, &mut pb.sink(), &mut Vec::new());
        if !typed_ok {
            return (pattern, init);
        }
        let depth = unroll_depth(&pattern) + 1;
        let resolved = self
            .engine
            .resolve_for_patterns(init_ty, Some(&self.env), depth);
        let mut um = UsefulnessMatrix::new(resolved);
        let pat = um.lower(&pattern);
        if let Some(missing) = um.find_missing(&[pat]) {
            self.error(
                format!(
                    "A binding's pattern must match every value, and this one misses {missing}. \
                     Use `match` to handle it"
                ),
                b.pattern.span(),
            );
        }
        (pattern, init)
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
        // (`scarlet` / `/` in `import scarlet/string`) are not clickable. The alias
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

        // Every public type of the module is reachable as `qualifier::Name` in
        // type position. Register each under the mangled `qualifier::Name` key
        // the hydrator probes for a qualified reference; type names cannot
        // contain ':', so these keys never collide with local declarations.
        let qualified_types: Vec<(String, _)> = self
            .module_table
            .get(&key)
            .map(|iface| {
                iface
                    .types
                    .iter()
                    .map(|(n, et)| (format!("{qualifier}::{n}"), et.info))
                    .collect()
            })
            .unwrap_or_default();
        for (mangled, ti) in qualified_types {
            self.env.store_type_info(&mangled, ti);
        }

        self.imported_qualifiers.insert(qualifier, key.clone());
    }

    /// Load the module `path` names, relative to the *importing* module's
    /// directory, and return its canonical identity.
    ///
    /// Resolution happens **before** the cache is consulted, and the key comes
    /// from the file we resolved to — never from `path` as written. Keying on
    /// the written path made `./b` mean one module program-wide: a `sub/mid.sca`
    /// importing `./b` silently received the root's `b.sca`. See
    /// `module::file_module_path`.
    /// How to name a module in a diagnostic: the path the importer wrote
    /// (`./lib`), falling back to the module's own last segment. Never the
    /// canonical key — that is an identity, not a name.
    fn module_name(&self, key: &ModuleKey) -> String {
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
        let resolved = match module::resolve_with(
            path,
            self.base_dir.as_deref(),
            self.stdlib_source_root.as_deref(),
        ) {
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
        if self.module_table.get(&key).is_some() {
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
        let mut parsed = parser.parse_program();
        crate::desugar::desugar_program(&mut parsed.ast);
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
        let origin = match source_path {
            Some(path) => ModuleOrigin::File {
                source_hash: hash,
                stat: None,
                path,
                refs,
            },
            None => ModuleOrigin::Embedded { refs },
        };
        self.module_table.bump_compile_count();
        // The watermark is captured *after* this module's own dependencies have
        // loaded (compile_module_body does that first) but reflects the arena
        // state immediately *before* this module's body added anything.
        self.module_table.insert_cached(
            key.clone(),
            CachedModule {
                iface: body.iface,
                origin,
                watermark: body.watermark,
                dependents: HashSet::new(),
            },
        );
        for imp in imports {
            self.module_table.record_dependent(&imp, &key);
        }
        self.module_table.record_dependent(&key, &importer);
        Some((canon, key))
    }

    /// Run `f` with [`Self::retain_namespaces`] set: module compiles inside it
    /// leave their by-name residue at root instead of scoping it to their
    /// frame. Stdlib bootstrap only — see the field's doc.
    pub(crate) fn with_retained_namespaces<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        let prev = self.retain_namespaces;
        self.retain_namespaces = true;
        let out = f(self);
        self.retain_namespaces = prev;
        out
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
        let scoped = (!self.retain_namespaces).then(|| {
            self.env.push_module_frame();
            self.frames.current.open_module()
        });
        ModuleFrame {
            module: std::mem::replace(&mut self.current_module, path),
            module_key: std::mem::replace(&mut self.current_module_key, key),
            module_path_slice: self.module_path_slice.take(),
            imported_qualifiers: std::mem::take(&mut self.imported_qualifiers),
            base_dir: std::mem::replace(&mut self.base_dir, base_dir),
            module_refs: std::mem::replace(&mut self.module_refs, ModuleReferences::new(mid)),
            scoped,
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
            scoped,
        } = old;
        if let Some(outer_base) = scoped {
            // Unwind the module's namespace: its env by-name entries and its
            // import items.
            self.frames.current.close_module(outer_base);
            self.env.pop_module_frame();
        }
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
        // Everything pushed from here until `leave_module_frame` was raised
        // while *this* module (or one of its dependencies, which stamp their
        // own ranges first in the `process_imports` recursion) was current, so
        // its spans point into this module's text, not the entry file's.
        let diag_mark = self.engine.diagnostics.len();
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
        // `toplevel_decls` drives the Core toplevel lower below; reset it so it
        // carries only *this* module's decls, not those of the dependencies
        // `process_imports` just compiled.
        self.toplevel_decls.clear();
        self.env.push_scope();
        let nodes = self.analyse_module(block, Some(&mut iface));
        self.lower_module_init(&key, &nodes, block.span);
        self.env.pop_scope();
        // Record actual type-id consumption so `id_high_water` tracks real
        // usage and a reused-range spill past `MODULE_TYPE_ID_RANGE` raises
        // `id_range_overflow` for the invalidate-all/reset recovery path.
        let used = self.env.next_type_id().0 - base.0;
        reservation.note_usage(&mut self.module_table, used);

        let refs = self.leave_module_frame(old);
        // Stamp provenance on every diagnostic this module's compile produced.
        // Dependencies compiled inside our range have already stamped theirs,
        // so only still-unstamped (entry-relative) ones become ours.
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

    /// The function a program starts at. Validates the entry file's `main`
    /// — `pub`, no parameters — whenever there is one, so `check` reports a
    /// malformed entry point too, and requires one when the program is going
    /// to be run. Returns its `FuncIdx` in that case; a program with no
    /// usable `main` gets a diagnostic, which is what keeps it from running.
    fn program_entry(
        &mut self,
        block: &ast::BlockExpression,
        check_only: bool,
        module_span: Span,
    ) -> Option<crate::core_ir::FuncIdx> {
        let main = block.body.iter().enumerate().find_map(|(node, n)| match n {
            ast::Node::Statement(s) => match s.as_ref() {
                ast::Statement::Declaration { decl, public } => match decl.as_ref() {
                    ast::Declaration::Function(fd) if fd.identifier.name == "main" => {
                        Some((node, fd, *public))
                    }
                    ast::Declaration::Function(_)
                    | ast::Declaration::Type(_)
                    | ast::Declaration::Const(_) => None,
                },
                ast::Statement::ImportDeclaration(_)
                | ast::Statement::Binding(_)
                | ast::Statement::Backpass(_) => None,
            },
            ast::Node::Expression(_) => None,
        });
        let Some((node, fd, public)) = main else {
            if !check_only {
                self.error(
                    "No `main` function: a program starts at `pub fn main()`".to_string(),
                    Span::point(module_span.start_line, module_span.start_column),
                );
            }
            return None;
        };
        let mut usable = true;
        if !public {
            self.error(
                "`main` must be public: a program starts at `pub fn main()`".to_string(),
                fd.identifier.span,
            );
            usable = false;
        }
        if !fd.params.is_empty() {
            self.error(
                "`main` takes no parameters (arguments are read with `os.argv()`)".to_string(),
                fd.identifier.span,
            );
            usable = false;
        }
        if check_only || !usable {
            return None;
        }
        // Compiled by `analyse_module` unless its body failed, in which case
        // the module is not clean and nothing runs anyway.
        let slot = self.toplevel_decls.iter().find(|d| d.node == node)?.slot;
        self.global_to_func.get(&slot).copied()
    }

    /// Perceus a just-lowered toplevel and record it: an imported module's
    /// joins [`Self::inits`], and the entry file's becomes the program's own.
    /// Returns the [`EntryToplevel`] witness for `TopKind::Entry` — the one
    /// place it is minted, so it cannot outrun the recording it stands for.
    fn record_toplevel(&mut self, lowered: LoweredBody, kind: TopKind) -> Option<EntryToplevel> {
        use crate::core_ir::perceus;
        // Every module-scope slot left on a binding's node. One still here was
        // bound by something that is not a binding statement.
        if !self.binding_globals.is_empty() {
            unclaimed_toplevel_slots(self.binding_globals.len());
        }
        let LoweredBody {
            name,
            core: top,
            pool,
        } = lowered;
        // Perceus runs so *temporaries* passed into calls are moved (rc==1 in
        // the callee → its own reuse fires). `keep_globals` then takes the
        // pinned globals back out: their last use in the toplevel is not their
        // last use in the program.
        let top = perceus::keep_globals(perceus::perceus(&pool, top));
        if std::env::var("CORE_DBG").is_ok() {
            eprintln!("=== {}\n{top}", self.engine.str(name));
        }
        let lowered = self.lowered_fn(name, top, pool);
        match kind {
            TopKind::Module => {
                self.inits.push(lowered);
                None
            }
            TopKind::Entry { entered } => {
                self.entry_toplevel = Some(lowered);
                self.main = entered;
                Some(EntryToplevel { _priv: () })
            }
        }
    }

    /// Elaborate and lower a just-analysed module toplevel to Core and record
    /// it, exactly as `compile_with` does for `__main__`.
    fn lower_module_init(&mut self, key: &ModuleKey, nodes: &[CNode], span: Span) {
        let Some(clean) = self.clean_module() else {
            return;
        };
        let name = self.engine.intern(key.as_str());
        let nil = self.ty_nil();
        // Elaborated and lowered even under `check_only` so a module toplevel
        // the front half cannot handle is a diagnostic rather than a crash at
        // `scarlet run`.
        let lowered = self.elaborate_then_materialize(clean, None, |c, pool, fns| {
            typed_ir::elaborate_toplevel(c, pool, fns, name, nodes, span, nil)
        });
        if !self.check_only {
            self.record_toplevel(lowered, TopKind::Module);
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
        let Some(iface) = self.module_table.get(module_key) else {
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

    /// Typecheck `expr` and build its node in the checked tree.
    ///
    /// The tree is what lets the elaborator use the typechecker's answers
    /// rather than re-derive them: re-instantiating a constructor or module
    /// function scheme would produce unresolved vars where the walk had a
    /// concrete `Con`.
    pub(super) fn compile_expr(&mut self, expr: &ast::Expression) -> CExpr {
        let (ty, kind) = self.compile_expr_inner(expr);
        CExpr {
            ty,
            span: expr.span(),
            kind,
        }
    }

    fn compile_expr_inner(&mut self, expr: &ast::Expression) -> (Ty, CKind) {
        match expr {
            ast::Expression::BinaryLiteral(bl) => self.compile_binary_literal(bl),
            ast::Expression::BlockExpression(be) => {
                self.push_block_scope();
                let mut last_ty = self.ty_nil();
                let mut nodes = Vec::with_capacity(be.body.len());

                for (i, node) in be.body.iter().enumerate() {
                    let is_last = i == be.body.len() - 1;
                    let (ty, cnode) = self.compile_node(node);
                    nodes.push(cnode);

                    if is_last {
                        last_ty = ty;
                    } else if matches!(node, ast::Node::Expression(_)) {
                        let resolved = self.engine.find(ty);
                        let is_nil = matches!(
                            self.engine.node(resolved),
                            TypeNode::Con { id, .. } if self.prelude.nil().is(id)
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
                (last_ty, CKind::Block(nodes))
            }
            ast::Expression::NumberLiteral(nl) => {
                // `const_number` is the single source of the int/float split
                // (and of the overflow diagnostic); the elaborator pools the
                // constant itself.
                let c = self.const_number(nl);
                let ty = if matches!(c, Const::Float(_)) {
                    self.engine.icon_float()
                } else {
                    self.ty_int()
                };
                (ty, CKind::Number(nl.clone()))
            }
            ast::Expression::StringLiteral(sl) => {
                (self.ty_string(), CKind::String(sl.value.clone()))
            }
            ast::Expression::InterpolatedString(is) => {
                let parts = is
                    .parts
                    .iter()
                    .map(|part| match part {
                        ast::InterpPart::Literal(sl) => CInterpPart::Literal(sl.value.clone()),
                        ast::InterpPart::Expr(e) => CInterpPart::Expr(self.compile_expr(e)),
                    })
                    .collect();
                (self.ty_string(), CKind::Interp(parts))
            }
            ast::Expression::Identifier(id) => self.compile_identifier(id),
            ast::Expression::BinaryExpression(be) => self.compile_binary(be),
            ast::Expression::UnaryExpression(ue) => {
                let operand = self.compile_expr(&ue.expression);
                let inner_ty = operand.ty;
                let ty = match ue.op {
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
                };
                let kind = CKind::Unary {
                    op: ue.op,
                    operand: Box::new(operand),
                };
                (ty, kind)
            }
            ast::Expression::IfExpression(ie) => {
                let cond = self.compile_expr(&ie.condition);
                let b = self.ty_bool();
                self.engine.unify_at(b, cond.ty, ie.condition.span());
                let then = self.compile_expr(&ie.body);
                let els = self.compile_expr(&ie.else_body);

                let ret = self.engine.fresh_var();
                self.engine
                    .unify_at(ret, then.ty, type_defining_span(&ie.body));
                self.engine
                    .unify_at(ret, els.ty, type_defining_span(&ie.else_body));
                let kind = CKind::If {
                    cond: Box::new(cond),
                    then: Box::new(then),
                    els: Box::new(els),
                };
                (ret, kind)
            }
            ast::Expression::MatchExpression(me) => self.compile_match(me),
            ast::Expression::ArrayExpression(ae) => self.compile_array(ae),
            ast::Expression::TupleExpression(te) => {
                let elems: Vec<CExpr> = te.elements.iter().map(|e| self.compile_expr(e)).collect();
                let elem_tys: Vec<Ty> = elems.iter().map(|e| e.ty).collect();
                (self.engine.mk_tuple(&elem_tys), CKind::Tuple(elems))
            }
            ast::Expression::ArrayIndexExpression(aie) => {
                let recv = self.compile_expr(&aie.expression);
                let elem_var = self.engine.fresh_var();
                let arr_expected = self.ty_array(elem_var);
                self.engine
                    .unify_at(arr_expected, recv.ty, aie.expression.span());

                if let ast::Expression::RangeExpression(r) = aie.index.as_ref() {
                    let start = self.compile_expr(&r.start);
                    let end = self.compile_expr(&r.end);
                    let int_t = self.ty_int();
                    self.engine.unify_at(int_t, start.ty, r.start.span());
                    self.engine.unify_at(int_t, end.ty, r.end.span());
                    // A slice can fail, when its range is not inside the
                    // array, so it is a `Result`, as `binary.slice_bits` is.
                    let slice = self.ty_array(elem_var);
                    let nil = self.ty_nil();
                    let kind = CKind::Slice {
                        recv: Box::new(recv),
                        start: Box::new(start),
                        end: Box::new(end),
                    };
                    (self.ty_result(slice, nil), kind)
                } else {
                    let index = self.compile_expr(&aie.index);
                    let int_t = self.ty_int();
                    self.engine.unify_at(int_t, index.ty, aie.index.span());
                    let kind = CKind::Index {
                        recv: Box::new(recv),
                        index: Box::new(index),
                    };
                    (self.ty_option(elem_var), kind)
                }
            }
            ast::Expression::RangeExpression(re) => {
                let start = self.compile_expr(&re.start);
                let end = self.compile_expr(&re.end);
                let int_t = self.ty_int();
                self.engine.unify_at(int_t, start.ty, re.start.span());
                self.engine.unify_at(int_t, end.ty, re.end.span());
                let i = self.ty_int();
                let kind = CKind::Range {
                    start: Box::new(start),
                    end: Box::new(end),
                };
                (self.ty_array(i), kind)
            }
            ast::Expression::FunctionExpression(fe) => {
                self.engine.enter_level();
                let (ty, closure) = self.compile_function_common(
                    &fe.params,
                    &fe.body,
                    fe.return_type.as_ref(),
                    None,
                );
                self.engine.leave_level();
                (ty, CKind::Closure(closure))
            }
            ast::Expression::FunctionCallExpression(fc) => self.compile_call(fc),
            ast::Expression::PropertyAccessExpression(pa) => self.compile_property_access(pa),
            ast::Expression::ModuleAccessExpression(ma) => self.compile_module_access(ma),
            ast::Expression::OrExpression(oe) => self.compile_or(oe),
            ast::Expression::PipeExpression(p) => {
                // Desugared away before type checking (`compile_with`/
                // `check_impl` rewrite every pipe first), same guarantee as
                // `Statement::Backpass`. Type both sides defensively so a
                // survivor still surfaces its operands' own diagnostics
                // instead of panicking.
                self.compile_expr(&p.left);
                self.compile_expr(&p.right);
                (self.engine.fresh_var(), CKind::Error)
            }
            ast::Expression::ErrorNode(err) => (self.error_node(err), CKind::Error),
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
    ) -> CExpr {
        if let Some(h) = hint
            && let ast::Expression::FunctionExpression(fe) = expr
        {
            let resolved = self.engine.find(h);
            if let TypeNode::Fun { params, .. } = self.engine.node(resolved)
                && params.len as usize == fe.params.len()
            {
                let params: Vec<Ty> = self.engine.children_of(params).to_vec();
                self.engine.enter_level();
                let (ty, closure) = self.compile_function_common(
                    &fe.params,
                    &fe.body,
                    fe.return_type.as_ref(),
                    Some(params.clone()),
                );
                self.engine.leave_level();
                return CExpr {
                    ty,
                    span: fe.span,
                    kind: CKind::Closure(closure),
                };
            }
        }
        self.compile_expr(expr)
    }

    // ========================================================================
    // Identifier
    // ========================================================================

    /// What a name denotes, decided here, in the frame that binds it, and
    /// recorded for the elaborator, which never resolves a name itself. Also
    /// says whether the name has a runtime binding.
    ///
    /// A `Local`/`ModuleFn` resolves the variable, with the usual `mark_used`
    /// and capture side effects. A constructor or builtin is fixed by its kind
    /// and has no plain runtime binding.
    fn name_at(&mut self, id: StrId, kind: ValueKind) -> (NameAt, bool) {
        if let Some(den) = Denotation::from_kind(kind) {
            return (NameAt::Den(den), false);
        }
        match self.resolve_variable(id) {
            Some(Place::ThisFunction(binder)) => (NameAt::Local(binder), true),
            Some(Place::Outside(den)) => (NameAt::Den(den), true),
            None if self.frames.at_toplevel() => (NameAt::Unbound, false),
            None => (NameAt::Den(Denotation::self_closure()), false),
        }
    }

    /// The declared parameter labels of a module `fn`. An empty slice is "not
    /// recorded", never "takes no parameters", so it refuses any label.
    fn param_labels_of(&self, kind: ValueKind) -> Option<Vec<StrId>> {
        match kind {
            ValueKind::ModuleFn { param_labels } if param_labels.len > 0 => {
                Some(self.engine.str_ids_of(param_labels).to_vec())
            }
            _ => None,
        }
    }

    /// How to reach a constructor named `name` that is not in scope: through
    /// each imported module that has one, as `alias::Name`.
    fn qualified_spellings(&self, name: &str) -> Vec<String> {
        let mut out: Vec<String> = self
            .imported_qualifiers
            .iter()
            .filter(|(_, key)| {
                self.module_table
                    .get(key)
                    .and_then(|m| m.values.get(name))
                    .is_some_and(|v| matches!(v.scheme.kind, ValueKind::Constructor { .. }))
            })
            .map(|(alias, _)| format!("`{alias}::{name}`"))
            .collect();
        out.sort();
        out
    }

    fn compile_identifier(&mut self, expr: &ast::Identifier) -> (Ty, CKind) {
        let name = &expr.name;

        let Some(scheme) = self.env.lookup(name) else {
            // A module's name is not a value: say what it is and how to reach
            // what is in it.
            if let Some(key) = self.imported_qualifiers.get(name) {
                let module = self.module_name(key);
                // A function or value of it, to show the shape of a use.
                let example = self.module_table.get(key).and_then(|m| {
                    m.values
                        .keys()
                        .filter(|k| k.starts_with(|c: char| c.is_ascii_lowercase()))
                        .min()
                        .cloned()
                });
                let example = match example {
                    Some(member) => format!(", like `{name}::{member}`"),
                    None => String::new(),
                };
                self.error(
                    format!(
                        "'{name}' is the module `{module}`, not a value. Use one of its members{example}"
                    ),
                    expr.span,
                );
                return (self.engine.fresh_var(), CKind::Error);
            }
            let qualified = self.qualified_spellings(name);
            if !qualified.is_empty() {
                self.error(
                    format!(
                        "Unknown identifier '{name}'. A constructor of that name is {}",
                        qualified.join(" or ")
                    ),
                    expr.span,
                );
            } else if let Some(suggestion) = self.env.suggest_name(name) {
                self.error(
                    format!(
                        "Unknown identifier '{}'. Closest match: '{}'.",
                        name, suggestion
                    ),
                    expr.span,
                );
            } else {
                self.error(format!("Unknown identifier '{}'", name), expr.span);
            }
            return (self.engine.fresh_var(), CKind::Error);
        };

        let ty = self.engine.instantiate(scheme, &self.frames.current.rigid);
        let kind = scheme.kind;
        let def = scheme.def;
        // `record` discards everything on the non-LSP path (see its early
        // return); skip the doc map probe + clone entirely there.
        let doc = self.doc_if_collecting(name);
        self.record(name, ty, expr.span, doc);
        self.record_value_use(def, expr.span, ReferenceKind::Unqualified);

        // `_`-prefixed locals are write-only: the prefix declares the value
        // intentionally unused, so a read contradicts the declaration.
        if name.starts_with('_') && matches!(kind, ValueKind::Local) {
            let msg = match name.trim_start_matches('_') {
                "" => format!(
                    "'{name}' is a discard binding and cannot be read; give it a name to use its value"
                ),
                stripped => format!(
                    "'{name}' is marked unused by its '_' prefix and cannot be read; rename it to '{stripped}'"
                ),
            };
            self.error(msg, expr.span);
            return (ty, CKind::Error);
        }

        let id = self.engine.intern(name);
        let (at, bound) = self.name_at(id, kind);
        let fact = NameFact {
            ty,
            at,
            param_labels: None,
        };
        self.check_named_value(name, None, expr.span, kind, bound);
        (ty, CKind::Name { name: id, fact })
    }

    /// Once a module's inference is final, reject any `_`-prefixed binder that
    /// discarded a value the type system knows is `Nil`: there is nothing to
    /// discard, and the explicit `Nil` pattern keeps the arm honest — it stops
    /// compiling the day the type grows real data.
    pub(super) fn check_nil_discards(&mut self) {
        let checks = std::mem::take(&mut self.nil_discards);
        for (name, ty, sp) in checks {
            let rep = self.engine.find(ty);
            if matches!(self.engine.node(rep), TypeNode::Con { id, .. } if self.prelude.nil().is(id))
            {
                self.error(
                    format!(
                        "'{name}' always matches Nil here; write the explicit 'Nil' pattern instead"
                    ),
                    sp,
                );
            }
        }
    }

    fn no_runtime_binding(&mut self, name: &str, qualifier: Option<&str>, sp: Span) {
        let msg = match qualifier {
            Some(m) => format!("'{name}' in module '{m}' has no runtime binding"),
            None => format!("'{name}' has a type but no runtime binding here"),
        };
        self.error(msg, sp);
    }

    /// Diagnose a named value used in value position. Constructors and
    /// builtins are legal (the elaborator synthesises a nullary build or an
    /// eta wrapper — see `typed_ir::eta`); a `Local`/`ModuleFn` with no
    /// runtime binding is an error.
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
            ValueKind::Local | ValueKind::ModuleFn { .. } => {
                if !has_binding {
                    self.no_runtime_binding(name, qualifier, sp);
                }
            }
        }
    }

    // ========================================================================
    // Binary ops
    // ========================================================================

    fn compile_binary(&mut self, expr: &ast::BinaryExpression) -> (Ty, CKind) {
        use ast::BinaryOp;
        let node = |left: CExpr, right: CExpr| CKind::Binary {
            op: expr.op,
            left: Box::new(left),
            right: Box::new(right),
        };
        let (constraint, is_cmp) = match expr.op {
            BinaryOp::And | BinaryOp::Or => {
                let left = self.compile_expr(&expr.left);
                let b = self.ty_bool();
                self.engine.unify_at(b, left.ty, expr.left.span());
                let right = self.compile_expr(&expr.right);
                self.engine.unify_at(b, right.ty, expr.right.span());
                return (self.ty_bool(), node(left, right));
            }
            BinaryOp::Eq | BinaryOp::Ne => {
                let left = self.compile_expr(&expr.left);
                let right = self.compile_expr(&expr.right);
                self.engine.unify_at(left.ty, right.ty, expr.span);
                return (self.ty_bool(), node(left, right));
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
        let left = self.compile_expr(&expr.left);
        let right = self.compile_expr(&expr.right);
        let operand = self.engine.fresh_constrained_var(constraint);
        self.engine.unify_at(operand, left.ty, expr.left.span());
        self.engine.unify_at(operand, right.ty, expr.right.span());
        let ty = if is_cmp { self.ty_bool() } else { operand };
        (ty, node(left, right))
    }

    // ========================================================================
    // Binary literals  ( `<<v:size:kind, ..>>` )
    // ========================================================================

    /// Type-check a `<<v:size:kind, ..>>` literal. Every segment's value (and
    /// its runtime size expression) is checked against the kind's expected
    /// type; the encoding itself is Core's job.
    fn compile_binary_literal(&mut self, bl: &ast::BinaryLiteral) -> (Ty, CKind) {
        let mut segs = Vec::with_capacity(bl.segments.len());
        for seg in &bl.segments {
            let value = self.compile_expr(&seg.value);
            let expected = match seg.spec {
                ast::BinSpec::Int { .. } => self.ty_int(),
                ast::BinSpec::Binary { .. } => self.ty_binary(),
                ast::BinSpec::Utf8 => self.ty_string(),
            };
            self.engine.unify_at(expected, value.ty, seg.value.span());
            let width = match &seg.spec {
                ast::BinSpec::Int { bits } => CWidth::Int(self.type_seg_size(bits.as_ref())),
                ast::BinSpec::Binary { bytes } => CWidth::Bytes(self.type_seg_size(bytes.as_ref())),
                ast::BinSpec::Utf8 => CWidth::Utf8,
            };
            segs.push(CBinSeg { value, width });
        }
        (self.ty_binary(), CKind::BinaryLiteral(segs))
    }

    /// A segment's size expression is a runtime `Int`, not a binding.
    fn type_seg_size(&mut self, size: Option<&ast::Expression>) -> Option<CExpr> {
        let e = size?;
        let size = self.compile_expr(e);
        let int_ty = self.ty_int();
        self.engine.unify_at(int_ty, size.ty, e.span());
        Some(size)
    }

    // ========================================================================
    // Array
    // ========================================================================

    /// Compile a run of non-spread array elements, unifying each with the
    /// array's element type variable.
    fn compile_array_run(
        &mut self,
        elems: &[ast::ArrayElement],
        elem_var: Ty,
        out: &mut Vec<CArrayElem>,
    ) {
        for elem in elems {
            if let ast::ArrayElement::Expression(e) = elem {
                let el = self.compile_expr(e);
                self.engine.unify_at(elem_var, el.ty, type_defining_span(e));
                out.push(CArrayElem::Elem(el));
            }
        }
    }

    fn compile_array(&mut self, expr: &ast::ArrayExpression) -> (Ty, CKind) {
        let elem_var = self.engine.fresh_var();
        let mut elems = Vec::with_capacity(expr.elements.len());
        let mut run_start = 0usize;
        for (i, e) in expr.elements.iter().enumerate() {
            let ast::ArrayElement::SpreadElement(spread) = e else {
                continue;
            };
            self.compile_array_run(&expr.elements[run_start..i], elem_var, &mut elems);
            run_start = i + 1;
            let inner = &spread.expression;
            let spread = self.compile_expr(inner);
            let expected = self.ty_array(elem_var);
            self.engine.unify_at(expected, spread.ty, inner.span());
            elems.push(CArrayElem::Spread(spread));
        }
        self.compile_array_run(&expr.elements[run_start..], elem_var, &mut elems);
        (self.ty_array(elem_var), CKind::Array(elems))
    }

    // ========================================================================
    // Function call
    // ========================================================================

    fn compile_call(&mut self, expr: &ast::FunctionCallExpression) -> (Ty, CKind) {
        // Resolve the callee to a name + scheme + load action without emitting
        // anything yet. This lets us dispatch on `ValueKind` for constructors
        // and builtins, and use the callee's known type to push hints into
        // function-literal arguments.
        if let Some(ResolvedCallee {
            name,
            span: name_span,
            scheme,
            has_binding,
            at,
            qualifier,
        }) = self.resolve_simple_callee(&expr.callee)
        {
            let inst_ty = self.engine.instantiate(&scheme, &self.frames.current.rigid);
            let param_labels = self.param_labels_of(scheme.kind);
            let fact = NameFact {
                ty: inst_ty,
                at,
                param_labels,
            };
            let id = self.engine.intern(name);
            let callee = match qualifier {
                None => CCallee::Name {
                    name: id,
                    fact,
                    span: name_span,
                },
                Some(_) => CCallee::Member {
                    name: id,
                    fact,
                    span: name_span,
                },
            };
            // Hover-only here; the graph occurrence (with the correct
            // Unqualified/Qualified kind) is emitted in `resolve_simple_callee`.
            // Skip the doc map probe + clone on the non-LSP path, where
            // `record` discards it (see its early return).
            let doc = self.doc_if_collecting(name);
            self.record(name, inst_ty, name_span, doc);

            let (ty, args) = match scheme.kind {
                ValueKind::Constructor {
                    type_id,
                    arity,
                    field_labels,
                    ..
                } => {
                    let (ty, args) = self.compile_ctor_call(
                        name,
                        type_id,
                        arity as usize,
                        field_labels,
                        inst_ty,
                        &expr.arguments,
                        expr.span,
                    );
                    (ty, Some(args))
                }
                ValueKind::Builtin { .. } => {
                    self.compile_call_args(inst_ty, None, &expr.arguments, expr.span)
                }
                ValueKind::Local | ValueKind::ModuleFn { .. } => {
                    // A module `fn` reached by name is the only callee here
                    // whose parameter names are known, so it is the only one
                    // that admits labelled arguments. An empty label slice
                    // means "not recorded", never "takes no parameters" (see
                    // `ValueKind::ModuleFn`), so it falls back to the
                    // positional-only refusal rather than to a zero-arity one.
                    let labels = match scheme.kind {
                        ValueKind::ModuleFn { param_labels } if param_labels.len > 0 => {
                            Some(param_labels)
                        }
                        _ => None,
                    };
                    let ret = self.compile_call_args(inst_ty, labels, &expr.arguments, expr.span);
                    if !has_binding {
                        let module = qualifier.as_ref().map(|k| self.module_name(k));
                        self.no_runtime_binding(name, module.as_deref(), name_span);
                    }
                    ret
                }
            };
            return match args {
                Some(args) => (ty, CKind::Call { callee, args }),
                None => (ty, CKind::Error),
            };
        }

        // Arbitrary callee expression: compile it first so we have its type
        // for argument hints. A `module::member` callee only gets here when
        // `resolve_simple_callee` has already reported it, so check just the
        // arguments rather than report it twice.
        let callee = match expr.callee.as_ref() {
            ast::Expression::ModuleAccessExpression(_) => None,
            callee => Some(self.compile_expr(callee)),
        };
        let callee_ty = match &callee {
            Some(c) => c.ty,
            None => self.engine.fresh_var(),
        };
        let (ty, args) = self.compile_call_args(callee_ty, None, &expr.arguments, expr.span);
        match (callee, args) {
            (Some(callee), Some(args)) => {
                let callee = CCallee::Expr(Box::new(callee));
                (ty, CKind::Call { callee, args })
            }
            _ => (ty, CKind::Error),
        }
    }

    /// Type-check and emit an argument list against a callee type using
    /// `match_fun_type`. The arguments come back in source order, or `None`
    /// when the callee is not a function at all.
    ///
    /// `param_labels` is the callee's declared parameter names when it is a
    /// module `fn` reached by name, and `None` for every other callee — a local
    /// binding holding a function, a `@vm` builtin, an arbitrary expression —
    /// none of which has parameter names at the call site. Only the first
    /// admits labelled arguments; the rest still refuse them. Spread stays
    /// constructor-only either way.
    ///
    /// The arguments are checked in *source* order, so the permutation a
    /// labelled call needs is applied to the parameter *types* here and to the
    /// emitted argument vector in `Elab::value_call`. Both sides take it from
    /// [`slot_labeled`] over this one slice, so the order the arguments are
    /// checked in and the order they are passed in cannot disagree.
    fn compile_call_args(
        &mut self,
        callee_ty: Ty,
        param_labels: Option<ArenaSlice<pool::StrSlices>>,
        args: &[ast::CallArg],
        call_span: Span,
    ) -> (Ty, Option<Vec<CArg>>) {
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
                return (self.engine.fresh_var(), None);
            }
        };

        // Which parameter each source argument fills. A call with no label is
        // the identity, so only a labelled one pays for the slotting.
        let labelled = args
            .iter()
            .any(|a| matches!(a, ast::CallArg::Labeled { .. }));
        let param_of_arg: Vec<Option<usize>> = match param_labels {
            Some(sl) if labelled => self.slot_call_args(sl, args),
            _ => (0..args.len()).map(Some).collect(),
        };

        let mut out = Vec::with_capacity(args.len());
        for (i, arg) in args.iter().enumerate() {
            let hint = param_of_arg[i].and_then(|p| params.get(p).cloned());
            let (value, arg_span) = match arg {
                ast::CallArg::Positional(e) => {
                    let v = self.compile_expr_with_hint(e, hint);
                    let ty = v.ty;
                    out.push(CArg::Positional(v));
                    (ty, e.span())
                }
                ast::CallArg::Labeled { label, value, .. } => {
                    if param_labels.is_none() {
                        self.error(
                            "Labelled arguments need a callee whose parameter names are known: \
                             a constructor, or a function named directly"
                                .to_string(),
                            label.span,
                        );
                    }
                    let v = self.compile_expr_with_hint(value, hint);
                    let ty = v.ty;
                    let label = self.engine.intern(&label.name);
                    out.push(CArg::Labeled { label, value: v });
                    (ty, value.span())
                }
                ast::CallArg::Spread(e) => {
                    self.error(
                        "Spread arguments are only allowed in constructor record-update calls"
                            .to_string(),
                        e.span(),
                    );
                    let v = self.compile_expr(e);
                    let ty = v.ty;
                    out.push(CArg::Spread(v));
                    (ty, e.span())
                }
            };
            if let Some(p) = hint {
                self.engine.unify_at(p, value, arg_span);
            }
        }

        (ret, Some(out))
    }

    /// Map each source argument to the parameter it fills, reporting a label
    /// naming no parameter and a parameter supplied twice. `None` for an
    /// argument that filled no slot: a positional past the last parameter,
    /// whose arity `match_fun_type` has already reported, or a bad label
    /// reported here.
    fn slot_call_args(
        &mut self,
        param_labels: ArenaSlice<pool::StrSlices>,
        args: &[ast::CallArg],
    ) -> Vec<Option<usize>> {
        // Interning up front releases the `&mut engine` borrow before the
        // `str_ids_of` slice is taken.
        let items: Vec<(Option<StrId>, (usize, Span))> = args
            .iter()
            .enumerate()
            .filter_map(|(i, a)| match a {
                ast::CallArg::Positional(e) => Some((None, (i, e.span()))),
                ast::CallArg::Labeled { label, .. } => {
                    Some((Some(self.engine.intern(&label.name)), (i, label.span)))
                }
                // Spread is refused by the caller and fills no parameter.
                ast::CallArg::Spread(_) => None,
            })
            .collect();
        // One slot per declared parameter, both sized and named from this one
        // slice, so there is no second width for a label table to drift from.
        let names: Vec<Option<StrId>> = self
            .engine
            .str_ids_of(param_labels)
            .iter()
            .map(|&n| Some(n))
            .collect();
        let (by_pos, errors) = slot_labeled(&names, items);
        for e in errors {
            match e {
                // An arity error `match_fun_type` has already reported.
                SlotError::ExtraPositional(_) => {}
                SlotError::UnknownLabel(label, (_, span)) => {
                    let available = self.engine.strs_of(param_labels).join(", ");
                    let label = self.engine.str(label).to_string();
                    self.error(
                        format!(
                            "This function has no parameter '{}'. Available: {}",
                            label, available
                        ),
                        span,
                    );
                }
                SlotError::Duplicate((_, span), param) => {
                    let dup = self
                        .engine
                        .str_ids_of(param_labels)
                        .get(param)
                        .map_or("_".to_string(), |&id| self.engine.str(id).to_string());
                    self.error(
                        format!("Parameter '{}' is specified more than once", dup),
                        span,
                    );
                }
            }
        }
        let mut out = vec![None; args.len()];
        for (p, slot) in by_pos.into_iter().enumerate() {
            if let Some((i, _)) = slot {
                out[i] = Some(p);
            }
        }
        out
    }

    fn compile_call_arg_value(&mut self, arg: &ast::CallArg) -> CExpr {
        match arg {
            ast::CallArg::Positional(e) => self.compile_expr(e),
            ast::CallArg::Labeled { value, .. } => self.compile_expr(value),
            ast::CallArg::Spread(e) => self.compile_expr(e),
        }
    }

    /// Resolve `module::member`: look the member up, note whether it has a
    /// runtime binding, and record the `Qualified` value-use. Shared by the
    /// callee position (`resolve_simple_callee`) and the value position
    /// (`compile_module_access`). `None` means the access has already been
    /// diagnosed.
    ///
    /// The module name is looked up among the imports only. A value binding of
    /// the same name cannot shadow it, because `.` and `::` never overlap.
    fn resolve_module_access<'a>(
        &mut self,
        ma: &'a ast::ModuleAccessExpression,
    ) -> Option<QualifiedMember<'a>> {
        let Some(module_key) = self.imported_qualifiers.get(&ma.module.name).cloned() else {
            self.unknown_module(&ma.module);
            return None;
        };
        let member = &ma.member;
        let (scheme, slot) = self.lookup_module_member(&module_key, &member.name, member.span)?;
        // The member is not in unqualified scope, so resolve the occurrence
        // through the imported scheme's canonical `def`.
        self.record_value_use(scheme.def, member.span, ReferenceKind::Qualified);
        self.record_qualifier_use(&ma.module);
        // A member with neither a fixed denotation nor a slot has no runtime
        // binding, which `check_named_value` reports, so nothing elaborates it.
        let at = match (Denotation::from_kind(scheme.kind), slot) {
            (Some(den), _) => NameAt::Den(den),
            (None, Some(slot)) => NameAt::Den(self.global_denotation(slot)),
            (None, None) => NameAt::Unbound,
        };
        Some(QualifiedMember {
            module_key,
            member_name: &member.name,
            member_span: member.span,
            scheme,
            has_binding: slot.is_some(),
            at,
        })
    }

    /// `name::member` where `name` is not an imported module. When a stdlib
    /// module goes by that name, say which import brings it in.
    fn unknown_module(&mut self, name: &ast::Identifier) {
        let modules: Vec<String> = crate::module::stdlib_modules()
            .into_iter()
            .filter(|path| path.len() > 1 && path.last() == Some(&name.name))
            .map(|path| format!("`import {}`", path.join("/")))
            .collect();
        let msg = if modules.is_empty() {
            format!(
                "Unknown module '{}'. Import it before reaching into it with `::`",
                name.name
            )
        } else {
            format!(
                "Unknown module '{}': it is the name of a stdlib module that is not imported. Add {}",
                name.name,
                modules.join(" or ")
            )
        };
        self.module_error(msg, name.span);
    }

    /// Best-effort resolution of a callee expression that is a plain name (or
    /// `module.name`). Returns the looked-up scheme and whether the callee has
    /// a runtime binding. Anything else falls through to the
    /// arbitrary-expression path.
    fn resolve_simple_callee<'a>(
        &mut self,
        callee: &'a ast::Expression,
    ) -> Option<ResolvedCallee<'a>> {
        match callee {
            ast::Expression::Identifier(id) => {
                let scheme = *self.env.lookup(&id.name)?;
                let sid = self.engine.intern(&id.name);
                let (at, has_binding) = self.name_at(sid, scheme.kind);
                // This path bypasses `compile_identifier`, so it is the genuine
                // unqualified-use seam for calls.
                self.record_value_use(scheme.def, id.span, ReferenceKind::Unqualified);
                Some(ResolvedCallee {
                    name: &id.name,
                    span: id.span,
                    scheme,
                    has_binding,
                    at,
                    qualifier: None,
                })
            }
            // Qualified callee `module::member()`. A failed lookup has already
            // been diagnosed, so it is "no simple callee" too.
            ast::Expression::ModuleAccessExpression(ma) => {
                let m = self.resolve_module_access(ma)?;
                Some(ResolvedCallee {
                    name: m.member_name,
                    span: m.member_span,
                    scheme: m.scheme,
                    has_binding: m.has_binding,
                    at: m.at,
                    qualifier: Some(m.module_key),
                })
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
        type_id: TypeId,
        arity: usize,
        field_labels_sl: ArenaSlice<pool::StrSlices>,
        inst_ty: Ty,
        args: &[ast::CallArg],
        call_span: Span,
    ) -> (Ty, Vec<CArg>) {
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
        // value structurally remembers which `args[i]` it came from, and its
        // checked node goes back to that argument.
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
                ast::CallArg::Labeled { label, value, .. } => {
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

        // Checking runs spread-first and then in declared-field order: the
        // spread's `unify_at` solves `result_ty` (and with it the constructor's
        // type params) before any `compile_expr_with_hint` pushes a parameter
        // type into a function-literal argument, and the diagnostics a
        // mistyped argument raises come out in field order. The checked nodes
        // still go back in source order, which is the order the elaborator
        // spills them in.
        //
        // An argument `slot_ctor_args` did not place is an arity or label error
        // it has already reported, and is left unchecked.
        let slots = &by_pos[..(field_labels_sl.len as usize).min(by_pos.len())];
        let mut checked: Vec<Option<CExpr>> = (0..args.len()).map(|_| None).collect();

        // Record-update: the base is unified with the constructor's result
        // type; the per-field projection is Core's job.
        if let Some((ai, e)) = spread {
            let base = self.compile_expr(e);
            self.engine.unify_at(result_ty, base.ty, e.span());
            checked[ai] = Some(base);
            let unfilled = (0..arity).filter(|&i| slots.get(i).is_none_or(Option::is_none));
            self.check_spread_fills(type_id, result_ty, field_labels_sl, unfilled, e.span());
        }

        for (i, slot_expr) in slots.iter().enumerate() {
            let Some(&(ai, value)) = *slot_expr else {
                continue;
            };
            let expected = param_tys.get(i).copied();
            let v = self.compile_expr_with_hint(value, expected);
            if let Some(p) = expected {
                self.engine.unify_at(p, v.ty, value.span());
            }
            checked[ai] = Some(v);
        }

        (result_ty, self.ctor_args(args, checked))
    }

    /// A constructor call's arguments in source order, from the values the
    /// check walk produced for them, `checked[i]` for `args[i]`. An argument
    /// with no value is one `slot_ctor_args` refused and has reported.
    fn ctor_args(&mut self, args: &[ast::CallArg], checked: Vec<Option<CExpr>>) -> Vec<CArg> {
        args.iter()
            .zip(checked)
            .map(|(a, v)| {
                let span = match a {
                    ast::CallArg::Positional(e) | ast::CallArg::Spread(e) => e.span(),
                    ast::CallArg::Labeled { value, .. } => value.span(),
                };
                let v = v.unwrap_or_else(|| CExpr {
                    ty: self.engine.fresh_var(),
                    span,
                    kind: CKind::Error,
                });
                match a {
                    ast::CallArg::Positional(_) => CArg::Positional(v),
                    ast::CallArg::Labeled { label, .. } => CArg::Labeled {
                        label: self.engine.intern(&label.name),
                        value: v,
                    },
                    ast::CallArg::Spread(_) => CArg::Spread(v),
                }
            })
            .collect()
    }

    /// `C(..base, ...)` fills each field it leaves out with `base.field`, so
    /// each of those has to be a field `base.field` could read: present on
    /// every variant of the type, by name. On a type with one variant that is
    /// every field. On a type with several it is only the fields they all
    /// share, and a spread that leaves out any other is refused, since which
    /// variant `base` is is not known until it runs.
    fn check_spread_fills(
        &mut self,
        type_id: TypeId,
        result_ty: Ty,
        field_labels: ArenaSlice<pool::StrSlices>,
        unfilled: impl Iterator<Item = usize>,
        span: Span,
    ) {
        let resolved = self.engine.find(result_ty);
        let (type_name, type_args) = match self.engine.node(resolved) {
            TypeNode::Con { name, args, .. } => (
                self.engine.str(name).to_string(),
                self.engine.children_of(args).to_vec(),
            ),
            _ => return,
        };
        let Some(info) = self.env.lookup_type_info_by_id(type_id) else {
            return;
        };
        let labels: SmallVec<[StrId; 4]> =
            SmallVec::from_slice(self.engine.str_ids_of(field_labels));
        for i in unfilled {
            let Some(&label) = labels.get(i) else {
                continue;
            };
            if let Err(FieldMismatch::MissingOn(variant)) =
                self.field_in_variants(info, &type_args, label, Some(span))
            {
                let field = self.engine.str(label).to_string();
                let why = self.missing_on(variant, &type_name);
                self.error(
                    format!(
                        "The spread cannot fill field '{field}', which {why}; write '{field}:' in the call"
                    ),
                    span,
                );
            }
        }
    }

    // ========================================================================
    // Property access
    // ========================================================================

    /// `module::member` in value position. Calls are handled in
    /// `compile_call`, through `resolve_simple_callee`.
    fn compile_module_access(&mut self, expr: &ast::ModuleAccessExpression) -> (Ty, CKind) {
        let Some(m) = self.resolve_module_access(expr) else {
            return (self.engine.fresh_var(), CKind::Error);
        };
        let ty = self
            .engine
            .instantiate(&m.scheme, &self.frames.current.rigid);
        let fact = NameFact {
            ty,
            at: m.at,
            param_labels: None,
        };
        self.record(m.member_name, ty, m.member_span, None);
        // Rendered lazily: the qualifier only reaches a diagnostic on the
        // no-runtime-binding path, so the common (bound) case skips the
        // display-name lookup entirely.
        let module = (!m.has_binding).then(|| self.module_name(&m.module_key));
        self.check_named_value(
            m.member_name,
            module.as_deref(),
            m.member_span,
            m.scheme.kind,
            m.has_binding,
        );
        let name = self.engine.intern(m.member_name);
        (ty, CKind::Member { name, fact })
    }

    fn compile_property_access(&mut self, expr: &ast::PropertyAccessExpression) -> (Ty, CKind) {
        // `c.f` where `c` is an imported module and not a value: point at `::`.
        if let (ast::Expression::Identifier(id), ast::PropertyKey::Field(field)) =
            (expr.left.as_ref(), &expr.right)
            && self.env.lookup(&id.name).is_none()
            && self.imported_qualifiers.contains_key(&id.name)
        {
            self.error(
                format!(
                    "'{0}' is a module, not a value. Reach its members with `::`, like `{0}::{1}`",
                    id.name, field.name
                ),
                id.span,
            );
            return (self.engine.fresh_var(), CKind::Error);
        }

        let recv = self.compile_expr(&expr.left);
        let left_ty = recv.ty;

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
                        return (self.engine.fresh_var(), CKind::Error);
                    }
                };

                let resolved = self.engine.find(left_ty);
                if let TypeNode::Tuple { elems } = self.engine.node(resolved) {
                    let elements = self.engine.children_of(elems);
                    if let Ok(idx) = u32::try_from(index)
                        && let Some(&ty) = elements.get(idx as usize)
                    {
                        let kind = CKind::TupleIndex {
                            recv: Box::new(recv),
                            idx,
                        };
                        return (ty, kind);
                    }
                    self.error(
                        format!(
                            "Tuple index {} out of bounds (tuple has {} elements)",
                            index,
                            elements.len()
                        ),
                        num.span,
                    );
                    return (self.engine.fresh_var(), CKind::Error);
                }
                self.error(
                    format!("Cannot index .{} on non-tuple type", index),
                    num.span,
                );
                (self.engine.fresh_var(), CKind::Error)
            }
            // `value.field` — labelled field shared by every variant.
            ast::PropertyKey::Field(field) => {
                let ty = self.compile_field_access(left_ty, &field.name, field.span);
                let kind = CKind::Field {
                    recv: Box::new(recv),
                    label: self.engine.intern(&field.name),
                };
                (ty, kind)
            }
        }
    }

    /// compile_field_access error path: report and yield a fresh tyvar.
    fn field_access_bail(&mut self, msg: String, span: Span) -> Ty {
        self.error(msg, span);
        self.engine.fresh_var()
    }

    /// Single source of truth for the nominal `.field` lookup, by name: where
    /// the field sits in each variant, plus its instantiated type. A field is
    /// only readable when EVERY variant carries that label at a unifiable
    /// type. Its position may differ from one variant to the next.
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
    ) -> Result<SharedField, FieldMismatch> {
        let variants = info.variants().ok_or(FieldMismatch::NotNominal)?;
        let mut ty: Option<Ty> = None;
        let mut slots = SmallVec::new();
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
            slots.push(i as u32);
            let substituted = self
                .engine
                .substitute_type_vars(fty, info.type_params, type_args);
            match ty {
                Some(existing) => {
                    if let Some(span) = unify_span {
                        self.engine.unify_at(existing, substituted, span);
                    }
                }
                None => ty = Some(substituted),
            }
        }
        let ty = ty.ok_or(FieldMismatch::NoVariants)?;
        Ok(SharedField { ty, slots })
    }

    /// The end of a sentence naming a field some variant of `type_name` lacks.
    fn missing_on(&self, variant: StrId, type_name: &str) -> String {
        format!(
            "is not present on every variant of '{type_name}' (missing on '{}')",
            self.engine.str(variant)
        )
    }

    /// Project `.field` out of a value. Because the runtime value's variant is
    /// not statically known, the field is only accessible if EVERY variant of
    /// the receiver type carries a label of that name and at the same type,
    /// wherever in the variant it sits.
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
            Ok(field) => field.ty,
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
                let why = self.missing_on(variant, &type_name);
                return self.field_access_bail(format!("Field '{field}' {why}"), field_span);
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

    fn compile_or(&mut self, expr: &ast::OrExpression) -> (Ty, CKind) {
        let lhs = self.compile_expr(&expr.expression);
        let left_ty = lhs.ty;
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
        let err_var = if self.prelude.option().is(lhs_type_id) {
            if let Some(recv) = &expr.receiver {
                self.error(
                    "'or' on an Option does not bind a value (the failure case carries nothing)"
                        .to_string(),
                    recv.span,
                );
            }
            None::<Ty>
        } else if self.prelude.result().is(lhs_type_id) {
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
            return (self.engine.fresh_var(), CKind::Error);
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
        let mut receiver = None;
        if let Some(e) = err_var
            && let Some(recv) = &expr.receiver
        {
            self.get_or_create_local(&recv.name);
            self.register_local_binding(&recv.name, e, recv.span);
            let sid = self.engine.intern(&recv.name);
            receiver = Some((sid, self.publish_local(sid)));
        }
        let body = self.compile_expr(&expr.body);
        self.pop_block_scope();
        self.engine
            .unify_at(success_var, body.ty, type_defining_span(&expr.body));

        let kind = CKind::Or {
            lhs: Box::new(lhs),
            receiver,
            body: Box::new(body),
        };
        (success_var, kind)
    }

    // ========================================================================
    // Functions
    // ========================================================================

    /// Typecheck a function body inside a live `enter_fn_frame`/
    /// `finish_fn_frame` pair with params already bound, and park it. This is
    /// the single seam between the typecheck [`Self::compile_expr`] walk and
    /// the Core IR pipeline, and it is a *hand-off*, not a call: the walk is
    /// where inference, diagnostics, hover facts and reference-graph collection
    /// happen, and it lowers nothing at all.
    ///
    /// The parked body is handed to `typed_ir::elaborate_body`→
    /// [`core_ir::lower`](crate::core_ir::lower)→`perceus` in pass 6,
    /// once the whole module has been walked. There is no fallback path and no
    /// error path: the elaborator covers every form a [`CleanModule`] can
    /// contain, so it returns a [`TypedFn`] rather than a `Result`.
    ///
    /// Elaboration and lowering run under `check_only` too: they are the only
    /// passes that can prove a well-typed program is actually compilable, so
    /// `scarlet check` must not skip them.
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
        param_binders: &[BinderId],
        body: &ast::Expression,
    ) -> (Ty, ParkedBody) {
        // Typecheck walk. Nested closures encountered here re-enter this fn
        // (via `compile_function_common`) and park their own bodies + reserve
        // their slots before we reserve ours.
        let params_end = self.frames.current.slots;
        let checked = self.compile_expr(body);
        let body_ty = checked.ty;
        // The walk still handed out slots for the pattern binds it reserved;
        // those slots are dead, so rewind to the params.
        self.frames.current.slots = params_end;
        debug_assert!(
            self.defer_depth > 0,
            "every function body must be walked inside a deferral region: \
             the Core pipeline runs after the walk, never during it"
        );
        // An ill-typed body is parked like any other and closed out empty at
        // drain time, where `CleanModule` cannot be minted for it: the
        // elaborator is never shown a subtree the checker rejected.
        let name = self
            .frames
            .current
            .self_name
            .unwrap_or_else(|| self.engine.intern("__anon__"));
        let param_binds: Vec<(StrId, BinderId, Ty)> = params
            .iter()
            .zip(param_binders)
            .zip(param_tys)
            .map(|((p, &binder), &ty)| (self.engine.intern(&p.identifier.name), binder, ty))
            .collect();
        let parked = ParkedBody {
            name,
            param_binds,
            body: checked,
            body_ty,
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
    /// only constructors of the [`TypedProgram`] that `lower` and `perceus`
    /// consume. Consuming a [`CleanModule`] here therefore closes the
    /// whole pipeline to a module that reported an error: not by convention,
    /// but because a poisoned module cannot produce the value the passes take.
    /// (`lower` needs no proof of its own for exactly that reason.)
    ///
    /// `at` says where the elaborated function belongs in the program it is
    /// lowered as part of: `Some(func_idx)` for a function body, whose reserved
    /// slot fixes its [`FuncIdx`]; `None` for a module toplevel, which is
    /// `TypedProgram::toplevel` and has no index. Either way the returned
    /// [`LoweredBody`] carries its `CoreFn`, and the eta wrappers the
    /// elaborator minted are already in their own slots.
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
        let im = self.immediates();
        for f in &mut fns {
            crate::core_ir::immediates::rewrite(f, &im);
        }
        // Names come from the typed program, index for index: `lower` keeps
        // `fns` in `TypedProgram::fns` order.
        let names: Vec<StrId> = program.fns.iter().map(|f| f.name).collect();
        let pool = Rc::new(program.pool);
        let wrappers = fns.split_off(eta_base);
        let wrappers = names[eta_base..].iter().copied().zip(wrappers).collect();
        self.materialize_eta_wrappers(&pool, eta_base, wrappers);
        let (name, core) = match at {
            Some(func_idx) => (names[func_idx.index()], fns.swap_remove(func_idx.index())),
            None => {
                let mut top = CoreFn {
                    params: Vec::new(),
                    body: toplevel,
                    ret_ty: program.toplevel.ret,
                };
                crate::core_ir::immediates::rewrite(&mut top, &im);
                (program.toplevel.name, top)
            }
        };
        LoweredBody { name, core, pool }
    }

    /// Elaborate one body into a whole-module [`TypedProgram`], reserving a
    /// function slot for every eta wrapper the walk minted.
    ///
    /// `TypedProgram::fns` is `FuncIdx`-indexed and so is [`Self::fns`], so the
    /// two must agree: `fns` is padded up to `Self::fns.len()` before the walk
    /// (`FnTable::push` mints each `FuncIdx` in append order), and each wrapper
    /// appended past that point gets its slot reserved here, in order. Nothing
    /// else may reserve a slot while the walk runs.
    fn elaborate(
        &mut self,
        at: Option<crate::core_ir::FuncIdx>,
        build: impl FnOnce(&mut Self, &mut ResolvedPool, &mut FnTable) -> TypedFn,
    ) -> Elaborated {
        let eta_base = self.fns.len();
        let mut pool = pool_for(&self.engine);
        // `RTy`s name nodes of `pool`, so the memo dies with the previous one.
        self.rty_cache.clear();
        let temps = TempTys::intern(self, &mut pool);
        let nil_ty = self.ty_nil();
        let nil = PreludeTys::resolve_rty(self, &mut pool, nil_ty);
        // The `fns` entries an earlier body already owns. Never lowered into
        // anything the caller reads — they exist so the next `FnTable::push`
        // lands on the slot reserved for it below.
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

        let built = build(self, &mut pool, &mut fns);

        if self.fns.len() != eta_base {
            function_reserved_during_elaboration();
        }
        for _ in fns.tail_from(FuncIdx::from_usize(eta_base)) {
            self.fns.push(None);
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
                pool,
                temps,
            },
            eta_base,
        }
    }

    /// Perceus the eta wrappers `fns[base..]` and fill the slots
    /// [`Self::elaborate`] reserved for them. They share the pool of the body
    /// that minted them.
    fn materialize_eta_wrappers(
        &mut self,
        pool: &Rc<ResolvedPool>,
        base: usize,
        wrappers: Vec<(StrId, CoreFn)>,
    ) {
        use crate::core_ir::perceus;
        for (i, (name, w)) in wrappers.into_iter().enumerate() {
            let core = perceus::perceus(pool, w);
            self.fns[FuncIdx::from_usize(base + i)] =
                Some(self.lowered_fn(name, core, Rc::clone(pool)));
        }
    }

    /// The prelude constructors Core IR spells as immediates.
    fn immediates(&self) -> crate::core_ir::immediates::Immediates {
        crate::core_ir::immediates::Immediates {
            true_: self.prelude.true_().into(),
            false_: self.prelude.false_().into(),
            nil: self.prelude.nil_ctor().into(),
        }
    }

    /// Package a finished body for the [`Program`]. Called while the module
    /// that owns the body is the one being compiled, which is what makes
    /// `current_module_key` its module.
    fn lowered_fn(&self, name: StrId, core: CoreFn, pool: Rc<ResolvedPool>) -> LoweredFn {
        LoweredFn::new(
            self.current_module_key.to_string(),
            self.engine.str(name).to_string(),
            core,
            pool,
        )
    }

    /// Run the Core pipeline (elaborate→`lower`→`perceus`) over one already
    /// typechecked body and fill its function slot.
    ///
    /// Runs in pass 6, after the whole module has been walked; the only caller
    /// is [`Self::elaborate_deferred`]. `func_idx` is the slot the walk
    /// reserved. Under `check_only` the pipeline stops after `lower` and the
    /// slot stays empty.
    ///
    /// The `CleanModule` is the caller's proof that this body typechecked (that
    /// nothing in the module failed to): a poisoned body has no types to
    /// elaborate.
    #[allow(clippy::too_many_arguments)]
    fn elaborate_body(
        &mut self,
        clean: CleanModule,
        name: StrId,
        param_binds: &[(StrId, BinderId, Ty)],
        body: &CExpr,
        body_ty: Ty,
        func_idx: FuncIdx,
    ) {
        use crate::core_ir::perceus;
        let LoweredBody { core, pool, .. } =
            self.elaborate_then_materialize(clean, Some(func_idx), |c, pool, fns| {
                typed_ir::elaborate_body(c, pool, fns, name, param_binds, body, body_ty, false)
            });
        if self.check_only {
            return;
        }
        let core = perceus::perceus(&pool, core);
        if std::env::var("CORE_DBG").is_ok() {
            eprintln!("=== {}\n{core}", self.engine.str(name));
        }
        self.fns[func_idx] = Some(self.lowered_fn(name, core, pool));
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
    /// `lower(p: &TypedProgram) -> CoreProgram` needs to exist: lowering cannot
    /// be a step of the typecheck walk if the walk's product is the module.
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
    /// What deferral does *not* move is which slot a declared body owns: those
    /// are reserved by `finish_fn_frame` during the walk, in walk order, in
    /// both `check` and `build`.
    pub(super) fn begin_deferred_elaboration(&mut self) {
        self.defer_depth += 1;
    }

    /// Close the region opened by [`Self::begin_deferred_elaboration`] and, at
    /// depth zero, run the Core pipeline over every parked body in walk order
    /// — innermost closure first. This is the module's whole `lower`→`perceus`
    /// phase: one loop, after the typecheck walk, over every body the walk
    /// produced.
    pub(super) fn end_deferred_elaboration(&mut self) {
        self.defer_depth -= 1;
        if self.defer_depth > 0 {
            return;
        }
        let bodies = std::mem::take(&mut self.deferred_bodies);
        for d in bodies {
            // Re-proved per body, not once for the run: `elaborate_deferred`
            // consumes the proof, and an internal error raised while lowering
            // one body poisons the module for the next.
            //
            // A decl in the module failed to typecheck when there is no proof:
            // the parked bodies may reference names inference never resolved,
            // and there is no typed IR to lower. Their slots stay empty, and a
            // module with an error builds no program.
            if let Some(clean) = self.clean_module() {
                self.elaborate_deferred(d, clean);
            }
        }
    }

    /// Elaborate a parked body. Everything it needs about names and closures
    /// is on the checked tree, so no frame state is restored for it.
    fn elaborate_deferred(&mut self, d: DeferredBody, clean: CleanModule) {
        self.elaborate_body(
            clean,
            d.name,
            &d.param_binds,
            &d.body,
            d.body_ty,
            d.func_idx,
        );
    }

    /// Compile a `fn(...) { ... }` expression. `param_hints` is `Some` when
    /// the lambda is being passed directly to a call site whose parameter
    /// types are known; in that case any unannotated parameter is given the
    /// hinted type rather than a fresh var so the body can immediately do
    /// field access etc.
    fn compile_function_common(
        &mut self,
        params: &[ast::FunctionParameter],
        body: &ast::Expression,
        return_annot: Option<&ast::TypeIdentifier>,
        param_hints: Option<Vec<Ty>>,
    ) -> (Ty, ClosureFact) {
        let saved = self.enter_fn_frame(None);

        // A lambda's annotation type-variables share scope with each other but
        // are local to the lambda; mint them via a fresh hydrator.
        let mut hydrator = Hydrator::new(AnnotationContext::Signature);
        let mut param_tys: Vec<Ty> = Vec::with_capacity(params.len());
        let mut binders: Vec<BinderId> = Vec::with_capacity(params.len());
        for (i, param) in params.iter().enumerate() {
            let p_ty = match &param.typ {
                Some(annot) => self.hydrate(&mut hydrator, annot),
                None => match &param_hints {
                    Some(h) if i < h.len() => h[i],
                    _ => self.engine.fresh_var(),
                },
            };
            param_tys.push(p_ty);
            binders.push(self.bind_param(param, p_ty));
        }
        // Any tyvars the hydrator minted are rigid for the body.
        self.frames
            .current
            .rigid
            .extend(hydrator.rigid_ids().iter().copied());

        let (body_ty, body_emit) = self.compile_fn_body(params, &param_tys, &binders, body);
        let ret_ty = match return_annot {
            Some(rt) => {
                let annot_ty = self.hydrate(&mut hydrator, rt);
                self.engine
                    .unify_at(annot_ty, body_ty, type_defining_span(body));
                annot_ty
            }
            None => body_ty,
        };

        // `finish_fn_frame` restored the enclosing frame, so the closure is as
        // that frame sees it, and it is that frame's elaboration that builds it.
        let closure = self.finish_fn_frame(saved, body_emit);
        (self.engine.mk_fun(&param_tys, ret_ty), closure)
    }

    /// Compile a top-level `fn` whose parameter and return types were already
    /// hydrated in Pass 3. The supplied `hydrator` carries the rigid generic
    /// ids for any annotated type variables so that `instantiate` inside the
    /// body leaves them intact, and we unify the body's inferred type against
    /// the preregistered shape rather than re-reading the annotations.
    ///
    /// `global_slot` is this fn's entry-frame slot from `Prepared::Fn`,
    /// threaded from Pass 3 so `global_to_func` is keyed by the same slot the
    /// caller emits `StoreLocal` for — no reliance on the name's local entry
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
        self.frames.current.rigid = hydrator.rigid_ids().clone();
        let binders: Vec<BinderId> = params
            .iter()
            .zip(param_tys.iter())
            .map(|(param, p_ty)| self.bind_param(param, *p_ty))
            .collect();

        let (body_ty, body_emit) = self.compile_fn_body(params, &param_tys, &binders, body);
        self.engine
            .unify_at(ret_ty, body_ty, type_defining_span(body));

        // `global_to_func` is how the module toplevel's elaboration finds this
        // fn's body (`ElabCtx::fn_of_global`, off the same slot it stores into),
        // in every mode — so the index has to be a real one under `check_only`
        // too.
        // `finish_fn_frame` reserved it; take it from there rather than
        // re-deriving it from `fns.len()`, which is only the same number by
        // accident of nothing else having reserved since.
        let func_idx = self.finish_fn_frame(saved, body_emit).func_idx;
        self.global_to_func.insert(global_slot, func_idx);
        self.engine.mk_fun(&param_tys, ret_ty)
    }

    /// Start walking a nested body, and open a new type-env scope for it.
    ///
    /// A lambda has no name to call itself by unless it is the init of a
    /// `name = fn(..)` binding, which leaves that name in `next_fn_self_name`.
    /// It must not inherit the enclosing body's: a lambda passed to a
    /// higher-order function and calling the enclosing `fn` would call itself.
    fn enter_fn_frame(&mut self, binding: Option<&str>) -> frame::Entered {
        let self_name = match binding {
            Some(n) => Some(self.engine.intern(n)),
            None => self.next_fn_self_name.take(),
        };
        self.env.push_scope();
        self.frames.enter(self_name)
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

    fn bind_param(&mut self, p: &ast::FunctionParameter, ty: Ty) -> BinderId {
        self.get_or_create_local(&p.identifier.name);
        self.register_local_binding(&p.identifier.name, ty, p.identifier.span);
        let sid = self.engine.intern(&p.identifier.name);
        self.publish_local(sid)
    }

    /// Close out a function body: reserve its function slot, park it as a
    /// [`DeferredBody`], and restore the enclosing frame and type-env. Returns
    /// the closure as the enclosing frame sees it: the slot, and how each
    /// capture loads there, which goes on the enclosing frame's checked tree.
    ///
    /// Nothing is lowered here: the body is lowered in pass 6, after the walk.
    fn finish_fn_frame(&mut self, entered: frame::Entered, parked: ParkedBody) -> ClosureFact {
        let mut finished = self.frames.leave(entered);
        let captured = std::mem::take(&mut finished.captures).into_names();
        self.env.pop_scope();
        self.report_unused(finished.into_root());

        // Reserve the body's slot now, so the `func_idx` that the
        // `ClosureFact` and `global_to_func` are about to record is the one the
        // elaborated body fills in.
        let func_idx = self.fns.push(None);
        let ParkedBody {
            name: body_name,
            param_binds,
            body,
            body_ty,
        } = parked;
        self.deferred_bodies.push(DeferredBody {
            name: body_name,
            param_binds,
            body,
            body_ty,
            func_idx,
        });

        // Re-resolving each capture in the *enclosing* frame is load-bearing:
        // `resolve_variable` promotes a name this body captured from further
        // out into the enclosing frame's own capture set, so transitive
        // captures chain outwards. It is also how each capture loads where the
        // closure is built, so the result is what the elaborator reads.
        let captures = captured
            .iter()
            .map(|&cap_name| {
                let name = self.engine.str(cap_name).to_string();
                let scheme = self.env.lookup(&name).copied();
                let (at, ty) = match scheme {
                    Some(scheme) => (
                        self.name_at(cap_name, scheme.kind).0,
                        self.engine.instantiate(&scheme, &self.frames.current.rigid),
                    ),
                    None => (NameAt::Unbound, self.engine.fresh_var()),
                };
                let fact = NameFact {
                    ty,
                    at,
                    param_labels: None,
                };
                (cap_name, fact)
            })
            .collect();
        ClosureFact { func_idx, captures }
    }

    // ========================================================================
    // Pattern matching
    // ========================================================================

    fn compile_match(&mut self, m: &ast::MatchExpression) -> (Ty, CKind) {
        let subject = self.compile_expr(&m.subject);
        let subject_ty = subject.ty;
        let result_ty = self.engine.fresh_var();
        let mut any_pattern_err = false;
        let mut b = PatternBindings::new();
        let mut arms = Vec::with_capacity(m.arms.len());

        for arm in &m.arms {
            self.push_block_scope();

            b.clear();
            let (pattern, ok) =
                self.type_pattern(&arm.pattern, subject_ty, &mut b.sink(), &mut Vec::new());
            any_pattern_err |= !ok;

            let guard = arm.guard.as_ref().map(|guard| {
                let g = self.compile_expr(guard);
                let bool_ty = self.ty_bool();
                self.engine
                    .unify_at(bool_ty, g.ty, type_defining_span(guard));
                g
            });

            let body = self.compile_expr(&arm.body);
            self.engine
                .unify_at(result_ty, body.ty, type_defining_span(&arm.body));

            self.pop_block_scope();
            arms.push(CArm {
                pattern,
                guard,
                body,
            });
        }
        if !any_pattern_err {
            let depth = arms
                .iter()
                .map(|arm| unroll_depth(&arm.pattern))
                .max()
                .unwrap_or(0)
                + 1;
            let resolved_subj =
                self.engine
                    .resolve_for_patterns(subject_ty, Some(&self.env), depth);
            let mut um = UsefulnessMatrix::new(resolved_subj);
            let all_pats: Vec<Pat> = arms.iter().map(|arm| um.lower(&arm.pattern)).collect();
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
                } else if let Some(missing) = um.catch_all(&all_pats[..i], &all_pats[i]) {
                    // A catch-all may only stand for values that cannot be
                    // listed. Over named variants it would silently take any
                    // variant added later, so the arms have to name them.
                    self.error(
                        format!(
                            "This pattern stands for {}. Name {} instead, like `{} ->`",
                            missing.join(", "),
                            if missing.len() == 1 { "it" } else { "them" },
                            missing.join(" | ")
                        ),
                        arm.pattern.span(),
                    );
                }
                if arm.guard.is_none() {
                    um.push(&all_pats[i]);
                }
            }
        }

        let kind = CKind::Match {
            subject: Box::new(subject),
            arms,
        };
        (result_ty, kind)
    }

    /// Single source of truth turning a numeric literal's source text into a
    /// [`Const`]. On i64 overflow / malformed input it emits a real diagnostic
    /// via [`Compiler::error`] and then returns a kind-preserving recovery
    /// zero. The recovery value is reachable ONLY on the post-diagnostic error
    /// branch, so it can never masquerade as a valid literal `0`: the compile
    /// has already failed.
    fn const_number(&mut self, n: &ast::NumberLiteral) -> Const {
        match number_literal_value(n) {
            Ok(c) => c,
            Err(e) => {
                self.error(e.message(&n.value), n.span);
                e.recovery()
            }
        }
    }
}

// ============================================================================
// Free helpers
// ============================================================================

/// The span that "defines" the type of an expression for error reporting:
/// for a block, the last node (recursively); otherwise the expression's own
/// span. This focuses a return-type or branch-type mismatch on the actual
/// value-producing sub-expression rather than the whole `{ ... }`.
fn type_defining_span(expr: &ast::Expression) -> Span {
    match expr {
        ast::Expression::BlockExpression(b) => match b.body.last() {
            Some(ast::Node::Expression(e)) => type_defining_span(e),
            Some(n) => n.span(),
            None => b.span,
        },
        _ => expr.span(),
    }
}

/// Why a numeric literal's source text could not be turned into a [`Const`].
///
/// The scanner produces decimal (`[0-9]+(\.[0-9]+)?`), hex (`0[xX][0-9A-Fa-f]+`),
/// or binary (`0[bB][01]+`), and the parser only prepends a leading `-` for
/// negative pattern literals. The integer branch fails on i64 overflow (hex
/// and binary are magnitudes, so `0x8000000000000000` is out of range, not
/// i64::MIN); the float branch effectively never fails for scanner output,
/// but `InvalidFloat` is kept so the function is total over every `&str`
/// rather than silently depending on that lexical invariant.
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

    /// Constant to substitute so compilation can continue after the
    /// diagnostic has been emitted. Kind-preserving so the inferred type still
    /// matches user intent and the compile doesn't cascade into spurious type
    /// errors.
    fn recovery(&self) -> Const {
        match self {
            NumLitError::IntOutOfRange => Const::Int(0),
            NumLitError::InvalidFloat => Const::Float(0.0),
        }
    }
}

/// Parse a numeric literal's source text into a [`Const`].
///
/// Total: the partiality is lifted into the return type so no caller can
/// obtain a fabricated constant for out-of-domain input. The only caller is
/// [`Compiler::const_number`], which emits a diagnostic before recovering.
fn number_literal_value(n: &ast::NumberLiteral) -> Result<Const, NumLitError> {
    let s = n.digits();
    if s.contains('.') {
        s.parse()
            .map(Const::Float)
            .map_err(|_| NumLitError::InvalidFloat)
    } else {
        n.as_int().map(Const::Int).ok_or(NumLitError::IntOutOfRange)
    }
}
