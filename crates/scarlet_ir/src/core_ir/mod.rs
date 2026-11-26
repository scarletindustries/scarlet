//! Core IR is what the compiler hands a backend. It's typed A-normal form
//! (ANF), which means every subexpression is bound to a `LocalId` by a `Let`.
//! That way last use and drops fall out of one backward scan (Perceus). The
//! passes over it live in the compiler, and `docs/vm-design.md` says what
//! running it means.

mod prim;

pub use prim::PrimOp;

use std::collections::{BTreeMap, BTreeSet};
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
    /// A place control comes together, scoped to one function.
    /// [`CoreExpr::Join`] declares it and [`CoreExpr::Jump`] goes to it. It
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

/// A typed local binding. Its [`RTy`] indexes the program's `ResolvedPool`,
/// which can't hold an unsolved inference variable, so what it says of a value
/// is never a guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoreBind {
    pub id: LocalId,
    pub ty: RTy,
}

impl CoreBind {
    pub fn new(id: LocalId, ty: RTy) -> Self {
        CoreBind { id, ty }
    }
}

/// A constructor's resolved identity, captured during lowering so a backend
/// doesn't need the type environment to dispatch on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
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

impl Callee {
    /// The local it reads, when it reads one.
    pub fn local(self) -> Option<LocalId> {
        match self {
            Callee::Local(f) => Some(f),
            Callee::Known(_) | Callee::Self_ => None,
        }
    }
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

/// The right-hand side of a `Let`. Every operand is a `LocalId`, because nested
/// evaluation has already been flattened into the enclosing spine.
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
                callee.local().into_iter().for_each(f);
            }
        }
    }
}

/// A Float literal in a pattern, by its value as `==` sees it: the two zeros are
/// one key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FloatKey(u64);

impl FloatKey {
    pub fn new(x: f64) -> Self {
        // `-0.0 + 0.0` is `0.0`, and any other number is left as it is.
        FloatKey((x + 0.0).to_bits())
    }

    pub fn get(self) -> f64 {
        f64::from_bits(self.0)
    }
}

/// The cases of a [`CoreExpr::Switch`]. Each is the values it is for and what is
/// done for them. No value is there twice, and no case is for none.
#[derive(Debug, Clone)]
pub enum Cases {
    /// By which variant of `ty` it is, as [`VariantRef::variant_idx`].
    Variant {
        ty: TypeId,
        arms: Arms<u16>,
    },
    /// By the value of an Int, which may be a big one.
    Int(Arms<i64>),
    Float(Arms<FloatKey>),
    /// By the text of a String. A constant is there once in the pool, so its id
    /// says which text it is.
    Text(Arms<ConstId>),
}

/// The expressions of a [`Cases`], by whatever its values are.
macro_rules! each_arm {
    ($cases:expr, $arms:ident => $of:expr) => {
        match $cases {
            Cases::Variant { arms: $arms, .. } => $of,
            Cases::Int($arms) => $of,
            Cases::Float($arms) => $of,
            Cases::Text($arms) => $of,
        }
    };
}

impl Cases {
    /// What is done in each case, in order.
    pub fn arms(&self) -> Vec<&CoreExpr> {
        each_arm!(self, arms => arms.iter().map(|(_, e)| e).collect())
    }

    pub fn arms_mut(&mut self) -> Vec<&mut CoreExpr> {
        each_arm!(self, arms => arms.iter_mut().map(|(_, e)| e).collect())
    }

    /// The same cases, with `f` of each in place of what is done in it.
    pub fn map(&self, mut f: impl FnMut(&CoreExpr) -> CoreExpr) -> Cases {
        match self {
            Cases::Variant { ty, arms } => Cases::Variant {
                ty: *ty,
                arms: map_arms(arms, &mut f),
            },
            Cases::Int(arms) => Cases::Int(map_arms(arms, &mut f)),
            Cases::Float(arms) => Cases::Float(map_arms(arms, &mut f)),
            Cases::Text(arms) => Cases::Text(map_arms(arms, &mut f)),
        }
    }
}

/// What each case is for, and what is done in it.
pub type Arms<K> = Vec<(Vec<K>, CoreExpr)>;

fn map_arms<K: Clone>(arms: &Arms<K>, f: &mut impl FnMut(&CoreExpr) -> CoreExpr) -> Arms<K> {
    arms.iter().map(|(keys, e)| (keys.clone(), f(e))).collect()
}

