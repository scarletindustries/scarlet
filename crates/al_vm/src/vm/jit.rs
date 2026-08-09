//! JIT module construction and runtime-symbol resolution: the layering seam
//! between `al_core`'s CLIF construction and this crate's runtime.
//!
//! `al_core` depends on this crate, never the reverse, but compiled bodies
//! must call runtime services owned here. Runtime symbols therefore cross
//! the crate boundary **by name**: CLIF emitted anywhere declares them
//! `Linkage::Import` under the canonical names in [`runtime_symbols`], which
//! [`jit_module`] registers with the `JITBuilder`, and Cranelift binds them
//! during relocation in [`finalize_into`] — at finalize time, not Rust link
//! time. Adding a runtime call is one shim, one [`runtime_symbols`] entry,
//! one [`RuntimeFns`] import.
//!
//! One [`JITModule`] holds one executable mapping shared by every scheduler.
//! Published entry pointers are raw code addresses and processes migrate
//! across scheduler threads mid-flight, so the mapping must be immortal:
//! nothing calls `free_memory`, and dropping the module after
//! [`finalize_into`] leaks the code pages deliberately.

// Publishing a finalized code address as a typed `NativeEntry` transmutes a
// `*const u8`, justified by `finalize_into`'s signature check.
#![allow(unsafe_code)]

use crate::FuncIdx;
use crate::bytecode::NativeTable;
use crate::bytecode::value::{
    NATIVE_HOLLOW_FOR_REUSE_SYMBOL, NATIVE_RELEASE_AT_ZERO_SYMBOL, native_hollow_for_reuse,
    native_release_at_zero,
};
use cranelift_codegen::ir::{AbiParam, Signature, Type, types};
use cranelift_codegen::isa::CallConv;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{FuncId, Linkage, Module, ModuleError, default_libcall_names};

use super::{native, native_shims, perf_map};

/// Why JIT construction or publication failed. Every variant is recoverable
/// the same way: leave the [`NativeTable`] slots empty and interpret.
///
/// An unresolved runtime symbol is not one of these — cranelift-jit panics
/// inside `finalize_definitions` instead. The correspondence test in this
/// module keeps that path unreachable.
#[derive(Debug)]
pub enum JitError {
    /// The host has no Cranelift backend, or ISA construction failed.
    Host(String),
    /// Declaration, definition or finalization failed inside the module.
    /// Boxed because `ModuleError` is >100 bytes.
    Module(Box<ModuleError>),
    /// [`finalize_into`] was handed a function not declared with the
    /// [`crate::bytecode::NativeEntry`] signature; publishing it would type-confuse every
    /// caller of the entry table.
    EntrySignature { name: String },
}

impl std::fmt::Display for JitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JitError::Host(msg) => write!(f, "native backend unavailable on this host: {msg}"),
            JitError::Module(err) => write!(f, "native backend module error: {err}"),
            JitError::EntrySignature { name } => write!(
                f,
                "native entry {name:?} was not declared with the NativeEntry signature"
            ),
        }
    }
}

impl std::error::Error for JitError {}

impl From<ModuleError> for JitError {
    fn from(err: ModuleError) -> JitError {
        JitError::Module(Box::new(err))
    }
}

/// Every runtime symbol JIT-compiled code may reference, as
/// `(canonical name, shim address)` pairs. [`RuntimeFns::declare`]'s imports
/// resolve against these names. An unused entry is harmless; a declared name
/// missing here panics inside `finalize_definitions`.
pub fn runtime_symbols() -> Vec<(&'static str, *const u8)> {
    let mut syms = vec![
        (
            NATIVE_RELEASE_AT_ZERO_SYMBOL,
            native_release_at_zero as *const u8,
        ),
        (
            NATIVE_HOLLOW_FOR_REUSE_SYMBOL,
            native_hollow_for_reuse as *const u8,
        ),
    ];
    syms.extend(native_shims::shim_symbols());
    syms.extend(native::rt_symbols());
    syms
}

