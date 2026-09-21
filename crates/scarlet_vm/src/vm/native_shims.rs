//! Runtime shims compiled code calls, matching the interpreter's semantics.
//!
//! A proven-`Int` value lives as a raw `i64` in registers, so only the boxing
//! seams need help. Scarlet Ints are 47-bit NaN-boxed immediates: a result outside
//! `[-2^47, 2^47)` spills to an arena `BigInt`, and an Int-typed frame slot may
//! already hold one. Compiled code inlines the fast paths and branches here on
//! the cold side.
//!
//! Div and mod are total like the interpreter (`x / 0 == 0`, `x % 0 == x`,
//! `MIN / -1` wraps), where a bare `sdiv`/`srem` would trap.
//!
//! The `al_shim_*_int_val` shims are the one-call cold path over value bits:
//! decode, release the operand references the caller transfers, apply, re-box.
//!
//! Generated code finds these by symbol name; the JIT registers every
//! [`shim_symbols`] pair, so the CLIF emitter names them without depending on
//! this crate.

// Generated code cannot pass `&mut VM` or `Value`, so this boundary works in
// raw pointers and raw NaN-box bits.
#![allow(unsafe_code)]

use crate::bytecode::value::{ReuseAddr, proof_violation, range_len};
use crate::bytecode::{NativeStatus, Op, Value, ValueView, seq};

use super::mailbox::Delivery;
use super::poll::{Parked, Resume};
use super::processes::Link;
use super::{Step, VM, VmResult};

/// Escape raw bits from an owned value without running its destructor. The
/// reference it carries transfers to the returned bits.
#[inline]
fn into_bits(v: Value) -> u64 {
    std::mem::ManuallyDrop::new(v).to_bits()
}

/// Truncating division, total like `Op::DivInt`: `x / 0 == 0`, `MIN / -1`
/// wraps instead of trapping.
#[inline]
fn div_int(a: i64, b: i64) -> i64 {
    if b == 0 { 0 } else { a.wrapping_div(b) }
}

/// Remainder, total like `Op::ModInt`: `x % 0 == x`, `MIN % -1 == 0`. The
/// result takes the dividend's sign.
#[inline]
fn mod_int(a: i64, b: i64) -> i64 {
    if b == 0 { a } else { a.wrapping_rem(b) }
}

/// Box a full-range `i64` as an Int value, spilling past the 47-bit immediate
/// range to an arena `BigInt`. The returned bits carry one owned reference the
/// caller must store or release exactly once.
///
/// # Safety
/// `vmx` must point at the running scheduler's live `VM`.
#[cold]
pub(crate) unsafe extern "C" fn al_shim_int_box(vmx: *mut VM, i: i64) -> u64 {
    // SAFETY: `vmx` is the running scheduler's VM per the contract above.
    let vm = unsafe { &mut *vmx };
    into_bits(vm.boxed_int(i))
}

/// `Op::MakeEnumPayload`'s allocation path behind the C ABI. Must mirror
/// `VM::make_enum_payload` bit for bit: `packed` is the emitter's
/// `type_id | variant_idx << 32` word, the name and label bits are frozen
/// immortal constants stored verbatim, and the hash word is written `0`
/// because 0 means "not yet hashed" to `EnumRef::hash`.
///
/// `payload` points at `len` value words, each carrying one owned reference
/// the caller transfers. The cell takes its own reference per field and this
/// shim releases the transferred ones.
///
/// # Safety
/// `vmx` must point at the running scheduler's live `VM`; `enum_name` and
/// `variant_name` must be the bits of live frozen `Str` values, `labels` of a
/// live frozen `Tuple` of `Str`s (all immortal, outliving the program);
/// `payload` must point at `len` initialized value words whose references
/// the caller owns and transfers to this call.
#[cold]
pub(crate) unsafe extern "C" fn al_shim_enum_alloc(
    vmx: *mut VM,
    packed: u64,
    enum_name: u64,
    variant_name: u64,
    labels: u64,
    payload: *const u64,
    len: i64,
) -> u64 {
    let (type_id, variant_idx) = crate::bytecode::value::unpack_variant(packed as i64);
    let n = len as usize;
    // SAFETY: immortal frozen constants per the contract above. Treating the
    // bits as owned is sound because immortal clone/drop never touches memory.
    let (en, vn, lb) = unsafe {
        (
            Value::from_bits(enum_name),
            Value::from_bits(variant_name),
            Value::from_bits(labels),
        )
    };
    debug_assert!(en.is_immortal() && vn.is_immortal() && lb.is_immortal());
    // SAFETY: `payload` points at `n` initialized value words. Borrowed: the
    // cell takes its own reference.
    let fields = unsafe { Value::slice_from_words(payload, n) };
    // SAFETY: `vmx` is the running scheduler's VM per the contract above.
    let vm = unsafe { &mut *vmx };
    let v = Value::enum_reuse_in(
        &mut vm.heap,
        ReuseAddr::none(),
        type_id,
        variant_idx,
        0,
        en,
        vn,
        lb,
        fields,
    );
    for i in 0..n {
        // SAFETY: each word carries one owned reference, released once here.
        drop(unsafe { Value::from_bits(payload.add(i).read()) });
    }
    into_bits(v)
}

/// `Op::MakeArray` behind the C ABI. `elems` points at `len` value words, each
/// carrying one owned reference the caller transfers; the array takes its own
/// reference per element and this shim releases the transferred ones.
///
/// # Safety
/// `vmx` must point at the running scheduler's live `VM`; `elems` must point
/// at `len` initialized value words whose references the caller owns and
/// transfers to this call.
#[cold]
pub(crate) unsafe extern "C" fn al_shim_make_array(
    vmx: *mut VM,
    elems: *const u64,
    len: i64,
) -> u64 {
    let n = len as usize;
    // SAFETY: `elems` points at `n` initialized value words. Borrowed: the
    // array takes its own reference.
    let vals = unsafe { Value::slice_from_words(elems, n) };
    // SAFETY: `vmx` is the running scheduler's VM per the contract above.
    let vm = unsafe { &mut *vmx };
    let v = Value::array_in(&mut vm.heap, vals);
    for i in 0..n {
        // SAFETY: each word carries one owned reference, released once here.
        drop(unsafe { Value::from_bits(elems.add(i).read()) });
    }
    into_bits(v)
}

/// `Op::MakeTuple` behind the C ABI: [`al_shim_make_array`] over `tuple_in`.
///
/// # Safety
/// Same contract as [`al_shim_make_array`].
#[cold]
pub(crate) unsafe extern "C" fn al_shim_make_tuple(
    vmx: *mut VM,
    elems: *const u64,
    len: i64,
) -> u64 {
    let n = len as usize;
    // SAFETY: see `al_shim_make_array`; identical contract.
    let vals = unsafe { Value::slice_from_words(elems, n) };
    // SAFETY: `vmx` is the running scheduler's VM per the contract above.
    let vm = unsafe { &mut *vmx };
    let v = Value::tuple_in(&mut vm.heap, vals);
    for i in 0..n {
        // SAFETY: each word carries one owned reference, released once here.
        drop(unsafe { Value::from_bits(elems.add(i).read()) });
    }
    into_bits(v)
}

/// `VM::seq_root`, made total. The coverage gate only admits these ops on a
/// proven `Array`-typed operand, so the interpreter's error arm is
/// unreachable; a stray value passes through untouched.
fn seq_root_total(vm: &mut VM, v: Value) -> Value {
    match v.kind() {
        ValueView::Array(_) => v,
        ValueView::Range(s, e) => seq::from_int_range(&mut vm.heap, s, e),
        _ => crate::bytecode::value::proof_violation("seq op on a non-sequence value"),
    }
}

