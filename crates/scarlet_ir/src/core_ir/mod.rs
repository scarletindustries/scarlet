//! Core IR: typed A-Normal Form, what the compiler hands a backend. Every
//! subexpression is a `Let`-bound `LocalId`, so last-use, drop and reuse fall
//! out of a linear backward scan (Perceus, ICFP'22 frame-limited). The passes
//! over it live in the compiler; `docs/vm-design.md` says what running it
//! means.

mod prim;

pub use prim::PrimOp;

use std::collections::BTreeMap;
use std::fmt;
use std::rc::Rc;

use crate::TypeId;
use crate::intrinsic::Intrinsic;
use crate::newtype_index;
use crate::rty::{RTy, ResolvedPool};
use crate::tivec::TiVec;

// The core IR's index spaces. Each is a `crate::tivec::Idx`, so the `TiVec` it
// indexes rejects the others at compile time. `GlobalSlot` (entry-frame stack
// space) is minted in `typed_ir`.

newtype_index!(
    /// Dense per-function local index. Minted by `lower` in evaluation order so
    /// a backward scan over the `Let` spine sees ids decrease monotonically.
    pub struct LocalId("%")
);

newtype_index!(
    /// Index into [`Program::consts`].
    pub struct ConstId("c")
);

/// A constant the IR names by [`ConstId`]: a literal's value, or one the
/// elaborator needs with no literal behind it (a pattern's length, a segment's
/// width).
#[derive(Debug, Clone)]
pub enum Const {
    Int(i64),
    Float(f64),
    String(String),
    /// `bit_len` bits, packed into `bytes` from the most significant bit.
    Binary {
        bytes: Vec<u8>,
        bit_len: u64,
    },
}

/// A [`Const`]'s identity. A float is compared by its bits, so `0.0` and
/// `-0.0` stay two constants, and a constant is always equal to itself.
#[derive(PartialEq, Eq, Hash)]
enum ConstKey<'a> {
    Int(i64),
    Float(u64),
    String(&'a str),
    Binary(&'a [u8], u64),
}

impl Const {
    fn key(&self) -> ConstKey<'_> {
        match self {
            Const::Int(i) => ConstKey::Int(*i),
            Const::Float(f) => ConstKey::Float(f.to_bits()),
            Const::String(s) => ConstKey::String(s),
            Const::Binary { bytes, bit_len } => ConstKey::Binary(bytes, *bit_len),
        }
    }
}

impl PartialEq for Const {
    fn eq(&self, other: &Self) -> bool {
        self.key() == other.key()
    }
}

impl Eq for Const {}

impl std::hash::Hash for Const {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.key().hash(state);
    }
}

newtype_index!(
    /// Labelled-continuation index, scoped to one lowered body. Declared by
    /// [`CoreExpr::LetCont`], transferred to by [`CoreExpr::Goto`]. Names code,
    /// never a value.
    pub struct JoinId("j")
);

newtype_index!(
    /// Index into `CoreProgram.fns`, numbered the same as `TypedProgram::fns`.
    pub struct FuncIdx("fn#")
);

/// A typed local binding. Its [`RTy`] indexes the elaborator's `ResolvedPool`,
/// where an unsolved inference variable is unrepresentable, so Perceus cannot
/// be handed a type that answers `is_heap` `false` because inference lost it.
#[derive(Debug, Clone)]
pub struct CoreBind {
    pub id: LocalId,
    pub ty: RTy,
    /// `Some(slot)` when this bind is a module-toplevel decl that must land in
    /// that global slot: function bodies read it with `Load::Global(slot)`, so
    /// the toplevel has to write the same slot.
    ///
    /// On the bind rather than in a `LocalId`-keyed side table, which would
    /// desync the moment a pass renumbers or drops a bind.
    pub global: Option<GlobalSlot>,
}

impl CoreBind {
    /// Fresh bind, pinned to no slot. `lower` pins [`Self::global`] afterwards.
    pub fn new(id: LocalId, ty: RTy) -> Self {
        CoreBind {
            id,
            ty,
            global: None,
        }
    }
}

/// Resolved constructor identity, captured at lowering so a backend need not
/// consult the type environment for dispatch. Perceus pairs drops on shape
/// equality: `type_id`, `variant_idx`, arity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VariantRef {
    pub type_id: TypeId,
    pub variant_idx: u16,
}

