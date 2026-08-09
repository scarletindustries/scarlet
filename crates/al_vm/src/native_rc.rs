//! Inline reference-count sequences for the native (Cranelift) backend:
//! the dup (retain) and drop (release) halves of Perceus, as CLIF.
//!
//! The gate in front of the rc-slot access is type-directed ([`RcGate`]).
//! A statically-unboxed type (Float/Bool/Nil) gets no sequence at all. A type
//! proven to be a heap cell has `SIGN|QNAN` already set, so only the
//! `VALUE_IMMORTAL` bit-0 test remains ([`RcGate::ProvenHeap`]). Everything
//! else takes the full mortal-heap mask ([`RcGate::Dynamic`]), the same bit
//! math as the interpreter's `retain_bits`/`release_bits`.
//!
//! Immortal objects carry no refcount prefix, so the immortal test precedes
//! every rc-slot access under either gate; neither sequence ever dereferences
//! a non-mortal word.
//!
//! Frees route through the interpreter's own `release_at_zero` shim, so
//! `FREED_OBJECTS` accounting (and `charge_reclamation` fairness with it)
//! stays identical between backends. Dup never calls out.

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{self, InstBuilder, MemFlagsData, types};
use cranelift_frontend::FunctionBuilder;

use crate::bytecode::value::{
    NATIVE_MORTAL_GATE_MASK, NATIVE_MORTAL_HEAP_BITS, NATIVE_PTR_MASK, NATIVE_RC_BYTE_OFFSET,
};

/// The `VALUE_IMMORTAL` bit: the one bit by which the mortal-gate mask
/// exceeds its expected result.
const IMMORTAL_BIT: u64 = NATIVE_MORTAL_GATE_MASK & !NATIVE_MORTAL_HEAP_BITS;

/// How much of the mortal-heap gate the type proof lets an RC sequence skip.
/// There is no variant for a proven immediate: that caller emits no sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RcGate {
    /// The full mortal-heap mask, exactly `release_bits`/`retain_bits`.
    Dynamic,
    /// The word is known to be a heap cell, so only the `VALUE_IMMORTAL`
    /// bit-0 test remains.
    ProvenHeap,
}

/// Emit the mortal test for `bits`: branch to `rc_block` when the word is a
/// mortal heap cell, `done_block` otherwise. Seals `rc_block`. Public because
/// `al_core::core_ir::clif`'s fused reuse-drop opens with the same gate.
pub fn emit_mortal_gate(
    builder: &mut FunctionBuilder,
    bits: ir::Value,
    gate: RcGate,
    rc_block: ir::Block,
    done_block: ir::Block,
) {
    let is_mortal = match gate {
        RcGate::Dynamic => {
            let g = builder.ins().band_imm(bits, NATIVE_MORTAL_GATE_MASK as i64);
            builder
                .ins()
                .icmp_imm(IntCC::Equal, g, NATIVE_MORTAL_HEAP_BITS as i64)
        }
        RcGate::ProvenHeap => {
            let g = builder.ins().band_imm(bits, IMMORTAL_BIT as i64);
            builder.ins().icmp_imm(IntCC::Equal, g, 0)
        }
    };
    builder
        .ins()
        .brif(is_mortal, rc_block, &[], done_block, &[]);
    builder.seal_block(rc_block);
}

/// Emit the dynamic drop gate for the NaN-boxed value word `bits`:
/// [`emit_drop`] at [`RcGate::Dynamic`].
pub fn emit_dynamic_drop(
    builder: &mut FunctionBuilder,
    bits: ir::Value,
    release_at_zero: ir::FuncRef,
) {
    emit_drop(builder, bits, release_at_zero, RcGate::Dynamic);
}

