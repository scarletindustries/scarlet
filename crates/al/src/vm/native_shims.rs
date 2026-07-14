//! Interpreter-parity runtime shims for natively compiled Int arithmetic.
//!
//! Inside a compiled function a proven-`Int` value lives as a raw `i64` in
//! registers, which is exact parity with the interpreter: its arithmetic arms
//! (`bin_int!` in `exec.rs`) also operate on the full `i64` domain — decode
//! with `as_int_typed`, apply a `wrapping_*` op, re-box with `push_int`.
//! Boxing only matters at the seams, and the seams are what these shims
//! cover:
//!
//! - **Spill.** AL Ints are 47-bit-payload NaN-boxed immediates; a result
//!   outside `[-2^47, 2^47)` boxes to an arena `BigInt`
//!   ([`VM::boxed_int`](super::VM) → `Value::int_in`). Compiled code inlines
//!   the `fits_small_int` range check after every arithmetic op and branches
//!   to [`al_shim_int_box`] on the cold side.
//! - **Unbox.** An Int-typed operand loaded from a frame slot may be a
//!   `BigInt` box rather than a small-int immediate. Compiled code inlines
//!   the small-int decode and branches to [`al_shim_int_unbox`] otherwise.
//! - **Div/Mod edge cases.** The interpreter is total: `x / 0 == 0`,
//!   `x % 0 == x`, and `MIN / -1` / `MIN % -1` wrap (`wrapping_div` /
//!   `wrapping_rem`, i.e. `MIN` and `0`). A bare `sdiv`/`srem` instruction
//!   would trap on both edges, so compiled code either inlines the guards or
//!   calls [`al_shim_div_int`] / [`al_shim_mod_int`]. There is no error
//!   path: like the interpreter's `Op::DivInt`/`Op::ModInt`, these never
//!   raise.
//! - **Whole-op fallback.** The `al_shim_{add,sub,mul,neg,div,mod}_int_val`
//!   shims reproduce a complete interpreter handler over value *bits*:
//!   decode both operands, release the references the caller transfers
//!   (mirroring the interpreter's operand pops), apply the op, box the
//!   result. They are the one-call cold path when an operand is already
//!   boxed and the emitter does not want to reassemble unbox/op/box inline.
//!
//! Reference-count accounting matches the interpreter exactly: operand
//! drops go through `Value::drop` (`release_bits`), so `BigInt` frees land
//! in the same `FREED_OBJECTS` accounting the interpreter's pops feed, and
//! results are boxed by the same `VM::boxed_int` the interpreter's
//! `push_int` uses.
//!
//! Generated code reaches these functions by symbol: the JIT finalize step
//! registers every [`shim_symbols`] pair with the builder
//! (`JITBuilder::symbol`), so `al_core`'s CLIF construction can name them
//! without a dependency on this crate.

// Designated unsafe module: generated code cannot pass `&mut VM` or `Value`,
// so the boundary works in raw pointers and raw NaN-box bits; every shim's
// safety contract states the ownership it assumes.
#![allow(unsafe_code)]

use al_core::TypeId;
use al_core::bytecode::Value;
use al_core::bytecode::value::ReuseAddr;

use super::VM;

/// Escape raw bits from an owned value without running its destructor: the
/// reference count the value carries transfers to the returned bits.
#[inline]
fn into_bits(v: Value) -> u64 {
    std::mem::ManuallyDrop::new(v).to_bits()
}

/// Truncating division with the interpreter's totality: `x / 0 == 0` and
/// `MIN / -1 == MIN` (wraps instead of trapping). Mirrors `Op::DivInt`.
#[inline]
fn div_int(a: i64, b: i64) -> i64 {
    if b == 0 { 0 } else { a.wrapping_div(b) }
}

/// Remainder with the interpreter's totality: `x % 0 == x` (not an error)
/// and `MIN % -1 == 0` (wraps instead of trapping). The result takes the
/// dividend's sign, as `wrapping_rem` does. Mirrors `Op::ModInt`.
#[inline]
fn mod_int(a: i64, b: i64) -> i64 {
    if b == 0 { a } else { a.wrapping_rem(b) }
}