/// The stdlib constructors the VM builds on its own, like the `Some(x)` that
/// `xs[i]` gives. The compiler says which constructor each one is; the VM
/// knows nothing else about the stdlib's types.
#[derive(Debug, Clone, Copy)]
pub struct Abi {
    /// `Some(value)`.
    pub some: VariantRef,
    /// `None`.
    pub none: VariantRef,
    /// `Ok(value)`.
    pub ok: VariantRef,
    /// `Err(error)`.
    pub err: VariantRef,
    /// `scarlet/binary.Radix`, when the program loads that module: the base
    /// `binary.parse_int` and `binary.from_int_ascii` are asked for.
    pub radix: Option<Radix>,
    /// `scarlet/io.IoError`, when the program loads that module: what a file
    /// read or write that failed says went wrong.
    pub io: Option<IoErrors>,
    /// `scarlet/json`'s types, when the program loads that module: what
    /// `json.parse` gives and `json.encode` reads.
    pub json: Option<JsonTypes>,
    /// `scarlet/http/h1`'s types, when the program loads that module.
    pub http: Option<HttpTypes>,
}

/// The constructors of `scarlet/binary.Radix`.
#[derive(Debug, Clone, Copy)]
pub struct Radix {
    pub dec: VariantRef,
    pub hex: VariantRef,
}

/// The constructors of `scarlet/io.IoError` the VM builds, each named for the
/// OS error it stands for.
#[derive(Debug, Clone, Copy)]
pub struct IoErrors {
    /// These hold the path the operation was given.
    pub not_found: VariantRef,
    pub permission_denied: VariantRef,
    pub already_exists: VariantRef,
    pub not_a_directory: VariantRef,
    pub is_a_directory: VariantRef,
    pub read_only_filesystem: VariantRef,
    pub filesystem_loop: VariantRef,
    pub file_too_large: VariantRef,
    /// These hold nothing.
    pub storage_full: VariantRef,
    pub quota_exceeded: VariantRef,
    pub unaligned_binary: VariantRef,
    /// Any other OS error: this holds its number.
    pub errno: VariantRef,
}

/// The constructors of `scarlet/http/h1` and `scarlet/http/headers` the VM
/// builds or reads: what a parsed head, a body's framing and a decoded chunked
/// body are.
#[derive(Debug, Clone, Copy)]
pub struct HttpTypes {
    /// `headers.Header(name, value)`.
    pub header: VariantRef,
    /// `h1.Version`.
    pub http10: VariantRef,
    pub http11: VariantRef,
    /// `h1.HeadFlags(conn, expect_100_continue)`, and `h1.ConnTokens`.
    pub head_flags: VariantRef,
    pub conn_neither: VariantRef,
    pub conn_close: VariantRef,
    pub conn_keep_alive: VariantRef,
    pub conn_both: VariantRef,
    /// `h1.Parsed`: a request head.
    pub parsed_done: VariantRef,
    pub parsed_need_more: VariantRef,
    pub parsed_bad: VariantRef,
    /// `h1.ParsedResponse`, and `h1.BadResponse`'s reasons.
    pub response_done: VariantRef,
    pub response_need_more: VariantRef,
    pub response_bad: VariantRef,
    pub bad_status_line: VariantRef,
    pub bad_version: VariantRef,
    pub bad_field: VariantRef,
    pub head_too_large: VariantRef,
    pub bad_framing: VariantRef,
    /// `h1.Framing`.
    pub no_body: VariantRef,
    pub length: VariantRef,
    pub chunked: VariantRef,
    pub framing_invalid: VariantRef,
    /// `h1.ChunkBody`.
    pub chunked_done: VariantRef,
    pub chunked_need_more: VariantRef,
    pub chunked_bad: VariantRef,
}

impl HttpTypes {
    fn variants(&self) -> [VariantRef; 26] {
        [
            self.header,
            self.http10,
            self.http11,
            self.head_flags,
            self.conn_neither,
            self.conn_close,
            self.conn_keep_alive,
            self.conn_both,
            self.parsed_done,
            self.parsed_need_more,
            self.parsed_bad,
            self.response_done,
            self.response_need_more,
            self.response_bad,
            self.bad_status_line,
            self.bad_version,
            self.bad_field,
            self.head_too_large,
            self.bad_framing,
            self.no_body,
            self.length,
            self.chunked,
            self.framing_invalid,
            self.chunked_done,
            self.chunked_need_more,
            self.chunked_bad,
        ]
    }
}