/// Emit the drop (release) sequence for the NaN-boxed value word `bits`,
/// gated at `gate` strength.
///
/// `release_at_zero` must be a declared reference to
/// [`crate::bytecode::value::native_release_at_zero`]: one pointer-sized
/// argument, no return.
///
/// Terminates the current block and leaves the builder in a new, sealed
/// merge block.
pub fn emit_drop(
    builder: &mut FunctionBuilder,
    bits: ir::Value,
    release_at_zero: ir::FuncRef,
    gate: RcGate,
) {
    let rc_block = builder.create_block();
    let dec_block = builder.create_block();
    let zero_block = builder.create_block();
    let done_block = builder.create_block();
    builder.set_cold_block(zero_block);

    emit_mortal_gate(builder, bits, gate, rc_block, done_block);

    // A count of u64::MAX is permanently live and never decrements, as in
    // `rc_decrement_is_zero`.
    builder.switch_to_block(rc_block);
    let obj = builder.ins().band_imm(bits, NATIVE_PTR_MASK as i64);
    let rc = builder.ins().load(
        types::I64,
        MemFlagsData::trusted(),
        obj,
        NATIVE_RC_BYTE_OFFSET,
    );
    let saturated = builder.ins().icmp_imm(IntCC::Equal, rc, -1);
    builder
        .ins()
        .brif(saturated, done_block, &[], dec_block, &[]);
    builder.seal_block(dec_block);

    builder.switch_to_block(dec_block);
    let dec = builder.ins().iadd_imm(rc, -1);
    builder
        .ins()
        .store(MemFlagsData::trusted(), dec, obj, NATIVE_RC_BYTE_OFFSET);
    let at_zero = builder.ins().icmp_imm(IntCC::Equal, dec, 0);
    builder
        .ins()
        .brif(at_zero, zero_block, &[], done_block, &[]);
    builder.seal_block(zero_block);

    builder.switch_to_block(zero_block);
    builder.ins().call(release_at_zero, &[obj]);
    builder.ins().jump(done_block, &[]);
    builder.seal_block(done_block);

    builder.switch_to_block(done_block);
}

/// Emit the dynamic dup (retain) for the NaN-boxed value word `bits`:
/// [`emit_dup`] at [`RcGate::Dynamic`].
pub fn emit_dynamic_dup(builder: &mut FunctionBuilder, bits: ir::Value) {
    emit_dup(builder, bits, RcGate::Dynamic);
}

/// Emit the dup (retain) for the NaN-boxed value word `bits`, gated at
/// `gate` strength: take one owned reference, exactly `rc_increment` behind
/// the mortal gate. Emitted wherever compiled code hands a borrowed register
/// copy to an ownership-taking consumer while the frame slot keeps its own
/// reference.
///
/// Terminates the current block and leaves the builder in a new, sealed
/// block.
pub fn emit_dup(builder: &mut FunctionBuilder, bits: ir::Value, gate: RcGate) {
    let rc_block = builder.create_block();
    let bump_block = builder.create_block();
    let done_block = builder.create_block();

    emit_mortal_gate(builder, bits, gate, rc_block, done_block);

    // Mirrors `rc_increment`'s `saturating_add(1)`: u64::MAX stays put.
    builder.switch_to_block(rc_block);
    let obj = builder.ins().band_imm(bits, NATIVE_PTR_MASK as i64);
    let rc = builder.ins().load(
        types::I64,
        MemFlagsData::trusted(),
        obj,
        NATIVE_RC_BYTE_OFFSET,
    );
    let saturated = builder.ins().icmp_imm(IntCC::Equal, rc, -1);
    builder
        .ins()
        .brif(saturated, done_block, &[], bump_block, &[]);
    builder.seal_block(bump_block);

    builder.switch_to_block(bump_block);
    let bumped = builder.ins().iadd_imm(rc, 1);
    builder
        .ins()
        .store(MemFlagsData::trusted(), bumped, obj, NATIVE_RC_BYTE_OFFSET);
    builder.ins().jump(done_block, &[]);
    builder.seal_block(done_block);

    builder.switch_to_block(done_block);
}

// These tests drive JIT-compiled code and touch raw refcount words.
#[allow(unsafe_code)]
#[cfg(test)]
mod tests {
    use cranelift_codegen::ir::AbiParam;
    use cranelift_codegen::settings::{self, Configurable};
    use cranelift_frontend::FunctionBuilderContext;
    use cranelift_jit::{JITBuilder, JITModule};
    use cranelift_module::{Linkage, Module, default_libcall_names};

    use super::*;
    use crate::bytecode::value::{
        NATIVE_RELEASE_AT_ZERO_SYMBOL, Value, native_release_at_zero, rc_increment, rc_slot,
        take_freed_objects,
    };
    use crate::heap::ProcHeap;

