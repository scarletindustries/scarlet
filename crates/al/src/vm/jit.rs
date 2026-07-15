//! JIT module construction and runtime-symbol resolution: the layering seam
//! between `al_core`'s CLIF construction and this crate's runtime.
//!
//! # The layering decision, documented
//!
//! `al_core` cannot depend on `crates/al`, yet JIT-compiled bodies must call
//! runtime services on both sides of that boundary: `al_core` owns the value
//! layout and its release path (`al_native_release_at_zero`), while this
//! crate owns the VM and the interpreter-parity shims
//! ([`native_shims`](super::native_shims)). Of the two seams the design
//! allows — CLIF construction living wholly in `al_core` with symbols
//! resolved late, or a per-body callback hook the `al` crate installs — the
//! compiler driver forced the *hook* for codegen placement:
//! [`compile_with_native`](al_core::bytecode::compile_with_native)'s
//! [`NativeHook`](al_core::bytecode::NativeHook) fires at the only point
//! where a body's post-perceus `CoreFn` and its per-body `ResolvedPool` are
//! both alive, and its docs record why codegen hangs off that seam.
//!
//! Symbol resolution is the *other half* of the seam, and it is what this
//! module pins down: **runtime symbols cross the crate boundary by NAME.**
//! CLIF emitted anywhere (including `al_core`'s helpers, e.g.
//! [`native_rc::emit_dynamic_drop`](al_core::core_ir::native_rc)) refers to
//! runtime functions as `Linkage::Import` declarations under the canonical
//! names collected in [`runtime_symbols`]; no crate ever links against the
//! other's functions directly. [`jit_module`] registers every
//! `(name, address)` pair with the `JITBuilder`
//! ([`JITBuilder::symbol`]), and Cranelift resolves the names during
//! relocation when [`finalize_into`] runs `finalize_definitions` — i.e. at
//! finalize time, not at Rust link time. Adding a runtime call is therefore
//! one shim + one entry in [`runtime_symbols`] + one import in
//! [`RuntimeFns`]; the A1/A2 shim families extend this table without
//! touching the mechanism.
//!
//! # Code lifetime
//!
//! One [`JITModule`] holds one executable mapping shared by every scheduler:
//! entry pointers published into the program's
//! [`NativeTable`](al_core::bytecode::NativeTable) are raw code addresses,
//! and processes migrate across scheduler threads mid-flight, so the mapping
//! is immortal. `cranelift-jit` only unmaps through the explicit (unsafe)
//! `free_memory` — which nothing here calls — so dropping the module after
//! [`finalize_into`] leaks the code pages by design.

// Designated unsafe module: publishing a finalized code address as a typed
// [`NativeEntry`] is a transmute from `*const u8`, justified by the
// signature check `finalize_into` performs against every published
// declaration.
#![allow(unsafe_code)]

use al_core::bytecode::value::{
    NATIVE_HOLLOW_FOR_REUSE_SYMBOL, NATIVE_RELEASE_AT_ZERO_SYMBOL, native_hollow_for_reuse,
    native_release_at_zero,
};
use al_core::bytecode::{NativeEntry, NativeTable};
use al_core::core_ir::FuncIdx;
use cranelift_codegen::ir::{AbiParam, Signature, Type, types};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{FuncId, Linkage, Module, ModuleError, default_libcall_names};

use super::{native, native_shims, perf_map};