/// The constructors of `scarlet/json` the VM builds or reads.
#[derive(Debug, Clone, Copy)]
pub struct JsonTypes {
    /// `Doc(arena, tape, idx)`: a parsed document, and every cursor into it.
    pub doc: VariantRef,
    /// `ParseError(offset, message)`.
    pub parse_error: VariantRef,
    /// `Json`'s, which `json.encode` writes out.
    pub null: VariantRef,
    pub boolean: VariantRef,
    pub integer: VariantRef,
    pub real: VariantRef,
    pub str: VariantRef,
    pub list: VariantRef,
    pub object: VariantRef,
    pub number: VariantRef,
}

impl JsonTypes {
    fn variants(&self) -> [VariantRef; 10] {
        [
            self.doc,
            self.parse_error,
            self.null,
            self.boolean,
            self.integer,
            self.real,
            self.str,
            self.list,
            self.object,
            self.number,
        ]
    }
}

impl IoErrors {
    fn variants(&self) -> [VariantRef; 12] {
        [
            self.not_found,
            self.permission_denied,
            self.already_exists,
            self.not_a_directory,
            self.is_a_directory,
            self.read_only_filesystem,
            self.filesystem_loop,
            self.file_too_large,
            self.storage_full,
            self.quota_exceeded,
            self.unaligned_binary,
            self.errno,
        ]
    }
}

impl Abi {
    /// Every constructor here, so their types' names can travel with them.
    pub fn variants(&self) -> Vec<VariantRef> {
        let mut all = vec![self.some, self.none, self.ok, self.err];
        if let Some(r) = self.radix {
            all.extend([r.dec, r.hex]);
        }
        if let Some(io) = self.io {
            all.extend(io.variants());
        }
        if let Some(json) = self.json {
            all.extend(json.variants());
        }
        if let Some(http) = self.http {
            all.extend(http.variants());
        }
        all
    }
}

/// What a type and its constructors are called, for anything that shows a
/// value of it to a person: `Some(1)`, `Point{ x: 1, y: 2 }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeNames {
    /// As declared, which is not always how an importer spells it.
    pub name: String,
    /// Indexed by [`VariantRef::variant_idx`].
    pub variants: Vec<VariantNames>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariantNames {
    pub name: String,
    /// Each field's label, in the order the constructor holds its fields.
    pub fields: Vec<String>,
}

/// Heap-cell shape for Perceus reuse pairing: a constructor cell with this
/// many fields. A `Drop` may only hand its cell to a `Ctor` of the same shape,
/// so the overwrite needs no resize.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReuseShape {
    pub fields: u16,
}

impl ReuseShape {
    #[allow(clippy::expect_used)] // ctor arity is bounded far below u16::MAX upstream
    pub fn ctor(arity: usize) -> Self {
        ReuseShape {
            fields: u16::try_from(arity).expect("constructor arity exceeds u16"),
        }
    }
}

/// Call target after resolution: a function known at compile time, the
/// function itself, or a closure held in a local.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Callee {
    Known(FuncIdx),
    Self_,
    Local(LocalId),
}

/// A slot in the entry (module) frame: where a module-scope binding lives. A
/// module-scope name may be bound more than once (an import shadowed by a later
/// `let`), so each binding carries the slot its own binding lands in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GlobalSlot(pub i32);

/// A slot in the *current* frame. A different index
/// space from [`GlobalSlot`] and [`CaptureIdx`], kept distinct so the three
/// cannot be swapped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FrameSlot(pub i32);

/// An index into the current closure's capture array.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CaptureIdx(pub i32);

/// A value read from somewhere other than a core-IR local.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Load {
    /// A raw frame slot the module walk assigned (a selective import).
    Slot(FrameSlot),
    /// An entry-frame slot: a module-scope binding.
    Global(GlobalSlot),
    /// A value the current closure captured.
    Capture(CaptureIdx),
    /// The current closure itself.
    SelfClosure,
}

