//! AST → [`Program`]: Hindley–Milner type inference fused with bytecode
//! emission in a single traversal.
//!
//! There is no separate typed IR and no second walk — `compile_expr` infers
//! a node's type and emits its instructions in the same call. The fusion is
//! the point: a type that unification just resolved is immediately
//! available to pick typed opcodes (`AddInt` over `Add`), pattern codegen
//! consults variant layouts mid-emission, and every diagnostic span comes
//! from the one traversal that saw the source. Module top level is the
//! exception: declarations are mutually recursive and order-free, so
//! `analysis.rs` runs its multi-pass declaration analysis first and hands
//! each body back to this file's pass.
//!
//! # Map of the file
//!
//! - **Reference-graph scaffolding** (`RawRef` → [`HoverFact`]): every name
//!   occurrence is buffered with its live `Ty` during the pass and lowered
//!   into the workspace `ReferenceGraph` once all unifications have settled.
//! - **[`Compiler`] + entry points** ([`compile`] / [`check`] /
//!   [`check_as_module`]): all codegen, scoping, and inference state in one
//!   struct, documented field by field.
//! - **Incremental recompilation** ([`Watermark`] / `reset_to`): a snapshot
//!   is six lengths, a rollback is six truncations.
//! - **[`IncrementalSession`]**: the LSP/workspace layer — owns a
//!   `Compiler` across edits, invalidates cached modules, answers
//!   hover/goto-def/find-refs/rename from the reference graph.
//! - **The pass itself** (`compile_node` / `compile_expr` and friends): the
//!   bulk of the file, one method per AST form.
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

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use super::{
    Function, Instruction, Op, PreludeBindings, Program, Value, enum_name_prefix_hash, op, op_ab,
    op_arg,
};
use crate::ast;
use crate::diagnostic::{Diagnostic, has_errors};
use crate::frozen::FrozenBuilder;
use indexmap::IndexMap;

use crate::module::{
    self, CachedModule, ModuleInterface, ModulePath, ModuleSource, ModuleTable, ResolveError,
    source_hash,
};
use crate::reference::{
    DefId, Definition, ModuleId, ModuleInterner, ModuleReferences, ReferenceGraph, span_contains,
    span_width,
};
use crate::span::Span;
use crate::token::Kind;
use crate::type_def::Type;
use crate::types::{
    ArenaSlice, Constraint, DefinitionLocation, EnginePoolWatermark, EntityKind, EnvWatermark,
    Hydrator, InferEngine, MatchFunTypeError, Pat, PatternBindings, Prim, Scheme, StrId, Ty,
    TypeEnv, TypeNode, UsefulnessMatrix, ValueKind, check_exhaustiveness, mono, new_engine,
    new_env, pattern_to_pat,
};

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
struct RawRef {
    span: Span,
    name: String,
    ty: Ty,
    doc: Option<String>,
    /// Interned id of the module this occurrence was recorded in. Resolved
    /// once at `record` time (memoised), so `finalize_references` builds the
    /// `HoverFact` without re-interning the path per occurrence.
    module: ModuleId,
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
// CompileResult
// ============================================================================

#[derive(Debug)]
pub struct CompileResult {
    pub program: Program,
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
struct Scope {
    locals: HashMap<String, LocalSlot>,
}

/// A live local binding: the stack slot it occupies plus the block-scope
/// nesting depth (`scope_marks.len()`) at which it was bound. The depth lets
/// `get_or_create_local` decide in O(1) whether a re-binding shadows an
/// enclosing scope (depth strictly shallower than the current scope ⇒ a fresh
/// slot must be allocated so the outer value survives the block) or simply
/// rebinds within the current scope (reuse the slot).
#[derive(Clone, Copy, Debug)]
struct LocalSlot {
    slot: i32,
    depth: u32,
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
    captures: HashMap<String, i32>,
    capture_names: Vec<String>,
    rigid_ids: HashSet<i32>,
    binding: String,
    tail: bool,
    jump_over: i32,
    func_start: i32,
}

pub struct Compiler {
    // --- Codegen state ---
    program: Program,
    /// Append handle to `program`'s frozen area.
    /// Every constant `Value` the compiler builds — literals, enum
    /// construction/match headers, folded binaries — is constructed through
    /// this builder (the `const_*` helpers below), never via bare `Value`
    /// constructors, so enum names and field labels from compile-time
    /// constants all point at the area's canonical interned allocations.
    frozen: FrozenBuilder,
    locals: HashMap<String, LocalSlot>,
    /// Scoped-symbol-table undo log. Every mutation of `locals` made inside an
    /// open block scope appends `(name, previous entry)`; `pop_local_scope`
    /// unwinds back to the entering scope's mark, restoring (or removing) each
    /// touched name. Replaces snapshotting the whole `locals` map on every
    /// block/if-arm/match-arm/lambda entry: cost is now O(bindings actually
    /// shadowed) instead of O(scopes × live locals).
    undo_log: Vec<(String, Option<LocalSlot>)>,
    /// `undo_log` length captured at each `push_local_scope`; `pop_local_scope`
    /// unwinds the log down to the popped mark.
    scope_marks: Vec<usize>,
    /// While emitting the 2nd..nth alternative of an or-pattern, maps each name
    /// bound by the 1st alternative to its slot so every alternative stores the
    /// binding into the same place (one logical binding, one slot). Without
    /// this, module scope's shadow-gets-a-fresh-slot rule would leave the arm
    /// body reading whichever slot the *last* alternative allocated — an
    /// unwritten slot whenever an earlier alternative was the one that matched.
    /// A stack because or-patterns nest.
    or_bind_overrides: Vec<HashMap<String, i32>>,
    /// Per-scope unused-binding tracking. Each frame maps a let/param/match
    /// name (that doesn't start with `_`) to its definition span; `mark_used`
    /// removes the entry on first reference. Anything left when the frame is
    /// popped is reported as an error. Frames are pushed/popped in lockstep
    /// with `scope_marks` (blocks) and `enter_fn_frame`/`finish_fn_frame`
    /// (params), so a use inside a nested closure marks the outer binding by
    /// walking the whole stack.
    unused: Vec<HashMap<String, Span>>,
    outer_scopes: Vec<Scope>,
    local_count: i32,
    captures: HashMap<String, i32>,
    capture_names: Vec<String>,
    current_binding: String,
    /// One-shot self-name handed to the next `enter_fn_frame(None)` so a
    /// `name = fn(...)` binding's lambda can self-recurse, without leaking the
    /// enclosing fn's binding into unrelated (e.g. HOF-arg) lambdas.
    next_fn_self_name: Option<String>,
    in_tail_position: bool,
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
    recorded: Vec<RawRef>,
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
    collect_hover_facts: bool,
    // --- Module state ---
    pub(super) module_table: ModuleTable,
    /// Append-only `ModulePath` ↔ `ModuleId` interner backing every `DefId`.
    /// Persistent across `reset_to`/incremental recompiles (like
    /// `module_table`) so a `DefId` minted in one `check` still resolves after
    /// later modules are added or evicted; ids are reused across recompiles.
    pub(super) ref_interner: ModuleInterner,
    pub(super) current_module: ModulePath,
    /// Memo of `current_module` interned into `engine.str_slices`; invalidated
    /// (set to `None`) whenever `current_module` is swapped in
    /// `compile_module_body`.
    pub(super) module_path_slice: Option<ArenaSlice>,
    /// Memo of a module-path `ArenaSlice` → its interned `ModuleId`. The
    /// `str_slices` pool is append-only within a compile, so a given
    /// `ArenaSlice` always denotes the same path (hence the same id); cleared
    /// in `reset_to`, the single point where the pool is rewound. Lets
    /// `defid_of`/`record` skip the per-occurrence `strs_of` + `path_key`
    /// allocations and string-hash probe on the per-keystroke recompile.
    pub(super) defid_module_memo: HashMap<ArenaSlice, ModuleId>,
    /// Qualifier name in this file → module key (path.join("/")).
    imported_qualifiers: HashMap<String, String>,
    base_dir: Option<PathBuf>,
    pub(super) prelude: PreludeBindings,
    /// Names user code may not redefine. Populated from the prelude module's
    /// interface (every exported type and constructor) so the set is derived
    /// from `al.al` rather than mirrored in Rust. `@vm` functions are
    /// deliberately excluded — `println` is shadowable.
    pub(super) reserved: HashSet<String>,
    /// When the binary is built with the static stdlib, this is the
    /// `&'static StaticStdlib` handle; lazy-hydrate fallthrough lookups consult
    /// it on a runtime-map miss. `None` for the from-source path
    /// (`register_prelude`, `precompile_stdlib`, LSP-editing-stdlib).
    static_stdlib: Option<&'static crate::static_ir::StaticStdlib>,
}

enum VarAccess {
    Local(i32),
    Capture(i32),
    Global(i32),
    Self_,
}

/// Outcome of resolving a `module.member` property-access shape against the
/// imported qualifiers. Three-state because the callee and value positions
/// diverge on the failure modes: a non-qualified shape is recoverable (the
/// caller falls through to its general path), whereas a failed member lookup
/// has already been diagnosed and must short-circuit.
enum QualifiedMember {
    /// Not a `qualifier.member` shape (or the qualifier is not imported).
    NotQualified,
    /// A qualified shape whose member lookup failed; `lookup_module_member`
    /// has already emitted the diagnostic.
    LookupFailed,
    Resolved {
        module_key: String,
        member_name: String,
        member_span: Span,
        scheme: Scheme,
        load: Option<VarAccess>,
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
        locals: HashMap::new(),
        undo_log: vec![],
        scope_marks: vec![],
        or_bind_overrides: vec![],
        unused: vec![],
        outer_scopes: vec![],
        local_count: 0,
        captures: HashMap::new(),
        capture_names: vec![],
        current_binding: String::new(),
        next_fn_self_name: None,
        in_tail_position: false,
        current_owner: None,
        engine: new_engine(),
        env: new_env(),
        rigid_ids: HashSet::new(),
        recorded: vec![],
        module_refs: main_refs,
        check_only,
        collect_hover_facts: false,
        module_table: ModuleTable::new(),
        ref_interner,
        current_module: module::main_module(),
        module_path_slice: None,
        defid_module_memo: HashMap::new(),
        imported_qualifiers: HashMap::new(),
        base_dir: base_dir.map(|p| p.to_path_buf()),
        prelude: PreludeBindings::default(),
        reserved: HashSet::new(),
        static_stdlib: None,
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
            pick(
                self.module_table.module_refs(&module::path_key(path))?,
                name,
            )
        }
    }
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
            return CompileResult {
                program: c.program,
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
    if let ast::Expression::BlockExpression(block) = expr {
        c.process_imports(block);
        c.bump_type_ids_past_reserved();
        c.analyse_module(block, None);
    } else {
        c.compile_expr(expr);
    }
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
        fuse(&mut c.program.code);
    }

    let (references, _facts) = c.finalize_references();
    let success = !has_errors(&c.engine.diagnostics);
    CompileResult {
        program: c.program,
        diagnostics: c.engine.diagnostics,
        references,
        success,
    }
}

/// Peephole superinstruction fusion. Linearly scans the fully-emitted code
/// stream and rewrites the hottest local+const Int sequences into single
/// dispatches:
///
///   PushLocal s; PushConst k; SubInt              → SubIntLC{a:s, b:k}
///   PushLocal s; PushConst k; AddInt              → AddIntLC{a:s, b:k}
///   PushLocal s; PushConst k; LtInt; JumpIfFalse t → JumpGeIntLC{a:s, b:k, operand:t}
///   PushLocal s; PushConst k; EqInt; JumpIfFalse t → JumpNeIntLC{a:s, b:k, operand:t}
///
/// The first slot of each window is overwritten with the fused op and the
/// remaining slots become `Nop`, so absolute jump targets (which the compiler
/// emits as `code_start + ip` and the VM reads back as `operand - code_start`)
/// stay valid without relocation. A window whose interior — every slot after
/// the head — is itself a jump target is left unfused: a branch landing on a
/// Nop would skip the fused effect entirely.
pub(super) fn fuse(code: &mut [Instruction]) {
    // Absolute addresses anything branches to. Only the head of a fused window
    // may be a target; landing mid-window after fusion would be incorrect.
    let mut targets: HashSet<i32> = HashSet::new();
    for instr in code.iter() {
        if matches!(
            instr.op,
            Op::Jump | Op::JumpIfFalse | Op::JumpIfTrue | Op::JumpGeIntLC | Op::JumpNeIntLC
        ) {
            targets.insert(instr.operand);
        }
    }

    let nop = op(Op::Nop);
    let len = code.len();
    let mut i = 0usize;
    while i + 2 < len {
        let i0 = code[i];
        let i1 = code[i + 1];
        if i0.op != Op::PushLocal
            || i1.op != Op::PushConst
            || i0.operand as u32 > u8::MAX as u32
            || i1.operand as u32 > u16::MAX as u32
            || targets.contains(&((i + 1) as i32))
            || targets.contains(&((i + 2) as i32))
        {
            i += 1;
            continue;
        }
        let (slot, konst) = (i0.operand as u8, i1.operand as u16);
        let i2 = code[i + 2];

        // 4-wide: PushLocal; PushConst; {Lt,Eq}Int; JumpIfFalse
        if i + 3 < len && !targets.contains(&((i + 3) as i32)) {
            let i3 = code[i + 3];
            if i3.op == Op::JumpIfFalse {
                let fused = match i2.op {
                    Op::LtInt => Some(Op::JumpGeIntLC),
                    Op::EqInt => Some(Op::JumpNeIntLC),
                    _ => None,
                };
                if let Some(f) = fused {
                    code[i] = op_ab(f, slot, konst, i3.operand);
                    code[i + 1] = nop;
                    code[i + 2] = nop;
                    code[i + 3] = nop;
                    i += 4;
                    continue;
                }
            }
        }

        // 3-wide: PushLocal; PushConst; {Add,Sub}Int
        let fused = match i2.op {
            Op::AddInt => Some(Op::AddIntLC),
            Op::SubInt => Some(Op::SubIntLC),
            _ => None,
        };
        if let Some(f) = fused {
            code[i] = op_ab(f, slot, konst, 0);
            code[i + 1] = nop;
            code[i + 2] = nop;
            i += 3;
            continue;
        }

        i += 1;
    }
}

// ============================================================================
// Incremental recompilation
// ============================================================================

/// Full snapshot of every append-only compiler structure, captured at module
/// boundaries so an `IncrementalSession` can roll back exactly to that point.
/// Ordering follows `EnginePoolWatermark` so `min()` over a set of watermarks
/// picks the earliest-compiled one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Watermark {
    pub engine: EnginePoolWatermark,
    pub env: EnvWatermark,
    pub code: usize,
    pub functions: usize,
    pub constants: usize,
    pub local_count: i32,
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
        self.unused.clear();
        self.outer_scopes.clear();
        self.captures.clear();
        self.capture_names.clear();
        self.current_binding.clear();
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
}

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
    graph: std::rc::Rc<crate::reference::ReferenceGraph>,
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
            graph: std::rc::Rc::new(crate::reference::ReferenceGraph::new()),
            type_facts: Vec::new(),
        }
    }

    pub fn compile_count(&self) -> u32 {
        self.c.module_table.compile_count
    }

    pub fn reference_graph(&self) -> &crate::reference::ReferenceGraph {
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
            .find(|(_, cm)| cm.source_path.as_deref() == Some(path))
            .map(|(_, cm)| &cm.iface.path)
    }

    /// Evict the cached module compiled from `path` (and its dependents) so the
    /// next `check()` recompiles it. Called from LSP `didChangeWatchedFiles`
    /// when a file changes on disk outside the editor. Drops any overlay for
    /// the path so disk content is re-read, and lowers `last_entry` to the
    /// evicted module's watermark so the arena is truncated correctly.
    pub fn invalidate_path(&mut self, path: &Path) {
        self.c.module_table.overlays.remove(path);
        let key = self
            .c
            .module_table
            .user_modules()
            .find(|(_, cm)| cm.source_path.as_deref() == Some(path))
            .map(|(k, _)| k.clone());
        if let Some(k) = key
            && let Some(w) = self.c.module_table.invalidate(&k)
        {
            let floor = self.last_entry.map_or(w, |le| le.min(w)).max(self.seed);
            self.last_entry = Some(floor);
        }
    }

    pub fn set_overlay(&mut self, path: PathBuf, text: String) {
        self.c.module_table.overlays.insert(path, text);
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
        if self.c.module_table.id_range_overflow
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
}

// ============================================================================
// IncrementalSession — hover query over the session's type-fact table. All
// other LSP queries (goto-def / find-refs / rename / symbols) are answered
// directly from the [`ReferenceGraph`] by the LSP server.
// ============================================================================