/// `Op::ArrayLen` behind the C ABI, on a proven `Array`-typed word. The result
/// is boxed because a lazy range's length can exceed the small-int payload.
/// The transferred reference is released.
///
/// # Safety
/// `vmx` must point at the running scheduler's live `VM`; `seq` must carry
/// one owned reference the caller transfers.
pub(crate) unsafe extern "C" fn al_shim_seq_len(vmx: *mut VM, seq: u64) -> u64 {
    // SAFETY: owned word per the contract above.
    let v = unsafe { Value::from_bits(seq) };
    let n = match v.kind() {
        ValueView::Array(a) => a.len() as i64,
        ValueView::Range(s, e) => range_len(s, e),
        ValueView::Tuple(t) => t.len() as i64,
        _ => crate::bytecode::value::proof_violation("ArrayLen on a non-sequence value"),
    };
    drop(v);
    // SAFETY: `vmx` is the running scheduler's VM per the contract above.
    let vm = unsafe { &mut *vmx };
    into_bits(vm.boxed_int(n))
}

/// `Op::BinByteSize` behind the C ABI, on a proven `Binary`-typed word. The
/// transferred reference is released.
///
/// # Safety
/// `vmx` must point at the running scheduler's live `VM`; `bin` must carry
/// one owned reference the caller transfers.
pub(crate) unsafe extern "C" fn al_shim_bin_byte_size(vmx: *mut VM, bin: u64) -> u64 {
    // SAFETY: owned word per the contract above.
    let v = unsafe { Value::from_bits(bin) };
    let n = match v.kind() {
        ValueView::Binary(b) => b.bit_len().div_ceil(8) as i64,
        _ => crate::bytecode::value::proof_violation("BinByteSize on a non-binary value"),
    };
    drop(v);
    // SAFETY: `vmx` is the running scheduler's VM per the contract above.
    let vm = unsafe { &mut *vmx };
    into_bits(vm.boxed_int(n))
}

/// `Op::Append` behind the C ABI. `buf[0]` is the sequence and `buf[1..len]`
/// the pushed elements, matching the interpreter's operand order. Every
/// transferred reference is released.
///
/// # Safety
/// `vmx` must point at the running scheduler's live `VM`; `buf` must point
/// at `len >= 1` initialized value words whose references the caller owns
/// and transfers to this call.
pub(crate) unsafe extern "C" fn al_shim_seq_append(vmx: *mut VM, buf: *const u64, len: i64) -> u64 {
    let n = len as usize;
    // SAFETY: `vmx` is the running scheduler's VM per the contract above.
    let vm = unsafe { &mut *vmx };
    // Each word carries one owned reference; taking it as a move (rather
    // than clone-then-release) keeps a sole reference unique, so the pushes
    // can edit the tree in place.
    // SAFETY: `buf` holds `n` initialized owned words, each read exactly once.
    let seq_word = unsafe { Value::from_bits(buf.read()) };
    let mut root = seq_root_total(vm, seq_word);
    for i in 1..n {
        // SAFETY: as above; word `i` is read exactly once.
        let e = unsafe { Value::from_bits(buf.add(i).read()) };
        root = seq::push_back(&mut vm.heap, root, e);
    }
    into_bits(root)
}

/// `Op::Prepend` behind the C ABI. `buf[..len-1]` are the elements in source
/// order and `buf[len-1]` the sequence, matching the interpreter's operand
/// order. Pushes front in reverse so the result is `[e0, .., ek-1, ..seq]`.
///
/// # Safety
/// Same contract as [`al_shim_seq_append`].
pub(crate) unsafe extern "C" fn al_shim_seq_prepend(
    vmx: *mut VM,
    buf: *const u64,
    len: i64,
) -> u64 {
    let n = len as usize;
    // SAFETY: `vmx` is the running scheduler's VM per the contract above.
    let vm = unsafe { &mut *vmx };
    // Moves, not clone-then-release: see `al_shim_seq_append`.
    // SAFETY: `buf` holds `n` initialized owned words, each read exactly once.
    let seq_word = unsafe { Value::from_bits(buf.add(n - 1).read()) };
    let mut root = seq_root_total(vm, seq_word);
    for i in (0..n - 1).rev() {
        // SAFETY: as above; word `i` is read exactly once.
        let e = unsafe { Value::from_bits(buf.add(i).read()) };
        root = seq::push_front(&mut vm.heap, root, e);
    }
    into_bits(root)
}

/// `Op::HttpParseHead` behind the C ABI: parse a request head out of a proven
/// `Binary`-typed buffer at `off`. Infallible here, since malformed input comes
/// back as an Scarlet enum value. The transferred buffer reference is released.
///
/// # Safety
/// `vmx` must point at the running scheduler's live `VM`; `buf` must carry
/// one owned reference the caller transfers.
pub(crate) unsafe extern "C" fn al_shim_http_parse_head(vmx: *mut VM, buf: u64, off: i64) -> u64 {
    // SAFETY: owned word per the contract above.
    let v = unsafe { Value::from_bits(buf) };
    // SAFETY: `vmx` is the running scheduler's VM per the contract above.
    let vm = unsafe { &mut *vmx };
    let r = match vm.templates.h1() {
        Ok(t) => super::http::parse_head(t, &mut vm.heap, &super::bin_ref(&v), off),
        // Unreachable: compiled code exists only for programs that bound the
        // H1 slots (validated at load).
        Err(_) => crate::bytecode::value::proof_violation("HttpParseHead with unbound H1 slots"),
    };
    drop(v);
    into_bits(r)
}

/// `Op::HttpHeadersValid` behind the C ABI, on a proven `Array(Header)`-typed
/// word. The shape error is unreachable under the proof; release answers
/// `false`.
///
/// # Safety
/// `headers` must carry one owned reference the caller transfers.
pub(crate) unsafe extern "C" fn al_shim_http_headers_valid(headers: u64) -> u64 {
    // SAFETY: owned word per the contract above.
    let v = unsafe { Value::from_bits(headers) };
    let r = super::http::headers_valid(&v).unwrap_or_else(|_| {
        crate::bytecode::value::proof_violation("HttpHeadersValid on a non-header array")
    });
    drop(v);
    into_bits(r)
}

/// `Op::HttpHeaderHas` behind the C ABI, on proven `Array(Header)` + `Binary`
/// words. Both transferred references are released.
///
/// # Safety
/// `headers` and `name` must each carry one owned reference the caller
/// transfers.
pub(crate) unsafe extern "C" fn al_shim_http_header_has(headers: u64, name: u64) -> u64 {
    // SAFETY: owned words per the contract above.
    let (h, n) = unsafe { (Value::from_bits(headers), Value::from_bits(name)) };
    let r = super::http::header_has(&h, &super::bin_ref(&n)).unwrap_or_else(|_| {
        crate::bytecode::value::proof_violation("HttpHeaderHas on a non-header array")
    });
    drop(h);
    drop(n);
    into_bits(r)
}