/// What a [`CoreExpr::Probe`] asks for. It is there or it is not, and asking changes
/// nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeOp {
    /// An element of an array. Args: `xs`, `i`. It finds element `i`.
    ArrayGet,
    /// Some elements of an array. Args: `xs`, `a`, `b`. It finds the array of those
    /// from `a` up to but not including `b`.
    ArraySlice,
    /// An array with another element. Args: `xs`, `i`, `x`. It finds `xs` with `x` at
    /// index `i`, when `xs` has that index.
    ArraySet,
    /// The first element of an array and the array of the others. Args: `xs`. An
    /// array with no element has neither.
    ArrayUncons,
    /// Some of the elements of an array that has `len` of them, or `len` or more when
    /// it may have `more`. Args: `xs`. It finds the elements from `from` on, as many
    /// as there are places for them, none of them past `len`. When it may have more,
    /// the last place is for the array of what is behind the first `len`.
    ///
    /// `[h, ..t]` is [`ProbeOp::ArrayUncons`].
    ArraySplit { len: u32, more: bool, from: u32 },
    /// What a map has for a key. Args: `m`, `k`. It finds that value.
    MapGet,
}

/// What a function does from some point on. It is a spine, a chain of nodes that
/// each do one thing and go on, which ends in a branch or in what is done last.
///
/// Each local is bound in one place, and each join declared in one.
#[derive(Debug, Clone)]
pub enum CoreExpr {
    Let {
        bind: CoreBind,
        rhs: Atom,
        body: Box<CoreExpr>,
    },
    /// Put `value` in a module-level slot. It is only in a toplevel, and there is
    /// one for each slot.
    SetGlobal {
        slot: GlobalSlot,
        value: LocalId,
        body: Box<CoreExpr>,
    },
    /// The one place control comes together. `body` runs, and each jump in it to
    /// `id` gives `params` their values and goes on with `cont`. `cont` can read
    /// what was bound before this node, but nothing bound in `body`, and cannot
    /// jump to `id`.
    Join {
        id: JoinId,
        params: Vec<CoreBind>,
        cont: Box<CoreExpr>,
        body: Box<CoreExpr>,
    },
    /// `perceus` inserts this after the last use of `local`. It releases the
    /// frame's reference.
    Drop { local: LocalId, body: Box<CoreExpr> },
    /// `perceus` makes this out of reads of `cell`, which is a `variant`, where it
    /// dies. The frame gives up its reference to the cell, and each local in
    /// `fields` gets the field with that index, with a reference of its own. Each
    /// field is there once, in order, and there is at least one.
    Open {
        cell: LocalId,
        variant: VariantRef,
        fields: Vec<(u16, CoreBind)>,
        body: Box<CoreExpr>,
    },
    /// Go on with the case that `scrut` is, or with `default` when it is none of
    /// them. With no `default`, it is one of them. It binds nothing, and a case
    /// reads the fields it needs of `scrut`.
    Switch {
        scrut: LocalId,
        cases: Cases,
        default: Option<Box<CoreExpr>>,
    },
    /// No arm of a match takes its value. The checker has proved that every match
    /// has one, so this never runs.
    NoArm,
    If {
        cond: LocalId,
        then: Box<CoreExpr>,
        els: Box<CoreExpr>,
    },
    /// Ask for `op` of `args`. Where it is there, put what it finds in `binds`, which
    /// is one place for each value that `op` finds, and go on with `hit`. A place may
    /// be none, for a value that nothing reads. Where it is not there, go on with
    /// `miss`, with nothing bound and every one of `args` as it was.
    Probe {
        op: ProbeOp,
        args: Vec<LocalId>,
        binds: Vec<Option<CoreBind>>,
        hit: Box<CoreExpr>,
        miss: Box<CoreExpr>,
    },
    /// Return this from the function.
    Ret(LocalId),
    /// Call this with the frame, and return what it returns.
    TailCall { callee: Callee, args: Vec<LocalId> },
    /// Go on with the continuation of the enclosing [`CoreExpr::Join`] that
    /// declares `to`, with an argument for each of its parameters.
    Jump { to: JoinId, args: Vec<LocalId> },
}