/// Shared body of the whole-op shims: decode both operands, release the
/// operand references the caller transferred (the interpreter's pops drop
/// theirs the same way), apply `op` over the full `i64` domain, and box the
/// result through [`VM::boxed_int`](super::VM).
///
/// # Safety
/// `vmx` must point at the running scheduler's live `VM`; `a` and `b` must
/// each be the bits of an Int-typed `Value` (small-int immediate or live
/// `BigInt` box) whose reference the caller owns and transfers to this call.
unsafe fn bin_int_val(vmx: *mut VM, a: u64, b: u64, op: impl FnOnce(i64, i64) -> i64) -> u64 {
    // SAFETY: ownership of both operand references is transferred per the
    // contract above; the drops below balance them exactly once.
    let (a, b) = unsafe { (Value::from_bits(a), Value::from_bits(b)) };
    let r = op(a.as_int_typed(), b.as_int_typed());
    drop(a);
    drop(b);
    // SAFETY: `vmx` is the running scheduler's VM per the contract above.
    let vm = unsafe { &mut *vmx };
    into_bits(vm.boxed_int(r))
}

/// Box a full-range `i64` as an Int value, spilling past the 47-bit
/// immediate range to an arena `BigInt` — the interpreter's `push_int`
/// boxing, minus the stack push. Compiled code inlines the
/// `fits_small_int` fast path and calls this only on the spill side, but
/// the shim is total either way.
///
/// The returned bits carry one owned reference (freshly allocated for a
/// spill); the caller must store or release it exactly once.
///
/// # Safety
/// `vmx` must point at the running scheduler's live `VM`.
#[cold]
pub unsafe extern "C" fn al_shim_int_box(vmx: *mut VM, i: i64) -> u64 {
    // SAFETY: `vmx` is the running scheduler's VM per the contract above.
    let vm = unsafe { &mut *vmx };
    into_bits(vm.boxed_int(i))
}

/// Decode an Int-typed value's bits to the full `i64` — `as_int_typed`
/// behind the C ABI. Compiled code inlines the small-int decode (sign-extend
/// the 48-bit payload) and calls this only when the value is a `BigInt` box,
/// but the shim is total either way.
///
/// Borrow-only: the caller keeps its reference; no count moves.
///
/// # Safety
/// `bits` must be the bits of an Int-typed `Value`; a `BigInt` box must be
/// live for the duration of the call.
#[cold]
pub unsafe extern "C" fn al_shim_int_unbox(bits: u64) -> i64 {
    // SAFETY: borrowed read per the contract above; ManuallyDrop keeps the
    // caller's reference count untouched.
    let v = std::mem::ManuallyDrop::new(unsafe { Value::from_bits(bits) });
    v.as_int_typed()
}

/// `Op::AddInt` over value bits: wrapping `i64` add, result re-boxed (spills
/// past the immediate range). Consumes both operand references.
///
/// # Safety
/// As [`bin_int_val`].
#[cold]
pub unsafe extern "C" fn al_shim_add_int_val(vmx: *mut VM, a: u64, b: u64) -> u64 {
    // SAFETY: forwarded contract.
    unsafe { bin_int_val(vmx, a, b, i64::wrapping_add) }
}

/// `Op::SubInt` over value bits. Consumes both operand references.
///
/// # Safety
/// As [`bin_int_val`].
#[cold]
pub unsafe extern "C" fn al_shim_sub_int_val(vmx: *mut VM, a: u64, b: u64) -> u64 {
    // SAFETY: forwarded contract.
    unsafe { bin_int_val(vmx, a, b, i64::wrapping_sub) }
}

/// `Op::MulInt` over value bits. Consumes both operand references.
///
/// # Safety
/// As [`bin_int_val`].
#[cold]
pub unsafe extern "C" fn al_shim_mul_int_val(vmx: *mut VM, a: u64, b: u64) -> u64 {
    // SAFETY: forwarded contract.
    unsafe { bin_int_val(vmx, a, b, i64::wrapping_mul) }
}

/// `Op::DivInt` over value bits ([`div_int`] semantics). Consumes both
/// operand references.
///
/// # Safety
/// As [`bin_int_val`].
#[cold]
pub unsafe extern "C" fn al_shim_div_int_val(vmx: *mut VM, a: u64, b: u64) -> u64 {
    // SAFETY: forwarded contract.
    unsafe { bin_int_val(vmx, a, b, div_int) }
}