/// Right-hand side of a `Let`, or the payload of a `Tail`. Every compound
/// operand is a `LocalId` — nested evaluation has already been linearised into
/// the enclosing `Let` spine.
#[derive(Debug, Clone)]
pub enum Atom {
    Local(LocalId),
    Const(ConstId),
    Load(Load),
    Nil,
    Bool(bool),
    Ctor {
        variant: VariantRef,
        fields: Vec<LocalId>,
        /// Set by `perceus`: the local whose dropped cell this constructor
        /// overwrites in place, when that cell turned out to be uniquely owned.
        /// `None` allocates fresh.
        reuse: Option<LocalId>,
    },
    /// A primitive operation on `args`. See [`PrimOp`] for each one's operands.
    PrimOp {
        op: PrimOp,
        args: Vec<LocalId>,
    },
    /// A call to a `@vm` built-in.
    Intrinsic {
        intrinsic: Intrinsic,
        args: Vec<LocalId>,
    },
    /// A closure over `captures`, running function `func_idx`. The body is
    /// its own entry in `Program::fns`, so this only refers to it.
    Closure {
        func_idx: FuncIdx,
        captures: Vec<LocalId>,
    },
    Call {
        callee: Callee,
        args: Vec<LocalId>,
    },
}

impl Atom {
    pub fn prim(op: PrimOp, args: Vec<LocalId>) -> Self {
        Atom::PrimOp { op, args }
    }

    /// The locals this atom reads, in push order. `Ctor`'s `reuse` is not one:
    /// it names a slot to overwrite, not a value pushed on the stack.
    fn operands(&self) -> impl Iterator<Item = LocalId> + '_ {
        let (pushed, trailing): (&[LocalId], Option<LocalId>) = match self {
            Atom::Local(x) => (&[], Some(*x)),
            Atom::Const(_) | Atom::Load(_) | Atom::Nil | Atom::Bool(_) => (&[], None),
            Atom::Ctor { fields, .. } => (fields, None),
            Atom::PrimOp { args, .. } | Atom::Intrinsic { args, .. } => (args, None),
            Atom::Closure { captures, .. } => (captures, None),
            Atom::Call { callee, args } => match callee {
                Callee::Local(id) => (args, Some(*id)),
                Callee::Known(_) | Callee::Self_ => (args, None),
            },
        };
        pushed.iter().copied().chain(trailing)
    }

    pub fn for_each_operand(&self, f: impl FnMut(LocalId)) {
        self.operands().for_each(f);
    }
}

/// Lowered match-arm pattern. `lower` flattens nesting into successive `Match`
/// nodes, so a `Ctor` arm binds its fields as fresh locals.
#[derive(Debug, Clone)]
pub enum CorePat {
    Wild,
    Bind(CoreBind),
    Lit(ConstId),
    Ctor {
        variant: VariantRef,
        fields: Vec<CoreBind>,
    },
}

impl CorePat {
    /// The locals this pattern introduces, in binding order.
    pub fn binds(&self) -> std::slice::Iter<'_, CoreBind> {
        match self {
            CorePat::Wild | CorePat::Lit(_) => <&[CoreBind]>::default().iter(),
            CorePat::Bind(b) => std::slice::from_ref(b).iter(),
            CorePat::Ctor { fields, .. } => fields.iter(),
        }
    }
}

/// ANF expression tree. The spine is a right-nested chain of `Let`s
/// terminating in `Tail`; `Match`/`If` are the only join points.
#[derive(Debug, Clone)]
pub enum CoreExpr {
    Let {
        bind: CoreBind,
        rhs: Atom,
        body: Box<CoreExpr>,
    },
    /// `let bind = <join>; body` where `join` is a control-flow tree in operand
    /// position. Every `Tail` inside `join` is a value, not a return. Kept
    /// distinct from `Let` so `rhs: Atom` stays operand-only, which Perceus's
    /// linear scan depends on.
    LetJoin {
        bind: CoreBind,
        join: Box<CoreExpr>,
        body: Box<CoreExpr>,
    },
    /// Declares the zero-arity continuation `cont`, in scope for every
    /// `Goto(id)` in `body`. Unlike `LetJoin` it runs only when a failure edge
    /// fires, may be entered from many edges, and sits in tail position. Zero
    /// arity works because `cont` reads only locals bound before this node, and
    /// this node dominates every `Goto` to `id`.
    LetCont {
        id: JoinId,
        cont: Box<CoreExpr>,
        body: Box<CoreExpr>,
    },
    /// Inserted by `perceus` at the last use of `local`. Releases the frame's
    /// reference and, when the cell is uniquely owned, parks the hollowed
    /// allocation for a paired `Ctor{reuse}`. `shape` is `Some` only when the
    /// allocation size is statically known; `None` drops for RC correctness but
    /// never pairs.
    Drop {
        local: LocalId,
        shape: Option<ReuseShape>,
        body: Box<CoreExpr>,
    },
    Match {
        scrut: LocalId,
        arms: Vec<(CorePat, CoreExpr)>,
        ty: RTy,
    },
    If {
        cond: LocalId,
        then: Box<CoreExpr>,
        els: Box<CoreExpr>,
        ty: RTy,
    },
    /// Tail position: return value or tail call.
    Tail(Atom),
    /// Transfer to the continuation an enclosing [`CoreExpr::LetCont`] declared
    /// for this `JoinId`. Legal only in tail position, so it carries no value.
    /// Many `Goto`s may share one label.
    Goto(JoinId),
}

