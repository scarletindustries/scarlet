//! Bit-for-bit interpreter frame layout for the native (Cranelift) backend.
//!
//! Compiled code keeps the interpreter's frame layout exactly: locals live in
//! process-stack slots at `frame.base_slot`, and registers hold values only
//! within a function. At every call site and reduction checkpoint
//! `(stack, frames)` must be bit-identical to the interpreter's state at that
//! bytecode ip — that is what keeps suspension, migration, per-function
//! fallback and the interpreter-as-oracle working.
//!
//! This module holds the CLIF sequences that read and write frame slots, and
//! the NaN-box transitions between a slot's value word and the raw register
//! payload. Which slot each `LocalId` owns comes from [`FrameLayout`], which
//! `emit` records while lowering the same body to bytecode, so the two
//! backends cannot disagree.
//!
//! Two rules a caller must respect:
//!
//! - A loaded slot word is a **borrow**. Anything that outlives the expression
//!   (a callee's argument, a stored slot, a returned value) must take its own
//!   reference via [`emit_retain`].
//! - An Int-typed slot may hold a spilled `BigInt`, so typed code must never
//!   assume the immediate form.
//!
//! The frame-base pointer handed to [`FrameSlots`] goes stale as soon as the
//! value stack can reallocate (any runtime call that pushes). Re-deriving it
//! is the calling convention's job, not this module's.

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{self, InstBuilder, MemFlagsData, types};
use cranelift_frontend::FunctionBuilder;

use super::LocalId;
use super::emit::FrameLayout;
use super::native_rc::{emit_dynamic_drop, emit_dynamic_dup};
use crate::bytecode::Value;
use crate::bytecode::value::NATIVE_PTR_MASK;

/// Symbol the JIT finalize step registers the Int spill shim under
/// (`crates/al`'s `al_shim_int_box`: `extern "C" fn(vmx, i64) -> u64`).
pub const NATIVE_INT_BOX_SYMBOL: &str = "al_shim_int_box";

/// NaN-box layout facts compiled code bakes into instruction immediates.
///
/// Derived through `Value`'s public constructors, not by re-spelling the
/// private header constants, so the encoding cannot drift between backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValueBits {
    /// Small-int header; also the zero-fill word `enter_frame!` writes into
    /// non-argument slots.
    pub int_header: u64,
    /// `(bits & header_mask) == int_header` is `is_small_int`.
    pub header_mask: u64,
    /// Selects the 48-bit immediate payload.
    pub payload_mask: u64,
    /// `Value::bool(false)`, also the Bool header: `bool_false | flag` boxes.
    pub bool_false: u64,
    /// `Value::bool(true)`.
    pub bool_true: u64,
    /// `Value::nil()`.
    pub nil: u64,
}

/// The process-wide [`ValueBits`], computed from `Value`'s public encoding.
pub fn value_bits() -> ValueBits {
    let int_header = Value::small_int(0).to_bits();
    let payload_mask = Value::small_int(-1).to_bits() ^ int_header;
    ValueBits {
        int_header,
        header_mask: !payload_mask,
        payload_mask,
        bool_false: Value::bool(false).to_bits(),
        bool_true: Value::bool(true).to_bits(),
        nil: Value::nil().to_bits(),
    }
}

/// The backend addressed a local `emit` elided (const alias / stack temp), so
/// no frame slot exists. The coverage gate must keep such locals in registers;
/// reaching this is a backend bug.
#[allow(clippy::panic)]
#[cold]
#[inline(never)]
fn slotless_local(id: LocalId) -> ! {
    panic!(
        "internal compiler error: native backend addressed local {id}, which owns no \
         frame slot. Please report this as a compiler bug."
    )
}

