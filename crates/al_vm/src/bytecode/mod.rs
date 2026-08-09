//! The compiled program: the instruction set, its 8-byte encoding, and the
//! [`Program`] container the VM executes. This is the whole contract between
//! a front-end compiler (AL's lives in `al_core`) and the VM.
//!
//! Three facts the rest of the crate leans on:
//!
//! 1. An [`Instruction`] is one aligned 8-byte word, so the dispatch loop's
//!    fetch is a single load via [`fetch`]. `a`/`b` reclaim padding bytes for
//!    superinstructions needing a second and third operand.
//! 2. [`Program::constants`] point into [`Program::frozen`], so they are
//!    stable raw pointers for the program's whole life, readable from every
//!    scheduler thread, and skipped by every per-process GC.
//! 3. `Program` is `Send + Sync` (asserted below), so a worker scheduler
//!    thread takes a plain `clone()` of the shared program.
//!
//! Adding an opcode touches five places: [`Op::has_jump_target`] and
//! [`Op::pushes_extra`] (both exhaustive, so they fail to compile until you
//! classify the new op), emission in `al_core`'s compiler, dispatch in
//! [`crate::vm::exec`], and `al_core::bytecode::builtin_op` if it is exposed
//! as a `@vm` intrinsic.

pub mod bits;
pub mod hamt;
pub mod native;
pub(crate) mod scratch;
pub mod seq;
pub mod value;
use std::sync::Arc;