/// Why JIT construction or publication failed. Every `JitError` is
/// recoverable by the same fallback: leave the [`NativeTable`] slots empty
/// and interpret (bytecode exists for every function regardless). Not every
/// failure surfaces as a `JitError`, though — an unresolved runtime symbol
/// panics inside `finalize_definitions` (cranelift-jit aborts with "can't
/// resolve symbol", naming it) rather than returning an error; the
/// correspondence test in this module is what keeps that path unreachable.
#[derive(Debug)]
pub enum JitError {
    /// The host has no Cranelift backend, or ISA construction failed.
    Host(String),
    /// Declaration, definition or finalization failed inside the module.
    /// Boxed: `ModuleError` is >100 bytes and would fatten every
    /// `Result<_, JitError>` on the compile path.
    Module(Box<ModuleError>),
    /// [`finalize_into`] was handed a function not declared with the
    /// [`NativeEntry`] signature; publishing it would type-confuse every
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
/// `(canonical name, shim address)` pairs — the complete resolution table
/// [`jit_module`] registers. Sourced from the crates that own each shim:
/// `al_core`'s release path plus this crate's interpreter-parity shims. The
/// names are the contract [`RuntimeFns::declare`]'s imports resolve against;
/// a symbol present here but never declared is harmless, a declared name
/// missing here panics inside `finalize_definitions` naming the symbol —
/// the correspondence test below is what keeps that path unreachable.
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

/// Build the one JIT module compiled bodies are defined into: host-native
/// ISA, with every [`runtime_symbols`] pair pre-registered so imports
/// resolve by name at finalize time.
///
/// Errors only when the host has no Cranelift backend; the caller's fallback
/// is to interpret everything.
pub fn jit_module() -> Result<JITModule, JitError> {
    let mut flags = settings::builder();
    // JIT'd code and the shims live at arbitrary addresses in one process:
    // calls must go through absolute addresses, not colocated PLT-style
    // stubs, and PIC buys nothing for never-relocated code.
    for (name, value) in [
        ("use_colocated_libcalls", "false"),
        ("is_pic", "false"),
        // Real register allocation is where the backend's win lives; the
        // whole compile unit is small enough that "speed" stays well inside
        // the load-time budget.
        ("opt_level", "speed"),
        // Compiled frames will eventually live on 256K per-process stacks
        // with one guard page ([`super::stack`]). A frame larger than a page
        // does a single unprobed `sub rsp, N` that can step clean over the
        // guard, landing its first local write in the NEXT process's stack —
        // silent corruption, no fault. Probing defaults off in Cranelift and
        // costs ~0 while frame parity keeps frames tiny; it must already be
        // on before anything makes frames big.
        ("enable_probestack", "true"),
        ("probestack_strategy", "inline"),
        // The context argument lives in the pinned register (r15/x21) for
        // the whole body and is re-read at every use, never spilled — a
        // frame must not hold a scheduler-derived word across a suspension
        // point (the stage-2 plan's F1 fix).
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

/// The [`NativeEntry`] signature in CLIF terms:
/// `extern "C" fn(vmx: *mut c_void) -> NativeStatus`, i.e. one pointer
/// parameter and one `i64` status return. Written down once so body
/// declarations and [`finalize_into`]'s check cannot drift apart.
pub fn native_entry_signature(module: &JITModule) -> Signature {
    let mut sig = module.make_signature();
    sig.params
        .push(AbiParam::new(module.target_config().pointer_type()));
    sig.returns.push(AbiParam::new(types::I64));
    sig
}

/// The declared runtime imports, one `FuncId` per shim, ready for
/// `declare_func_in_func` at CLIF-emission sites. Declaring them once up
/// front keeps every signature beside the name it belongs to; Cranelift
/// dedupes repeat declarations, so calling [`RuntimeFns::declare`] is
/// idempotent per module.
pub struct RuntimeFns {
    /// `al_native_release_at_zero(obj)` — the cold free-at-zero call the
    /// inline drop gate branches to
    /// ([`native_rc::emit_dynamic_drop`](al_core::core_ir::native_rc)).
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
    /// frame under the interpreter's frame floor (call kind 2, used directly
    /// only when the emitter has done its own frame handshake).
    pub enter_interp: FuncId,
    /// `al_rt_call(vmx, target, resume_ip, args, argc, out) -> status` —
    /// known call, kinds 1 and 2: frame handshake, entry checkpoint,
    /// table dispatch with frame-floor fallback, result to `out` on `Done`.
    pub rt_call: FuncId,
    /// `al_rt_tail_call(vmx, target, args, argc) -> status` — cross-function
    /// tail call: collapse in place, checkpoint, `TailCall` to the driver.
    /// Compiled tail sites are `return al_rt_tail_call(...)`.
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
        let mut import =
            |name: &'static str, params: &[Type], ret: Option<Type>| -> Result<FuncId, JitError> {
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

/// One compiled body handed to [`finalize_into`]: the entry-table slot it
/// publishes to, the module-level function carrying the definition, and the
/// metadata the [`perf_map`] writer needs. `func_id` must name a *defined*
/// function, not a bare declaration — the definer constructs a `JitDef` only
/// after a successful `define_function`, and [`finalize_into`] relies on
/// that. [`JITModule`] exposes no code-size
/// accessor after finalization, so the definer captures `code_size` while it
/// still holds the compiled context
/// (`ctx.compiled_code().unwrap().code_info().total_size` right after
/// `define_function`).
pub struct JitDef {
    pub fn_idx: FuncIdx,
    pub func_id: FuncId,
    /// Source-level function name; perf-map symbols are `al::<name>`.
    pub name: String,
    /// Finalized machine-code size in bytes.
    pub code_size: u32,
}

/// The finalize step: resolve every pending relocation (this is where the
/// [`runtime_symbols`] names bind to their shim addresses), then publish
/// each def's code address into the program's entry table — and, under
/// `AL_PERF_MAP=1`, one `/tmp/perf-<pid>.map` symbol line per def
/// ([`perf_map`]), since this is the moment code addresses become final.
///
/// Every published declaration is checked against
/// [`native_entry_signature`] first — refusing a mismatched signature here
/// keeps the `*const u8` → [`NativeEntry`] transmute below sound for every
/// later `table.get` caller. The signature check cannot see whether a
/// declaration carries a definition — cranelift keeps that private — so a
/// declared-but-never-defined `func_id` panics in `get_finalized_function`
/// rather than returning an error; [`JitDef`]'s contract (constructed only
/// from a successful `define_function`) is what rules that out.
/// The module must be kept alive (or dropped
/// *without* `free_memory`, which leaks the mapping — see the module docs)
/// for as long as the table's pointers can run, i.e. forever.
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
    module.finalize_definitions()?;
    for def in defs {
        let code = module.get_finalized_function(def.func_id);
        // SAFETY: `code` is the finalized body of a function declared with
        // `native_entry_signature` (checked above), which is `NativeEntry`'s
        // ABI by construction; the mapping is immortal (module docs).
        let entry = unsafe { std::mem::transmute::<*const u8, NativeEntry>(code) };
        table.set(def.fn_idx, entry);
        perf_map::record(code as usize, def.code_size as usize, &def.name);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use al_core::bytecode::NativeStatus;
    use al_core::bytecode::Value;
    use al_core::bytecode::value::take_freed_objects;
    use al_core::core_ir::native_rc::emit_dynamic_drop;
    use al_core::heap::ProcHeap;
    use al_core::tivec::Idx;
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
        // Every import `RuntimeFns` declares resolves against this table:
        // a declared name missing from `runtime_symbols` would only panic
        // inside `finalize_definitions`, so pin the correspondence here.
        //
        // Each row pins one import's whole ABI chain: the typed fn-pointer
        // coercion makes a shim-signature edit a compile error here, the
        // CLIF types are asserted against what `RuntimeFns::declare`
        // declared, and the declared name must resolve to that exact
        // address in `runtime_symbols()` — so neither side of the
        // Rust-shim / CLIF-import contract can drift silently.
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

    /// The seam end-to-end: CLIF calls `al_shim_int_box` by name, the
    /// builder-registered symbol resolves at finalize, and the executed code
    /// spills exactly like the interpreter's boxing.
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

    /// The cross-crate composition the layering exists for: CLIF emitted by
    /// an `al_core` helper (`emit_dynamic_drop`) calling a symbol owned by
    /// `al_core` (`al_native_release_at_zero`), compiled and resolved through
    /// this crate's module + registry, with frees landing in the shared
    /// `FREED_OBJECTS` accounting.
    #[test]
    fn al_core_emitted_clif_resolves_through_this_registry() {
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
        assert_eq!(entry(std::ptr::null_mut()), NativeStatus::Done);
        assert!(table.get(FuncIdx::from_usize(1)).is_none());
    }
}