/// Frame-slot access: local `id`'s word lives at `base + 8 * layout.slot(id)`,
/// where `base` is `&mut stack[frame.base_slot]` for the running frame.
///
/// `base` goes stale whenever the value stack may reallocate, so the body
/// emitter re-derives it after every pushing call and builds a fresh
/// `FrameSlots`. Cheap and `Copy` for that reason.
#[derive(Clone, Copy)]
pub struct FrameSlots<'a> {
    layout: &'a FrameLayout,
    base: ir::Value,
}

impl<'a> FrameSlots<'a> {
    pub fn new(layout: &'a FrameLayout, base: ir::Value) -> FrameSlots<'a> {
        FrameSlots { layout, base }
    }

    /// The slot `id` occupies, aborting on an elided local (backend bug).
    pub fn slot_of(&self, id: LocalId) -> i32 {
        match self.layout.slot(id) {
            Some(s) => s,
            None => slotless_local(id),
        }
    }

    /// Read `stack[base_slot + slot]`, borrowed (see the module docs).
    pub fn load_slot(&self, b: &mut FunctionBuilder, slot: i32) -> ir::Value {
        b.ins()
            .load(types::I64, MemFlagsData::trusted(), self.base, slot * 8)
    }

    /// [`FrameSlots::load_slot`] by `LocalId`.
    pub fn load(&self, b: &mut FunctionBuilder, id: LocalId) -> ir::Value {
        self.load_slot(b, self.slot_of(id))
    }

    /// Write `stack[base_slot + slot] = bits` with `StoreLocal`'s semantics:
    /// release the old word, then store. `bits` must carry an owned reference
    /// — the slot takes it.
    ///
    /// `release_at_zero` is a declared reference to the
    /// [`NATIVE_RELEASE_AT_ZERO_SYMBOL`](crate::bytecode::value::NATIVE_RELEASE_AT_ZERO_SYMBOL)
    /// import. The release gate branches, so the builder ends up in a fresh
    /// sealed block; codegen continues there.
    pub fn store_slot(
        &self,
        b: &mut FunctionBuilder,
        slot: i32,
        bits: ir::Value,
        release_at_zero: ir::FuncRef,
    ) {
        let old = self.load_slot(b, slot);
        emit_dynamic_drop(b, old, release_at_zero);
        self.store_slot_no_release(b, slot, bits);
    }

    /// [`FrameSlots::store_slot`] by `LocalId`.
    pub fn store(
        &self,
        b: &mut FunctionBuilder,
        id: LocalId,
        bits: ir::Value,
        release_at_zero: ir::FuncRef,
    ) {
        self.store_slot(b, self.slot_of(id), bits, release_at_zero)
    }

    /// Raw slot write, no release of the old word. Correct **only** when the
    /// slot provably still holds an immediate (the `enter_frame!` zero-fill, a
    /// Bool/Nil, an unspilled Int). A self-tail loop re-stores its slots over
    /// possibly-heap words every iteration and must use the releasing store.
    pub fn store_slot_no_release(&self, b: &mut FunctionBuilder, slot: i32, bits: ir::Value) {
        b.ins()
            .store(MemFlagsData::trusted(), bits, self.base, slot * 8);
    }

    /// [`FrameSlots::store_slot_no_release`] by `LocalId`.
    pub fn store_no_release(&self, b: &mut FunctionBuilder, id: LocalId, bits: ir::Value) {
        self.store_slot_no_release(b, self.slot_of(id), bits)
    }
}

/// NaN-box an `i64` known to be in the 47-bit immediate range. Arithmetic
/// results that may leave the range must use [`box_int`].
pub fn box_small_int(b: &mut FunctionBuilder, facts: &ValueBits, payload: ir::Value) -> ir::Value {
    let masked = b.ins().band_imm(payload, facts.payload_mask as i64);
    b.ins().bor_imm(masked, facts.int_header as i64)
}

/// Sign-extend a small-int word's 48-bit payload. Meaningful only when the
/// word really is a small-int immediate.
pub fn unbox_small_int(b: &mut FunctionBuilder, bits: ir::Value) -> ir::Value {
    let shifted = b.ins().ishl_imm(bits, 16);
    b.ins().sshr_imm(shifted, 16)
}

/// Decode an Int-typed value word to the full `i64` — `as_int_typed` inlined.
/// A spilled `BigInt` takes the cold branch.
///
/// Terminates the current block; on return the builder sits in a sealed merge
/// block whose parameter is the decoded `i64`.
pub fn unbox_int(b: &mut FunctionBuilder, facts: &ValueBits, bits: ir::Value) -> ir::Value {
    let big = b.create_block();
    let merge = b.create_block();
    b.append_block_param(merge, types::I64);
    b.set_cold_block(big);

    let hdr = b.ins().band_imm(bits, facts.header_mask as i64);
    let is_small = b.ins().icmp_imm(IntCC::Equal, hdr, facts.int_header as i64);
    let small = unbox_small_int(b, bits);
    b.ins().brif(is_small, merge, &[small.into()], big, &[]);
    b.seal_block(big);

    b.switch_to_block(big);
    let obj = b.ins().band_imm(bits, NATIVE_PTR_MASK as i64);
    let payload = b.ins().load(types::I64, MemFlagsData::trusted(), obj, 8);
    b.ins().jump(merge, &[payload.into()]);
    b.seal_block(merge);

    b.switch_to_block(merge);
    b.block_params(merge)[0]
}

/// NaN-box a full-range `i64` with the interpreter's spill rule: inline in the
/// immediate range, otherwise a cold call to the `al_shim_int_box` import
/// ([`NATIVE_INT_BOX_SYMBOL`]). The returned word carries one owned reference
/// either way.
///
/// `int_box`'s signature is `(i64 vmx, i64 payload) -> i64 bits`, `vmx` being
/// the opaque VM pointer the entry received. Terminates the current block; on
/// return the builder sits in a sealed merge block holding the boxed word.
pub fn box_int(
    b: &mut FunctionBuilder,
    facts: &ValueBits,
    vmx: ir::Value,
    payload: ir::Value,
    int_box: ir::FuncRef,
) -> ir::Value {
    let spill = b.create_block();
    let merge = b.create_block();
    b.append_block_param(merge, types::I64);
    b.set_cold_block(spill);

    // `fits_small_int(i)` == "the 48-bit sign-extension round-trips".
    let trunc = unbox_small_int(b, payload);
    let fits = b.ins().icmp(IntCC::Equal, trunc, payload);
    let fast = box_small_int(b, facts, payload);
    b.ins().brif(fits, merge, &[fast.into()], spill, &[]);
    b.seal_block(spill);

    b.switch_to_block(spill);
    let call = b.ins().call(int_box, &[vmx, payload]);
    let boxed = b.inst_results(call)[0];
    b.ins().jump(merge, &[boxed.into()]);
    b.seal_block(merge);

    b.switch_to_block(merge);
    b.block_params(merge)[0]
}

/// NaN-box an `i8` flag (an `icmp` result) as a Bool value word.
pub fn box_bool(b: &mut FunctionBuilder, facts: &ValueBits, flag: ir::Value) -> ir::Value {
    let wide = b.ins().uextend(types::I64, flag);
    b.ins().bor_imm(wide, facts.bool_false as i64)
}

/// Whether a Bool value word is `true`. Total on Bool-typed values, but not
/// the interpreter's `is_truthy` (which raises on non-Bool), so the coverage
/// gate must have proven the type first.
pub fn bool_is_true(b: &mut FunctionBuilder, facts: &ValueBits, bits: ir::Value) -> ir::Value {
    b.ins().icmp_imm(IntCC::Equal, bits, facts.bool_true as i64)
}

/// Take one owned reference to a value word. Emitted where compiled code hands
/// a borrowed register copy to an ownership-taking consumer while the frame
/// slot keeps its own reference.
///
/// Terminates the current block; on return the builder sits in a fresh sealed
/// block where codegen continues.
pub fn emit_retain(b: &mut FunctionBuilder, bits: ir::Value) {
    emit_dynamic_dup(b, bits);
}

// Tests drive JIT-compiled code and read raw refcount words.
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
        NATIVE_RELEASE_AT_ZERO_SYMBOL, native_release_at_zero, rc_slot, take_freed_objects,
    };
    use crate::heap::ProcHeap;

    /// Stand-in for the Int spill shim: the production `al_shim_int_box` also
    /// reduces to `Value::int_in` against the process heap. `vmx` is unused
    /// here but the generated code must still thread it.
    unsafe extern "C" fn test_int_box(_vmx: i64, i: i64) -> u64 {
        let mut h = ProcHeap::new();
        std::mem::ManuallyDrop::new(Value::int_in(&mut h, i)).to_bits()
    }

    /// JIT one exported function built by `build`, which receives declared
    /// `FuncRef`s for `release_at_zero` and the Int spill shim.
    fn jit(
        params: usize,
        returns: usize,
        build: impl FnOnce(&mut FunctionBuilder, ir::FuncRef, ir::FuncRef),
    ) -> (JITModule, *const u8) {
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
        jb.symbol(NATIVE_INT_BOX_SYMBOL, test_int_box as *const u8);
        let mut module = JITModule::new(jb);

        let ptr_ty = module.target_config().pointer_type();
        let mut release_sig = module.make_signature();
        release_sig.params.push(AbiParam::new(ptr_ty));
        let release_id = module
            .declare_function(NATIVE_RELEASE_AT_ZERO_SYMBOL, Linkage::Import, &release_sig)
            .unwrap();
        let mut box_sig = module.make_signature();
        box_sig.params.push(AbiParam::new(types::I64));
        box_sig.params.push(AbiParam::new(types::I64));
        box_sig.returns.push(AbiParam::new(types::I64));
        let box_id = module
            .declare_function(NATIVE_INT_BOX_SYMBOL, Linkage::Import, &box_sig)
            .unwrap();

        let mut sig = module.make_signature();
        for _ in 0..params {
            sig.params.push(AbiParam::new(types::I64));
        }
        for _ in 0..returns {
            sig.returns.push(AbiParam::new(types::I64));
        }
        let fn_id = module
            .declare_function("under_test", Linkage::Export, &sig)
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
            let release = module.declare_func_in_func(release_id, b.func);
            let int_box = module.declare_func_in_func(box_id, b.func);
            build(&mut b, release, int_box);
            b.finalize();
        }
        module.define_function(fn_id, &mut ctx).unwrap();
        module.clear_context(&mut ctx);
        module.finalize_definitions().unwrap();
        let code = module.get_finalized_function(fn_id);
        (module, code)
    }

    /// The base pointer generated code addresses slots against.
    fn frame_base(frame: &mut [Value]) -> i64 {
        frame.as_mut_ptr() as i64
    }

    fn layout_with_slots(n: u32) -> FrameLayout {
        // Params always own slots 0.., which is all these tests address.
        use crate::core_ir::testkit;
        use crate::core_ir::{Atom, CoreExpr};
        use crate::typed_ir::RTy;
        let params = (0..n).map(|i| testkit::bind(i, RTy(0))).collect();
        let f = testkit::func(params, CoreExpr::Tail(Atom::Local(LocalId(0))), RTy(0));
        let mut ctx = testkit::ctx(None);
        crate::core_ir::emit::emit(&f, &mut ctx).layout
    }

    #[test]
    fn bit_facts_match_the_value_encoding() {
        let facts = value_bits();
        for i in [0i64, 1, -1, 42, -42, (1 << 47) - 1, -(1 << 47)] {
            let bits = Value::small_int(i).to_bits();
            assert_eq!(bits & facts.header_mask, facts.int_header, "{i}");
            assert_eq!(((bits & facts.payload_mask) as i64) << 16 >> 16, i);
        }
        // A spilled BigInt fails the small-int header test.
        let mut h = ProcHeap::new();
        let big = Value::int_in(&mut h, i64::MAX);
        assert_ne!(big.to_bits() & facts.header_mask, facts.int_header);
        assert_eq!(facts.bool_true, facts.bool_false | 1);
        assert_eq!(Value::nil().to_bits(), facts.nil);
    }

    #[test]
    fn load_reads_the_interpreter_slot_word() {
        let layout = layout_with_slots(3);
        let (_m, code) = jit(1, 1, |b, _release, _box| {
            let base = b.block_params(b.current_block().unwrap())[0];
            let slots = FrameSlots::new(&layout, base);
            let v = slots.load(b, LocalId(2));
            b.ins().return_(&[v]);
        });
        let f: extern "C" fn(i64) -> u64 = unsafe { std::mem::transmute(code) };

        let mut frame = vec![
            Value::small_int(10),
            Value::small_int(20),
            Value::small_int(30),
        ];
        let got = f(frame_base(&mut frame));
        assert_eq!(got, Value::small_int(30).to_bits());
        // Borrow-only: the frame still owns its words untouched.
        assert_eq!(frame[2].to_bits(), got);
    }

    #[test]
    fn store_matches_store_local_including_the_release_of_the_old_word() {
        let layout = layout_with_slots(2);
        let (_m, code) = jit(2, 0, |b, release, _box| {
            let block = b.current_block().unwrap();
            let (base, bits) = (b.block_params(block)[0], b.block_params(block)[1]);
            let slots = FrameSlots::new(&layout, base);
            slots.store(b, LocalId(1), bits, release);
            b.ins().return_(&[]);
        });
        let f: extern "C" fn(i64, u64) = unsafe { std::mem::transmute(code) };

        // Old word is a mortal BigInt with rc 1, so the store must free it.
        let mut h = ProcHeap::new();
        let old = Value::int_in(&mut h, i64::MAX);
        let mut frame = vec![Value::small_int(0), old];
        take_freed_objects();
        f(frame_base(&mut frame), Value::small_int(7).to_bits());
        assert_eq!(take_freed_objects(), 1, "old BigInt must be freed");
        assert_eq!(frame[1].to_bits(), Value::small_int(7).to_bits());

        // Releasing an immediate is a dynamic no-op.
        take_freed_objects();
        f(frame_base(&mut frame), Value::small_int(9).to_bits());
        assert_eq!(take_freed_objects(), 0);
        assert_eq!(frame[1].to_bits(), Value::small_int(9).to_bits());

        // Forget, not drop: the BigInt above was already freed.
        std::mem::forget(frame);
    }

    #[test]
    fn store_takes_ownership_the_slot_keeps_one_reference() {
        let layout = layout_with_slots(1);
        let (_m, code) = jit(2, 0, |b, release, _box| {
            let block = b.current_block().unwrap();
            let (base, bits) = (b.block_params(block)[0], b.block_params(block)[1]);
            let slots = FrameSlots::new(&layout, base);
            slots.store(b, LocalId(0), bits, release);
            b.ins().return_(&[]);
        });
        let f: extern "C" fn(i64, u64) = unsafe { std::mem::transmute(code) };

        let mut h = ProcHeap::new();
        let v = Value::int_in(&mut h, i64::MAX); // rc = 1, transferred below
        let bits = std::mem::ManuallyDrop::new(v).to_bits();
        let mut frame = vec![Value::small_int(0)];
        f(frame_base(&mut frame), bits);
        assert_eq!(frame[0].to_bits(), bits);
        // SAFETY: the object is live; the frame holds its one reference.
        unsafe {
            assert_eq!(*rc_slot((bits & NATIVE_PTR_MASK) as *const u64), 1);
        }
        take_freed_objects();
        drop(frame);
        assert_eq!(take_freed_objects(), 1);
    }

    #[test]
    fn unbox_int_decodes_small_and_spilled_words() {
        let facts = value_bits();
        let (_m, code) = jit(1, 1, |b, _release, _box| {
            let bits = b.block_params(b.current_block().unwrap())[0];
            let i = unbox_int(b, &facts, bits);
            b.ins().return_(&[i]);
        });
        let f: extern "C" fn(u64) -> i64 = unsafe { std::mem::transmute(code) };

        for i in [0i64, 1, -1, 42, (1 << 47) - 1, -(1 << 47)] {
            assert_eq!(f(Value::small_int(i).to_bits()), i);
        }
        let mut h = ProcHeap::new();
        for i in [(1i64 << 47), -(1i64 << 47) - 1, i64::MAX, i64::MIN] {
            let big = Value::int_in(&mut h, i);
            assert_eq!(f(big.to_bits()), i);
        }
    }

    #[test]
    fn box_int_matches_push_int_boxing_spill_included() {
        let facts = value_bits();
        let (_m, code) = jit(2, 1, |b, _release, int_box| {
            let block = b.current_block().unwrap();
            let (vmx, payload) = (b.block_params(block)[0], b.block_params(block)[1]);
            let bits = box_int(b, &facts, vmx, payload, int_box);
            b.ins().return_(&[bits]);
        });
        let f: extern "C" fn(i64, i64) -> u64 = unsafe { std::mem::transmute(code) };

        for i in [0i64, 5, -5, (1 << 47) - 1, -(1 << 47)] {
            assert_eq!(f(0, i), Value::small_int(i).to_bits(), "{i}");
        }
        for i in [(1i64 << 47), i64::MAX, i64::MIN] {
            let bits = f(0, i);
            let v = unsafe { Value::from_bits(bits) };
            assert!(v.is_heap() && !v.is_immortal(), "{i} must spill");
            assert_eq!(v.as_int(), Some(i));
        }
    }

    #[test]
    fn bool_boxing_round_trips_the_two_words() {
        let facts = value_bits();
        let (_m, code) = jit(2, 1, |b, _release, _box| {
            let block = b.current_block().unwrap();
            let (a, bb) = (b.block_params(block)[0], b.block_params(block)[1]);
            // Boxed, decoded, re-boxed: exercises both directions.
            let flag = b.ins().icmp(IntCC::SignedLessThan, a, bb);
            let boxed = box_bool(b, &facts, flag);
            let truth = bool_is_true(b, &facts, boxed);
            let reboxed = box_bool(b, &facts, truth);
            b.ins().return_(&[reboxed]);
        });
        let f: extern "C" fn(i64, i64) -> u64 = unsafe { std::mem::transmute(code) };
        assert_eq!(f(1, 2), Value::bool(true).to_bits());
        assert_eq!(f(2, 1), Value::bool(false).to_bits());
        assert_eq!(f(1, 1), Value::bool(false).to_bits());
    }

    #[test]
    fn retain_matches_owned_from_bits() {
        let (_m, code) = jit(1, 0, |b, _release, _box| {
            let bits = b.block_params(b.current_block().unwrap())[0];
            emit_retain(b, bits);
            b.ins().return_(&[]);
        });
        let f: extern "C" fn(u64) = unsafe { std::mem::transmute(code) };

        // Immediates must never be dereferenced.
        f(Value::small_int(7).to_bits());
        f(Value::nil().to_bits());
        f(Value::bool(true).to_bits());

        let mut h = ProcHeap::new();
        let v = Value::int_in(&mut h, i64::MAX);
        f(v.to_bits());
        // SAFETY: live mortal object; retain must have bumped its count.
        unsafe {
            assert_eq!(*rc_slot(v.heap_obj()), 2);
            // Saturated counts stay saturated (permanently live).
            *rc_slot(v.heap_obj()) = u64::MAX;
            f(v.to_bits());
            assert_eq!(*rc_slot(v.heap_obj()), u64::MAX);
        }
        std::mem::forget(v); // saturated: intentionally permanent
    }

    /// `let %2 = AddInt(%0, %1)` compiled here, run against the same frame the
    /// interpreter sequence `PushLocal 0; PushLocal 1; AddInt; StoreLocal 2`
    /// produces. Frame words must come out bit-identical (immediates) or
    /// value-identical with matching allocation traffic (spills).
    #[test]
    fn store_through_add_matches_the_interpreter_frame() {
        let facts = value_bits();
        let layout = layout_with_slots(3);
        let (_m, code) = jit(2, 0, |b, release, int_box| {
            let block = b.current_block().unwrap();
            let (base, vmx) = (b.block_params(block)[0], b.block_params(block)[1]);
            let slots = FrameSlots::new(&layout, base);
            let a_bits = slots.load(b, LocalId(0));
            let b_bits = slots.load(b, LocalId(1));
            let a = unbox_int(b, &facts, a_bits);
            let bv = unbox_int(b, &facts, b_bits);
            let sum = b.ins().iadd(a, bv); // wrapping, like `wrapping_add`
            let boxed = box_int(b, &facts, vmx, sum, int_box);
            slots.store(b, LocalId(2), boxed, release);
            b.ins().return_(&[]);
        });
        let f: extern "C" fn(i64, i64) = unsafe { std::mem::transmute(code) };

        // The interpreter's effect on the frame, in Rust.
        let interp = |frame: &mut [Value]| {
            let a = frame[0].clone();
            let b = frame[1].clone();
            let r = a.as_int_typed().wrapping_add(b.as_int_typed());
            let mut h = ProcHeap::new();
            frame[2] = Value::int_in(&mut h, r);
        };

        // small + small -> small: strict bit equality of the whole frame.
        let mk = |a: i64, b: i64| {
            vec![
                Value::small_int(a),
                Value::small_int(b),
                Value::small_int(0), // the enter_frame! zero-fill
            ]
        };
        let (mut native, mut oracle) = (mk(20, 22), mk(20, 22));
        take_freed_objects();
        f(frame_base(&mut native), 0);
        let native_frees = take_freed_objects();
        interp(&mut oracle);
        let oracle_frees = take_freed_objects();
        assert_eq!(native_frees, oracle_frees);
        for (n, o) in native.iter().zip(oracle.iter()) {
            assert_eq!(n.to_bits(), o.to_bits());
        }

        // small + small -> spill: the two BigInt boxes are distinct
        // allocations, so compare decoded payloads.
        let (mut native, mut oracle) = (mk((1 << 47) - 1, 1), mk((1 << 47) - 1, 1));
        take_freed_objects();
        f(frame_base(&mut native), 0);
        let native_frees = take_freed_objects();
        interp(&mut oracle);
        let oracle_frees = take_freed_objects();
        assert_eq!(native_frees, oracle_frees);
        assert_eq!(native[2].as_int(), Some(1 << 47));
        assert_eq!(native[2].as_int(), oracle[2].as_int());
        assert!(native[2].is_heap() && oracle[2].is_heap());
        assert!(native[2].is_unique(), "spilled result owns rc 1");

        // Overwriting a spilled word releases it: both backends free one.
        let mut native2 = vec![
            Value::small_int(1),
            Value::small_int(2),
            std::mem::replace(&mut native[2], Value::small_int(0)),
        ];
        let mut oracle2 = vec![
            Value::small_int(1),
            Value::small_int(2),
            std::mem::replace(&mut oracle[2], Value::small_int(0)),
        ];
        take_freed_objects();
        f(frame_base(&mut native2), 0);
        let native_frees = take_freed_objects();
        interp(&mut oracle2);
        let oracle_frees = take_freed_objects();
        assert_eq!(native_frees, 1);
        assert_eq!(native_frees, oracle_frees);
        for (n, o) in native2.iter().zip(oracle2.iter()) {
            assert_eq!(n.to_bits(), o.to_bits());
        }
    }
}