/// `Op::HttpSerializeHead` behind the C ABI: one fresh binary over the
/// serialized head. `code` is passed unboxed. The shape error is unreachable
/// under the proofs; release answers nil.
///
/// # Safety
/// `vmx` must point at the running scheduler's live `VM`; `reason` and
/// `headers` must each carry one owned reference the caller transfers.
pub(crate) unsafe extern "C" fn al_shim_http_serialize_head(
    vmx: *mut VM,
    code: i64,
    reason: u64,
    headers: u64,
) -> u64 {
    // SAFETY: owned words per the contract above.
    let (rv, hv) = unsafe { (Value::from_bits(reason), Value::from_bits(headers)) };
    // SAFETY: `vmx` is the running scheduler's VM per the contract above.
    let vm = unsafe { &mut *vmx };
    let r = super::http::serialize_head(&mut vm.heap, code, &super::bin_ref(&rv), &hv)
        .unwrap_or_else(|_| {
            crate::bytecode::value::proof_violation("HttpSerializeHead on a non-header array")
        });
    drop(rv);
    drop(hv);
    into_bits(r)
}

/// `Op::HttpFraming` behind the C ABI, on a proven `Array(Header)`-typed word:
/// classify the body framing. The shape error is unreachable under the proof;
/// release answers nil.
///
/// # Safety
/// `vmx` must point at the running scheduler's live `VM`; `headers` must
/// carry one owned reference the caller transfers.
pub(crate) unsafe extern "C" fn al_shim_http_framing(vmx: *mut VM, headers: u64) -> u64 {
    // SAFETY: owned word per the contract above.
    let v = unsafe { Value::from_bits(headers) };
    // SAFETY: `vmx` is the running scheduler's VM per the contract above.
    let vm = unsafe { &mut *vmx };
    let r = vm
        .templates
        .h1()
        .and_then(|t| super::http::framing(t, &mut vm.heap, &v))
        .unwrap_or_else(|_| {
            crate::bytecode::value::proof_violation("HttpFraming on a non-header array")
        });
    drop(v);
    into_bits(r)
}

/// [`div_int`] on unboxed `i64`s.
pub(crate) extern "C" fn al_shim_div_int(a: i64, b: i64) -> i64 {
    div_int(a, b)
}

/// [`mod_int`] on unboxed `i64`s.
pub(crate) extern "C" fn al_shim_mod_int(a: i64, b: i64) -> i64 {
    mod_int(a, b)
}

/// `Op::PushGlobal`: the entry frame's slot `slot`, borrowed. Globals are
/// written once before any body reads them and never reassigned, so the word
/// stays live for the program; the caller retains only where it keeps one. A
/// shim rather than an inline load because the array is a `Vec` with no
/// ABI-stable base.
///
/// # Safety
/// `vmx` must point at the running scheduler's live `VM`, and `slot` must be
/// in range for its global area; the emitter guarantees both. The returned
/// word carries no reference the caller owns.
pub(crate) unsafe extern "C" fn al_shim_push_global(vmx: *mut VM, slot: i64) -> u64 {
    // SAFETY: `vmx` is the running scheduler's VM per the contract above.
    let vm = unsafe { &mut *vmx };
    vm.globals[slot as usize].to_bits()
}

/// The generic bridge for [`is_native_bridge_op`](crate::bytecode::is_native_bridge_op)
/// opcodes. `buf` holds `argc` owned operand words in interpreter push order
/// (`buf[0]` deepest, `buf[argc-1]` on top); each reference is transferred to
/// this call. The shim pushes them onto the value stack, runs the interpreter's
/// own op method (which pops them and pushes one result), and returns the owned
/// result bits paired with the caller's frame base.
///
/// The base rides back in the return pair because the operand pushes can grow
/// the value stack, and a growth moves it: any frame-base pointer the compiled
/// caller cached before this call is stale the moment the Vec reallocates.
/// Returning the fresh base costs nothing (it comes back in a register) and
/// lets the caller re-establish its view without a second runtime call.
///
/// The op methods' only `Err` arms are typed-pop mismatches the type checker
/// already excludes for well-typed bytecode, so a failure here is a proof
/// violation, not a recoverable error — no status word is needed.
///
/// # Safety
/// `vmx` must point at the running scheduler's live `VM`; `buf` must point at
/// `argc` initialized value words whose references the caller owns and
/// transfers; `op_code` must be `op as u8` for an op
/// [`is_native_bridge_op`](crate::bytecode::is_native_bridge_op) admits.
pub(crate) unsafe extern "C" fn al_shim_op(
    vmx: *mut VM,
    op_code: i64,
    operand: i64,
    buf: *const u64,
    argc: i64,
) -> super::native::ContEntry {
    // SAFETY: `vmx` is the running scheduler's VM per the contract above.
    let vm = unsafe { &mut *vmx };
    for i in 0..argc as usize {
        // SAFETY: `buf` holds `argc` initialized owned words; each transfers
        // its reference onto the value stack here, balanced by the op's pops.
        vm.stack
            .push(unsafe { Value::from_bits(buf.add(i).read()) });
    }
    // The ops that charge reductions take a budget; compiled code spends the
    // same counter the entry prologue and checkpoints use.
    let mut reds = vm.native_reds;
    let out = vm.run_bridge_op(op_code as u8, operand as i32, &mut reds);
    vm.native_reds = reds;
    let result = match out {
        Ok(()) => match vm.stack.pop() {
            Some(v) => into_bits(v),
            None => proof_violation("bridge op produced no result"),
        },
        // The type checker excludes every reachable error arm of these ops.
        Err(_) => proof_violation("bridge op hit a type-proof-excluded error"),
    };
    let base_slot = vm.frame().base_slot;
    super::native::ContEntry {
        base: vm.stack.as_mut_ptr().wrapping_add(base_slot).cast(),
        result,
    }
}

/// Each dispatched opcode's discriminant as a named constant, so the
/// dispatchers below can `match` on them directly — patterns, not guard
/// chains, which is what lets the compiler emit a jump table.
mod opc {
    use super::Op;

