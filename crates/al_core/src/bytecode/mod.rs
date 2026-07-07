//! The compiled program: the instruction set, its 8-byte encoding, and the
//! [`Program`] container the VM executes.
//!
//! This module is the contract between the compiler (this crate) and the VM
//! (the `al` crate): everything needed to build, describe, and ship a
//! runnable program lives under `bytecode/`. The VM consumes these types;
//! it never sees the AST.
//!
//! # The three load-bearing facts
//!
//! 1. **An [`Instruction`] is one aligned 8-byte word** (1B op + 1B `a` +
//!    2B `b` + 4B operand, `repr(C)`), so the dispatch loop's fetch is a
//!    single load via [`fetch`]. `a`/`b` reclaim padding bytes for
//!    superinstructions that need a second and third operand.
//! 2. **[`Program::constants`] point into [`Program::frozen`]**, the
//!    `Arc`-held program-wide frozen area. Constants are therefore stable
//!    raw pointers for the program's whole life, readable from every
//!    scheduler thread, and skipped by every per-process GC.
//! 3. **`Program` is `Send + Sync`** (asserted below): constants are frozen
//!    or immediate words and function names are `Arc<str>`s, so a worker
//!    scheduler thread takes a plain `clone()` of the shared program — no
//!    owned mirror, no per-thread constant re-hydration.
//!
//! Adding an opcode touches two places: emission in [`compiler`] and
//! dispatch in the VM's interpreter loop. Heap values the op builds are
//! reference counted, so it simply allocates its result.
//!
//! # Reading order
//!
//! | file                | the one thing it does                          |
//! |---------------------|------------------------------------------------|
//! | this file           | [`Op`], [`Instruction`], [`Function`],         |
//! |                     | [`Program`]                                    |
//! | [`compiler`]        | AST → `Program`: HM inference fused with       |
//! |                     | bytecode emission                              |
//! | `session`           | LSP layer: `IncrementalSession`, `Watermark`   |
//! |                     | rollback, reference-graph finalization         |
//! | `peephole`          | superinstruction fusion over the emitted code  |
//! | `analysis`          | module top level: multi-pass declaration       |
//! |                     | analysis (type heads → aliases → slots →       |
//! |                     | ctors → SCC inference)                         |
//! | `prelude` /         | load `src/std/al.al` into every compile;       |
//! | [`prelude_bindings`]| capture strongly-typed handles to its names    |
//! | [`value`]           | the NaN-boxed [`Value`] word, heap object      |
//! |                     | layouts, the [`Arena`] trait                   |
//! | [`seq`]             | the persistent RRB vector backing `Array`      |
//! | [`hamt`]            | the persistent HAMT backing `Map`              |

mod analysis;
pub mod bits;
pub mod compiler;
pub mod hamt;
mod peephole;
mod prelude;
pub mod prelude_bindings;
pub mod seq;
mod session;
pub mod value;
use std::sync::Arc;

pub use compiler::*;
pub use prelude_bindings::{CtorRef, PreludeBindings, TypeRef};
pub use session::{HoverFact, IncrementalSession, Watermark};
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

    // Typed arithmetic — emitted by `compile_binary` when the operand `Ty`
    // resolves to a concrete prim post-unification. The generic ops above
    // remain as the polymorphic fallback.
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
    JumpIfTrue,
    Call,
    TailCall,
    /// Self-recursive call: `func_idx` is read from the live frame, skipping
    /// the `PushSelf; pop; match Closure` dance. operand = argc.
    CallSelf,
    TailCallSelf,
    Ret,

    // Superinstructions — fused hot sequences from the peephole pass. Packed
    // operands live in `Instruction.{a,b}`; jump targets stay in `operand`.
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
    MakeTuple,
    TupleIndex,
    MakeRange,
    Index,
    /// `[arr] -> arr[operand]` — unchecked element fetch, index as an
    /// immediate operand, no `Option` wrapper. Emitted by array-destructuring
    /// patterns *after* a length check has proven the index in-bounds, so it
    /// skips the `Some(_)` box/unbox round-trip that `Index; UnwrapEnum` pays
    /// per element on every stdlib list traversal.
    ElemAt,
    /// `[arr, idx] -> elem` — fused safe-index-with-fallback. The single
    /// idiomatic `arr[i] or default` otherwise compiles to `Index`
    /// (`Some(elem)`/`None` box: 2 heap allocs + a hash per element) followed
    /// by `Dup; <2×PushConst None header>; MatchEnum; JumpIfFalse; UnwrapEnum`
    /// only to immediately discard the box. This fetches the element directly:
    /// in-bounds pushes `elem` and falls through; out-of-bounds jumps to
    /// `operand` (the recovery body), never materializing the `Option`.
    IndexOrElse,
    ArrayLen,
    ArraySlice,
    ArrayConcat,
    /// `[e0, .., e_{k-1}, ..seq]` — prepend k stack elements onto `seq`
    /// (`Instruction.operand` = k). Sublinear front-cons; the producer half
    /// of the old O(n²).
    Prepend,
    /// `[seq, n] -> seq[n..]` — structure-shared tail. The consumer half of
    /// the old O(n²); replaces `ArraySlice` in `[h, ..rest]` patterns.
    Drop,
    /// `[seq, e] -> seq` with `e` pushed on the back. Totality for
    /// `[..spread, x]`; no stdlib use today.
    Append,
    GetField,

    // Tagged values (enums / custom types)
    MakeEnumPayload,
    MatchEnum,
    UnwrapEnum,

    // Closures
    MakeClosure,
    PushCapture,
    PushSelf,

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
    /// for multi-segment `<<>>` literals so an n-segment build copies each
    /// byte once into a single backing, instead of a `BinAppend` chain that
    /// re-copies the accumulated prefix per segment (O(n*B)) and discards
    /// n-1 intermediate boxes. The binary mirror of `StrConcatN`.
    BinConcatN,
    BinFromInt,
    BinReadInt,
    BinTake,
    BinReadUtf8,
    /// `[bin, at_bits, prefix] -> Bool` — whether `bin`'s logical bits starting
    /// at `at_bits` begin with `prefix`'s logical bits. Out-of-range is `false`,
    /// never an error. Emitted by `<<>>` pattern codegen for literal segments
    /// (string literals and coalesced integer-literal runs): one bounds check +
    /// one byte compare instead of per-byte read/compare ops.
    BinMatchPrefix,
    /// `[bin, at_bits, len_bits] -> Binary` — O(1) sub-view sharing the backing,
    /// no `Option` wrapper. Emitted by `<<>>` pattern codegen after its own
    /// bounds check has proven the range valid; `BinSlice` (checked, `Option`)
    /// remains the public builtin.
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

    // I/O operations (experimental)
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

    // Concurrency (experimental, al/experiments/scheduler)
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
    /// True when this op's `operand` is an absolute instruction index that
    /// jump-patching / peephole must remap. Keep this the ONE authority —
    /// `emit_jump` debug-asserts against it so a new jump op forgotten here
    /// trips in debug rather than silently miscompiling under fusion.
    pub const fn has_jump_target(self) -> bool {
        matches!(
            self,
            Op::Jump
                | Op::JumpIfFalse
                | Op::JumpIfTrue
                | Op::JumpGeIntLC
                | Op::JumpNeIntLC
                | Op::IndexOrElse
        )
    }
}