    /// JIT-compile `extern "C" fn(bits: u64)` whose body is exactly one
    /// emitted RC sequence, with the release symbol resolved to the real
    /// `native_release_at_zero`.
    fn jit_rc_seq(
        emit: impl FnOnce(&mut FunctionBuilder, ir::Value, ir::FuncRef),
    ) -> (JITModule, unsafe extern "C" fn(u64)) {
        let mut flags = settings::builder();
        flags.set("use_colocated_libcalls", "false").unwrap();
        flags.set("is_pic", "false").unwrap();
        let isa = cranelift_native::builder()
            .unwrap()
            .finish(settings::Flags::new(flags))
            .unwrap();
        let mut jb = JITBuilder::with_isa(isa, default_libcall_names());
        jb.symbol(
            NATIVE_RELEASE_AT_ZERO_SYMBOL,
            native_release_at_zero as *const u8,
        );
        let mut module = JITModule::new(jb);

        let ptr_ty = module.target_config().pointer_type();
        let mut release_sig = module.make_signature();
        release_sig.params.push(AbiParam::new(ptr_ty));
        let release_id = module
            .declare_function(NATIVE_RELEASE_AT_ZERO_SYMBOL, Linkage::Import, &release_sig)
            .unwrap();

        let mut sig = module.make_signature();
        sig.params.push(AbiParam::new(types::I64));
        let seq_id = module
            .declare_function("rc_seq", Linkage::Export, &sig)
            .unwrap();

        let mut ctx = module.make_context();
        ctx.func.signature = sig;
        let mut fbc = FunctionBuilderContext::new();
        {
            let mut b = FunctionBuilder::new(&mut ctx.func, &mut fbc);
            let entry = b.create_block();
            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            b.seal_block(entry);
            let bits = b.block_params(entry)[0];
            let release = module.declare_func_in_func(release_id, b.func);
            emit(&mut b, bits, release);
            b.ins().return_(&[]);
            b.finalize();
        }
        module.define_function(seq_id, &mut ctx).unwrap();
        module.clear_context(&mut ctx);
        module.finalize_definitions().unwrap();
        let code = module.get_finalized_function(seq_id);
        // SAFETY: the finalized function has exactly this signature.
        (module, unsafe {
            std::mem::transmute::<*const u8, unsafe extern "C" fn(u64)>(code)
        })
    }

    fn jit_drop_gate() -> (JITModule, unsafe extern "C" fn(u64)) {
        jit_rc_seq(emit_dynamic_drop)
    }

    fn jit_dup_gate() -> (JITModule, unsafe extern "C" fn(u64)) {
        jit_rc_seq(|b, bits, _release| emit_dynamic_dup(b, bits))
    }

    #[test]
    fn ignores_immediates_and_immortal_values() {
        let (_m, drop_gate) = jit_drop_gate();
        take_freed_objects();

        // Immediates: not heap, must not be dereferenced.
        for v in [Value::small_int(7), Value::small_int(-1), Value::nil()] {
            unsafe { drop_gate(v.to_bits()) };
        }

        // A frozen BigInt has no refcount slot.
        use crate::frozen::FrozenArea;
        use std::sync::Arc;
        let area = Arc::new(FrozenArea::new());
        let mut b = area.builder();
        let frozen = b.int(i64::MAX).into_value();
        assert!(frozen.is_heap() && frozen.is_immortal());
        unsafe { drop_gate(frozen.to_bits()) };

        assert_eq!(take_freed_objects(), 0);
    }

    #[test]
    fn decrements_and_frees_at_zero_via_release_at_zero() {
        let (_m, drop_gate) = jit_drop_gate();
        let mut h = ProcHeap::new();
        let v = Value::int_in(&mut h, i64::MAX); // mortal BigInt box, rc = 1
        assert!(v.is_heap() && !v.is_immortal());
        let bits = v.to_bits();
        // SAFETY: mortal heap object with a refcount slot.
        unsafe { rc_increment(v.heap_obj()) }; // rc = 2

        take_freed_objects();
        unsafe { drop_gate(bits) }; // 2 -> 1: still live
        assert_eq!(take_freed_objects(), 0);
        assert_eq!(v.as_int(), Some(i64::MAX));

        unsafe { drop_gate(bits) }; // 1 -> 0: freed through release_at_zero
        assert_eq!(take_freed_objects(), 1);
        std::mem::forget(v); // the gate already released the last reference
    }

    #[test]
    fn saturated_count_never_decrements() {
        let (_m, drop_gate) = jit_drop_gate();
        let mut h = ProcHeap::new();
        let v = Value::int_in(&mut h, i64::MAX);
        // SAFETY: mortal heap object with a refcount slot.
        unsafe { *rc_slot(v.heap_obj()) = u64::MAX };

        take_freed_objects();
        unsafe { drop_gate(v.to_bits()) };
        assert_eq!(take_freed_objects(), 0);
        // SAFETY: still live — saturated counts are permanently live.
        unsafe { assert_eq!(*rc_slot(v.heap_obj()), u64::MAX) };
        std::mem::forget(v); // saturated: intentionally permanent
    }