/// Build the one JIT module compiled bodies are defined into, with every
/// [`runtime_symbols`] pair pre-registered.
///
/// Errors only when the host has no Cranelift backend; the caller's fallback
/// is to interpret everything.
pub fn jit_module() -> Result<JITModule, JitError> {
    let mut flags = settings::builder();
    // JIT'd code and the shims sit at arbitrary addresses in one process, so
    // calls need absolute addresses rather than colocated PLT-style stubs.
    for (name, value) in [
        ("use_colocated_libcalls", "false"),
        ("is_pic", "false"),
        ("opt_level", "speed"),
        // Compiled frames live on 256K per-process stacks with one guard
        // page ([`super::stack`]). Without probing, a frame bigger than a
        // page does one `sub rsp, N` that steps clean over the guard and
        // writes into the next process's stack: silent corruption, no fault.
        ("enable_probestack", "true"),
        ("probestack_strategy", "inline"),
        // The context argument stays in the pinned register (r15/x21) and is
        // re-read at every use. A frame must never hold a scheduler-derived
        // word across a suspension point.
        ("enable_pinned_reg", "true"),
    ] {
        flags
            .set(name, value)
            .map_err(|e| JitError::Host(e.to_string()))?;
    }
    let isa = cranelift_native::builder()
        .map_err(|e| JitError::Host(e.to_string()))?
        .finish(settings::Flags::new(flags))
        .map_err(|e| JitError::Host(e.to_string()))?;
    let mut builder = JITBuilder::with_isa(isa, default_libcall_names());
    for (name, addr) in runtime_symbols() {
        builder.symbol(name, addr);
    }
    Ok(JITModule::new(builder))
}

/// The [`NativeEntry`] signature in CLIF terms: one pointer parameter, one
/// `i64` status return. Written once so body declarations and
/// [`finalize_into`]'s check cannot drift apart.
pub fn native_entry_signature(module: &JITModule) -> Signature {
    let mut sig = module.make_signature();
    // The tail calling convention is what admits `return_call`: an AL-level
    // tail transfer between compiled bodies stays in machine code instead of
    // bouncing through the trampoline. Rust never calls a tail-cc pointer
    // directly — every entry goes through [`entry_trampoline`].
    sig.call_conv = CallConv::Tail;
    sig.params
        .push(AbiParam::new(module.target_config().pointer_type()));
    sig.returns.push(AbiParam::new(types::I64));
    sig
}

/// Compile the one SystemV bridge the VM calls entries through:
/// `fn(ctx, entry) -> status`. Cranelift emits the callee-save spills the
/// tail-cc callee (which preserves nothing) requires, so the assembly bracket
/// in `bytecode::native` only has to keep the pinned register alive.
pub fn entry_trampoline(module: &mut JITModule) -> Result<FuncId, JitError> {
    use cranelift_codegen::ir::InstBuilder;
    use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
    let ptr = module.target_config().pointer_type();
    let mut sig = module.make_signature();
    sig.params.push(AbiParam::new(ptr)); // ctx
    sig.params.push(AbiParam::new(ptr)); // tail-cc entry to invoke
    sig.returns.push(AbiParam::new(types::I64));
    let id = module.declare_function("al_entry_trampoline", Linkage::Export, &sig)?;
    let mut ctx = module.make_context();
    ctx.func.signature = sig;
    let callee_sig = native_entry_signature(module);
    let mut fbc = FunctionBuilderContext::new();
    {
        let mut b = FunctionBuilder::new(&mut ctx.func, &mut fbc);
        let entry = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        b.seal_block(entry);
        let params = b.block_params(entry).to_vec();
        let sig_ref = b.import_signature(callee_sig);
        let call = b.ins().call_indirect(sig_ref, params[1], &[params[0]]);
        let status = b.inst_results(call)[0];
        b.ins().return_(&[status]);
        b.finalize();
    }
    module.define_function(id, &mut ctx)?;
    module.clear_context(&mut ctx);
    Ok(id)
}