impl CoreExpr {
    /// Calls `f` for each constructor this expression builds, switches on or takes apart,
    /// once per mention, in no set order.
    pub fn for_each_variant(&self, mut f: impl FnMut(VariantRef)) {
        let mut work = vec![self];
        while let Some(e) = work.pop() {
            match e {
                CoreExpr::Let { rhs, body, .. } => {
                    rhs.builds().into_iter().for_each(&mut f);
                    work.push(body);
                }
                CoreExpr::Join { cont, body, .. } => {
                    work.push(cont);
                    work.push(body);
                }
                CoreExpr::SetGlobal { body, .. } | CoreExpr::Drop { body, .. } => work.push(body),
                CoreExpr::Open { variant, body, .. } => {
                    f(*variant);
                    work.push(body);
                }
                CoreExpr::Switch { cases, default, .. } => {
                    if let Cases::Variant { ty, arms } = cases {
                        for idx in arms.iter().flat_map(|(variants, _)| variants) {
                            f(VariantRef {
                                type_id: *ty,
                                variant_idx: *idx,
                            });
                        }
                    }
                    work.extend(cases.arms());
                    work.extend(default.as_deref());
                }
                CoreExpr::If {
                    then: a, els: b, ..
                }
                | CoreExpr::Probe {
                    hit: a, miss: b, ..
                } => {
                    work.push(a);
                    work.push(b);
                }
                CoreExpr::Ret(_)
                | CoreExpr::TailCall { .. }
                | CoreExpr::Jump { .. }
                | CoreExpr::NoArm => {}
            }
        }
    }

    /// The joins that every way through their body comes to. The continuation of
    /// any other can be left out, by a way that returns or jumps further.
    pub fn always_reached(&self) -> BTreeSet<JoinId> {
        let mut walk = Exits::default();
        walk.outermost(self, 0);
        walk.reached
    }
}

/// The walk of [`CoreExpr::always_reached`].
#[derive(Default)]
struct Exits {
    /// How many joins each join in scope is in, itself among them.
    depths: BTreeMap<JoinId, u32>,
    reached: BTreeSet<JoinId>,
}

impl Exits {
    /// The outermost place that a way through `e` ends at, as the depth of the join
    /// it jumps to, or 0 when it leaves the function. `e` is in `depth` joins. It
    /// is `u32::MAX` when no way ends.
    ///
    /// The spine is walked in a loop, so recursion goes as deep as bodies nest.
    fn outermost(&mut self, mut e: &CoreExpr, depth: u32) -> u32 {
        let mut least = u32::MAX;
        let end = loop {
            match e {
                CoreExpr::Let { body, .. }
                | CoreExpr::SetGlobal { body, .. }
                | CoreExpr::Drop { body, .. }
                | CoreExpr::Open { body, .. } => e = body,
                CoreExpr::Join { id, cont, body, .. } => {
                    let own = depth.saturating_add(1);
                    self.depths.insert(*id, own);
                    let to = self.outermost(body, own);
                    self.depths.remove(id);
                    if to == own {
                        self.reached.insert(*id);
                    } else {
                        least = least.min(to);
                    }
                    e = cont;
                }
                CoreExpr::Switch { cases, default, .. } => {
                    let all = cases.arms().into_iter().chain(default.as_deref());
                    let all = all.map(|arm| self.outermost(arm, depth));
                    break all.min().unwrap_or(u32::MAX);
                }
                CoreExpr::If {
                    then: a, els: b, ..
                }
                | CoreExpr::Probe {
                    hit: a, miss: b, ..
                } => {
                    let a = self.outermost(a, depth);
                    break a.min(self.outermost(b, depth));
                }
                CoreExpr::Ret(_) | CoreExpr::TailCall { .. } => break 0,
                CoreExpr::NoArm => break u32::MAX,
                CoreExpr::Jump { to, .. } => break self.depths.get(to).copied().unwrap_or(0),
            }
        };
        least.min(end)
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
        write!(f, "{}:{}", self.id, self.ty)
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

fn write_list(f: &mut fmt::Formatter<'_>, xs: &[impl fmt::Display]) -> fmt::Result {
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
                write_list(f, fields)?;
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
                write_list(f, args)?;
                f.write_str(")")
            }
            Atom::Intrinsic { intrinsic, args } => {
                write!(f, "{intrinsic:?}(")?;
                write_list(f, args)?;
                f.write_str(")")
            }
            Atom::Closure { func_idx, captures } => {
                write!(f, "closure {func_idx}(")?;
                write_list(f, captures)?;
                f.write_str(")")
            }
            Atom::Call { callee, args } => {
                write!(f, "call {callee}(")?;
                write_list(f, args)?;
                f.write_str(")")
            }
        }
    }
}