impl CoreExpr {
    /// Calls `f` for each constructor this expression builds or matches on,
    /// once per mention, in no set order.
    pub fn for_each_variant(&self, mut f: impl FnMut(VariantRef)) {
        let mut work = vec![self];
        while let Some(e) = work.pop() {
            match e {
                CoreExpr::Let { rhs, body, .. } => {
                    if let Atom::Ctor { variant, .. } = rhs {
                        f(*variant);
                    }
                    work.push(body);
                }
                CoreExpr::Tail(Atom::Ctor { variant, .. }) => f(*variant),
                CoreExpr::Tail(_) | CoreExpr::Goto(_) => {}
                CoreExpr::LetJoin { join, body, .. }
                | CoreExpr::LetCont {
                    cont: join, body, ..
                } => {
                    work.push(join);
                    work.push(body);
                }
                CoreExpr::Drop { body, .. } => work.push(body),
                CoreExpr::Match { arms, .. } => {
                    for (pat, arm) in arms {
                        if let CorePat::Ctor { variant, .. } = pat {
                            f(*variant);
                        }
                        work.push(arm);
                    }
                }
                CoreExpr::If { then, els, .. } => {
                    work.push(then);
                    work.push(els);
                }
            }
        }
    }

    /// The `(bind, entry-frame slot)` pairs pinned on this expression's
    /// outermost `Let`/`LetJoin` spine, in binding order.
    ///
    /// Meaningful on a module toplevel, where module decls are spine bindings
    /// by construction. The walk steps over the `Drop`s Perceus interleaves and
    /// stops at the first join point. The pinning lives only on the binds, so
    /// nothing can disagree with it.
    pub fn toplevel_globals(&self) -> Vec<(LocalId, GlobalSlot)> {
        let mut out = Vec::new();
        let mut cur = self;
        loop {
            match cur {
                CoreExpr::Let { bind, body, .. } | CoreExpr::LetJoin { bind, body, .. } => {
                    if let Some(slot) = bind.global {
                        out.push((bind.id, slot));
                    }
                    cur = body;
                }
                CoreExpr::Drop { body, .. } | CoreExpr::LetCont { body, .. } => cur = body,
                CoreExpr::Match { .. }
                | CoreExpr::If { .. }
                | CoreExpr::Tail(_)
                | CoreExpr::Goto(_) => return out,
            }
        }
    }
}

/// One lowered function. Its name travels beside it, on [`LoweredFn`].
#[derive(Debug, Clone)]
pub struct CoreFn {
    pub params: Vec<CoreBind>,
    pub body: CoreExpr,
    pub ret_ty: RTy,
}

/// One body ready for a backend: its core IR after Perceus, and the pool its
/// `RTy`s index. The pool is shared, because one elaboration lowers a body
/// together with the eta wrappers it minted.
#[derive(Debug, Clone)]
pub struct LoweredFn {
    /// The key of the module whose source this body was lowered from, as
    /// text: `main` for the entry file, `scarlet/array` for a stdlib module,
    /// a canonical path for any other file. For people to read, and for
    /// picking out one module's functions.
    pub module: String,
    /// The source name, for anything a person reads: a crash report, a stack
    /// trace, a profile. Unique only within `module`: `scarlet/array` and
    /// `scarlet/option` both have a `map`.
    pub name: String,
    pub core: CoreFn,
    /// What every `RTy` in `core` indexes. Nothing reads a body's types until
    /// a backend does, but dropping the pool would leave those indices
    /// pointing into an arena that no longer exists.
    #[expect(
        dead_code,
        reason = "the RTys in `core` index it; the backend that reads them is not written yet"
    )]
    pool: Rc<ResolvedPool>,
}

impl LoweredFn {
    pub fn new(module: String, name: String, core: CoreFn, pool: Rc<ResolvedPool>) -> Self {
        LoweredFn {
            module,
            name,
            core,
            pool,
        }
    }
}