pub use native::{NativeCtx, NativeEntry, NativeStatus, NativeTable};
pub use value::{
    Arena, BinaryRef, ClosureRef, EnumRef, HeapTag, MapBacking, MapRef, SeqRef, SocketValue, Value,
    ValueView, enum_hash_with_payload, enum_name_prefix_hash, freed_objects_pending, hash_value,
    take_freed_objects, values_equal,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Op {
    // Stack manipulation
    PushConst,
    PushLocal,
    PushGlobal,
    StoreLocal,
    PushNil,
    PushTrue,
    PushFalse,
    Pop,
    Dup,

    // Arithmetic
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Neg,

    // Typed arithmetic, emitted when the operand type resolves to a concrete
    // prim. The generic ops above are the polymorphic fallback.
    AddInt,
    SubInt,
    MulInt,
    DivInt,
    ModInt,
    NegInt,
    AddFloat,
    SubFloat,
    MulFloat,
    DivFloat,
    NegFloat,
    AddStr,

    // Comparison
    Eq,
    Neq,
    Lt,
    Gt,
    Lte,
    Gte,

    // Typed comparison
    LtInt,
    GtInt,
    LteInt,
    GteInt,
    EqInt,
    NeqInt,
    LtFloat,
    GtFloat,
    LteFloat,
    GteFloat,

    // Logic
    Not,

    // Control flow
    Jump,
    JumpIfFalse,
    Call,
    TailCall,
    /// Self-recursive call: `func_idx` comes from the live frame. operand = argc.
    CallSelf,
    TailCallSelf,
    /// Known-target call: the callee is a capture-free top-level fn whose
    /// `func_idx` is a compile-time immediate, so the callee value is never
    /// pushed and `Call`'s tag/arity checks are elided. b = argc,
    /// operand = func_idx.
    CallKnown,
    TailCallKnown,
    Ret,

    // Superinstructions fused by the peephole pass. Packed operands live in
    // `Instruction.{a,b}`; jump targets stay in `operand`.
    /// `stack[base+a] - constants[b]` → push. (PushLocal;PushConst;SubInt)
    SubIntLC,
    /// `stack[base+a] + constants[b]` → push. (PushLocal;PushConst;AddInt)
    AddIntLC,
    /// `if stack[base+a] >= constants[b] { ip = operand }`.
    /// (PushLocal;PushConst;LtInt;JumpIfFalse)
    JumpGeIntLC,
    /// `if stack[base+a] != constants[b] { ip = operand }`.
    /// (PushLocal;PushConst;EqInt;JumpIfFalse)
    JumpNeIntLC,
    /// Peephole padding so absolute jump targets stay valid after fusion.
    Nop,

    // Data structures
    MakeArray,
    /// `[e0, .., e_{n-1}] -> tuple`. `operand` = n. No reuse variant: `lower`
    /// pairs `Drop`/`Reuse` tokens only for user-declared constructors.
    MakeTuple,
    TupleIndex,
    MakeRange,
    Index,
    /// `[arr, idx, default] -> elem|default`. Skips the `Some` box `Index`
    /// must build and an immediately-following `or` then throws away. Only
    /// fused when the default is a pure atom, so eager evaluation is free;
    /// `or { <expression> }` is left alone.
    IndexOr,
    /// `[arr] -> arr[operand]` — unchecked fetch, no `Option` wrapper.
    /// Emitted by array-destructuring patterns only after a length check has
    /// proven the index in bounds.
    ElemAt,
    ArrayLen,
    ArraySlice,
    ArrayConcat,
    /// `[e0, .., e_{k-1}, ..seq]` — prepend k stack elements onto `seq`
    /// (`operand` = k). Sublinear front-cons.
    Prepend,
    /// `[seq, n] -> seq[n..]` — structure-shared tail. Replaces `ArraySlice`
    /// in `[h, ..rest]` patterns.
    SeqDrop,
    /// `[seq, e] -> seq` with `e` pushed on the back. No stdlib use today.
    Append,
    GetField,
    /// `[enum] -> payload[operand]` — as `GetField` but without the tag and
    /// bounds checks. Emitted when the scrutinee type is a resolved `Con`.
    GetFieldUnchecked,

    // Tagged values (enums / custom types)
    /// `[type_id, enum_name, variant_name, labels, p0, .., p_{b-1}, reuse?] ->
    /// enum`. `b` = payload arity, `operand` = prehash constant idx. `a` = 1
    /// when a Perceus reuse token sits on top of the payloads.
    MakeEnumPayload,
    MatchEnum,
    UnwrapEnum,
    /// `[enum] -> ` — computed jump by variant index. `a` = variant count,
    /// `operand` = base of a contiguous table of `a` `Jump` instructions, one
    /// per variant. Emitted for an exhaustive match on a resolved enum.
    SwitchTag,

    // Closures
    /// `[cap0, .., cap_{cc-1}, reuse?] -> closure`. `operand` = func_idx.
    /// `a` = 1 when a Perceus reuse token sits on top of the captures.
    MakeClosure,
    PushCapture,
    PushSelf,

    // Perceus drop-guided reuse (frame-limited, ICFP'22). `core_ir::perceus`
    // is authoritative.
    /// `[]` — operand = local slot. Last use of `stack[base+slot]`: release
    /// the frame's reference. If the value is uniquely owned (rc==1) the cell
    /// stays in the slot for a following same-shape `Reuse` instead of being
    /// freed; the slot is the per-frame reuse table. Pushes nothing.
    Drop,
    /// `[] -> cell|nil` — operand = local slot. Pushes the cell a preceding
    /// `Drop` parked there, or nil so the constructor allocates fresh.
    /// `MakeEnumPayload` is the only consumer.
    ///
    /// A token survives an intervening `Call`. "Frame-limited" means the
    /// callee never sees the parked cell, not that a call clears the token —
    /// clearing at calls would delete every reuse in
    /// `map xs f = match xs { Cons(h, t) -> Cons(f h, map t f) }`.
    Reuse,

    // String operations
    ToString,
    StrConcatN,
    StrSplit,
    StrLen,
    StrContains,
    StrTrim,
    IntToString,

    // Binary operations
    BinFromString,
    BinToString,
    BinBitSize,
    BinByteSize,
    BinSlice,
    BinAppend,
    /// `[b1, .., bn] -> Binary` — N-ary concatenation (operand = n). Emitted
    /// for multi-segment `<<>>` literals; a `BinAppend` chain would re-copy
    /// the accumulated prefix per segment. The binary mirror of `StrConcatN`.
    BinConcatN,
    BinFromInt,
    BinReadInt,
    BinTake,
    BinReadUtf8,
    /// `[bin, at_bits, prefix] -> Bool` — whether `bin`'s logical bits from
    /// `at_bits` begin with `prefix`'s. Out-of-range is `false`, never an
    /// error. Emitted by `<<>>` pattern codegen for literal segments.
    BinMatchPrefix,
    /// `[bin, at_bits, len_bits] -> Binary` — O(1) sub-view sharing the
    /// backing, no `Option` wrapper. Emitted by `<<>>` pattern codegen after
    /// its own bounds check; `BinSlice` is the checked public builtin.
    BinView,
    /// `[haystack, needle, from] -> Option(Int)` — byte-substring search.
    BinIndexOf,
    /// `[bin, i] -> Option(Int)` — byte at index `i`, or `None` when out of range.
    BinByteAt,
    /// `[bin, radix] -> Option(Int)` — ASCII integer parse (radix 10/16),
    /// overflow-checked (returns `None` rather than wrapping).
    BinParseInt,
    /// `[a, b] -> Bool` — ASCII-case-insensitive byte equality.
    BinEqIgnoreAsciiCase,
    /// `[bin] -> Binary` — ASCII-lowercased copy.
    BinToAsciiLower,
    /// `[n, radix] -> Binary` — render an Int as ASCII (radix 10/16).
    BinFromIntAscii,

    // HTTP/1.1 protocol ops (al/http/h1, al/http/headers). The byte scanning
    // and value assembly run in native code, while every protocol
    // *decision* (framing precedence, keep-alive, 100-continue) stays in AL.
    /// `[buf, off] -> Parsed` — parse one request head (request line + header
    /// block) from `buf` at byte offset `off`. Pushes an `al/http/h1.Parsed`:
    /// `Done(method, target, version, headers, consumed)` / `NeedMore` /
    /// `Bad(status)`. Method, target, and header names/values are zero-copy
    /// views into `buf`'s backing.
    HttpParseHead,
    /// `[headers] -> Framing` — RFC 7230 §3.3.3 body framing over an
    /// `Array(Header)`: `NoBody` / `Length(n)` / `Chunked` / `Invalid(status)`.
    HttpFraming,
    /// `[buf, off, max] -> ChunkBody` — decode a chunked transfer-encoded body
    /// from `buf` at byte offset `off`, refusing to decode more than `max`
    /// bytes. Pushes an `al/http/h1.ChunkBody`: `ChunkedDone(body, trailers,
    /// consumed)` / `ChunkedNeedMore` / `ChunkedBad(status)`. The decoded body
    /// is one owned binary; trailer names/values are zero-copy views.
    HttpChunkDecode,
    /// `[headers, name] -> Option(Binary)` — value of the first header whose
    /// name matches `name` ASCII-case-insensitively.
    HttpHeaderGet,
    /// `[headers, name] -> Bool` — whether any header name matches `name`
    /// ASCII-case-insensitively.
    HttpHeaderHas,
    HttpHeadersValid,
    /// `[code, reason, headers] -> Binary` — serialize a response head (status
    /// line, header block, terminating blank line) as one contiguous buffer.
    HttpSerializeHead,

    // Float operations
    FloatFloor,
    FloatCeil,
    FloatRound,
    FloatTruncate,
    FloatFromInt,
    FloatToString,

    // Misc
    Print,
    StackDepth,
    Halt,

    // I/O operations
    FileRead,
    FileWrite,
    TcpListen,
    TcpAccept,
    TcpConnect,
    TcpRead,
    /// Read with an absolute monotonic-ms deadline:
    /// `[sock, max, deadline_ms] -> Result(Binary, NetError)`. Parks until data
    /// arrives, the peer closes, or the deadline passes (then `Err`).
    TcpReadUntil,
    TcpWrite,
    /// Vectored write: `[sock, Array(Binary)] -> Result(Nil, NetError)` in one
    /// writev syscall.
    TcpWriteParts,
    TcpClose,
    TcpCloseServer,
    TcpLocalAddr,
    /// `[host] -> Result(IpAddress, NetError)` — resolve a hostname to an IP
    /// address. IP literals return immediately; hostnames offload to the
    /// blocking pool so the syscall never stalls the scheduler. (al/net.resolve)
    DnsResolve,
    /// `[String] -> Option(IpAddress)` — parse an IP literal, `None` on
    /// anything else. The only supported constructor for `IpAddress`.
    /// (al/net/address.parse)
    IpParse,

    // Concurrency (al/scheduler)
    /// Spawn a lightweight process running the popped closure.
    ProcessSpawn,
    /// Spawn the popped closure pinned to the current scheduler — the child
    /// runs on the core that spawned it and any sockets it captured stay in
    /// place (no fd move, no cross-core handoff). Used by the per-core accept
    /// loop so a connection is handled on the core that accepted it.
    SpawnLocal,
    /// Spawn one copy of the popped closure pinned to every live scheduler.
    /// Drives the shared-nothing accept fan-out: each core runs its own
    /// acceptor against its own `SO_REUSEPORT` socket.
    SpawnOnEach,
    /// Park the current process for `ms` milliseconds.
    Sleep,
    /// Push milliseconds elapsed since a process-global monotonic epoch (Int).
    Monotonic,

    /// Push the program's command-line arguments as an `Array(String)`: the
    /// entrypoint path followed by every argument given after it on the
    /// command line. (al/process.argv)
    Argv,

    // Maps (al/map). A `Map(k, v)` is an opaque heap value with a pluggable
    // backing; the only backing today is the zero-copy process-environment
    // view produced by `EnvMap`. Read ops dispatch on the backing.
    /// Push a `Map(String, String)` that reads through to the process
    /// environment — no environment data is copied. (al/process.env)
    EnvMap,
    /// `[map, key] -> Option(v)` — look up a key. (al/map.get)
    MapGet,
    /// `[map, key] -> Bool` — membership test. (al/map.has)
    MapHas,
    /// `[map] -> Array(k)` — every key, in the backing's iteration order.
    /// (al/map.keys)
    MapKeys,
    /// `[map] -> Array(v)` — every value, parallel to `MapKeys`. (al/map.values)
    MapValues,
    /// `[map] -> Int` — number of entries. (al/map.size)
    MapSize,
    /// `[] -> Map(k, v)` — a fresh empty in-memory (HAMT) map. (al/map.new)
    MapNew,
    /// `[map, key, value] -> Map(k, v)` — `map` with `key` bound to `value`,
    /// as a new map sharing untouched subtrees. (al/map.set)
    MapSet,
    /// `[map, key] -> Map(k, v)` — `map` without `key`. (al/map.delete)
    MapDelete,
    /// `[map] -> Array((k, v))` — every entry as a `(key, value)` tuple.
    /// (al/map.to_list)
    MapToList,
}

impl Op {
    /// True when this op's `operand` is an instruction index — absolute in
    /// [`Program::code`], function-relative inside a block `core_ir::emit`
    /// has just produced — rather than a constant id, slot, count, or
    /// function index. Jump-patching, peephole fusion and
    /// `core_ir::emit::relocate` all key off this, so it is the ONE authority
    /// on which operands are addresses.
    ///
    /// The match is exhaustive on purpose: a new opcode does not compile until
    /// its operand is classified here, rather than defaulting to "not an
    /// address" and silently surviving relocation unshifted.
    pub const fn has_jump_target(self) -> bool {
        match self {
            // `SwitchTag`'s operand is the jump-table base; the table's own
            // entries are ordinary `Jump`s, covered by the first arm.
            Op::Jump | Op::JumpIfFalse | Op::JumpGeIntLC | Op::JumpNeIntLC | Op::SwitchTag => true,

            Op::PushConst
            | Op::PushLocal
            | Op::PushGlobal
            | Op::StoreLocal
            | Op::PushNil
            | Op::PushTrue
            | Op::PushFalse
            | Op::Pop
            | Op::Dup
            | Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Div
            | Op::Mod
            | Op::Neg
            | Op::AddInt
            | Op::SubInt
            | Op::MulInt
            | Op::DivInt
            | Op::ModInt
            | Op::NegInt
            | Op::AddFloat
            | Op::SubFloat
            | Op::MulFloat
            | Op::DivFloat
            | Op::NegFloat
            | Op::AddStr
            | Op::Eq
            | Op::Neq
            | Op::Lt
            | Op::Gt
            | Op::Lte
            | Op::Gte
            | Op::LtInt
            | Op::GtInt
            | Op::LteInt
            | Op::GteInt
            | Op::EqInt
            | Op::NeqInt
            | Op::LtFloat
            | Op::GtFloat
            | Op::LteFloat
            | Op::GteFloat
            | Op::Not
            | Op::Call
            | Op::TailCall
            | Op::CallSelf
            | Op::TailCallSelf
            | Op::CallKnown
            | Op::TailCallKnown
            | Op::Ret
            | Op::SubIntLC
            | Op::AddIntLC
            | Op::Nop
            | Op::MakeArray
            | Op::MakeTuple
            | Op::TupleIndex
            | Op::MakeRange
            | Op::Index
            | Op::IndexOr
            | Op::ElemAt
            | Op::ArrayLen
            | Op::ArraySlice
            | Op::ArrayConcat
            | Op::Prepend
            | Op::SeqDrop
            | Op::Append
            | Op::GetField
            | Op::GetFieldUnchecked
            | Op::MakeEnumPayload
            | Op::MatchEnum
            | Op::UnwrapEnum
            | Op::MakeClosure
            | Op::PushCapture
            | Op::PushSelf
            | Op::Drop
            | Op::Reuse
            | Op::ToString
            | Op::StrConcatN
            | Op::StrSplit
            | Op::StrLen
            | Op::StrContains
            | Op::StrTrim
            | Op::IntToString
            | Op::BinFromString
            | Op::BinToString
            | Op::BinBitSize
            | Op::BinByteSize
            | Op::BinSlice
            | Op::BinAppend
            | Op::BinConcatN
            | Op::BinFromInt
            | Op::BinReadInt
            | Op::BinTake
            | Op::BinReadUtf8
            | Op::BinMatchPrefix
            | Op::BinView
            | Op::BinIndexOf
            | Op::BinByteAt
            | Op::BinParseInt
            | Op::BinEqIgnoreAsciiCase
            | Op::BinToAsciiLower
            | Op::BinFromIntAscii
            | Op::HttpParseHead
            | Op::HttpFraming
            | Op::HttpChunkDecode
            | Op::HttpHeaderGet
            | Op::HttpHeaderHas
            | Op::HttpHeadersValid
            | Op::HttpSerializeHead
            | Op::FloatFloor
            | Op::FloatCeil
            | Op::FloatRound
            | Op::FloatTruncate
            | Op::FloatFromInt
            | Op::FloatToString
            | Op::Print
            | Op::StackDepth
            | Op::Halt
            | Op::FileRead
            | Op::FileWrite
            | Op::TcpListen
            | Op::TcpAccept
            | Op::TcpConnect
            | Op::TcpRead
            | Op::TcpReadUntil
            | Op::TcpWrite
            | Op::TcpWriteParts
            | Op::TcpClose
            | Op::TcpCloseServer
            | Op::TcpLocalAddr
            | Op::DnsResolve
            | Op::IpParse
            | Op::ProcessSpawn
            | Op::SpawnLocal
            | Op::SpawnOnEach
            | Op::Sleep
            | Op::Monotonic
            | Op::Argv
            | Op::EnvMap
            | Op::MapGet
            | Op::MapHas
            | Op::MapKeys
            | Op::MapValues
            | Op::MapSize
            | Op::MapNew
            | Op::MapSet
            | Op::MapDelete
            | Op::MapToList => false,
        }
    }

    /// True when this op's lowering is not "push args, dispatch, exactly one
    /// value on the stack": `Print` leaves nothing (emit appends `PushNil`)
    /// and `BinReadUtf8` leaves two words (emit appends `MakeTuple 2`).
    /// `core_ir::emit`'s operand hoisting and if-condition fusion both treat
    /// a primop as a single-value expression, so they key off this and it is
    /// the ONE authority on which primops break that shape.
    ///
    /// The match is exhaustive on purpose: a new opcode does not compile
    /// until its stack effect is classified here, rather than defaulting to
    /// "one value" and silently miscompiling around hoisting/fusion.
    pub const fn pushes_extra(self) -> bool {
        match self {
            Op::Print | Op::BinReadUtf8 => true,

            Op::PushConst
            | Op::PushLocal
            | Op::PushGlobal
            | Op::StoreLocal
            | Op::PushNil
            | Op::PushTrue
            | Op::PushFalse
            | Op::Pop
            | Op::Dup
            | Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Div
            | Op::Mod
            | Op::Neg
            | Op::AddInt
            | Op::SubInt
            | Op::MulInt
            | Op::DivInt
            | Op::ModInt
            | Op::NegInt
            | Op::AddFloat
            | Op::SubFloat
            | Op::MulFloat
            | Op::DivFloat
            | Op::NegFloat
            | Op::AddStr
            | Op::Eq
            | Op::Neq
            | Op::Lt
            | Op::Gt
            | Op::Lte
            | Op::Gte
            | Op::LtInt
            | Op::GtInt
            | Op::LteInt
            | Op::GteInt
            | Op::EqInt
            | Op::NeqInt
            | Op::LtFloat
            | Op::GtFloat
            | Op::LteFloat
            | Op::GteFloat
            | Op::Not
            | Op::Jump
            | Op::JumpIfFalse
            | Op::Call
            | Op::TailCall
            | Op::CallSelf
            | Op::TailCallSelf
            | Op::CallKnown
            | Op::TailCallKnown
            | Op::Ret
            | Op::SubIntLC
            | Op::AddIntLC
            | Op::JumpGeIntLC
            | Op::JumpNeIntLC
            | Op::Nop
            | Op::MakeArray
            | Op::MakeTuple
            | Op::TupleIndex
            | Op::MakeRange
            | Op::Index
            | Op::IndexOr
            | Op::ElemAt
            | Op::ArrayLen
            | Op::ArraySlice
            | Op::ArrayConcat
            | Op::Prepend
            | Op::SeqDrop
            | Op::Append
            | Op::GetField
            | Op::GetFieldUnchecked
            | Op::MakeEnumPayload
            | Op::MatchEnum
            | Op::UnwrapEnum
            | Op::SwitchTag
            | Op::MakeClosure
            | Op::PushCapture
            | Op::PushSelf
            | Op::Drop
            | Op::Reuse
            | Op::ToString
            | Op::StrConcatN
            | Op::StrSplit
            | Op::StrLen
            | Op::StrContains
            | Op::StrTrim
            | Op::IntToString
            | Op::BinFromString
            | Op::BinToString
            | Op::BinBitSize
            | Op::BinByteSize
            | Op::BinSlice
            | Op::BinAppend
            | Op::BinConcatN
            | Op::BinFromInt
            | Op::BinReadInt
            | Op::BinTake
            | Op::BinMatchPrefix
            | Op::BinView
            | Op::BinIndexOf
            | Op::BinByteAt
            | Op::BinParseInt
            | Op::BinEqIgnoreAsciiCase
            | Op::BinToAsciiLower
            | Op::BinFromIntAscii
            | Op::HttpParseHead
            | Op::HttpFraming
            | Op::HttpChunkDecode
            | Op::HttpHeaderGet
            | Op::HttpHeaderHas
            | Op::HttpHeadersValid
            | Op::HttpSerializeHead
            | Op::FloatFloor
            | Op::FloatCeil
            | Op::FloatRound
            | Op::FloatTruncate
            | Op::FloatFromInt
            | Op::FloatToString
            | Op::StackDepth
            | Op::Halt
            | Op::FileRead
            | Op::FileWrite
            | Op::TcpListen
            | Op::TcpAccept
            | Op::TcpConnect
            | Op::TcpRead
            | Op::TcpReadUntil
            | Op::TcpWrite
            | Op::TcpWriteParts
            | Op::TcpClose
            | Op::TcpCloseServer
            | Op::TcpLocalAddr
            | Op::DnsResolve
            | Op::IpParse
            | Op::ProcessSpawn
            | Op::SpawnLocal
            | Op::SpawnOnEach
            | Op::Sleep
            | Op::Monotonic
            | Op::Argv
            | Op::EnvMap
            | Op::MapGet
            | Op::MapHas
            | Op::MapKeys
            | Op::MapValues
            | Op::MapSize
            | Op::MapNew
            | Op::MapSet
            | Op::MapDelete
            | Op::MapToList => false,
        }
    }
}
/// A single bytecode instruction. `a`/`b` are packed sub-operands that reclaim
/// the 3 bytes of padding between `op` and `operand`; single-operand ops leave
/// them zero. Layout: 1B op + 1B a + 2B b + 4B operand = 8B, `Copy`.
///
/// `repr(C)` pins the field order so `as_u64`/`from_u64` are sound; the
/// dispatch loop fetches via that path so the per-instruction read stays a
/// single 8-byte load even when the loop and this type live in different
/// crates (LLVM's load-combine pass otherwise misses it across the boundary
/// — costs ~30% on `bench_heavy.al`).
#[derive(Debug, Clone, Copy)]
#[repr(C, align(8))]
pub struct Instruction {
    pub op: Op,
    pub a: u8,
    pub b: u16,
    pub operand: i32,
}

const _: () = assert!(std::mem::size_of::<Instruction>() == 8);
const _: () = assert!(std::mem::align_of::<Instruction>() == 8);

/// Fetch one instruction with a single 8-byte load. See the `Instruction` doc
/// for why the dispatch loop must go through this rather than `code[i]`.
///
/// Returns `None` when `i` is out of bounds.
// Designated unsafe scope: the bounded u64-load + transmute below is the one
// unsafe site in this module. Measured safe alternatives, both rejected
// (bench_heavy.al, best-of-N, 2026-06): plain `code.get(i).copied()` on this
// struct +19%; `repr(transparent)` u64 newtype with shift accessors +8-11%
// even with an unsafe op decode, +18% fully safe via num_enum — LLVM
// schedules the dispatch loop worse against shift extraction than against
// SROA'd struct fields, so the packed-newtype "zero-cost" theory fails.
#[allow(unsafe_code)]
#[inline(always)]
pub fn fetch(code: &[Instruction], i: usize) -> Option<Instruction> {
    if i >= code.len() {
        return None;
    }
    // SAFETY: `i < code.len()` is checked above, so the read is in-bounds.
    // `repr(C, align(8))` + `repr(u8)` on `Op` + the size/align asserts
    // pin `Instruction` to a fully-initialized 8-byte POD, so reading the slice
    // as `[u64]` is sound and transmuting one element back recovers the same
    // `Instruction` (the `Op` byte is valid because it came from `code`). The
    // u64-then-transmute path is the point — it forces one `ldr` where SROA on
    // a direct struct copy would otherwise emit four separate field loads.
    unsafe {
        let raw = *code.as_ptr().cast::<u64>().add(i);
        Some(std::mem::transmute::<u64, Instruction>(raw))
    }
}

#[derive(Debug, Clone)]
pub struct Function {
    pub name: Arc<str>,
    pub arity: i32,
    pub locals: i32,
    pub capture_count: i32,
    pub code_start: i32,
    pub code_len: i32,
}

#[derive(Debug, Clone, Default)]
pub struct Program {
    pub constants: Vec<Value>,
    pub functions: Vec<Function>,
    pub code: Vec<Instruction>,
    pub entry: i32,
    /// The program-wide frozen area the `constants`
    /// were built into by the compiler / hydration `FrozenBuilder`. `Arc`-held
    /// here so the area lives exactly as long as the program that points into
    /// it; every scheduler's clone of the program shares the same area.
    pub frozen: Arc<crate::frozen::FrozenArea>,
    /// Compiled-function entry points, one slot per `functions` entry and
    /// indexed by the same `FuncIdx` numbering. `Arc`-backed like `frozen`,
    /// so every scheduler's clone of the program consults one table. Sized
    /// by [`compiler`] once the function list is final; an empty table (the
    /// default) means "interpret everything". See [`native`].
    pub native: NativeTable,
    /// Constructors the VM may instantiate, interned into `frozen` by the
    /// front end. Indexed by the `abi` table below.
    pub templates: crate::tivec::TiVec<crate::abi::TemplateIdx, crate::template::EnumTemplate>,
    /// Which template answers each [`AbiSlot`](crate::abi::AbiSlot) — the
    /// runtime's only knowledge of a front end's stdlib.
    pub abi: crate::template::AbiTable,
}

// Worker scheduler threads clone the shared program (load-bearing fact 3):
// constants are frozen/immediate words, names are `Arc<str>`, the area is
// `Arc<FrozenArea>`. This must stay thread-shareable.
const _: () = crate::assert_send_sync::<Program>();

pub fn op(o: Op) -> Instruction {
    Instruction {
        op: o,
        a: 0,
        b: 0,
        operand: 0,
    }
}

pub fn op_arg(o: Op, operand: i32) -> Instruction {
    Instruction {
        op: o,
        a: 0,
        b: 0,
        operand,
    }
}

pub fn op_ab(o: Op, a: u8, b: u16, operand: i32) -> Instruction {
    Instruction {
        op: o,
        a,
        b,
        operand,
    }
}
