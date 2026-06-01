mod analysis;
pub mod compiler;
mod prelude;
pub mod prelude_bindings;
pub mod transfer;
mod value;
use std::rc::Rc;

pub use compiler::*;
pub use prelude_bindings::{CtorRef, PreludeBindings, TypeRef};
pub use value::{
    BinaryValue, ClosureValue, EnumValue, HeapValue, Seq, SocketValue, TupleValue, Value,
    ValueView, enum_hash_with_payload, enum_name_prefix_hash, hash_value,
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
    /// no `Result` wrapper. Emitted by `<<>>` pattern codegen after its own
    /// bounds check has proven the range valid; `BinSlice` (checked, `Result`)
    /// remains the public builtin.
    BinView,
    /// `[haystack, needle, from] -> Option(Int)` — byte-substring search.
    BinIndexOf,
    /// `[bin, i] -> Int` — byte at index `i`, or -1 when out of range.
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
    // and value assembly run in native code — the Erlang
    // `erlang:decode_packet(http_bin, ...)` precedent — while every protocol
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
    /// `[sock, max, deadline_ms] -> Result(Binary, String)`. Parks until data
    /// arrives, the peer closes, or the deadline passes (then `Err`).
    TcpReadUntil,
    TcpWrite,
    /// Vectored write: `[sock, Array(Binary)] -> Result(Nil, String)` in one
    /// writev syscall.
    TcpWriteParts,
    TcpClose,
    TcpCloseServer,
    TcpLocalAddr,

    // Concurrency (experimental, al/experiments/scheduler)
    /// Spawn a lightweight process running the popped closure.
    ProcessSpawn,
    /// Park the current process for `ms` milliseconds.
    Sleep,
    /// Push milliseconds elapsed since a process-global monotonic epoch (Int).
    Monotonic,
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
/// # Safety
/// `i` must be in-bounds for `code`.
#[inline(always)]
pub unsafe fn fetch(code: &[Instruction], i: usize) -> Instruction {
    debug_assert!(i < code.len());
    // SAFETY: `repr(C, align(8))` + `repr(u8)` on `Op` + the size/align asserts
    // pin `Instruction` to a fully-initialized 8-byte POD, so reading the slice
    // as `[u64]` is sound and transmuting one element back recovers the same
    // `Instruction` (the `Op` byte is valid because it came from `code`). The
    // u64-then-transmute path is the point — it forces one `ldr` where SROA on
    // a direct struct copy would otherwise emit four separate field loads.
    unsafe {
        let raw = *code.as_ptr().cast::<u64>().add(i);
        std::mem::transmute::<u64, Instruction>(raw)
    }
}

#[derive(Debug, Clone)]
pub struct Function {
    pub name: Rc<str>,
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
}

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