/// A whole compiled program in core IR: what a backend runs.
///
/// Running it means running every one of [`Self::inits`] in order, then
/// [`Self::toplevel`], then calling [`Self::main`] when there is one. Every
/// module-scope binding lives in one shared frame of [`Self::globals`] slots,
/// which is what a `GlobalSlot` indexes.
#[derive(Debug, Clone)]
pub struct Program {
    /// Every function, indexed by [`FuncIdx`]: declared ones, lambdas and the
    /// eta wrappers elaboration minted.
    pub fns: TiVec<FuncIdx, LoweredFn>,
    pub consts: Vec<Const>,
    /// Each imported module's top level, a module after everything it imports.
    pub inits: Vec<LoweredFn>,
    /// The entry file's own top level.
    pub toplevel: LoweredFn,
    /// `pub fn main`, when the program has one to start at.
    pub main: Option<FuncIdx>,
    /// How many global slots module-scope bindings occupy. Every `GlobalSlot`
    /// is below this.
    pub globals: u32,
    /// The names of every type the program builds or matches a constructor
    /// of, or that the VM builds one of ([`Self::abi`]). Every [`VariantRef`]
    /// in the program has its type here.
    pub types: BTreeMap<TypeId, TypeNames>,
    pub abi: Abi,
}

// Printer for the golden tests in `crates/scarlet/tests/core_ir.rs`. Ids print as
// `%n` and constants as `cN` so snapshots survive string-interner churn. An
// `RTy` prints as `:N`, its raw pool index, which shifts whenever the
// elaborator allocates a different number of nodes before it — the harness
// renumbers those per snapshot (`normalize_tys`). The `:` sigil belongs to this
// printer, not to `Display for RTy`.

impl fmt::Display for CoreBind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.id, self.ty)?;
        match self.global {
            Some(GlobalSlot(slot)) => write!(f, "@g{slot}"),
            None => Ok(()),
        }
    }
}

impl fmt::Display for Callee {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Callee::Known(i) => write!(f, "{i}"),
            Callee::Self_ => f.write_str("self"),
            Callee::Local(l) => write!(f, "{l}"),
        }
    }
}

fn write_locals(f: &mut fmt::Formatter<'_>, xs: &[LocalId]) -> fmt::Result {
    for (i, x) in xs.iter().enumerate() {
        if i > 0 {
            f.write_str(", ")?;
        }
        write!(f, "{x}")?;
    }
    Ok(())
}

impl fmt::Display for Atom {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Atom::Local(l) => write!(f, "{l}"),
            Atom::Const(c) => write!(f, "{c}"),
            Atom::Load(Load::Slot(s)) => write!(f, "slot{}", s.0),
            Atom::Load(Load::Global(g)) => write!(f, "global{}", g.0),
            Atom::Load(Load::Capture(c)) => write!(f, "capture{}", c.0),
            Atom::Load(Load::SelfClosure) => f.write_str("self"),
            Atom::Nil => f.write_str("nil"),
            Atom::Bool(b) => write!(f, "{b}"),
            Atom::Ctor {
                variant,
                fields,
                reuse,
            } => {
                write!(f, "ctor {}.{}(", variant.type_id.0, variant.variant_idx)?;
                write_locals(f, fields)?;
                f.write_str(")")?;
                if let Some(r) = reuse {
                    write!(f, " reuse {r}")?;
                }
                Ok(())
            }
            Atom::PrimOp { op, args } => {
                write!(f, "{op:?}(")?;
                write_locals(f, args)?;
                f.write_str(")")
            }
            Atom::Intrinsic { intrinsic, args } => {
                write!(f, "{intrinsic:?}(")?;
                write_locals(f, args)?;
                f.write_str(")")
            }
            Atom::Closure { func_idx, captures } => {
                write!(f, "closure {func_idx}(")?;
                write_locals(f, captures)?;
                f.write_str(")")
            }
            Atom::Call { callee, args } => {
                write!(f, "call {callee}(")?;
                write_locals(f, args)?;
                f.write_str(")")
            }
        }
    }
}

impl fmt::Display for CorePat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CorePat::Wild => f.write_str("_"),
            CorePat::Bind(b) => write!(f, "{}", b.id),
            CorePat::Lit(c) => write!(f, "{c}"),
            CorePat::Ctor { variant, fields } => {
                write!(f, "{}.{}(", variant.type_id.0, variant.variant_idx)?;
                for (i, b) in fields.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{}", b.id)?;
                }
                f.write_str(")")
            }
        }
    }
}

