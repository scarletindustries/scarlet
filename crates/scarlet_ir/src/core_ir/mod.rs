//! Core IR is what the compiler hands a backend. It's typed A-normal form
//! (ANF), which means every subexpression is bound to a `LocalId` by a `Let`.
//! That way last use and drops fall out of one backward scan (Perceus). The
//! passes over it live in the compiler, and `docs/vm-design.md` says what
//! running it means.

mod prim;

pub use prim::PrimOp;

use std::collections::BTreeMap;
use std::fmt;

use crate::TypeId;
use crate::intrinsic::Intrinsic;
use crate::newtype_index;
use crate::rty::{RTy, ResolvedPool};
use crate::tivec::TiVec;

// The core IR's index spaces. Each is a `crate::tivec::Idx`, so the `TiVec` it
// indexes rejects the others at compile time. `GlobalSlot`, a slot in the entry
// frame, isn't one of them: the compiler in `scarlet_core` hands those out.

newtype_index!(
    /// A dense local index, scoped to one function. `lower` hands these out in
    /// evaluation order. A later pass may move a `Let`, so the order of the
    /// spine is what says when a local is bound, and not its number.
    pub struct LocalId("%")
);

newtype_index!(
    /// An index into [`Program::consts`].
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
    /// A labelled continuation, scoped to one lowered body.
    /// [`CoreExpr::LetCont`] declares it and [`CoreExpr::Goto`] jumps to it. It
    /// names code, never a value.
    pub struct JoinId("j")
);

newtype_index!(
    /// An index into [`Program::fns`], numbered the same as `TypedProgram::fns`.
    pub struct FuncIdx("fn#")
);

/// The most parameters a function takes. The VM names a frame's registers by
/// a byte, and the arguments arrive in the first of them.
pub const MAX_PARAMS: usize = 250;

/// A typed local binding. Its [`RTy`] indexes the elaborator's `ResolvedPool`,
/// which can't hold an unsolved inference variable. So Perceus can never get a
/// type that answers `is_heap` with `false` only because inference lost it.
#[derive(Debug, Clone)]
pub struct CoreBind {
    pub id: LocalId,
    pub ty: RTy,
    /// `Some(slot)` when this bind is a module's top-level declaration that
    /// must land in that global slot. Function bodies read it with
    /// `Load::Global(slot)`, so the top level has to write the same slot.
    ///
    /// It lives on the bind, not in a side table keyed by `LocalId`, because
    /// that table would fall out of step as soon as a pass renumbers or drops
    /// a bind.
    pub global: Option<GlobalSlot>,
}

impl CoreBind {
    /// A fresh bind, pinned to no slot. `lower` sets [`Self::global`] later.
    pub fn new(id: LocalId, ty: RTy) -> Self {
        CoreBind {
            id,
            ty,
            global: None,
        }
    }
}

/// A constructor's resolved identity, captured during lowering so a backend
/// doesn't need the type environment to dispatch on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VariantRef {
    pub type_id: TypeId,
    pub variant_idx: u16,
}

/// How a set of constructors names each one. A constructor with no fields is
/// a value by itself, and one with `N` fields has a cell to make, so the two
/// need not be named by the same thing.
pub trait Naming: Copy + fmt::Debug {
    type Plain: Copy + fmt::Debug;
    type Fields<const N: usize>: Copy + fmt::Debug;
}

/// The names the compiler gives.
#[derive(Debug, Clone, Copy)]
pub struct ByVariant;

impl Naming for ByVariant {
    type Plain = VariantRef;
    type Fields<const N: usize> = VariantRef;
}

/// Finds what a backend calls each constructor, or says why it has none.
pub trait Rename {
    type To: Naming;
    type Error;

    /// A constructor with no fields.
    fn plain(&mut self, v: VariantRef) -> Result<<Self::To as Naming>::Plain, Self::Error>;

    /// A constructor with `N` fields.
    fn fields<const N: usize>(
        &mut self,
        v: VariantRef,
    ) -> Result<<Self::To as Naming>::Fields<N>, Self::Error>;
}

