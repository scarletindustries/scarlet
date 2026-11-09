//! Turns the AST into core IR. It drives Hindley–Milner type inference,
//! elaboration to the typed IR, lowering to core IR and Perceus. The output is
//! a [`Program`], which holds every function body, every module's top level,
//! and `main`.
//!
//! Function bodies go through the Core IR pipeline. First the fused
//! [`ModuleChecker::compile_expr`] walk runs type inference. A type that
//! unification just solved is available straight away, so elaboration can pick
//! typed operations (`IntAdd` instead of `Add`) and pattern lowering can look
//! at variant layouts. Then the checked body is elaborated, lowered to
//! [`crate::core_ir`] ANF, and run through the Core→Core passes (Perceus reuse,
//! and later mode inference). See [`ModuleChecker::compile_fn_body`] and
//! `docs/core-ir-spec.md`.
//!
//! A module's top level is the exception. Its declarations can refer to each
//! other in any order, so `analysis.rs` runs its declaration analysis in
//! several passes first and hands each body back to this file.
//!
//! # Map of the module
//!
//! - **This file** has the [`Compiler`] and the entry points ([`compile`],
//!   [`check`] and [`check_as_module`]). It holds the scoping and inference
//!   state in one struct, documented field by field. It also has the pass
//!   itself (`compile_node`, `compile_expr` and the rest, one method per AST
//!   form) and drives the Core IR pipeline
//!   ([`ModuleChecker::compile_fn_body`]).
//! - **`patterns.rs`** type-checks patterns: [`ModuleChecker::type_pattern`],
//!   plus looking up constructors and putting their arguments in slots.
//! - **`bridges.rs`** has the [`ElabCtx`] impl the elaborator uses to talk to
//!   this compilation.
//! - **`tests.rs`** has the unit tests.
//!
//! The LSP and workspace layer that keeps a `Compiler` across edits lives in
//! [`super::session`]. That is [`IncrementalSession`], [`Watermark`] and
//! `reset_to`, and building the reference graph at the end.
//!
//! # Invariants
//!
//! - Everything a compile adds to (the inference pool, the type env, the
//!   function table and the constants) only grows between module boundaries.
//!   That is why rolling back to a `Watermark` is just truncation.
//! - Each body being walked has a frame, and each block in it has a map of its
//!   own names (`frame.rs`). Closing a block drops its map.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use super::session::{RawRef, Watermark};
use super::{PreludeBindings, TypeRef};
use crate::ast;
use crate::core_ir::{
    Abi, Const, ConstId, CoreFn, FuncIdx, HttpTypes, IoErrors, JsonTypes, LoweredFn, ParseReasons,
    Program, Radix, TypeNames, VariantNames, VariantRef,
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

/// The proof the elaborator asks for before it will run. It has its own module
/// so nothing else in this module can forge the private field.
mod clean {
    use crate::diagnostic::{Diagnostic, has_errors};

    /// Proof that, at the moment it was minted, the module being compiled had
    /// produced no error diagnostic.
    ///
    /// After the typecheck walk there is no error node anywhere.
    /// [`TypedExpr`](crate::typed_ir::TypedExpr) has no poison arm, `lower` has
    /// nothing to emit for a subtree the typechecker rejected, and `perceus`
    /// has no type to work out its shape from. Instead of teaching every pass
    /// to recognise poison, we make the whole pipeline *unreachable* from a
    /// poisoned module at its **first** gate. That gate is the elaborator, not
    /// `lower`. `typed_ir::elaborate_body` and `elaborate_toplevel` are the
    /// only things that build a [`TypedFn`](crate::typed_ir::TypedFn), and so
    /// the [`TypedProgram`](crate::typed_ir::TypedProgram) that `lower` takes.
    /// [`ModuleChecker::elaborate_then_materialize`](super::ModuleChecker::elaborate_then_materialize)
    /// is the only place in this compiler that calls them. It takes a
    /// `CleanModule`, and [`CleanModule::mint`] is the only way to make one.
    ///
    /// (`core_ir::lower::lower` itself takes no proof, because it doesn't need
    /// one. Its argument *is* the proof. A `TypedProgram` can't exist for a
    /// module that failed to typecheck, because the only way to build one goes
    /// through the gate above.)
    ///
    /// So a body the walk rejected is never elaborated. It is closed out empty,
    /// which is what the fused pipeline always did. In particular, the
    /// elaborator aborts on a `CKind::Error` node, which is exactly what a
    /// rejected subtree looks like in the checked tree. That abort can't be
    /// reached from a module that already reported a type error, so a plain
    /// type error never turns into a compiler-bug report.
    ///
    /// It is neither `Copy` nor `Clone`, and is taken by value. Being clean
    /// before one elaboration says nothing about the state after it, because an
    /// elaboration can add diagnostics itself. So each one proves it again.
    #[must_use]
    pub(super) struct CleanModule {
        _priv: (),
    }

    impl CleanModule {
        /// `Some` exactly when `diagnostics` has no error. That is the same
        /// test `CompileResult::success` reports, so gating on this proof can't
        /// change which diagnostics the user sees.
        pub(super) fn mint(diagnostics: &[Diagnostic]) -> Option<Self> {
            (!has_errors(diagnostics)).then_some(Self { _priv: () })
        }
    }
}
use clean::CleanModule;

/// Proof that the entry frame's toplevel was spliced into the code, so the
/// user's program really is in the `Program` and not just the stdlib init that
/// comes before it.
///
/// Only [`ModuleChecker::record_toplevel`] mints it, on its `Entry` path, and
/// it is the only way to reach [`CompileResult::into_runnable`]. Without it, a
/// compile that rejected the module could still hand back a program that runs
/// the stdlib and nothing else. That program doesn't fail. It quietly evaluates
/// to whatever the globals were pre-filled with. A caller can't tell that apart
/// from a real result, and one didn't.
#[must_use]
struct EntryToplevel {
    _priv: (),
}

/// Why looking up a `.field` on a nominal type failed. It carries enough of the
/// variant at fault for the typecheck path to write its diagnostics. Lowering
/// just throws it away.
enum FieldMismatch {
    /// The receiver's type has no variants at all (an alias, an opaque type or
    /// a builtin).
    NotNominal,
    /// The variant list is empty, so no field can exist.
    NoVariants,
    /// A variant with no field of that name, so a value of that variant has
    /// nothing to read.
    MissingOn(StrId),
}

/// A field every variant of a type declares, found by its name.
struct SharedField {
    /// Its type, which every variant agrees on.
    ty: Ty,
    /// Where it sits in each variant, by variant index. Variants may disagree,
    /// because a read looks the field up by name, not by position.
    slots: SmallVec<[u32; 4]>,
}

// ============================================================================
// CompileResult
// ============================================================================

#[derive(Debug)]
/// Whether a compilation unit reports the bindings it never uses.
///
/// A file is a whole program, so a binding nothing reads is a mistake worth
/// reporting. A REPL entry is a piece of a session, where the line that uses
/// the name hasn't been typed yet. There, the same check would reject exactly
/// the entries the user meant.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum UnusedBindings {
    #[default]
    Report,
    Ignore,
}

/// What a module's top level may contain.
///
/// A program is made of declarations (`import`, `fn`, `type` and `const`) and
/// starts at `pub fn main()`. Statements at module scope are rejected, in the
/// entry file just as in an imported module. The REPL is the one place where
/// code is entered one statement at a time, and its bindings have to last from
/// one entry to the next, which is what module-scope bindings are. So the REPL
/// compiles in [`Script`](ModuleScope::Script) mode, and nothing else does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ModuleScope {
    #[default]
    DeclarationsOnly,
    Script,
}

pub struct CompileResult {
    /// The program, when the compile built one. It is `None` for a check, for a
    /// module that failed, and for the incremental (LSP) session.
    ///
    /// It is private, and you can only reach it through
    /// [`into_runnable`](Self::into_runnable), which also asks for the
    /// [`EntryToplevel`] witness.
    program: Option<Program>,
    /// Set exactly when the entry toplevel was spliced. See [`EntryToplevel`].
    entry: Option<EntryToplevel>,
    pub diagnostics: Vec<Diagnostic>,
    /// The workspace reference graph, which is the one source of truth for
    /// goto-def, find-references, rename, symbols and dead code. It is an `Rc`
    /// handle, so the `IncrementalSession` that owns it can keep answering
    /// queries on the same graph after handing back the result.
    pub(crate) references: Rc<ReferenceGraph>,
}

impl CompileResult {
    /// Whether the compile succeeded, which by definition means `diagnostics`
    /// has no error. It is worked out, not stored, so no place that builds a
    /// result can disagree with the diagnostics it hands back.
    pub fn success(&self) -> bool {
        !has_errors(&self.diagnostics)
    }

    /// The program to run, and the only way to reach one.
    ///
    /// It is `None` unless the compiler recorded the user's toplevel, which a
    /// check-only compile and a rejected module both skip. The answer comes
    /// from that recording itself ([`EntryToplevel`]), not from reading
    /// `diagnostics` again. So a caller that filters out a diagnostic can't
    /// turn a compile that never lowered into a run.
    pub fn into_runnable(self) -> Option<Program> {
        let _recorded = self.entry?;
        self.program
    }

    /// A compile that built no program. That is a check-only session, or a
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
// Compiler: HM type inference and elaboration in a single pass
// ============================================================================

/// A value the module itself declares, under the module's own name for it: a
/// `fn` or `const`, a constructor of one of its types, or an `@vm` builtin.
#[derive(Clone, Copy, Debug)]
pub(super) struct ModuleValue {
    scheme: Scheme,
    /// Where a `fn` or `const` lives. A constructor or builtin has no runtime
    /// binding: its scheme's kind says what it is.
    decl: Option<DeclSite>,
}

/// Where one of the module's own `fn`/`const` declarations lives.
#[derive(Clone, Copy, Debug)]
pub(super) struct DeclSite {
    /// The entry-frame slot the toplevel stores it in.
    pub(super) slot: GlobalSlot,
    /// Its binder in the module toplevel.
    pub(super) binder: BinderId,
}

/// One top-level `fn` or `const` declaration of the module being compiled, in
/// the form the toplevel elaboration needs.
///
/// Both facts the elaborator needs about a declaration live on this one record.
/// One is where it sits in the module block, so the spine can be scheduled in
/// dependency order. The other is which entry-frame slot the check walk already
/// gave it, so `TypedBind::global` pins the `StoreLocal` that the `PushGlobal`s
/// already emitted into function bodies point at. Nothing maps the *name* back
/// to a slot.
#[derive(Clone, Copy, Debug)]
pub(super) struct ToplevelDecl {
    /// The index of the declaration's node in the module block's `body`.
    pub(super) node: usize,
    /// The declaration's binder in the module toplevel. It is only read to
    /// resolve a forward *reference* to this declaration from inside the
    /// toplevel spine, and the answer is this record's `slot`.
    pub(super) binder: BinderId,
    /// The entry-frame slot that Pass 3 allocated for the declaration.
    pub(super) slot: GlobalSlot,
}

// `bool_cluster` fires on the three bool fields below, and the answer is that
// all 8 states are legal, so there's no illegal combination for an enum to
// name. They are switches on three unrelated subsystems: cutting the pipeline
// short, collecting hover facts, and namespace scoping. Both extremes
// happen. All unset is an ordinary `compile`. All set is an
// `IncrementalSession`, which builds a `check_only` compiler, sets
// `collect_hover_facts`, then calls `register_prelude`, which runs under
// `with_retained_namespaces`. The mode flags in this struct that did carry an
// invariant are already enums (see [`UnusedBindings`] and [`ModuleScope`]), and
// these three carry none between them.
#[cfg_attr(mordant, allow(bool_cluster))]
/// What lives as long as one compile or one editor session. That is the module
/// table, the type store, and everything compiled so far. A module's own state
/// lives on its [`ModuleChecker`] instead.
pub struct Compiler {
    /// Every function this compile reserved, indexed by [`FuncIdx`] and filled
    /// in once its body is lowered. A slot stays `None` under `check_only` and
    /// in a module that failed. [`Self::finish_program`] only builds a
    /// [`Program`] when every slot is filled.
    pub(super) fns: TiVec<FuncIdx, Option<LoweredFn>>,
    /// Each imported module's lowered top level, in the order they finished
    /// compiling, so a module comes after everything it imports.
    pub(super) inits: Vec<LoweredFn>,
    /// The entry file's lowered top level, once recorded.
    pub(super) entry_toplevel: Option<LoweredFn>,
    /// `pub fn main`, when the entry file has a usable one.
    pub(super) main: Option<FuncIdx>,
    /// Every constant this compile pooled, indexed by [`ConstId`]. Once the
    /// program is lowered, the whole list goes to
    /// [`CoreProgram::consts`](crate::core_ir::CoreProgram).
    pub(super) consts: Vec<Const>,
    /// Each pooled constant → its `ConstId`, so `add_constant` returns the
    /// existing id for a repeated literal instead of pushing a copy. A lookup
    /// checks itself (the id is in bounds and still names that constant), so
    /// truncating the pool in `reset_to` doesn't need to clear this too. A
    /// stale entry just misses.
    const_dedup: HashMap<Const, ConstId>,
    /// Set only while the prelude compiles. The by-name env entries the prelude
    /// leaves behind *are* the stdlib namespace every program sees, so its
    /// [`ModuleChecker`] doesn't undo them when it finishes. User modules
    /// always compile with this unset.
    retain_namespaces: bool,
    /// Global slot → [`FuncIdx`] for every top-level `fn` compiled so far.
    /// `compile_declared_function` fills it in after `finish_fn_frame` assigns
    /// the `func_idx`. `global_denotation` reads it to choose between
    /// [`Denotation::known_fn`] and [`Denotation::global`], which lets Core
    /// call a known top-level function directly (`Callee::Known`) instead of
    /// loading it and calling the value. A miss (a forward reference inside an
    /// SCC, or a global that isn't a function) falls back to calling the value.
    pub(super) global_to_func: HashMap<GlobalSlot, crate::core_ir::FuncIdx>,
    /// The memo for [`ElabCtx::resolve_rty`], keyed by union-find root. It
    /// keeps one pool node per solved type for the whole `TypedProgram` that
    /// the current `elaborate` call is building, and is cleared there along
    /// with the pool.
    rty_cache: HashMap<Ty, RTy>,
    pub(super) engine: InferEngine,
    pub(super) env: TypeEnv,
    pub(super) recorded: Vec<RawRef>,
    check_only: bool,
    /// Whether to buffer a `RawRef` for each occurrence and resolve them into
    /// the `HoverFact` table in `finalize_references`. Only
    /// `IncrementalSession` (the LSP) uses `HoverFact`s. The free `compile` and
    /// `check` entry points throw them away, so they leave this `false` and
    /// skip the whole resolve pass, which builds a deep `Type` tree, allocates
    /// a `HashSet` and calls `name.to_string()` for every occurrence. The
    /// reference graph (goto-def, find-references and dead code) is built
    /// separately from `module_refs` and isn't affected.
    pub(super) collect_hover_facts: bool,
    /// Whether closing a block reports the names nothing read. See
    /// [`UnusedBindings`].
    pub(super) unused_bindings: UnusedBindings,
    /// Whether the entry accepts statements at module scope. See
    /// [`ModuleScope`]. Imported modules never accept them.
    pub(super) module_scope: ModuleScope,
    /// The memo for [`Compiler::restricted_generalization_cons`], which holds
    /// the TypeIds whose parameters must not generalise at a value binding.
    /// Stdlib type ids don't change across `reset_to` (the watermark sits past
    /// the static stdlib), so the memo survives a session's rewinds.
    restricted_gen_cons: Option<HashSet<TypeId>>,
    pub(super) module_table: ModuleTable,
    /// The `ModulePath` ↔ `ModuleId` interner behind every `DefId`, which only
    /// grows. Like `module_table`, it lasts across `reset_to` and incremental
    /// recompiles, so a `DefId` minted in one `check` still resolves after
    /// later modules are added or evicted. Ids are reused across recompiles.
    pub(super) ref_interner: ModuleInterner,
    /// Memo of a module-path `ArenaSlice` → its interned `ModuleId`. The
    /// `str_slices` pool only grows during a compile, so an `ArenaSlice` always
    /// names the same path and so the same id. It is cleared in `reset_to`, the
    /// one place the pool is rewound. It lets `defid_of` and `record` skip the
    /// `strs_of` call, the key-join allocations and the string-hash lookup for
    /// every occurrence on each keystroke's recompile.
    pub(super) defid_module_memo: HashMap<ArenaSlice<pool::StrSlices>, ModuleId>,
    /// Canonical module key → the import path as the user wrote it. The
    /// identity is the resolved file, but diagnostics should still say `./lib`,
    /// not `/private/var/.../lib`.
    module_display: HashMap<ModuleKey, String>,
    /// When this is set, `scarlet/...` imports resolve to `.sca` files under
    /// this directory (the `src/std` in this repo) instead of the embedded
    /// copy. So a session that edits the stdlib compiles it like any other
    /// module tree on disk.
    pub(super) stdlib_source_root: Option<PathBuf>,
    pub(super) prelude: PreludeBindings,
    /// Names user code may not redefine. It is filled from the prelude module's
    /// interface (every exported type and constructor), so the set comes from
    /// `scarlet.sca` and isn't copied into Rust. `@vm` functions are left out
    /// on purpose, so you can shadow `println`.
    pub(super) reserved: BTreeSet<String>,
    /// The values every module sees without an import: the prelude's, by
    /// name, in the order the prelude declares them.
    pub(super) prelude_values: IndexMap<StrId, Scheme>,
    /// The entry-frame slots every module's toplevel has handed out so far.
    /// Each module's toplevel frame starts counting here.
    pub(super) slots: i32,
    /// What the entry module's check recorded for the reference graph.
    pub(super) entry_refs: ModuleReferences,
}

/// One module's body while it is being checked. A fresh one is made once the
/// modules it imports are compiled, and it goes away when the module is done,
/// so nothing about one module can leak into the next.
pub struct ModuleChecker<'c> {
    pub(super) c: &'c mut Compiler,
    /// Whether this module's by-name env entries are undone when it is done. It
    /// is false only while the prelude compiles, because every module sees its
    /// names.
    scoped: bool,
    /// The bodies the walk is inside, the current one first.
    frames: frame::Frames,
    /// Pattern binders starting with `_` whose discarded type has to be looked
    /// at again once the module's inference is final. Discarding a value the
    /// type system knows is `Nil` hides nothing, so the explicit `Nil` pattern
    /// is required instead. [`ModuleChecker::check_nil_discards`] drains this.
    nil_discards: Vec<(String, Ty, Span)>,
    /// This module's own top-level `fn` and `const` declarations, in the order
    /// Pass 5 visits SCCs (leaves first). Once the imports are compiled, this
    /// is cleared down to the entry file's scope.
    ///
    /// One record serves both halves of the toplevel elaboration. The
    /// [`ToplevelDecl::node`] index *schedules* the spine. The elaborator walks
    /// the module block's declaration nodes in this order, not source order, so
    /// a `const` referred to before it is declared is stored before it is read.
    /// The [`ToplevelDecl::slot`] on that same record is the entry-frame slot
    /// the declaration's `TypedBind::global` is pinned to. It is also the one a
    /// forward *reference* inside an SCC loads with `PushGlobal`. The
    /// definition and its uses read the same field of the same record, so they
    /// can't disagree.
    pub(super) toplevel_decls: Vec<ToplevelDecl>,
    /// The names the binding being checked has bound at module scope, with
    /// their entry-frame slots. [`ModuleChecker::compile_statement`] moves them
    /// onto that binding's [`CNode::Binding`], so a slot travels with the
    /// binding it belongs to and nothing has to pair them up by order later.
    binding_globals: Vec<(StrId, GlobalSlot)>,
    /// True only while `analyse_module` walks a module's own statement list.
    /// Being a global ([`ModuleChecker::binds_a_global`]) doesn't tell you this
    /// on its own. Imports, the prelude's slot seeds and `fn` and `const`
    /// declarations are all globals bound outside the statement walk, and each
    /// reaches the elaborator by its own route.
    pub(super) walking_module_statements: bool,
    /// A name, and its type, handed to the next `enter_fn_frame(None)` only, so
    /// the lambda in a `name = fn(...)` binding can call itself, without
    /// leaking the enclosing function's binding into unrelated lambdas (such as
    /// one passed as an argument).
    next_fn_self_name: Option<(StrId, Ty)>,
    /// The values the module itself declares, by name, in the order it
    /// declares them.
    names: IndexMap<StrId, ModuleValue>,
    /// The definition whose body is being compiled. It is the `owner` of every
    /// reference-graph occurrence emitted while it is set, which is how
    /// dead-code reachability gets its edges between definitions. It is `None`
    /// at module top level, so code that really runs there roots its
    /// references. It is set and restored around each `fn`, `const` and `type`
    /// body in the single-pass walk, and it covers nested lambdas, because a
    /// lambda has no `DefId` and its body's references belong to the definition
    /// around it.
    pub(super) current_owner: Option<DefId>,
    /// The definitions and references this module records for the reference
    /// graph. [`Self::finish`] hands them over, onto the module's
    /// `CachedModule` or to [`Compiler::entry_refs`] for the entry.
    module_refs: ModuleReferences,
    pub(super) current_module: ModulePath,
    /// The canonical cache key of `current_module`, so the bookkeeping for each
    /// module never works a key out again from a path that may not be resolved.
    current_module_key: ModuleKey,
    /// `current_module`, interned into `engine.str_slices` once.
    pub(super) module_path_slice: Option<ArenaSlice<pool::StrSlices>>,
    /// A qualifier name in this file → the imported module's canonical key.
    imported_qualifiers: HashMap<String, ModuleKey>,
    /// Function bodies whose typecheck walk has finished but whose Core
    /// pipeline (`lower` → `perceus`) waits until the declaration group around
    /// them has been generalised. See [`Self::begin_deferred_elaboration`].
    deferred_bodies: Vec<DeferredBody>,
    /// How deep the open deferral regions are nested. `compile_fn_body` runs
    /// the Core pipeline straight away when this is zero, which is the case for
    /// REPL entries, top-level bindings and bare expressions, because they are
    /// walked outside Pass 5 of `analyse_module`.
    defer_depth: u32,
}