/// `CoreExpr` printed `depth` levels in.
pub struct Indented<'a>(pub &'a CoreExpr, pub usize);

impl fmt::Display for Indented<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Indented(e, depth) = *self;
        let pad = |f: &mut fmt::Formatter<'_>, d: usize| {
            for _ in 0..d {
                f.write_str("  ")?;
            }
            Ok(())
        };
        // Iterative: a lowered body is one long spine, and recursing it would
        // blow the Rust stack.
        let mut cur = e;
        loop {
            pad(f, depth)?;
            match cur {
                CoreExpr::Let { bind, rhs, body } => {
                    writeln!(f, "let {bind} = {rhs}")?;
                    cur = body;
                }
                CoreExpr::LetJoin { bind, join, body } => {
                    writeln!(f, "letj {bind} =")?;
                    Indented(join, depth + 1).fmt(f)?;
                    cur = body;
                }
                CoreExpr::LetCont { id, cont, body } => {
                    writeln!(f, "letc {id} =")?;
                    Indented(cont, depth + 1).fmt(f)?;
                    cur = body;
                }
                CoreExpr::Drop { local, shape, body } => {
                    match shape {
                        Some(s) => writeln!(f, "drop {local} [ctor:{}]", s.fields)?,
                        None => writeln!(f, "drop {local}")?,
                    }
                    cur = body;
                }
                CoreExpr::Tail(a) => {
                    return writeln!(f, "ret {a}");
                }
                CoreExpr::Goto(id) => {
                    return writeln!(f, "goto {id}");
                }
                CoreExpr::If {
                    cond,
                    then,
                    els,
                    ty,
                } => {
                    writeln!(f, "if {cond} :{ty}")?;
                    Indented(then, depth + 1).fmt(f)?;
                    pad(f, depth)?;
                    writeln!(f, "else")?;
                    return Indented(els, depth + 1).fmt(f);
                }
                CoreExpr::Match { scrut, arms, ty } => {
                    writeln!(f, "match {scrut} :{ty}")?;
                    for (pat, body) in arms {
                        pad(f, depth + 1)?;
                        writeln!(f, "| {pat} ->")?;
                        Indented(body, depth + 2).fmt(f)?;
                    }
                    return Ok(());
                }
            }
        }
    }
}

impl fmt::Display for CoreExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Indented(self, 0).fmt(f)
    }
}

/// A function under `head`, then its parameters, return type and body.
fn write_fn(f: &mut fmt::Formatter<'_>, head: fmt::Arguments<'_>, core: &CoreFn) -> fmt::Result {
    write!(f, "{head}(")?;
    for (i, p) in core.params.iter().enumerate() {
        if i > 0 {
            f.write_str(", ")?;
        }
        write!(f, "{p}")?;
    }
    writeln!(f, ") -> :{}", core.ret_ty)?;
    write!(f, "{}", Indented(&core.body, 1))
}

impl fmt::Display for CoreFn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_fn(f, format_args!("fn"), self)
    }
}

/// Headed with the module and source name rather than the interned id, which
/// is what a person reading a listing can use: `fn scarlet/array.map(...)`.
impl fmt::Display for LoweredFn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_fn(
            f,
            format_args!("fn {}.{}", self.module, self.name),
            &self.core,
        )
    }
}