/// `Op::ModInt` over value bits ([`mod_int`] semantics). Consumes both
/// operand references.
///
/// # Safety
/// As [`bin_int_val`].
#[cold]
pub unsafe extern "C" fn al_shim_mod_int_val(vmx: *mut VM, a: u64, b: u64) -> u64 {
    // SAFETY: forwarded contract.
    unsafe { bin_int_val(vmx, a, b, mod_int) }
}

/// `Op::NegInt` over value bits: wrapping negation (`MIN` stays `MIN`),
/// result re-boxed (`-SMALL_INT_MIN` spills — the immediate range is
/// asymmetric). Consumes the operand reference.
///
/// # Safety
/// `vmx` must point at the running scheduler's live `VM`; `a` must be the
/// bits of an Int-typed `Value` whose reference the caller transfers.
#[cold]
pub unsafe extern "C" fn al_shim_neg_int_val(vmx: *mut VM, a: u64) -> u64 {
    // SAFETY: ownership of the operand reference is transferred per the
    // contract above; the drop balances it exactly once.
    let a = unsafe { Value::from_bits(a) };
    let r = a.as_int_typed().wrapping_neg();
    drop(a);
    // SAFETY: `vmx` is the running scheduler's VM per the contract above.
    let vm = unsafe { &mut *vmx };
    into_bits(vm.boxed_int(r))
}

/// `Op::MakeEnumPayload`'s allocation path behind the C ABI: build a tagged
/// enum cell in the running process heap — one `ProcHeap` allocation, exactly
/// the interpreter's (`Value::enum_reuse_in` with no reuse candidate). Cold:
/// compiled constructor sites take their in-place reuse fast path inline and
/// call here only when a fresh cell is needed.
///
/// The header words mirror `VM::make_enum_payload` bit for bit:
///
/// - `packed` is the compile-time `type_id | variant_idx << 32` word the
///   emitter baked (the interpreter reads the same value from its pooled Int
///   constant).
/// - `enum_name` / `variant_name` / `labels` are the *bits* of frozen
///   constants (two `Str`s and a `Tuple` of `Str`s), baked as instruction
///   immediates — immortal words whose clone/drop is free, stored into the
///   cell verbatim like the interpreter's constant-pool pushes.
/// - The hash word is written `0`: 0-means-unhashed is load-bearing
///   (`EnumRef::hash` computes and caches lazily; constructing is orders of
///   magnitude more common than hashing).
///
/// `payload` points at `len` value words, each carrying one owned reference
/// the caller transfers (its operand pushes); the construction takes the
/// cell's own reference per field and this shim releases the transferred
/// ones — the interpreter's build-then-truncate, in the same order.
///
/// # Safety
/// `vmx` must point at the running scheduler's live `VM`; `enum_name` and
/// `variant_name` must be the bits of live frozen `Str` values, `labels` of a
/// live frozen `Tuple` of `Str`s (all immortal, outliving the program);
/// `payload` must point at `len` initialized value words whose references
/// the caller owns and transfers to this call.
#[cold]
pub unsafe extern "C" fn al_shim_enum_alloc(
    vmx: *mut VM,
    packed: u64,
    enum_name: u64,
    variant_name: u64,
    labels: u64,
    payload: *const u64,
    len: i64,
) -> u64 {
    // The interpreter's decode of the packed header constant, bit for bit
    // (`make_enum_payload`: `TypeId(packed as i32)`, `(packed >> 32) as u16`).
    let type_id = TypeId(packed as i32);
    let variant_idx = (packed >> 32) as u16;
    let n = len as usize;
    // SAFETY: immortal frozen constants per the contract above — treating
    // the bits as owned is sound because immortal clone/drop never touches
    // memory; `move_child` stores the words verbatim.
    let (en, vn, lb) = unsafe {
        (
            Value::from_bits(enum_name),
            Value::from_bits(variant_name),
            Value::from_bits(labels),
        )
    };
    debug_assert!(en.is_immortal() && vn.is_immortal() && lb.is_immortal());
    // SAFETY: `payload` points at `n` initialized value words (`Value` is
    // repr(transparent) over u64); borrowed here — the construction takes
    // its own reference per field (`store_child`).
    let fields: &[Value] = if n == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(payload.cast::<Value>(), n) }
    };
    // SAFETY: `vmx` is the running scheduler's VM per the contract above.
    let vm = unsafe { &mut *vmx };
    // Hash 0 = not yet hashed; see the doc comment.
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
    // The interpreter's `stack.truncate(base)`: release the operand
    // references the caller transferred.
    for i in 0..n {
        // SAFETY: each word carries one owned reference, released exactly
        // once here.
        drop(unsafe { Value::from_bits(payload.add(i).read()) });
    }
    into_bits(v)
}