    pub const ADD: u8 = Op::Add as u8;
    pub const ADD_FLOAT: u8 = Op::AddFloat as u8;
    pub const ADD_STR: u8 = Op::AddStr as u8;
    pub const APPEND: u8 = Op::Append as u8;
    pub const ARGV: u8 = Op::Argv as u8;
    pub const ARRAY_CONCAT: u8 = Op::ArrayConcat as u8;
    pub const ARRAY_LEN: u8 = Op::ArrayLen as u8;
    pub const ARRAY_SLICE: u8 = Op::ArraySlice as u8;
    pub const BIN_APPEND: u8 = Op::BinAppend as u8;
    pub const BIN_BIT_SIZE: u8 = Op::BinBitSize as u8;
    pub const BIN_BYTE_AT: u8 = Op::BinByteAt as u8;
    pub const BIN_BYTE_SIZE: u8 = Op::BinByteSize as u8;
    pub const BIN_CONCAT_N: u8 = Op::BinConcatN as u8;
    pub const BIN_EQ_IGNORE_ASCII_CASE: u8 = Op::BinEqIgnoreAsciiCase as u8;
    pub const BIN_FROM_INT: u8 = Op::BinFromInt as u8;
    pub const BIN_FROM_INT_ASCII: u8 = Op::BinFromIntAscii as u8;
    pub const BIN_FROM_STRING: u8 = Op::BinFromString as u8;
    pub const BIN_INDEX_OF: u8 = Op::BinIndexOf as u8;
    pub const BIN_MATCH_PREFIX: u8 = Op::BinMatchPrefix as u8;
    pub const BIN_PARSE_INT: u8 = Op::BinParseInt as u8;
    pub const BIN_READ_INT: u8 = Op::BinReadInt as u8;
    pub const BIN_READ_UTF8: u8 = Op::BinReadUtf8 as u8;
    pub const BIN_SLICE: u8 = Op::BinSlice as u8;
    pub const BIN_TAKE: u8 = Op::BinTake as u8;
    pub const BIN_TO_ASCII_LOWER: u8 = Op::BinToAsciiLower as u8;
    pub const BIN_TO_STRING: u8 = Op::BinToString as u8;
    pub const BIN_VIEW: u8 = Op::BinView as u8;
    pub const BIT_AND: u8 = Op::BitAnd as u8;
    pub const BIT_NOT: u8 = Op::BitNot as u8;
    pub const BIT_OR: u8 = Op::BitOr as u8;
    pub const BIT_SHL: u8 = Op::BitShl as u8;
    pub const BIT_SHR: u8 = Op::BitShr as u8;
    pub const BIT_XOR: u8 = Op::BitXor as u8;
    pub const DIV: u8 = Op::Div as u8;
    pub const DIV_FLOAT: u8 = Op::DivFloat as u8;
    pub const DNS_RESOLVE: u8 = Op::DnsResolve as u8;
    pub const DNS_RESOLVE_UNTIL: u8 = Op::DnsResolveUntil as u8;
    pub const ELEM_AT: u8 = Op::ElemAt as u8;
    pub const ENV_MAP: u8 = Op::EnvMap as u8;
    pub const EQ: u8 = Op::Eq as u8;
    pub const FILE_READ: u8 = Op::FileRead as u8;
    pub const FILE_WRITE: u8 = Op::FileWrite as u8;
    pub const FLOAT_CEIL: u8 = Op::FloatCeil as u8;
    pub const FLOAT_FLOOR: u8 = Op::FloatFloor as u8;
    pub const FLOAT_FROM_INT: u8 = Op::FloatFromInt as u8;
    pub const FLOAT_ROUND: u8 = Op::FloatRound as u8;
    pub const FLOAT_TO_STRING: u8 = Op::FloatToString as u8;
    pub const FLOAT_TRUNCATE: u8 = Op::FloatTruncate as u8;
    pub const GET_FIELD: u8 = Op::GetField as u8;
    pub const GT: u8 = Op::Gt as u8;
    pub const GT_FLOAT: u8 = Op::GtFloat as u8;
    pub const GTE: u8 = Op::Gte as u8;
    pub const GTE_FLOAT: u8 = Op::GteFloat as u8;
    pub const HTTP_CHUNK_DECODE: u8 = Op::HttpChunkDecode as u8;
    pub const HTTP_FRAMING: u8 = Op::HttpFraming as u8;
    pub const HTTP_HEADER_GET: u8 = Op::HttpHeaderGet as u8;
    pub const HTTP_HEADER_HAS: u8 = Op::HttpHeaderHas as u8;
    pub const HTTP_HEADERS_VALID: u8 = Op::HttpHeadersValid as u8;
    pub const HTTP_PARSE_HEAD: u8 = Op::HttpParseHead as u8;
    pub const HTTP_PARSE_RESPONSE_HEAD: u8 = Op::HttpParseResponseHead as u8;
    pub const HTTP_SERIALIZE_HEAD: u8 = Op::HttpSerializeHead as u8;
    pub const INDEX: u8 = Op::Index as u8;
    pub const INDEX_OR: u8 = Op::IndexOr as u8;
    pub const INT_FROM_STRING: u8 = Op::IntFromString as u8;
    pub const INT_TO_STRING: u8 = Op::IntToString as u8;
    pub const IP_PARSE: u8 = Op::IpParse as u8;
    pub const LT: u8 = Op::Lt as u8;
    pub const LT_FLOAT: u8 = Op::LtFloat as u8;
    pub const LTE: u8 = Op::Lte as u8;
    pub const LTE_FLOAT: u8 = Op::LteFloat as u8;
    pub const MAKE_RANGE: u8 = Op::MakeRange as u8;
    pub const MAP_DELETE: u8 = Op::MapDelete as u8;
    pub const MAP_GET: u8 = Op::MapGet as u8;
    pub const MAP_HAS: u8 = Op::MapHas as u8;
    pub const MAP_KEYS: u8 = Op::MapKeys as u8;
    pub const MAP_NEW: u8 = Op::MapNew as u8;
    pub const MAP_SET: u8 = Op::MapSet as u8;
    pub const MAP_SIZE: u8 = Op::MapSize as u8;
    pub const MAP_TO_LIST: u8 = Op::MapToList as u8;
    pub const JSON_PARSE: u8 = Op::JsonParse as u8;
    pub const JSON_KIND: u8 = Op::JsonKind as u8;
    pub const JSON_LEN: u8 = Op::JsonLen as u8;
    pub const JSON_FIELD: u8 = Op::JsonField as u8;
    pub const JSON_INDEX: u8 = Op::JsonIndex as u8;
    pub const JSON_ENTRIES: u8 = Op::JsonEntries as u8;
    pub const JSON_ELEMENTS: u8 = Op::JsonElements as u8;
    pub const JSON_STRING: u8 = Op::JsonString as u8;
    pub const JSON_INT: u8 = Op::JsonInt as u8;
    pub const JSON_INT_TEXT: u8 = Op::JsonIntText as u8;
    pub const JSON_FLOAT: u8 = Op::JsonFloat as u8;
    pub const JSON_BOOL: u8 = Op::JsonBool as u8;
    pub const JSON_ENCODE: u8 = Op::JsonEncode as u8;
    pub const MAP_VALUES: u8 = Op::MapValues as u8;
    pub const MOD: u8 = Op::Mod as u8;
    pub const MONOTONIC: u8 = Op::Monotonic as u8;
    pub const WALL_CLOCK: u8 = Op::WallClock as u8;
    pub const RANDOM_BYTES: u8 = Op::RandomBytes as u8;
    pub const MUL: u8 = Op::Mul as u8;
    pub const MUL_FLOAT: u8 = Op::MulFloat as u8;
    pub const NEG: u8 = Op::Neg as u8;
    pub const NEG_FLOAT: u8 = Op::NegFloat as u8;
    pub const NEQ: u8 = Op::Neq as u8;
    pub const PORT_CLOSE: u8 = Op::PortClose as u8;
    pub const PORT_SPAWN: u8 = Op::PortSpawn as u8;
    pub const PREPEND: u8 = Op::Prepend as u8;
    pub const PRINT: u8 = Op::Print as u8;
    pub const PROCESS_DEMONITOR: u8 = Op::ProcessDemonitor as u8;
    pub const PROCESS_KILL: u8 = Op::ProcessKill as u8;
    pub const PROCESS_MONITOR: u8 = Op::ProcessMonitor as u8;
    pub const SUPERVISOR_NEW: u8 = Op::SupervisorNew as u8;
    pub const SUPERVISOR_WORKER: u8 = Op::SupervisorWorker as u8;
    pub const FACTORY_NEW: u8 = Op::FactoryNew as u8;
    pub const FACTORY_LOOKUP_OR_START: u8 = Op::FactoryLookupOrStart as u8;
    pub const FACTORY_LOOKUP: u8 = Op::FactoryLookup as u8;
    pub const SUPERVISED_OF: u8 = Op::SupervisedOf as u8;
    pub const SUPERVISED_PARENT: u8 = Op::SupervisedParent as u8;
    pub const SUPERVISED_CHILDREN: u8 = Op::SupervisedChildren as u8;
    pub const SUPERVISED_COUNT: u8 = Op::SupervisedCount as u8;
    pub const SUPERVISED_INFO: u8 = Op::SupervisedInfo as u8;
    pub const WATCH_NEW: u8 = Op::WatchNew as u8;
    pub const WATCH_CANCEL: u8 = Op::WatchCancel as u8;
    pub const PROCESS_SELF: u8 = Op::ProcessSelf as u8;
    pub const PROCESS_SPAWN: u8 = Op::ProcessSpawn as u8;
    pub const PROCESS_SPAWN_UNLINKED: u8 = Op::ProcessSpawnUnlinked as u8;
    pub const SEQ_DROP: u8 = Op::SeqDrop as u8;
    pub const SLEEP: u8 = Op::Sleep as u8;
    pub const SUBJECT_NEW: u8 = Op::SubjectNew as u8;
    pub const SUBJECT_RECEIVE: u8 = Op::SubjectReceive as u8;
    pub const SUBJECT_RECEIVE_UNTIL: u8 = Op::SubjectReceiveUntil as u8;
    pub const SUBJECT_SEND: u8 = Op::SubjectSend as u8;
    pub const SUBJECT_SEND_URGENT: u8 = Op::SubjectSendUrgent as u8;
    pub const SUPERVISOR_WORKER_ON_EACH: u8 = Op::SupervisorWorkerOnEach as u8;
    pub const FACTORY_SPAWN: u8 = Op::FactorySpawn as u8;
    pub const STACK_DEPTH: u8 = Op::StackDepth as u8;
    pub const LIVE_SUBJECTS: u8 = Op::LiveSubjects as u8;
    pub const BLOCKING_THREADS: u8 = Op::BlockingThreads as u8;
    pub const STR_CONCAT_N: u8 = Op::StrConcatN as u8;
    pub const STR_CONTAINS: u8 = Op::StrContains as u8;
    pub const STR_LEN: u8 = Op::StrLen as u8;
    pub const STR_SPLIT: u8 = Op::StrSplit as u8;
    pub const STR_TO_GRAPHEMES: u8 = Op::StrToGraphemes as u8;
    pub const STR_TRIM: u8 = Op::StrTrim as u8;
    pub const SUB: u8 = Op::Sub as u8;
    pub const SUB_FLOAT: u8 = Op::SubFloat as u8;
    pub const TCP_ACCEPT: u8 = Op::TcpAccept as u8;
    pub const TCP_CLOSE: u8 = Op::TcpClose as u8;
    pub const TCP_CLOSE_SERVER: u8 = Op::TcpCloseServer as u8;
    pub const TCP_CONNECT: u8 = Op::TcpConnect as u8;
    pub const TCP_CONNECT_UNTIL: u8 = Op::TcpConnectUntil as u8;
    pub const TCP_GIVE: u8 = Op::TcpGive as u8;
    pub const TCP_LISTEN: u8 = Op::TcpListen as u8;
    pub const TCP_LOCAL_ADDR: u8 = Op::TcpLocalAddr as u8;
    pub const TCP_READ: u8 = Op::TcpRead as u8;
    pub const TCP_READ_UNTIL: u8 = Op::TcpReadUntil as u8;
    pub const TCP_WRITE: u8 = Op::TcpWrite as u8;
    pub const TCP_WRITE_PARTS: u8 = Op::TcpWriteParts as u8;
    pub const TLS_CLOSE: u8 = Op::TlsClose as u8;
    pub const TLS_HANDSHAKE: u8 = Op::TlsHandshake as u8;
    pub const TLS_HANDSHAKE_UNTIL: u8 = Op::TlsHandshakeUntil as u8;
    pub const TLS_READ: u8 = Op::TlsRead as u8;
    pub const TLS_READ_UNTIL: u8 = Op::TlsReadUntil as u8;
    pub const TLS_WRITE: u8 = Op::TlsWrite as u8;
    pub const TO_STRING: u8 = Op::ToString as u8;
    pub const TUPLE_INDEX: u8 = Op::TupleIndex as u8;
    pub const WIRE_DECODE: u8 = Op::WireDecode as u8;
    pub const WIRE_ENCODE: u8 = Op::WireEncode as u8;
}