/// One body, elaborated into a whole-module [`TypedProgram`] that the Core
/// pipeline can take.
///
/// The elaborator mints a `FuncIdx` for every eta wrapper it needs by pushing
/// onto its `FnTable`, and `FuncIdx` is also this compiler's index into
/// [`Compiler::fns`] (it is what `Atom::Closure` and `Callee::Known` carry). So
/// `fns` is padded up to `Compiler::fns.len()` before the walk starts, and each
/// wrapper the walk adds past that point gets its slot reserved here, in the
/// same order. `eta_base` is the padding length, so `fns[eta_base..]` are
/// exactly the wrappers.
struct Elaborated {
    program: TypedProgram,
    eta_base: usize,
}

/// One body that has gone all the way through `lower`: the `CoreFn` itself and
/// the arena its `RTy`s index into.
///
/// A module toplevel's global-slot pinnings travel in the IR itself. `lower`
/// copied `TypedBind::global`, which `ElabCtx::global_slot` stamps during
/// elaboration, onto each module-scope `Let`'s `CoreBind`. So the binds can't
/// disagree with the `Load::Global` every function body reads, because there is
/// no side table to fall out of step with them.
struct LoweredBody {
    /// The function's name, from the `TypedFn` it was lowered from.
    name: StrId,
    core: CoreFn,
    pool: Rc<ResolvedPool>,
}

/// A module-scope slot was bound outside any binding statement, so no binding
/// carries it to the elaborator and its global is never stored. This aborts in
/// release builds as well as debug, because only a compiler bug binds one
/// there.
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
/// would call a function that doesn't exist. This aborts, because only a
/// compiler bug can leave one behind.
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

/// Something reserved a function while the elaborator was walking, so the
/// `FuncIdx` that the next `FnTable::push` mints would stop naming it. This
/// aborts in release builds as well as debug, because every eta wrapper
/// reserved after the stray push would quietly call the wrong function.
#[allow(clippy::panic)]
#[cold]
#[inline(never)]
fn function_reserved_during_elaboration() -> ! {
    panic!(
        "internal compiler error: a function was reserved while the elaborator walked. \
         Report this as a compiler bug."
    )
}

/// A function body waiting between its typecheck walk and its elaboration.
///
/// The walk settles everything about the body except its *types*: which
/// function slot it owns, and which names it captured at which indices. Types
/// keep changing until the whole SCC has been inferred and generalised, so
/// `lower`, the pass that reads them, runs last, from this record.
///
/// It is built whole in exactly one place, [`ModuleChecker::finish_fn_frame`],
/// from the walk half ([`ParkedBody`]) plus the frame half that only becomes
/// final there. No field is ever patched in later.
struct DeferredBody {
    name: StrId,
    param_binds: Vec<(StrId, BinderId, Ty)>,
    /// What the check walk built for the body: every node with the type it
    /// inferred, and every name and lambda with what it resolved to.
    body: CExpr,
    body_ty: Ty,
    /// The function slot this body fills in. `finish_fn_frame` reserves it in
    /// *both* modes, because a [`ClosureFact`] and `global_to_func` record it
    /// and `lower` bakes it into `Atom::Closure` and `Callee::Known`, and all
    /// of that happens under `check_only` too.
    func_idx: FuncIdx,
}

/// The walk half of a [`DeferredBody`]. It is everything
/// [`ModuleChecker::compile_fn_body`] knows about the body it just checked,
/// carried by value to the matching [`ModuleChecker::finish_fn_frame`], which
/// adds the frame half (the captures and `func_idx`) and pushes the complete
/// `DeferredBody`.
///
/// There is only one outcome, and that is the point. The typecheck walk can't
/// lower a function body, because `compile_fn_body` has nothing to hand
/// `finish_fn_frame` but this record. The boundary between the phases is a
/// type, not a convention. The deferred body is lowered and fills its function
/// slot in pass 6 of `analyse_module`.
struct ParkedBody {
    name: StrId,
    param_binds: Vec<(StrId, BinderId, Ty)>,
    body: CExpr,
    body_ty: Ty,
}

/// A name resolved to a constructor, in the form `type_ctor_pattern` needs to
/// put the pattern's arguments in slots and type them. `Compiler::lookup_ctor`
/// and `lookup_ctor_qualified` produce it.
struct CtorLookup {
    variant: VariantRef,
    type_name: StrId,
    arity: usize,
    field_labels: ArenaSlice<pool::StrSlices>,
    scheme: Scheme,
}

impl CtorLookup {
    /// The lookup for `scheme` when it names a constructor, or `None` if not.
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

/// Which toplevel [`ModuleChecker::record_toplevel`] is recording, the entry
/// file's or an imported module's.
enum TopKind {
    /// `__main__`, which is the program's own toplevel, and the `main` it
    /// starts at when it has one. A script (the REPL) has no `main`, and it
    /// prints the value its toplevel ends with.
    Entry { entered: Option<FuncIdx> },
    /// An imported module, whose toplevel runs for its effects.
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

/// A callee that resolved to a plain name or a qualified `module::member`. It
/// holds the scheme that was looked up, plus everything `compile_call` needs to
/// dispatch on the scheme's `ValueKind` and write diagnostics about the callee.
struct ResolvedCallee<'a> {
    name: &'a str,
    span: Span,
    scheme: Scheme,
    at: NameAt,
    /// The module a qualified callee resolved through. Diagnostics show it with
    /// `Compiler::module_name`, which is the name the user sees, never the key.
    qualifier: Option<ModuleKey>,
    /// Whether the callee has a runtime binding. Only a qualified member can
    /// lack one: a module member with no entry-frame slot.
    has_binding: bool,
}

struct CompiledBody {
    iface: ModuleInterface,
    watermark: Watermark,
    /// The reference-graph data for this module's body, which `load_module`
    /// moves into its `CachedModule`. The module's type-id range is both
    /// reserved and recorded inside `compile_module_body`, so its start isn't
    /// passed back out here, on purpose.
    refs: ModuleReferences,
}

/// Everything one compile can be asked to do differently. The entry points
/// below name the combinations, so call sites don't spell them out and a new
/// option doesn't mean a new argument on five functions.
#[derive(Default)]
pub struct CompileOptions<'a> {
    /// The directory that relative imports resolve against. `None` rejects
    /// them.
    pub base_dir: Option<&'a Path>,
    /// Analyse without emitting a program.
    pub check_only: bool,
    /// Analyse the buffer *as* this module instead of as `main`.
    pub as_module: Option<ModulePath>,
    /// Whether bindings nothing reads are reported. See [`UnusedBindings`].
    pub unused_bindings: UnusedBindings,
    /// Whether the entry may contain module-scope statements. See
    /// [`ModuleScope`].
    pub module_scope: ModuleScope,
}

impl<'a> CompileOptions<'a> {
    /// Emit a program from `base_dir`, which is what compiling a file means.
    /// Every other entry point below is this with one field changed.
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

/// Analyse a file *as* a specific stdlib module. This is for editing
/// `src/std/**/*.sca` inside the Scarlet repo, so the LSP and CLI don't report
/// that `@vm` is only for the stdlib or that `Result` is reserved. It always
/// only checks.
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

pub(crate) fn new_compiler(check_only: bool) -> Compiler {
    let mut ref_interner = ModuleInterner::new();
    let entry_refs = ModuleReferences::new(ref_interner.intern(&module::main_module()));
    Compiler {
        fns: TiVec::new(),
        inits: Vec::new(),
        entry_toplevel: None,
        main: None,
        consts: Vec::new(),
        const_dedup: HashMap::new(),
        retain_namespaces: false,
        global_to_func: HashMap::new(),
        rty_cache: HashMap::new(),
        engine: new_engine(),
        env: new_env(),
        recorded: vec![],
        check_only,
        collect_hover_facts: false,
        unused_bindings: UnusedBindings::Report,
        module_scope: ModuleScope::default(),
        restricted_gen_cons: None,
        module_table: ModuleTable::new(),
        module_display: HashMap::new(),
        ref_interner,
        defid_module_memo: HashMap::new(),
        stdlib_source_root: None,
        prelude: PreludeBindings::default(),
        reserved: BTreeSet::new(),
        prelude_values: IndexMap::new(),
        slots: 0,
        entry_refs,
    }
}

/// The module doing an import. It says where its relative imports resolve from,
/// and the key the imported module records as a dependent.
pub(crate) struct Importer<'a> {
    pub(crate) dir: Option<&'a Path>,
    pub(crate) key: &'a ModuleKey,
}

impl<'c> ModuleChecker<'c> {
    /// Start checking the module `path`, whose identity is `key`. Every module
    /// it imports must already be compiled.
    pub(crate) fn new(c: &'c mut Compiler, path: ModulePath, key: ModuleKey) -> ModuleChecker<'c> {
        // A module's by-name env entries are its own, so they are undone when
        // it is done. The prelude's are kept on purpose, because they are the
        // namespace every module sees.
        let scoped = !c.retain_namespaces;
        if scoped {
            c.env.push_module_frame();
        }
        let module_refs = ModuleReferences::new(c.ref_interner.intern(&path));
        let frames = frame::Frames::toplevel(c.slots);
        ModuleChecker {
            c,
            frames,
            nil_discards: Vec::new(),
            toplevel_decls: Vec::new(),
            binding_globals: Vec::new(),
            walking_module_statements: false,
            next_fn_self_name: None,
            names: IndexMap::new(),
            current_owner: None,
            module_refs,
            current_module: path,
            current_module_key: key,
            module_path_slice: None,
            imported_qualifiers: HashMap::new(),
            deferred_bodies: Vec::new(),
            defer_depth: 0,
            scoped,
        }
    }

    /// Finish the module, handing back what it recorded for the reference
    /// graph.
    pub(crate) fn finish(self) -> ModuleReferences {
        self.c.slots = self.frames.globals;
        if self.scoped {
            self.c.env.pop_module_frame();
        }
        self.module_refs
    }
}

impl ModuleChecker<'_> {
    /// The `DefId` of the `Type` named `name` declared in module `path`. It is
    /// read from the reference graph's own definitions, which are the one
    /// source of truth.
    ///
    /// It must *not* go through the flat `env.definitions`. Every record
    /// `type Foo {..}` registers a `Constructor` with the same name, which
    /// overwrites the earlier `Foo => Type` entry (the `IndexMap` keeps the
    /// last write). So once the module that defines it is fully analysed, that
    /// lookup gives the constructor and no type occurrence is ever recorded.
    /// The graph keeps `Type` and `Constructor` as separate `DefId`s under the
    /// same name, so `defs_named` finds the type without any doubt.
    ///
    /// A lookup in the same module reads the live collector, because the type
    /// def is emitted when its declaration is visited, before any annotation
    /// that names it is hydrated. A type from another module reads the imported
    /// module's stored `module_refs`. It is `None` for static or hydrated
    /// stdlib modules, whose type declaration spans are not stored. That is a
    /// known limit: goto-def on a stdlib type is outside what this path
    /// handles.
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
            pick(self.c.module_table.module_refs_by_path(path)?, name)
        }
    }
}

/// A constructor pattern as written, like `NotFound(path)`, or
/// `io::NotFound(..)` reached through a module qualifier.
struct CtorPattern<'a> {
    qualifier: Option<&'a ast::Identifier>,
    name: &'a ast::Identifier,
    args: &'a [ast::PatternArg],
    /// It ends in `..`, so the fields it leaves out match anything.
    rest: bool,
    span: Span,
}

/// What the check walk built for an entry, which is a module block's nodes or a
/// bare expression.
pub(crate) enum CheckedEntry {
    Module(Vec<CNode>, Span),
    Expr(CExpr),
}