    #[test]
    fn dup_ignores_immediates_and_immortal_values() {
        let (_m, dup_gate) = jit_dup_gate();
        take_freed_objects();

        // Immediates: not heap, must not be dereferenced.
        for v in [Value::small_int(7), Value::small_int(-1), Value::nil()] {
            unsafe { dup_gate(v.to_bits()) };
        }

        // A frozen value has no refcount slot.
        use crate::frozen::FrozenArea;
        use std::sync::Arc;
        let area = Arc::new(FrozenArea::new());
        let mut b = area.builder();
        let frozen = b.int(i64::MAX).into_value();
        assert!(frozen.is_heap() && frozen.is_immortal());
        unsafe { dup_gate(frozen.to_bits()) };
        assert_eq!(frozen.as_int(), Some(i64::MAX));

        assert_eq!(take_freed_objects(), 0);
    }

    #[test]
    fn dup_matches_rc_increment_bit_for_bit() {
        let (_m, dup_gate) = jit_dup_gate();
        let mut h = ProcHeap::new();

        // Twin objects: one bumped by the JIT-compiled dup, the other by the
        // interpreter's `rc_increment`. Counts must match, saturation included.
        let starts = [1u64, 2, 5, u64::MAX - 1, u64::MAX];
        take_freed_objects();
        for start in starts {
            let native = Value::int_in(&mut h, i64::MAX);
            let oracle = Value::int_in(&mut h, i64::MAX);
            // SAFETY: both are live mortal heap objects with rc slots.
            unsafe {
                *rc_slot(native.heap_obj()) = start;
                *rc_slot(oracle.heap_obj()) = start;
                dup_gate(native.to_bits());
                rc_increment(oracle.heap_obj());
                assert_eq!(
                    *rc_slot(native.heap_obj()),
                    *rc_slot(oracle.heap_obj()),
                    "start = {start}"
                );
                // Restore rc 1 so the `Value` drops release cleanly.
                *rc_slot(native.heap_obj()) = 1;
                *rc_slot(oracle.heap_obj()) = 1;
            }
        }
        // Every twin was released by its `Value` drop: dup left no leak.
        assert_eq!(take_freed_objects(), 2 * starts.len() as u64);
    }

    fn jit_proven_drop() -> (JITModule, unsafe extern "C" fn(u64)) {
        jit_rc_seq(|b, bits, release| emit_drop(b, bits, release, RcGate::ProvenHeap))
    }

    fn jit_proven_dup() -> (JITModule, unsafe extern "C" fn(u64)) {
        jit_rc_seq(|b, bits, _release| emit_dup(b, bits, RcGate::ProvenHeap))
    }

    /// An immortal heap word has no rc prefix, so the proven-heap gate's
    /// remaining bit-0 test must keep both sequences off its rc slot.
    #[test]
    fn proven_heap_gate_skips_immortal_values() {
        let (_m1, drop_gate) = jit_proven_drop();
        let (_m2, dup_gate) = jit_proven_dup();
        use crate::frozen::FrozenArea;
        use std::sync::Arc;
        let area = Arc::new(FrozenArea::new());
        let mut b = area.builder();
        let frozen = b.str("proven-heap").into_value();
        assert!(frozen.is_heap() && frozen.is_immortal());

        take_freed_objects();
        unsafe { drop_gate(frozen.to_bits()) };
        unsafe { dup_gate(frozen.to_bits()) };
        assert_eq!(frozen.as_str(), Some("proven-heap"));
        assert_eq!(take_freed_objects(), 0);
    }

    /// On a mortal heap cell the proven-heap sequences count exactly like
    /// the dynamic ones.
    #[test]
    fn proven_heap_gate_counts_like_dynamic() {
        let (_m1, drop_gate) = jit_proven_drop();
        let (_m2, dup_gate) = jit_proven_dup();
        let mut h = ProcHeap::new();
        let v = Value::str_in(&mut h, "cell"); // mortal, rc = 1
        assert!(v.is_heap() && !v.is_immortal());
        let bits = v.to_bits();

        take_freed_objects();
        unsafe { dup_gate(bits) }; // 1 -> 2
        // SAFETY: live mortal heap object with an rc slot.
        unsafe { assert_eq!(*rc_slot(v.heap_obj()), 2) };
        unsafe { drop_gate(bits) }; // 2 -> 1
        assert_eq!(take_freed_objects(), 0);
        assert_eq!(v.as_str(), Some("cell"));
        unsafe { drop_gate(bits) }; // 1 -> 0: freed through release_at_zero
        assert_eq!(take_freed_objects(), 1);
        std::mem::forget(v); // the gate already released the last reference
    }
}