impl fmt::Display for FloatKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.get())
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
                CoreExpr::SetGlobal { slot, value, body } => {
                    writeln!(f, "set global{} = {value}", slot.0)?;
                    cur = body;
                }
                // The body comes first, as it runs first.
                CoreExpr::Join {
                    id,
                    params,
                    cont,
                    body,
                } => {
                    write!(f, "join {id}(")?;
                    write_list(f, params)?;
                    writeln!(f, ") after")?;
                    Indented(body, depth.saturating_add(1)).fmt(f)?;
                    cur = cont;
                }
                CoreExpr::Drop { local, body } => {
                    writeln!(f, "drop {local}")?;
                    cur = body;
                }
                CoreExpr::Ret(value) => {
                    return writeln!(f, "ret {value}");
                }
                CoreExpr::TailCall { callee, args } => {
                    write!(f, "tail call {callee}(")?;
                    write_list(f, args)?;
                    return writeln!(f, ")");
                }
                CoreExpr::Jump { to, args } => {
                    write!(f, "jump {to}(")?;
                    write_list(f, args)?;
                    return writeln!(f, ")");
                }
                CoreExpr::If { cond, then, els } => {
                    writeln!(f, "if {cond}")?;
                    Indented(then, depth.saturating_add(1)).fmt(f)?;
                    pad(f, depth)?;
                    writeln!(f, "else")?;
                    return Indented(els, depth.saturating_add(1)).fmt(f);
                }
                CoreExpr::Open {
                    cell,
                    variant,
                    fields,
                    body,
                } => {
                    let VariantRef {
                        type_id,
                        variant_idx,
                    } = variant;
                    write!(f, "open {cell} {}.{variant_idx}(", type_id.0)?;
                    for (i, (field, bind)) in fields.iter().enumerate() {
                        let comma = if i > 0 { ", " } else { "" };
                        write!(f, "{comma}{field}: {bind}")?;
                    }
                    writeln!(f, ")")?;
                    cur = body;
                }
                CoreExpr::Switch {
                    scrut,
                    cases,
                    default,
                } => {
                    writeln!(f, "switch {scrut}")?;
                    let mut arm = |keys: &dyn Fn(&mut fmt::Formatter<'_>) -> fmt::Result,
                                   body: &CoreExpr| {
                        pad(f, depth.saturating_add(1))?;
                        f.write_str("| ")?;
                        keys(f)?;
                        f.write_str(" ->\n")?;
                        Indented(body, depth.saturating_add(2)).fmt(f)
                    };
                    match cases {
                        Cases::Variant { ty, arms } => {
                            for (variants, body) in arms {
                                let named = variants.iter().map(|v| format!("{}.{v}", ty.0));
                                let named: Vec<String> = named.collect();
                                arm(&|f| write_list(f, &named), body)?;
                            }
                        }
                        Cases::Int(arms) => {
                            for (keys, body) in arms {
                                arm(&|f| write_list(f, keys), body)?;
                            }
                        }
                        Cases::Float(arms) => {
                            for (keys, body) in arms {
                                arm(&|f| write_list(f, keys), body)?;
                            }
                        }
                        Cases::Text(arms) => {
                            for (keys, body) in arms {
                                arm(&|f| write_list(f, keys), body)?;
                            }
                        }
                    }
                    if let Some(body) = default {
                        arm(&|f| f.write_str("_"), body)?;
                    }
                    return Ok(());
                }
                CoreExpr::Probe {
                    op,
                    args,
                    binds,
                    hit,
                    miss,
                } => {
                    write!(f, "probe {op:?}(")?;
                    write_list(f, args)?;
                    writeln!(f, ")")?;
                    let named = binds.iter().map(|bind| match bind {
                        Some(bind) => bind.to_string(),
                        None => "_".to_string(),
                    });
                    let named: Vec<String> = named.collect();
                    let inner = depth.saturating_add(1);
                    pad(f, inner)?;
                    write!(f, "| hit(")?;
                    write_list(f, &named)?;
                    writeln!(f, ") ->")?;
                    Indented(hit, depth.saturating_add(2)).fmt(f)?;
                    pad(f, inner)?;
                    writeln!(f, "| miss ->")?;
                    return Indented(miss, depth.saturating_add(2)).fmt(f);
                }
                CoreExpr::NoArm => return writeln!(f, "no arm"),
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
    write_list(f, &core.params)?;
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