/// Resolve an `@vm(name)` intrinsic key to its VM opcode. This is the ONE
/// place the string→Op mapping lives; analysis calls it while registering the
/// stdlib so an unknown key is a well-located compile error at the annotation
/// rather than an `Internal:` fallthrough during codegen.
pub fn builtin_op(name: &str) -> Option<Op> {
    Some(match name {
        "println" => Op::Print,
        "string__inspect" => Op::ToString,
        "internal__stack_depth" => Op::StackDepth,
        "io__read_file" => Op::FileRead,
        "io__write_file" => Op::FileWrite,
        "net__listen" => Op::TcpListen,
        "net__accept" => Op::TcpAccept,
        "net__connect" => Op::TcpConnect,
        "net__close" => Op::TcpCloseServer,
        "net__local_addr" => Op::TcpLocalAddr,
        "net__resolve" => Op::DnsResolve,
        "address__parse" => Op::IpParse,
        "socket__read" => Op::TcpRead,
        "socket__read_until" => Op::TcpReadUntil,
        "socket__write" => Op::TcpWrite,
        "socket__write_parts" => Op::TcpWriteParts,
        "socket__close" => Op::TcpClose,
        "string__split" => Op::StrSplit,
        "string__length" => Op::StrLen,
        "string__contains" => Op::StrContains,
        "string__trim" => Op::StrTrim,
        "int__to_string" => Op::IntToString,
        "binary__from_string" => Op::BinFromString,
        "binary__to_string" => Op::BinToString,
        "binary__bit_size" => Op::BinBitSize,
        "binary__byte_size" => Op::BinByteSize,
        "binary__slice" => Op::BinSlice,
        "binary__append" => Op::BinAppend,
        "binary__index_of" => Op::BinIndexOf,
        "binary__byte_at" => Op::BinByteAt,
        "binary__parse_int" => Op::BinParseInt,
        "binary__eq_ignore_ascii_case" => Op::BinEqIgnoreAsciiCase,
        "binary__to_ascii_lower" => Op::BinToAsciiLower,
        "binary__from_int_ascii" => Op::BinFromIntAscii,
        "http__parse_head" => Op::HttpParseHead,
        "http__framing" => Op::HttpFraming,
        "http__chunk_decode" => Op::HttpChunkDecode,
        "http__header_get" => Op::HttpHeaderGet,
        "http__header_has" => Op::HttpHeaderHas,
        "http__serialize_head" => Op::HttpSerializeHead,
        "float__floor" => Op::FloatFloor,
        "float__ceil" => Op::FloatCeil,
        "float__round" => Op::FloatRound,
        "float__truncate" => Op::FloatTruncate,
        "float__from_int" => Op::FloatFromInt,
        "float__to_string" => Op::FloatToString,
        "scheduler__spawn" => Op::ProcessSpawn,
        "scheduler__spawn_local" => Op::SpawnLocal,
        "scheduler__spawn_on_each" => Op::SpawnOnEach,
        "scheduler__sleep" => Op::Sleep,
        "time__monotonic" => Op::Monotonic,
        "process__argv" => Op::Argv,
        "process__env" => Op::EnvMap,
        "map__get" => Op::MapGet,
        "map__has" => Op::MapHas,
        "map__keys" => Op::MapKeys,
        "map__values" => Op::MapValues,
        "map__size" => Op::MapSize,
        "map__new" => Op::MapNew,
        "map__set" => Op::MapSet,
        "map__delete" => Op::MapDelete,
        "map__to_list" => Op::MapToList,
        _ => return None,
    })
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
