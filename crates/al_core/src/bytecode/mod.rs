//! The compiler side of the bytecode boundary: everything that produces a
//! runnable [`Program`] from AL source. The ISA, [`Program`], the NaN-boxed
//! `value` and the heap live in `al_vm` and are re-exported here, so
//! `al_core::bytecode::*` is the one import for both halves of the contract.

mod analysis;
pub mod binop;
pub mod compiler;
mod peephole;
mod prelude;
pub mod prelude_bindings;
mod session;

pub use al_vm::bytecode::*;
pub use binop::{BinopKind, ShortCircuitOp, ValueBinop, specialize_binop};
pub use compiler::*;
pub use prelude_bindings::{CtorRef, PreludeBindings, TypeRef};
pub use session::{HoverFact, IncrementalSession, Watermark};

/// Resolve an `@vm(name)` intrinsic key to its VM opcode. The only
/// string→Op mapping; analysis calls it while registering the stdlib so an
/// unknown key errors at the annotation, not during codegen.
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
        "array__length" => Op::ArrayLen,
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
        "http__headers_valid" => Op::HttpHeadersValid,
        "http__serialize_head" => Op::HttpSerializeHead,
        "float__floor" => Op::FloatFloor,
        "float__ceil" => Op::FloatCeil,
        "float__round" => Op::FloatRound,
        "float__truncate" => Op::FloatTruncate,
        "float__from_int" => Op::FloatFromInt,
        "float__to_string" => Op::FloatToString,
        "scheduler__spawn" => Op::ProcessSpawn,
        "scheduler__spawn_local" => Op::SpawnLocal,
        "scheduler__spawn_per_core" => Op::SpawnOnEach,
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