/// Declares a set of constructors the VM knows, each beside how many fields
/// it has, and after `with` the smaller set it holds, if it holds one.
macro_rules! constructors {
    (@rename $r:ident Plain $v:expr) => { $r.plain($v)? };
    (@rename $r:ident Fields $n:literal $v:expr) => { $r.fields::<$n>($v)? };
    ($($(#[$doc:meta])* pub struct $name:ident {
        $($(#[$field_doc:meta])* $field:ident: $form:ident $(<$n:literal>)?),* $(,)?
    } $(with $(#[$set_doc:meta])* $set:ident: $of:ident)?)*) => {$(
        $(#[$doc])*
        #[derive(Debug, Clone, Copy)]
        pub struct $name<N: Naming = ByVariant> {
            $($(#[$field_doc])* pub $field: N::$form $(<$n>)?,)*
            $($(#[$set_doc])* pub $set: $of<N>,)?
        }

        impl $name {
            /// The same constructors, named as `r` names them.
            fn rename<R: Rename>(self, r: &mut R) -> Result<$name<R::To>, R::Error> {
                Ok($name {
                    $($field: constructors!(@rename r $form $($n)? self.$field),)*
                    $($set: self.$set.rename(r)?,)?
                })
            }
        }
    )*};
}

/// The stdlib constructors the VM builds on its own, like the `Some(x)` that
/// `xs[i]` gives. The compiler says which constructor each one is; the VM
/// knows nothing else about the stdlib's types.
#[derive(Debug, Clone, Copy)]
pub struct Abi<N: Naming = ByVariant> {
    /// `Some(value)`.
    pub some: N::Fields<1>,
    /// `None`.
    pub none: N::Plain,
    /// `Ok(value)`.
    pub ok: N::Fields<1>,
    /// `Err(error)`.
    pub err: N::Fields<1>,
    /// `scarlet/binary.Radix`, when the program loads that module: the base
    /// `binary.parse_int` and `binary.from_int_ascii` are asked for.
    pub radix: Option<Radix<N>>,
    /// `scarlet/io.IoError`, when the program loads that module: what a file
    /// read or write that failed says went wrong.
    pub io: Option<IoErrors<N>>,
    /// `scarlet/json`'s types, when the program loads that module: what
    /// `json.parse` gives and `json.encode` reads.
    pub json: Option<JsonTypes<N>>,
    /// `scarlet/http/h1`'s types, when the program loads that module.
    pub http: Option<HttpTypes<N>>,
    /// `scarlet/net/address`'s types, when the program loads that module.
    pub address: Option<AddressTypes<N>>,
    /// `scarlet/net/error` and `scarlet/net/socket`'s types, when the program
    /// loads both.
    pub net: Option<NetTypes<N>>,
    /// `scarlet/net/tls`'s types, when the program loads that module.
    pub tls: Option<TlsTypes<N>>,
}

constructors! {
    /// The constructors of `scarlet/net/address`.
    pub struct AddressTypes {
        /// `IpAddress`'s two forms, each holding the address as text.
        v4: Fields<1>,
        v6: Fields<1>,
        /// `SocketAddress(ip, port)`.
        socket_address: Fields<2>,
    }

    /// The constructors of `scarlet/net/error.NetError` and
    /// `scarlet/net/socket` that the VM builds.
    pub struct NetTypes {
        timed_out: Plain,
        connection_refused: Plain,
        connection_reset: Plain,
        connection_aborted: Plain,
        not_connected: Plain,
        broken_pipe: Plain,
        addr_in_use: Plain,
        addr_not_available: Plain,
        network_down: Plain,
        network_unreachable: Plain,
        host_unreachable: Plain,
        permission_denied: Plain,
        unexpected_eof: Plain,
        unaligned_binary: Plain,
        invalid_port: Plain,
        /// Any other OS error: this holds its number.
        errno: Fields<1>,
        /// `socket.Socket(conn, peer)`.
        socket: Fields<2>,
        /// `socket.Read`: `Data(bytes)` and `Closed`.
        data: Fields<1>,
        closed: Plain,
    }

    /// The constructors of `scarlet/net/tls` that the VM builds.
    pub struct TlsTypes {
        /// `TlsSocket(conn, peer, server_name)`.
        tls_socket: Fields<3>,
        certificate_unknown_issuer: Plain,
        certificate_expired: Plain,
        certificate_not_yet_valid: Plain,
        certificate_revoked: Plain,
        hostname_mismatch: Plain,
        bad_certificate: Plain,
        protocol_error: Plain,
        handshake_failed: Plain,
        invalid_server_name: Plain,
        /// `Transport(cause)`, holding a `NetError`.
        transport: Fields<1>,
    }

    /// The constructors of `scarlet/binary.Radix`.
    pub struct Radix {
        dec: Plain,
        hex: Plain,
    }

    /// The constructors of `scarlet/io.IoError` the VM builds, each named for the
    /// OS error it stands for.
    pub struct IoErrors {
        /// These hold the path the operation was given.
        not_found: Fields<1>,
        permission_denied: Fields<1>,
        already_exists: Fields<1>,
        not_a_directory: Fields<1>,
        is_a_directory: Fields<1>,
        read_only_filesystem: Fields<1>,
        filesystem_loop: Fields<1>,
        file_too_large: Fields<1>,
        /// These hold nothing.
        storage_full: Plain,
        quota_exceeded: Plain,
        unaligned_binary: Plain,
        /// Any other OS error: this holds its number.
        errno: Fields<1>,
    }

    /// The constructors of `scarlet/http/h1` and `scarlet/http/headers` the VM
    /// builds or reads: what a parsed head, a body's framing and a decoded chunked
    /// body are.
    pub struct HttpTypes {
        /// `headers.Header(name, value)`.
        header: Fields<2>,
        /// `h1.Version`.
        http10: Plain,
        http11: Plain,
        /// `h1.HeadFlags(conn, expect_100_continue)`, and `h1.ConnTokens`.
        head_flags: Fields<2>,
        conn_neither: Plain,
        conn_close: Plain,
        conn_keep_alive: Plain,
        conn_both: Plain,
        /// `h1.Parsed`: a request head.
        parsed_done: Fields<6>,
        parsed_need_more: Plain,
        parsed_bad: Fields<1>,
        /// `h1.ParsedResponse`, and `h1.BadResponse`'s reasons.
        response_done: Fields<6>,
        response_need_more: Plain,
        response_bad: Fields<1>,
        bad_status_line: Plain,
        bad_version: Plain,
        bad_field: Plain,
        head_too_large: Plain,
        bad_framing: Plain,
        /// `h1.Framing`.
        no_body: Plain,
        length: Fields<1>,
        chunked: Plain,
        framing_invalid: Fields<1>,
        /// `h1.ChunkBody`.
        chunked_done: Fields<3>,
        chunked_need_more: Plain,
        chunked_bad: Fields<1>,
    }

    /// The constructors of `scarlet/json` the VM builds or reads.
    pub struct JsonTypes {
        /// `Doc(arena, tape, idx)`: a parsed document, and every cursor into it.
        doc: Fields<3>,
        /// `ParseError(offset, reason)`.
        parse_error: Fields<2>,
        /// The constructors of `Json`, which `json.encode` writes out.
        null: Plain,
        boolean: Fields<1>,
        integer: Fields<1>,
        real: Fields<1>,
        str: Fields<1>,
        list: Fields<1>,
        object: Fields<1>,
        number: Fields<1>,
    } with
    /// The constructors of `ParseReason`, which a `ParseError` carries as its `reason`.
    reasons: ParseReasons

    /// The constructors of `scarlet/json.ParseReason`, which says why a document didn't parse.
    pub struct ParseReasons {
        unexpected_end: Plain,
        unexpected_byte: Plain,
        trailing_data: Plain,
        unterminated_string: Plain,
        bad_escape: Plain,
        bad_unicode: Plain,
        bad_number: Plain,
        not_utf8: Plain,
        not_whole_bytes: Plain,
        too_large: Plain,
    }
}

impl Abi {
    /// The same constructors, named as `r` names them.
    pub fn rename<R: Rename>(self, r: &mut R) -> Result<Abi<R::To>, R::Error> {
        Ok(Abi {
            some: r.fields::<1>(self.some)?,
            none: r.plain(self.none)?,
            ok: r.fields::<1>(self.ok)?,
            err: r.fields::<1>(self.err)?,
            radix: self.radix.map(|t| t.rename(r)).transpose()?,
            io: self.io.map(|t| t.rename(r)).transpose()?,
            json: self.json.map(|t| t.rename(r)).transpose()?,
            http: self.http.map(|t| t.rename(r)).transpose()?,
            address: self.address.map(|t| t.rename(r)).transpose()?,
            net: self.net.map(|t| t.rename(r)).transpose()?,
            tls: self.tls.map(|t| t.rename(r)).transpose()?,
        })
    }

    /// Every constructor here, so their types' names can travel with them.
    pub fn variants(self) -> Vec<VariantRef> {
        let mut all = Listed(Vec::new());
        match self.rename(&mut all) {
            Ok(_) => all.0,
        }
    }
}

/// Lists the constructors it is asked to name, and names none.
struct Listed(Vec<VariantRef>);

#[derive(Debug, Clone, Copy)]
struct Unnamed;

impl Naming for Unnamed {
    type Plain = ();
    type Fields<const N: usize> = ();
}

impl Rename for Listed {
    type To = Unnamed;
    type Error = std::convert::Infallible;

    fn plain(&mut self, v: VariantRef) -> Result<(), Self::Error> {
        self.0.push(v);
        Ok(())
    }

    fn fields<const N: usize>(&mut self, v: VariantRef) -> Result<(), Self::Error> {
        self.0.push(v);
        Ok(())
    }
}

/// What a type and its constructors are called, for anything that shows a
/// value of it to a person: `Some(1)`, `Point{ x: 1, y: 2 }`. The VM also reads
/// here how many fields each constructor has.
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
    /// The label of every field, in the order the constructor holds them.
    pub fields: Vec<String>,
}

/// What a call calls, once resolved: a function known at compile time, the
/// current function itself, or a closure held in a local.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Callee {
    Known(FuncIdx),
    Self_,
    Local(LocalId),
}

/// A slot in the entry (module) frame, where a module-scope binding lives. A
/// module-scope name can be bound more than once, such as an import shadowed
/// by a later `let`, so each binding carries the slot it lands in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GlobalSlot(pub i32);

/// An index into the current closure's capture array.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CaptureIdx(pub i32);

/// A value read from somewhere other than a core-IR local.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Load {
    /// An entry-frame slot: a module-scope binding.
    Global(GlobalSlot),
    /// A value the current closure captured.
    Capture(CaptureIdx),
    /// The current closure itself.
    SelfClosure,
}

/// The right-hand side of a `Let`, or the payload of a `Tail`. Every compound
/// operand is a `LocalId`, because nested evaluation has already been
/// flattened into the enclosing `Let` spine.
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
    },
    /// `base`, which is a `variant`, with the fields in `sets` in place of its
    /// own. A backend may write them where `base` is when nothing else holds
    /// it, and `base` is held until this runs. Each field is there once, by its
    /// index, in order, and there is at least one.
    Update {
        base: LocalId,
        variant: VariantRef,
        sets: Vec<(u16, LocalId)>,
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
    /// A closure over `captures` that runs function `func_idx`. The body is
    /// its own entry in `Program::fns`, so this only points at it.
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

    /// The constructor this atom builds, when it builds one.
    fn builds(&self) -> Option<VariantRef> {
        match self {
            Atom::Ctor { variant, .. } | Atom::Update { variant, .. } => Some(*variant),
            Atom::Local(_)
            | Atom::Const(_)
            | Atom::Load(_)
            | Atom::Nil
            | Atom::Bool(_)
            | Atom::PrimOp { .. }
            | Atom::Intrinsic { .. }
            | Atom::Closure { .. }
            | Atom::Call { .. } => None,
        }
    }

    /// Call `f` with each local this atom reads, in push order.
    pub fn for_each_operand(&self, mut f: impl FnMut(LocalId)) {
        match self {
            Atom::Local(x) => f(*x),
            Atom::Const(_) | Atom::Load(_) | Atom::Nil | Atom::Bool(_) => {}
            Atom::Ctor { fields: all, .. }
            | Atom::PrimOp { args: all, .. }
            | Atom::Intrinsic { args: all, .. }
            | Atom::Closure { captures: all, .. } => all.iter().for_each(|x| f(*x)),
            Atom::Update { base, sets, .. } => {
                f(*base);
                sets.iter().for_each(|(_, x)| f(*x));
            }
            Atom::Call { callee, args } => {
                args.iter().for_each(|x| f(*x));
                match callee {
                    Callee::Local(x) => f(*x),
                    Callee::Known(_) | Callee::Self_ => {}
                }
            }
        }
    }
}

/// A lowered match-arm pattern. `lower` flattens nested patterns into a chain
/// of `Match` nodes, so a `Ctor` arm binds its fields as fresh locals.
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

/// An ANF expression tree. The spine is a right-nested chain of `Let`s that
/// ends in a `Tail`. `Match` and `If` are the only places control flow splits
/// and joins.
#[derive(Debug, Clone)]
pub enum CoreExpr {
    Let {
        bind: CoreBind,
        rhs: Atom,
        body: Box<CoreExpr>,
    },
    /// `let bind = <join>; body`, where `join` is a control-flow tree in
    /// operand position. Every `Tail` inside `join` is a value, not a return.
    /// It's separate from `Let` so `rhs: Atom` only ever holds operands, which
    /// Perceus's linear scan depends on.
    LetJoin {
        bind: CoreBind,
        join: Box<CoreExpr>,
        body: Box<CoreExpr>,
    },
    /// Declares the continuation `cont`, which takes no arguments and is in
    /// scope for every `Goto(id)` in `body`. Unlike `LetJoin`, it runs only when
    /// a failure edge fires, many edges can enter it, and it sits in tail
    /// position. It needs no arguments because `cont` only reads locals bound
    /// before this node, and this node dominates every `Goto` to `id`: every
    /// path to one of them passes through here.
    LetCont {
        id: JoinId,
        cont: Box<CoreExpr>,
        body: Box<CoreExpr>,
    },
    /// `perceus` inserts this at the last use of `local`. It releases the
    /// frame's reference.
    Drop { local: LocalId, body: Box<CoreExpr> },
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
    /// Tail position: a return value or a tail call.
    Tail(Atom),
    /// Jump to the continuation an enclosing [`CoreExpr::LetCont`] declared for
    /// this `JoinId`. It's only allowed in tail position, so it carries no
    /// value. Many `Goto`s can share one label.
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
                    rhs.builds().into_iter().for_each(&mut f);
                    work.push(body);
                }
                CoreExpr::Tail(atom) => atom.builds().into_iter().for_each(&mut f),
                CoreExpr::Goto(_) => {}
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
    /// and a canonical path for any other file. It's for people to read, and
    /// for picking out one module's functions.
    pub module: String,
    /// The source name, for anything a person reads, like a crash report, a
    /// stack trace or a profile. It's only unique within `module`, because
    /// `scarlet/array` and `scarlet/option` both have a `map`.
    pub name: String,
    pub core: CoreFn,
    /// Whether the body holds a value that may be a handle, or hold one, and
    /// that no `Drop` gives up. It is then given up when a backend next uses the
    /// place it keeps it in, so a backend that shares places says when that is.
    pub undropped: bool,
}

impl LoweredFn {
    pub fn new(module: String, name: String, core: CoreFn, undropped: bool) -> Self {
        LoweredFn {
            module,
            name,
            core,
            undropped,
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
    /// Each imported module's top level. A module comes after everything it
    /// imports.
    pub inits: Vec<LoweredFn>,
    /// The entry file's own top level.
    pub toplevel: LoweredFn,
    /// `pub fn main`, when the program has one to start at.
    pub main: Option<FuncIdx>,
    /// How many global slots module-scope bindings occupy. Every `GlobalSlot`
    /// is below this.
    pub globals: u32,
    /// Every type the program builds or matches a constructor of, or that the
    /// VM builds one of ([`Self::abi`]). Every [`VariantRef`] in the program
    /// has its type here, with every variant of it. A backend makes a
    /// constructor's cell by what this says of its fields, so it refuses a
    /// [`VariantRef`] that is not here, and a constructor or a pattern with
    /// another number of fields.
    pub types: BTreeMap<TypeId, TypeNames>,
    pub abi: Abi,
    /// What every `RTy` in every body indexes.
    pub pool: ResolvedPool,
}

// The printer for the golden tests in `crates/scarlet/tests/core_ir.rs`. Ids
// print as `%n` and constants as `cN`, so snapshots survive changes to string
// interning. An `RTy` prints as `:N`, its raw pool index. That index shifts
// whenever the elaborator makes a different number of nodes before it, so the
// harness renumbers them in each snapshot (`renumber`). The `:` sigil belongs to
// this printer, not to `Display for RTy`.

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
            Atom::Load(Load::Global(g)) => write!(f, "global{}", g.0),
            Atom::Load(Load::Capture(c)) => write!(f, "capture{}", c.0),
            Atom::Load(Load::SelfClosure) => f.write_str("self"),
            Atom::Nil => f.write_str("nil"),
            Atom::Bool(b) => write!(f, "{b}"),
            Atom::Ctor { variant, fields } => {
                write!(f, "ctor {}.{}(", variant.type_id.0, variant.variant_idx)?;
                write_locals(f, fields)?;
                f.write_str(")")
            }
            Atom::Update {
                base,
                variant,
                sets,
            } => {
                let VariantRef {
                    type_id,
                    variant_idx,
                } = variant;
                write!(f, "update {}.{}(..{base}", type_id.0, variant_idx)?;
                for (field, local) in sets {
                    write!(f, ", {field}: {local}")?;
                }
                f.write_str(")")
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

/// A `CoreExpr` printed `depth` levels in.
pub struct Indented<'a>(&'a CoreExpr, usize);

impl fmt::Display for Indented<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Indented(e, depth) = *self;
        let pad = |f: &mut fmt::Formatter<'_>, d: usize| {
            for _ in 0..d {
                f.write_str("  ")?;
            }
            Ok(())
        };
        // Loop instead of recursing, because a lowered body is one long spine
        // and recursing down it would overflow the Rust stack.
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
                    Indented(join, depth.saturating_add(1)).fmt(f)?;
                    cur = body;
                }
                CoreExpr::LetCont { id, cont, body } => {
                    writeln!(f, "letc {id} =")?;
                    Indented(cont, depth.saturating_add(1)).fmt(f)?;
                    cur = body;
                }
                CoreExpr::Drop { local, body } => {
                    writeln!(f, "drop {local}")?;
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
                    Indented(then, depth.saturating_add(1)).fmt(f)?;
                    pad(f, depth)?;
                    writeln!(f, "else")?;
                    return Indented(els, depth.saturating_add(1)).fmt(f);
                }
                CoreExpr::Match { scrut, arms, ty } => {
                    writeln!(f, "match {scrut} :{ty}")?;
                    for (pat, body) in arms {
                        pad(f, depth.saturating_add(1))?;
                        writeln!(f, "| {pat} ->")?;
                        Indented(body, depth.saturating_add(2)).fmt(f)?;
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

/// The heading uses the module and source name instead of the interned id,
/// because that's what a person reading a listing can use:
/// `fn scarlet/array.map(...)`.
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
mod tests;