/// The declared runtime imports, one `FuncId` per shim, ready for
/// `declare_func_in_func` at CLIF-emission sites. Cranelift dedupes repeat
/// declarations, so [`RuntimeFns::declare`] is idempotent per module.
pub struct RuntimeFns {
    /// `al_native_release_at_zero(obj)` — the cold free-at-zero call the
    /// inline drop gate branches to
    /// ([`native_rc::emit_dynamic_drop`](crate::native_rc)).
    pub release_at_zero: FuncId,
    /// `al_shim_int_box(vmx, i) -> bits` — box a full-range `i64`, spilling
    /// past the 47-bit immediate range.
    pub int_box: FuncId,
    /// `al_shim_int_unbox(bits) -> i` — decode an Int value that may be a
    /// `BigInt` box.
    pub int_unbox: FuncId,
    /// `al_shim_add_int_val(vmx, a, b) -> bits` — whole-op fallback add.
    pub add_int_val: FuncId,
    /// `al_shim_sub_int_val(vmx, a, b) -> bits`.
    pub sub_int_val: FuncId,
    /// `al_shim_mul_int_val(vmx, a, b) -> bits`.
    pub mul_int_val: FuncId,
    /// `al_shim_div_int_val(vmx, a, b) -> bits`.
    pub div_int_val: FuncId,
    /// `al_shim_mod_int_val(vmx, a, b) -> bits`.
    pub mod_int_val: FuncId,
    /// `al_shim_neg_int_val(vmx, a) -> bits`.
    pub neg_int_val: FuncId,
    /// `al_shim_div_int(a, b) -> q` — unboxed division with interpreter
    /// totality.
    pub div_int: FuncId,
    /// `al_shim_mod_int(a, b) -> r` — unboxed remainder with interpreter
    /// totality.
    pub mod_int: FuncId,
    /// `al_rt_enter_interp(vmx) -> status` — run the already-pushed callee
    /// frame under the interpreter's frame floor. Used directly only when the
    /// emitter has done its own frame handshake.
    pub enter_interp: FuncId,
    /// `al_rt_call(vmx, target, resume_ip, args, argc, out) -> status` —
    /// known call: frame handshake, entry checkpoint, table dispatch with
    /// frame-floor fallback, result to `out` on `Done`.
    pub rt_call: FuncId,
    /// `al_rt_tail_call(vmx, target, args, argc) -> status` — cross-function
    /// tail call. Compiled tail sites are `return al_rt_tail_call(...)`.
    pub rt_tail_call: FuncId,
    /// `al_rt_checkpoint(vmx) -> status` — the reds checkpoint at a
    /// self-tail-call back-edge (`Done` = keep looping, `Yield` = unwind
    /// with the frame resumable at ip 0).
    pub rt_checkpoint: FuncId,
    /// `al_rt_frame_base(vmx) -> ptr` — address of the top frame's slot 0;
    /// re-fetched after any stack-growing seam.
    pub rt_frame_base: FuncId,
    /// `al_rt_ret(vmx, result)` — the compiled epilogue: pop frame, truncate
    /// to its base, push the result.
    pub rt_ret: FuncId,
}