impl fmt::Display for Program {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for func in &self.fns {
            writeln!(f, "{}", func.core)?;
        }
        for init in &self.inits {
            writeln!(f, "init:")?;
            Indented(&init.core.body, 1).fmt(f)?;
        }
        writeln!(f, "toplevel:")?;
        Indented(&self.toplevel.core.body, 1).fmt(f)?;
        if let Some(main) = self.main {
            writeln!(f, "main: {main}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bind(id: u32, ty: RTy) -> CoreBind {
        CoreBind::new(LocalId(id), ty)
    }

    fn vref(tid: i32, idx: u16) -> VariantRef {
        VariantRef {
            type_id: TypeId(tid),
            variant_idx: idx,
        }
    }

    #[test]
    fn atom_forms() {
        assert_eq!(Atom::Local(LocalId(3)).to_string(), "%3");
        assert_eq!(Atom::Const(ConstId(7)).to_string(), "c7");
        assert_eq!(
            Atom::prim(PrimOp::IntAdd, vec![LocalId(0), LocalId(1)]).to_string(),
            "IntAdd(%0, %1)"
        );
        assert_eq!(
            Atom::PrimOp {
                op: PrimOp::TupleField(2),
                args: vec![LocalId(0)],
            }
            .to_string(),
            "TupleField(2)(%0)"
        );
        assert_eq!(
            Atom::Closure {
                func_idx: FuncIdx(5),
                captures: vec![LocalId(1)],
            }
            .to_string(),
            "closure fn#5(%1)"
        );
        assert_eq!(
            Atom::Ctor {
                variant: vref(7, 1),
                fields: vec![LocalId(2)],
                reuse: None,
            }
            .to_string(),
            "ctor 7.1(%2)"
        );
        assert_eq!(
            Atom::Ctor {
                variant: vref(7, 1),
                fields: vec![LocalId(2), LocalId(3)],
                reuse: Some(LocalId(9)),
            }
            .to_string(),
            "ctor 7.1(%2, %3) reuse %9"
        );
        assert_eq!(
            Atom::Call {
                callee: Callee::Known(FuncIdx(9)),
                args: vec![LocalId(0)],
            }
            .to_string(),
            "call fn#9(%0)"
        );
        assert_eq!(
            Atom::Call {
                callee: Callee::Self_,
                args: vec![],
            }
            .to_string(),
            "call self()"
        );
        assert_eq!(
            Atom::Call {
                callee: Callee::Local(LocalId(4)),
                args: vec![LocalId(5), LocalId(6)],
            }
            .to_string(),
            "call %4(%5, %6)"
        );
    }

    #[test]
    fn pat_forms() {
        assert_eq!(CorePat::Wild.to_string(), "_");
        assert_eq!(CorePat::Lit(ConstId(2)).to_string(), "c2");
        assert_eq!(CorePat::Bind(bind(3, RTy(0))).to_string(), "%3");
        assert_eq!(
            CorePat::Ctor {
                variant: vref(4, 0),
                fields: vec![bind(5, RTy(2)), bind(6, RTy(2))],
            }
            .to_string(),
            "4.0(%5, %6)"
        );
    }

    #[test]
    fn let_drop_spine_is_flat() {
        let e = CoreExpr::Let {
            bind: bind(2, RTy(10)),
            rhs: Atom::prim(PrimOp::IntAdd, vec![LocalId(0), LocalId(1)]),
            body: Box::new(CoreExpr::Drop {
                local: LocalId(0),
                shape: Some(ReuseShape::ctor(3)),
                body: Box::new(CoreExpr::Let {
                    bind: bind(3, RTy(11)),
                    rhs: Atom::Ctor {
                        variant: vref(7, 1),
                        fields: vec![LocalId(2)],
                        reuse: Some(LocalId(0)),
                    },
                    body: Box::new(CoreExpr::Tail(Atom::Local(LocalId(3)))),
                }),
            }),
        };
        assert_eq!(
            e.to_string(),
            "\
let %2:10 = IntAdd(%0, %1)
drop %0 [ctor:3]
let %3:11 = ctor 7.1(%2) reuse %0
ret %3
"
        );
    }

    #[test]
    fn if_and_match_indent() {
        let m = CoreExpr::Match {
            scrut: LocalId(0),
            arms: vec![
                (
                    CorePat::Ctor {
                        variant: vref(4, 0),
                        fields: vec![bind(5, RTy(2))],
                    },
                    CoreExpr::Tail(Atom::Call {
                        callee: Callee::Self_,
                        args: vec![LocalId(5)],
                    }),
                ),
                (CorePat::Wild, CoreExpr::Tail(Atom::Const(ConstId(0)))),
            ],
            ty: RTy(9),
        };
        let e = CoreExpr::If {
            cond: LocalId(1),
            then: Box::new(m),
            els: Box::new(CoreExpr::Tail(Atom::Local(LocalId(0)))),
            ty: RTy(9),
        };
        assert_eq!(
            e.to_string(),
            "\
if %1 :9
  match %0 :9
    | 4.0(%5) ->
      ret call self(%5)
    | _ ->
      ret c0
else
  ret %0
"
        );
    }

    #[test]
    fn core_fn_header_and_body() {
        let f = CoreFn {
            params: vec![bind(0, RTy(1)), bind(1, RTy(1))],
            body: CoreExpr::Tail(Atom::prim(PrimOp::IntLt, vec![LocalId(0), LocalId(1)])),
            ret_ty: RTy(3),
        };
        assert_eq!(
            f.to_string(),
            "\
fn(%0:1, %1:1) -> :3
  ret IntLt(%0, %1)
"
        );
    }
}