/// Every compile runs through here. Each entry point above is a named
/// [`CompileOptions`].
pub fn compile_with(expr: &ast::Expression, options: CompileOptions<'_>) -> CompileResult {
    let CompileOptions {
        base_dir,
        check_only,
        as_module,
        unused_bindings,
        module_scope,
    } = options;

    // Remove the backpass sugar here, before any typing pass, so inference and
    // lowering never see a `Statement::Backpass`.
    let mut expr = expr.clone();
    crate::desugar::desugar_expression(&mut expr);
    let expr = &expr;

    let mut c = new_compiler(check_only);
    c.unused_bindings = unused_bindings;
    c.module_scope = module_scope;
    let (entry_path, entry_key) = match as_module.clone() {
        Some(m) => {
            // The user is editing stdlib source, so the `scarlet/...` imports
            // next to it must resolve to the files on disk, not the stale
            // embedded copy.
            c.stdlib_source_root = base_dir.and_then(module::find_stdlib_root);
            // `as_module` is always a stdlib path (see `check_as_module`), and
            // its written form is its canonical identity.
            let key = ModuleKey::for_stdlib(&m);
            (m, key)
        }
        None => (module::main_module(), ModuleKey::main()),
    };

    // When analysing the prelude file itself, don't load the prelude on top of
    // it, because that would make every type a redefinition of itself. Other
    // stdlib modules still need the prelude for `Result`, `Nil` and the rest.
    let is_prelude_self = as_module.as_deref() == Some(module::scarlet_prelude().as_slice());
    if !is_prelude_self {
        c.register_prelude();
        if has_errors(&c.engine.diagnostics) {
            // The prelude itself failed. The user's program was never compiled,
            // so there is nothing to hand back, not even part of one.
            return CompileResult::analysis_only(
                c.engine.diagnostics,
                Rc::new(ReferenceGraph::new()),
            );
        }
    }

    let importer = Importer {
        dir: base_dir,
        key: &entry_key,
    };
    let loaded = c.load_entry_imports(expr, &importer);
    let mut m = ModuleChecker::new(&mut c, entry_path, entry_key.clone());
    let (checked, top_ty) = m.check_entry(expr, loaded);
    // The `main` a program starts at. It is `None` for a check, a script, a
    // module, or a program with no usable `main` (which then isn't clean).
    let entered = match (expr, &as_module) {
        (ast::Expression::BlockExpression(block), None)
            if module_scope == ModuleScope::DeclarationsOnly =>
        {
            m.program_entry(block, check_only, expr.span())
        }
        _ => None,
    };

    // Elaborate the module toplevel and lower it into Core, like every other
    // function body (docs/core-ir-spec.md §Pipeline step 3). The elaborator
    // handles top-level `fn`, `const`, `type` and `import` declarations, so it
    // covers real modules. Function bodies were already lowered during
    // `analyse_module`, and they read the declarations next to them with
    // `Load::Global(slot)`, where `slot` is what Pass 3 allocated for
    // declarations first. `ElabCtx::global_slot` stamped the same slot onto
    // each `TypedBind`, and `lower` copied it onto each toplevel `Let`'s
    // `CoreBind`, so the definition and its uses agree.
    //
    // Elaboration and lowering run under `check_only` too. If the front half
    // can't handle a well-typed program, `scarlet check` has to report it, not
    // leave it to blow up at `scarlet run`.
    //
    // This stays `None` unless the recording below really happens. That is what
    // stops a rejected or check-only compile from handing back a `Program` that
    // runs the stdlib init and nothing else.
    // Stays `None` unless the recording below actually happens, which is what
    // keeps a rejected or check-only compile from handing back a `Program`
    // that runs the stdlib init and nothing else.
    let mut entry = None;
    if let Some(clean) = m.c.clean_module() {
        let name = m.c.engine.intern("__main__");
        let lowered = m.elaborate_then_materialize(clean, None, |m, pool, fns| match &checked {
            CheckedEntry::Module(nodes, span) => {
                typed_ir::elaborate_toplevel(m, pool, fns, name, nodes, *span, top_ty)
            }
            CheckedEntry::Expr(body) => {
                typed_ir::elaborate_body(m, pool, fns, name, &[], body, top_ty, true)
            }
        });
        if !check_only {
            entry = m.record_toplevel(lowered, TopKind::Entry { entered });
        }
    }
    c.entry_refs = m.finish();

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
    /// Compile what the entry file imports. An entry that is a bare expression
    /// imports nothing.
    pub(crate) fn load_entry_imports(
        &mut self,
        expr: &ast::Expression,
        importer: &Importer<'_>,
    ) -> Vec<Option<(ModulePath, ModuleKey)>> {
        let ast::Expression::BlockExpression(block) = expr else {
            return Vec::new();
        };
        let loaded = self.load_imports(block, importer);
        self.bump_type_ids_past_reserved();
        loaded
    }
}

impl ModuleChecker<'_> {
    /// Check the entry file, which is a module's statements or a bare
    /// expression. `loaded` is what [`Compiler::load_entry_imports`] compiled
    /// for it.
    pub(crate) fn check_entry(
        &mut self,
        expr: &ast::Expression,
        loaded: Vec<Option<(ModulePath, ModuleKey)>>,
    ) -> (CheckedEntry, Ty) {
        if let ast::Expression::BlockExpression(block) = expr {
            self.bind_imports(block, loaded);
            let nodes = self.analyse_module(block, None);
            return (CheckedEntry::Module(nodes, block.span), self.c.ty_nil());
        }
        // Open the block `analyse_module` opens around a module's statements,
        // so the expression's names are reported when nothing reads them.
        self.open_block();
        // Put the same phase boundary around this that `analyse_module` puts
        // around its walk. A program that is a bare expression can still
        // contain lambdas, and none of them may be lowered while the expression
        // is being checked.
        self.begin_deferred_elaboration();
        let body = self.compile_expr(expr);
        self.end_deferred_elaboration();
        self.check_nil_discards();
        self.close_block();
        let ty = body.ty;
        (CheckedEntry::Expr(body), ty)
    }
}

impl Compiler {
    pub(super) fn is_reserved(&self, name: &str) -> bool {
        self.reserved.contains(name)
    }

    /// The type constructors whose parameters must stay monomorphic when a
    /// binding's type is generalised. This is the relaxed value restriction. A
    /// `Subject`'s argument names what a live mailbox carries, so quantifying
    /// it would let one queue be sent one type and received as another. We find
    /// these by module identity, never by name alone, so a user type called
    /// `Subject` isn't affected. A module that can't be resolved gives an empty
    /// set, which is sound, because no value of the type can occur in a program
    /// that never loaded it.
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
                // Only memoise a hit. A module we can't resolve now (one this
                // compile hasn't loaded yet) may load later in this session.
                self.restricted_gen_cons = Some(set.clone());
                set
            }
            None => HashSet::new(),
        }
    }

    /// Assemble the [`Program`] once the entry toplevel is recorded. By then
    /// every reserved function slot is filled. An empty one is a function that
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
            globals: self.slots as u32,
            types,
            abi,
        })
    }

    /// The constructors of `scarlet/binary.Radix`, by name, when the program
    /// loaded that module. If a constructor is renamed this is `None`, and the
    /// built-ins that read one stop instead of reading the wrong base.
    fn radix(&self) -> Option<Radix> {
        let variant = self.stdlib_variants(&["scarlet", "binary"], "Radix")?;
        Some(Radix {
            dec: variant("Dec")?,
            hex: variant("Hex")?,
        })
    }

    /// The constructors of `scarlet/io.IoError`, by name, like [`Self::radix`].
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

    /// The constructors of `Doc`, `ParseError` and `Json` in `scarlet/json`, by
    /// name, like [`Self::radix`].
    fn json_types(&self) -> Option<JsonTypes> {
        let module = ["scarlet", "json"];
        let doc = self.stdlib_variants(&module, "Doc")?;
        let parse_error = self.stdlib_variants(&module, "ParseError")?;
        let reason = self.stdlib_variants(&module, "ParseReason")?;
        let json = self.stdlib_variants(&module, "Json")?;
        Some(JsonTypes {
            doc: doc("Doc")?,
            parse_error: parse_error("ParseError")?,
            reasons: ParseReasons {
                unexpected_end: reason("UnexpectedEnd")?,
                unexpected_byte: reason("UnexpectedByte")?,
                trailing_data: reason("TrailingData")?,
                unterminated_string: reason("UnterminatedString")?,
                bad_escape: reason("BadEscape")?,
                bad_unicode: reason("BadUnicode")?,
                bad_number: reason("BadNumber")?,
                not_utf8: reason("NotUtf8")?,
                not_whole_bytes: reason("NotWholeBytes")?,
                too_large: reason("TooLarge")?,
            },
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

    /// The constructors in `scarlet/http/h1` and `scarlet/http/headers`, by
    /// name, like [`Self::radix`].
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

    /// The names of every type that a constructor in `bodies` or in `abi`
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

    /// Pool `c`, reusing an existing entry if the same constant is already
    /// there.
    ///
    /// The `const_dedup` memo survives `IncrementalSession::reset_to`, which
    /// truncates `consts` without clearing it. So every hit is checked again
    /// against the live pool. A stale entry either falls out of range or names
    /// a different constant, and then `c` is pooled again.
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
}

impl ModuleChecker<'_> {
    /// Bind `name` in the current block, to a value of `scheme` bound at
    /// `span`: the one way a body gets a local. The binding is made whole, so
    /// uses walked before this, and the binding's own init, still mean
    /// whatever the name meant before.
    ///
    /// A binding the module's own statement walk makes at the module's own
    /// depth also takes an entry-frame slot, queued for the toplevel
    /// elaboration on [`Self::binding_globals`]. A module's `fn`/`const`
    /// declarations are not locals: [`Self::declare`] has them.
    fn bind_local(
        &mut self,
        name: &str,
        ty: Ty,
        scheme: Scheme,
        span: Span,
        doc: Option<String>,
    ) -> BinderId {
        let id = self.c.engine.intern(name);
        let global = (self.walking_module_statements && self.binds_a_global()).then(|| {
            let slot = self.frames.alloc_global();
            self.binding_globals.push((id, slot));
            slot
        });
        let m = self.current_module_slice();
        let def = DefinitionLocation::new(span, m, EntityKind::Value);
        let binder = self.frames.current.mint_binder();
        let scheme = Scheme {
            def: Some(def),
            ..scheme
        };
        self.frames.current.insert(
            id,
            frame::Local {
                scheme,
                binder,
                global,
            },
        );
        self.track_binding(name, span);
        if let Some(doc) = &doc {
            self.c.env.store_doc(name, doc.clone());
        }
        self.record(name, ty, span, doc.clone());
        self.emit_value_def(span, name, doc);
        binder
    }

    /// [`Self::bind_local`] for a binding that is not generalized: a
    /// parameter, a pattern name, an `or` receiver.
    fn bind_mono(&mut self, name: &str, ty: Ty, span: Span) -> BinderId {
        self.bind_local(name, ty, mono(ty), span, None)
    }

    /// Whether a binding made *now* is in the module's own statements. That
    /// means no function frame is open, and the depth is at most the one block
    /// `analyse_module` opens around them. Every nested block, `if` or `match`
    /// at module level opens another.
    fn binds_a_global(&self) -> bool {
        self.frames.at_toplevel() && self.frames.current.depth() <= 1
    }

    /// Give one of the module's own `fn`/`const` declarations its entry-frame
    /// slot and its binder in the toplevel. Its scheme comes later, from
    /// [`Self::declare`].
    pub(super) fn alloc_decl(&mut self) -> DeclSite {
        DeclSite {
            slot: self.frames.alloc_global(),
            binder: self.frames.current.mint_binder(),
        }
    }

    /// Name a value the module itself declares, or give it its final scheme.
    pub(super) fn declare(&mut self, name: &str, scheme: Scheme, decl: Option<DeclSite>) {
        let id = self.c.engine.intern(name);
        self.names.insert(id, ModuleValue { scheme, decl });
    }

    /// Open a block. Must be matched by [`Self::close_block`].
    pub(super) fn open_block(&mut self) {
        self.frames.current.open_block();
    }

    /// Close the innermost block, reporting the names in it nothing read.
    pub(super) fn close_block(&mut self) {
        if let Some(block) = self.frames.current.close_block() {
            self.report_unused(block);
        }
    }

    /// Record a binding, parameter or match binding for the unused-variable
    /// check. Names starting with `_` are exempt. If the same name was already
    /// tracked in this block and never used, report that earlier binding now,
    /// because it is shadowed and nothing can refer to it any more.
    fn track_binding(&mut self, name: &str, sp: Span) {
        if name.starts_with('_') {
            return;
        }
        let id = self.c.engine.intern(name);
        if let Some(prev_sp) = self.frames.current.track(id, sp) {
            self.c.unused_binding(name, prev_sp);
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
            let name = self.c.engine.str(id).to_string();
            self.c.unused_binding(&name, sp);
        }
    }

    /// What the value name `name` means here: its scheme, and where it lives
    /// at runtime. The current body's locals come first, then its own name,
    /// then the bodies around it, then the module's own values, then the
    /// prelude's. A name read from a body around this one is captured on the
    /// way, unless it is a module toplevel binding, which is loaded from its
    /// entry-frame slot when the read runs.
    fn resolve_name(&mut self, name: StrId) -> Option<(Scheme, NameAt)> {
        self.frames.mark_used(name);
        let frame = &self.frames.current;
        if let Some(local) = frame.lookup(name) {
            return Some((local.scheme, NameAt::Local(local.binder)));
        }
        if let Some(idx) = frame.captures.get(name) {
            let (_, local) = self.frames.outer_local(name)?;
            return Some((local.scheme, NameAt::Den(Denotation::capture(idx))));
        }
        match frame.self_name {
            Some(frame::SelfName::Lambda(n, ty)) if n == name => {
                return Some((mono(ty), NameAt::Den(Denotation::self_closure())));
            }
            // A top-level fn calls itself through its entry-frame slot, so a
            // value load reads the slot; `PushSelf` would read the sentinel
            // `captures` a `CallKnown` frame carries.
            Some(frame::SelfName::Decl(n)) if n == name => {
                let v = self.names.get(&name)?;
                let at = match v.decl {
                    Some(d) => Denotation::self_toplevel_fn(d.slot),
                    None => Denotation::self_closure(),
                };
                return Some((v.scheme, NameAt::Den(at)));
            }
            _ => {}
        }
        if let Some((depth, local)) = self.frames.outer_local(name) {
            let at = match local.global {
                Some(slot) if depth == 0 => self.global_denotation(slot),
                _ => Denotation::capture(self.frames.current.captures.add(name)),
            };
            return Some((local.scheme, NameAt::Den(at)));
        }
        if let Some(v) = self.names.get(&name) {
            let at = match v.decl {
                Some(d) if self.frames.at_toplevel() => NameAt::Local(d.binder),
                Some(d) => NameAt::Den(self.global_denotation(d.slot)),
                None => NameAt::Den(Denotation::from_kind(v.scheme.kind)?),
            };
            return Some((v.scheme, at));
        }
        let scheme = *self.c.prelude_values.get(&name)?;
        Some((scheme, NameAt::Den(Denotation::from_kind(scheme.kind)?)))
    }

    /// The scheme `name` has here, read without resolving where it lives: no
    /// capture, no use recorded.
    fn scheme_of(&self, name: StrId) -> Option<Scheme> {
        if let Some(local) = self.frames.current.lookup(name) {
            return Some(local.scheme);
        }
        if let Some((_, local)) = self.frames.outer_local(name) {
            return Some(local.scheme);
        }
        self.names
            .get(&name)
            .map(|v| v.scheme)
            .or_else(|| self.c.prelude_values.get(&name).copied())
    }

    /// The visible value name closest to `name`, for "did you mean".
    fn suggest_name(&self, name: &str) -> Option<String> {
        let engine = &self.c.engine;
        let candidates = self
            .c
            .prelude_values
            .keys()
            .chain(self.names.keys())
            .copied()
            .chain(self.frames.names())
            .map(|id| engine.str(id));
        crate::types::closest_name(name, candidates).map(str::to_string)
    }

    /// A module-scope value at `slot`. It is a known top-level `fn` if one has
    /// been registered for that slot, and an opaque global if not.
    fn global_denotation(&self, slot: GlobalSlot) -> Denotation {
        match self.c.global_to_func.get(&slot) {
            Some(&func_idx) => Denotation::known_fn(slot, func_idx),
            None => Denotation::global(slot),
        }
    }

    /// What one of *this module's* top-level declarations means, read off the
    /// same [`ToplevelDecl`] record whose `slot` the elaborated definition is
    /// pinned to. This scans the module's own declarations in a line, but there
    /// are only a handful and it runs once per forward reference.
    fn decl_denotation(&self, binder: BinderId) -> Option<Denotation> {
        let decl = self.toplevel_decls.iter().find(|d| d.binder == binder)?;
        Some(self.global_denotation(decl.slot))
    }
}

impl Compiler {
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

    /// Mint the proof the elaborator asks for, if the module has earned it.
    /// `None` means some subtree is poisoned and nothing may be elaborated. The
    /// user has already been told why, and a body the checker rejected is
    /// closed out empty instead of being handed to `typed_ir::elaborate_body`.
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
            format!("'{name}' is never used. Start its name with '_' if that's on purpose"),
        ));
    }
}

impl ModuleChecker<'_> {
    /// Look up `name` in the doc map, but only while collecting hover facts.
    /// Anywhere else `record` throws the doc away, so skip the clone.
    fn doc_if_collecting(&self, name: &str) -> Option<String> {
        if self.c.collect_hover_facts {
            self.c.env.lookup_doc(name)
        } else {
            None
        }
    }

    pub(super) fn record(&mut self, name: &str, ty: Ty, sp: Span, doc: Option<String>) {
        // Only `IncrementalSession` (the LSP) uses hover facts. On the free
        // `compile` and `check` path they would be resolved and then thrown
        // away, so skip the `name.to_string()` and the `RawRef` buffering
        // there.
        if !self.c.collect_hover_facts {
            return;
        }
        let module = self.current_module_id();
        self.c.recorded.push(RawRef {
            span: sp,
            name: name.to_string(),
            ty,
            doc,
            module,
        });
    }
}