/// The bridge for [`is_native_try_op`](crate::bytecode::is_native_try_op)
/// opcodes: the ops with a reachable runtime error.
///
/// Like [`al_shim_op`], but returns a [`NativeStatus`] rather than the value,
/// because the op can fail on user data. On `Done` the result is on the value
/// stack for the caller to pop; otherwise the error is parked in the VM and
/// the status unwinds the native frames.
///
/// # Safety
/// As [`al_shim_op`], with `op_code` one
/// [`is_native_try_op`](crate::bytecode::is_native_try_op) admits.
pub(crate) unsafe extern "C" fn al_shim_try_op(
    vmx: *mut VM,
    op_code: i64,
    operand: i64,
    buf: *const u64,
    argc: i64,
) -> u64 {
    // SAFETY: `vmx` is the running scheduler's VM per the contract above.
    let vm = unsafe { &mut *vmx };
    for i in 0..argc as usize {
        // SAFETY: `buf` holds `argc` initialized owned words, transferred here.
        vm.stack
            .push(unsafe { Value::from_bits(buf.add(i).read()) });
    }
    match vm.run_try_op(op_code as u8, operand as i32) {
        Ok(()) => NativeStatus::Done as u64,
        Err(e) => vm.status_from_outcome(Err(e)) as u64,
    }
}

impl VM {
    /// Dispatch an [`is_native_try_op`](crate::bytecode::is_native_try_op)
    /// opcode to its interpreter method.
    ///
    /// `_operand` is kept because `al_shim_try_op` is handed one by the C ABI
    /// and every sibling dispatcher takes the same pair; `ArraySlice` is the
    /// only `Try` op left since the wire ops moved to `Bridge` (T-466), and it
    /// takes none. Give it a name again when a `Try` op needs it.
    fn run_try_op(&mut self, op_code: u8, _operand: i32) -> VmResult<()> {
        match op_code {
            opc::ARRAY_SLICE => self.seq_slice(),
            _ => proof_violation("run_try_op on an op is_native_try_op excludes"),
        }
    }
}

/// The bridge for [`is_native_park_op`](crate::bytecode::is_native_park_op)
/// opcodes: the ops that can suspend the process.
///
/// Runs like [`al_shim_op`], but reports back a [`NativeStatus`] instead of a
/// value, because the op may not finish. On completion the op has left its one
/// result on the value stack and the caller pops it. On a park, the op's
/// [`Resume`] picks which of the caller's two resume ordinals the frame
/// re-enters at.
///
/// The value stack is left exactly as the op arranged it, which is what makes
/// the retry protocol work: a retrying op pushes its operands back, and the
/// caller's retry attempt passes `argc == 0` so this call consumes those same
/// words instead of re-supplying them. Compiled code could not re-supply them
/// anyway — a parking op's operands are often slotless temps with no home to
/// reload from once the machine frame is gone.
///
/// # Safety
/// `vmx` must point at the running scheduler's live `VM`; `buf` must point at
/// `argc` initialized value words whose references the caller transfers;
/// `op_code` must be one [`is_native_park_op`](crate::bytecode::is_native_park_op)
/// admits, and both ordinals must name resume points of the running body.
pub(crate) unsafe extern "C" fn al_shim_park_op(
    vmx: *mut VM,
    op_code: i64,
    buf: *const u64,
    argc: i64,
    retry_ordinal: i64,
    cont_ordinal: i64,
) -> u64 {
    // SAFETY: `vmx` is the running scheduler's VM per the contract above.
    let vm = unsafe { &mut *vmx };
    for i in 0..argc as usize {
        // SAFETY: `buf` holds `argc` initialized owned words, transferred here.
        vm.stack
            .push(unsafe { Value::from_bits(buf.add(i).read()) });
    }
    let mut reds = vm.native_reds;
    let out = vm.run_park_op(op_code as u8, &mut reds);
    vm.native_reds = reds;
    match out {
        Ok(None) => NativeStatus::Done as u64,
        Ok(Some(parked)) => {
            let ordinal = match parked.resume {
                Resume::Retry => retry_ordinal,
                Resume::Continue => cont_ordinal,
            };
            vm.frame_mut().ip = ordinal as i32;
            vm.status_from_outcome(Ok(Step::Parked(parked.wait))) as u64
        }
        Err(e) => vm.status_from_outcome(Err(e)) as u64,
    }
}

