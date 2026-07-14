//! JIT spike: prove a trivial Cranelift-JIT'd function actually *executes* on
//! this host before anything is built on top of the native backend.
//!
//! On aarch64-apple-darwin this is the load-bearing check that cranelift-jit's
//! MAP_JIT + W^X handling (pthread_jit_write_protect_np toggling around code
//! writes) works end-to-end: define CLIF, compile, finalize into an executable
//! mapping, and *call* the resulting pointer. If any of that is broken, these
//! fail immediately instead of surfacing as a mystery crash deep inside the
//! real backend.
//!
//! Three escalating spikes:
//!   1. straight-line arithmetic (`add`)
//!   2. control flow with a loop back-edge (`sum_to`) — the shape self-tail
//!      calls lower to
//!   3. a JIT'd caller invoking a Rust `extern "C"` shim registered by NAME
//!      via `JITBuilder::symbol` — the runtime-shim seam the calling
//!      convention depends on

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{AbiParam, InstBuilder, types};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{Linkage, Module, default_libcall_names};

/// A `JITBuilder` configured for the host, the same way the real backend
/// constructs one: native ISA, non-PIC, no colocated libcalls.
fn host_jit_builder() -> JITBuilder {
    let mut flags = settings::builder();
    flags.set("use_colocated_libcalls", "false").unwrap();
    flags.set("is_pic", "false").unwrap();
    let isa = cranelift_native::builder()
        .expect("host ISA supported by cranelift")
        .finish(settings::Flags::new(flags))
        .expect("host ISA flags valid");
    JITBuilder::with_isa(isa, default_libcall_names())
}

#[test]
fn jit_straight_line_add_executes() {
    let mut module = JITModule::new(host_jit_builder());
    let mut ctx = module.make_context();
    let mut fb_ctx = FunctionBuilderContext::new();

    let mut sig = module.make_signature();
    sig.params.push(AbiParam::new(types::I64));
    sig.params.push(AbiParam::new(types::I64));
    sig.returns.push(AbiParam::new(types::I64));
    let func_id = module
        .declare_function("spike_add", Linkage::Export, &sig)
        .unwrap();

    ctx.func.signature = sig;
    {
        let mut b = FunctionBuilder::new(&mut ctx.func, &mut fb_ctx);
        let entry = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        b.seal_block(entry);
        let (x, y) = (b.block_params(entry)[0], b.block_params(entry)[1]);
        let sum = b.ins().iadd(x, y);
        b.ins().return_(&[sum]);
        b.finalize();
    }
    module.define_function(func_id, &mut ctx).unwrap();
    module.clear_context(&mut ctx);
    module.finalize_definitions().unwrap();

    let code = module.get_finalized_function(func_id);
    let add: extern "C" fn(i64, i64) -> i64 =
        unsafe { std::mem::transmute::<*const u8, extern "C" fn(i64, i64) -> i64>(code) };
    assert_eq!(add(2, 40), 42);
    assert_eq!(add(-7, 7), 0);
    assert_eq!(add(i64::MAX, 1), i64::MIN, "wrapping iadd semantics");
}