impl RuntimeFns {
    pub fn declare(module: &mut JITModule) -> Result<RuntimeFns, JitError> {
        let ptr = module.target_config().pointer_type();
        let i64t = types::I64;
        // A name declared here but missing from `runtime_symbols` would only
        // surface as a crash inside Cranelift at finalize. Check membership at
        // declare time instead, so drift is an error with the name in it.
        let registered: std::collections::HashSet<&'static str> =
            runtime_symbols().into_iter().map(|(n, _)| n).collect();
        let mut import =
            |name: &'static str, params: &[Type], ret: Option<Type>| -> Result<FuncId, JitError> {
                if !registered.contains(name) {
                    return Err(JitError::Host(format!(
                        "runtime helper {name} is declared but not in the symbol table"
                    )));
                }
                let mut sig = module.make_signature();
                sig.params.extend(params.iter().map(|&t| AbiParam::new(t)));
                sig.returns.extend(ret.map(AbiParam::new));
                Ok(module.declare_function(name, Linkage::Import, &sig)?)
            };
        Ok(RuntimeFns {
            release_at_zero: import(NATIVE_RELEASE_AT_ZERO_SYMBOL, &[ptr], None)?,
            int_box: import("al_shim_int_box", &[ptr, i64t], Some(i64t))?,
            int_unbox: import("al_shim_int_unbox", &[i64t], Some(i64t))?,
            add_int_val: import("al_shim_add_int_val", &[ptr, i64t, i64t], Some(i64t))?,
            sub_int_val: import("al_shim_sub_int_val", &[ptr, i64t, i64t], Some(i64t))?,
            mul_int_val: import("al_shim_mul_int_val", &[ptr, i64t, i64t], Some(i64t))?,
            div_int_val: import("al_shim_div_int_val", &[ptr, i64t, i64t], Some(i64t))?,
            mod_int_val: import("al_shim_mod_int_val", &[ptr, i64t, i64t], Some(i64t))?,
            neg_int_val: import("al_shim_neg_int_val", &[ptr, i64t], Some(i64t))?,
            div_int: import("al_shim_div_int", &[i64t, i64t], Some(i64t))?,
            mod_int: import("al_shim_mod_int", &[i64t, i64t], Some(i64t))?,
            enter_interp: import("al_rt_enter_interp", &[ptr], Some(i64t))?,
            rt_call: import("al_rt_call", &[ptr, i64t, i64t, ptr, i64t, ptr], Some(i64t))?,
            rt_tail_call: import("al_rt_tail_call", &[ptr, i64t, ptr, i64t], Some(i64t))?,
            rt_checkpoint: import("al_rt_checkpoint", &[ptr], Some(i64t))?,
            rt_frame_base: import("al_rt_frame_base", &[ptr], Some(ptr))?,
            rt_ret: import("al_rt_ret", &[ptr, i64t], None)?,
        })
    }
}

/// One compiled body handed to [`finalize_into`].
///
/// `func_id` must name a *defined* function, not a bare declaration:
/// construct a `JitDef` only after a successful `define_function`.
/// [`JITModule`] exposes no code-size accessor after finalization, so
/// `code_size` must be captured from the compiled context at that same point.
pub struct JitDef {
    pub fn_idx: FuncIdx,
    pub func_id: FuncId,
    /// Source-level function name; perf-map symbols are `al::<name>`.
    pub name: String,
    /// Finalized machine-code size in bytes.
    pub code_size: u32,
}