impl VM {
    /// Dispatch an [`is_native_park_op`](crate::bytecode::is_native_park_op)
    /// opcode to its interpreter method. Mirrors the interpreter's `park!`
    /// arms; the two share the method bodies.
    fn run_park_op(&mut self, op_code: u8, reds: &mut i32) -> VmResult<Option<Parked>> {
        match op_code {
            opc::FILE_READ => self.file_read(reds),
            opc::FILE_WRITE => self.file_write(reds),
            opc::TCP_ACCEPT => self.tcp_accept(reds),
            opc::TCP_CONNECT => self.tcp_connect(reds),
            opc::TCP_CONNECT_UNTIL => self.tcp_connect_until(reds),
            opc::TCP_READ => self.tcp_read(reds),
            opc::TCP_READ_UNTIL => self.tcp_read_until(reds),
            opc::TCP_WRITE => self.tcp_write(reds),
            opc::TCP_WRITE_PARTS => self.tcp_write_parts(reds),
            opc::TLS_HANDSHAKE => self.tls_handshake(reds),
            opc::TLS_HANDSHAKE_UNTIL => self.tls_handshake_until(reds),
            opc::TLS_READ => self.tls_read(reds),
            opc::TLS_READ_UNTIL => self.tls_read_until(reds),
            opc::TLS_WRITE => self.tls_write(reds),
            opc::DNS_RESOLVE => self.dns_resolve(reds),
            opc::DNS_RESOLVE_UNTIL => self.dns_resolve_until(reds),
            opc::SLEEP => self.sleep(),
            opc::PORT_SPAWN => self.port_spawn(reds),
            opc::PORT_CLOSE => self.port_close(reds),
            opc::SUBJECT_RECEIVE => self.subject_receive(reds),
            opc::SUBJECT_RECEIVE_UNTIL => self.subject_receive_until(reds),
            _ => proof_violation("run_park_op on an op is_native_park_op excludes"),
        }
    }
}