#[test]
fn jit_loop_back_edge_executes() {
    let mut module = JITModule::new(host_jit_builder());
    let mut ctx = module.make_context();
    let mut fb_ctx = FunctionBuilderContext::new();

    let mut sig = module.make_signature();
    sig.params.push(AbiParam::new(types::I64));
    sig.returns.push(AbiParam::new(types::I64));
    let func_id = module
        .declare_function("spike_sum_to", Linkage::Export, &sig)
        .unwrap();

    // sum_to(n) = n + (n-1) + ... + 1, as an explicit loop: the CLIF shape a
    // self-tail-recursive AL function compiles to (header block params are the
    // next iteration's args).
    ctx.func.signature = sig;
    {
        let mut b = FunctionBuilder::new(&mut ctx.func, &mut fb_ctx);
        let entry = b.create_block();
        let header = b.create_block();
        let body = b.create_block();
        let exit = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.append_block_param(header, types::I64); // n
        b.append_block_param(header, types::I64); // acc
        b.append_block_param(exit, types::I64);

        b.switch_to_block(entry);
        b.seal_block(entry);
        let n0 = b.block_params(entry)[0];
        let zero = b.ins().iconst(types::I64, 0);
        b.ins().jump(header, &[n0.into(), zero.into()]);

        b.switch_to_block(header);
        let n = b.block_params(header)[0];
        let acc = b.block_params(header)[1];
        let done = b.ins().icmp_imm(IntCC::SignedLessThanOrEqual, n, 0);
        b.ins().brif(done, exit, &[acc.into()], body, &[]);

        b.switch_to_block(body);
        b.seal_block(body);
        let acc2 = b.ins().iadd(acc, n);
        let n2 = b.ins().iadd_imm(n, -1);
        b.ins().jump(header, &[n2.into(), acc2.into()]);
        b.seal_block(header);

        b.switch_to_block(exit);
        b.seal_block(exit);
        let result = b.block_params(exit)[0];
        b.ins().return_(&[result]);
        b.finalize();
    }
    module.define_function(func_id, &mut ctx).unwrap();
    module.clear_context(&mut ctx);
    module.finalize_definitions().unwrap();

    let code = module.get_finalized_function(func_id);
    let sum_to: extern "C" fn(i64) -> i64 =
        unsafe { std::mem::transmute::<*const u8, extern "C" fn(i64) -> i64>(code) };
    assert_eq!(sum_to(0), 0);
    assert_eq!(sum_to(1), 1);
    assert_eq!(sum_to(10), 55);
    assert_eq!(sum_to(100_000), 5_000_050_000);
}

extern "C" fn spike_shim(x: i64) -> i64 {
    x + 1
}

#[test]
fn jit_calls_rust_shim_registered_by_name() {
    let mut builder = host_jit_builder();
    builder.symbol("al_spike_shim", spike_shim as *const u8);
    let mut module = JITModule::new(builder);
    let mut ctx = module.make_context();
    let mut fb_ctx = FunctionBuilderContext::new();

    let mut shim_sig = module.make_signature();
    shim_sig.params.push(AbiParam::new(types::I64));
    shim_sig.returns.push(AbiParam::new(types::I64));
    let shim_id = module
        .declare_function("al_spike_shim", Linkage::Import, &shim_sig)
        .unwrap();

    let mut sig = module.make_signature();
    sig.params.push(AbiParam::new(types::I64));
    sig.returns.push(AbiParam::new(types::I64));
    let func_id = module
        .declare_function("spike_call_shim", Linkage::Export, &sig)
        .unwrap();

    // f(x) = shim(shim(x)) — two calls so the return value of one call feeds
    // the argument of the next, not just a pass-through.
    ctx.func.signature = sig;
    {
        let mut b = FunctionBuilder::new(&mut ctx.func, &mut fb_ctx);
        let shim_ref = module.declare_func_in_func(shim_id, b.func);
        let entry = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        b.seal_block(entry);
        let x = b.block_params(entry)[0];
        let call1 = b.ins().call(shim_ref, &[x]);
        let y = b.inst_results(call1)[0];
        let call2 = b.ins().call(shim_ref, &[y]);
        let z = b.inst_results(call2)[0];
        b.ins().return_(&[z]);
        b.finalize();
    }
    module.define_function(func_id, &mut ctx).unwrap();
    module.clear_context(&mut ctx);
    module.finalize_definitions().unwrap();

    let code = module.get_finalized_function(func_id);
    let f: extern "C" fn(i64) -> i64 =
        unsafe { std::mem::transmute::<*const u8, extern "C" fn(i64) -> i64>(code) };
    assert_eq!(f(40), 42);
    assert_eq!(f(-1), 1);
}
