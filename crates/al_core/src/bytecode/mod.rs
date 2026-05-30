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