impl VM {
    /// Dispatch an [`is_native_bridge_op`](crate::bytecode::is_native_bridge_op)
    /// opcode to its interpreter method. `operand` is the op's flattened
    /// immediate (unused by ops without one). Mirrors the interpreter's
    /// `run_slice` arms exactly; the two share the method bodies.
    fn run_bridge_op(&mut self, op_code: u8, operand: i32, reds: &mut i32) -> VmResult<()> {
        match op_code {
            opc::INDEX => self.seq_index(),
            opc::INDEX_OR => self.seq_index_or(operand),
            // The only bridge arms that charge reductions. `reds` is already
            // threaded here for the whole bridge, so the compiled path spends
            // the same counter `exec.rs` does and the two cannot disagree
            // about what a wire call cost.
            opc::WIRE_ENCODE => self.wire_encode(operand, reds),
            opc::WIRE_DECODE => self.wire_decode(operand, reds),
            opc::ELEM_AT => self.elem_at(operand),
            opc::SEQ_DROP => self.seq_drop(),
            opc::ARRAY_CONCAT => self.seq_concat(),
            opc::MAKE_RANGE => self.make_range(),
            opc::MAP_GET => self.map_get(),
            opc::MAP_HAS => self.map_has(),
            opc::MAP_KEYS => self.map_keys(),
            opc::MAP_VALUES => self.map_values(),
            opc::MAP_SIZE => self.map_size(),
            opc::MAP_NEW => self.map_new(),
            opc::MAP_SET => self.map_set(),
            opc::MAP_DELETE => self.map_delete(),
            opc::MAP_TO_LIST => self.map_to_list(),
            opc::JSON_PARSE => self.json_parse(),
            opc::JSON_KIND => self.json_kind(),
            opc::JSON_LEN => self.json_len(),
            opc::JSON_FIELD => self.json_field(),
            opc::JSON_INDEX => self.json_index(),
            opc::JSON_ENTRIES => self.json_entries(),
            opc::JSON_ELEMENTS => self.json_elements(),
            opc::JSON_STRING => self.json_string(),
            opc::JSON_INT => self.json_int(),
            opc::JSON_INT_TEXT => self.json_int_text(),
            opc::JSON_FLOAT => self.json_float(),
            opc::JSON_BOOL => self.json_bool(),
            opc::JSON_ENCODE => self.json_encode(),
            opc::ENV_MAP => self.env_map(),
            opc::STR_SPLIT => self.str_split(),
            opc::STR_LEN => self.str_len(),
            opc::STR_CONTAINS => self.str_contains(),
            opc::STR_TRIM => self.str_trim(),
            opc::STR_TO_GRAPHEMES => self.str_to_graphemes(),
            opc::INT_TO_STRING => self.int_to_string(),
            opc::INT_FROM_STRING => self.int_from_string(),
            opc::TO_STRING => self.op_to_string(),
            opc::STR_CONCAT_N => self.str_concat_n(operand as usize),
            opc::BIN_FROM_STRING => self.bin_from_string(),
            opc::BIN_TO_STRING => self.bin_to_string(),
            opc::BIN_BIT_SIZE => self.bin_bit_size(),
            opc::BIN_SLICE => self.bin_slice(),
            opc::BIN_APPEND => self.bin_append(),
            opc::BIN_CONCAT_N => self.bin_concat_n(operand as usize),
            opc::BIN_MATCH_PREFIX => self.bin_match_prefix(),
            opc::BIN_INDEX_OF => self.bin_index_of(),
            opc::BIN_BYTE_AT => self.bin_byte_at(),
            opc::BIN_PARSE_INT => self.bin_parse_int(),
            opc::BIN_EQ_IGNORE_ASCII_CASE => self.bin_eq_ignore_ascii_case(),
            opc::BIN_TO_ASCII_LOWER => self.bin_to_ascii_lower(),
            opc::BIN_FROM_INT_ASCII => self.bin_from_int_ascii(),
            opc::ADD_FLOAT => self.add_float(),
            opc::SUB_FLOAT => self.sub_float(),
            opc::MUL_FLOAT => self.mul_float(),
            opc::DIV_FLOAT => self.div_float(),
            opc::NEG_FLOAT => self.neg_float(),
            opc::LT_FLOAT => self.lt_float(),
            opc::GT_FLOAT => self.gt_float(),
            opc::LTE_FLOAT => self.lte_float(),
            opc::GTE_FLOAT => self.gte_float(),
            opc::FLOAT_FLOOR => self.float_floor(),
            opc::FLOAT_CEIL => self.float_ceil(),
            opc::FLOAT_ROUND => self.float_round(),
            opc::FLOAT_TRUNCATE => self.float_truncate(),
            opc::FLOAT_FROM_INT => self.float_from_int(),
            opc::FLOAT_TO_STRING => self.float_to_string(),
            opc::MONOTONIC => self.monotonic(),
            opc::WALL_CLOCK => self.wall_clock(),
            opc::RANDOM_BYTES => self.random_bytes(),
            opc::IP_PARSE => self.ip_parse(),
            opc::HTTP_CHUNK_DECODE => self.http_chunk_decode(),
            opc::HTTP_PARSE_RESPONSE_HEAD => self.http_parse_response_head(),
            opc::ADD => self.add(),
            opc::SUB => self.sub(),
            opc::MUL => self.mul(),
            opc::DIV => self.div(),
            opc::MOD => self.rem(),
            opc::NEG => self.neg(),
            opc::BIT_AND => self.bit_and(),
            opc::BIT_OR => self.bit_or(),
            opc::BIT_XOR => self.bit_xor(),
            opc::BIT_NOT => self.bit_not(),
            opc::BIT_SHL => self.bit_shl(),
            opc::BIT_SHR => self.bit_shr(),
            // The polymorphic comparisons: the emitter sends these here only
            // when it could not prove both operands Int (the Int case lowers
            // inline via `nop_of`).
            opc::EQ => self.eq_values(),
            opc::NEQ => self.neq_values(),
            opc::LT => self.compare_push(|o| o.is_lt()),
            opc::GT => self.compare_push(|o| o.is_gt()),
            opc::LTE => self.compare_push(|o| o.is_le()),
            opc::GTE => self.compare_push(|o| o.is_ge()),
            // The checked twins of the proof-dependent fast paths: the
            // emitter sends a site here when the type could not be proven, so
            // the unchecked shim would have been unsound.
            opc::TUPLE_INDEX => self.tuple_index(operand),
            opc::APPEND => self.seq_append(operand),
            opc::PREPEND => self.seq_prepend(operand),
            opc::ARRAY_LEN => self.seq_len(),
            opc::BIN_BYTE_SIZE => self.bin_byte_size(),
            opc::HTTP_PARSE_HEAD => self.http_parse_head(),
            opc::HTTP_HEADERS_VALID => self.http_headers_valid(),
            opc::HTTP_FRAMING => self.http_framing(),
            opc::HTTP_HEADER_HAS => self.http_header_has(),
            opc::HTTP_SERIALIZE_HEAD => self.http_serialize_head(),
            opc::ADD_STR => self.str_concat2(),
            opc::GET_FIELD => self.get_field(operand),
            opc::BIN_FROM_INT => self.bin_from_int(),
            opc::BIN_READ_INT => self.bin_read_int(),
            opc::BIN_TAKE => self.bin_take(),
            opc::BIN_VIEW => self.bin_view(),
            // Pushes (codepoint, nbits); Core sees one `(Int, Int)`, and the
            // bytecode emitter follows the op with a `MakeTuple 2`.
            opc::BIN_READ_UTF8 => {
                self.bin_read_utf8()?;
                self.make_tuple(2)
            }
            opc::HTTP_HEADER_GET => self.http_header_get(),
            opc::STACK_DEPTH => self.stack_depth(),
            opc::LIVE_SUBJECTS => self.live_subjects(),
            opc::BLOCKING_THREADS => self.blocking_threads(),
            opc::TCP_LOCAL_ADDR => self.tcp_local_addr(),
            opc::PROCESS_SPAWN => self.process_spawn(reds, Link::ToParent),
            opc::PROCESS_SPAWN_UNLINKED => self.process_spawn(reds, Link::None),
            opc::PROCESS_KILL => self.process_kill(reds),
            opc::PROCESS_SELF => {
                self.process_self();
                Ok(())
            }
            opc::PROCESS_MONITOR => self.process_monitor(reds),
            opc::SUPERVISOR_NEW => self.supervisor_new(),
            opc::SUPERVISOR_WORKER => self.supervisor_worker(reds),
            opc::FACTORY_NEW => self.factory_new(reds),
            opc::FACTORY_LOOKUP_OR_START => self.factory_lookup_or_start(reds),
            opc::FACTORY_LOOKUP => self.factory_lookup(),
            opc::SUPERVISED_OF => self.supervised_of(),
            opc::SUPERVISED_PARENT => self.supervised_parent(),
            opc::SUPERVISED_CHILDREN => self.supervised_children(),
            opc::SUPERVISED_COUNT => self.supervised_count(),
            opc::SUPERVISED_INFO => self.supervised_info(),
            opc::WATCH_NEW => self.watch_new(reds),
            opc::WATCH_CANCEL => self.watch_cancel(),
            opc::PROCESS_DEMONITOR => self.process_demonitor(),
            opc::ARGV => self.argv(),
            opc::TCP_LISTEN => self.tcp_listen(),
            opc::TCP_CLOSE => self.tcp_close(reds),
            opc::TLS_CLOSE => self.tls_close(reds),
            opc::TCP_GIVE => self.tcp_give(),
            opc::TCP_CLOSE_SERVER => self.tcp_close_server(),
            opc::SUPERVISOR_WORKER_ON_EACH => self.supervisor_worker_on_each(reds),
            opc::FACTORY_SPAWN => self.factory_spawn(reds),
            opc::SUBJECT_NEW => self.subject_new(),
            opc::SUBJECT_SEND => self.subject_send(reds, Delivery::Back),
            opc::SUBJECT_SEND_URGENT => self.subject_send(reds, Delivery::Front),
            // `Print` is the one void op: it pushes nothing, and the bytecode
            // emitter supplies the `()` with a following `PushNil`. The bridge
            // returns exactly one value, so it must do the same.
            opc::PRINT => {
                self.print_op(reds)?;
                // `Op::PushNil` pushes the prelude's `Nil` *constructor*, not
                // the primitive nil word — callers match on it as an enum.
                let nil = self.make_nil()?;
                self.stack.push(nil);
                Ok(())
            }
            _ => proof_violation("run_bridge_op on an op is_native_bridge_op excludes"),
        }
    }
}

/// `Op::PushCapture`: capture `idx` of the closure this frame is running,
/// borrowed. The frame's `captures` handle is the closure itself, and it is
/// rooted for the frame's whole life, so the word stays live; the caller
/// retains only where it keeps one. A shim rather than an inline load because
/// the frame is not in the value stack's ABI-stable region.
///
/// # Safety
/// `vmx` must point at the running scheduler's live `VM`, and `idx` must be in
/// range for the running closure; the emitter guarantees both. The returned
/// word carries no reference the caller owns.
pub(crate) unsafe extern "C" fn al_shim_push_capture(vmx: *mut VM, idx: i64) -> u64 {
    // SAFETY: `vmx` is the running scheduler's VM per the contract above.
    let vm = unsafe { &mut *vmx };
    match vm
        .frame()
        .captures
        .as_closure()
        .and_then(|cl| cl.captures().get(idx as usize))
    {
        Some(v) => v.to_bits(),
        // The compiler mints capture indices from the closure it built.
        None => proof_violation("PushCapture index out of range for the running closure"),
    }
}

/// `Op::PushSelf`: the closure this frame is running, borrowed. Same rooting
/// argument as [`al_shim_push_capture`].
///
/// # Safety
/// `vmx` must point at the running scheduler's live `VM`. The returned word
/// carries no reference the caller owns.
pub(crate) unsafe extern "C" fn al_shim_push_self(vmx: *mut VM) -> u64 {
    // SAFETY: `vmx` is the running scheduler's VM per the contract above.
    let vm = unsafe { &mut *vmx };
    vm.frame().captures.to_bits()
}