impl Compiler {
    /// Resolve a module-path `ArenaSlice` to its stable `ModuleId`, memoised on
    /// the `Copy` slice. The `str_slices` pool only grows during a compile, so
    /// a slice always names the same path and so the same id. The memo is
    /// dropped in `reset_to`, the one place the pool is rewound. On a cache
    /// hit, this replaces a `strs_of` call (a `Vec<String>` and a `String` per
    /// segment) and a `ref_interner.intern` call (a joined `String` from
    /// `ModuleKey::of` and a string-hash lookup) for every occurrence with one
    /// lookup keyed on the `ArenaSlice`.
    fn module_id_of_slice(&mut self, sl: ArenaSlice<pool::StrSlices>) -> ModuleId {
        if let Some(&id) = self.defid_module_memo.get(&sl) {
            return id;
        }
        let path = self.engine.strs_of(sl);
        let id = self.ref_interner.intern(&path);
        self.defid_module_memo.insert(sl, id);
        id
    }
}

impl ModuleChecker<'_> {
    /// The interned `ModuleId` of `current_module`. It reuses the memoised
    /// module-path slice, so the busy recording path neither clones the
    /// `ModulePath` nor interns it again.
    fn current_module_id(&mut self) -> ModuleId {
        let sl = self.current_module_slice();
        self.c.module_id_of_slice(sl)
    }

    /// Resolve a [`DefinitionLocation`] from the env (declaring span, module
    /// `ArenaSlice` and [`EntityKind`]) to a stable reference-graph [`DefId`].
    /// The module slice is interned again through the long-lived `ref_interner`
    /// so the id survives incremental recompiles. The declaring span is rebuilt
    /// as a single line, so a definition's `DefId` is identical, bit for bit,
    /// to the `DefId` that every occurrence of it targets.
    pub(super) fn defid_of(&mut self, dl: DefinitionLocation) -> DefId {
        let module = self.c.module_id_of_slice(dl.module);
        DefId::new(module, dl.span(), dl.entity)
    }

    /// The [`DefId`] to keep in `current_owner` while a top-level `fn`, `const`
    /// or `type` body is compiled. Every reference emitted inside that body,
    /// including inside nested lambdas (which have no `DefId` of their own),
    /// belongs to this owner. These are the edges between definitions that the
    /// dead-code reachability walk follows.
    pub(super) fn owner_defid(&mut self, name_span: Span, entity: EntityKind) -> DefId {
        let module = self.current_module_slice();
        self.defid_of(DefinitionLocation::new(name_span, module, entity))
    }

    /// Record an occurrence of a resolved name in the current module's
    /// collector. It belongs to the definition whose body is being compiled
    /// (`current_owner`, which is `None` for top-level code that really runs).
    /// The owner gives the dead-code reachability walk its edges between
    /// definitions. Without it, every reference in a `fn`, `const` or `type`
    /// body would look like top-level code that runs, so anything it names
    /// would count as a live root and no private definition that is only dead
    /// through others would ever be reported. This only records. It never
    /// touches inference, schemes, slots or diagnostics.
    fn record_ref(&mut self, occ: Span, kind: ReferenceKind, target: DefId) {
        self.module_refs
            .add_reference(self.current_owner, Reference::new(occ, kind, target));
    }

    /// Resolve a value name's canonical [`DefinitionLocation`] (carried on its
    /// `Scheme.def`) to a [`DefId`], and record an occurrence of `kind` at
    /// `occ`. This does nothing when the name has no recorded definition.
    /// Builtins act as outside targets, which is harmless: they have no
    /// definition in the graph, so the edge dangles and never affects
    /// reachability.
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

    /// Record a `Qualifier` occurrence for the module alias that a qualified
    /// member use was reached through. The qualifier names the import, not a
    /// value, so the occurrence targets the `ModuleAlias` binding, and hover
    /// and goto-def on the `b` of `b.add(..)` reach module `b`. It isn't a use
    /// site, so find-references, rename and the unused-import check aren't
    /// affected. Whether the alias counts as used depends on the member's own
    /// `Qualified` occurrence, which the caller records. `bind_import`
    /// registered the alias `Definition` under the qualifier's name, so finding
    /// it needs no extra bookkeeping. An unknown qualifier records nothing.
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
    /// definition, and record an occurrence of `kind` at `occ`. This does
    /// nothing when the module has no such type (for example static or hydrated
    /// stdlib modules). It is the type version of [`Self::record_value_use`].
    fn record_type_use(&mut self, path: &ModulePath, name: &str, occ: Span, kind: ReferenceKind) {
        if let Some(target) = self.type_defid_in_module(path, name) {
            self.record_ref(occ, kind, target);
        }
    }

    /// Record a declaration in the current module's reference collector, plus
    /// one `Definition` occurrence at the declaring name so find-references
    /// includes the declaration site. The last write of a `Definition` record
    /// wins for its `DefId`, because a definition may be emitted again as more
    /// is learned about it (its doc and visibility settle on the last write).
    /// The occurrence is only emitted the first time, so the declaration isn't
    /// counted twice. That occurrence is owned by the definition itself (not
    /// `None`), so it is never mistaken for top-level code that runs. An edge
    /// from a definition to itself can never make that definition reachable, so
    /// a dead definition stays dead. This only records. It never touches
    /// inference, schemes, slots or diagnostics.
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

    /// Register a local binder as an `EntityKind::Value` definition in the
    /// graph, so goto-def, find-references and hover work on it and its uses.
    /// Only the LSP needs this. The graph is built on every compile, but
    /// `scarlet run` and `scarlet check` never ask about locals, and the
    /// dead-code pass ignores `EntityKind::Value` definitions, so the
    /// `collect_hover_facts` check keeps this off the busy path.
    fn emit_value_def(&mut self, sp: Span, name: &str, doc: Option<String>) {
        if self.c.collect_hover_facts {
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
}

impl Compiler {
    /// Small wrappers over the engine's `mk_con` for prelude types. They use
    /// the captured `PreludeBindings`, so the type's identity (not just its
    /// name) is what counts where it matters. The nullary ones go through the
    /// engine's cache for each primitive, so a recompile mints one `Int` node
    /// instead of one per literal.
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
}

impl ModuleChecker<'_> {
    /// Hydrate a type annotation through `h`. If that fails, record the error
    /// and fall back to a fresh type variable so inference can carry on. Every
    /// type name the hydrator resolved is drained as an `Unqualified`
    /// occurrence of that type's declaration, whether hydration succeeded or
    /// not, because a nested name can resolve before a later one fails. The
    /// target is the canonical `Type` definition in the module that owns it,
    /// resolved through the reference graph and not through `env.definitions`
    /// (where constructors overwrite types). So an occurrence and its
    /// declaration share one `DefId`. This only records, and the `Ty` it
    /// returns is exactly what it would be without the recording.
    pub(super) fn hydrate(&mut self, h: &mut Hydrator, t: &ast::TypeIdentifier) -> Ty {
        let result = h.type_from_ast(t, &self.c.env, &mut self.c.engine);
        for hit in h.take_type_refs() {
            let path = self.c.engine.strs_of(hit.module);
            let name = self.c.engine.str(hit.name).to_string();
            let kind = match hit.qualifier {
                Some(_) => ReferenceKind::Qualified,
                None => ReferenceKind::Unqualified,
            };
            self.record_type_use(&path, &name, hit.span, kind);
            // A `module::Type` occurrence names the import through its
            // qualifier, just like a `module.value` use. So record the
            // `Qualifier` occurrence, which makes the import count as used and
            // lets hover and goto-def resolve the qualifier.
            if let Some((qid, qspan)) = hit.qualifier {
                let q = ast::Identifier {
                    name: self.c.engine.str(qid).to_string(),
                    span: qspan,
                };
                self.record_qualifier_use(&q);
            }
        }
        match result {
            Ok(ty) => ty,
            Err(d) => {
                self.c.engine.diagnostics.push(d);
                self.c.engine.fresh_var()
            }
        }
    }

    /// Like [`Self::hydrate`], for an optional annotation. `None` gives a fresh
    /// type variable.
    pub(super) fn hydrate_opt(&mut self, h: &mut Hydrator, t: Option<&ast::TypeIdentifier>) -> Ty {
        match t {
            Some(t) => self.hydrate(h, t),
            None => self.c.engine.fresh_var(),
        }
    }

    /// Hydrate the annotation on a binding like `x T = ...`, where any type
    /// variable it names must already be in scope (that is, be a parameter of
    /// the enclosing function).
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
                (self.c.ty_nil(), n)
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
                                name: self.c.engine.intern(&ident.name),
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
                // `analyse_module` handles top-level declarations, so reaching
                // one here means it is nested inside an expression block.
                let (kind, sp) = match decl.as_ref() {
                    ast::Declaration::Function(fd) => ("named function", fd.span),
                    ast::Declaration::Type(td) => ("type", td.span),
                    ast::Declaration::Const(cb) => ("const", cb.span),
                };
                self.c.error(
                    format!("{kind} declarations are only allowed at the top level"),
                    sp,
                );
                CNode::Nothing
            }
            // Imports are handled at the start of compilation, before walking
            // the body. By the time we reach one here it is already resolved
            // (or rejected), so there's nothing left to do.
            ast::Statement::ImportDeclaration(_) => CNode::Nothing,
            ast::Statement::Backpass(bp) => {
                // Backpasses are removed before compilation. One can only
                // survive if the parser rejected it (at module top level, for
                // example), which already raised an error. Type the call anyway
                // so its own diagnostics still show.
                CNode::Effect(self.compile_expr(&bp.call))
            }
        }
    }

    /// `x = e` or `x Type = e`. Returns the initialiser and the binding's
    /// binder. The name is bound once the initialiser is checked, so the
    /// initialiser still reads whatever the name meant before (`x = x + 1`).
    /// A lambda initialiser may call itself by the name.
    fn compile_name_binding(
        &mut self,
        vb: &ast::Binding,
        ident: &ast::Identifier,
    ) -> (CExpr, BinderId) {
        let name_id = self.c.engine.intern(&ident.name);
        let annot_ty = vb.typ.as_ref().map(|a| self.hydrate_annotation(a));

        self.c.engine.enter_level();
        let self_ty = self.c.engine.fresh_var();
        // Hand the name to the lambda's own frame through a value that its
        // `enter_fn_frame` uses up once. Setting it on the current frame would
        // leak into nested lambdas and lambdas passed as arguments, and resolve
        // their calls to the enclosing function as `Self_`, which emits
        // `CallSelf` against the wrong, live frame.
        let init_is_fn = matches!(vb.init, ast::Expression::FunctionExpression(_));
        let saved_self = if init_is_fn {
            self.next_fn_self_name.replace((name_id, self_ty))
        } else {
            self.next_fn_self_name.take()
        };
        let init = self.compile_expr_with_hint(&vb.init, annot_ty);
        let init_ty = init.ty;
        self.next_fn_self_name = saved_self;

        self.c.engine.unify_at(self_ty, init_ty, vb.init.span());
        self.c.engine.leave_level();

        let final_ty = if let Some(a) = annot_ty {
            self.c
                .engine
                .unify_at(a, init_ty, type_defining_span(&vb.init));
            a
        } else {
            init_ty
        };

        let restricted = self.c.restricted_generalization_cons();
        let scheme = self.c.engine.generalize_restricted(final_ty, &restricted);
        let binder = self.bind_local(&ident.name, final_ty, scheme, ident.span, vb.doc.clone());
        (init, binder)
    }

    /// `(a, b) = e` or `Point(x:, ..) = e`. This is typed like a `match` with
    /// one arm, and that arm has to cover every value, so only a pattern that
    /// can't fail works as a binding.
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
            .c
            .engine
            .resolve_for_patterns(init_ty, Some(&self.c.env), depth);
        let mut um = UsefulnessMatrix::new(resolved);
        let pat = um.lower(&pattern);
        if let Some(missing) = um.find_missing(&[pat]) {
            self.c.error(
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

    /// Bind the module's imports. [`Compiler::load_imports`] already compiled
    /// each one and handed back `loaded` in the same order.
    fn bind_imports(
        &mut self,
        block: &ast::BlockExpression,
        loaded: Vec<Option<(ModulePath, ModuleKey)>>,
    ) {
        for (imp, loaded) in import_decls(block).zip(loaded) {
            if let Some((canon, key)) = loaded {
                self.bind_import(imp, canon, key);
            }
        }
    }
}

impl Compiler {
    /// A dependency that was a cache hit reserved its id range but added
    /// nothing to `next_type_id` on this pass. So move the entry file past
    /// every reserved range before it allocates any type ids of its own.
    fn bump_type_ids_past_reserved(&mut self) {
        let hw = self.module_table.id_high_water();
        let cur = self.env.next_type_id();
        if hw > cur {
            self.env.set_next_type_id(hw);
        }
    }
}

impl ModuleChecker<'_> {
    /// `canon` is the module's identity, which is the file it resolved to.
    /// Every key further on (the module table, the reference interner and the
    /// qualifier map) must use it, never `imp.path`, which is only how *this*
    /// file spelt it.
    fn bind_import(&mut self, imp: &ast::ImportDeclaration, canon: ModulePath, key: ModuleKey) {
        // The default qualifier is the last segment of the module name.
        // Relative markers live in `path.leading`, so they can't end up here.
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

        // In the reference graph, the qualifier binding is a `ModuleAlias`
        // definition. The last segment of the module path is an `Import`
        // occurrence and the `as` alias is an `Alias` occurrence. Both target
        // the alias, so qualified `q.member` uses, find-references and rename
        // resolve back to this import, and the unused-import rule can see it.
        // The `Import` occurrence is recorded at `imp.path_span`, which is only
        // the last segment, so the `import` keyword and the earlier path
        // segments (`scarlet` and `/` in `import scarlet/string`) aren't
        // clickable. The alias `Definition` also stores the full declaration
        // span (`decl_span`) and the imported `ModuleId` (`imports_module`), so
        // unused-import detection reads both directly instead of working them
        // out from where the occurrences sit.
        let alias_span = imp.alias.as_ref().map_or(imp.span, |a| a.span);
        let cur_path = self.current_module.clone();
        let cur_mid = self.c.ref_interner.intern(&cur_path);
        let imported_mid = self.c.ref_interner.intern(&canon);
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
        // `Import` occurrence targets a `DefId` in that module. Goto-def on the
        // `b` in `import a/b` then lands in `b`'s file, not on the alias's own
        // binding (which would jump nowhere). Find-references, rename and the
        // unused-import rule all go by the alias `Definition` above, so
        // pointing only the occurrence elsewhere changes nothing for them.
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

        // Every public type of the module can be named as `qualifier::Name`
        // where a type goes. Register each one under the combined
        // `qualifier::Name` key that the hydrator looks for on a qualified
        // reference. Type names can't contain ':', so these keys never clash
        // with local declarations.
        let qualified_types: Vec<(String, _)> = self
            .c
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
            self.c.env.store_type_info(&mangled, ti);
        }

        self.imported_qualifiers.insert(qualifier, key.clone());
    }
}

impl Compiler {
    /// How to name a module in a diagnostic. This is the path the importer
    /// wrote (`./lib`), or else the module's own last segment. It is never the
    /// canonical key, because that is an identity, not a name.
    fn module_name(&self, key: &ModuleKey) -> String {
        self.module_display.get(key).cloned().unwrap_or_else(|| {
            let k = key.as_str();
            k.rsplit('/').next().unwrap_or(k).to_string()
        })
    }

    /// Compile every module that `block` imports, in order, before the
    /// importer's own check starts. An import that failed to load gives `None`,
    /// and was already reported.
    fn load_imports(
        &mut self,
        block: &ast::BlockExpression,
        importer: &Importer<'_>,
    ) -> Vec<Option<(ModulePath, ModuleKey)>> {
        import_decls(block)
            .map(|imp| {
                let loaded = self.load_module(&imp.path, imp.span, importer)?;
                self.module_display
                    .insert(loaded.1.clone(), imp.path.to_string());
                Some(loaded)
            })
            .collect()
    }