/// Resolve every pending relocation (where [`runtime_symbols`] names bind to
/// their shim addresses), then publish each def's code address into the
/// program's entry table, plus a [`perf_map`] line under `AL_PERF_MAP=1`.
///
/// Every declaration is checked against [`native_entry_signature`] first;
/// that check is what keeps the `*const u8` → [`NativeEntry`] transmute
/// sound for later `table.get` callers. It cannot see whether a declaration
/// carries a definition, so a declared-but-never-defined `func_id` panics in
/// `get_finalized_function`; [`JitDef`]'s contract rules that out.
///
/// The module must outlive the table's pointers, i.e. forever.
pub fn finalize_into(
    module: &mut JITModule,
    defs: &[JitDef],
    table: &NativeTable,
) -> Result<(), JitError> {
    let entry_sig = native_entry_signature(module);
    for def in defs {
        let decl = module.declarations().get_function_decl(def.func_id);
        if decl.signature != entry_sig {
            return Err(JitError::EntrySignature {
                name: decl.name.clone().unwrap_or_default(),
            });
        }
    }
    let tramp = entry_trampoline(module)?;
    module.finalize_definitions()?;
    table.set_trampoline(module.get_finalized_function(tramp));
    for def in defs {
        let code = module.get_finalized_function(def.func_id);
        table.set(def.fn_idx, code);
        perf_map::record(code as usize, def.code_size as usize, &def.name);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::bytecode::NativeStatus;
    use crate::bytecode::Value;
    use crate::bytecode::value::take_freed_objects;
    use crate::heap::ProcHeap;
    use crate::native_rc::emit_dynamic_drop;
    use crate::tivec::Idx;
    use cranelift_codegen::ir::InstBuilder;
    use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};

    use super::super::{VM, halt_test_vm};
    use super::*;

    /// Define one exported function whose body `build` writes, then finalize
    /// the module and hand back the code address.
    fn define_and_finalize(
        module: &mut JITModule,
        name: &str,
        sig: Signature,
        build: impl FnOnce(&mut JITModule, &mut FunctionBuilder),
    ) -> *const u8 {
        let id = module
            .declare_function(name, Linkage::Export, &sig)
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
            build(module, &mut b);
            b.finalize();
        }
        module.define_function(id, &mut ctx).unwrap();
        module.clear_context(&mut ctx);
        module.finalize_definitions().unwrap();
        module.get_finalized_function(id)
    }

    #[test]
    fn runtime_symbol_names_are_unique_and_all_declarable() {
        let syms = runtime_symbols();
        for (i, (name, addr)) in syms.iter().enumerate() {
            assert!(!addr.is_null(), "{name} has a null address");
            assert!(
                syms[..i].iter().all(|(n, _)| n != name),
                "duplicate runtime symbol {name}"
            );
        }
        // A declared name missing from `runtime_symbols` would only show up
        // as a panic inside `finalize_definitions`, so pin the whole ABI
        // chain here: the typed fn-pointer coercion turns a shim-signature
        // edit into a compile error, the CLIF types are checked against the
        // declaration, and the name must resolve to that exact address.
        let mut module = jit_module().unwrap();
        let fns = RuntimeFns::declare(&mut module).unwrap();
        let ptr = module.target_config().pointer_type();
        let i64t = types::I64;
        type BinValShim = unsafe extern "C" fn(*mut VM, u64, u64) -> u64;
        let release: unsafe extern "C" fn(*mut u64) = native_release_at_zero;
        let int_box: unsafe extern "C" fn(*mut VM, i64) -> u64 = native_shims::al_shim_int_box;
        let int_unbox: unsafe extern "C" fn(u64) -> i64 = native_shims::al_shim_int_unbox;
        let add_int_val: BinValShim = native_shims::al_shim_add_int_val;
        let sub_int_val: BinValShim = native_shims::al_shim_sub_int_val;
        let mul_int_val: BinValShim = native_shims::al_shim_mul_int_val;
        let div_int_val: BinValShim = native_shims::al_shim_div_int_val;
        let mod_int_val: BinValShim = native_shims::al_shim_mod_int_val;
        let neg_int_val: unsafe extern "C" fn(*mut VM, u64) -> u64 =
            native_shims::al_shim_neg_int_val;
        let div_int: extern "C" fn(i64, i64) -> i64 = native_shims::al_shim_div_int;
        let mod_int: extern "C" fn(i64, i64) -> i64 = native_shims::al_shim_mod_int;
        let enter_interp: unsafe extern "C" fn(*mut VM) -> NativeStatus =
            native::al_rt_enter_interp;
        let rt_call: unsafe extern "C" fn(
            *mut VM,
            i64,
            i64,
            *const u64,
            i64,
            *mut u64,
        ) -> NativeStatus = native::al_rt_call;
        let rt_tail_call: unsafe extern "C" fn(*mut VM, i64, *const u64, i64) -> NativeStatus =
            native::al_rt_tail_call;
        let rt_checkpoint: unsafe extern "C" fn(*mut VM) -> NativeStatus = native::al_rt_checkpoint;
        let rt_frame_base: unsafe extern "C" fn(*mut VM) -> *mut u64 = native::al_rt_frame_base;
        let rt_ret: unsafe extern "C" fn(*mut VM, u64) = native::al_rt_ret;
        let rows: [(FuncId, *const u8, Vec<Type>, Option<Type>); 17] = [
            (fns.release_at_zero, release as *const u8, vec![ptr], None),
            (
                fns.int_box,
                int_box as *const u8,
                vec![ptr, i64t],
                Some(i64t),
            ),
            (
                fns.int_unbox,
                int_unbox as *const u8,
                vec![i64t],
                Some(i64t),
            ),
            (
                fns.add_int_val,
                add_int_val as *const u8,
                vec![ptr, i64t, i64t],
                Some(i64t),
            ),
            (
                fns.sub_int_val,
                sub_int_val as *const u8,
                vec![ptr, i64t, i64t],
                Some(i64t),
            ),
            (
                fns.mul_int_val,
                mul_int_val as *const u8,
                vec![ptr, i64t, i64t],
                Some(i64t),
            ),
            (
                fns.div_int_val,
                div_int_val as *const u8,
                vec![ptr, i64t, i64t],
                Some(i64t),
            ),
            (
                fns.mod_int_val,
                mod_int_val as *const u8,
                vec![ptr, i64t, i64t],
                Some(i64t),
            ),
            (
                fns.neg_int_val,
                neg_int_val as *const u8,
                vec![ptr, i64t],
                Some(i64t),
            ),
            (
                fns.div_int,
                div_int as *const u8,
                vec![i64t, i64t],
                Some(i64t),
            ),
            (
                fns.mod_int,
                mod_int as *const u8,
                vec![i64t, i64t],
                Some(i64t),
            ),
            (
                fns.enter_interp,
                enter_interp as *const u8,
                vec![ptr],
                Some(i64t),
            ),
            (
                fns.rt_call,
                rt_call as *const u8,
                vec![ptr, i64t, i64t, ptr, i64t, ptr],
                Some(i64t),
            ),
            (
                fns.rt_tail_call,
                rt_tail_call as *const u8,
                vec![ptr, i64t, ptr, i64t],
                Some(i64t),
            ),
            (
                fns.rt_checkpoint,
                rt_checkpoint as *const u8,
                vec![ptr],
                Some(i64t),
            ),
            (
                fns.rt_frame_base,
                rt_frame_base as *const u8,
                vec![ptr],
                Some(ptr),
            ),
            (fns.rt_ret, rt_ret as *const u8, vec![ptr, i64t], None),
        ];
        for (id, addr, params, ret) in rows {
            let decl = module.declarations().get_function_decl(id);
            let name = decl.name.as_deref().expect("imports are named");
            let expected_params: Vec<AbiParam> = params.into_iter().map(AbiParam::new).collect();
            assert_eq!(
                decl.signature.params, expected_params,
                "{name}: declared CLIF params drifted from the shim's extern \"C\" ABI"
            );
            let expected_ret: Vec<AbiParam> = ret.map(AbiParam::new).into_iter().collect();
            assert_eq!(
                decl.signature.returns, expected_ret,
                "{name}: declared CLIF return drifted from the shim's extern \"C\" ABI"
            );
            let (_, registered) = syms
                .iter()
                .find(|(n, _)| *n == name)
                .unwrap_or_else(|| panic!("declared import {name} is not in runtime_symbols()"));
            assert_eq!(
                *registered, addr,
                "{name} resolves to a different function than the one whose ABI is pinned here"
            );
        }
    }

    /// CLIF calls `al_shim_int_box` by name, the registered symbol resolves
    /// at finalize, and the executed code spills like the interpreter does.
    #[test]
    fn by_name_import_resolves_to_the_registered_shim() {
        let mut module = jit_module().unwrap();
        let fns = RuntimeFns::declare(&mut module).unwrap();
        let ptr_ty = module.target_config().pointer_type();
        let mut sig = module.make_signature();
        sig.params.push(AbiParam::new(ptr_ty));
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I64));
        let code = define_and_finalize(&mut module, "box_caller", sig, |module, b| {
            let entry = b.current_block().unwrap();
            let vmx = b.block_params(entry)[0];
            let i = b.block_params(entry)[1];
            let int_box = module.declare_func_in_func(fns.int_box, b.func);
            let call = b.ins().call(int_box, &[vmx, i]);
            let bits = b.inst_results(call)[0];
            b.ins().return_(&[bits]);
        });
        // SAFETY: compiled with exactly this signature above.
        let box_caller = unsafe {
            std::mem::transmute::<*const u8, unsafe extern "C" fn(*mut VM, i64) -> u64>(code)
        };

        let mut vm = halt_test_vm();
        let vmp = &raw mut vm;
        // In-range: identical bits to the interpreter's immediate encoding.
        let bits = unsafe { box_caller(vmp, 42) };
        assert_eq!(bits, Value::small_int(42).to_bits());
        // Out of range: a live BigInt box in the running process heap.
        let bits = unsafe { box_caller(vmp, i64::MAX) };
        let spilled = unsafe { Value::from_bits(bits) };
        assert!(spilled.is_heap() && !spilled.is_immortal());
        assert_eq!(spilled.as_int(), Some(i64::MAX));
    }

    /// CLIF emitted by `emit_dynamic_drop` calls a runtime symbol it never
    /// links against, resolved through this registry. The same path
    /// `al_core`'s CLIF construction takes to reach the runtime.
    #[test]
    fn front_end_emitted_clif_resolves_through_this_registry() {
        let mut module = jit_module().unwrap();
        let fns = RuntimeFns::declare(&mut module).unwrap();
        let mut sig = module.make_signature();
        sig.params.push(AbiParam::new(types::I64));
        let code = define_and_finalize(&mut module, "drop_bits", sig, |module, b| {
            let entry = b.current_block().unwrap();
            let bits = b.block_params(entry)[0];
            let release = module.declare_func_in_func(fns.release_at_zero, b.func);
            emit_dynamic_drop(b, bits, release);
            b.ins().return_(&[]);
        });
        // SAFETY: compiled with exactly this signature above.
        let drop_bits =
            unsafe { std::mem::transmute::<*const u8, unsafe extern "C" fn(u64)>(code) };

        let mut heap = ProcHeap::new();
        let v = Value::int_in(&mut heap, i64::MAX); // mortal BigInt, rc = 1
        assert!(v.is_heap() && !v.is_immortal());
        take_freed_objects();
        unsafe { drop_bits(v.to_bits()) }; // 1 -> 0: freed through the shim
        assert_eq!(take_freed_objects(), 1);
        std::mem::forget(v); // the JIT'd gate released the last reference
    }

    /// `finalize_into` publishes runnable entries into a `NativeTable` and
    /// refuses declarations that do not carry the entry signature.
    #[test]
    fn finalize_into_publishes_entries_and_checks_the_signature() {
        let mut module = jit_module().unwrap();
        let sig = native_entry_signature(&module);
        let id = module
            .declare_function("al_fn_0", Linkage::Export, &sig)
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
            let done = b.ins().iconst(types::I64, NativeStatus::Done as u64 as i64);
            b.ins().return_(&[done]);
            b.finalize();
        }
        module.define_function(id, &mut ctx).unwrap();
        module.clear_context(&mut ctx);

        // A wrong-signature declaration is refused up front, before anything
        // is published.
        let mut bad_sig = module.make_signature();
        bad_sig.returns.push(AbiParam::new(types::I64));
        let bad = module
            .declare_function("wrong_shape", Linkage::Import, &bad_sig)
            .unwrap();
        let table = NativeTable::new(2);
        let def = |fn_idx: usize, func_id: FuncId, name: &str| JitDef {
            fn_idx: FuncIdx::from_usize(fn_idx),
            func_id,
            name: name.to_string(),
            code_size: 0,
        };
        match finalize_into(&mut module, &[def(1, bad, "wrong_shape")], &table) {
            Err(JitError::EntrySignature { name }) => assert_eq!(name, "wrong_shape"),
            other => panic!("expected EntrySignature error, got {other:?}"),
        }
        assert!(table.get(FuncIdx::from_usize(1)).is_none());

        finalize_into(&mut module, &[def(0, id, "al_fn_0")], &table).unwrap();
        let entry = table.get(FuncIdx::from_usize(0)).expect("published");
        let tramp = table.trampoline();
        assert!(
            !tramp.is_null(),
            "finalize_into must publish the trampoline"
        );
        // SAFETY: finalized trampoline + entry from this module; the body
        // ignores its ctx.
        let status = unsafe {
            crate::bytecode::native::call_entry_preserving_pinned(
                std::ptr::null_mut(),
                tramp,
                entry,
            )
        };
        assert_eq!(status, NativeStatus::Done);
        assert!(table.get(FuncIdx::from_usize(1)).is_none());
    }
}