/// Every shim as `(symbol name, address)` for `JITBuilder::symbol`. These
/// names are what the CLIF emitter's declared externals resolve against.
pub(crate) fn shim_symbols() -> [(&'static str, *const u8); 21] {
    [
        ("al_shim_push_global", al_shim_push_global as *const u8),
        ("al_shim_int_box", al_shim_int_box as *const u8),
        ("al_shim_enum_alloc", al_shim_enum_alloc as *const u8),
        ("al_shim_make_array", al_shim_make_array as *const u8),
        ("al_shim_make_tuple", al_shim_make_tuple as *const u8),
        ("al_shim_seq_len", al_shim_seq_len as *const u8),
        ("al_shim_seq_append", al_shim_seq_append as *const u8),
        ("al_shim_seq_prepend", al_shim_seq_prepend as *const u8),
        ("al_shim_bin_byte_size", al_shim_bin_byte_size as *const u8),
        (
            "al_shim_http_parse_head",
            al_shim_http_parse_head as *const u8,
        ),
        (
            "al_shim_http_headers_valid",
            al_shim_http_headers_valid as *const u8,
        ),
        (
            "al_shim_http_header_has",
            al_shim_http_header_has as *const u8,
        ),
        (
            "al_shim_http_serialize_head",
            al_shim_http_serialize_head as *const u8,
        ),
        ("al_shim_http_framing", al_shim_http_framing as *const u8),
        ("al_shim_div_int", al_shim_div_int as *const u8),
        ("al_shim_mod_int", al_shim_mod_int as *const u8),
        ("al_shim_op", al_shim_op as *const u8),
        ("al_shim_push_capture", al_shim_push_capture as *const u8),
        ("al_shim_push_self", al_shim_push_self as *const u8),
        ("al_shim_park_op", al_shim_park_op as *const u8),
        ("al_shim_try_op", al_shim_try_op as *const u8),
    ]
}

#[cfg(test)]
mod tests {
    use crate::TypeId;
    use crate::bytecode::value::HeapTag;

    use super::super::halt_test_vm;
    use super::*;

    const SMALL_MIN: i64 = -(1i64 << 47);
    const SMALL_MAX: i64 = (1i64 << 47) - 1;

    fn unbox_and_release(bits: u64) -> i64 {
        unsafe { Value::from_bits(bits) }.as_int_typed()
    }

    fn is_bigint(bits: u64) -> bool {
        let v = std::mem::ManuallyDrop::new(unsafe { Value::from_bits(bits) });
        v.heap_tag() == Some(HeapTag::BigInt)
    }

    #[test]
    fn small_int_range_bounds_match_the_value_encoding() {
        assert!(Value::fits_small_int(SMALL_MIN));
        assert!(Value::fits_small_int(SMALL_MAX));
        assert!(!Value::fits_small_int(SMALL_MIN - 1));
        assert!(!Value::fits_small_int(SMALL_MAX + 1));
    }

    #[test]
    fn div_shim_matches_interpreter_edges() {
        assert_eq!(al_shim_div_int(7, 2), 3);
        assert_eq!(al_shim_div_int(-7, 2), -3); // truncates toward zero
        assert_eq!(al_shim_div_int(7, -2), -3);
        assert_eq!(al_shim_div_int(5, 0), 0); // x / 0 == 0, no error
        assert_eq!(al_shim_div_int(0, 0), 0);
        assert_eq!(al_shim_div_int(i64::MIN, -1), i64::MIN); // wraps, no trap
        assert_eq!(al_shim_div_int(i64::MIN, 1), i64::MIN);
    }

    #[test]
    fn mod_shim_matches_interpreter_edges() {
        assert_eq!(al_shim_mod_int(7, 2), 1);
        assert_eq!(al_shim_mod_int(-7, 2), -1); // takes the dividend's sign
        assert_eq!(al_shim_mod_int(7, -2), 1);
        assert_eq!(al_shim_mod_int(5, 0), 5); // x % 0 == x, no error
        assert_eq!(al_shim_mod_int(-5, 0), -5);
        assert_eq!(al_shim_mod_int(i64::MIN, -1), 0); // wraps, no trap
    }

    #[test]
    fn enum_alloc_builds_an_interpreter_shaped_cell() {
        use std::sync::Arc;

        use crate::bytecode::value::take_freed_objects;
        use crate::frozen::FrozenArea;

        let mut vm = halt_test_vm();
        let vmp = &raw mut vm;

        // Frozen name/label constants, what the emitter bakes.
        let area = Arc::new(FrozenArea::new());
        let mut fb = area.builder();
        let en = fb.str("Shape").into_value();
        let vn = fb.str("Point").into_value();
        let labels = {
            let x = fb.str("x");
            let y = fb.str("y");
            fb.tuple(vec![x, y]).into_value()
        };

        let packed: u64 = 42u64 | (1u64 << 32);
        let a = Value::small_int(7).to_bits();
        // A boxed field proves the reference transfer.
        let b = into_bits(Value::int_in(&mut vm.heap, i64::MAX));
        let payload = [a, b];

        take_freed_objects();
        let bits = unsafe {
            al_shim_enum_alloc(
                vmp,
                packed,
                en.to_bits(),
                vn.to_bits(),
                labels.to_bits(),
                payload.as_ptr(),
                2,
            )
        };
        // Nothing freed: the transferred field references moved into the cell.
        assert_eq!(take_freed_objects(), 0);

        let v = unsafe { Value::from_bits(bits) };
        let e = v.as_enum().expect("shim must build an Enum cell");
        assert_eq!(e.type_id(), TypeId(42));
        assert_eq!(e.variant_idx(), 1);
        assert_eq!(e.enum_name(), "Shape");
        assert_eq!(e.variant_name(), "Point");
        assert_eq!(e.payload().len(), 2);
        assert_eq!(e.payload()[0].as_int(), Some(7));
        assert_eq!(e.payload()[1].as_int(), Some(i64::MAX));

        // The hash word is written 0 and computed on first use.
        assert_ne!(v.as_enum().unwrap().hash(), 0);

        drop(v);
        assert_eq!(take_freed_objects(), 2);
    }

    /// `al_shim_int_box` must keep in-range Ints immediate and spill past the
    /// 47-bit payload to a BigInt, exactly like the interpreter's `boxed_int`.
    #[test]
    fn int_box_keeps_immediates_and_spills_the_boundary() {
        let mut vm = halt_test_vm();
        let vmp = &raw mut vm;
        for i in [0, 1, -1, SMALL_MIN, SMALL_MAX] {
            let bits = unsafe { al_shim_int_box(vmp, i) };
            assert_eq!(bits, Value::small_int(i).to_bits());
        }
        for i in [SMALL_MAX + 1, SMALL_MIN - 1, i64::MAX, i64::MIN] {
            let bits = unsafe { al_shim_int_box(vmp, i) };
            assert!(is_bigint(bits), "{i} must spill");
            assert_eq!(unbox_and_release(bits), i);
        }
    }

    #[test]
    fn symbol_table_names_are_unique_and_addresses_non_null() {
        let syms = shim_symbols();
        for (i, (name, addr)) in syms.iter().enumerate() {
            assert!(!addr.is_null(), "{name} has a null address");
            assert!(
                syms[..i].iter().all(|(n, _)| n != name),
                "duplicate symbol {name}"
            );
        }
    }
}