impl IncrementalSession {
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
    pub fn module_id_base(&self, key: &str) -> Option<i32> {
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
            .filter(|f| f.module == m && span_contains(&f.span, line, col))
            .min_by_key(|f| span_width(&f.span))?;
        Some((f.name.clone(), f.ty.clone(), f.doc.clone()))
    }
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
    pub(crate) fn into_parts(
        self,
    ) -> (
        (Program, PreludeBindings, HashSet<String>, i32, i32),
        InferEngine,
    ) {
        (
            (
                self.program,
                self.prelude,
                self.reserved,
                self.env.next_type_id(),
                self.local_count,
            ),
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
        self.module_table.static_fallback = Some(s);
        self.prelude = s.prelude.clone();
        self.engine.set_prim_ids(crate::types::PrimIds {
            int: self.prelude.int.id,
            float: self.prelude.float.id,
            string: self.prelude.string.id,
        });
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
            self.bind_local(format!("__pre{}", slot), slot);
        }

        // Stdlib type-infos: copy eagerly so both the by-name map (annotation
        // resolution) and the by-id registry (nominal lookups: exhaustiveness,
        // field access, hover) hit without hydration. Seeded below the session
        // watermark, so they are never truncated and never overwritten in
        // place (a colliding user type gets its own id and its own entry).
        for (name, idx) in s.typeinfo_by_name {
            let ti = s.typeinfos[*idx as usize];
            self.env.type_info.insert((*name).to_string(), ti);
            self.env.type_info_by_id.insert(ti.id, ti);
        }

        // Re-export prelude constructor/@vm schemes (Some/None/Ok/Err/True/...)
        // into root scope.
        let key = module::path_key(&module::al_prelude());
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
    // Codegen primitives (no-op in check_only mode)
    // ========================================================================

    #[inline]
    pub(super) fn emit(&mut self, o: Op) {
        if self.check_only {
            return;
        }
        self.program.code.push(op(o));
    }

    #[inline]
    pub(super) fn emit_arg(&mut self, o: Op, operand: i32) {
        if self.check_only {
            return;
        }
        self.program.code.push(op_arg(o, operand));
    }

    #[inline]
    pub(super) fn emit_b(&mut self, o: Op, b: u16, operand: i32) {
        if self.check_only {
            return;
        }
        self.program.code.push(op_ab(o, 0, b, operand));
    }

    /// Emit a forward jump with a placeholder target and return its address
    /// for later `patch_jump`.
    fn emit_jump(&mut self, o: Op) -> i32 {
        let addr = self.current_addr();
        self.emit_arg(o, 0);
        addr
    }

    #[inline]
    fn patch_jump(&mut self, addr: i32) {
        if self.check_only {
            return;
        }
        self.program.code[addr as usize].operand = self.current_addr();
    }

    fn patch_all(&mut self, addrs: &[i32]) {
        for &a in addrs {
            self.patch_jump(a);
        }
    }

    fn current_addr(&self) -> i32 {
        self.program.code.len() as i32
    }

    fn add_constant(&mut self, v: Value) -> i32 {
        if self.check_only {
            return 0;
        }
        self.program.constants.push(v);
        self.program.constants.len() as i32 - 1
    }

    // Frozen constant-pool helpers: every constant `Value` is built through
    // `self.frozen` and then pooled. Check-only
    // mode skips the build like `add_constant` skips the pool.

    /// Pool a frozen Int constant.
    fn const_int(&mut self, i: i64) -> i32 {
        if self.check_only {
            return 0;
        }
        let v = self.frozen.int(i);
        self.add_constant(v)
    }

    /// Pool a frozen string constant (interned: every pool entry with the
    /// same contents shares one frozen allocation).
    fn const_str(&mut self, s: &str) -> i32 {
        if self.check_only {
            return 0;
        }
        let v = self.frozen.str(s);
        self.add_constant(v)
    }

    /// Pool a frozen field-label array for the interned label ids in
    /// `field_labels`. Interned as a unit, so every construction site of the
    /// same variant shares one frozen label array.
    fn const_str_array(&mut self, field_labels: ArenaSlice) -> i32 {
        if self.check_only {
            return 0;
        }
        let labels: Vec<&str> = self
            .engine
            .str_ids_of(field_labels)
            .iter()
            .map(|&l| self.engine.str(l))
            .collect();
        let v = self.frozen.str_array(&labels);
        self.add_constant(v)
    }

    /// Pool a frozen binary constant of `bit_len` bits.
    fn const_binary(&mut self, bytes: Vec<u8>, bit_len: u64) -> i32 {
        if self.check_only {
            return 0;
        }
        let v = self.frozen.binary_bits(bytes, bit_len);
        self.add_constant(v)
    }

    pub(super) fn get_or_create_local(&mut self, name: &str) -> i32 {
        // A name bound by the first alternative of an in-progress or-pattern
        // must reuse that slot in every later alternative (see
        // `or_bind_overrides`); the shadowing rules below are for *sequential*
        // rebindings, which alternatives are not.
        if let Some(overrides) = self.or_bind_overrides.last()
            && let Some(&slot) = overrides.get(name)
        {
            return slot;
        }
        if let Some(entry) = self.locals.get(name).copied() {
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
        self.bind_local(name.to_string(), idx);
        idx
    }

    /// Bind `name` to `slot` in the current scope, recording the displaced
    /// entry on the undo log so `pop_local_scope` can restore it. Outside any
    /// open block scope there is nothing to unwind, so the log entry is
    /// skipped (the binding is captured by the next scope's mark instead).
    fn bind_local(&mut self, name: String, slot: i32) {
        let entry = LocalSlot {
            slot,
            depth: self.scope_marks.len() as u32,
        };
        let prev = self.locals.insert(name.clone(), entry);
        if !self.scope_marks.is_empty() {
            self.undo_log.push((name, prev));
        }
    }

    #[inline]
    pub(super) fn alloc_temp(&mut self) -> i32 {
        let idx = self.local_count;
        self.local_count += 1;
        idx
    }

    #[inline]
    pub(super) fn spill_temp(&mut self) -> i32 {
        let idx = self.alloc_temp();
        self.emit_arg(Op::StoreLocal, idx);
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
            // captured at the matching push, so it never exceeds the log; the
            // clamp only guards a hypothetical unbalanced edge.
            let mark = mark.min(self.undo_log.len());
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

    /// Record a let/param/match binding for unused-variable checking. Names
    /// starting with `_` are exempt. If the same name was already tracked in
    /// this scope and never used, that earlier binding is reported now (it's
    /// been shadowed and can no longer be referenced).
    pub(super) fn track_binding(&mut self, name: &str, sp: Span) {
        if name.starts_with('_') {
            return;
        }
        let prev = self
            .unused
            .last_mut()
            .and_then(|s| s.insert(name.to_string(), sp));
        if let Some(prev_sp) = prev {
            self.error(
                format!("'{name}' is unused; prefix with '_' to ignore"),
                prev_sp,
            );
        }
    }

    /// Mark a name as used, searching innermost scope outward so a closure
    /// referencing an enclosing binding clears it there.
    fn mark_used(&mut self, name: &str) {
        for scope in self.unused.iter_mut().rev() {
            if scope.remove(name).is_some() {
                return;
            }
        }
    }

    fn pop_unused_scope(&mut self) {
        let Some(scope) = self.unused.pop() else {
            return;
        };
        let mut leftover: Vec<(String, Span)> = scope.into_iter().collect();
        leftover.sort_by_key(|(_, sp)| (sp.start_line, sp.start_column));
        for (name, sp) in leftover {
            self.error(format!("'{name}' is unused; prefix with '_' to ignore"), sp);
        }
    }

    fn emit_load(&mut self, access: &VarAccess) {
        match access {
            VarAccess::Local(idx) => self.emit_arg(Op::PushLocal, *idx),
            VarAccess::Capture(idx) => self.emit_arg(Op::PushCapture, *idx),
            VarAccess::Global(idx) => self.emit_arg(Op::PushGlobal, *idx),
            VarAccess::Self_ => self.emit(Op::PushSelf),
        }
    }

    fn resolve_variable(&mut self, name: &str) -> Option<VarAccess> {
        self.mark_used(name);
        if let Some(entry) = self.locals.get(name) {
            return Some(VarAccess::Local(entry.slot));
        }
        if let Some(idx) = self.captures.get(name) {
            return Some(VarAccess::Capture(*idx));
        }
        // Search enclosing scopes innermost-first so inner bindings shadow
        // outer ones. The bottom of the stack (index 0) is always the entry
        // frame: its locals live at absolute stack indices and can be loaded
        // directly with PushGlobal instead of being captured by value. This
        // is what makes mutually-recursive top-level fns work — the sibling's
        // slot is read at call time, not at MakeClosure time.
        for (i, scope) in self.outer_scopes.iter().enumerate().rev() {
            if let Some(entry) = scope.locals.get(name) {
                if name == self.current_binding {
                    return Some(VarAccess::Self_);
                }
                if i == 0 {
                    return Some(VarAccess::Global(entry.slot));
                }
                let capture_idx = self.capture_names.len() as i32;
                self.captures.insert(name.to_string(), capture_idx);
                self.capture_names.push(name.to_string());
                return Some(VarAccess::Capture(capture_idx));
            }
        }
        None
    }

    // ========================================================================
    // Diagnostics & LSP recording
    // ========================================================================

    pub(super) fn error(&mut self, msg: String, sp: Span) {
        self.engine.diagnostics.push(Diagnostic::error(sp, msg));
    }

    pub(super) fn note(&mut self, msg: String, sp: Span) {
        self.engine.diagnostics.push(Diagnostic::hint(sp, msg));
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
    /// segment) plus `ref_interner.intern` (`path_key` joined `String` + a
    /// string-hash probe) with one `ArenaSlice`-keyed probe on a cache hit.
    fn module_id_of_slice(&mut self, sl: ArenaSlice) -> ModuleId {
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
    fn defid_of(&mut self, dl: DefinitionLocation) -> DefId {
        let module = self.module_id_of_slice(dl.module);
        DefId::new(module, def_loc_span(&dl), dl.entity)
    }

    /// The [`DefId`] to stash in `current_owner` while a top-level
    /// `fn`/`const`/`type` body is compiled. Every reference emitted inside
    /// that body — including inside nested lambdas, which have no `DefId` of
    /// their own — is attributed to this owner: the def→def edge channel the
    /// dead-code reachability walk follows.
    pub(super) fn owner_defid(&mut self, name_span: Span, entity: EntityKind) -> DefId {
        let module = self.current_module_slice();
        self.defid_of(def_loc(name_span, module, entity))
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
    fn record_ref(&mut self, occ: Span, kind: crate::reference::ReferenceKind, target: DefId) {
        self.module_refs.add_reference(
            self.current_owner,
            crate::reference::Reference::new(occ, kind, target),
        );
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
        kind: crate::reference::ReferenceKind,
    ) {
        if let Some(dl) = def {
            let target = self.defid_of(dl);
            self.record_ref(occ, kind, target);
        }
    }

    /// Resolve a type name in `path`'s module to its canonical `Type`
    /// definition and record an occurrence of `kind` at `occ`. No-op when the
    /// module has no such type (e.g. static/hydrated stdlib modules). Mirror of
    /// [`Self::record_value_use`] for type references.
    fn record_type_use(
        &mut self,
        path: &ModulePath,
        name: &str,
        occ: Span,
        kind: crate::reference::ReferenceKind,
    ) {
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
    ) {
        let defid = self.defid_of(dl);
        let first = self.module_refs.definition(defid).is_none();
        self.module_refs
            .add_definition(crate::reference::Definition::new(defid, name, doc, is_pub));
        if first {
            self.module_refs.add_reference(
                Some(defid),
                crate::reference::Reference::new(
                    defid.span,
                    crate::reference::ReferenceKind::Definition,
                    defid,
                ),
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
            self.emit_def(def_loc(sp, m, EntityKind::Value), name, doc, false);
        }
    }

    // ========================================================================
    // Construction header / Nil emission
    // ========================================================================

    /// Push the four header constants `[type_id, type_name, variant_name,
    /// field_labels]` onto the stack. The VM `MakeEnumPayload` op pops payloads
    /// from the top then the header beneath them, so the header must be on the
    /// stack BEFORE argument compilation. Construction-only: match sites compare
    /// just type id + variant and push the slim two-item `emit_match_header`
    /// instead. Caller emits `MakeEnumPayload n` after pushing the args.
    fn emit_construct_header(
        &mut self,
        type_id: i32,
        type_name: &str,
        variant_name: &str,
        field_labels: ArenaSlice,
    ) {
        let id_c = self.const_int(type_id as i64);
        self.emit_arg(Op::PushConst, id_c);
        let en_c = self.const_str(type_name);
        self.emit_arg(Op::PushConst, en_c);
        let vn_c = self.const_str(variant_name);
        self.emit_arg(Op::PushConst, vn_c);
        let lc = self.const_str_array(field_labels);
        self.emit_arg(Op::PushConst, lc);
    }

    /// Construction-only header: the four-item `emit_construct_header`, then a
    /// constant carrying `enum_name_prefix_hash(type_name, variant_name)`. The
    /// names are compile-time constants, so the hash is computed once here and
    /// the VM folds only the payload hashes in at runtime. Unlike the rest of
    /// the header it is *not* `PushConst`-ed onto the stack: its constant-pool
    /// index is returned and threaded into `MakeEnumPayload`'s `operand`, so
    /// the VM reads it by reference (no extra opcode dispatch, no stack
    /// push/pop, no per-construction refcount churn). Match sites keep the
    /// plain four-item header — they compare type id / variant and never
    /// recompute a hash.
    fn emit_construct_header_for_build(
        &mut self,
        type_id: i32,
        type_name: &str,
        variant_name: &str,
        field_labels: ArenaSlice,
    ) -> i32 {
        self.emit_construct_header(type_id, type_name, variant_name, field_labels);
        let prefix = enum_name_prefix_hash(type_name, variant_name);
        self.const_int(prefix as i64)
    }

    /// Push the slim two-item match header `[type_id, variant_name]`. `MatchEnum`
    /// only compares the scrutinee's type id and variant name, so unlike the
    /// construction header it needs neither the enum name nor the field-label
    /// array. The VM pops `variant_name`, then `type_id`, then the scrutinee.
    fn emit_match_header(&mut self, type_id: i32, variant_name: &str) {
        let id_c = self.const_int(type_id as i64);
        self.emit_arg(Op::PushConst, id_c);
        let vn_c = self.const_str(variant_name);
        self.emit_arg(Op::PushConst, vn_c);
    }

    /// Push the prelude `Nil` value. Replaces every site that previously
    /// emitted `PushNone`: block tail-is-statement, no-match fallthrough, and
    /// the implicit return of a builtin that yields no value. The VM constructs
    /// the tagged value from `program.prelude.nil`.
    fn emit_nil(&mut self) {
        self.emit(Op::PushNil);
    }

    /// `Bool` is a real two-ctor type at the language level but its runtime
    /// representation stays the unboxed `Value::Bool` so `if`/`JumpIfFalse`/
    /// `&&`/`||` remain a single branch. Construction: emit `PushTrue`/`PushFalse`
    /// instead of the `MakeEnum` header. Match: emit a `Value::Bool` literal +
    /// `Eq` instead of `MatchEnum`. Returns `true` when it handled `type_id`.
    fn try_emit_bool_value(&mut self, type_id: i32, variant_name: &str) -> bool {
        if !self.prelude.is_bool(type_id) {
            return false;
        }
        let is_true = variant_name == super::prelude_bindings::names::TRUE;
        self.emit(if is_true { Op::PushTrue } else { Op::PushFalse });
        true
    }

    fn try_emit_bool_match(
        &mut self,
        type_id: i32,
        variant_name: &str,
        fail_jumps: &mut Vec<i32>,
    ) -> bool {
        if !self.prelude.is_bool(type_id) {
            return false;
        }
        let is_true = variant_name == super::prelude_bindings::names::TRUE;
        self.emit(if is_true { Op::PushTrue } else { Op::PushFalse });
        self.emit(Op::Eq);
        fail_jumps.push(self.emit_jump(Op::JumpIfFalse));
        true
    }

    /// Thin wrappers over the engine's `mk_con` for prelude types, fed by the
    /// captured `PreludeBindings` so the type identity (not just the name
    /// string) is the source of truth where it matters.
    fn ty_bool(&mut self) -> Ty {
        self.engine
            .mk_con(self.prelude.bool.id, self.prelude.bool.name, &[])
    }
    fn ty_int(&mut self) -> Ty {
        self.engine
            .mk_con(self.prelude.int.id, self.prelude.int.name, &[])
    }
    fn ty_string(&mut self) -> Ty {
        self.engine
            .mk_con(self.prelude.string.id, self.prelude.string.name, &[])
    }
    fn ty_nil(&mut self) -> Ty {
        self.engine
            .mk_con(self.prelude.nil.id, self.prelude.nil.name, &[])
    }
    fn ty_array(&mut self, elem: Ty) -> Ty {
        self.engine
            .mk_con(self.prelude.array.id, self.prelude.array.name, &[elem])
    }
    fn ty_binary(&mut self) -> Ty {
        self.engine
            .mk_con(self.prelude.binary.id, self.prelude.binary.name, &[])
    }
    fn ty_option(&mut self, inner: Ty) -> Ty {
        self.engine
            .mk_con(self.prelude.option.id, self.prelude.option.name, &[inner])
    }
    fn ty_result(&mut self, ok: Ty, err: Ty) -> Ty {
        self.engine
            .mk_con(self.prelude.result.id, self.prelude.result.name, &[ok, err])
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
            self.record_type_use(
                &path,
                &hit.name,
                hit.span,
                crate::reference::ReferenceKind::Unqualified,
            );
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

    /// Hydrate an annotation through a fresh hydrator that does *not* permit
    /// new type variables. Used for `let x: T = ...` annotations, where any
    /// type-variable name must already be in scope (i.e. a parameter of the
    /// enclosing fn).
    fn hydrate_annotation(&mut self, t: &ast::TypeIdentifier) -> Ty {
        let mut h = Hydrator::new();
        h.disallow_new_type_variables();
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
                let defer_slot = self.outer_scopes.is_empty() && self.locals.contains_key(&name);
                let idx = if defer_slot {
                    None
                } else {
                    Some(self.get_or_create_local(&name))
                };

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
                    self.next_fn_self_name.replace(name.clone())
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
                    def_loc(vb.identifier.span, m, EntityKind::Value),
                );
                self.track_binding(&name, vb.identifier.span);
                if let Some(doc) = &vb.doc {
                    self.env.store_doc(&name, doc.clone());
                }
                self.record(&name, final_ty, vb.identifier.span, vb.doc.clone());
                self.emit_value_def(vb.identifier.span, &name, vb.doc.clone());

                let idx = match idx {
                    Some(i) => i,
                    None => self.get_or_create_local(&name),
                };
                self.emit_arg(Op::StoreLocal, idx);
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
                    self.type_pattern(pattern, elem_vars[i], &mut b);
                }
                self.bind_pattern_initials(&b);

                let temp = self.spill_temp();

                for (i, pattern) in tdb.patterns.iter().enumerate() {
                    self.emit_arg(Op::PushLocal, temp);
                    self.emit_arg(Op::TupleIndex, i as i32);
                    let mut fails: Vec<i32> = vec![];
                    self.emit_pattern(pattern, &mut fails);
                    if !fails.is_empty() {
                        self.error(
                            "Destructuring binding pattern must be irrefutable".to_string(),
                            pattern.span(),
                        );
                        self.patch_all(&fails);
                    }
                }
            }
            ast::Statement::Declaration(d) | ast::Statement::PublicDeclaration(d) => {
                // Top-level declarations are handled by `analyse_module`; reaching
                // one here means it's nested inside an expression block.
                let (kind, sp) = match d.as_ref() {
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
                // Statement position: the asserted value is evaluated for its
                // effects then dropped.
                self.emit(Op::Pop);
            }
            ast::Statement::CtorDestructuringBinding(cdb) => {
                // `Ctor(p1, ..) = expr` — an irrefutable single-arm destructure.
                // Lowered like a one-arm match: type the pattern against the
                // init, bind its names, then require the single pattern to be
                // exhaustive (so only single-constructor types qualify; a
                // multi-constructor or nested-optional pattern is refutable).
                let init_ty = self.compile_expr(&cdb.init);

                let mut b = PatternBindings::new();
                let typed_ok = self.type_pattern(&cdb.pattern, init_ty, &mut b);

                self.bind_pattern_initials(&b);

                if typed_ok {
                    let resolved = self.engine.resolve(init_ty, Some(&self.env));
                    let pat = pattern_to_pat(&cdb.pattern, &resolved);
                    if let Some(missing) = check_exhaustiveness(&[pat], &resolved) {
                        self.error(
                            format!(
                                "constructor destructuring binding must be irrefutable; \
                                 pattern does not cover {missing}"
                            ),
                            cdb.span,
                        );
                    }
                }

                let temp = self.spill_temp();
                self.emit_arg(Op::PushLocal, temp);
                let mut fails: Vec<i32> = vec![];
                self.emit_pattern(&cdb.pattern, &mut fails);
                self.patch_all(&fails);
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

    fn process_imports(&mut self, block: &ast::BlockExpression) {
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
    fn bump_type_ids_past_reserved(&mut self) {
        let hw = self.module_table.id_high_water();
        let cur = self.env.next_type_id();
        if hw > cur {
            self.env.set_next_type_id(hw);
        }
    }

    fn process_import(&mut self, imp: &ast::ImportDeclaration) {
        let key = module::path_key(&imp.path);

        if !self.load_module(&imp.path, imp.span) {
            return;
        }

        let qualifier = imp
            .alias
            .as_ref()
            .map(|a| a.name.clone())
            .unwrap_or_else(|| {
                imp.path
                    .iter()
                    .rev()
                    .find(|s| s.as_str() != "." && s.as_str() != "..")
                    .cloned()
                    .unwrap_or_else(|| key.clone())
            });

        // Reference graph: the qualifier binding is a `ModuleAlias`
        // definition; the final module-name path segment is an `Import`
        // occurrence and the `as` alias an `Alias` occurrence, both targeting
        // it so qualified `q.member` uses, find-references, and rename resolve
        // back to this import (and the unused-import reachability rule can see
        // it). The `Import` occ is recorded at `imp.path_span` — the final
        // segment only — so the `import` keyword and earlier path segments
        // (`al` / `/` in `import al/string`) are not clickable; the alias
        // definition keeps the full `alias_span` so selective-import dead-code
        // detection still derives the declaration's extent from it.
        let alias_span = imp.alias.as_ref().map_or(imp.span, |a| a.span);
        let cur_path = self.current_module.clone();
        let cur_mid = self.ref_interner.intern(&cur_path);
        let alias_defid = DefId::new(cur_mid, alias_span, EntityKind::ModuleAlias);
        self.module_refs.add_definition(Definition::new(
            alias_defid,
            qualifier.clone(),
            None,
            false,
        ));
        // The module-name segment points at the *imported* module, so its
        // `Import` occurrence targets a `DefId` owned by that module: goto-def
        // on the `b` in `import a/b` lands on `b`'s file, not the importing
        // alias's own binding (a no-op self-jump). find-references / rename / the
        // unused-import rule all key off the alias `Definition` above
        // (`has_real_use` derives the declaration extent from it), so retargeting
        // only the occurrence changes nothing for them.
        let imported_mid = self.ref_interner.intern(&imp.path);
        self.module_refs.add_reference(
            None,
            crate::reference::Reference::new(
                imp.path_span,
                crate::reference::ReferenceKind::Import,
                DefId::new(imported_mid, imp.path_span, EntityKind::ModuleAlias),
            ),
        );
        if let Some(a) = imp.alias.as_ref() {
            self.module_refs.add_reference(
                None,
                crate::reference::Reference::new(
                    a.span,
                    crate::reference::ReferenceKind::Alias,
                    alias_defid,
                ),
            );
        }

        self.imported_qualifiers.insert(qualifier, key.clone());

        // Per-item occurrences resolved through the imported module's
        // interface. Collected with owned `DefinitionLocation`s / type names
        // while `iface` borrows `module_table`, then turned into `DefId`s and
        // recorded once that borrow has ended.
        let mut item_refs: Vec<(Span, crate::reference::ReferenceKind, DefinitionLocation)> =
            Vec::new();
        let mut type_item_refs: Vec<(Span, crate::reference::ReferenceKind, String)> = Vec::new();
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
                // to the real declaration, and renaming X rewrites it.
                if let Some(dl) = vdef {
                    item_refs.push((
                        item.name.span,
                        crate::reference::ReferenceKind::Unqualified,
                        dl,
                    ));
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
                    let alias_dl = def_loc(a.span, module, EntityKind::Value);
                    self.env.define(
                        &local_name,
                        Scheme {
                            def: Some(alias_dl),
                            ..ev.scheme
                        },
                    );
                    let alias_defid = self.defid_of(alias_dl);
                    let canonical = vdef.map(|dl| self.defid_of(dl));
                    let mut def = Definition::new(alias_defid, local_name.clone(), None, false);
                    // goto-def / hover on Y chains to X's real declaration; rename
                    // and find-references stay on this alias.
                    def.alias_of = canonical;
                    self.module_refs.add_definition(def);
                    item_refs.push((a.span, crate::reference::ReferenceKind::Alias, alias_dl));
                } else {
                    self.env.define(&local_name, ev.scheme);
                }
                if let Some(slot) = ev.local_slot {
                    self.bind_local(local_name.clone(), slot);
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
                    crate::reference::ReferenceKind::Unqualified,
                    item.name.name.clone(),
                ));
                if let Some(a) = item.alias.as_ref() {
                    type_item_refs.push((
                        a.span,
                        crate::reference::ReferenceKind::Alias,
                        item.name.name.clone(),
                    ));
                }
            }
            if val.is_none() && typ.is_none() {
                if is_private {
                    self.error(
                        format!("'{}' is private in module '{}'", item.name.name, key),
                        item.name.span,
                    );
                } else {
                    self.error(
                        format!("Module '{}' has no member '{}'", key, item.name.name),
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
            self.record_type_use(&imp.path, &tyname, occ, kind);
        }
    }

    pub(crate) fn load_module(&mut self, path: &ModulePath, at: Span) -> bool {
        let key = module::path_key(path);
        let importer = module::path_key(&self.current_module);
        if self.module_table.get_or_hydrate(&key).is_some() {
            self.module_table.record_dependent(&key, &importer);
            return true;
        }
        if self.module_table.is_loading(&key) {
            self.error(format!("Import cycle detected at module '{key}'"), at);
            return false;
        }

        let source = match module::resolve(path, self.base_dir.as_deref()) {
            Ok(s) => s,
            Err(ResolveError::NotFound(p)) => {
                self.error(format!("Unknown module '{p}' (not found)"), at);
                return false;
            }
            Err(ResolveError::NoBaseDir) => {
                self.error(
                    "Relative imports are not allowed without a file context (e.g. REPL)"
                        .to_string(),
                    at,
                );
                return false;
            }
        };

        let (text, child_base, source_path): (String, Option<PathBuf>, Option<PathBuf>) =
            match source {
                ModuleSource::Embedded(s) => (s.to_string(), None, None),
                ModuleSource::File(p) => {
                    let read = self
                        .module_table
                        .read_source(&p)
                        .or_else(|| std::fs::read_to_string(&p).ok());
                    match read {
                        Some(t) => (t, p.parent().map(|d| d.to_path_buf()), Some(p)),
                        None => {
                            self.error(format!("Failed to read module '{key}'"), at);
                            return false;
                        }
                    }
                }
            };

        let hash = source_hash(&text);
        let mut sc = crate::scanner::new_scanner(text);
        let mut parser = crate::parser::new_parser(&mut sc);
        let parsed = parser.parse_program();
        for d in parsed.diagnostics {
            self.engine.diagnostics.push(Diagnostic {
                span: at,
                severity: d.severity,
                message: format!("In module '{key}': {}", d.message),
            });
        }

        self.module_table.mark_loading(&key);
        let (iface, imports) = self.compile_module_body(path.clone(), &parsed.ast, child_base);
        // Watermark is captured *after* this module's own dependencies have
        // loaded (compile_module_body does that first) but reflects the arena
        // state immediately *before* this module's body added anything. We
        // can't observe that point directly post-hoc, so compile_module_body
        // returns it via the iface side-channel.
        let watermark = source_path.as_ref().map(|_| iface.watermark);
        self.module_table.compile_count += 1;
        self.module_table.insert_cached(
            key.clone(),
            CachedModule {
                iface: iface.iface,
                source_hash: hash,
                watermark,
                source_path,
                // Reference-graph data collected while this module's body was
                // analysed; persisting it alongside `iface`/`watermark` lets a
                // cross-module reference into this module survive an unrelated
                // recompile.
                module_refs: Some(Rc::new(iface.refs)),
                dependents: HashSet::new(),
            },
        );
        for imp in imports {
            self.module_table.record_dependent(&imp, &key);
        }
        self.module_table.record_dependent(&key, &importer);
        true
    }

    fn compile_module_body(
        &mut self,
        path: ModulePath,
        block: &ast::BlockExpression,
        base_dir: Option<PathBuf>,
    ) -> (CompiledBody, Vec<String>) {
        let mut iface = ModuleInterface::new(path.clone());
        let mid = self.ref_interner.intern(&path);

        let old_module = std::mem::replace(&mut self.current_module, path);
        let old_mod_slice = self.module_path_slice.take();
        let old_qualifiers = std::mem::take(&mut self.imported_qualifiers);
        let old_base = std::mem::replace(&mut self.base_dir, base_dir);
        // Swap in a fresh collector for this submodule's body (mirrors the
        // `current_module`/`imported_qualifiers` save/restore dance); the
        // typecheck pass populates it and we move the result into the module's
        // `CachedModule` below.
        let old_refs = std::mem::replace(&mut self.module_refs, ModuleReferences::new(mid));

        self.process_imports(block);
        let imports: Vec<String> = self.imported_qualifiers.values().cloned().collect();
        // Reserve (or reuse) this module's stable 256-aligned type-id range
        // *before* the watermark is captured, so the captured watermark's
        // `env.next_type_id == base`. On invalidation `reset_to`/`truncate_to`
        // restores `next_type_id` from that watermark, putting the module back
        // at its own range start — editing one module never shifts another's
        // ids. Deps have already loaded (and reserved their ranges) via the
        // `process_imports` recursion above, so the floor (current
        // `next_type_id`) already sits past every stdlib + dependency id.
        // `path` was moved into `self.current_module` above and
        // `process_imports` save/restores it, so it still holds this module's
        // own path here — the same key `load_module` caches this module under.
        let key = module::path_key(&self.current_module);
        let floor = self.env.next_type_id();
        let (base, reused) = self.module_table.id_base_for(&key, floor);
        self.env.set_next_type_id(base);
        let watermark = self.watermark();
        self.analyse_module(block, Some(&mut iface));
        // Record actual type-id consumption so `id_high_water` tracks real
        // usage and a reused-range spill past `MODULE_TYPE_ID_RANGE` raises
        // `id_range_overflow` for the invalidate-all/reset recovery path.
        let used = self.env.next_type_id() - base;
        self.module_table.note_id_usage(base, used, reused);

        self.current_module = old_module;
        self.module_path_slice = old_mod_slice;
        self.imported_qualifiers = old_qualifiers;
        self.base_dir = old_base;
        let refs = std::mem::replace(&mut self.module_refs, old_refs);
        (
            CompiledBody {
                iface,
                watermark,
                refs,
            },
            imports,
        )
    }

    /// Look up a `qualifier.member` reference where `qualifier` is an imported
    /// module name. Returns the value's scheme and (if it has a runtime
    /// binding) its slot in the entry frame.
    fn lookup_module_member(
        &mut self,
        module_key: &str,
        member: &str,
        member_span: Span,
    ) -> Option<(Scheme, Option<i32>)> {
        let Some(iface) = self.module_table.get_or_hydrate(module_key) else {
            self.error(format!("Module '{module_key}' is not loaded"), member_span);
            return None;
        };
        if let Some(ev) = iface.values.get(member).cloned() {
            return Some((ev.scheme, ev.local_slot));
        }
        if iface.private_names.contains(member) {
            self.error(
                format!("'{member}' is private in module '{module_key}'"),
                member_span,
            );
        } else {
            self.error(
                format!("Module '{module_key}' has no member '{member}'"),
                member_span,
            );
        }
        None
    }

    // ========================================================================
    // Expressions
    // ========================================================================

    pub(super) fn compile_expr(&mut self, expr: &ast::Expression) -> Ty {
        let is_tail = self.in_tail_position;
        self.in_tail_position = false;

        match expr {
            ast::Expression::BinaryLiteral(bl) => self.compile_binary_literal(bl),
            ast::Expression::BlockExpression(be) => {
                self.env.push_scope();
                self.push_local_scope();
                let mut last_ty = self.ty_nil();

                for (i, node) in be.body.iter().enumerate() {
                    let is_last = i == be.body.len() - 1;
                    self.in_tail_position = is_tail && is_last;
                    let ty = self.compile_node(node);
                    self.in_tail_position = false;

                    if is_last {
                        last_ty = ty;
                    } else if matches!(node, ast::Node::Expression(_)) {
                        let resolved = self.engine.find(ty);
                        let is_nil = matches!(
                            self.engine.node(resolved),
                            TypeNode::Con { id, .. } if id == self.prelude.nil.id
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
                        self.emit(Op::Pop);
                    }
                }

                if !matches!(be.body.last(), Some(ast::Node::Expression(_))) {
                    self.emit_nil();
                }

                self.pop_local_scope();
                self.env.pop_scope();
                last_ty
            }
            ast::Expression::NumberLiteral(nl) => {
                let v = self.const_number(nl);
                let ty = if v.is_float() {
                    self.engine.icon_float()
                } else {
                    self.ty_int()
                };
                let idx = self.add_constant(v);
                self.emit_arg(Op::PushConst, idx);
                ty
            }
            ast::Expression::StringLiteral(sl) => {
                let idx = self.const_str(sl.value.as_str());
                self.emit_arg(Op::PushConst, idx);
                self.ty_string()
            }
            ast::Expression::InterpolatedString(is) => {
                if is.parts.is_empty() {
                    let idx = self.const_str("");
                    self.emit_arg(Op::PushConst, idx);
                } else {
                    for part in &is.parts {
                        self.compile_expr(part);
                        self.emit(Op::ToString);
                    }
                    if is.parts.len() > 1 {
                        self.emit_arg(Op::StrConcatN, is.parts.len() as i32);
                    }
                }
                self.ty_string()
            }
            ast::Expression::Identifier(id) => self.compile_identifier(id),
            ast::Expression::BinaryExpression(be) => self.compile_binary(be),
            ast::Expression::UnaryExpression(ue) => {
                let inner_ty = self.compile_expr(&ue.expression);
                match ue.op.kind {
                    Kind::PuncExclamationMark => {
                        let b = self.ty_bool();
                        self.engine.unify_at(b, inner_ty, ue.expression.span());
                        self.emit(Op::Not);
                        self.ty_bool()
                    }
                    Kind::PuncMinus => {
                        let result = self.engine.fresh_constrained_var(Constraint::Numeric);
                        self.engine.unify_at(result, inner_ty, ue.expression.span());
                        let opc = match self.engine.resolved_prim(result) {
                            Some(Prim::Int) => Op::NegInt,
                            Some(Prim::Float) => Op::NegFloat,
                            _ => Op::Neg,
                        };
                        self.emit(opc);
                        result
                    }
                    _ => {
                        self.error(format!("Unknown unary operator: {}", ue.op.kind), ue.span);
                        self.engine.fresh_var()
                    }
                }
            }
            ast::Expression::IfExpression(ie) => {
                let cond_ty = self.compile_expr(&ie.condition);
                let b = self.ty_bool();
                self.engine.unify_at(b, cond_ty, ie.condition.span());

                let else_jump = self.emit_jump(Op::JumpIfFalse);

                self.in_tail_position = is_tail;
                let then_ty = self.compile_expr(&ie.body);
                self.in_tail_position = false;

                let end_jump = self.emit_jump(Op::Jump);

                self.patch_jump(else_jump);

                self.in_tail_position = is_tail;
                let else_ty = self.compile_expr(&ie.else_body);
                self.in_tail_position = false;
                self.patch_jump(end_jump);

                let ret = self.engine.fresh_var();
                self.engine
                    .unify_at(ret, then_ty, type_defining_span(&ie.body));
                self.engine
                    .unify_at(ret, else_ty, type_defining_span(&ie.else_body));
                ret
            }
            ast::Expression::MatchExpression(me) => self.compile_match(me, is_tail),
            ast::Expression::ArrayExpression(ae) => self.compile_array(ae),
            ast::Expression::TupleExpression(te) => {
                let mut elem_tys: Vec<Ty> = Vec::with_capacity(te.elements.len());
                for elem in &te.elements {
                    elem_tys.push(self.compile_expr(elem));
                }
                self.emit_arg(Op::MakeTuple, te.elements.len() as i32);
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
                    self.emit(Op::ArraySlice);
                    self.ty_array(elem_var)
                } else {
                    let idx_ty = self.compile_expr(&aie.index);
                    let int_t = self.ty_int();
                    self.engine.unify_at(int_t, idx_ty, aie.index.span());
                    self.emit(Op::Index);
                    self.ty_option(elem_var)
                }
            }
            ast::Expression::RangeExpression(re) => {
                let start_ty = self.compile_expr(&re.start);
                let end_ty = self.compile_expr(&re.end);
                let int_t = self.ty_int();
                self.engine.unify_at(int_t, start_ty, re.start.span());
                self.engine.unify_at(int_t, end_ty, re.end.span());
                self.emit(Op::MakeRange);
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
                );
                self.engine.leave_level();
                ty
            }
            ast::Expression::FunctionCallExpression(fc) => self.compile_call(fc, is_tail),
            ast::Expression::PropertyAccessExpression(pa) => self.compile_property_access(pa),
            ast::Expression::OrExpression(oe) => self.compile_or(oe),
            ast::Expression::ErrorNode(_) => self.engine.fresh_var(),
        }
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
                self.engine.enter_level();
                let ty = self.compile_function_common(
                    &fe.params,
                    &fe.body,
                    fe.return_type.as_ref(),
                    Some(params.clone()),
                );
                self.engine.leave_level();
                return ty;
            }
        }
        self.compile_expr(expr)
    }

    // ========================================================================
    // Identifier
    // ========================================================================

    /// Resolve the runtime load for a value reference: `Local`/`ModuleFn` push a
    /// variable (with the usual `mark_used` + capture side effects), while
    /// constructors and builtins have no plain runtime binding.
    fn value_load(&mut self, name: &str, kind: &ValueKind) -> Option<VarAccess> {
        match kind {
            ValueKind::Local | ValueKind::ModuleFn => self.resolve_variable(name),
            _ => None,
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
            self.emit_nil();
            return self.engine.fresh_var();
        };

        let ty = self.engine.instantiate(scheme, &self.rigid_ids);
        let kind = scheme.kind;
        let def = scheme.def;
        // `record` discards everything on the non-LSP path (see its early
        // return); skip the doc map probe + clone entirely there.
        let doc = self.doc_if_collecting(name);
        self.record(name, ty, expr.span, doc);
        self.record_value_use(def, expr.span, crate::reference::ReferenceKind::Unqualified);

        let load = self.value_load(name, &kind);
        self.emit_named_value(name, None, expr.span, kind, load);
        ty
    }

    fn no_runtime_binding(&mut self, name: &str, qualifier: Option<&str>, sp: Span) {
        let msg = match qualifier {
            Some(m) => format!("'{name}' in module '{m}' has no runtime binding"),
            None => format!("'{name}' has a type but no runtime binding here"),
        };
        self.error(msg, sp);
        self.emit_nil();
    }

    fn emit_named_value(
        &mut self,
        name: &str,
        qualifier: Option<&str>,
        sp: Span,
        kind: ValueKind,
        load: Option<VarAccess>,
    ) {
        match kind {
            ValueKind::Constructor {
                type_name,
                type_id,
                arity,
                field_labels,
                ..
            } => self.emit_ctor_as_value(type_id, type_name, name, field_labels, arity),
            ValueKind::Builtin { .. } => self.error_builtin_as_value(name, sp),
            ValueKind::Local | ValueKind::ModuleFn => match load {
                Some(a) => self.emit_load(&a),
                None => self.no_runtime_binding(name, qualifier, sp),
            },
        }
    }

    /// Emit a constructor referenced as a first-class value (e.g. `map(Some, xs)`):
    /// nullary builds the tagged value inline, n-ary synthesises a closure.
    fn emit_ctor_as_value(
        &mut self,
        type_id: i32,
        type_name: StrId,
        variant: &str,
        labels: ArenaSlice,
        arity: u16,
    ) {
        let type_name = self.engine.str(type_name).to_string();
        if arity == 0 {
            if self.try_emit_bool_value(type_id, variant) {
                return;
            }
            let ph = self.emit_construct_header_for_build(type_id, &type_name, variant, labels);
            self.emit_b(Op::MakeEnumPayload, 0, ph);
        } else {
            self.emit_ctor_closure(type_id, &type_name, variant, labels, arity as i32);
        }
    }

    fn error_builtin_as_value(&mut self, name: &str, sp: Span) {
        self.error(
            format!("'{}' is a builtin and cannot be used as a value", name),
            sp,
        );
        self.emit_nil();
    }

    /// Emit a fresh anonymous function `fn(a0..aN) { Ctor(a0..aN) }` and a
    /// `MakeClosure` for it with no captures. Used when a non-nullary
    /// constructor is referenced as a value.
    fn emit_ctor_closure(
        &mut self,
        type_id: i32,
        type_name: &str,
        variant_name: &str,
        field_labels: ArenaSlice,
        arity: i32,
    ) {
        if self.check_only {
            return;
        }
        let jump_over = self.emit_jump(Op::Jump);
        let func_start = self.current_addr();

        let ph =
            self.emit_construct_header_for_build(type_id, type_name, variant_name, field_labels);
        for i in 0..arity {
            self.emit_arg(Op::PushLocal, i);
        }
        self.emit_b(Op::MakeEnumPayload, arity as u16, ph);
        self.emit(Op::Ret);

        self.patch_jump(jump_over);

        self.program.functions.push(Function {
            name: format!("{type_name}.{variant_name}").into(),
            arity,
            locals: arity,
            capture_count: 0,
            code_start: func_start,
            code_len: self.current_addr() - func_start,
        });
        let func_idx = self.program.functions.len() as i32 - 1;
        self.emit_arg(Op::MakeClosure, func_idx);
    }

    // ========================================================================
    // Binary ops
    // ========================================================================

    fn compile_binary(&mut self, expr: &ast::BinaryExpression) -> Ty {
        if matches!(expr.op.kind, Kind::LogicalAnd | Kind::LogicalOr) {
            let jmp = if expr.op.kind == Kind::LogicalAnd {
                Op::JumpIfFalse
            } else {
                Op::JumpIfTrue
            };
            let l_ty = self.compile_expr(&expr.left);
            let b = self.ty_bool();
            self.engine.unify_at(b, l_ty, expr.left.span());
            self.emit(Op::Dup);
            let end_jump = self.emit_jump(jmp);
            self.emit(Op::Pop);
            let r_ty = self.compile_expr(&expr.right);
            self.engine.unify_at(b, r_ty, expr.right.span());
            self.patch_jump(end_jump);
            return self.ty_bool();
        }

        let l_ty = self.compile_expr(&expr.left);
        let r_ty = self.compile_expr(&expr.right);

        let (constraint, generic, is_cmp) = match expr.op.kind {
            Kind::PuncPlus => (Constraint::Addable, Op::Add, false),
            Kind::PuncMinus => (Constraint::Numeric, Op::Sub, false),
            Kind::PuncMul => (Constraint::Numeric, Op::Mul, false),
            Kind::PuncDiv => (Constraint::Numeric, Op::Div, false),
            Kind::PuncMod => (Constraint::Numeric, Op::Mod, false),
            Kind::PuncLt => (Constraint::Numeric, Op::Lt, true),
            Kind::PuncGt => (Constraint::Numeric, Op::Gt, true),
            Kind::PuncLte => (Constraint::Numeric, Op::Lte, true),
            Kind::PuncGte => (Constraint::Numeric, Op::Gte, true),
            Kind::PuncEqualsComparator | Kind::PuncNotEqual => {
                self.engine.unify_at(l_ty, r_ty, expr.span);
                let is_eq = expr.op.kind == Kind::PuncEqualsComparator;
                let opc = match self.engine.resolved_prim(l_ty) {
                    Some(Prim::Int) if is_eq => Op::EqInt,
                    Some(Prim::Int) => Op::NeqInt,
                    _ if is_eq => Op::Eq,
                    _ => Op::Neq,
                };
                self.emit(opc);
                return self.ty_bool();
            }
            _ => {
                self.error(
                    format!("Unknown binary operator: {}", expr.op.kind),
                    expr.span,
                );
                return self.engine.fresh_var();
            }
        };
        let operand = self.engine.fresh_constrained_var(constraint);
        self.engine.unify_at(operand, l_ty, expr.left.span());
        self.engine.unify_at(operand, r_ty, expr.right.span());
        // Specialize the opcode when the operand type has resolved to a
        // concrete prim. Unbound (still-polymorphic) operands keep the generic
        // op so the VM's tag-dispatching path handles them.
        let opc = match (generic, self.engine.resolved_prim(operand)) {
            (Op::Add, Some(Prim::Int)) => Op::AddInt,
            (Op::Add, Some(Prim::Float)) => Op::AddFloat,
            (Op::Add, Some(Prim::String)) => Op::AddStr,
            (Op::Sub, Some(Prim::Int)) => Op::SubInt,
            (Op::Sub, Some(Prim::Float)) => Op::SubFloat,
            (Op::Mul, Some(Prim::Int)) => Op::MulInt,
            (Op::Mul, Some(Prim::Float)) => Op::MulFloat,
            (Op::Div, Some(Prim::Int)) => Op::DivInt,
            (Op::Div, Some(Prim::Float)) => Op::DivFloat,
            (Op::Mod, Some(Prim::Int)) => Op::ModInt,
            (Op::Lt, Some(Prim::Int)) => Op::LtInt,
            (Op::Lt, Some(Prim::Float)) => Op::LtFloat,
            (Op::Gt, Some(Prim::Int)) => Op::GtInt,
            (Op::Gt, Some(Prim::Float)) => Op::GtFloat,
            (Op::Lte, Some(Prim::Int)) => Op::LteInt,
            (Op::Lte, Some(Prim::Float)) => Op::LteFloat,
            (Op::Gte, Some(Prim::Int)) => Op::GteInt,
            (Op::Gte, Some(Prim::Float)) => Op::GteFloat,
            (g, _) => g,
        };
        self.emit(opc);
        if is_cmp { self.ty_bool() } else { operand }
    }

    // ========================================================================
    // Binary literals  ( `<<v:size:kind, ..>>` )
    // ========================================================================

    /// Build a `Binary` by encoding each segment to a bit-string fragment and
    /// concatenating them left-to-right. Bit-syntax defaults: a bare
    /// segment is an 8-bit big-endian integer; `:N` / `:size(e)` give an
    /// explicit bit width; `:bytes(e)` / `:binary` splice a (prefix of a)
    /// binary; `:utf8` UTF-8-encodes a string.
    fn compile_binary_literal(&mut self, bl: &ast::BinaryLiteral) -> Ty {
        let bin_ty = self.ty_binary();
        if bl.segments.is_empty() {
            let c = self.const_binary(vec![], 0);
            self.emit_arg(Op::PushConst, c);
            return bin_ty;
        }
        // All-literal binaries (`<<'\r\n'>>`, `<<13, 10>>`, `<<1:4, 2:4>>`)
        // fold to a single pooled constant: zero runtime ops and zero
        // allocation per evaluation instead of a BinFromString/BinFromInt +
        // BinConcatN.
        if let Some((bytes, bit_len)) = Self::const_fold_binary_literal(bl) {
            let c = self.const_binary(bytes, bit_len);
            self.emit_arg(Op::PushConst, c);
            return bin_ty;
        }
        // Push every segment's fragment, then concatenate once: each byte is
        // copied a single time into one backing, instead of a BinAppend chain
        // that re-copies the accumulated prefix per segment.
        for seg in &bl.segments {
            self.compile_bin_segment_value(seg);
        }
        if bl.segments.len() > 1 {
            self.emit_arg(Op::BinConcatN, bl.segments.len() as i32);
        }
        bin_ty
    }

    /// The compile-time bytes of a `<<>>` literal whose every segment is a
    /// literal: string segments contribute UTF-8 bytes, integer segments their
    /// value MSB-first at their (literal) width. `None` when any segment needs
    /// runtime evaluation; type errors (float widths, non-Int values) also
    /// yield `None` so the normal path reports them.
    fn const_fold_binary_literal(bl: &ast::BinaryLiteral) -> Option<(Vec<u8>, u64)> {
        let mut bytes: Vec<u8> = Vec::new();
        let mut bit_len: u64 = 0;
        for seg in &bl.segments {
            match seg.kind {
                ast::BinKind::Utf8 => match &seg.value {
                    ast::Expression::StringLiteral(s) => {
                        push_literal_bytes(&mut bytes, &mut bit_len, s.value.as_bytes());
                    }
                    _ => return None,
                },
                ast::BinKind::Int => {
                    let ast::Expression::NumberLiteral(n) = &seg.value else {
                        return None;
                    };
                    let v = n.value.parse::<i64>().ok()?;
                    let bits = match &seg.size {
                        None => 8,
                        Some(ast::Expression::NumberLiteral(w)) => {
                            let w = w.value.parse::<i64>().ok()?;
                            if seg.unit == ast::BinUnit::Bytes {
                                w.max(0) * 8
                            } else {
                                w.max(0)
                            }
                        }
                        Some(_) => return None,
                    };
                    push_literal_bits(&mut bytes, &mut bit_len, v, bits as u64);
                }
                ast::BinKind::Binary => return None,
            }
        }
        Some((bytes, bit_len))
    }

    /// Push a single segment's value onto the stack as a `Binary` fragment.
    fn compile_bin_segment_value(&mut self, seg: &ast::BinSegment) {
        match seg.kind {
            ast::BinKind::Int => {
                let vty = self.compile_expr(&seg.value);
                let int_ty = self.ty_int();
                self.engine.unify_at(int_ty, vty, seg.value.span());
                self.emit_seg_size_bits(seg.size.as_ref(), seg.unit, 8);
                self.emit(Op::BinFromInt);
            }
            ast::BinKind::Binary => {
                let vty = self.compile_expr(&seg.value);
                let bin_ty = self.ty_binary();
                self.engine.unify_at(bin_ty, vty, seg.value.span());
                if let Some(sz) = &seg.size {
                    // `:bytes(e)` — splice the first `e` bytes (clamped).
                    self.emit_seg_size_bits(Some(sz), seg.unit, 0);
                    self.emit(Op::BinTake);
                }
                // `:binary` (no size) splices the whole value as-is.
            }
            ast::BinKind::Utf8 => {
                let vty = self.compile_expr(&seg.value);
                let str_ty = self.ty_string();
                self.engine.unify_at(str_ty, vty, seg.value.span());
                self.emit(Op::BinFromString);
            }
        }
    }

    /// Push the bit width of a segment onto the stack as an `Int`. An absent
    /// spec uses `default`; a `bytes(..)` spec is multiplied by 8.
    fn emit_seg_size_bits(
        &mut self,
        size: Option<&ast::Expression>,
        unit: ast::BinUnit,
        default: i64,
    ) {
        match size {
            None => {
                let c = self.const_int(default);
                self.emit_arg(Op::PushConst, c);
            }
            Some(e) => {
                let ety = self.compile_expr(e);
                let int_ty = self.ty_int();
                self.engine.unify_at(int_ty, ety, e.span());
                if unit == ast::BinUnit::Bytes {
                    let c = self.const_int(8);
                    self.emit_arg(Op::PushConst, c);
                    self.emit(Op::Mul);
                }
            }
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
        let has_spread = expr
            .elements
            .iter()
            .any(|e| matches!(e, ast::ArrayElement::SpreadElement(_)));

        if !has_spread {
            self.compile_array_run(&expr.elements, elem_var);
            self.emit_arg(Op::MakeArray, expr.elements.len() as i32);
            return self.ty_array(elem_var);
        }

        let mut have_result = false;
        let mut i = 0usize;
        while i < expr.elements.len() {
            if let ast::ArrayElement::SpreadElement(spread) = &expr.elements[i] {
                let Some(inner) = &spread.expression else {
                    self.error(
                        "Spread in array literal missing expression".to_string(),
                        spread.span,
                    );
                    i += 1;
                    continue;
                };
                let spread_ty = self.compile_expr(inner);
                let expected = self.ty_array(elem_var);
                self.engine.unify_at(expected, spread_ty, inner.span());
                if have_result {
                    self.emit(Op::ArrayConcat);
                } else {
                    have_result = true;
                }
                i += 1;
                continue;
            }

            // Contiguous run of non-spread element expressions [i, j).
            let mut j = i;
            while j < expr.elements.len()
                && !matches!(&expr.elements[j], ast::ArrayElement::SpreadElement(_))
            {
                j += 1;
            }

            // Head form `[e0, .., e_{k-1}, ..spread]` (nothing emitted yet, so
            // the spread is the whole tail): front-cons the k elements onto the
            // spread in O(k) via `Prepend`, instead of `MakeArray k` followed by
            // an O(n) `ArrayConcat`. This is the producer half of the old O(n²).
            if !have_result
                && j < expr.elements.len()
                && let ast::ArrayElement::SpreadElement(spread) = &expr.elements[j]
                && let Some(inner) = &spread.expression
            {
                self.compile_array_run(&expr.elements[i..j], elem_var);
                let spread_ty = self.compile_expr(inner);
                let expected = self.ty_array(elem_var);
                self.engine.unify_at(expected, spread_ty, inner.span());
                self.emit_arg(Op::Prepend, (j - i) as i32);
                have_result = true;
                i = j + 1;
                continue;
            }

            self.compile_array_run(&expr.elements[i..j], elem_var);
            let group_count = (j - i) as i32;
            if have_result {
                self.emit_arg(Op::Append, group_count);
            } else {
                self.emit_arg(Op::MakeArray, group_count);
                have_result = true;
            }
            i = j;
        }

        if !have_result {
            self.emit_arg(Op::MakeArray, 0);
        }
        self.ty_array(elem_var)
    }

    // ========================================================================
    // Function call
    // ========================================================================

    fn compile_call(&mut self, expr: &ast::FunctionCallExpression, is_tail: bool) -> Ty {
        // Resolve the callee to a name + scheme + load action without emitting
        // anything yet. This lets us dispatch on `ValueKind` for constructors
        // and builtins, and use the callee's known type to push hints into
        // function-literal arguments.
        if let Some((name, name_span, scheme, load)) = self.resolve_simple_callee(&expr.callee) {
            let inst_ty = self.engine.instantiate(&scheme, &self.rigid_ids);
            // Hover-only here; the graph occurrence (with the correct
            // Unqualified/Qualified kind) is emitted in `resolve_simple_callee`.
            // Skip the doc map probe + clone on the non-LSP path, where
            // `record` discards it (see its early return).
            let doc = self.doc_if_collecting(&name);
            self.record(&name, inst_ty, name_span, doc);

            return match scheme.kind {
                ValueKind::Constructor {
                    type_name,
                    type_id,
                    arity,
                    field_labels,
                    ..
                } => {
                    let type_name = self.engine.str(type_name).to_string();
                    self.compile_ctor_call(
                        &name,
                        &type_name,
                        type_id,
                        arity as usize,
                        field_labels,
                        inst_ty,
                        &expr.arguments,
                        expr.span,
                    )
                }
                ValueKind::Builtin { op } => {
                    let ret = self.compile_positional_args(inst_ty, &expr.arguments, expr.span);
                    let op = self.engine.str(op).to_string();
                    self.emit_builtin_op(&op, expr.span);
                    ret
                }
                ValueKind::Local | ValueKind::ModuleFn => {
                    let ret = self.compile_positional_args(inst_ty, &expr.arguments, expr.span);
                    let general = if is_tail { Op::TailCall } else { Op::Call };
                    let op = match load {
                        // Self-recursion: skip PushSelf and emit the fused
                        // self-call op so the VM can read func_idx/captures
                        // straight off the live frame.
                        Some(VarAccess::Self_) => {
                            if is_tail {
                                Op::TailCallSelf
                            } else {
                                Op::CallSelf
                            }
                        }
                        Some(a) => {
                            self.emit_load(&a);
                            general
                        }
                        None => {
                            self.no_runtime_binding(&name, None, name_span);
                            general
                        }
                    };
                    self.emit_arg(op, expr.arguments.len() as i32);
                    ret
                }
            };
        }

        // Arbitrary callee expression: compile it first so we have its type
        // for argument hints, stash the value, compile args, then call.
        let callee_ty = self.compile_expr(&expr.callee);
        let temp = self.spill_temp();

        let ret = self.compile_positional_args(callee_ty, &expr.arguments, expr.span);
        self.emit_arg(Op::PushLocal, temp);
        let op = if is_tail { Op::TailCall } else { Op::Call };
        self.emit_arg(op, expr.arguments.len() as i32);
        ret
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
    /// qualifiers: look the member up, compute its runtime load, and record the
    /// `Qualified` value-use. Shared by the callee position
    /// (`resolve_simple_callee`) and the value position
    /// (`compile_property_access`), which diverge only on how they treat the
    /// two failure modes — see [`QualifiedMember`].
    fn resolve_qualified_member(&mut self, pa: &ast::PropertyAccessExpression) -> QualifiedMember {
        let ast::Expression::Identifier(left) = pa.left.as_ref() else {
            return QualifiedMember::NotQualified;
        };
        let Some(module_key) = self.imported_qualifiers.get(&left.name).cloned() else {
            return QualifiedMember::NotQualified;
        };
        let ast::Expression::Identifier(member) = pa.right.as_ref() else {
            return QualifiedMember::NotQualified;
        };
        let Some((scheme, slot)) =
            self.lookup_module_member(&module_key, &member.name, member.span)
        else {
            return QualifiedMember::LookupFailed;
        };
        let load = slot.map(VarAccess::Global);
        // The member is not in unqualified scope, so resolve the occurrence
        // through the imported scheme's canonical `def`.
        self.record_value_use(
            scheme.def,
            member.span,
            crate::reference::ReferenceKind::Qualified,
        );
        QualifiedMember::Resolved {
            module_key,
            member_name: member.name.clone(),
            member_span: member.span,
            scheme,
            load,
        }
    }

    /// Best-effort resolution of a callee expression that is a plain name (or
    /// `module.name`). Returns the looked-up scheme and how to push the
    /// runtime value when needed. Anything else falls through to the
    /// arbitrary-expression path.
    fn resolve_simple_callee(
        &mut self,
        callee: &ast::Expression,
    ) -> Option<(String, Span, Scheme, Option<VarAccess>)> {
        match callee {
            ast::Expression::Identifier(id) => {
                let scheme = *self.env.lookup(&id.name)?;
                let load = self.value_load(&id.name, &scheme.kind);
                // Unqualified callee `name()` — the genuine unqualified-use
                // seam for calls (this path bypasses `compile_identifier`).
                // `record` in `compile_call` is hover-only.
                self.record_value_use(
                    scheme.def,
                    id.span,
                    crate::reference::ReferenceKind::Unqualified,
                );
                Some((id.name.clone(), id.span, scheme, load))
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
                        load,
                        ..
                    } => Some((member_name, member_span, scheme, load)),
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
        type_name: &str,
        type_id: i32,
        arity: usize,
        field_labels_sl: ArenaSlice,
        inst_ty: Ty,
        args: &[ast::CallArg],
        call_span: Span,
    ) -> Ty {
        let field_labels: Vec<String> = self.engine.strs_of(field_labels_sl);
        // The instantiated scheme is `IFun{params, ret}` for arity>0 or `ICon`
        // for arity==0. Extract param/result types.
        let r = self.engine.find(inst_ty);
        let (param_tys, result_ty) = match self.engine.node(r) {
            TypeNode::Fun { params, ret } => (self.engine.children_of(params).to_vec(), ret),
            _ => (vec![], r),
        };

        // Partition the arguments into a spread base (at most one) and a map
        // of explicitly-supplied fields keyed by their declared position.
        let mut spread: Option<&ast::Expression> = None;
        for arg in args {
            if let ast::CallArg::Spread(e) = arg {
                if spread.is_some() {
                    self.error(
                        "Constructor call may have at most one spread".to_string(),
                        e.span(),
                    );
                }
                spread = Some(e);
            }
        }
        let (by_pos, _) = self.slot_ctor_args(
            variant_name,
            arity,
            &field_labels,
            args.iter().filter_map(|a| match a {
                ast::CallArg::Positional(e) => Some((None, e, e.span())),
                ast::CallArg::Labeled { label, value } => Some((Some(label), value, label.span)),
                ast::CallArg::Spread(_) => None,
            }),
            if spread.is_some() {
                None
            } else {
                Some((call_span, ""))
            },
            true,
        );

        // Record-update: compile the base, store it, then for every field not
        // explicitly supplied, project it out of the base via GetField.
        let spread_slot: Option<i32> = spread.map(|e| {
            let base_ty = self.compile_expr(e);
            self.engine.unify_at(result_ty, base_ty, e.span());
            self.spill_temp()
        });

        if arity == 0 && self.try_emit_bool_value(type_id, variant_name) {
            return result_ty;
        }

        let ph =
            self.emit_construct_header_for_build(type_id, type_name, variant_name, field_labels_sl);

        for (i, slot_expr) in by_pos.iter().enumerate().take(field_labels.len()) {
            let expected = param_tys.get(i).copied();
            match slot_expr {
                Some(e) => {
                    let ty = self.compile_expr_with_hint(e, expected);
                    if let Some(p) = expected {
                        self.engine.unify_at(p, ty, e.span());
                    }
                }
                None => match spread_slot {
                    Some(slot) => {
                        self.emit_arg(Op::PushLocal, slot);
                        self.emit_arg(Op::GetField, i as i32);
                    }
                    None => {
                        // Missing-field error was already raised; push a
                        // placeholder so MakeEnumPayload arity stays correct.
                        self.emit_nil();
                    }
                },
            }
        }

        self.emit_b(Op::MakeEnumPayload, arity as u16, ph);

        result_ty
    }

    fn emit_builtin_op(&mut self, name: &str, sp: Span) {
        match name {
            "println" => {
                self.emit(Op::Print);
                self.emit_nil();
            }
            "string__inspect" => self.emit(Op::ToString),
            "internal__stack_depth" => self.emit(Op::StackDepth),
            "io__read_file" => self.emit(Op::FileRead),
            "io__write_file" => self.emit(Op::FileWrite),
            "net__listen" => self.emit(Op::TcpListen),
            "net__accept" => self.emit(Op::TcpAccept),
            "net__connect" => self.emit(Op::TcpConnect),
            "net__close" => self.emit(Op::TcpCloseServer),
            "net__local_addr" => self.emit(Op::TcpLocalAddr),
            "net__resolve" => self.emit(Op::DnsResolve),
            "socket__read" => self.emit(Op::TcpRead),
            "socket__read_until" => self.emit(Op::TcpReadUntil),
            "socket__write" => self.emit(Op::TcpWrite),
            "socket__write_parts" => self.emit(Op::TcpWriteParts),
            "socket__close" => self.emit(Op::TcpClose),
            "string__split" => self.emit(Op::StrSplit),
            "string__length" => self.emit(Op::StrLen),
            "string__contains" => self.emit(Op::StrContains),
            "string__trim" => self.emit(Op::StrTrim),
            "int__to_string" => self.emit(Op::IntToString),
            "binary__from_string" => self.emit(Op::BinFromString),
            "binary__to_string" => self.emit(Op::BinToString),
            "binary__bit_size" => self.emit(Op::BinBitSize),
            "binary__byte_size" => self.emit(Op::BinByteSize),
            "binary__slice" => self.emit(Op::BinSlice),
            "binary__append" => self.emit(Op::BinAppend),
            "binary__index_of" => self.emit(Op::BinIndexOf),
            "binary__byte_at" => self.emit(Op::BinByteAt),
            "binary__parse_int" => self.emit(Op::BinParseInt),
            "binary__eq_ignore_ascii_case" => self.emit(Op::BinEqIgnoreAsciiCase),
            "binary__to_ascii_lower" => self.emit(Op::BinToAsciiLower),
            "binary__from_int_ascii" => self.emit(Op::BinFromIntAscii),
            "http__parse_head" => self.emit(Op::HttpParseHead),
            "http__framing" => self.emit(Op::HttpFraming),
            "http__chunk_decode" => self.emit(Op::HttpChunkDecode),
            "http__header_get" => self.emit(Op::HttpHeaderGet),
            "http__header_has" => self.emit(Op::HttpHeaderHas),
            "http__serialize_head" => self.emit(Op::HttpSerializeHead),
            "float__floor" => self.emit(Op::FloatFloor),
            "float__ceil" => self.emit(Op::FloatCeil),
            "float__round" => self.emit(Op::FloatRound),
            "float__truncate" => self.emit(Op::FloatTruncate),
            "float__from_int" => self.emit(Op::FloatFromInt),
            "float__to_string" => self.emit(Op::FloatToString),
            "scheduler__spawn" => self.emit(Op::ProcessSpawn),
            "scheduler__spawn_local" => self.emit(Op::SpawnLocal),
            "scheduler__spawn_on_each" => self.emit(Op::SpawnOnEach),
            "scheduler__sleep" => self.emit(Op::Sleep),
            "time__monotonic" => self.emit(Op::Monotonic),
            "process__argv" => self.emit(Op::Argv),
            "process__env" => self.emit(Op::EnvMap),
            "map__get" => self.emit(Op::MapGet),
            "map__has" => self.emit(Op::MapHas),
            "map__keys" => self.emit(Op::MapKeys),
            "map__values" => self.emit(Op::MapValues),
            "map__size" => self.emit(Op::MapSize),
            "map__new" => self.emit(Op::MapNew),
            "map__set" => self.emit(Op::MapSet),
            "map__delete" => self.emit(Op::MapDelete),
            "map__to_list" => self.emit(Op::MapToList),
            _ => {
                self.error(format!("Internal: builtin '{}' has no codegen", name), sp);
            }
        }
    }

    // ========================================================================
    // Property access
    // ========================================================================

    fn compile_property_access(&mut self, expr: &ast::PropertyAccessExpression) -> Ty {
        // `module.member` qualified access (no call — calls are handled in
        // compile_call via resolve_simple_callee).
        match self.resolve_qualified_member(expr) {
            QualifiedMember::NotQualified => {}
            QualifiedMember::LookupFailed => {
                self.emit_nil();
                return self.engine.fresh_var();
            }
            QualifiedMember::Resolved {
                module_key,
                member_name,
                member_span,
                scheme,
                load,
            } => {
                let ty = self.engine.instantiate(&scheme, &self.rigid_ids);
                self.record(&member_name, ty, member_span, None);
                self.emit_named_value(
                    &member_name,
                    Some(&module_key),
                    member_span,
                    scheme.kind,
                    load,
                );
                return ty;
            }
        }

        let left_ty = self.compile_expr(&expr.left);

        // `tuple.N` index.
        if let ast::Expression::NumberLiteral(num) = expr.right.as_ref() {
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
            self.emit_arg(Op::TupleIndex, index);

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
                    expr.right.span(),
                );
                return self.engine.fresh_var();
            }
            self.error(
                format!("Cannot index .{} on non-tuple type", index),
                expr.right.span(),
            );
            return self.engine.fresh_var();
        }

        // `value.field` — labelled field shared by every variant.
        if let ast::Expression::Identifier(field) = expr.right.as_ref() {
            return self.compile_field_access(left_ty, &field.name, field.span);
        }

        self.error("Invalid property access".to_string(), expr.right.span());
        self.engine.fresh_var()
    }

    /// compile_field_access error path: report, pop receiver, push nil, fresh tyvar.
    fn field_access_bail(&mut self, msg: String, span: Span) -> Ty {
        self.error(msg, span);
        self.emit(Op::Pop);
        self.emit_nil();
        self.engine.fresh_var()
    }

    /// Project `.field` out of a value. Because the runtime value's variant is
    /// not statically known, the field is only accessible if EVERY variant of
    /// the receiver type carries a label of that name and at the same type.
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

        let Some(variants) = info.variants() else {
            return self.field_access_bail(
                format!("Type '{}' has no accessible fields", type_name),
                field_span,
            );
        };

        let field_id = self.engine.intern(field);
        let mut field_ty: Option<Ty> = None;
        let mut field_idx: Option<usize> = None;
        for v in self.engine.variants_of(variants).to_vec() {
            let variant_name = self.engine.str(v.name).to_string();
            let fields = self.engine.variant_fields_of(v.fields).to_vec();
            match fields.iter().position(|f| f.label == field_id) {
                Some(i) => {
                    let substituted = self.engine.substitute_type_vars(
                        fields[i].ty,
                        info.type_params,
                        &type_args,
                    );
                    match field_ty {
                        Some(existing) => {
                            self.engine.unify_at(existing, substituted, field_span);
                        }
                        None => field_ty = Some(substituted),
                    }
                    match field_idx {
                        Some(prev) if prev != i => {
                            return self.field_access_bail(
                                format!(
                                    "Field '{}' is not at the same position in every variant of '{}' (position {} in '{}', expected {})",
                                    field, type_name, i, variant_name, prev
                                ),
                                field_span,
                            );
                        }
                        _ => field_idx = Some(i),
                    }
                }
                None => {
                    return self.field_access_bail(
                        format!(
                            "Field '{}' is not present on every variant of '{}' (missing on '{}')",
                            field, type_name, variant_name
                        ),
                        field_span,
                    );
                }
            }
        }

        let result_ty = field_ty.unwrap_or_else(|| self.engine.fresh_var());
        self.emit_arg(Op::GetField, field_idx.unwrap_or(0) as i32);

        let qualified = format!("{}.{}", type_name, field);
        let doc = self.env.lookup_doc(&qualified);
        self.record(&qualified, result_ty, field_span, doc);
        // `Type.field` is a dotted key in `env.definitions` (no local can
        // shadow it), so resolving it here is safe and keeps goto-def /
        // find-references / rename working for record fields, and feeds the
        // field-level dead-code reachability walk.
        let fdef = self.env.lookup_definition(&qualified);
        self.record_value_use(
            fdef,
            field_span,
            crate::reference::ReferenceKind::Unqualified,
        );
        result_ty
    }

    // ========================================================================
    // Or expression (Option/Result unwrapping)
    // ========================================================================

    fn compile_or(&mut self, expr: &ast::OrExpression) -> Ty {
        // Fused fast path: `arr[i] or default` — the single idiomatic safe
        // index. The generic path below builds a `Some(elem)`/`None` box per
        // element (2 heap allocs + a hash in `make_some`) only to immediately
        // `MatchEnum`/`UnwrapEnum` it away. `IndexOrElse` fetches the element
        // directly or branches to the recovery body. A receiver
        // (`arr[i] -> x or ...`) is *not* fused: an indexed value is always an
        // `Option`, so the generic path must still raise the "'or' on an
        // Option does not bind a value" error. A range index is a slice
        // (`Array`, not `Option`), so it also stays on the generic path where
        // it correctly errors.
        if expr.receiver.is_none()
            && let ast::Expression::ArrayIndexExpression(aie) = expr.expression.as_ref()
            && !matches!(aie.index.as_ref(), ast::Expression::RangeExpression(_))
        {
            return self.compile_index_or_else(aie, &expr.body);
        }

        let left_ty = self.compile_expr(&expr.expression);
        let resolved = self.engine.find(left_ty);

        let success_var = self.engine.fresh_var();

        // Determine which prelude type the LHS is. The Con node carries its
        // nominal id, so user-defined types that happen to be called
        // "Option"/"Result" can never match.
        let lhs_type_id = match self.engine.node(resolved) {
            TypeNode::Con { id, .. } => id,
            _ => 0,
        };

        // The Option and Result paths share the same bytecode shape; they differ
        // only in the expected ICon, the failure constructor, and whether the
        // failure case carries a bindable payload (`err_var`).
        use super::prelude_bindings::names as pn;
        let (type_id, fail_ctor, err_var) = if self.prelude.is_option(lhs_type_id) {
            if let Some(recv) = &expr.receiver {
                self.error(
                    "'or' on an Option does not bind a value (the failure case carries nothing)"
                        .to_string(),
                    recv.span,
                );
            }
            (self.prelude.option.id, pn::NONE, None::<Ty>)
        } else if self.prelude.is_result(lhs_type_id) {
            let e = self.engine.fresh_var();
            (self.prelude.result.id, pn::ERR, Some(e))
        } else {
            let s = self.engine.type_to_str(left_ty);
            self.error(
                format!(
                    "'or' requires the left side to be Option(_) or Result(_, _), got '{}'",
                    s
                ),
                expr.expression.span(),
            );
            // Best-effort recovery: leave the LHS on the stack and still
            // type-check the body so downstream errors are not cascaded.
            self.env.push_scope();
            self.push_local_scope();
            let _ = self.compile_expr(&expr.body);
            self.emit(Op::Pop);
            self.pop_local_scope();
            self.env.pop_scope();
            return self.engine.fresh_var();
        };

        let expected = match &err_var {
            Some(e) => self.ty_result(success_var, *e),
            None => self.ty_option(success_var),
        };
        self.engine
            .unify_at(expected, left_ty, expr.expression.span());

        // [v] → Dup → [v,v] → MatchEnum <fail_ctor> → [v,bool]
        self.emit(Op::Dup);
        self.emit_match_header(type_id, fail_ctor);
        self.emit(Op::MatchEnum);
        let skip = self.emit_jump(Op::JumpIfFalse);

        // Failure branch: discard or unwrap-and-bind the payload, then
        // evaluate the recovery body.
        self.env.push_scope();
        self.push_local_scope();
        if let Some(e) = err_var {
            self.emit(Op::UnwrapEnum);
            if let Some(recv) = &expr.receiver {
                let idx = self.get_or_create_local(&recv.name);
                self.emit_arg(Op::StoreLocal, idx);
                self.register_local_binding(&recv.name, e, recv.span);
            } else {
                self.emit(Op::Pop);
            }
        } else {
            self.emit(Op::Pop);
        }
        let body_ty = self.compile_expr(&expr.body);
        self.pop_local_scope();
        self.env.pop_scope();
        self.engine
            .unify_at(success_var, body_ty, type_defining_span(&expr.body));
        let end = self.emit_jump(Op::Jump);

        // Success branch: unwrap the single payload.
        self.patch_jump(skip);
        self.emit(Op::UnwrapEnum);
        self.patch_jump(end);

        success_var
    }

    /// Fused `arr[i] or body`. Type-checks `arr[i]` exactly as the generic
    /// `ArrayIndexExpression` arm (LHS must be `Array(elem)`, index `Int`) but
    /// never commits to an `Option` result. Emits:
    ///
    /// ```text
    ///   <arr> <idx> IndexOrElse to_body   ; in-bounds: push elem, fall through
    ///               Jump end              ; skip the recovery body
    ///   to_body:    <body>                ; out-of-bounds: push default
    ///   end:
    /// ```
    ///
    /// Zero allocations per element (direct fetch + branch) vs. the generic
    /// path's per-element `Some`/`None` box (2 heap allocs + a hash) plus
    /// `Dup; 2×PushConst; MatchEnum; JumpIfFalse; UnwrapEnum`.
    fn compile_index_or_else(
        &mut self,
        aie: &ast::ArrayIndexExpression,
        body: &ast::Expression,
    ) -> Ty {
        let arr_ty = self.compile_expr(&aie.expression);
        let elem_var = self.engine.fresh_var();
        let arr_expected = self.ty_array(elem_var);
        self.engine
            .unify_at(arr_expected, arr_ty, aie.expression.span());
        let idx_ty = self.compile_expr(&aie.index);
        let int_t = self.ty_int();
        self.engine.unify_at(int_t, idx_ty, aie.index.span());

        // [arr, idx] -> IndexOrElse: in-bounds pushes the element and falls
        // through to `Jump end` (skipping the body); out-of-bounds jumps to
        // the recovery body.
        let to_body = self.emit_jump(Op::IndexOrElse);
        let end = self.emit_jump(Op::Jump);

        self.patch_jump(to_body);
        self.env.push_scope();
        self.push_local_scope();
        let body_ty = self.compile_expr(body);
        self.pop_local_scope();
        self.env.pop_scope();
        self.patch_jump(end);

        self.engine
            .unify_at(elem_var, body_ty, type_defining_span(body));
        elem_var
    }

    // ========================================================================
    // Functions
    // ========================================================================

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
    ) -> Ty {
        let saved = self.enter_fn_frame(None);

        // A lambda's annotation type-variables share scope with each other but
        // are local to the lambda; mint them via a fresh hydrator.
        let mut hydrator = Hydrator::new();
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

        let body_ty = self.compile_expr(body);
        let ret_ty = match return_annot {
            Some(rt) => {
                let annot_ty = self.hydrate(&mut hydrator, rt);
                self.engine
                    .unify_at(annot_ty, body_ty, type_defining_span(body));
                annot_ty
            }
            None => body_ty,
        };

        self.finish_fn_frame(saved, "__anon__", params.len());
        self.engine.mk_fun(&param_tys, ret_ty)
    }

    /// Compile a top-level `fn` whose parameter and return types were already
    /// hydrated in Pass 3. The supplied `hydrator` carries the rigid generic
    /// ids for any annotated type variables so that `instantiate` inside the
    /// body leaves them intact, and we unify the body's inferred type against
    /// the preregistered shape rather than re-reading the annotations.
    pub(super) fn compile_declared_function(
        &mut self,
        name: &str,
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

        let body_ty = self.compile_expr(body);
        self.engine
            .unify_at(ret_ty, body_ty, type_defining_span(body));

        self.finish_fn_frame(saved, name, params.len());
        self.engine.mk_fun(&param_tys, ret_ty)
    }

    /// Snapshot the enclosing frame's codegen state, push a fresh inner frame,
    /// emit the jump-over placeholder that lets execution skip the embedded
    /// body, and open a new type-env scope. Param binding emits no bytecode so
    /// `func_start` here equals the address after params are bound.
    fn enter_fn_frame(&mut self, binding: Option<&str>) -> FnFrame {
        // The enclosing frame's locals already map any preallocated
        // entry-frame slots (analysis.rs Pass 3); moving the map into
        // `outer_scopes` as-is lets a nested lambda resolve the enclosing fn
        // name through outer_scopes[0] at the right slot (depth is irrelevant
        // for outer-scope resolution). `finish_fn_frame` moves it back.
        self.outer_scopes.push(Scope {
            locals: std::mem::take(&mut self.locals),
        });
        let jump_over = self.emit_jump(Op::Jump);
        self.env.push_scope();
        self.unused.push(HashMap::new());
        FnFrame {
            undo_base: self.undo_log.len(),
            marks_base: self.scope_marks.len(),
            local_count: std::mem::replace(&mut self.local_count, 0),
            captures: std::mem::take(&mut self.captures),
            capture_names: std::mem::take(&mut self.capture_names),
            rigid_ids: self.rigid_ids.clone(),
            binding: match binding {
                Some(n) => std::mem::replace(&mut self.current_binding, n.to_string()),
                None => {
                    // A lambda / fn-expression has no self-name UNLESS it is the
                    // RHS of a `name = fn(...)` binding, which stashes that name
                    // in `next_fn_self_name` (consumed one-shot here). Inheriting
                    // the enclosing fn's `current_binding` is wrong: a HOF-arg
                    // lambda calling the enclosing fn would mis-resolve to Self_
                    // and emit CallSelf against the lambda's own live frame.
                    let old = std::mem::take(&mut self.current_binding);
                    if let Some(n) = self.next_fn_self_name.take() {
                        self.current_binding = n;
                    }
                    old
                }
            },
            tail: std::mem::replace(&mut self.in_tail_position, true),
            jump_over,
            func_start: self.current_addr(),
        }
    }

    /// Register a freshly-typed local binding into the type environment, the
    /// dead-code/reference graph, and the hover record, then emit its
    /// value-def. These steps must stay in lockstep, so every local-binder
    /// site (params, pattern names, the `or`-receiver) funnels through here
    /// after reserving its slot with `get_or_create_local`.
    fn register_local_binding(&mut self, name: &str, ty: Ty, sp: Span) {
        let m = self.current_module_slice();
        self.env
            .define_at(name, mono(ty), def_loc(sp, m, EntityKind::Value));
        self.track_binding(name, sp);
        self.record(name, ty, sp, None);
        self.emit_value_def(sp, name, None);
    }

    fn bind_param(&mut self, p: &ast::FunctionParameter, ty: Ty) {
        self.get_or_create_local(&p.identifier.name);
        self.register_local_binding(&p.identifier.name, ty, p.identifier.span);
    }

    /// Close out a function body: emit `Ret`, restore the snapshotted frame and
    /// type-env, patch the jump-over, register the `Function` metadata, then
    /// load each captured value and emit `MakeClosure`.
    fn finish_fn_frame(&mut self, saved: FnFrame, name: &str, arity: usize) {
        self.in_tail_position = saved.tail;
        self.current_binding = saved.binding;
        self.emit(Op::Ret);
        self.rigid_ids = saved.rigid_ids;
        self.env.pop_scope();
        self.pop_unused_scope();
        self.patch_jump(saved.jump_over);

        let captured = std::mem::replace(&mut self.capture_names, saved.capture_names);
        if !self.check_only {
            self.program.functions.push(Function {
                name: name.into(),
                arity: arity as i32,
                locals: self.local_count,
                capture_count: captured.len() as i32,
                code_start: saved.func_start,
                code_len: self.current_addr() - saved.func_start - 1,
            });
        }
        let func_idx = self.program.functions.len() as i32 - 1;

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
        self.captures = saved.captures;

        for cap_name in &captured {
            if let Some(access) = self.resolve_variable(cap_name) {
                self.emit_load(&access);
            }
        }
        self.emit_arg(Op::MakeClosure, func_idx);
    }

    // ========================================================================
    // Pattern matching
    // ========================================================================

    fn compile_match(&mut self, m: &ast::MatchExpression, is_tail: bool) -> Ty {
        let subject_ty = self.compile_expr(&m.subject);
        let subject_slot = self.spill_temp();

        let result_ty = self.engine.fresh_var();
        let mut end_jumps: Vec<i32> = vec![];
        let mut any_pattern_err = false;

        for arm in &m.arms {
            self.env.push_scope();
            self.push_local_scope();

            let mut b = PatternBindings::new();
            if !self.type_pattern(&arm.pattern, subject_ty, &mut b) {
                any_pattern_err = true;
            }
            self.bind_pattern_initials(&b);

            let mut fail_jumps: Vec<i32> = vec![];
            self.emit_arg(Op::PushLocal, subject_slot);
            self.emit_pattern(&arm.pattern, &mut fail_jumps);

            if let Some(guard) = &arm.guard {
                let guard_ty = self.compile_expr(guard);
                let bool_ty = self.ty_bool();
                self.engine
                    .unify_at(bool_ty, guard_ty, type_defining_span(guard));
                fail_jumps.push(self.emit_jump(Op::JumpIfFalse));
            }

            self.in_tail_position = is_tail;
            let body_ty = self.compile_expr(&arm.body);
            self.in_tail_position = false;
            self.engine
                .unify_at(result_ty, body_ty, type_defining_span(&arm.body));

            end_jumps.push(self.emit_jump(Op::Jump));
            self.patch_all(&fail_jumps);

            self.pop_local_scope();
            self.env.pop_scope();
        }

        // Fallthrough is unreachable when the match is exhaustive; emit a Nil
        // so the VM stack is balanced if it somehow is reached.
        self.emit_nil();

        self.patch_all(&end_jumps);

        if !any_pattern_err {
            let resolved_subj = self.engine.resolve(subject_ty, Some(&self.env));
            let all_pats: Vec<Pat> = m
                .arms
                .iter()
                .map(|arm| pattern_to_pat(&arm.pattern, &resolved_subj))
                .collect();
            // Guarded arms don't contribute to exhaustiveness (the guard may be
            // false) and don't make later arms unreachable.
            let unguarded: Vec<Pat> = m
                .arms
                .iter()
                .zip(&all_pats)
                .filter(|(arm, _)| arm.guard.is_none())
                .map(|(_, p)| p.clone())
                .collect();
            if let Some(missing) = check_exhaustiveness(&unguarded, &resolved_subj) {
                self.error(
                    format!("Match is not exhaustive, missing: {}", missing),
                    m.subject.span(),
                );
            }
            let mut prior_unguarded = UsefulnessMatrix::new(resolved_subj);
            for (i, arm) in m.arms.iter().enumerate() {
                if !prior_unguarded.is_useful(&all_pats[i]) {
                    self.error(
                        "This pattern is unreachable; a previous pattern matches the same values"
                            .to_string(),
                        arm.pattern.span(),
                    );
                }
                if arm.guard.is_none() {
                    prior_unguarded.push(all_pats[i].clone());
                }
            }
        }

        result_ty
    }

    /// Bind a pattern's freshly-typed names (populated by `type_pattern`
    /// into `b.initial`): reserve a local slot for each and funnel it through
    /// [`Self::register_local_binding`].
    fn bind_pattern_initials(&mut self, b: &PatternBindings) {
        for (name, (ty, sp)) in &b.initial {
            self.get_or_create_local(name);
            self.register_local_binding(name, *ty, *sp);
        }
    }

    // ========================================================================
    // Pattern type-checking (no codegen)
    // ========================================================================

    /// Type-check a pattern against `expected`, recording bound names in `b`.
    /// Emits no bytecode. Returns `false` if any unification or binding step
    /// failed; the caller propagates this so that exhaustiveness/usefulness
    /// checks (which assume well-typed patterns) can be skipped.
    fn type_pattern(&mut self, pat: &ast::Pattern, expected: Ty, b: &mut PatternBindings) -> bool {
        match pat {
            ast::Pattern::Binary {
                segments,
                rest,
                span,
            } => {
                let bin_ty = self.ty_binary();
                let mut ok = self.engine.unify_at(expected, bin_ty, *span);
                for seg in segments {
                    // A string-literal Utf8 segment (`<<'GET ', ..>>`) matches
                    // its encoded bytes as a prefix; it binds nothing and has
                    // no value type to check. Other Int / Utf8 segments bind an
                    // integer (a value or a codepoint); Binary segments bind a
                    // sub-binary. Size expressions are runtime values, not
                    // bindings, so they are type-checked alongside codegen in
                    // `emit_pattern`.
                    if Self::utf8_literal_segment(seg).is_some() {
                        continue;
                    }
                    let seg_val_ty = match seg.kind {
                        ast::BinKind::Int | ast::BinKind::Utf8 => self.ty_int(),
                        ast::BinKind::Binary => self.ty_binary(),
                    };
                    ok &= self.type_pattern(&seg.value, seg_val_ty, b);
                }
                if let Some(r) = rest
                    && let Some(id) = &r.binding
                {
                    let rest_ty = self.ty_binary();
                    ok &= b.bind(&id.name, rest_ty, id.span, &mut self.engine);
                }
                ok
            }
            ast::Pattern::Wildcard { .. } => true,
            ast::Pattern::Var { name } => b.bind(&name.name, expected, name.span, &mut self.engine),
            ast::Pattern::Literal(lit) => {
                let (lit_ty, sp) = match lit {
                    ast::PatternLiteral::Number(n) => (
                        if n.value.contains('.') {
                            self.engine.icon_float()
                        } else {
                            self.ty_int()
                        },
                        n.span,
                    ),
                    ast::PatternLiteral::String(s) => (self.ty_string(), s.span),
                };
                self.engine.unify_at(expected, lit_ty, sp)
            }
            ast::Pattern::Range { start, end, span } => {
                let int_t = self.ty_int();
                let mut ok = self.engine.unify_at(expected, int_t, *span);
                ok &= self.type_pattern(start, int_t, b);
                ok &= self.type_pattern(end, int_t, b);
                ok
            }
            ast::Pattern::Tuple { elements, span } => {
                let fresh: Vec<Ty> = (0..elements.len())
                    .map(|_| self.engine.fresh_var())
                    .collect();
                let tup = self.engine.mk_tuple(&fresh);
                let mut ok = self.engine.unify_at(expected, tup, *span);
                for (elem, &ety) in elements.iter().zip(fresh.iter()) {
                    ok &= self.type_pattern(elem, ety, b);
                }
                ok
            }
            ast::Pattern::Array { elements, span } => {
                let elem_var = self.engine.fresh_var();
                let arr_ty = self.ty_array(elem_var);
                let mut ok = self.engine.unify_at(expected, arr_ty, *span);

                let mut seen_spread = false;
                for (i, e) in elements.iter().enumerate() {
                    match e {
                        ast::ArrayPatternElement::Pattern(p) => {
                            ok &= self.type_pattern(p, elem_var, b);
                        }
                        ast::ArrayPatternElement::Spread { binding, span: ssp } => {
                            if seen_spread {
                                self.error(
                                    "Array pattern may contain at most one spread".to_string(),
                                    *ssp,
                                );
                                ok = false;
                            } else if i != elements.len() - 1 {
                                self.error(
                                    "Spread in array pattern must be the last element".to_string(),
                                    *ssp,
                                );
                                ok = false;
                            }
                            seen_spread = true;
                            if let Some(id) = binding {
                                let rest_ty = self.ty_array(elem_var);
                                ok &= b.bind(&id.name, rest_ty, id.span, &mut self.engine);
                            }
                        }
                    }
                }
                ok
            }
            ast::Pattern::Constructor {
                name,
                args,
                rest,
                span,
            } => self.type_ctor_pattern(name, args, *rest, *span, expected, b),
            ast::Pattern::Or { patterns, .. } => {
                // Scope the canonical binding set to this or-pattern: `enter_or`
                // snapshots the surrounding mode and the `initial` boundary and
                // types the first alternative in `Initial` mode; `exit_or`
                // restores the surrounding mode so a sibling binding after the
                // or-pattern is not stranded in leftover `Alternative` mode.
                let scope = b.enter_or();
                let mut ok = true;
                let mut iter = patterns.iter();
                if let Some(first) = iter.next() {
                    ok &= self.type_pattern(first, expected, b);
                }
                for alt in iter {
                    b.enter_alternative(&scope);
                    ok &= self.type_pattern(alt, expected, b);
                    ok &= b.finish_alternative(alt.span(), &mut self.engine);
                }
                b.exit_or(scope);
                ok
            }
        }
    }

    /// Resolve a constructor name to `(type_id, type_name, arity, field_labels, scheme)`.
    fn lookup_ctor(&self, name: &str) -> Option<(i32, String, usize, Vec<String>, Scheme)> {
        let scheme = *self.env.lookup(name)?;
        match scheme.kind {
            ValueKind::Constructor {
                type_id,
                type_name,
                arity,
                field_labels,
                ..
            } => Some((
                type_id,
                self.engine.str(type_name).to_string(),
                arity as usize,
                self.engine.strs_of(field_labels),
                scheme,
            )),
            _ => None,
        }
    }

    fn type_ctor_pattern(
        &mut self,
        name: &ast::Identifier,
        args: &[ast::PatternArg],
        rest: bool,
        span: Span,
        expected: Ty,
        b: &mut PatternBindings,
    ) -> bool {
        let Some((_, type_name, arity, field_labels, scheme)) = self.lookup_ctor(&name.name) else {
            let msg = if self.env.lookup(&name.name).is_some() {
                format!(
                    "'{}' is not a constructor and cannot be used in a pattern",
                    name.name
                )
            } else {
                format!("Unknown constructor '{}' in pattern", name.name)
            };
            self.error(msg, name.span);
            return false;
        };

        let inst = self.engine.instantiate(&scheme, &self.rigid_ids);
        let qualified = format!("{}.{}", type_name, name.name);
        let doc = self.env.lookup_doc(&qualified);
        self.record(&qualified, inst, name.span, doc);

        let r = self.engine.find(inst);
        match self.engine.node(r) {
            TypeNode::Fun { params, ret } => {
                let params: Vec<Ty> = self.engine.children_of(params).to_vec();
                // Bidirectional pivot: unify the constructor's return type with
                // the expected subject type FIRST so that the param types share
                // type-variable cells with the subject and recursing into args
                // refines the subject's type.
                let mut ok = self.engine.unify_at(expected, ret, span);

                let (by_pos, args_ok) = self.slot_ctor_args(
                    &name.name,
                    arity,
                    &field_labels,
                    args.iter().map(|a| match a {
                        ast::PatternArg::Positional(p) => (None, p, p.span()),
                        ast::PatternArg::Labeled { label, pattern } => {
                            (Some(label), pattern, label.span)
                        }
                    }),
                    if rest {
                        None
                    } else {
                        Some((span, ". Use '..' to ignore them"))
                    },
                    true,
                );
                ok &= args_ok;
                for (i, sub) in by_pos.iter().enumerate() {
                    if let Some(p) = sub {
                        let field_ty = params
                            .get(i)
                            .cloned()
                            .unwrap_or_else(|| self.engine.fresh_var());
                        ok &= self.type_pattern(p, field_ty, b);
                    }
                }
                ok
            }
            _ => {
                let mut ok = self.engine.unify_at(expected, r, span);
                if !args.is_empty() {
                    self.error(
                        format!(
                            "Constructor '{}' takes no arguments but {} were given",
                            name.name,
                            args.len()
                        ),
                        span,
                    );
                    ok = false;
                }
                ok
            }
        }
    }

    /// Slot positional + labelled constructor args into field-declaration
    /// order, emitting (when `diag`) too-many-positional / unknown-label /
    /// duplicate-field diagnostics, and a missing-fields diagnostic when
    /// `missing` is `Some((span, hint))`. Shared by ctor calls and patterns.
    fn slot_ctor_args<'a, T>(
        &mut self,
        name: &str,
        arity: usize,
        field_labels: &[String],
        args: impl Iterator<Item = (Option<&'a ast::Identifier>, &'a T, Span)>,
        missing: Option<(Span, &str)>,
        diag: bool,
    ) -> (Vec<Option<&'a T>>, bool) {
        let mut by_pos: Vec<Option<&'a T>> = vec![None; arity];
        let mut next_positional = 0usize;
        let mut ok = true;
        let label_at = |i: usize| field_labels.get(i).map(String::as_str).unwrap_or("_");
        for (label, val, sp) in args {
            let (idx, sp) = match label {
                None => {
                    let i = next_positional;
                    next_positional += 1;
                    if i >= arity {
                        if diag {
                            self.error(
                                format!(
                                    "Constructor '{}' has {} field(s) but more were supplied",
                                    name, arity
                                ),
                                sp,
                            );
                        }
                        ok = false;
                        continue;
                    }
                    (i, sp)
                }
                Some(l) => match field_labels.iter().position(|fl| fl == &l.name) {
                    Some(i) => (i, l.span),
                    None => {
                        if diag {
                            self.error(
                                format!(
                                    "Constructor '{}' has no field '{}'. Available: {}",
                                    name,
                                    l.name,
                                    field_labels.join(", ")
                                ),
                                l.span,
                            );
                        }
                        ok = false;
                        continue;
                    }
                },
            };
            if by_pos[idx].is_some() {
                if diag {
                    self.error(
                        format!("Field '{}' is specified more than once", label_at(idx)),
                        sp,
                    );
                }
                ok = false;
            }
            by_pos[idx] = Some(val);
        }
        if let Some((span, hint)) = missing {
            let absent: Vec<&str> = (0..arity)
                .filter(|i| by_pos[*i].is_none())
                .map(&label_at)
                .collect();
            if !absent.is_empty() {
                if diag {
                    self.error(
                        format!(
                            "Constructor '{}' is missing field(s): {}{}",
                            name,
                            absent.join(", "),
                            hint
                        ),
                        span,
                    );
                }
                ok = false;
            }
        }
        (by_pos, ok)
    }

    // ========================================================================
    // Pattern code generation (no typing)
    // ========================================================================

    /// Emit bytecode that matches a pattern against the value currently on top
    /// of the stack. The value is CONSUMED on every path. On a structural
    /// non-match a `JumpIfFalse` placeholder is appended to `fail_jumps` for
    /// the caller to patch. Variable bindings are stored to the local slot
    /// allocated for them by `compile_match` (looked up via `self.locals`), so
    /// `type_pattern` must have run for this arm beforehand.
    fn emit_pattern(&mut self, pattern: &ast::Pattern, fail_jumps: &mut Vec<i32>) {
        match pattern {
            ast::Pattern::Binary { segments, rest, .. } => {
                self.emit_binary_pattern(segments, rest.as_ref(), fail_jumps);
            }
            ast::Pattern::Wildcard { .. } => {
                self.emit(Op::Pop);
            }
            ast::Pattern::Var { name } => {
                if name.name == "_" {
                    self.emit(Op::Pop);
                } else {
                    let idx = self.get_or_create_local(&name.name);
                    self.emit_arg(Op::StoreLocal, idx);
                }
            }
            ast::Pattern::Literal(lit) => {
                let v = match lit {
                    ast::PatternLiteral::Number(n) => self.const_number(n),
                    ast::PatternLiteral::String(s) => self.frozen.str(s.value.as_str()),
                };
                let c = self.add_constant(v);
                self.emit_arg(Op::PushConst, c);
                self.emit(Op::Eq);
                fail_jumps.push(self.emit_jump(Op::JumpIfFalse));
            }
            ast::Pattern::Range { start, end, .. } => {
                let temp = self.spill_temp();

                self.emit_arg(Op::PushLocal, temp);
                self.emit_range_bound(start);
                self.emit(Op::Gte);
                fail_jumps.push(self.emit_jump(Op::JumpIfFalse));
                self.emit_arg(Op::PushLocal, temp);
                self.emit_range_bound(end);
                self.emit(Op::Lt);
                fail_jumps.push(self.emit_jump(Op::JumpIfFalse));
            }
            ast::Pattern::Tuple { elements, .. } => {
                let temp = self.spill_temp();

                for (i, elem) in elements.iter().enumerate() {
                    self.emit_arg(Op::PushLocal, temp);
                    self.emit_arg(Op::TupleIndex, i as i32);
                    self.emit_pattern(elem, fail_jumps);
                }
            }
            ast::Pattern::Array { elements, .. } => {
                let mut spread_idx: Option<usize> = None;
                for (i, e) in elements.iter().enumerate() {
                    if matches!(e, ast::ArrayPatternElement::Spread { .. }) {
                        spread_idx = Some(i);
                    }
                }
                let pre_count = spread_idx.unwrap_or(elements.len());

                let temp = self.spill_temp();

                self.emit_arg(Op::PushLocal, temp);
                self.emit(Op::ArrayLen);
                let pc = self.const_int(pre_count as i64);
                self.emit_arg(Op::PushConst, pc);
                if spread_idx.is_some() {
                    self.emit(Op::Gte);
                } else {
                    self.emit(Op::Eq);
                }
                fail_jumps.push(self.emit_jump(Op::JumpIfFalse));

                for (i, elem) in elements.iter().take(pre_count).enumerate() {
                    if let ast::ArrayPatternElement::Pattern(p) = elem {
                        // Length was checked above, so element `i` exists.
                        // `ElemAt` fetches it directly — no `Some(_)` box/unbox
                        // round-trip per element.
                        self.emit_arg(Op::PushLocal, temp);
                        self.emit_arg(Op::ElemAt, i as i32);
                        self.emit_pattern(p, fail_jumps);
                    }
                }

                if let Some(si) = spread_idx
                    && let ast::ArrayPatternElement::Spread { binding, .. } = &elements[si]
                    && let Some(id) = binding
                {
                    self.emit_arg(Op::PushLocal, temp);
                    let pc2 = self.const_int(pre_count as i64);
                    self.emit_arg(Op::PushConst, pc2);
                    self.emit(Op::Drop);
                    let local_idx = self.get_or_create_local(&id.name);
                    self.emit_arg(Op::StoreLocal, local_idx);
                }
            }
            ast::Pattern::Constructor { name, args, .. } => {
                self.emit_ctor_pattern(name, args, fail_jumps);
            }
            ast::Pattern::Or { patterns, .. } => {
                let temp = self.spill_temp();

                let mut matched_jumps: Vec<i32> = vec![];
                let mut overrides_pushed = false;
                for (i, alt) in patterns.iter().enumerate() {
                    let mut alt_fails: Vec<i32> = vec![];
                    self.emit_arg(Op::PushLocal, temp);
                    self.emit_pattern(alt, &mut alt_fails);
                    if i == 0 && patterns.len() > 1 {
                        // Typing guarantees every alternative binds the same
                        // names; pin them to the slots the first alternative
                        // chose so the arm body reads a slot that every
                        // alternative actually wrote.
                        let mut names = Vec::new();
                        Self::pattern_bound_names(alt, &mut names);
                        let map: HashMap<String, i32> = names
                            .into_iter()
                            .filter_map(|n| self.locals.get(&n).map(|e| (n, e.slot)))
                            .collect();
                        self.or_bind_overrides.push(map);
                        overrides_pushed = true;
                    }
                    if i < patterns.len() - 1 {
                        matched_jumps.push(self.emit_jump(Op::Jump));
                        self.patch_all(&alt_fails);
                    } else {
                        fail_jumps.extend(alt_fails);
                    }
                }
                if overrides_pushed {
                    self.or_bind_overrides.pop();
                }
                self.patch_all(&matched_jumps);
            }
        }
    }

    /// Destructure the `Binary` on top of the stack against a `<<>>` pattern.
    ///
    /// A bit cursor walks the value. The cursor is tracked at COMPILE TIME
    /// (`Some(bit_offset)`) for as long as every preceding segment has a
    /// statically-known width — literal prefixes, Int segments with literal
    /// sizes, `bytes(k)` slices with literal `k` — and is spilled into a
    /// runtime temp only at the first variable-width segment (a `:utf8`
    /// codepoint or a runtime size expression). Fixed-layout patterns
    /// therefore compile to constant-offset reads with consolidated bounds
    /// checks and no cursor arithmetic at all.
    ///
    /// Consecutive literal segments (string literals, integer literals with
    /// literal widths) are coalesced into one constant prefix compared with a
    /// single `Op::BinMatchPrefix`: `<<'HTTP/1.1', ..rest>>` is one byte
    /// compare, not eight read/compare pairs.
    ///
    /// Binary-kind segments and `..rest` bind zero-copy views via
    /// `Op::BinView` (no `Result` box) — the surrounding bounds checks have
    /// already proven the range valid.
    ///
    /// With no trailing `..rest` the binary must be consumed exactly; `..`
    /// allows (and optionally binds) the remainder.
    fn emit_binary_pattern(
        &mut self,
        segments: &[ast::BinSegmentPat],
        rest: Option<&ast::BinaryPatternRest>,
        fail_jumps: &mut Vec<i32>,
    ) {
        use BinOperand::{Const, Local};

        // scrutinee Binary -> bin_temp (consumed off the stack on every path).
        let bin_temp = self.spill_temp();

        self.emit_arg(Op::PushLocal, bin_temp);
        self.emit(Op::BinBitSize);
        let total_temp = self.spill_temp();

        // Per-segment static widths; None = width known only at runtime.
        let widths: Vec<Option<i64>> = segments.iter().map(Self::static_segment_bits).collect();
        let all_static = widths.iter().all(|w| w.is_some());

        // The bit cursor: `Some(k)` while statically known, `None` once it
        // lives in cursor_temp. The temp slot is reserved up front but written
        // only if the cursor actually goes dynamic.
        let cursor_temp = self.alloc_temp();
        let mut cursor: Option<i64> = Some(0);
        // Static bit offset already proven in bounds by an emitted check.
        let mut checked: i64 = 0;

        // Fully-static exact-length pattern: ONE up-front total-length check
        // covers every segment read and replaces the trailing
        // exact-consumption check.
        if all_static && rest.is_none() {
            let static_total: i64 = widths.iter().flatten().sum();
            let c = self.const_int(static_total);
            self.emit_arg(Op::PushConst, c);
            self.emit_arg(Op::PushLocal, total_temp);
            self.emit(Op::Eq);
            fail_jumps.push(self.emit_jump(Op::JumpIfFalse));
            checked = static_total;
        }

        let mut i = 0;
        while i < segments.len() {
            // At a static cursor, one bounds check covers the whole run of
            // statically-sized segments ahead of it.
            if let Some(at) = cursor
                && widths[i].is_some()
            {
                let mut end = at;
                for w in &widths[i..] {
                    match w {
                        Some(bits) => end += bits,
                        None => break,
                    }
                }
                if end > checked {
                    let c = self.const_int(end);
                    self.emit_arg(Op::PushConst, c);
                    self.emit_arg(Op::PushLocal, total_temp);
                    self.emit(Op::Lte);
                    fail_jumps.push(self.emit_jump(Op::JumpIfFalse));
                    checked = end;
                }
            }

            // Literal segments: coalesce the run starting here into one
            // constant prefix and compare it with a single op.
            if let Some((bytes, bit_len, run)) = Self::literal_prefix_run(&segments[i..]) {
                let c = self.const_binary(bytes, bit_len);
                self.emit_arg(Op::PushLocal, bin_temp);
                self.emit_push_cursor(cursor, cursor_temp);
                self.emit_arg(Op::PushConst, c);
                self.emit(Op::BinMatchPrefix);
                fail_jumps.push(self.emit_jump(Op::JumpIfFalse));
                self.emit_advance_cursor(&mut cursor, cursor_temp, Const(bit_len as i64));
                i += run;
                continue;
            }

            let seg = &segments[i];
            let width = widths[i];
            i += 1;

            // `:utf8` codepoint segment — variable width: read one codepoint,
            // match it, then advance by however many bits it consumed
            // (0 == decode failure).
            if seg.kind == ast::BinKind::Utf8 {
                self.emit_spill_cursor(&mut cursor, cursor_temp);
                self.emit_arg(Op::PushLocal, bin_temp);
                self.emit_arg(Op::PushLocal, cursor_temp);
                self.emit(Op::BinReadUtf8);
                let nbits_temp = self.spill_temp();
                let cp_temp = self.spill_temp();

                self.emit_arg(Op::PushLocal, nbits_temp);
                let zc = self.const_int(0);
                self.emit_arg(Op::PushConst, zc);
                self.emit(Op::Gt);
                fail_jumps.push(self.emit_jump(Op::JumpIfFalse));

                self.emit_arg(Op::PushLocal, cp_temp);
                self.emit_pattern(&seg.value, fail_jumps);

                self.emit_advance_cursor(&mut cursor, cursor_temp, Local(nbits_temp));
                continue;
            }

            let width_src = if let Some(bits) = width {
                // Statically-sized segment. At a static cursor bounds were
                // proven by the consolidated check; at a runtime cursor each
                // segment checks cursor + width <= total itself.
                if cursor.is_none() {
                    self.emit_arg(Op::PushLocal, cursor_temp);
                    self.emit_push_operand(Const(bits));
                    self.emit(Op::Add);
                    self.emit_arg(Op::PushLocal, total_temp);
                    self.emit(Op::Lte);
                    fail_jumps.push(self.emit_jump(Op::JumpIfFalse));
                }
                Const(bits)
            } else {
                // Runtime-sized segment: a dynamic `size(e)`/`bytes(e)`, or a
                // sizeless `:binary` slice that consumes the remaining bits.
                let bits_temp = self.alloc_temp();
                if let Some(e) = &seg.size {
                    let ety = self.compile_expr(e);
                    let int_ty = self.ty_int();
                    self.engine.unify_at(int_ty, ety, e.span());
                    if seg.unit == ast::BinUnit::Bytes {
                        let c = self.const_int(8);
                        self.emit_arg(Op::PushConst, c);
                        self.emit(Op::Mul);
                    }
                    self.emit_arg(Op::StoreLocal, bits_temp);

                    // Bounds: cursor + width <= total.
                    self.emit_spill_cursor(&mut cursor, cursor_temp);
                    self.emit_arg(Op::PushLocal, cursor_temp);
                    self.emit_arg(Op::PushLocal, bits_temp);
                    self.emit(Op::Add);
                    self.emit_arg(Op::PushLocal, total_temp);
                    self.emit(Op::Lte);
                    fail_jumps.push(self.emit_jump(Op::JumpIfFalse));
                } else {
                    // `:binary` with no size: all remaining bits. In bounds
                    // by the cursor <= total invariant; no check needed.
                    self.emit_spill_cursor(&mut cursor, cursor_temp);
                    self.emit_arg(Op::PushLocal, total_temp);
                    self.emit_arg(Op::PushLocal, cursor_temp);
                    self.emit(Op::Sub);
                    self.emit_arg(Op::StoreLocal, bits_temp);
                }
                Local(bits_temp)
            };
            self.emit_segment_read(
                bin_temp,
                width_src,
                seg,
                &mut cursor,
                cursor_temp,
                fail_jumps,
            );
        }

        match rest {
            Some(r) => {
                if let Some(id) = &r.binding {
                    // rest = bin[cursor .. total]  (cursor <= total holds).
                    self.emit_arg(Op::PushLocal, bin_temp);
                    self.emit_push_cursor(cursor, cursor_temp);
                    self.emit_arg(Op::PushLocal, total_temp);
                    self.emit_push_cursor(cursor, cursor_temp);
                    self.emit(Op::Sub);
                    self.emit(Op::BinView);
                    let idx = self.get_or_create_local(&id.name);
                    self.emit_arg(Op::StoreLocal, idx);
                }
            }
            None => {
                // No rest: the binary must be consumed exactly. Fully-static
                // patterns already proved this with the up-front length check.
                if !(all_static && cursor.is_some()) {
                    self.emit_push_cursor(cursor, cursor_temp);
                    self.emit_arg(Op::PushLocal, total_temp);
                    self.emit(Op::Eq);
                    fail_jumps.push(self.emit_jump(Op::JumpIfFalse));
                }
            }
        }
    }

    /// Push an operand: a pooled int constant or a local slot.
    fn emit_push_operand(&mut self, src: BinOperand) {
        match src {
            BinOperand::Const(v) => {
                let c = self.const_int(v);
                self.emit_arg(Op::PushConst, c);
            }
            BinOperand::Local(slot) => self.emit_arg(Op::PushLocal, slot),
        }
    }

    /// Push the current bit cursor: a constant when statically known, the
    /// runtime temp otherwise.
    fn emit_push_cursor(&mut self, cursor: Option<i64>, cursor_temp: i32) {
        let src = cursor.map_or(BinOperand::Local(cursor_temp), BinOperand::Const);
        self.emit_push_operand(src);
    }

    /// Read `width` bits at the current cursor — a zero-copy view for
    /// `:binary` segments, an int read otherwise — match the result against
    /// the segment's value pattern, and advance the cursor past the segment.
    fn emit_segment_read(
        &mut self,
        bin_temp: i32,
        width: BinOperand,
        seg: &ast::BinSegmentPat,
        cursor: &mut Option<i64>,
        cursor_temp: i32,
        fail_jumps: &mut Vec<i32>,
    ) {
        self.emit_arg(Op::PushLocal, bin_temp);
        self.emit_push_cursor(*cursor, cursor_temp);
        self.emit_push_operand(width);
        if seg.kind == ast::BinKind::Binary {
            self.emit(Op::BinView);
        } else {
            self.emit(Op::BinReadInt);
        }
        self.emit_pattern(&seg.value, fail_jumps);
        self.emit_advance_cursor(cursor, cursor_temp, width);
    }

    /// Advance the cursor by `by` bits: arithmetic at compile time while both
    /// cursor and width are static, an add-and-store otherwise (spilling the
    /// cursor first if needed).
    fn emit_advance_cursor(&mut self, cursor: &mut Option<i64>, cursor_temp: i32, by: BinOperand) {
        if let (Some(k), BinOperand::Const(b)) = (*cursor, by) {
            *cursor = Some(k + b);
            return;
        }
        self.emit_spill_cursor(cursor, cursor_temp);
        self.emit_arg(Op::PushLocal, cursor_temp);
        self.emit_push_operand(by);
        self.emit(Op::Add);
        self.emit_arg(Op::StoreLocal, cursor_temp);
    }

    /// Force the cursor into its runtime temp (writing the temp if it was
    /// still static). After this, `cursor_temp` holds the live cursor.
    fn emit_spill_cursor(&mut self, cursor: &mut Option<i64>, cursor_temp: i32) {
        if let Some(k) = *cursor {
            let c = self.const_int(k);
            self.emit_arg(Op::PushConst, c);
            self.emit_arg(Op::StoreLocal, cursor_temp);
            *cursor = None;
        }
    }

    /// The compile-time bit width of a pattern segment, or `None` when it is
    /// known only at runtime: a dynamic size expression, a `:utf8` codepoint,
    /// or a sizeless `:binary` (rest-of-binary) slice.
    fn static_segment_bits(seg: &ast::BinSegmentPat) -> Option<i64> {
        match seg.kind {
            // String literals have a fixed UTF-8 width; codepoints don't.
            ast::BinKind::Utf8 => Self::utf8_literal_segment(seg).map(|s| (s.len() as i64) * 8),
            ast::BinKind::Int | ast::BinKind::Binary => match &seg.size {
                None => (seg.kind == ast::BinKind::Int).then_some(8),
                Some(ast::Expression::NumberLiteral(n)) => {
                    let v = n.value.parse::<i64>().ok()?;
                    Some(if seg.unit == ast::BinUnit::Bytes {
                        v.max(0) * 8
                    } else {
                        v.max(0)
                    })
                }
                Some(_) => None,
            },
        }
    }

    /// The string of a `<<'literal'>>` (Utf8 string-literal) pattern segment.
    fn utf8_literal_segment(seg: &ast::BinSegmentPat) -> Option<&str> {
        if seg.kind != ast::BinKind::Utf8 {
            return None;
        }
        match &seg.value {
            ast::Pattern::Literal(ast::PatternLiteral::String(s)) => Some(&s.value),
            _ => None,
        }
    }

    /// Coalesce the longest run of literal segments starting at `segs[0]` into
    /// one constant bit-string: string literals contribute their UTF-8 bytes,
    /// integer literals their value encoded MSB-first at their (literal)
    /// width, mirroring `Op::BinFromInt`. Returns the encoded bytes, the bit
    /// length, and how many segments the run covers; `None` when `segs[0]` is
    /// not a literal segment.
    fn literal_prefix_run(segs: &[ast::BinSegmentPat]) -> Option<(Vec<u8>, u64, usize)> {
        let mut bytes: Vec<u8> = Vec::new();
        let mut bit_len: u64 = 0;
        let mut run = 0;
        for seg in segs {
            if let Some(s) = Self::utf8_literal_segment(seg) {
                push_literal_bytes(&mut bytes, &mut bit_len, s.as_bytes());
                run += 1;
                continue;
            }
            if seg.kind == ast::BinKind::Int
                && let ast::Pattern::Literal(ast::PatternLiteral::Number(n)) = &seg.value
                && let Ok(v) = n.value.parse::<i64>()
                && let Some(bits) = Self::static_segment_bits(seg)
            {
                push_literal_bits(&mut bytes, &mut bit_len, v, bits as u64);
                run += 1;
                continue;
            }
            break;
        }
        (run > 0).then_some((bytes, bit_len, run))
    }

    /// Collect every name a pattern binds, in source order. For an or-pattern
    /// only the first alternative is walked — typing already enforces that all
    /// alternatives bind the identical set.
    fn pattern_bound_names(pattern: &ast::Pattern, out: &mut Vec<String>) {
        pattern.for_each_binder(ast::OrAlternatives::First, &mut |b| {
            if let ast::PatternBinder::Name(id) = b {
                out.push(id.name.clone());
            }
        });
    }

    fn emit_ctor_pattern(
        &mut self,
        name: &ast::Identifier,
        args: &[ast::PatternArg],
        fail_jumps: &mut Vec<i32>,
    ) {
        let Some((type_id, _, arity, field_labels, _)) = self.lookup_ctor(&name.name) else {
            // Typing already reported the error; consume the value and bail.
            self.emit(Op::Pop);
            return;
        };

        if self.try_emit_bool_match(type_id, &name.name, fail_jumps) {
            return;
        }

        let temp = self.spill_temp();

        self.emit_arg(Op::PushLocal, temp);
        self.emit_match_header(type_id, &name.name);
        self.emit(Op::MatchEnum);
        fail_jumps.push(self.emit_jump(Op::JumpIfFalse));

        if arity == 0 {
            return;
        }

        // Typing already emitted reorder diagnostics; suppress duplicates here.
        let (by_pos, _) = self.slot_ctor_args(
            &name.name,
            arity,
            &field_labels,
            args.iter().map(|a| match a {
                ast::PatternArg::Positional(p) => (None, p, p.span()),
                ast::PatternArg::Labeled { label, pattern } => (Some(label), pattern, label.span),
            }),
            None,
            false,
        );

        if !by_pos.iter().any(|p| p.is_some()) {
            return;
        }

        self.emit_arg(Op::PushLocal, temp);
        self.emit(Op::UnwrapEnum);
        // VM pushes payloads in declaration order (last on top); spill them
        // to temps so each can be pattern-matched independently.
        let temp_base = self.local_count;
        self.local_count += arity as i32;
        for i in (0..arity as i32).rev() {
            self.emit_arg(Op::StoreLocal, temp_base + i);
        }
        for (i, sub) in by_pos.iter().enumerate() {
            if let Some(p) = sub {
                self.emit_arg(Op::PushLocal, temp_base + i as i32);
                self.emit_pattern(p, fail_jumps);
            }
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
            Ok(v) => v,
            Err(e) => {
                self.error(e.message(&n.value), n.span);
                e.recovery(&mut self.frozen)
            }
        }
    }

    fn emit_range_bound(&mut self, p: &ast::Pattern) {
        if let ast::Pattern::Literal(ast::PatternLiteral::Number(n)) = p {
            let v = self.const_number(n);
            let c = self.add_constant(v);
            self.emit_arg(Op::PushConst, c);
        } else {
            // type_pattern already errored on non-numeric bounds; emit a 0 so
            // the bytecode stays balanced.
            let c = self.const_int(0);
            self.emit_arg(Op::PushConst, c);
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

/// Operand source for binary-pattern codegen: a constant or a local slot.
#[derive(Clone, Copy)]
enum BinOperand {
    Const(i64),
    Local(i32),
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
    fn recovery(&self, frozen: &mut FrozenBuilder) -> Value {
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
fn number_literal_value(s: &str, frozen: &mut FrozenBuilder) -> Result<Value, NumLitError> {
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

// --- Compile-time bit-string building for `<<>>` literal pattern prefixes ---

/// Append one bit (MSB-first addressing) onto a bit-string accumulator.
fn push_literal_bit(bytes: &mut Vec<u8>, bit_len: &mut u64, bit: u8) {
    let idx = (*bit_len / 8) as usize;
    if idx >= bytes.len() {
        bytes.push(0);
    }
    bytes[idx] |= (bit & 1) << (7 - (*bit_len % 8));
    *bit_len += 1;
}

/// Append the low `n_bits` of `value` MSB-first, mirroring `Op::BinFromInt`'s
/// runtime encoding so a coalesced literal prefix is bit-identical to what the
/// segments would have produced one by one.
fn push_literal_bits(bytes: &mut Vec<u8>, bit_len: &mut u64, value: i64, n_bits: u64) {
    let v = value as u64;
    for i in 0..n_bits {
        let pos = n_bits - 1 - i;
        let bit = if pos < 64 { ((v >> pos) & 1) as u8 } else { 0 };
        push_literal_bit(bytes, bit_len, bit);
    }
}

/// Append whole bytes; a byte-aligned accumulator (the common case) extends
/// directly, an unaligned one packs bit by bit.
fn push_literal_bytes(bytes: &mut Vec<u8>, bit_len: &mut u64, new: &[u8]) {
    if bit_len.is_multiple_of(8) {
        bytes.extend_from_slice(new);
        *bit_len += (new.len() as u64) * 8;
    } else {
        for &b in new {
            push_literal_bits(bytes, bit_len, b as i64, 8);
        }
    }
}

pub(super) fn def_loc(sp: Span, module: ArenaSlice, entity: EntityKind) -> DefinitionLocation {
    DefinitionLocation {
        line: sp.start_line,
        column: sp.start_column,
        end_col: sp.end_column,
        module,
        entity,
    }
}

/// The declaring-name span carried by a [`DefinitionLocation`]. A declaring
/// identifier is always single-line, so the reconstructed span is exactly the
/// one [`def_loc`] was handed — keeping a definition's `DefId` equal to the
/// `DefId` every occurrence of it targets.
fn def_loc_span(dl: &DefinitionLocation) -> Span {
    Span {
        start_line: dl.line,
        start_column: dl.column,
        end_line: dl.line,
        end_column: dl.end_col,
    }
}

impl Compiler {
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
            match &cm.module_refs {
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
    fn finalize_references(&mut self) -> (Rc<ReferenceGraph>, Vec<HoverFact>) {
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

#[cfg(test)]
mod bug2_local_binders_as_definitions {
    //! Local binders — `name = ..` bindings, fn/lambda parameters, pattern
    //! binders (match arms, tuple/array/ctor destructure), and `or`-receivers —
    //! must be registered as graph `Definition`s, not merely typed in the env.
    //! Without the `Definition` record `ReferenceGraph::definition(target)`
    //! returns `None`, so goto-def / find-refs / hover (the handler path, which
    //! goes through `definition_at` / `references_to`) are dead on every local.
    //! Emission is gated on `collect_hover_facts`, so this is LSP-only and the
    //! `al run` / `al check` graph stays untouched.

    use super::*;
    use crate::parser::new_parser;
    use crate::reference::ReferenceKind;
    use crate::scanner::new_scanner;

    /// Compile `src` as the entry module on the LSP path (`collect_hover_facts`
    /// on, so local-def emission fires) and hand back the populated collector.
    fn collect(src: &str) -> Compiler {
        let mut s = new_scanner(src.to_string());
        let pr = new_parser(&mut s).parse_program();
        assert!(
            !crate::diagnostic::has_errors(&pr.diagnostics),
            "snippet failed to parse: {:?}",
            pr.diagnostics,
        );
        let block = pr.ast;
        let mut c = new_compiler(None, true);
        c.collect_hover_facts = true;
        c.register_prelude();
        assert!(
            !crate::diagnostic::has_errors(&c.engine.diagnostics),
            "prelude failed to load: {:?}",
            c.engine.diagnostics,
        );
        c.process_imports(&block);
        c.analyse_module(&block, None);
        assert!(
            !crate::diagnostic::has_errors(&c.engine.diagnostics),
            "snippet failed to compile: {:?}",
            c.engine.diagnostics,
        );
        c
    }

    /// The single `DefId` declared under `name` in the entry collector.
    fn sole_def(c: &Compiler, name: &str) -> DefId {
        let defs = c.module_refs.defs_named(name);
        assert_eq!(
            defs.len(),
            1,
            "expected exactly one `{name}` def, got {defs:?}"
        );
        defs[0]
    }

    /// Whether any recorded occurrence is an unqualified *use* of `target`.
    fn has_use(c: &Compiler, target: DefId) -> bool {
        c.module_refs
            .occurrences()
            .iter()
            .any(|o| o.target == target && o.kind == ReferenceKind::Unqualified)
    }

    #[test]
    fn every_local_binder_kind_is_a_graph_definition() {
        let c = collect(
            "fn ident(p Int) Int {\n\
            \x20 v = p\n\
            \x20 v\n\
            }\n\
            \n\
            fn matcher(o Option(Int)) Int {\n\
            \x20 match o {\n\
            \x20   Some(inner) -> inner\n\
            \x20   None -> 0\n\
            \x20 }\n\
            }\n\
            \n\
            fn recover(r Result(Int, Int)) Int {\n\
            \x20 r or e -> e\n\
            }\n",
        );

        // p: bind_param, v: var binding, inner: pattern binder, e: or-receiver.
        for name in ["p", "v", "inner", "e"] {
            let d = sole_def(&c, name);
            assert_eq!(d.entity, EntityKind::Value, "`{name}` is not a Value def");
            assert!(
                c.module_refs.definition(d).is_some(),
                "`{name}` binder was not registered as a graph Definition",
            );
            // The use site already recorded an Unqualified occurrence targeting
            // this binder; with the Definition present the goto-def / find-refs
            // chain (occurrence -> target -> definition()) now closes.
            assert!(has_use(&c, d), "no recorded use targets `{name}`");
        }
    }

    #[test]
    fn goto_def_on_a_local_use_resolves_to_a_real_definition() {
        // Mirrors the handler path: resolve_position(use) -> target, then
        // definition(target) — the latter returned `None` before the fix.
        let c = collect("fn f(p Int) Int {\n  v = p\n  v\n}\n");
        let v = sole_def(&c, "v");

        let target = c
            .module_refs
            .resolve_position(2, 2)
            .expect("cursor on the `v` use resolves to a target");
        assert_eq!(target, v);
        assert!(
            c.module_refs.definition(target).is_some(),
            "use resolved to a target with no Definition record",
        );
    }

    #[test]
    fn shadowing_keeps_inner_and_outer_as_distinct_definitions() {
        // Two sequential `x` binders -> two distinct DefIds (distinct identifier
        // spans). `define_at` overwrites the env, so the RHS of `x = x` (compiled
        // before the second binder lands) sees the outer and the trailing `x`
        // the inner.
        let c = collect("fn f(s Int) Int {\n  x = s\n  x = x\n  x\n}\n");
        let mut defs = c.module_refs.defs_named("x").to_vec();
        assert_eq!(defs.len(), 2, "expected outer + inner `x`, got {defs:?}");
        defs.sort_by_key(|d| d.span.start_line);
        let (outer, inner) = (defs[0], defs[1]);
        assert_ne!(outer, inner);
        assert!(c.module_refs.definition(outer).is_some());
        assert!(c.module_refs.definition(inner).is_some());

        // RHS use on line 2 targets the OUTER binder; the trailing `x` on line 3
        // targets the INNER one.
        assert_eq!(c.module_refs.resolve_position(2, 6), Some(outer));
        assert_eq!(c.module_refs.resolve_position(3, 2), Some(inner));
    }
}