    /// Load the module that `path` names, relative to the *importing* module's
    /// directory, and return its canonical identity.
    ///
    /// The path is resolved **before** we look in the cache, and the key comes
    /// from the file it resolved to, never from `path` as written. Keying on
    /// the written path made `./b` mean one module across the whole program,
    /// so a `sub/mid.sca` that imported `./b` quietly got the root's `b.sca`.
    /// See `module::file_module_path`.
    pub(crate) fn load_module(
        &mut self,
        path: &ast::ImportPath,
        at: Span,
        importer: &Importer<'_>,
    ) -> Option<(ModulePath, ModuleKey)> {
        let resolved =
            match module::resolve_with(path, importer.dir, self.stdlib_source_root.as_deref()) {
                Ok(r) => r,
                // These two get more specific advice about import syntax than
                // the shared `Display` text of `ResolveError` gives.
                Err(ResolveError::BareName(p)) => {
                    self.module_error(
                        format!(
                            "Unknown module '{p}'. Package imports aren't supported yet, \
                         so use a relative path like `./{p}`"
                        ),
                        at,
                    );
                    return None;
                }
                Err(ResolveError::NoBaseDir) => {
                    self.module_error(
                        "Relative imports don't work without a file, such as in the REPL"
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

        // The identity of a module on disk is the file it resolved to, and
        // `resolve` already minted the canonical path and key from it.
        let module::ResolvedModule { source, canon, key } = resolved;
        let importer = importer.key.clone();
        if self.module_table.get(&key).is_some() {
            self.module_table.record_dependent(&key, &importer);
            return Some((canon, key));
        }
        if self.module_table.is_loading(&key) {
            self.module_error(
                format!(
                    "Import cycle at module '{key}'. Modules can't import each other in a loop"
                ),
                at,
            );
            return None;
        }

        let (text, child_base, source_path): (String, Option<PathBuf>, Option<PathBuf>) =
            match source {
                ModuleSource::Embedded(s) => (s.to_string(), None, None),
                ModuleSource::File(p) => match self.module_table.read_source(&p) {
                    Ok(t) => (t, p.parent().map(|d| d.to_path_buf()), Some(p)),
                    Err(e) => {
                        self.module_error(format!("Couldn't read module '{key}': {e}"), at);
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
            // The span is `at`, the import site in the *importing* module, so
            // provenance is left for the importer's own stamping pass.
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
        // The watermark is taken *after* this module's own dependencies have
        // loaded (`compile_module_body` does that first), but it shows the
        // arena state just *before* this module's body added anything.
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

    /// Run `f` with [`Self::retain_namespaces`] set. Modules compiled inside it
    /// leave their by-name entries at the root instead of scoping them to their
    /// frame. This is only for loading the stdlib at startup, so see the
    /// field's doc.
    pub(crate) fn with_retained_namespaces<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        let prev = self.retain_namespaces;
        self.retain_namespaces = true;
        let out = f(self);
        self.retain_namespaces = prev;
        out
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
        // Everything pushed from here on was raised while this module (or one
        // of its imports, which stamp their own first) was being compiled, so
        // its spans point into this module's text, not the entry file's.
        let diag_mark = self.engine.diagnostics.len();
        // A module is interned before the modules it imports, so module ids
        // follow the order modules are reached in.
        self.ref_interner.intern(&path);
        let loaded = self.load_imports(
            block,
            &Importer {
                dir: base_dir.as_deref(),
                key: &key,
            },
        );
        let imports: Vec<ModuleKey> = loaded.iter().flatten().map(|(_, k)| k.clone()).collect();
        // Reserve (or reuse) this module's stable type-id range, aligned to
        // 256, *before* the watermark is taken, so the watermark's
        // `env.next_type_id == base`. On invalidation, `reset_to` and
        // `truncate_to` restore `next_type_id` from that watermark, which puts
        // the module back at the start of its own range. So editing one module
        // never shifts another's ids. Its imports have already reserved their
        // ranges, so the floor (the current `next_type_id`) is already past
        // every stdlib and dependency id. `key` is the same key that
        // `load_module` caches this module under.
        let floor = self.env.next_type_id();
        let reservation = self.module_table.id_base_for(&key, floor);
        let base = reservation.base();
        self.env.set_next_type_id(base);
        let watermark = self.watermark();

        let mut checker = ModuleChecker::new(self, path, key.clone());
        checker.module_refs.set_doc(doc);
        checker.bind_imports(block, loaded);
        let nodes = checker.analyse_module(block, Some(&mut iface));
        checker.lower_module_init(&key, &nodes, block.span);
        let refs = checker.finish();

        // Record how many type ids the module really used, so `id_high_water`
        // follows real usage, and a reused range that spills past
        // `MODULE_TYPE_ID_RANGE` sets `id_range_overflow` for the recovery path
        // that invalidates and resets everything.
        let used = self.env.next_type_id().0 - base.0;
        reservation.note_usage(&mut self.module_table, used);

        // Stamp provenance on every diagnostic this module's compile produced.
        // Its imports have already stamped theirs, so only the ones still
        // unstamped (relative to the entry) become ours.
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
}

impl ModuleChecker<'_> {
    /// The function a program starts at. This checks the entry file's `main`
    /// (it must be `pub` and take no parameters) whenever there is one, so
    /// `check` reports a broken entry point too. It also requires one when the
    /// program is going to be run, and returns its `FuncIdx` then. A program
    /// with no usable `main` gets a diagnostic, which is what stops it running.
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
                self.c.error(
                    "There's no `main` function. A program starts at `pub fn main()`, so add one"
                        .to_string(),
                    Span::point(module_span.start_line, module_span.start_column),
                );
            }
            return None;
        };
        let mut usable = true;
        if !public {
            self.c.error(
                "`main` must be public, because a program starts at `pub fn main()`".to_string(),
                fd.identifier.span,
            );
            usable = false;
        }
        if !fd.params.is_empty() {
            self.c.error(
                "`main` takes no parameters. Read the program's arguments with `os.argv()` instead"
                    .to_string(),
                fd.identifier.span,
            );
            usable = false;
        }
        if check_only || !usable {
            return None;
        }
        // `analyse_module` compiled this unless its body failed, and in that
        // case the module isn't clean and nothing runs anyway.
        let slot = self.toplevel_decls.iter().find(|d| d.node == node)?.slot;
        self.c.global_to_func.get(&slot).copied()
    }

    /// Run Perceus on a toplevel that was just lowered, and record it. An
    /// imported module's toplevel joins [`Self::inits`], and the entry file's
    /// becomes the program's own. For `TopKind::Entry` this returns the
    /// [`EntryToplevel`] witness. This is the only place it is minted, so it
    /// can't get ahead of the recording it stands for.
    fn record_toplevel(&mut self, lowered: LoweredBody, kind: TopKind) -> Option<EntryToplevel> {
        use crate::core_ir::perceus;
        // Every module-scope slot left on a binding's node. One that is still
        // here was bound by something that isn't a binding statement.
        if !self.binding_globals.is_empty() {
            unclaimed_toplevel_slots(self.binding_globals.len());
        }
        let LoweredBody {
            name,
            core: top,
            pool,
        } = lowered;
        // We run Perceus so that *temporaries* passed into calls are moved
        // (rc==1 in the callee, so its own reuse fires). `keep_globals` then
        // takes the pinned globals back out, because their last use in the
        // toplevel is not their last use in the program.
        let top = perceus::keep_globals(perceus::perceus(&pool, top));
        if std::env::var("CORE_DBG").is_ok() {
            eprintln!("=== {}\n{top}", self.c.engine.str(name));
        }
        let lowered = self.lowered_fn(name, top, pool);
        match kind {
            TopKind::Module => {
                self.c.inits.push(lowered);
                None
            }
            TopKind::Entry { entered } => {
                self.c.entry_toplevel = Some(lowered);
                self.c.main = entered;
                Some(EntryToplevel { _priv: () })
            }
        }
    }

    /// Elaborate a module toplevel that was just analysed, lower it to Core and
    /// record it, just as `compile_with` does for `__main__`.
    fn lower_module_init(&mut self, key: &ModuleKey, nodes: &[CNode], span: Span) {
        let Some(clean) = self.c.clean_module() else {
            return;
        };
        let name = self.c.engine.intern(key.as_str());
        let nil = self.c.ty_nil();
        // Elaborate and lower it even under `check_only`, so a module toplevel
        // the front half can't handle becomes a diagnostic and not a crash at
        // `scarlet run`.
        let lowered = self.elaborate_then_materialize(clean, None, |c, pool, fns| {
            typed_ir::elaborate_toplevel(c, pool, fns, name, nodes, span, nil)
        });
        if !self.c.check_only {
            self.record_toplevel(lowered, TopKind::Module);
        }
    }

    /// Look up a `qualifier.member` reference where `qualifier` is the name of
    /// an imported module. Returns the value's scheme and, if it has a runtime
    /// binding, its slot in the entry frame.
    fn lookup_module_member(
        &mut self,
        module_key: &ModuleKey,
        member: &str,
        member_span: Span,
    ) -> Option<(Scheme, Option<GlobalSlot>)> {
        let Some(iface) = self.c.module_table.get(module_key) else {
            self.c
                .module_error(format!("Module '{module_key}' is not loaded"), member_span);
            return None;
        };
        if let Some(ev) = iface.values.get(member).cloned() {
            return Some((ev.scheme, ev.local_slot));
        }
        let private = iface.private_names.contains(member);
        let name = self.c.module_name(module_key);
        if private {
            self.c.error(
                format!("'{member}' is private in module '{name}'"),
                member_span,
            );
        } else {
            self.c.error(
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
    /// The tree lets the elaborator use the typechecker's answers instead of
    /// working them out again. Instantiating a constructor or module function
    /// scheme again would give unsolved variables where the walk had a concrete
    /// `Con`.
    fn compile_expr(&mut self, expr: &ast::Expression) -> CExpr {
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
                self.open_block();
                let mut last_ty = self.c.ty_nil();
                let mut nodes = Vec::with_capacity(be.body.len());

                for (i, node) in be.body.iter().enumerate() {
                    let is_last = i == be.body.len() - 1;
                    let (ty, cnode) = self.compile_node(node);
                    nodes.push(cnode);

                    if is_last {
                        last_ty = ty;
                    } else if matches!(node, ast::Node::Expression(_)) {
                        let resolved = self.c.engine.find(ty);
                        let is_nil = matches!(
                            self.c.engine.node(resolved),
                            TypeNode::Con { id, .. } if self.c.prelude.nil().is(id)
                        );
                        let is_var = matches!(self.c.engine.node(resolved), TypeNode::Var(_));
                        if !is_nil && !is_var {
                            let ty_str = self.c.engine.type_to_str(ty);
                            self.c.error(
                                format!(
                                    "This expression's value of type '{ty_str}' isn't used. Assign it to a name, or write '_ = ...' to throw it away"
                                ),
                                node.span(),
                            );
                        }
                    }
                }

                self.close_block();
                (last_ty, CKind::Block(nodes))
            }
            ast::Expression::NumberLiteral(nl) => {
                // `const_number` is the one place that decides between int and
                // float (and raises the overflow diagnostic). The elaborator
                // pools the constant itself.
                let c = self.c.const_number(nl);
                let ty = if matches!(c, Const::Float(_)) {
                    self.c.engine.icon_float()
                } else {
                    self.c.ty_int()
                };
                (ty, CKind::Number(nl.clone()))
            }
            ast::Expression::StringLiteral(sl) => {
                (self.c.ty_string(), CKind::String(sl.value.clone()))
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
                (self.c.ty_string(), CKind::Interp(parts))
            }
            ast::Expression::Identifier(id) => self.compile_identifier(id),
            ast::Expression::BinaryExpression(be) => self.compile_binary(be),
            ast::Expression::UnaryExpression(ue) => {
                let operand = self.compile_expr(&ue.expression);
                let inner_ty = operand.ty;
                let ty = match ue.op {
                    ast::UnaryOp::Not => {
                        let b = self.c.ty_bool();
                        self.c.engine.unify_at(b, inner_ty, ue.expression.span());
                        self.c.ty_bool()
                    }
                    ast::UnaryOp::Neg => {
                        let result = self.c.engine.fresh_constrained_var(Constraint::Numeric);
                        self.c
                            .engine
                            .unify_at(result, inner_ty, ue.expression.span());
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
                let b = self.c.ty_bool();
                self.c.engine.unify_at(b, cond.ty, ie.condition.span());
                let then = self.compile_expr(&ie.body);
                let els = self.compile_expr(&ie.else_body);

                let ret = self.c.engine.fresh_var();
                self.c
                    .engine
                    .unify_at(ret, then.ty, type_defining_span(&ie.body));
                self.c
                    .engine
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
                (self.c.engine.mk_tuple(&elem_tys), CKind::Tuple(elems))
            }
            ast::Expression::ArrayIndexExpression(aie) => {
                let recv = self.compile_expr(&aie.expression);
                let elem_var = self.c.engine.fresh_var();
                let arr_expected = self.c.ty_array(elem_var);
                self.c
                    .engine
                    .unify_at(arr_expected, recv.ty, aie.expression.span());

                if let ast::Expression::RangeExpression(r) = aie.index.as_ref() {
                    let start = self.compile_expr(&r.start);
                    let end = self.compile_expr(&r.end);
                    let int_t = self.c.ty_int();
                    self.c.engine.unify_at(int_t, start.ty, r.start.span());
                    self.c.engine.unify_at(int_t, end.ty, r.end.span());
                    // A slice can fail, when its range is not inside the
                    // array, so it is a `Result`, as `binary.slice_bits` is.
                    let slice = self.c.ty_array(elem_var);
                    let nil = self.c.ty_nil();
                    let kind = CKind::Slice {
                        recv: Box::new(recv),
                        start: Box::new(start),
                        end: Box::new(end),
                    };
                    (self.c.ty_result(slice, nil), kind)
                } else {
                    let index = self.compile_expr(&aie.index);
                    let int_t = self.c.ty_int();
                    self.c.engine.unify_at(int_t, index.ty, aie.index.span());
                    let kind = CKind::Index {
                        recv: Box::new(recv),
                        index: Box::new(index),
                    };
                    (self.c.ty_option(elem_var), kind)
                }
            }
            ast::Expression::RangeExpression(re) => {
                let start = self.compile_expr(&re.start);
                let end = self.compile_expr(&re.end);
                let int_t = self.c.ty_int();
                self.c.engine.unify_at(int_t, start.ty, re.start.span());
                self.c.engine.unify_at(int_t, end.ty, re.end.span());
                let i = self.c.ty_int();
                let kind = CKind::Range {
                    start: Box::new(start),
                    end: Box::new(end),
                };
                (self.c.ty_array(i), kind)
            }
            ast::Expression::FunctionExpression(fe) => {
                self.c.engine.enter_level();
                let (ty, closure) = self.compile_function_common(
                    &fe.params,
                    &fe.body,
                    fe.return_type.as_ref(),
                    None,
                );
                self.c.engine.leave_level();
                (ty, CKind::Closure(closure))
            }
            ast::Expression::FunctionCallExpression(fc) => self.compile_call(fc),
            ast::Expression::PropertyAccessExpression(pa) => self.compile_property_access(pa),
            ast::Expression::ModuleAccessExpression(ma) => self.compile_module_access(ma),
            ast::Expression::OrExpression(oe) => self.compile_or(oe),
            ast::Expression::PipeExpression(p) => {
                // Pipes are removed before type checking (`compile_with` and
                // `check_impl` rewrite every pipe first), the same guarantee as
                // for `Statement::Backpass`. Type both sides anyway, so one
                // that survives still shows its operands' own diagnostics
                // instead of panicking.
                self.compile_expr(&p.left);
                self.compile_expr(&p.right);
                (self.c.engine.fresh_var(), CKind::Error)
            }
            ast::Expression::ErrorNode(err) => (self.error_node(err), CKind::Error),
        }
    }

    /// An `ErrorNode` stands in for a part of the source the parser couldn't
    /// read, so there's nothing in it to check or elaborate. Every place in the
    /// parser that builds one also records the error, but the entry file's
    /// parse diagnostics never go into `engine.diagnostics`. So we repeat the
    /// error here, but only when nothing else has already denied the module its
    /// [`CleanModule`] proof.
    ///
    /// That keeps the diagnostics a user sees exactly the same. The CLI stops
    /// before checking a file that failed to parse, the LSP only publishes the
    /// parse errors for such a buffer, and an imported module's parse errors
    /// are folded in before its body is walked. It also makes the one form in
    /// the language that can't be elaborated a checked error, not a bug report
    /// from a later pass.
    fn error_node(&mut self, err: &ast::ErrorNode) -> Ty {
        if self.c.clean_module().is_some() {
            self.c.engine.diagnostics.push(Diagnostic::error(
                err.span,
                DiagnosticCode::ParseError,
                err.error.to_string(),
            ));
        }
        self.c.engine.fresh_var()
    }

    /// Compile an expression with an optional hint about the type it should
    /// have. The hint is only used for function-literal arguments where the
    /// parameter type is known. It is pushed down so lambda parameters without
    /// annotations get a concrete type straight away, which avoids "can't
    /// access field on unknown type" errors inside the body.
    pub(super) fn compile_expr_with_hint(
        &mut self,
        expr: &ast::Expression,
        hint: Option<Ty>,
    ) -> CExpr {
        if let Some(h) = hint
            && let ast::Expression::FunctionExpression(fe) = expr
        {
            let resolved = self.c.engine.find(h);
            if let TypeNode::Fun { params, .. } = self.c.engine.node(resolved)
                && params.len as usize == fe.params.len()
            {
                let params: Vec<Ty> = self.c.engine.children_of(params).to_vec();
                self.c.engine.enter_level();
                let (ty, closure) = self.compile_function_common(
                    &fe.params,
                    &fe.body,
                    fe.return_type.as_ref(),
                    Some(params.clone()),
                );
                self.c.engine.leave_level();
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

    /// The declared parameter labels of a module `fn`. An empty slice means
    /// "not recorded", never "takes no parameters", so it refuses any label.
    fn param_labels_of(&self, kind: ValueKind) -> Option<Vec<StrId>> {
        match kind {
            ValueKind::ModuleFn { param_labels } if param_labels.len > 0 => {
                Some(self.c.engine.str_ids_of(param_labels).to_vec())
            }
            _ => None,
        }
    }

    /// How to reach a constructor called `name` that isn't in scope, which is
    /// through each imported module that has one, as `alias::Name`.
    fn qualified_spellings(&self, name: &str) -> Vec<String> {
        let mut out: Vec<String> = self
            .imported_qualifiers
            .iter()
            .filter(|(_, key)| {
                self.c
                    .module_table
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
        let id = self.c.engine.intern(name);

        let Some((scheme, at)) = self.resolve_name(id) else {
            // A module's name isn't a value, so say what it is and how to reach
            // what's in it.
            if let Some(key) = self.imported_qualifiers.get(name) {
                let module = self.c.module_name(key);
                // Pick one of its functions or values, to show what a use looks
                // like.
                let example = self.c.module_table.get(key).and_then(|m| {
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
                self.c.error(
                    format!(
                        "'{name}' is the module `{module}`, not a value. Use one of its members{example}"
                    ),
                    expr.span,
                );
                return (self.c.engine.fresh_var(), CKind::Error);
            }
            let qualified = self.qualified_spellings(name);
            if !qualified.is_empty() {
                self.c.error(
                    format!(
                        "Unknown identifier '{name}'. A constructor of that name is {}",
                        qualified.join(" or ")
                    ),
                    expr.span,
                );
            } else if let Some(suggestion) = self.suggest_name(name) {
                self.c.error(
                    format!(
                        "Unknown identifier '{}'. Did you mean '{}'?",
                        name, suggestion
                    ),
                    expr.span,
                );
            } else {
                self.c
                    .error(format!("Unknown identifier '{}'", name), expr.span);
            }
            return (self.c.engine.fresh_var(), CKind::Error);
        };

        let ty = self
            .c
            .engine
            .instantiate(&scheme, &self.frames.current.rigid);
        let kind = scheme.kind;
        let def = scheme.def;
        // Outside the LSP, `record` throws everything away (see its early
        // return), so skip looking up and cloning the doc there.
        let doc = self.doc_if_collecting(name);
        self.record(name, ty, expr.span, doc);
        self.record_value_use(def, expr.span, ReferenceKind::Unqualified);

        // Locals that start with `_` can't be read. The prefix says the value
        // is unused on purpose, so reading it contradicts that.
        if name.starts_with('_') && matches!(kind, ValueKind::Local) {
            let msg = match name.trim_start_matches('_') {
                "" => format!(
                    "'{name}' is a discard binding and can't be read. Give it a name to use its value"
                ),
                stripped => format!(
                    "'{name}' starts with '_', which marks it unused, so it can't be read. Rename it to '{stripped}'"
                ),
            };
            self.c.error(msg, expr.span);
            return (ty, CKind::Error);
        }

        let fact = NameFact {
            ty,
            at,
            param_labels: None,
        };
        (ty, CKind::Name { name: id, fact })
    }

    /// Once a module's inference is final, reject any binder starting with `_`
    /// that discarded a value the type system knows is `Nil`. There is nothing
    /// to discard, and the explicit `Nil` pattern keeps the arm honest, because
    /// it stops compiling the day the type gains real data.
    pub(super) fn check_nil_discards(&mut self) {
        let checks = std::mem::take(&mut self.nil_discards);
        for (name, ty, sp) in checks {
            let rep = self.c.engine.find(ty);
            if matches!(self.c.engine.node(rep), TypeNode::Con { id, .. } if self.c.prelude.nil().is(id))
            {
                self.c.error(
                    format!(
                        "'{name}' always matches Nil here, so write the explicit 'Nil' pattern instead"
                    ),
                    sp,
                );
            }
        }
    }

    /// A module member whose interface gives it a type but no entry-frame
    /// slot.
    fn no_runtime_binding(&mut self, name: &str, module: &str, sp: Span) {
        self.c.error(
            format!("'{name}' in module '{module}' has no runtime binding"),
            sp,
        );
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
                let b = self.c.ty_bool();
                self.c.engine.unify_at(b, left.ty, expr.left.span());
                let right = self.compile_expr(&expr.right);
                self.c.engine.unify_at(b, right.ty, expr.right.span());
                return (self.c.ty_bool(), node(left, right));
            }
            BinaryOp::Eq | BinaryOp::Ne => {
                let left = self.compile_expr(&expr.left);
                let right = self.compile_expr(&expr.right);
                self.c.engine.unify_at(left.ty, right.ty, expr.span);
                return (self.c.ty_bool(), node(left, right));
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
        let operand = self.c.engine.fresh_constrained_var(constraint);
        self.c.engine.unify_at(operand, left.ty, expr.left.span());
        self.c.engine.unify_at(operand, right.ty, expr.right.span());
        let ty = if is_cmp { self.c.ty_bool() } else { operand };
        (ty, node(left, right))
    }

    // ========================================================================
    // Binary literals  ( `<<v:size:kind, ..>>` )
    // ========================================================================

    /// Type-check a `<<v:size:kind, ..>>` literal. Every segment's value, and
    /// its runtime size expression, is checked against the type its kind
    /// expects. The encoding itself is Core's job.
    fn compile_binary_literal(&mut self, bl: &ast::BinaryLiteral) -> (Ty, CKind) {
        let mut segs = Vec::with_capacity(bl.segments.len());
        for seg in &bl.segments {
            let value = self.compile_expr(&seg.value);
            let expected = match seg.spec {
                ast::BinSpec::Int { .. } => self.c.ty_int(),
                ast::BinSpec::Binary { .. } => self.c.ty_binary(),
                ast::BinSpec::Utf8 => self.c.ty_string(),
            };
            self.c.engine.unify_at(expected, value.ty, seg.value.span());
            let width = match &seg.spec {
                ast::BinSpec::Int { bits } => CWidth::Int(self.type_seg_size(bits.as_ref())),
                ast::BinSpec::Binary { bytes } => CWidth::Bytes(self.type_seg_size(bytes.as_ref())),
                ast::BinSpec::Utf8 => CWidth::Utf8,
            };
            segs.push(CBinSeg { value, width });
        }
        (self.c.ty_binary(), CKind::BinaryLiteral(segs))
    }

    /// A segment's size expression is a runtime `Int`, not a binding.
    fn type_seg_size(&mut self, size: Option<&ast::Expression>) -> Option<CExpr> {
        let e = size?;
        let size = self.compile_expr(e);
        let int_ty = self.c.ty_int();
        self.c.engine.unify_at(int_ty, size.ty, e.span());
        Some(size)
    }

    // ========================================================================
    // Array
    // ========================================================================

    /// Compile a run of array elements that aren't spreads, unifying each with
    /// the array's element type variable.
    fn compile_array_run(
        &mut self,
        elems: &[ast::ArrayElement],
        elem_var: Ty,
        out: &mut Vec<CArrayElem>,
    ) {
        for elem in elems {
            if let ast::ArrayElement::Expression(e) = elem {
                let el = self.compile_expr(e);
                self.c
                    .engine
                    .unify_at(elem_var, el.ty, type_defining_span(e));
                out.push(CArrayElem::Elem(el));
            }
        }
    }

    fn compile_array(&mut self, expr: &ast::ArrayExpression) -> (Ty, CKind) {
        let elem_var = self.c.engine.fresh_var();
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
            let expected = self.c.ty_array(elem_var);
            self.c.engine.unify_at(expected, spread.ty, inner.span());
            elems.push(CArrayElem::Spread(spread));
        }
        self.compile_array_run(&expr.elements[run_start..], elem_var, &mut elems);
        (self.c.ty_array(elem_var), CKind::Array(elems))
    }

    // ========================================================================
    // Function call
    // ========================================================================

    fn compile_call(&mut self, expr: &ast::FunctionCallExpression) -> (Ty, CKind) {
        // Resolve the callee to a name, a scheme and a load action without
        // emitting anything yet. That lets us dispatch on `ValueKind` for
        // constructors and builtins, and use the callee's known type to push
        // hints into function-literal arguments.
        if let Some(ResolvedCallee {
            name,
            span: name_span,
            scheme,
            has_binding,
            at,
            qualifier,
        }) = self.resolve_simple_callee(&expr.callee)
        {
            let inst_ty = self
                .c
                .engine
                .instantiate(&scheme, &self.frames.current.rigid);
            let param_labels = self.param_labels_of(scheme.kind);
            let fact = NameFact {
                ty: inst_ty,
                at,
                param_labels,
            };
            let id = self.c.engine.intern(name);
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
            // This is only for hover. `resolve_simple_callee` emits the graph
            // occurrence, with the right `Unqualified` or `Qualified` kind.
            // Outside the LSP, `record` throws the doc away (see its early
            // return), so skip looking it up and cloning it there.
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
                    // that accepts labelled arguments. An empty label slice
                    // means "not recorded", never "takes no parameters" (see
                    // `ValueKind::ModuleFn`), so it falls back to refusing
                    // labels, not to treating the function as taking none.
                    let labels = match scheme.kind {
                        ValueKind::ModuleFn { param_labels } if param_labels.len > 0 => {
                            Some(param_labels)
                        }
                        _ => None,
                    };
                    let ret = self.compile_call_args(inst_ty, labels, &expr.arguments, expr.span);
                    if let (false, Some(key)) = (has_binding, &qualifier) {
                        let module = self.c.module_name(key);
                        self.no_runtime_binding(name, &module, name_span);
                    }
                    ret
                }
            };
            return match args {
                Some(args) => (ty, CKind::Call { callee, args }),
                None => (ty, CKind::Error),
            };
        }

        // The callee is some other expression. Compile it first so we have its
        // type for argument hints. A `module::member` callee only gets here
        // when `resolve_simple_callee` has already reported it, so only check
        // the arguments and don't report it twice.
        let callee = match expr.callee.as_ref() {
            ast::Expression::ModuleAccessExpression(_) => None,
            callee => Some(self.compile_expr(callee)),
        };
        let callee_ty = match &callee {
            Some(c) => c.ty,
            None => self.c.engine.fresh_var(),
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
    /// when the callee isn't a function at all.
    ///
    /// `param_labels` is the callee's declared parameter names when it is a
    /// module `fn` reached by name. It is `None` for every other callee (a
    /// local binding holding a function, a `@vm` builtin, or any other
    /// expression), because none of those has parameter names at the call site.
    /// Only the first kind accepts labelled arguments, and the rest still
    /// refuse them. Spread is only for constructors either way.
    ///
    /// The arguments are checked in *source* order, so the reordering a
    /// labelled call needs is applied here to the parameter *types*, and to the
    /// emitted arguments in `Elab::value_call`. Both get it from
    /// [`slot_labeled`] over this one slice, so the order the arguments are
    /// checked in and the order they are passed in can't disagree.
    fn compile_call_args(
        &mut self,
        callee_ty: Ty,
        param_labels: Option<ArenaSlice<pool::StrSlices>>,
        args: &[ast::CallArg],
        call_span: Span,
    ) -> (Ty, Option<Vec<CArg>>) {
        let (params, ret) = match self.c.engine.match_fun_type(callee_ty, args.len()) {
            Ok(pr) => pr,
            Err(MatchFunTypeError::IncorrectArity {
                expected,
                given,
                params,
                ret,
            }) => {
                self.c.error(
                    format!("Expected {} argument(s), got {}", expected, given),
                    call_span,
                );
                (params, ret)
            }
            Err(MatchFunTypeError::NotFn { ty }) => {
                let s = self.c.engine.type_to_str(ty);
                self.c.error(
                    format!(
                        "This value of type '{}' is not a function and cannot be called",
                        s
                    ),
                    call_span,
                );
                for arg in args {
                    self.compile_call_arg_value(arg);
                }
                return (self.c.engine.fresh_var(), None);
            }
        };

        // Which parameter each source argument fills. A call with no labels
        // keeps the order as it is, so only a labelled one pays for the
        // slotting.
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
                        self.c.error(
                            "Labelled arguments need a callee whose parameter names are known: \
                             a constructor, or a function named directly"
                                .to_string(),
                            label.span,
                        );
                    }
                    let v = self.compile_expr_with_hint(value, hint);
                    let ty = v.ty;
                    let label = self.c.engine.intern(&label.name);
                    out.push(CArg::Labeled { label, value: v });
                    (ty, value.span())
                }
                ast::CallArg::Spread(e) => {
                    self.c.error(
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
                self.c.engine.unify_at(p, value, arg_span);
            }
        }

        (ret, Some(out))
    }

    /// Map each source argument to the parameter it fills, and report a label
    /// that names no parameter or a parameter given twice. An argument that
    /// filled no slot gets `None`. That is a positional argument past the last
    /// parameter, whose count `match_fun_type` has already reported, or a bad
    /// label reported here.
    fn slot_call_args(
        &mut self,
        param_labels: ArenaSlice<pool::StrSlices>,
        args: &[ast::CallArg],
    ) -> Vec<Option<usize>> {
        // Interning first releases the `&mut engine` borrow before we take the
        // `str_ids_of` slice.
        let items: Vec<(Option<StrId>, (usize, Span))> = args
            .iter()
            .enumerate()
            .filter_map(|(i, a)| match a {
                ast::CallArg::Positional(e) => Some((None, (i, e.span()))),
                ast::CallArg::Labeled { label, .. } => {
                    Some((Some(self.c.engine.intern(&label.name)), (i, label.span)))
                }
                // The caller refuses spreads, so one fills no parameter.
                ast::CallArg::Spread(_) => None,
            })
            .collect();
        // One slot per declared parameter, sized and named from this one slice,
        // so there's no second width for a label table to drift from.
        let names: Vec<Option<StrId>> = self
            .c
            .engine
            .str_ids_of(param_labels)
            .iter()
            .map(|&n| Some(n))
            .collect();
        let (by_pos, errors) = slot_labeled(&names, items);
        for e in errors {
            match e {
                // A wrong argument count, which `match_fun_type` has already
                // reported.
                SlotError::ExtraPositional(_) => {}
                SlotError::UnknownLabel(label, (_, span)) => {
                    let available = self.c.engine.strs_of(param_labels).join(", ");
                    let label = self.c.engine.str(label).to_string();
                    self.c.error(
                        format!(
                            "This function has no parameter '{}'. Available: {}",
                            label, available
                        ),
                        span,
                    );
                }
                SlotError::Duplicate((_, span), param) => {
                    let dup = self
                        .c
                        .engine
                        .str_ids_of(param_labels)
                        .get(param)
                        .map_or("_".to_string(), |&id| self.c.engine.str(id).to_string());
                    self.c.error(
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

    /// Resolve `module::member`. This looks the member up, notes whether it has
    /// a runtime binding, and records the `Qualified` value use. Both the
    /// callee position (`resolve_simple_callee`) and the value position
    /// (`compile_module_access`) use it. `None` means the access was already
    /// reported.
    ///
    /// The module name is only looked up among the imports. A value binding
    /// with the same name can't shadow it, because `.` and `::` never overlap.
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
        // The member isn't in unqualified scope, so resolve the occurrence
        // through the imported scheme's canonical `def`.
        self.record_value_use(scheme.def, member.span, ReferenceKind::Qualified);
        self.record_qualifier_use(&ma.module);
        // A member with neither a fixed meaning nor a slot has no runtime
        // binding. `check_named_value` reports that, so nothing elaborates it.
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

    /// `name::member` where `name` isn't an imported module. When a stdlib
    /// module has that name, say which import brings it in.
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
        self.c.module_error(msg, name.span);
    }

    /// Try to resolve a callee expression that is a plain name (or
    /// `module.name`). Returns the scheme that was looked up and whether the
    /// callee has a runtime binding. Anything else falls through to the path
    /// for other expressions.
    fn resolve_simple_callee<'a>(
        &mut self,
        callee: &'a ast::Expression,
    ) -> Option<ResolvedCallee<'a>> {
        match callee {
            ast::Expression::Identifier(id) => {
                let sid = self.c.engine.intern(&id.name);
                let (scheme, at) = self.resolve_name(sid)?;
                // This path skips `compile_identifier`, so this is where an
                // unqualified use in a call really gets recorded.
                self.record_value_use(scheme.def, id.span, ReferenceKind::Unqualified);
                Some(ResolvedCallee {
                    name: &id.name,
                    span: id.span,
                    scheme,
                    at,
                    qualifier: None,
                    has_binding: true,
                })
            }
            // A qualified callee, `module::member()`. A failed lookup was
            // already reported, so it also counts as "no simple callee".
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

    /// Compile a call where the callee is a data constructor. It handles
    /// positional, labelled and `..base` (record update) arguments, and puts
    /// them in the order the fields are declared before emitting.
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
        // The instantiated scheme is `IFun{params, ret}` when there are
        // arguments, or `ICon` when there are none. Pull out the parameter and
        // result types.
        let r = self.c.engine.find(inst_ty);
        let (param_tys, result_ty) = match self.c.engine.node(r) {
            TypeNode::Fun { params, ret } => (self.c.engine.children_of(params).to_vec(), ret),
            _ => (vec![], r),
        };

        // Split the arguments into a spread base (at most one) and a map of the
        // fields given explicitly, keyed by their declared position. Each
        // expression is paired with the index of its source argument, so a
        // slotted value remembers which `args[i]` it came from, and its checked
        // node goes back to that argument.
        let mut spread: Option<(usize, &ast::Expression)> = None;
        for (i, arg) in args.iter().enumerate() {
            if let ast::CallArg::Spread(e) = arg {
                if spread.is_some() {
                    self.c.error(
                        "A constructor call can only have one spread".to_string(),
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

        // Check the spread first and then the fields in declared order. The
        // spread's `unify_at` solves `result_ty` (and with it the constructor's
        // type parameters) before any `compile_expr_with_hint` pushes a
        // parameter type into a function-literal argument, and the diagnostics
        // a mistyped argument raises come out in field order. The checked nodes
        // still go back in source order, which is the order the elaborator
        // spills them in.
        //
        // An argument that `slot_ctor_args` didn't place is an arity or label
        // error it has already reported, so it is left unchecked.
        let slots = &by_pos[..(field_labels_sl.len as usize).min(by_pos.len())];
        let mut checked: Vec<Option<CExpr>> = (0..args.len()).map(|_| None).collect();

        // For a record update, the base is unified with the constructor's
        // result type. Projecting each field is Core's job.
        if let Some((ai, e)) = spread {
            let base = self.compile_expr(e);
            self.c.engine.unify_at(result_ty, base.ty, e.span());
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
                self.c.engine.unify_at(p, v.ty, value.span());
            }
            checked[ai] = Some(v);
        }

        (result_ty, self.ctor_args(args, checked))
    }

    /// A constructor call's arguments in source order, built from the values
    /// the check walk produced for them, with `checked[i]` for `args[i]`. An
    /// argument with no value is one that `slot_ctor_args` refused and already
    /// reported.
    fn ctor_args(&mut self, args: &[ast::CallArg], checked: Vec<Option<CExpr>>) -> Vec<CArg> {
        args.iter()
            .zip(checked)
            .map(|(a, v)| {
                let span = match a {
                    ast::CallArg::Positional(e) | ast::CallArg::Spread(e) => e.span(),
                    ast::CallArg::Labeled { value, .. } => value.span(),
                };
                let v = v.unwrap_or_else(|| CExpr {
                    ty: self.c.engine.fresh_var(),
                    span,
                    kind: CKind::Error,
                });
                match a {
                    ast::CallArg::Positional(_) => CArg::Positional(v),
                    ast::CallArg::Labeled { label, .. } => CArg::Labeled {
                        label: self.c.engine.intern(&label.name),
                        value: v,
                    },
                    ast::CallArg::Spread(_) => CArg::Spread(v),
                }
            })
            .collect()
    }

    /// `C(..base, ...)` fills each field it leaves out with `base.field`, so
    /// each of those has to be a field `base.field` could read, meaning one
    /// that every variant of the type has by name. On a type with one variant
    /// that is every field. On a type with several it is only the fields they
    /// all share, and a spread that leaves out any other field is refused,
    /// because which variant `base` is isn't known until the program runs.
    fn check_spread_fills(
        &mut self,
        type_id: TypeId,
        result_ty: Ty,
        field_labels: ArenaSlice<pool::StrSlices>,
        unfilled: impl Iterator<Item = usize>,
        span: Span,
    ) {
        let resolved = self.c.engine.find(result_ty);
        let (type_name, type_args) = match self.c.engine.node(resolved) {
            TypeNode::Con { name, args, .. } => (
                self.c.engine.str(name).to_string(),
                self.c.engine.children_of(args).to_vec(),
            ),
            _ => return,
        };
        let Some(info) = self.c.env.lookup_type_info_by_id(type_id) else {
            return;
        };
        let labels: SmallVec<[StrId; 4]> =
            SmallVec::from_slice(self.c.engine.str_ids_of(field_labels));
        for i in unfilled {
            let Some(&label) = labels.get(i) else {
                continue;
            };
            if let Err(FieldMismatch::MissingOn(variant)) =
                self.field_in_variants(info, &type_args, label, Some(span))
            {
                let field = self.c.engine.str(label).to_string();
                let why = self.missing_on(variant, &type_name);
                self.c.error(
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

    /// `module::member` where a value goes. `compile_call` handles calls,
    /// through `resolve_simple_callee`.
    fn compile_module_access(&mut self, expr: &ast::ModuleAccessExpression) -> (Ty, CKind) {
        let Some(m) = self.resolve_module_access(expr) else {
            return (self.c.engine.fresh_var(), CKind::Error);
        };
        let ty = self
            .c
            .engine
            .instantiate(&m.scheme, &self.frames.current.rigid);
        let fact = NameFact {
            ty,
            at: m.at,
            param_labels: None,
        };
        self.record(m.member_name, ty, m.member_span, None);
        // A constructor or builtin needs no binding, because the elaborator
        // builds a nullary value or an eta wrapper for it (see `typed_ir::eta`).
        let needs_binding = matches!(m.scheme.kind, ValueKind::Local | ValueKind::ModuleFn { .. });
        if needs_binding && !m.has_binding {
            let module = self.c.module_name(&m.module_key);
            self.no_runtime_binding(m.member_name, &module, m.member_span);
        }
        let name = self.c.engine.intern(m.member_name);
        (ty, CKind::Member { name, fact })
    }

    fn compile_property_access(&mut self, expr: &ast::PropertyAccessExpression) -> (Ty, CKind) {
        // In `c.f`, `c` is an imported module and not a value, so point at
        // `::`.
        if let (ast::Expression::Identifier(id), ast::PropertyKey::Field(field)) =
            (expr.left.as_ref(), &expr.right)
            && (self.c.engine.lookup_str(&id.name))
                .and_then(|n| self.scheme_of(n))
                .is_none()
            && self.imported_qualifiers.contains_key(&id.name)
        {
            self.c.error(
                format!(
                    "'{0}' is a module, not a value. Reach its members with `::`, like `{0}::{1}`",
                    id.name, field.name
                ),
                id.span,
            );
            return (self.c.engine.fresh_var(), CKind::Error);
        }

        let recv = self.compile_expr(&expr.left);
        let left_ty = recv.ty;

        match &expr.right {
            // A `tuple.N` index.
            ast::PropertyKey::TupleIndex(num) => {
                let index = match num.value.parse::<i32>() {
                    Ok(i) => i,
                    Err(_) => {
                        self.c.error(
                            format!("Tuple index must be an integer, got '{}'", num.value),
                            num.span,
                        );
                        return (self.c.engine.fresh_var(), CKind::Error);
                    }
                };

                let resolved = self.c.engine.find(left_ty);
                if let TypeNode::Tuple { elems } = self.c.engine.node(resolved) {
                    let elements = self.c.engine.children_of(elems);
                    if let Ok(idx) = u32::try_from(index)
                        && let Some(&ty) = elements.get(idx as usize)
                    {
                        let kind = CKind::TupleIndex {
                            recv: Box::new(recv),
                            idx,
                        };
                        return (ty, kind);
                    }
                    self.c.error(
                        format!(
                            "Tuple index {} is out of range, because the tuple has {} elements",
                            index,
                            elements.len()
                        ),
                        num.span,
                    );
                    return (self.c.engine.fresh_var(), CKind::Error);
                }
                self.c.error(
                    format!("Can't use .{} here, because the value isn't a tuple", index),
                    num.span,
                );
                (self.c.engine.fresh_var(), CKind::Error)
            }
            // `value.field`, a labelled field that every variant shares.
            ast::PropertyKey::Field(field) => {
                let ty = self.compile_field_access(left_ty, &field.name, field.span);
                let kind = CKind::Field {
                    recv: Box::new(recv),
                    label: self.c.engine.intern(&field.name),
                };
                (ty, kind)
            }
        }
    }

    /// The error path of `compile_field_access`, which reports the error and
    /// gives a fresh type variable.
    fn field_access_bail(&mut self, msg: String, span: Span) -> Ty {
        self.c.error(msg, span);
        self.c.engine.fresh_var()
    }

    /// The one place that looks up a `.field` on a nominal type, by name. It
    /// returns where the field sits in each variant, plus its instantiated
    /// type. A field can only be read when EVERY variant has that label at a
    /// type that unifies. Its position may differ from one variant to the next.
    ///
    /// `unify_span` is `Some` on the typecheck path, where the field types of
    /// each variant still have to be unified (and any mismatch reported). The
    /// lowering path passes `None`, because the typecheck has already run.
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
        // `Variant` and `VariantField` are `Copy` and the pools only grow, so
        // copy each entry out by index instead of calling `to_vec()` on the
        // slices to get them past the `&mut engine` calls in the loop.
        for vi in 0..variants.len as usize {
            let v = self.c.engine.variants_of(variants)[vi];
            let hit = self
                .c
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
                .c
                .engine
                .substitute_type_vars(fty, info.type_params, type_args);
            match ty {
                Some(existing) => {
                    if let Some(span) = unify_span {
                        self.c.engine.unify_at(existing, substituted, span);
                    }
                }
                None => ty = Some(substituted),
            }
        }
        let ty = ty.ok_or(FieldMismatch::NoVariants)?;
        Ok(SharedField { ty, slots })
    }

    /// The end of a sentence that names a field some variant of `type_name`
    /// doesn't have.
    fn missing_on(&self, variant: StrId, type_name: &str) -> String {
        format!(
            "is not present on every variant of '{type_name}' (missing on '{}')",
            self.c.engine.str(variant)
        )
    }

    /// Read `.field` out of a value. The variant of the value isn't known until
    /// the program runs, so the field can only be read if EVERY variant of the
    /// receiver type has a label of that name at the same type, wherever in the
    /// variant it sits.
    ///
    /// It hands the walk to [`ModuleChecker::field_in_variants`], which
    /// lowering also uses, so the slot index the typecheck approved and the
    /// `TupleIndex` that is emitted can never disagree.
    fn compile_field_access(&mut self, receiver_ty: Ty, field: &str, field_span: Span) -> Ty {
        let resolved = self.c.engine.find(receiver_ty);
        let (type_id, type_name, type_args) = match self.c.engine.node(resolved) {
            TypeNode::Con { id, name, args } => (
                id,
                self.c.engine.str(name).to_string(),
                self.c.engine.children_of(args).to_vec(),
            ),
            TypeNode::Var(_) => {
                return self.field_access_bail(
                    format!(
                        "Can't access field '{}' because the value's type isn't known here. Add a type annotation",
                        field
                    ),
                    field_span,
                );
            }
            _ => {
                let s = self.c.engine.type_to_str(resolved);
                return self.field_access_bail(
                    format!("Type '{}' has no field '{}'", s, field),
                    field_span,
                );
            }
        };

        // Look up by identity. The receiver's variants come from the type the
        // `Con` node identifies, never from whatever type with the same name a
        // flat lookup by name would find first.
        let Some(info) = self.c.env.lookup_type_info_by_id(type_id) else {
            return self.field_access_bail(
                format!("Type '{}' has no field '{}'", type_name, field),
                field_span,
            );
        };

        let field_id = self.c.engine.intern(field);
        let result_ty = match self.field_in_variants(info, &type_args, field_id, Some(field_span)) {
            Ok(field) => field.ty,
            // A variant list with no variants at all. There's nothing to read,
            // and the empty type was already reported at its declaration.
            Err(FieldMismatch::NoVariants) => self.c.engine.fresh_var(),
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
        // `Type.field` is a dotted key in `env.definitions` that no local can
        // shadow, so resolving it here is safe. It keeps goto-def,
        // find-references and rename working for record fields, and feeds the
        // dead-code reachability walk for fields.
        let fdef = self.c.env.lookup_definition(&qualified);
        self.record_value_use(fdef, field_span, ReferenceKind::Unqualified);
        result_ty
    }

    // ========================================================================
    // Or expression (Option/Result unwrapping)
    // ========================================================================

    fn compile_or(&mut self, expr: &ast::OrExpression) -> (Ty, CKind) {
        let lhs = self.compile_expr(&expr.expression);
        let left_ty = lhs.ty;
        let resolved = self.c.engine.find(left_ty);

        let success_var = self.c.engine.fresh_var();

        // Work out which prelude type the left side is. The `Con` node carries
        // its nominal id, so a user type that happens to be called "Option" or
        // "Result" can never match.
        let lhs_type_id = match self.c.engine.node(resolved) {
            TypeNode::Con { id, .. } => id,
            _ => TypeId::NONE,
        };

        // Option and Result only differ in the `ICon` we expect and in whether
        // the failure case carries a payload you can bind (`err_var`).
        let err_var = if self.c.prelude.option().is(lhs_type_id) {
            if let Some(recv) = &expr.receiver {
                self.c.error(
                    "'or' on an Option can't bind a value, because None carries nothing"
                        .to_string(),
                    recv.span,
                );
            }
            None::<Ty>
        } else if self.c.prelude.result().is(lhs_type_id) {
            Some(self.c.engine.fresh_var())
        } else {
            let s = self.c.engine.type_to_str(left_ty);
            self.c.error(
                format!(
                    "'or' requires the left side to be Option(_) or Result(_, _), got '{}'",
                    s
                ),
                expr.expression.span(),
            );
            // Recover as well as we can, by still type-checking the body so
            // later errors don't pile up.
            self.open_block();
            let _ = self.compile_expr(&expr.body);
            self.close_block();
            return (self.c.engine.fresh_var(), CKind::Error);
        };

        let expected = match &err_var {
            Some(e) => self.c.ty_result(success_var, *e),
            None => self.c.ty_option(success_var),
        };
        self.c
            .engine
            .unify_at(expected, left_ty, expr.expression.span());

        // The failure branch is the recovery body, with the `Err` payload bound
        // to the receiver when there is one.
        self.open_block();
        let mut receiver = None;
        if let Some(e) = err_var
            && let Some(recv) = &expr.receiver
        {
            let sid = self.c.engine.intern(&recv.name);
            receiver = Some((sid, self.bind_mono(&recv.name, e, recv.span)));
        }
        let body = self.compile_expr(&expr.body);
        self.close_block();
        self.c
            .engine
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

    /// Typecheck a function body inside an open `enter_fn_frame` and
    /// `finish_fn_frame` pair, with its parameters already bound, and park it.
    /// This is the one meeting point between the typecheck
    /// [`Self::compile_expr`] walk and the Core IR pipeline, and it *hands the
    /// body off* instead of calling the pipeline. The walk is where inference,
    /// diagnostics, hover facts and reference-graph collection happen, and it
    /// lowers nothing at all.
    ///
    /// In pass 6, once the whole module has been walked, the parked body goes
    /// through `typed_ir::elaborate_body` →
    /// [`core_ir::lower`](crate::core_ir::lower) → `perceus`. There's no
    /// fallback path and no error path. The elaborator covers every form a
    /// [`CleanModule`] can contain, so it returns a [`TypedFn`] and not a
    /// `Result`.
    ///
    /// Elaboration and lowering run under `check_only` too. They are the only
    /// passes that can prove a well-typed program really compiles, so
    /// `scarlet check` must not skip them.
    ///
    /// The Core pipeline doesn't run here at all. The body is parked in
    /// `deferred_bodies` and elaborated once the whole module has been walked
    /// (pass 6 of `analyse_module`). So `lower` reads solved types, not
    /// variables inference hasn't pinned down yet, and sees the module as a
    /// whole instead of one SCC at a time.
    ///
    /// Returns `(body_ty, parked)`. See [`ParkedBody`].
    fn compile_fn_body(
        &mut self,
        params: &[ast::FunctionParameter],
        param_tys: &[Ty],
        param_binders: &[BinderId],
        body: &ast::Expression,
    ) -> (Ty, ParkedBody) {
        // The typecheck walk. Nested closures found here come back into this
        // function (through `compile_function_common`), park their own bodies
        // and reserve their slots before we reserve ours.
        let checked = self.compile_expr(body);
        let body_ty = checked.ty;
        debug_assert!(
            self.defer_depth > 0,
            "every function body must be walked inside a deferral region: \
             the Core pipeline runs after the walk, never during it"
        );
        // A body that failed to typecheck is parked like any other and closed
        // out empty when the bodies are drained, because `CleanModule` can't be
        // minted for it. The elaborator is never shown a subtree the checker
        // rejected.
        let name = self
            .frames
            .current
            .self_name
            .map(frame::SelfName::name)
            .unwrap_or_else(|| self.c.engine.intern("__anon__"));
        let param_binds: Vec<(StrId, BinderId, Ty)> = params
            .iter()
            .zip(param_binders)
            .zip(param_tys)
            .map(|((p, &binder), &ty)| (self.c.engine.intern(&p.identifier.name), binder, ty))
            .collect();
        let parked = ParkedBody {
            name,
            param_binds,
            body: checked,
            body_ty,
        };
        (body_ty, parked)
    }

    /// Elaborate one body that has already been typechecked, lower the whole
    /// `TypedProgram` it produces, and write the bodies of every eta wrapper
    /// the elaborator added to it.
    ///
    /// **This is the only way into the Core pipeline.** It is the only place in
    /// this compiler that calls `typed_ir::elaborate_body` or
    /// `elaborate_toplevel`, and those two are the only things that build a
    /// [`TypedFn`]. So they are also the only things that build the
    /// [`TypedProgram`] that `lower` and `perceus` take. Taking a
    /// [`CleanModule`] here closes the whole pipeline to a module that reported
    /// an error. That isn't a convention: a poisoned module just can't produce
    /// the value the passes take. (That is also why `lower` needs no proof of
    /// its own.)
    ///
    /// `at` says where the elaborated function belongs in the program it is
    /// lowered as part of. It is `Some(func_idx)` for a function body, whose
    /// reserved slot fixes its [`FuncIdx`], and `None` for a module toplevel,
    /// which is `TypedProgram::toplevel` and has no index. Either way, the
    /// returned [`LoweredBody`] carries its `CoreFn`, and the eta wrappers the
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
        let im = self.c.immediates();
        for f in &mut fns {
            crate::core_ir::immediates::rewrite(f, &im);
        }
        // The names come from the typed program, index for index, because
        // `lower` keeps `fns` in the same order as `TypedProgram::fns`.
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

    /// Elaborate one body into a whole-module [`TypedProgram`], and reserve a
    /// function slot for every eta wrapper the walk minted.
    ///
    /// `TypedProgram::fns` is indexed by `FuncIdx`, and so is [`Self::fns`], so
    /// the two must agree. `fns` is padded up to `Self::fns.len()` before the
    /// walk (`FnTable::push` mints each `FuncIdx` in the order it adds them),
    /// and each wrapper added past that point gets its slot reserved here, in
    /// order. Nothing else may reserve a slot while the walk runs.
    fn elaborate(
        &mut self,
        at: Option<crate::core_ir::FuncIdx>,
        build: impl FnOnce(&mut Self, &mut ResolvedPool, &mut FnTable) -> TypedFn,
    ) -> Elaborated {
        let eta_base = self.c.fns.len();
        let mut pool = pool_for(&self.c.engine);
        // `RTy`s name nodes in `pool`, so the memo goes when the previous pool
        // does.
        self.c.rty_cache.clear();
        let temps = TempTys::intern(self, &mut pool);
        let nil_ty = self.c.ty_nil();
        let nil = PreludeTys::resolve_rty(self, &mut pool, nil_ty);
        // The `fns` entries an earlier body already owns. They are never
        // lowered into anything the caller reads. They only exist so the next
        // `FnTable::push` lands on the slot reserved for it below.
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

        if self.c.fns.len() != eta_base {
            function_reserved_during_elaboration();
        }
        for _ in fns.tail_from(FuncIdx::from_usize(eta_base)) {
            self.c.fns.push(None);
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

    /// Run Perceus on the eta wrappers `fns[base..]` and fill the slots that
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
            self.c.fns[FuncIdx::from_usize(base + i)] =
                Some(self.lowered_fn(name, core, Rc::clone(pool)));
        }
    }
}

impl Compiler {
    /// The prelude constructors that Core IR writes as immediates.
    fn immediates(&self) -> crate::core_ir::immediates::Immediates {
        crate::core_ir::immediates::Immediates {
            true_: self.prelude.true_().into(),
            false_: self.prelude.false_().into(),
            nil: self.prelude.nil_ctor().into(),
        }
    }
}

impl ModuleChecker<'_> {
    /// Package a finished body for the [`Program`]. This is called while the
    /// module that owns the body is the one being compiled, which is what makes
    /// `current_module_key` its module.
    fn lowered_fn(&self, name: StrId, core: CoreFn, pool: Rc<ResolvedPool>) -> LoweredFn {
        LoweredFn::new(
            self.current_module_key.to_string(),
            self.c.engine.str(name).to_string(),
            core,
            pool,
        )
    }

    /// Run the Core pipeline (elaborate → `lower` → `perceus`) over one body
    /// that has already been typechecked, and fill its function slot.
    ///
    /// This runs in pass 6, after the whole module has been walked, and only
    /// [`Self::elaborate_deferred`] calls it. `func_idx` is the slot the walk
    /// reserved. Under `check_only` the pipeline stops after `lower` and the
    /// slot stays empty.
    ///
    /// The `CleanModule` is the caller's proof that this body typechecked (that
    /// nothing in the module failed to), because a poisoned body has no types
    /// to elaborate.
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
        if self.c.check_only {
            return;
        }
        let core = perceus::perceus(&pool, core);
        if std::env::var("CORE_DBG").is_ok() {
            eprintln!("=== {}\n{core}", self.c.engine.str(name));
        }
        self.c.fns[func_idx] = Some(self.lowered_fn(name, core, pool));
    }

    /// Open the elaboration phase boundary. Every function body walked until
    /// the matching [`Self::end_deferred_elaboration`] is parked instead of
    /// elaborated, and the Core pipeline runs over all of them at once there.
    ///
    /// Exactly two places open one, and between them they cover every body the
    /// compiler ever walks. One is `analyse_module`, around the whole walk of
    /// pass 5 (the SCC loop *and* the toplevel bindings and bare expressions
    /// after it). The other is `compile_impl`'s program made of a bare
    /// expression. `compile_fn_body` asserts it is inside one. The assertion is
    /// only a backstop, because [`ParkedBody`] leaves it nothing else to
    /// return.
    ///
    /// It is a boundary around the whole module and not each SCC, because
    /// `lower(p: &TypedProgram) -> CoreProgram` needs that to exist. Lowering
    /// can't be a step of the typecheck walk if what the walk produces is the
    /// module.
    ///
    /// A body's types are also only final once its SCC has been inferred,
    /// `leave_level` has run and `generalize_top` has quantified what stayed
    /// free. Draining at the end of the module is always later than that.
    ///
    /// Here is exactly what this gets us. After `generalize_top`, a body
    /// variable that inference never solved has a `Generic` root instead of an
    /// `Unbound` one, so `lower` can never see a variable that is about to be
    /// quantified away underneath it. That is what keeps zonking honest,
    /// because a `Generic` root maps to a bound index instead of an invented
    /// opaque `Bound`. It doesn't measurably win any opcodes. On the T0
    /// workload set the mix of opcodes is the same (no typed ops recovered),
    /// because `compile_binary` already unifies both operands during the walk.
    /// The cases the reordering newly resolves, a variable pinned by a later
    /// function in the same SCC or by the `unify_at(ret_ty, body_ty)` after the
    /// body, do exist, but they don't change any opcodes there.
    ///
    /// What deferral does *not* change is which slot a declared body owns.
    /// `finish_fn_frame` reserves those during the walk, in walk order, in both
    /// `check` and `build`.
    pub(super) fn begin_deferred_elaboration(&mut self) {
        self.defer_depth += 1;
    }

    /// Close the region opened by [`Self::begin_deferred_elaboration`]. At
    /// depth zero, run the Core pipeline over every parked body in walk order,
    /// innermost closure first. This is the module's whole `lower` → `perceus`
    /// phase: one loop, after the typecheck walk, over every body the walk
    /// produced.
    pub(super) fn end_deferred_elaboration(&mut self) {
        self.defer_depth -= 1;
        if self.defer_depth > 0 {
            return;
        }
        let bodies = std::mem::take(&mut self.deferred_bodies);
        for d in bodies {
            // Prove this again for each body, not once for the whole run.
            // `elaborate_deferred` uses up the proof, and an internal error
            // raised while lowering one body poisons the module for the next.
            //
            // If there's no proof, a declaration in the module failed to
            // typecheck. The parked bodies may refer to names inference never
            // resolved, and there's no typed IR to lower. Their slots stay
            // empty, and a module with an error builds no program.
            if let Some(clean) = self.c.clean_module() {
                self.elaborate_deferred(d, clean);
            }
        }
    }

    /// Elaborate a parked body. Everything it needs to know about names and
    /// closures is on the checked tree, so no frame state is restored for it.
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

    /// Compile a `fn(...) { ... }` expression. `param_hints` is `Some` when the
    /// lambda is passed straight to a call whose parameter types are known.
    /// Then any parameter without an annotation gets the hinted type instead of
    /// a fresh variable, so the body can access fields and so on straight away.
    fn compile_function_common(
        &mut self,
        params: &[ast::FunctionParameter],
        body: &ast::Expression,
        return_annot: Option<&ast::TypeIdentifier>,
        param_hints: Option<Vec<Ty>>,
    ) -> (Ty, ClosureFact) {
        let saved = self.enter_fn_frame(None);

        // The type variables in a lambda's annotations share a scope with each
        // other but belong to the lambda, so mint them with a fresh hydrator.
        let mut hydrator = Hydrator::new(AnnotationContext::Signature);
        let mut param_tys: Vec<Ty> = Vec::with_capacity(params.len());
        let mut binders: Vec<BinderId> = Vec::with_capacity(params.len());
        for (i, param) in params.iter().enumerate() {
            let p_ty = match &param.typ {
                Some(annot) => self.hydrate(&mut hydrator, annot),
                None => match &param_hints {
                    Some(h) if i < h.len() => h[i],
                    _ => self.c.engine.fresh_var(),
                },
            };
            param_tys.push(p_ty);
            binders.push(self.bind_param(param, p_ty));
        }
        // Any type variables the hydrator minted are rigid in the body.
        self.frames
            .current
            .rigid
            .extend(hydrator.rigid_ids().iter().copied());

        let (body_ty, body_emit) = self.compile_fn_body(params, &param_tys, &binders, body);
        let ret_ty = match return_annot {
            Some(rt) => {
                let annot_ty = self.hydrate(&mut hydrator, rt);
                self.c
                    .engine
                    .unify_at(annot_ty, body_ty, type_defining_span(body));
                annot_ty
            }
            None => body_ty,
        };

        // `finish_fn_frame` restored the enclosing frame, so the closure is
        // what that frame sees, and that frame's elaboration builds it.
        let closure = self.finish_fn_frame(saved, body_emit);
        (self.c.engine.mk_fun(&param_tys, ret_ty), closure)
    }

    /// Compile a top-level `fn` whose parameter and return types were already
    /// hydrated in Pass 3. The `hydrator` passed in carries the rigid generic
    /// ids for any annotated type variables, so `instantiate` inside the body
    /// leaves them alone, and we unify the body's inferred type against the
    /// shape registered earlier instead of reading the annotations again.
    ///
    /// `global_slot` is this function's entry-frame slot from `Prepared::Fn`,
    /// passed down from Pass 3. So `global_to_func` is keyed by the same slot
    /// the caller emits `StoreLocal` for, without relying on the name's local
    /// entry still holding that slot when this runs.
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
        self.c
            .engine
            .unify_at(ret_ty, body_ty, type_defining_span(body));

        // `global_to_func` is how the module toplevel's elaboration finds this
        // function's body (`ElabCtx::fn_of_global`, from the same slot it
        // stores into), in every mode. So the index has to be a real one under
        // `check_only` too. `finish_fn_frame` reserved it, so take it from
        // there instead of working it out from `fns.len()`, which is only the
        // same number because nothing else happened to reserve a slot in
        // between.
        let func_idx = self.finish_fn_frame(saved, body_emit).func_idx;
        self.c.global_to_func.insert(global_slot, func_idx);
        self.c.engine.mk_fun(&param_tys, ret_ty)
    }

    /// Start walking a nested body. A top-level `fn` calls itself by its
    /// `decl` name.
    ///
    /// A lambda has no name to call itself by unless it is the initialiser of a
    /// `name = fn(..)` binding, which leaves that name in `next_fn_self_name`.
    /// It must not inherit the enclosing body's name, or a lambda passed to a
    /// higher-order function that calls the enclosing `fn` would call itself.
    fn enter_fn_frame(&mut self, decl: Option<&str>) -> frame::Entered {
        let self_name = match decl {
            Some(n) => Some(frame::SelfName::Decl(self.c.engine.intern(n))),
            None => self
                .next_fn_self_name
                .take()
                .map(|(n, ty)| frame::SelfName::Lambda(n, ty)),
        };
        self.frames.enter(self_name)
    }

    fn bind_param(&mut self, p: &ast::FunctionParameter, ty: Ty) -> BinderId {
        self.bind_mono(&p.identifier.name, ty, p.identifier.span)
    }

    /// Close out a function body. This reserves its function slot, parks it as
    /// a [`DeferredBody`], and restores the enclosing frame and type env. It
    /// returns the closure as the enclosing frame sees it: the slot, and how
    /// each capture loads there, which goes on the enclosing frame's checked
    /// tree.
    ///
    /// Nothing is lowered here. The body is lowered in pass 6, after the walk.
    fn finish_fn_frame(&mut self, entered: frame::Entered, parked: ParkedBody) -> ClosureFact {
        let mut finished = self.frames.leave(entered);
        let captured = std::mem::take(&mut finished.captures).into_names();
        self.report_unused(finished.into_root());

        // Reserve the body's slot now, so the `func_idx` that the `ClosureFact`
        // and `global_to_func` are about to record is the one the elaborated
        // body fills in.
        let func_idx = self.c.fns.push(None);
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

        // Resolving each capture again in the *enclosing* frame matters.
        // `resolve_name` moves a name this body captured from further out
        // into the enclosing frame's own capture set, so captures chain
        // outwards through every level. It is also how each capture loads where
        // the closure is built, so the result is what the elaborator reads.
        let captures = captured
            .iter()
            .map(|&cap_name| {
                let (at, ty) = match self.resolve_name(cap_name) {
                    Some((scheme, at)) => (
                        at,
                        self.c
                            .engine
                            .instantiate(&scheme, &self.frames.current.rigid),
                    ),
                    None => (NameAt::Unbound, self.c.engine.fresh_var()),
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
        let result_ty = self.c.engine.fresh_var();
        let mut any_pattern_err = false;
        let mut b = PatternBindings::new();
        let mut arms = Vec::with_capacity(m.arms.len());

        for arm in &m.arms {
            self.open_block();

            b.clear();
            let (pattern, ok) =
                self.type_pattern(&arm.pattern, subject_ty, &mut b.sink(), &mut Vec::new());
            any_pattern_err |= !ok;

            let guard = arm.guard.as_ref().map(|guard| {
                let g = self.compile_expr(guard);
                let bool_ty = self.c.ty_bool();
                self.c
                    .engine
                    .unify_at(bool_ty, g.ty, type_defining_span(guard));
                g
            });

            let body = self.compile_expr(&arm.body);
            self.c
                .engine
                .unify_at(result_ty, body.ty, type_defining_span(&arm.body));

            self.close_block();
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
                self.c
                    .engine
                    .resolve_for_patterns(subject_ty, Some(&self.c.env), depth);
            let mut um = UsefulnessMatrix::new(resolved_subj);
            let all_pats: Vec<Pat> = arms.iter().map(|arm| um.lower(&arm.pattern)).collect();
            // Guarded arms don't count towards exhaustiveness (the guard may be
            // false), and don't make later arms unreachable.
            let unguarded = m
                .arms
                .iter()
                .zip(&all_pats)
                .filter(|(arm, _)| arm.guard.is_none())
                .map(|(_, p)| p);
            if let Some(missing) = um.find_missing(unguarded) {
                self.c.error(
                    format!("Match is not exhaustive. Add arms for: {}", missing),
                    m.subject.span(),
                );
            }
            for (i, arm) in m.arms.iter().enumerate() {
                if !um.is_useful(&all_pats[i]) {
                    self.c.error(
                        "This pattern is unreachable, because an earlier pattern matches the same values"
                            .to_string(),
                        arm.pattern.span(),
                    );
                } else if let Some(missing) = um.catch_all(&all_pats[..i], &all_pats[i]) {
                    // A catch-all may only stand for values that can't be
                    // listed. Over named variants it would quietly take any
                    // variant added later, so the arms have to name them.
                    self.c.error(
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
}

impl Compiler {
    /// The one place that turns a number literal's source text into a
    /// [`Const`]. On i64 overflow or bad input, it emits a real diagnostic with
    /// [`Compiler::error`] and then returns a zero of the same kind so
    /// compiling can carry on. That zero can ONLY be reached after the
    /// diagnostic, so it can never pass for a valid literal `0`, because the
    /// compile has already failed.
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

/// A module's imports, which are the `import` statements it starts with.
fn import_decls(block: &ast::BlockExpression) -> impl Iterator<Item = &ast::ImportDeclaration> {
    block.body.iter().map_while(|node| match node {
        ast::Node::Statement(stmt) => match stmt.as_ref() {
            ast::Statement::ImportDeclaration(imp) => Some(imp),
            _ => None,
        },
        ast::Node::Expression(_) => None,
    })
}

/// The span that "defines" the type of an expression, for error reporting. For
/// a block it is the last node (recursively), and otherwise the expression's
/// own span. This points a return-type or branch-type mismatch at the part that
/// actually produces the value, not the whole `{ ... }`.
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

/// Why a number literal's source text couldn't be turned into a [`Const`].
///
/// The scanner produces decimal (`[0-9]+(\.[0-9]+)?`), hex
/// (`0[xX][0-9A-Fa-f]+`) or binary (`0[bB][01]+`), and the parser only adds a
/// leading `-` for negative literals in patterns. The integer branch fails on
/// i64 overflow. Hex and binary are magnitudes, so `0x8000000000000000` is out
/// of range and isn't i64::MIN. The float branch in practice never fails for
/// scanner output, but we keep `InvalidFloat` so the function handles every
/// `&str` instead of quietly depending on what the scanner produces.
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

    /// The constant to use instead, so compiling can carry on after the
    /// diagnostic has been emitted. It keeps the kind, so the inferred type
    /// still matches what the user meant and the compile doesn't go on to raise
    /// bogus type errors.
    fn recovery(&self) -> Const {
        match self {
            NumLitError::IntOutOfRange => Const::Int(0),
            NumLitError::InvalidFloat => Const::Float(0.0),
        }
    }
}

/// Parse a number literal's source text into a [`Const`].
///
/// It handles every input, because failure is part of the return type, so no
/// caller can get a made-up constant for input out of range. The only caller is
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