/// Truncating division on unboxed `i64`s with the interpreter's `Op::DivInt`
/// totality — see [`div_int`]. Pure; safe to call from anywhere.
pub extern "C" fn al_shim_div_int(a: i64, b: i64) -> i64 {
    div_int(a, b)
}

/// Remainder on unboxed `i64`s with the interpreter's `Op::ModInt` totality
/// — see [`mod_int`]. Pure; safe to call from anywhere.
pub extern "C" fn al_shim_mod_int(a: i64, b: i64) -> i64 {
    mod_int(a, b)
}

/// Every shim, as `(symbol name, address)` pairs for JIT symbol
/// registration (`JITBuilder::symbol`). The names here are the contract the
/// CLIF emitter's `declare`d externals resolve against; keep the two sides
/// sourced from this one table.
pub fn shim_symbols() -> [(&'static str, *const u8); 11] {
    [
        ("al_shim_int_box", al_shim_int_box as *const u8),
        ("al_shim_int_unbox", al_shim_int_unbox as *const u8),
        ("al_shim_enum_alloc", al_shim_enum_alloc as *const u8),
        ("al_shim_add_int_val", al_shim_add_int_val as *const u8),
        ("al_shim_sub_int_val", al_shim_sub_int_val as *const u8),
        ("al_shim_mul_int_val", al_shim_mul_int_val as *const u8),
        ("al_shim_div_int_val", al_shim_div_int_val as *const u8),
        ("al_shim_mod_int_val", al_shim_mod_int_val as *const u8),
        ("al_shim_neg_int_val", al_shim_neg_int_val as *const u8),
        ("al_shim_div_int", al_shim_div_int as *const u8),
        ("al_shim_mod_int", al_shim_mod_int as *const u8),
    ]
}

#[cfg(test)]
mod tests {
    use al_core::bytecode::value::HeapTag;

    use super::super::halt_test_vm;
    use super::*;

    const SMALL_MIN: i64 = -(1i64 << 47);
    const SMALL_MAX: i64 = (1i64 << 47) - 1;

    // Decode a result's bits and release the reference the shim returned.
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
    fn box_keeps_in_range_ints_immediate() {
        let mut vm = halt_test_vm();
        let vmp = &raw mut vm;
        for i in [0, 1, -1, 42, SMALL_MIN, SMALL_MAX] {
            let bits = unsafe { al_shim_int_box(vmp, i) };
            assert_eq!(bits, Value::small_int(i).to_bits());
            assert_eq!(unsafe { al_shim_int_unbox(bits) }, i);
        }
    }

    #[test]
    fn box_spills_past_the_small_int_range_and_unbox_round_trips() {
        let mut vm = halt_test_vm();
        let vmp = &raw mut vm;
        for i in [SMALL_MAX + 1, SMALL_MIN - 1, i64::MAX, i64::MIN] {
            let bits = unsafe { al_shim_int_box(vmp, i) };
            assert!(is_bigint(bits));
            assert_eq!(unsafe { al_shim_int_unbox(bits) }, i);
            assert_eq!(unbox_and_release(bits), i);
        }
    }

    #[test]
    fn add_spills_at_the_small_boundary_and_wraps_at_i64() {
        let mut vm = halt_test_vm();
        let vmp = &raw mut vm;

        // Small operands whose sum leaves the immediate range: BigInt spill.
        let a = Value::small_int(SMALL_MAX).to_bits();
        let b = Value::small_int(1).to_bits();
        let r = unsafe { al_shim_add_int_val(vmp, a, b) };
        assert!(is_bigint(r));
        assert_eq!(unbox_and_release(r), SMALL_MAX + 1);

        // A boxed operand at i64::MAX: wraps to i64::MIN like the
        // interpreter's wrapping_add, staying a BigInt.
        let a = into_bits(Value::int_in(&mut vm.heap, i64::MAX));
        let b = Value::small_int(1).to_bits();
        let r = unsafe { al_shim_add_int_val(vmp, a, b) };
        assert_eq!(unbox_and_release(r), i64::MIN);
    }

    #[test]
    fn sub_and_mul_wrap_like_the_interpreter() {
        let mut vm = halt_test_vm();
        let vmp = &raw mut vm;

        let a = into_bits(Value::int_in(&mut vm.heap, i64::MIN));
        let b = Value::small_int(1).to_bits();
        let r = unsafe { al_shim_sub_int_val(vmp, a, b) };
        assert_eq!(unbox_and_release(r), i64::MAX);

        let a = Value::small_int(SMALL_MAX).to_bits();
        let b = Value::small_int(SMALL_MAX).to_bits();
        let r = unsafe { al_shim_mul_int_val(vmp, a, b) };
        assert_eq!(unbox_and_release(r), SMALL_MAX.wrapping_mul(SMALL_MAX));
    }

    #[test]
    fn div_and_mod_val_shims_cover_the_boxed_edges() {
        let mut vm = halt_test_vm();
        let vmp = &raw mut vm;

        // MIN / -1 wraps to MIN (still a BigInt), MIN % -1 is 0 (small).
        let a = into_bits(Value::int_in(&mut vm.heap, i64::MIN));
        let b = Value::small_int(-1).to_bits();
        let r = unsafe { al_shim_div_int_val(vmp, a, b) };
        assert!(is_bigint(r));
        assert_eq!(unbox_and_release(r), i64::MIN);

        let a = into_bits(Value::int_in(&mut vm.heap, i64::MIN));
        let b = Value::small_int(-1).to_bits();
        let r = unsafe { al_shim_mod_int_val(vmp, a, b) };
        assert_eq!(r, Value::small_int(0).to_bits());

        // x / 0 == 0 and x % 0 == x, even for a boxed x — the mod result is
        // a fresh box, exactly like the interpreter's pop-then-push_int.
        let a = into_bits(Value::int_in(&mut vm.heap, i64::MAX));
        let b = Value::small_int(0).to_bits();
        let r = unsafe { al_shim_div_int_val(vmp, a, b) };
        assert_eq!(r, Value::small_int(0).to_bits());

        let a = into_bits(Value::int_in(&mut vm.heap, i64::MAX));
        let b = Value::small_int(0).to_bits();
        let r = unsafe { al_shim_mod_int_val(vmp, a, b) };
        assert!(is_bigint(r));
        assert_eq!(unbox_and_release(r), i64::MAX);
    }

    #[test]
    fn neg_wraps_at_i64_min_and_spills_at_the_asymmetric_edge() {
        let mut vm = halt_test_vm();
        let vmp = &raw mut vm;

        // -SMALL_MIN == 2^47 does not fit the immediate range: spill.
        let a = Value::small_int(SMALL_MIN).to_bits();
        let r = unsafe { al_shim_neg_int_val(vmp, a) };
        assert!(is_bigint(r));
        assert_eq!(unbox_and_release(r), -SMALL_MIN);

        // i64::MIN negates to itself (wrapping), like the interpreter.
        let a = into_bits(Value::int_in(&mut vm.heap, i64::MIN));
        let r = unsafe { al_shim_neg_int_val(vmp, a) };
        assert_eq!(unbox_and_release(r), i64::MIN);
    }

    #[test]
    fn enum_alloc_builds_an_interpreter_shaped_cell() {
        use std::sync::Arc;

        use al_core::bytecode::value::take_freed_objects;
        use al_core::frozen::FrozenArea;

        let mut vm = halt_test_vm();
        let vmp = &raw mut vm;

        // Frozen name/label constants, exactly what the emitter bakes.
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
        // A boxed field proves the reference transfer: the cell must take
        // its own count and the shim must release the transferred one.
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
        // Building the cell frees nothing: the transferred field references
        // moved into it.
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

        // Lazy hash: the word is written 0 and computed on first use.
        assert_ne!(v.as_enum().unwrap().hash(), 0);

        // Dropping the one reference frees the cell and its boxed field.
        drop(v);
        assert_eq!(take_freed_objects(), 2);
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
