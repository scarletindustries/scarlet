use std::collections::{HashMap, VecDeque};
use std::io::{ErrorKind, IoSlice, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[cfg(test)]
use al_core::bytecode::Function;
use al_core::bytecode::transfer;
use al_core::bytecode::{
    BinaryValue, ClosureValue, EnumValue, HeapValue, Op, Program, Seq, SocketValue, Value,
    ValueView, enum_hash_with_payload, enum_name_prefix_hash,
};
use al_core::static_ir::VariantTemplate;

use crate::stdlib;

mod binary;
mod sched;

use sched::{Runtime, Seed};

type VmResult<T> = Result<T, String>;

#[derive(Debug, Clone)]
struct CallFrame {
    func_idx: i32,
    code_start: i32,
    ip: i32,
    base_slot: usize,
    // Shared (not owned) so that pushing a child frame on every
    // `Op::Call`/`Op::TailCall`/`Op::CallSelf` is an `Rc` refcount bump rather
    // than an O(k) `Vec<Value>` clone. See `ClosureValue::captures`.
    captures: Rc<[Value]>,
}

/// How many function applications a process may make before it is preempted.
/// Mirrors BEAM's per-process reduction budget (`CONTEXT_REDS`).
const REDUCTION_BUDGET: i32 = 4000;

/// What one I/O operation (a syscall) costs in reductions. Charging I/O keeps
/// an accept/read loop from monopolizing its scheduler: ~40 I/O ops fill a
/// budget, after which the process is preempted at its next call.
const IO_REDUCTION_COST: i32 = 100;

/// What a parked process is waiting for.
#[derive(Debug)]
enum Wait {
    /// A socket (listener or connection) must become readable.
    Readable(i32),
    /// A connection must become writable.
    Writable(i32),
    /// An outbound connect must finish (signalled by writability). Unlike the
    /// other I/O waits, the instruction is not re-run on wake: the completion
    /// handler pushes the result directly onto the process's stack.
    Connecting(i32),
    /// Runnable again at this instant.
    Until(Instant),
}

/// Why an execution slice ended.
enum Step {
    /// The current process ran to completion; its result is its top-of-stack.
    Done,
    /// The reduction budget ran out; the process is resumable as-is.
    Yield,
    /// The process must wait for I/O readiness or a timer.
    Parked(Wait),
}

/// A suspended lightweight process — a complete resumable continuation.
/// The *running* process lives directly in `VM.stack`/`VM.frames` so the
/// dispatch loop is untouched; a context switch swaps those vectors with a
/// `Process` (a few pointer moves).
struct Process {
    stack: Vec<Value>,
    frames: Vec<CallFrame>,
    is_main: bool,
}

/// Precomputed prelude enum templates. The constant-shape parts (names, labels,
/// name-prefix hash) are built once at VM init; hot paths clone instead of
/// rebuilding. For payload-carrying variants the stored `hash` is the
/// name-prefix hash (`enum_name_prefix_hash(en, vn)`) which `with_payload`
/// completes by folding in the single payload value.
struct PreludeTemplates {
    nil: Rc<EnumValue>,
    none: Rc<EnumValue>,
    some: EnumValue,
    ok: EnumValue,
    err: EnumValue,
    ip_v4: EnumValue,
    ip_v6: EnumValue,
    socket_address: EnumValue,
    socket: EnumValue,
}

/// Convert a build.rs-emitted `stdlib::*` template into the runtime form.
fn enum_template(t: &VariantTemplate) -> EnumValue {
    EnumValue {
        type_id: t.type_id,
        enum_name: t.type_name.into(),
        variant_name: t.variant_name.into(),
        field_labels: t.labels.iter().map(|s| Rc::from(*s)).collect(),
        payload: Vec::new(),
        hash: enum_name_prefix_hash(t.type_name, t.variant_name),
    }
}

impl PreludeTemplates {
    fn new() -> Self {
        PreludeTemplates {
            nil: Rc::new(enum_template(&stdlib::prelude::NIL)),
            none: Rc::new(enum_template(&stdlib::prelude::NONE)),
            some: enum_template(&stdlib::prelude::SOME),
            ok: enum_template(&stdlib::prelude::OK),
            err: enum_template(&stdlib::prelude::ERR),
            ip_v4: enum_template(&stdlib::net::address::V4),
            ip_v6: enum_template(&stdlib::net::address::V6),
            socket_address: enum_template(&stdlib::net::address::SOCKET_ADDRESS),
            socket: enum_template(&stdlib::net::socket::SOCKET),
        }
    }

    #[inline]
    fn with_payload(template: &EnumValue, v: Value) -> Value {
        let mut e = template.clone();
        e.hash = enum_hash_with_payload(e.hash, std::slice::from_ref(&v));
        e.payload = vec![v];
        Value::enum_(e)
    }

    #[inline]
    fn with_payloads(template: &EnumValue, vs: Vec<Value>) -> Value {
        let mut e = template.clone();
        e.hash = enum_hash_with_payload(e.hash, &vs);
        e.payload = vs;
        Value::enum_(e)
    }

    fn socket_address(&self, a: std::net::SocketAddr) -> Value {
        let ip_tpl = if a.is_ipv6() {
            &self.ip_v6
        } else {
            &self.ip_v4
        };
        let ip = Self::with_payload(ip_tpl, Value::str(a.ip().to_string()));
        Self::with_payloads(&self.socket_address, vec![ip, Value::int(a.port() as i64)])
    }

    fn make_socket(&self, conn: Value, peer: std::net::SocketAddr) -> Value {
        Self::with_payloads(&self.socket, vec![conn, self.socket_address(peer)])
    }
}

pub struct VM {
    program: Program,
    templates: PreludeTemplates,
    stack: Vec<Value>,
    frames: Vec<CallFrame>,
    /// One cached closure per capture-free function, indexed by func_idx.
    /// `PushSelf` clones the cached `Rc` (refcount bump, zero alloc) instead
    /// of rebuilding name + closure from the frame on every self-call.
    /// Capture-carrying functions get `None` and fall back to the frame path.
    self_closures: Vec<Option<Rc<ClosureValue>>>,
    /// Memoized field-label slices, one entry per non-prelude ctor site, keyed
    /// by the address of that site's pooled labels-array constant. The labels
    /// are statically constant per constructor, so this turns the O(fields)
    /// `Vec` + boxed-slice rebuild in `MakeEnumPayload` into a one-time
    /// conversion plus an `Rc` refcount bump on every later construction.
    label_cache: HashMap<usize, Rc<[Rc<str>]>>,
    next_socket_id: i32,
    tcp_listeners: HashMap<i32, TcpListener>,
    tcp_connections: HashMap<i32, TcpStream>,
    /// Outbound connections whose non-blocking connect is still in flight,
    /// keyed by their already-allocated socket id.
    pending_connects: HashMap<i32, socket2::Socket>,
    /// Reusable scratch buffer for socket reads, grown on demand, so each
    /// read allocates only its result.
    read_scratch: Vec<u8>,
    /// Whether the currently-running process is the main (root) process.
    current_is_main: bool,
    /// The main process's result, stashed if it finishes while other
    /// processes are still running.
    main_result: Option<Value>,
    /// Runnable processes in round-robin order (the running one is not here).
    run_queue: VecDeque<Process>,
    /// Processes waiting on I/O readiness or timers.
    parked: Vec<(Wait, Process)>,
    /// OS event queue (kqueue/epoll); created when a process first parks on
    /// I/O, or adopted from the runtime when multi-core scheduling boots.
    poller: Option<Arc<polling::Poller>>,
    /// Multi-core runtime, booted by the first spawn. `None` for programs
    /// that never spawn and for single-core runs (AL_SCHEDULERS=1).
    runtime: Option<Arc<Runtime>>,
    /// This VM's scheduler index; 0 is the main thread.
    scheduler_index: usize,
    /// The global (literal) area: top-level bindings, addressed by
    /// `Op::PushGlobal`. On scheduler 0 it mirrors main's frame; workers
    /// hydrate it from the runtime's shared area.
    globals: Vec<Value>,
    /// The shared-area version this scheduler last hydrated from.
    globals_synced_version: u64,
}

pub fn new_vm(program: Program) -> VM {
    let templates = PreludeTemplates::new();
    let empty_captures: Rc<[Value]> = Vec::new().into();
    let self_closures = program
        .functions
        .iter()
        .enumerate()
        .map(|(idx, f)| {
            (f.capture_count == 0).then(|| {
                Rc::new(ClosureValue {
                    func_idx: idx as i32,
                    captures: empty_captures.clone(),
                    name: f.name.clone(),
                })
            })
        })
        .collect();
    // The global (literal) area is sized by the entry function: top-level
    // bindings are its "locals", mirrored into this table as they are written.
    let globals_len = program
        .functions
        .get(program.entry as usize)
        .map(|f| f.locals as usize)
        .unwrap_or(0);
    VM {
        program,
        templates,
        stack: Vec::new(),
        frames: Vec::new(),
        self_closures,
        label_cache: HashMap::new(),
        next_socket_id: 1,
        tcp_listeners: HashMap::new(),
        tcp_connections: HashMap::new(),
        pending_connects: HashMap::new(),
        read_scratch: Vec::new(),
        current_is_main: false,
        main_result: None,
        run_queue: VecDeque::new(),
        parked: Vec::new(),
        poller: None,
        runtime: None,
        scheduler_index: 0,
        globals: vec![Value::int(0); globals_len],
        globals_synced_version: 0,
    }
}

impl VM {
    // The interpreter pushes the entry frame before the loop and never pops the
    // last one while running, so a frame is always live. These accessors are the
    // single audited place that relies on that invariant.
    #[inline]
    #[allow(clippy::unwrap_used)]
    fn frame(&self) -> &CallFrame {
        self.frames.last().unwrap()
    }

    #[inline]
    #[allow(clippy::unwrap_used)]
    fn frame_mut(&mut self) -> &mut CallFrame {
        self.frames.last_mut().unwrap()
    }

    pub fn run(&mut self) -> VmResult<Value> {
        let entry = self.program.entry;
        let main_func = &self.program.functions[entry as usize];
        let (code_start, locals) = (main_func.code_start, main_func.locals);

        self.frames.push(CallFrame {
            func_idx: entry,
            code_start,
            ip: 0,
            base_slot: 0,
            captures: Vec::new().into(),
        });

        for _ in 0..locals {
            self.stack.push(Value::int(0));
        }

        self.current_is_main = true;
        let result = self.scheduler_loop();

        // Multi-core shutdown: by the time scheduler 0's loop returns Ok, the
        // global live count is zero, so workers are exiting; join them. On
        // error the program is dying — process exit reaps the workers.
        if result.is_ok()
            && let Some(rt) = &self.runtime
        {
            let workers = std::mem::take(&mut *sched::lock(&rt.workers));
            for handle in workers {
                if handle.join().is_err() {
                    eprintln!("scheduler: a worker thread terminated abnormally");
                }
            }
        }
        result
    }

    /// Drive processes to completion, round-robin with reduction-budget
    /// preemption. Scheduler 0 returns the main process's result once every
    /// process in the program (across all schedulers) has finished; worker
    /// schedulers return Nil when the program is over.
    fn scheduler_loop(&mut self) -> VmResult<Value> {
        loop {
            // Make sure some process is current.
            if self.frames.is_empty() && !self.acquire_work()? {
                let result = self.main_result.take().unwrap_or_else(|| self.make_nil());
                return Ok(result);
            }

            match self.execute_slice()? {
                Step::Done => {
                    // The finished process's result is its top-of-stack.
                    if self.current_is_main {
                        let result = self.stack.pop().unwrap_or_else(|| self.make_nil());
                        self.main_result = Some(result);
                    }
                    self.stack.clear();
                    self.frames.clear();
                    self.current_is_main = false;
                    if let Some(rt) = &self.runtime {
                        rt.process_finished();
                    }
                }
                Step::Yield => {
                    // Preempted. Wake any ready parked processes; when there is
                    // nothing else local AND no idle scheduler to take them,
                    // pick up injector seeds so queued work still gets
                    // time-sliced in when every scheduler is busy.
                    if !self.parked.is_empty() {
                        self.poll_parked(false)?;
                    }
                    if self.run_queue.is_empty() && !self.others_idle() {
                        self.take_seeds()?;
                    }
                    if !self.run_queue.is_empty() {
                        let outgoing = self.suspend_current();
                        self.run_queue.push_back(outgoing);
                        // The queue is non-empty, so this always succeeds.
                        if let Some(p) = self.run_queue.pop_front() {
                            self.resume(p);
                        }
                    }
                }
                Step::Parked(wait) => {
                    self.register_wait(&wait)?;
                    let outgoing = self.suspend_current();
                    self.parked.push((wait, outgoing));
                }
            }
        }
    }

    /// Detach the running process's state into a `Process`.
    fn suspend_current(&mut self) -> Process {
        Process {
            stack: std::mem::take(&mut self.stack),
            frames: std::mem::take(&mut self.frames),
            is_main: std::mem::take(&mut self.current_is_main),
        }
    }

    /// Make `p` the running process.
    fn resume(&mut self, p: Process) {
        self.stack = p.stack;
        self.frames = p.frames;
        self.current_is_main = p.is_main;
    }

    /// Get something to run into `stack`/`frames`. Blocks while there is
    /// nothing local to run but the program is still alive somewhere.
    /// Returns false once the whole program has finished.
    fn acquire_work(&mut self) -> VmResult<bool> {
        loop {
            // 1. Local runnable processes.
            if let Some(p) = self.run_queue.pop_front() {
                self.resume(p);
                return Ok(true);
            }

            // 2. Remote seeds.
            if self.take_seeds()? {
                continue;
            }

            // 3. Local parked I/O / timers: wait for them, but stay wakeable
            //    by other schedulers (seed submissions, program end).
            if !self.parked.is_empty() {
                self.set_parked_flag(true);
                let poll_result = self.poll_parked(true);
                self.set_parked_flag(false);
                poll_result?;
                continue;
            }

            // 4. Nothing local at all.
            if self.runtime.is_none() {
                // Single scheduler: the program is over.
                return Ok(false);
            }
            if self.runtime_finished() {
                return Ok(false);
            }
            // Wait for a seed or for the program to end. Set the parked flag
            // first, then re-check (a submitter who missed our flag may have
            // pushed in between; `notify` is sticky so the reverse race is
            // safe).
            self.set_parked_flag(true);
            if self.take_seeds()? {
                self.set_parked_flag(false);
                continue;
            }
            if self.runtime_finished() {
                self.set_parked_flag(false);
                return Ok(false);
            }
            let wait_result = self.wait_for_notify();
            self.set_parked_flag(false);
            wait_result?;
        }
    }

    /// Move a batch of seeds from the shared injector into the local run
    /// queue. Returns whether any arrived.
    fn take_seeds(&mut self) -> VmResult<bool> {
        let batch = match &self.runtime {
            Some(rt) => rt.take_seeds(),
            None => return Ok(false),
        };
        if batch.is_empty() {
            return Ok(false);
        }
        // Seeds may reference top-level bindings published after our last
        // sync; refresh the global area first. (Publish happens-before
        // submit, submit happens-before take, so this can never be stale.)
        self.sync_globals();
        for seed in batch {
            self.hydrate_seed(seed)?;
        }
        Ok(true)
    }

    /// Bring this scheduler's global area up to date with the runtime's
    /// shared (published) globals.
    fn sync_globals(&mut self) {
        let Some(rt) = &self.runtime else {
            return;
        };
        let version = rt
            .globals_version
            .load(std::sync::atomic::Ordering::Acquire);
        if version == self.globals_synced_version {
            return;
        }
        let shared = sched::lock(&rt.globals);
        if self.globals.len() < shared.len() {
            self.globals.resize(shared.len(), Value::int(0));
        }
        for (slot, entry) in shared.iter().enumerate() {
            if let Some(sv) = entry {
                self.globals[slot] = transfer::value_in(sv);
            }
        }
        drop(shared);
        self.globals_synced_version = version;
    }

    /// Whether the whole program has finished (multi-core mode only).
    fn runtime_finished(&self) -> bool {
        match &self.runtime {
            Some(rt) => rt.is_finished(),
            None => false,
        }
    }

    /// Whether some other scheduler is idle and able to take injector seeds.
    fn others_idle(&self) -> bool {
        match &self.runtime {
            Some(rt) => rt.any_other_idle(self.scheduler_index),
            None => false,
        }
    }

    /// Mark this scheduler as parked/unparked so seed submitters know who to
    /// wake.
    fn set_parked_flag(&self, parked: bool) {
        if let Some(rt) = &self.runtime {
            rt.parked_flags[self.scheduler_index]
                .store(parked, std::sync::atomic::Ordering::Release);
        }
    }

    /// Block until another scheduler notifies this one (seed submitted or
    /// program finished).
    fn wait_for_notify(&mut self) -> VmResult<()> {
        self.ensure_poller()?;
        let Some(poller) = &self.poller else {
            return Ok(());
        };
        let mut events = polling::Events::new();
        poller
            .wait(&mut events, None)
            .map_err(|e| format!("scheduler wait failed: {e}"))?;
        Ok(())
    }

    /// Create this scheduler's poller if it does not exist yet.
    fn ensure_poller(&mut self) -> VmResult<()> {
        if self.poller.is_none() {
            let poller =
                polling::Poller::new().map_err(|e| format!("cannot create OS poller: {e}"))?;
            self.poller = Some(Arc::new(poller));
        }
        Ok(())
    }

    fn execute_slice(&mut self) -> VmResult<Step> {
        // Hoist the active frame's scalar state into locals so the per-instruction
        // path avoids two Vec indexes. Synced back to self.frames on Call/TailCall/Ret.
        // `func_idx` is hoisted for `CallSelf`/`TailCallSelf`, which resolve the
        // callee from the live frame instead of popping a closure.
        let (mut ip, mut code_start, mut base_slot, mut func_idx) = {
            let f = self.frame();
            (f.ip, f.code_start, f.base_slot, f.func_idx)
        };
        // Remaining reduction budget for this slice; one function application
        // costs one reduction. Exhaustion preempts the process.
        let mut reds = REDUCTION_BUDGET;

        loop {
            let addr = code_start + ip;
            if addr as usize >= self.program.code.len() {
                break;
            }

            // SAFETY: bounds checked immediately above.
            let instr = unsafe { al_core::bytecode::fetch(&self.program.code, addr as usize) };
            ip += 1;

            // Typed-op dispatch helpers. Defined here (after `instr`) so the
            // free `self`/`instr`/`ip`/`base_slot`/`code_start` resolve by
            // macro_rules definition-site hygiene. Each expands to exactly the
            // hand-written arm body, so codegen is byte-identical.
            macro_rules! bin {
                ($acc:ident, $ctor:ident, |$a:ident, $b:ident| $body:expr) => {{
                    let $b = self.pop()?.$acc();
                    let $a = self.pop()?.$acc();
                    self.stack.push(Value::$ctor($body));
                }};
            }
            macro_rules! un {
                ($acc:ident, $ctor:ident, |$a:ident| $body:expr) => {{
                    let $a = self.pop()?.$acc();
                    self.stack.push(Value::$ctor($body));
                }};
            }
            macro_rules! lc_arith {
                (|$a:ident, $b:ident| $body:expr) => {{
                    let $a = self.stack[base_slot + instr.a as usize].as_int_unchecked();
                    let $b = self.program.constants[instr.b as usize].as_int_unchecked();
                    self.stack.push(Value::int($body));
                }};
            }
            macro_rules! lc_jump {
                (|$a:ident, $b:ident| $cond:expr) => {{
                    let $a = self.stack[base_slot + instr.a as usize].as_int_unchecked();
                    let $b = self.program.constants[instr.b as usize].as_int_unchecked();
                    if $cond {
                        ip = instr.operand - code_start;
                    }
                }};
            }

            match instr.op {
                Op::PushConst => {
                    self.stack
                        .push(self.program.constants[instr.operand as usize].clone());
                }
                Op::PushLocal => {
                    let slot = base_slot + instr.operand as usize;
                    self.stack.push(self.stack[slot].clone());
                }
                Op::PushGlobal => {
                    // Top-level bindings live in the global (literal) area,
                    // shared by every process on this scheduler.
                    let slot = instr.operand as usize;
                    self.stack.push(self.globals[slot].clone());
                }
                Op::StoreLocal => {
                    let slot = base_slot + instr.operand as usize;
                    let v = self.pop()?;
                    self.stack[slot] = v;
                    // Top-level bindings (the main process's entry frame) are
                    // the program's globals: mirror them into the global area
                    // and, when other schedulers exist, publish them. Globals
                    // are write-once, so each slot publishes at most once.
                    // Spawned processes also run at base_slot 0, hence the
                    // is_main guard.
                    if base_slot == 0 && self.current_is_main {
                        if slot >= self.globals.len() {
                            self.globals.resize(slot + 1, Value::int(0));
                        }
                        self.globals[slot] = self.stack[slot].clone();
                        if let Some(rt) = &self.runtime {
                            let mut sink = Vec::new();
                            let sv = transfer::value_out(&self.globals[slot], &mut sink);
                            rt.publish_global(slot, sv);
                        }
                    }
                }
                Op::PushNil => {
                    self.stack.push(self.make_nil());
                }
                Op::PushTrue => {
                    self.stack.push(Value::bool(true));
                }
                Op::PushFalse => {
                    self.stack.push(Value::bool(false));
                }
                Op::Pop => {
                    self.pop()?;
                }
                Op::Dup => {
                    let v = self.peek()?.clone();
                    self.stack.push(v);
                }
                Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Mod => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    self.stack.push(binary_op(a, b, instr.op)?);
                }
                Op::Neg => {
                    let a = self.pop()?;
                    if let Some(i) = a.as_int() {
                        self.stack.push(Value::int(i.wrapping_neg()));
                    } else if let Some(f) = a.as_float() {
                        self.stack.push(Value::float(-f));
                    } else {
                        return Err(format!(
                            "Cannot negate non-numeric value '{}'",
                            value_type_name(&a)
                        ));
                    }
                }

                // ---- Type-specialized arithmetic ----------------------------
                // Emitted only when unification proved both operands concrete,
                // so the tag is a debug-only invariant (`as_*_unchecked`).
                // Totality matches `binary_op`/`float_op` exactly: wrap on
                // overflow, x/0=0, x%0=x, non-finite float → 0.0.
                Op::AddInt => bin!(as_int_unchecked, int, |a, b| a.wrapping_add(b)),
                Op::SubInt => bin!(as_int_unchecked, int, |a, b| a.wrapping_sub(b)),
                Op::MulInt => bin!(as_int_unchecked, int, |a, b| a.wrapping_mul(b)),
                Op::DivInt => {
                    bin!(as_int_unchecked, int, |a, b| if b == 0 {
                        0
                    } else {
                        a.wrapping_div(b)
                    })
                }
                Op::ModInt => {
                    bin!(as_int_unchecked, int, |a, b| if b == 0 {
                        a
                    } else {
                        a.wrapping_rem(b)
                    })
                }
                Op::NegInt => un!(as_int_unchecked, int, |a| a.wrapping_neg()),
                Op::AddFloat => bin!(as_float_unchecked, float, |a, b| a + b),
                Op::SubFloat => bin!(as_float_unchecked, float, |a, b| a - b),
                Op::MulFloat => bin!(as_float_unchecked, float, |a, b| a * b),
                Op::DivFloat => {
                    bin!(as_float_unchecked, float, |a, b| if b == 0.0 {
                        0.0
                    } else {
                        a / b
                    })
                }
                Op::NegFloat => un!(as_float_unchecked, float, |a| -a),
                Op::AddStr => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    match (a.as_str(), b.as_str()) {
                        (Some(sa), Some(sb)) => {
                            self.stack.push(concat_str(sa, sb));
                        }
                        _ => {
                            debug_assert!(false, "AddStr on non-Str");
                            self.stack.push(Value::str(""));
                        }
                    }
                }

                Op::Eq => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    self.stack.push(Value::bool(values_equal(&a, &b)));
                }
                Op::Neq => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    self.stack.push(Value::bool(!values_equal(&a, &b)));
                }
                Op::Lt | Op::Gt | Op::Lte | Op::Gte => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    self.stack.push(Value::bool(compare(a, b, instr.op)?));
                }

                // ---- Type-specialized comparison ----------------------------
                Op::LtInt => bin!(as_int_unchecked, bool, |a, b| a < b),
                Op::GtInt => bin!(as_int_unchecked, bool, |a, b| a > b),
                Op::LteInt => bin!(as_int_unchecked, bool, |a, b| a <= b),
                Op::GteInt => bin!(as_int_unchecked, bool, |a, b| a >= b),
                Op::EqInt => bin!(as_int_unchecked, bool, |a, b| a == b),
                Op::NeqInt => bin!(as_int_unchecked, bool, |a, b| a != b),
                Op::LtFloat => bin!(as_float_unchecked, bool, |a, b| a < b),
                Op::GtFloat => bin!(as_float_unchecked, bool, |a, b| a > b),
                Op::LteFloat => bin!(as_float_unchecked, bool, |a, b| a <= b),
                Op::GteFloat => bin!(as_float_unchecked, bool, |a, b| a >= b),

                Op::Not => {
                    let a = self.pop()?;
                    self.stack.push(Value::bool(!is_truthy(&a)));
                }
                Op::Jump => {
                    ip = instr.operand - code_start;
                }
                Op::JumpIfFalse => {
                    let cond = self.pop()?;
                    if !is_truthy(&cond) {
                        ip = instr.operand - code_start;
                    }
                }
                Op::JumpIfTrue => {
                    let cond = self.pop()?;
                    if is_truthy(&cond) {
                        ip = instr.operand - code_start;
                    }
                }

                // ---- Superinstructions --------------------------------------
                // Peephole-fused (PushLocal a; PushConst b; <op>) sequences;
                // operands packed into Instruction.{a,b}. Zero stack traffic
                // for the compare+branch forms.
                Op::AddIntLC => lc_arith!(|a, b| a.wrapping_add(b)),
                Op::SubIntLC => lc_arith!(|a, b| a.wrapping_sub(b)),
                Op::JumpGeIntLC => lc_jump!(|a, b| a >= b),
                Op::JumpNeIntLC => lc_jump!(|a, b| a != b),
                Op::Nop => {}

                Op::Call | Op::TailCall => {
                    let arity = instr.operand;
                    let callee = self.pop()?;

                    if let Some(cl) = callee.as_closure() {
                        let func = &self.program.functions[cl.func_idx as usize];
                        let (func_arity, func_locals, func_code_start) =
                            (func.arity, func.locals, func.code_start);

                        if arity != func_arity {
                            return Err(format!(
                                "Expected {} arguments, got {}",
                                func_arity, arity
                            ));
                        }

                        let cl_func_idx = cl.func_idx;
                        let cl_captures = cl.captures.clone();
                        let args_start = self.stack.len() - arity as usize;

                        if instr.op == Op::TailCall {
                            self.stack.drain(base_slot..args_start);
                            let f = self.frame_mut();
                            f.func_idx = cl_func_idx;
                            f.code_start = func_code_start;
                            f.ip = 0;
                            f.captures = cl_captures;
                        } else {
                            self.frame_mut().ip = ip;
                            self.frames.push(CallFrame {
                                func_idx: cl_func_idx,
                                code_start: func_code_start,
                                ip: 0,
                                base_slot: args_start,
                                captures: cl_captures,
                            });
                            base_slot = args_start;
                        }

                        for _ in arity..func_locals {
                            self.stack.push(Value::int(0));
                        }

                        ip = 0;
                        code_start = func_code_start;
                        func_idx = cl_func_idx;

                        // One function application = one reduction. The callee
                        // frame is fully consistent here, so preemption is a
                        // plain return — resume re-hoists from the frame.
                        reds -= 1;
                        if reds <= 0 {
                            return Ok(Step::Yield);
                        }
                    } else {
                        return Err("Cannot call non-function".to_string());
                    }
                }
                Op::CallSelf | Op::TailCallSelf => {
                    // Self-recursion fast path: callee is the live frame's
                    // function, so we skip the closure pop, the `Value::Closure`
                    // tag match, and the arity check (statically guaranteed by
                    // `compile_call`). `func_idx`/`code_start` are already the
                    // target's, so the only frame work is slot bookkeeping.
                    let arity = instr.operand;
                    let func = &self.program.functions[func_idx as usize];
                    let func_locals = func.locals;
                    let args_start = self.stack.len() - arity as usize;

                    if instr.op == Op::TailCallSelf {
                        // Reuse the frame in place. Captures are already the
                        // current frame's — self-recursion cannot change the
                        // closed-over environment — so no `captures.clone()`.
                        self.stack.drain(base_slot..args_start);
                        let f = self.frame_mut();
                        f.ip = 0;
                    } else {
                        // Push a child frame. A self-call from inside a
                        // capture-carrying closure must see the same captures,
                        // so clone from the current frame (cheap: empty Vec for
                        // every top-level fn, which is the bench's only case).
                        let captures = self.frame().captures.clone();
                        self.frame_mut().ip = ip;
                        self.frames.push(CallFrame {
                            func_idx,
                            code_start,
                            ip: 0,
                            base_slot: args_start,
                            captures,
                        });
                        base_slot = args_start;
                    }

                    for _ in arity..func_locals {
                        self.stack.push(Value::int(0));
                    }
                    ip = 0;

                    reds -= 1;
                    if reds <= 0 {
                        return Ok(Step::Yield);
                    }
                }
                Op::Ret => {
                    let ret_val = self.pop()?;
                    let Some(old_frame) = self.frames.pop() else {
                        return Err("internal error: return with no active call frame".to_string());
                    };

                    self.stack.truncate(old_frame.base_slot);

                    self.stack.push(ret_val);

                    match self.frames.last() {
                        None => break,
                        Some(f) => {
                            ip = f.ip;
                            code_start = f.code_start;
                            base_slot = f.base_slot;
                            func_idx = f.func_idx;
                        }
                    }
                }
                Op::MakeArray => {
                    let len = instr.operand as usize;
                    let arr = self.pop_n(len)?;
                    self.stack.push(Value::array(arr));
                }
                Op::MakeTuple => {
                    let len = instr.operand as usize;
                    let arr = self.pop_n(len)?;
                    self.stack.push(Value::tuple(arr));
                }
                Op::TupleIndex => {
                    let tuple_val = self.pop()?;
                    if let Some(HeapValue::Tuple(t)) = tuple_val.as_heap() {
                        let idx = instr.operand;
                        if idx >= 0 && (idx as usize) < t.elements.len() {
                            self.stack.push(t.elements[idx as usize].clone());
                        } else {
                            return Err(format!(
                                "Tuple index {} out of bounds (len: {})",
                                idx,
                                t.elements.len()
                            ));
                        }
                    } else {
                        return Err(format!(
                            "Expected tuple, got '{}'",
                            value_type_name(&tuple_val)
                        ));
                    }
                }
                Op::MakeRange => {
                    let end_val = self.pop()?;
                    let start_val = self.pop()?;

                    if let (Some(start), Some(end)) = (start_val.as_int(), end_val.as_int()) {
                        self.stack.push(Value::range(start, end));
                    } else {
                        return Err(format!(
                            "Range bounds must be integers, got '{}' and '{}'",
                            value_type_name(&start_val),
                            value_type_name(&end_val)
                        ));
                    }
                }
                Op::Index => {
                    let idx_val = self.pop()?;
                    let arr_val = self.pop()?;
                    let v = match (arr_val.as_heap(), idx_val.as_int()) {
                        (Some(HeapValue::Array(arr)), Some(idx)) if idx >= 0 => {
                            match arr.get(idx as usize) {
                                Some(elem) => self.make_some(elem.clone()),
                                None => self.make_none(),
                            }
                        }
                        (Some(HeapValue::Range(start, end)), Some(idx)) if idx >= 0 => {
                            match range_elem(*start, *end, idx) {
                                Some(elem) => self.make_some(Value::int(elem)),
                                None => self.make_none(),
                            }
                        }
                        _ => self.make_none(),
                    };
                    self.stack.push(v);
                }
                Op::IndexOrElse => {
                    // Fused `arr[i] or default`. In-bounds: push the element and
                    // fall through (the compiler emits `Jump end` next, skipping
                    // the recovery body). Out-of-bounds: jump to `operand`, the
                    // recovery body. Never builds an `Option` box.
                    let idx_val = self.pop()?;
                    let arr_val = self.pop()?;
                    match (arr_val.as_heap(), idx_val.as_int()) {
                        (Some(HeapValue::Array(arr)), Some(idx)) if idx >= 0 => {
                            match arr.get(idx as usize) {
                                Some(elem) => self.stack.push(elem.clone()),
                                None => ip = instr.operand - code_start,
                            }
                        }
                        (Some(HeapValue::Range(start, end)), Some(idx)) if idx >= 0 => {
                            match range_elem(*start, *end, idx) {
                                Some(elem) => self.stack.push(Value::int(elem)),
                                None => ip = instr.operand - code_start,
                            }
                        }
                        _ => ip = instr.operand - code_start,
                    }
                }
                Op::ElemAt => {
                    let arr_val = self.pop()?;
                    let idx = instr.operand as i64;
                    let v = match arr_val.as_heap() {
                        Some(HeapValue::Array(arr)) if idx >= 0 => match arr.get(idx as usize) {
                            Some(elem) => elem.clone(),
                            None => {
                                return Err(format!(
                                    "Array index {} out of bounds (len: {}). \
                                     This is likely a compiler bug.",
                                    idx,
                                    arr.len()
                                ));
                            }
                        },
                        Some(HeapValue::Range(start, end)) if idx >= 0 => {
                            match range_elem(*start, *end, idx) {
                                Some(elem) => Value::int(elem),
                                None => {
                                    return Err(format!(
                                        "Range index {} out of bounds (len: {}). \
                                         This is likely a compiler bug.",
                                        idx,
                                        range_len(*start, *end)
                                    ));
                                }
                            }
                        }
                        _ => {
                            return Err(format!(
                                "Cannot index non-array type '{}'. \
                                 This is likely a compiler bug.",
                                value_type_name(&arr_val)
                            ));
                        }
                    };
                    self.stack.push(v);
                }
                Op::ArrayLen => {
                    let arr_val = self.pop()?;
                    match arr_val.as_heap() {
                        Some(HeapValue::Array(a)) => self.stack.push(Value::int(a.len() as i64)),
                        Some(HeapValue::Range(s, e)) => {
                            self.stack.push(Value::int(range_len(*s, *e)))
                        }
                        Some(HeapValue::Tuple(t)) => {
                            self.stack.push(Value::int(t.elements.len() as i64))
                        }
                        _ => {
                            return Err(format!(
                                "Cannot get length of non-array type '{}'",
                                value_type_name(&arr_val)
                            ));
                        }
                    }
                }
                Op::ArraySlice => {
                    let end_val = self.pop()?;
                    let start_val = self.pop()?;
                    let arr_val = self.pop()?;

                    let (Some(start), Some(end)) = (start_val.as_int(), end_val.as_int()) else {
                        return Err("Slice indices must be integers".to_string());
                    };
                    match arr_val.as_heap() {
                        Some(HeapValue::Array(arr)) => {
                            if start >= 0 && (end as usize) <= arr.len() && start <= end {
                                let sliced = arr.skip(start as usize).take((end - start) as usize);
                                self.stack.push(Value::array_seq(sliced));
                            } else {
                                return Err(format!(
                                    "Slice indices out of bounds: [{}..{}] (array length is {})",
                                    start,
                                    end,
                                    arr.len()
                                ));
                            }
                        }
                        Some(HeapValue::Range(rs, re)) => {
                            let len = range_len(*rs, *re);
                            if start >= 0 && end <= len && start <= end {
                                self.stack.push(Value::range(rs + start, rs + end));
                            } else {
                                return Err(format!(
                                    "Slice indices out of bounds: [{}..{}] (range length is {})",
                                    start, end, len
                                ));
                            }
                        }
                        _ => {
                            return Err(format!(
                                "Cannot slice non-array type '{}'",
                                value_type_name(&arr_val)
                            ));
                        }
                    }
                }
                Op::ArrayConcat => {
                    let arr2_val = self.pop()?;
                    let arr1_val = self.pop()?;
                    // `into_seq` consumes each operand, so a uniquely-owned
                    // left array is moved out and `append`ed in place (no
                    // structural clone / COW spine copy); an aliased array
                    // falls back to the O(1)-clone path. Range/non-array
                    // routing + error messages are preserved by `into_seq`.
                    let mut a = into_seq(arr1_val)?;
                    a.append(into_seq(arr2_val)?);
                    self.stack.push(Value::array_seq(a));
                }
                Op::Prepend => {
                    let k = instr.operand as usize;
                    let seq_val = self.pop()?;
                    let mut seq = into_seq(seq_val)?;
                    // Stack below `seq` holds e0..e_{k-1} (e0 pushed first), so
                    // popping yields e_{k-1}..e0; push_front each so the final
                    // order is [e0, e1, .., e_{k-1}, ..seq].
                    for _ in 0..k {
                        let e = self.pop()?;
                        seq.push_front(e);
                    }
                    self.stack.push(Value::array_seq(seq));
                }
                Op::Drop => {
                    let n_val = self.pop()?;
                    let seq_val = self.pop()?;
                    let Some(n) = n_val.as_int() else {
                        return Err("Drop count must be an integer".to_string());
                    };
                    let n = n.max(0);
                    // Dropping n from a lazy `s..e` Range is the slice [n, len)
                    // = `s+n .. e`; keep it O(1) instead of materializing via
                    // `into_seq`. Array/non-array routing matches the producer
                    // side (consume sole-owner Array, else identical error).
                    if let Some(HeapValue::Range(s, e)) = seq_val.as_heap() {
                        let len = range_len(*s, *e);
                        let n = n.min(len);
                        self.stack.push(Value::range(s + n, *e));
                        continue;
                    }
                    let mut seq = into_seq(seq_val)?;
                    let n = (n as usize).min(seq.len());
                    for _ in 0..n {
                        seq.pop_front();
                    }
                    self.stack.push(Value::array_seq(seq));
                }
                Op::Append => {
                    let m = instr.operand as usize;
                    let tail = self.pop_n(m)?;
                    let seq_val = self.pop()?;
                    let mut seq = into_seq(seq_val)?;
                    for e in tail {
                        seq.push_back(e);
                    }
                    self.stack.push(Value::array_seq(seq));
                }
                Op::GetField => {
                    let val = self.pop()?;
                    let idx = instr.operand;
                    if let Some(ev) = val.as_enum() {
                        if idx >= 0 && (idx as usize) < ev.payload.len() {
                            self.stack.push(ev.payload[idx as usize].clone());
                        } else {
                            return Err(format!(
                                "Field index {} out of bounds on {}.{} (len: {})",
                                idx,
                                ev.enum_name,
                                ev.variant_name,
                                ev.payload.len()
                            ));
                        }
                    } else {
                        return Err(format!(
                            "Cannot access field on '{}'",
                            value_type_name(&val)
                        ));
                    }
                }
                Op::MakeClosure => {
                    let func_idx = instr.operand;
                    let func = self.program.functions[func_idx as usize].clone();

                    let cc = func.capture_count as usize;
                    // The one place captures are materialized: build the shared
                    // `Rc<[Value]>` once here so every later invocation only
                    // bumps the refcount.
                    let captures: Rc<[Value]> = self.pop_n(cc)?.into();

                    self.stack.push(Value::closure(ClosureValue {
                        func_idx,
                        captures,
                        name: func.name,
                    }));
                }
                Op::PushCapture => {
                    let capture_idx = instr.operand as usize;
                    let v = match self.frame().captures.get(capture_idx) {
                        Some(v) => v.clone(),
                        None => {
                            return Err(format!("Capture index out of bounds: {}", capture_idx));
                        }
                    };
                    self.stack.push(v);
                }
                Op::PushSelf => {
                    let func_idx = self.frame().func_idx;
                    let val = match &self.self_closures[func_idx as usize] {
                        // Capture-free fn (every top-level fn): clone the
                        // cached Rc — refcount bump, zero allocation.
                        Some(cached) => Value::closure_rc(cached.clone()),
                        // Capture-carrying closure: rebuild from the frame.
                        None => Value::closure(ClosureValue {
                            func_idx,
                            captures: self.frame().captures.clone(),
                            name: self.program.functions[func_idx as usize].name.clone(),
                        }),
                    };
                    self.stack.push(val);
                }
                Op::Print => {
                    let val = self.pop()?;
                    println!("{}", inspect(&val));
                }
                Op::StackDepth => {
                    self.stack.push(Value::int(self.frames.len() as i64));
                }
                Op::MakeEnumPayload => {
                    let payload_count = instr.b as usize;
                    let payloads = self.pop_n(payload_count)?;

                    let labels_val = self.pop()?;
                    let variant_name_val = self.pop()?;
                    let enum_name_val = self.pop()?;
                    let type_id_val = self.pop()?;

                    let Some(type_id) = type_id_val.as_int() else {
                        return Err("Enum type id must be int".to_string());
                    };
                    let type_id = type_id as i32;

                    let Some(enum_name) = enum_name_val.as_str() else {
                        return Err("Enum name must be string".to_string());
                    };

                    // The field-label array is a per-ctor-site pooled constant
                    // (`emit_construct_header` → `add_constant(Value::array)`),
                    // and `PushConst` clones constants by `Rc` bump, so the
                    // popped `Rc<Seq>` is pointer-identical to the constant-pool
                    // entry and stable for the VM's lifetime (constants are
                    // never mutated during `run`). Convert it to the shared
                    // `Rc<[Rc<str>]>` once per site and memoize by that address;
                    // every later construction at the site clones one `Rc` (a
                    // refcount bump) instead of re-walking the array into a
                    // fresh `Vec` + boxed slice.
                    let labels_key = match labels_val.as_array() {
                        Some(seq) => Rc::as_ptr(seq) as usize,
                        None => return Err("Field labels must be an array".to_string()),
                    };
                    let field_labels = match self.label_cache.get(&labels_key) {
                        Some(cached) => cached.clone(),
                        None => {
                            let labels = expect_string_array(labels_val)?;
                            self.label_cache.insert(labels_key, labels.clone());
                            labels
                        }
                    };

                    let Some(variant_name) = variant_name_val.as_str() else {
                        return Err("Variant name must be string".to_string());
                    };

                    // `instr.operand` indexes the constant pool at the
                    // compile-time `enum_name_prefix_hash(enum_name,
                    // variant_name)`. Folding only the payload hashes here is
                    // bit-identical to hashing the name bytes then the
                    // payloads in one pass, without re-walking the static
                    // name bytes. The prehash constant is read by reference —
                    // unlike the header it is never `PushConst`-ed, so there is
                    // no extra opcode dispatch, no stack push/pop, and no
                    // per-construction refcount churn on the path.
                    let name_prefix_hash =
                        self.program.constants[instr.operand as usize].as_int_unchecked() as u64;
                    let hash = enum_hash_with_payload(name_prefix_hash, &payloads);
                    // `enum_name`/`variant_name` are `Rc<str>` shared with the
                    // bytecode constant pool — clone is a refcount bump, not a
                    // fresh string allocation per construction.
                    self.stack.push(Value::enum_(EnumValue {
                        type_id,
                        enum_name: enum_name.clone(),
                        variant_name: variant_name.clone(),
                        field_labels,
                        payload: payloads,
                        hash,
                    }));
                }
                Op::MatchEnum => {
                    let variant_name = self.pop()?;
                    let type_id_val = self.pop()?;
                    let val = self.pop()?;

                    let Some(variant_str) = variant_name.as_str() else {
                        return Err("Enum/variant names must be strings".to_string());
                    };
                    let Some(type_id) = type_id_val.as_int() else {
                        return Err("Enum type id must be int".to_string());
                    };
                    let type_id = type_id as i32;

                    if let Some(ev) = val.as_enum() {
                        self.stack.push(Value::bool(
                            ev.type_id == type_id && *ev.variant_name == **variant_str,
                        ));
                    } else {
                        self.stack.push(Value::bool(false));
                    }
                }
                Op::UnwrapEnum => {
                    let enum_val = self.pop()?;
                    if let Some(ev) = enum_val.as_enum() {
                        for p in ev.payload.iter() {
                            self.stack.push(p.clone());
                        }
                    } else {
                        return Err("Cannot unwrap non-enum value".to_string());
                    }
                }
                Op::ToString => {
                    let val = self.pop()?;
                    // Already-Str values stringify to a byte-identical Rc<str>;
                    // skip the inspect()+Value::str round-trip (2 allocs + 2
                    // copies) and push the existing handle straight back.
                    if val.as_str().is_some() {
                        self.stack.push(val);
                    } else {
                        self.stack.push(Value::str(inspect(&val)));
                    }
                }
                Op::StrConcatN => {
                    let n = instr.operand as usize;
                    let len = self.stack.len();
                    if n > len {
                        return Err("Stack underflow. This is likely a compiler bug.".to_string());
                    }
                    let base = len - n;
                    let mut total = 0usize;
                    for v in &self.stack[base..] {
                        match v.as_str() {
                            Some(s) => total += s.len(),
                            None => return Err("str_concat requires strings".to_string()),
                        }
                    }
                    let mut out = String::with_capacity(total);
                    for v in self.stack.drain(base..) {
                        if let Some(s) = v.as_str() {
                            out.push_str(s);
                        }
                    }
                    self.stack.push(Value::str(out));
                }
                Op::Halt => {
                    break;
                }
                Op::FileRead => {
                    reds -= IO_REDUCTION_COST;
                    let path = self.pop_str("io.read_file")?;
                    let res = std::fs::read(&*path).map(Value::binary);
                    self.push_io_result("read file", res);
                }
                Op::FileWrite => {
                    reds -= IO_REDUCTION_COST;
                    let bin = self.pop_binary("io.write_file")?;
                    let path = self.pop_str("io.write_file")?;
                    if let Some(v) = self.reject_unaligned(bin.bit_len, "file") {
                        self.stack.push(v);
                        continue;
                    }
                    let nil = self.make_nil();
                    let res = std::fs::write(&*path, &*bin.bytes).map(|_| nil);
                    self.push_io_result("write file", res);
                }
                Op::TcpListen => {
                    let port = self.pop_int("net.listen")?;
                    let host = self.pop_str("net.listen")?;
                    let res = TcpListener::bind((&*host, port as u16)).and_then(|listener| {
                        // Non-blocking so accept parks the calling process
                        // instead of stalling every process on this scheduler.
                        listener.set_nonblocking(true)?;
                        let socket_id = self.alloc_socket_id();
                        self.tcp_listeners.insert(socket_id, listener);
                        Ok(Value::socket(SocketValue {
                            id: socket_id,
                            is_listener: true,
                        }))
                    });
                    // A listener may be stored in a top-level binding and used
                    // from any scheduler; share its fd program-wide.
                    if let Some(HeapValue::Socket(sv)) = res.as_ref().ok().and_then(|v| v.as_heap())
                    {
                        self.share_listener(sv.id);
                    }
                    self.push_io_result("listen", res);
                }
                Op::TcpAccept => {
                    reds -= IO_REDUCTION_COST;
                    let sv = self.pop_listener("net.accept")?;
                    if !self.ensure_listener(sv.id) {
                        return Err("Invalid listener socket".to_string());
                    }
                    let accept_res = match self.tcp_listeners.get(&sv.id) {
                        Some(listener) => listener.accept(),
                        None => return Err("Invalid listener socket".to_string()),
                    };
                    match accept_res {
                        Ok((conn, peer)) => {
                            let v = self.adopt_connection(conn, peer);
                            self.stack.push(v);
                        }
                        Err(e) if e.kind() == ErrorKind::WouldBlock => {
                            // No pending connection: park until the listener is
                            // readable, then re-run this instruction.
                            self.stack.push(Value::socket(sv));
                            self.frame_mut().ip = ip - 1;
                            return Ok(Step::Parked(Wait::Readable(sv.id)));
                        }
                        Err(e) => self.push_io_result("accept", Err(e)),
                    }
                }
                Op::TcpConnect => {
                    reds -= IO_REDUCTION_COST;
                    let port = self.pop_int("net.connect")?;
                    let host = self.pop_str("net.connect")?;

                    // Resolve the address. IP literals never block; hostnames
                    // use the system resolver, which does.
                    let addr = match resolve_addr(&host, port as u16) {
                        Ok(addr) => addr,
                        Err(e) => {
                            self.push_io_result("connect", Err(e));
                            continue;
                        }
                    };

                    match start_connect(&addr) {
                        // Local connects can complete immediately.
                        Ok(ConnectStart::Connected(stream)) => {
                            let v = self.adopt_connection(stream, addr);
                            self.stack.push(v);
                        }
                        Ok(ConnectStart::Pending(socket)) => {
                            // Park until the socket signals completion via
                            // writability; the completion handler pushes the
                            // result, and execution resumes after this
                            // instruction.
                            let id = self.alloc_socket_id();
                            self.pending_connects.insert(id, socket);
                            self.frame_mut().ip = ip;
                            return Ok(Step::Parked(Wait::Connecting(id)));
                        }
                        Err(e) => self.push_io_result("connect", Err(e)),
                    }
                }
                Op::TcpRead => {
                    reds -= IO_REDUCTION_COST;
                    let max = self.pop_int("socket.read")?;
                    let sock_val = self.pop()?;
                    let sv = connection_socket(&sock_val, "socket.read")?;
                    // At least 1 byte, at most 8 MiB per read.
                    let max = (max.max(1) as usize).min(8 * 1024 * 1024);
                    if self.read_scratch.len() < max {
                        self.read_scratch.resize(max, 0);
                    }
                    let read_res = match self.tcp_connections.get_mut(&sv.id) {
                        Some(conn) => conn.read(&mut self.read_scratch[..max]),
                        None => return Err("Invalid connection socket".to_string()),
                    };
                    match read_res {
                        Ok(n) => {
                            let data = Value::binary(self.read_scratch[..n].to_vec());
                            let ok = self.make_ok(data);
                            self.stack.push(ok);
                        }
                        Err(e) if e.kind() == ErrorKind::WouldBlock => {
                            // Nothing to read yet: park until readable, then
                            // re-run this instruction.
                            self.stack.push(sock_val);
                            self.stack.push(Value::int(max as i64));
                            self.frame_mut().ip = ip - 1;
                            return Ok(Step::Parked(Wait::Readable(sv.id)));
                        }
                        Err(e) => self.push_io_result("read", Err(e)),
                    }
                }
                Op::TcpWrite => {
                    reds -= IO_REDUCTION_COST;
                    let bin = self.pop_binary("socket.write")?;
                    let sock_val = self.pop()?;
                    let sv = connection_socket(&sock_val, "socket.write")?;
                    if let Some(v) = self.reject_unaligned(bin.bit_len, "socket") {
                        self.stack.push(v);
                        continue;
                    }
                    let conn = match self.tcp_connections.get_mut(&sv.id) {
                        Some(conn) => conn,
                        None => return Err("Invalid connection socket".to_string()),
                    };
                    // Write what the socket will take. If it fills up mid-way,
                    // park and resume this instruction with the remaining bytes.
                    let mut written = 0;
                    let mut park_remaining: Option<Vec<u8>> = None;
                    let result = loop {
                        if written == bin.bytes.len() {
                            break Ok(());
                        }
                        match conn.write(&bin.bytes[written..]) {
                            Ok(0) => {
                                break Err(std::io::Error::new(
                                    ErrorKind::WriteZero,
                                    "connection closed",
                                ));
                            }
                            Ok(n) => written += n,
                            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                                park_remaining = Some(bin.bytes[written..].to_vec());
                                break Ok(());
                            }
                            Err(e) if e.kind() == ErrorKind::Interrupted => {}
                            Err(e) => break Err(e),
                        }
                    };
                    if let Some(remaining) = park_remaining {
                        self.stack.push(sock_val);
                        self.stack.push(Value::binary(remaining));
                        self.frame_mut().ip = ip - 1;
                        return Ok(Step::Parked(Wait::Writable(sv.id)));
                    }
                    let nil = self.make_nil();
                    self.push_io_result("write", result.map(|_| nil));
                }
                Op::TcpWriteParts => {
                    reds -= IO_REDUCTION_COST;
                    let parts_val = self.pop()?;
                    let sock_val = self.pop()?;
                    let sv = connection_socket(&sock_val, "socket.write_parts")?;
                    let Some(parts) = parts_val.as_array() else {
                        return Err(
                            "socket.write_parts requires an Array(Binary). This is likely a compiler bug."
                                .to_string(),
                        );
                    };

                    // Collect the parts, rejecting non-byte-aligned binaries.
                    let mut bins: Vec<Rc<BinaryValue>> = Vec::with_capacity(parts.len());
                    let mut unaligned = false;
                    for p in parts.iter() {
                        match p.as_heap() {
                            Some(HeapValue::Binary(b)) => {
                                if !b.bit_len.is_multiple_of(8) {
                                    unaligned = true;
                                    break;
                                }
                                bins.push(b.clone());
                            }
                            _ => {
                                return Err(
                                    "socket.write_parts requires an Array(Binary). This is likely a compiler bug."
                                        .to_string(),
                                );
                            }
                        }
                    }
                    if unaligned {
                        let err = self
                            .make_err(Value::str("cannot write non-byte-aligned binary to socket"));
                        self.stack.push(err);
                        continue;
                    }

                    let conn = match self.tcp_connections.get_mut(&sv.id) {
                        Some(conn) => conn,
                        None => return Err("Invalid connection socket".to_string()),
                    };

                    // Vectored write: every part goes to the kernel in one
                    // syscall per pass. Partial progress advances (idx, offset);
                    // WouldBlock parks and resumes with only the unwritten tail.
                    let mut idx = 0usize;
                    let mut offset = 0usize;
                    let mut park = false;
                    let result = loop {
                        if idx == bins.len() {
                            break Ok(());
                        }
                        let mut slices: Vec<IoSlice> = Vec::with_capacity(bins.len() - idx);
                        slices.push(IoSlice::new(&bins[idx].bytes[offset..]));
                        for b in &bins[idx + 1..] {
                            slices.push(IoSlice::new(&b.bytes));
                        }
                        match conn.write_vectored(&slices) {
                            Ok(0) => {
                                break Err(std::io::Error::new(
                                    ErrorKind::WriteZero,
                                    "connection closed",
                                ));
                            }
                            Ok(mut n) => {
                                while n > 0 && idx < bins.len() {
                                    let left = bins[idx].bytes.len() - offset;
                                    if n >= left {
                                        n -= left;
                                        idx += 1;
                                        offset = 0;
                                    } else {
                                        offset += n;
                                        n = 0;
                                    }
                                }
                            }
                            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                                park = true;
                                break Ok(());
                            }
                            Err(e) if e.kind() == ErrorKind::Interrupted => {}
                            Err(e) => break Err(e),
                        }
                    };

                    if park {
                        // Re-run with only the unwritten tail.
                        let mut remaining: Vec<Value> = Vec::with_capacity(bins.len() - idx);
                        remaining.push(Value::binary(bins[idx].bytes[offset..].to_vec()));
                        for b in &bins[idx + 1..] {
                            remaining.push(Value::binary(b.bytes.to_vec()));
                        }
                        self.stack.push(sock_val);
                        self.stack.push(Value::array(remaining));
                        self.frame_mut().ip = ip - 1;
                        return Ok(Step::Parked(Wait::Writable(sv.id)));
                    }
                    let nil = self.make_nil();
                    self.push_io_result("write", result.map(|_| nil));
                }
                Op::TcpClose => {
                    reds -= IO_REDUCTION_COST;
                    let sv = self.pop_connection("socket.close")?;
                    if let Some(conn) = self.tcp_connections.remove(&sv.id)
                        && let Some(poller) = &self.poller
                    {
                        // Deregister before dropping; ignore "wasn't registered".
                        let _ = poller.delete(&conn);
                    }
                    let nil = self.make_nil();
                    let v = self.make_ok(nil);
                    self.stack.push(v);
                }
                Op::TcpCloseServer => {
                    let sv = self.pop_listener("net.close")?;
                    if let Some(listener) = self.tcp_listeners.remove(&sv.id)
                        && let Some(poller) = &self.poller
                    {
                        let _ = poller.delete(&listener);
                    }
                    // Closing a server retires it everywhere.
                    if let Some(rt) = &self.runtime {
                        sched::lock(&rt.shared_listeners).remove(&sv.id);
                    }
                    let nil = self.make_nil();
                    let v = self.make_ok(nil);
                    self.stack.push(v);
                }
                Op::TcpLocalAddr => {
                    let sv = self.pop_listener("net.local_addr")?;
                    if !self.ensure_listener(sv.id) {
                        return Err("Invalid listener socket".to_string());
                    }
                    let res = match self.tcp_listeners.get(&sv.id) {
                        Some(l) => l.local_addr().map(|a| self.templates.socket_address(a)),
                        None => return Err("Invalid listener socket".to_string()),
                    };
                    self.push_io_result("get local address", res);
                }
                Op::ProcessSpawn => {
                    let f = self.pop()?;
                    self.spawn_process(f)?;
                    let nil = self.make_nil();
                    self.stack.push(nil);
                }
                Op::Sleep => {
                    let ms = self.pop_int("scheduler.sleep")?;
                    let nil = self.make_nil();
                    self.stack.push(nil);
                    if ms > 0 {
                        // The Nil result is already pushed, so on wake the
                        // process resumes at the next instruction.
                        self.frame_mut().ip = ip;
                        let deadline = Instant::now() + Duration::from_millis(ms as u64);
                        return Ok(Step::Parked(Wait::Until(deadline)));
                    }
                }
                Op::StrSplit => {
                    let delim = self.pop_str("strings.split")?;
                    let s = self.pop_str("strings.split")?;
                    let parts: Vec<Value> = if delim.is_empty() {
                        s.chars().map(|c| Value::str(c.to_string())).collect()
                    } else {
                        s.split(&*delim).map(Value::str).collect()
                    };
                    self.stack.push(Value::array(parts));
                }
                Op::StrLen => {
                    let s = self.pop_str("strings.length")?;
                    self.stack.push(Value::int(s.chars().count() as i64));
                }
                Op::StrContains => {
                    let n = self.pop_str("strings.contains")?;
                    let h = self.pop_str("strings.contains")?;
                    self.stack.push(Value::bool(h.contains(&*n)));
                }
                Op::StrTrim => {
                    let s = self.pop_str("strings.trim")?;
                    self.stack.push(Value::str(s.trim()));
                }
                Op::IntToString => {
                    let n = self.pop_int("int.to_string")?;
                    self.stack.push(Value::str(n.to_string()));
                }
                Op::BinFromString => {
                    let s = self.pop_str("binary.from_string")?;
                    self.stack.push(Value::binary(s.as_bytes().to_vec()));
                }
                Op::BinToString => {
                    let bin = self.pop_binary("binary.to_string")?;
                    let v = if bin.bit_len % 8 != 0 {
                        self.make_err(self.make_nil())
                    } else {
                        match std::str::from_utf8(&bin.bytes) {
                            Ok(s) => self.make_ok(Value::str(s)),
                            Err(_) => self.make_err(self.make_nil()),
                        }
                    };
                    self.stack.push(v);
                }
                Op::BinBitSize => {
                    let bin = self.pop_binary("binary.bit_size")?;
                    self.stack.push(Value::int(bin.bit_len as i64));
                }
                Op::BinByteSize => {
                    let bin = self.pop_binary("binary.byte_size")?;
                    self.stack.push(Value::int(bin.bit_len.div_ceil(8) as i64));
                }
                Op::BinSlice => {
                    let take = self.pop_int("binary.slice")?;
                    let at = self.pop_int("binary.slice")?;
                    let bin = self.pop_binary("binary.slice")?;
                    let v = if at < 0 || take < 0 || (at as u64) + (take as u64) > bin.bit_len {
                        self.make_err(self.make_nil())
                    } else {
                        let out = binary::slice(&bin.bytes, at as u64, take as u64);
                        self.make_ok(Value::binary_bits(out, take as u64))
                    };
                    self.stack.push(v);
                }
                Op::BinAppend => {
                    let b = self.pop_binary("binary.append")?;
                    let a = self.pop_binary("binary.append")?;
                    let out = binary::append(&a.bytes, a.bit_len, &b.bytes, b.bit_len);
                    self.stack
                        .push(Value::binary_bits(out, a.bit_len + b.bit_len));
                }
                // `<<v:n>>` — encode the low `n` bits of an integer.
                Op::BinFromInt => {
                    let num_bits = self.pop_int("binary segment width")?;
                    let value = self.pop_int("binary segment value")?;
                    let nb = num_bits.max(0) as u64;
                    self.stack
                        .push(Value::binary_bits(binary::from_int(value, nb), nb));
                }
                // `<<a:n>>` pattern — decode `n` bits at an offset as an Int.
                Op::BinReadInt => {
                    let num_bits = self.pop_int("binary segment width")?;
                    let at = self.pop_int("binary segment offset")?;
                    let bin = self.pop_binary("binary pattern")?;
                    let v = binary::read_int(
                        &bin.bytes,
                        bin.bit_len,
                        at.max(0) as u64,
                        num_bits.max(0) as u64,
                    );
                    self.stack.push(Value::int(v));
                }
                // `<<x:bytes(n)>>` — splice the first `min(n, len)` bits.
                Op::BinTake => {
                    let n = self.pop_int("binary segment width")?;
                    let bin = self.pop_binary("binary segment")?;
                    let take = (n.max(0) as u64).min(bin.bit_len);
                    self.stack
                        .push(Value::binary_bits(binary::slice(&bin.bytes, 0, take), take));
                }
                // `<<c:utf8>>` pattern — decode one codepoint; pushes the
                // codepoint and the bits consumed (0,0 on decode failure).
                Op::BinReadUtf8 => {
                    let at = self.pop_int("binary segment offset")?;
                    let bin = self.pop_binary("binary pattern")?;
                    let (cp, nbits) = binary::read_utf8(&bin.bytes, bin.bit_len, at.max(0) as u64)
                        .map_or((0, 0), |(c, n)| (c as i64, n as i64));
                    self.stack.push(Value::int(cp));
                    self.stack.push(Value::int(nbits));
                }
                // Float→Int casts use saturating `as i64`; Value floats are
                // canonicalized finite (no NaN/Inf), so these stay total.
                Op::FloatFloor => {
                    let f = self.pop_float("float.floor")?;
                    self.stack.push(Value::int(f.floor() as i64));
                }
                Op::FloatCeil => {
                    let f = self.pop_float("float.ceil")?;
                    self.stack.push(Value::int(f.ceil() as i64));
                }
                Op::FloatRound => {
                    let f = self.pop_float("float.round")?;
                    self.stack.push(Value::int(f.round() as i64));
                }
                Op::FloatTruncate => {
                    let f = self.pop_float("float.truncate")?;
                    self.stack.push(Value::int(f.trunc() as i64));
                }
                Op::FloatFromInt => {
                    let n = self.pop_int("float.from_int")?;
                    self.stack.push(Value::float(n as f64));
                }
                Op::FloatToString => {
                    let f = self.pop_float("float.to_string")?;
                    self.stack.push(Value::str(f64_str(f)));
                }
            }
        }

        // Halt, Ret with no caller, or running off the end of the code: the
        // current process is finished and its result is its top-of-stack.
        Ok(Step::Done)
    }

    fn pop(&mut self) -> VmResult<Value> {
        self.stack
            .pop()
            .ok_or_else(|| "Stack underflow. This is likely a compiler bug.".to_string())
    }

    fn pop_n(&mut self, n: usize) -> VmResult<Vec<Value>> {
        let len = self.stack.len();
        if n > len {
            return Err("Stack underflow. This is likely a compiler bug.".to_string());
        }
        Ok(self.stack.split_off(len - n))
    }

    fn peek(&self) -> VmResult<&Value> {
        self.stack
            .last()
            .ok_or_else(|| "Stack underflow. This is likely a compiler bug.".to_string())
    }

    // --- Typed pop helpers (stdlib / I/O ops only — not the arithmetic loop) --

    #[inline]
    fn pop_str(&mut self, op: &str) -> VmResult<Rc<str>> {
        match self.pop()?.as_str() {
            Some(s) => Ok(s.clone()),
            None => Err(format!("{op} requires a String")),
        }
    }

    #[inline]
    fn pop_int(&mut self, op: &str) -> VmResult<i64> {
        match self.pop()?.as_int() {
            Some(n) => Ok(n),
            None => Err(format!("{op} requires an Int")),
        }
    }

    /// Pop a numeric value as `f64`. Accepts Int too (coerced), mirroring the
    /// numeric tolerance of the arithmetic loop (`Op::Neg` etc.).
    #[inline]
    fn pop_float(&mut self, op: &str) -> VmResult<f64> {
        let v = self.pop()?;
        if let Some(f) = v.as_float() {
            Ok(f)
        } else if let Some(n) = v.as_int() {
            Ok(n as f64)
        } else {
            Err(format!("{op} requires a Float"))
        }
    }

    #[inline]
    fn pop_binary(&mut self, op: &str) -> VmResult<Rc<BinaryValue>> {
        match self.pop()?.as_heap() {
            Some(HeapValue::Binary(b)) => Ok(b.clone()),
            _ => Err(format!("{op} requires a Binary")),
        }
    }

    #[inline]
    fn pop_listener(&mut self, op: &str) -> VmResult<SocketValue> {
        match self.pop()?.as_heap() {
            Some(HeapValue::Socket(s)) => Ok(*s),
            _ => Err(format!("{op} requires a Server")),
        }
    }

    /// `Socket` is the AL record `{ conn Connection, peer SocketAddress }`;
    /// the underlying `SocketValue` is the first payload field.
    fn pop_connection(&mut self, op: &str) -> VmResult<SocketValue> {
        let v = self.pop()?;
        if let Some(e) = v.as_enum()
            && let Some(HeapValue::Socket(s)) = e.payload.first().and_then(|c| c.as_heap())
        {
            return Ok(*s);
        }
        Err(format!("{op} requires a Socket"))
    }

    // --- Prelude value constructors ------------------------------------------

    #[inline]
    fn make_nil(&self) -> Value {
        Value::enum_rc(Rc::clone(&self.templates.nil))
    }

    #[inline]
    fn make_none(&self) -> Value {
        Value::enum_rc(Rc::clone(&self.templates.none))
    }

    #[inline]
    fn make_some(&self, v: Value) -> Value {
        PreludeTemplates::with_payload(&self.templates.some, v)
    }

    #[inline]
    fn make_ok(&self, v: Value) -> Value {
        PreludeTemplates::with_payload(&self.templates.ok, v)
    }

    #[inline]
    fn make_err(&self, v: Value) -> Value {
        PreludeTemplates::with_payload(&self.templates.err, v)
    }

    // --- I/O helpers ---------------------------------------------------------

    #[inline]
    fn push_io_result(&mut self, label: &str, r: std::io::Result<Value>) {
        let v = match r {
            Ok(v) => self.make_ok(v),
            Err(err) => self.make_err(Value::str(format!("Failed to {}: {}", label, err))),
        };
        self.stack.push(v);
    }

    #[inline]
    fn reject_unaligned(&self, bits: u64, dest: &str) -> Option<Value> {
        (!bits.is_multiple_of(8)).then(|| {
            self.make_err(Value::str(format!(
                "cannot write non-byte-aligned binary to {dest}"
            )))
        })
    }

    // --- Lightweight processes (al/experiments/scheduler) --------------------

    /// Allocate a socket id that is unique across every scheduler: the
    /// scheduler index lives in the top bits, so sockets can move between
    /// schedulers (inside spawn seeds) without colliding.
    #[inline]
    fn alloc_socket_id(&mut self) -> i32 {
        let id = ((self.scheduler_index as i32) << 24) | self.next_socket_id;
        self.next_socket_id += 1;
        id
    }

    /// Make a listener visible to every scheduler, so a listener stored in a
    /// top-level binding can be used from spawned processes anywhere.
    fn share_listener(&mut self, id: i32) {
        let Some(rt) = &self.runtime else {
            return;
        };
        if let Some(l) = self.tcp_listeners.get(&id)
            && let Ok(dup) = l.try_clone()
        {
            sched::lock(&rt.shared_listeners).insert(id, dup);
        }
    }

    /// Resolve `id` to a listener in this scheduler's table, hydrating its fd
    /// from the runtime's shared listeners on a local miss.
    fn ensure_listener(&mut self, id: i32) -> bool {
        if self.tcp_listeners.contains_key(&id) {
            return true;
        }
        let dup = match &self.runtime {
            Some(rt) => sched::lock(&rt.shared_listeners)
                .get(&id)
                .and_then(|l| l.try_clone().ok()),
            None => None,
        };
        match dup {
            Some(l) => {
                self.tcp_listeners.insert(id, l);
                true
            }
            None => false,
        }
    }

    /// Take ownership of a connected stream (from accept or connect):
    /// configure it, register it in the connection table, and build the AL
    /// `Ok(Socket)` result.
    fn adopt_connection(&mut self, stream: TcpStream, peer: std::net::SocketAddr) -> Value {
        let _ = stream.set_nonblocking(true);
        // Small request/response exchanges should not sit in Nagle's buffer
        // waiting for an ACK.
        let _ = stream.set_nodelay(true);
        let id = self.alloc_socket_id();
        self.tcp_connections.insert(id, stream);
        let handle = Value::socket(SocketValue {
            id,
            is_listener: false,
        });
        let v = self.templates.make_socket(handle, peer);
        self.make_ok(v)
    }

    /// `scheduler.spawn(f)`: start a new process running the closure `f`.
    ///
    /// The first spawn boots the multi-core runtime (one scheduler per CPU
    /// core). With the runtime up, the process is shipped as a `Send` seed
    /// that whichever scheduler is free will run; otherwise it is created
    /// directly on this scheduler.
    fn spawn_process(&mut self, f: Value) -> VmResult<()> {
        match f.as_closure() {
            Some(cl) => {
                if self.program.functions[cl.func_idx as usize].arity != 0 {
                    return Err(
                        "spawned functions take no arguments. This is likely a compiler bug."
                            .to_string(),
                    );
                }
            }
            None => {
                return Err("spawn requires a function. This is likely a compiler bug.".to_string());
            }
        }

        // Boot the multi-core runtime on the first spawn (scheduler 0 only;
        // single-core machines and AL_SCHEDULERS=1 stay single-scheduler).
        if self.runtime.is_none() && self.scheduler_index == 0 {
            let count = sched::scheduler_count();
            if count > 1 {
                self.ensure_poller()?;
                if let Some(poller) = &self.poller {
                    let program = Arc::new(transfer::program_out(&self.program));
                    match sched::boot(program, count, Arc::clone(poller)) {
                        Ok(rt) => {
                            // Publish every global written before the runtime
                            // existed so worker schedulers can hydrate them.
                            let mut sink = Vec::new();
                            for (slot, value) in self.globals.iter().enumerate() {
                                rt.publish_global(slot, transfer::value_out(value, &mut sink));
                            }
                            // Same for already-open listeners: share their fds
                            // so top-level listener bindings work everywhere.
                            {
                                let mut shared = sched::lock(&rt.shared_listeners);
                                for (id, l) in &self.tcp_listeners {
                                    if let Ok(dup) = l.try_clone() {
                                        shared.insert(*id, dup);
                                    }
                                }
                            }
                            self.runtime = Some(rt);
                        }
                        Err(e) => {
                            eprintln!("warning: cannot start schedulers ({e}); running on one core")
                        }
                    }
                }
            }
        }

        if self.runtime.is_some() {
            let seed = self.build_seed(&f)?;
            if let Some(rt) = &self.runtime {
                rt.submit(seed);
            }
        } else {
            self.spawn_process_local(f)?;
        }
        Ok(())
    }

    /// Create a process directly on this scheduler.
    ///
    /// The process reads top-level bindings from this scheduler's global
    /// (literal) area, so its stack holds nothing but the closure's own
    /// locals — spawning allocates two small vectors and nothing else.
    fn spawn_process_local(&mut self, f: Value) -> VmResult<()> {
        let Some(cl) = f.as_closure() else {
            return Err("spawn requires a function. This is likely a compiler bug.".to_string());
        };
        let func = &self.program.functions[cl.func_idx as usize];
        let (func_idx, code_start, locals) = (cl.func_idx, func.code_start, func.locals);

        let mut stack = Vec::with_capacity(locals as usize + 8);
        for _ in 0..locals {
            stack.push(Value::int(0));
        }

        let frames = vec![CallFrame {
            func_idx,
            code_start,
            ip: 0,
            base_slot: 0,
            captures: cl.captures.clone(),
        }];

        self.run_queue.push_back(Process {
            stack,
            frames,
            is_main: false,
        });
        Ok(())
    }

    /// Deep-copy a closure into a `Send` seed. Captured sockets transfer with
    /// it: connections move (this scheduler loses them), listeners are dup'd
    /// so both sides keep accepting.
    fn build_seed(&mut self, f: &Value) -> VmResult<Seed> {
        let mut captured_sockets = Vec::new();
        let closure = transfer::value_out(f, &mut captured_sockets);

        let mut listeners = Vec::new();
        let mut connections = Vec::new();
        for id in captured_sockets {
            if let Some(l) = self.tcp_listeners.get(&id) {
                match l.try_clone() {
                    Ok(dup) => listeners.push((id, dup)),
                    Err(e) => eprintln!("spawn: cannot share listener: {e}"),
                }
            }
            if let Some(c) = self.tcp_connections.remove(&id) {
                // The fd is leaving this scheduler; drop any poller registration.
                if let Some(poller) = &self.poller {
                    let _ = poller.delete(&c);
                }
                connections.push((id, c));
            }
        }

        Ok(Seed {
            closure,
            listeners,
            connections,
        })
    }

    /// Build a runnable process on this scheduler from a `Send` seed: adopt
    /// its sockets, rebuild its closure on this heap, and queue it. Top-level
    /// bindings come from the global area (synced in `take_seeds`), so the
    /// seed itself carries none.
    fn hydrate_seed(&mut self, seed: Seed) -> VmResult<()> {
        for (id, l) in seed.listeners {
            self.tcp_listeners.insert(id, l);
        }
        for (id, c) in seed.connections {
            self.tcp_connections.insert(id, c);
        }

        let closure = transfer::value_in(&seed.closure);
        self.spawn_process_local(closure)
    }

    /// Register interest in I/O readiness with the OS poller before parking.
    fn register_wait(&mut self, wait: &Wait) -> VmResult<()> {
        let (id, readable) = match wait {
            Wait::Readable(id) => (*id, true),
            Wait::Writable(id) | Wait::Connecting(id) => (*id, false),
            Wait::Until(_) => return Ok(()),
        };

        self.ensure_poller()?;
        let Some(poller) = &self.poller else {
            return Ok(());
        };

        let event = if readable {
            polling::Event::readable(id as usize)
        } else {
            polling::Event::writable(id as usize)
        };

        // The fd lives in one of the socket tables; sockets stay in those
        // tables (and thus alive) for as long as they are registered — close
        // deregisters before dropping.
        if let Some(listener) = self.tcp_listeners.get(&id) {
            // SAFETY: see above — the listener outlives its registration.
            unsafe { poller.add(listener, event) }
                .or_else(|e| {
                    if e.kind() == ErrorKind::AlreadyExists {
                        poller.modify(listener, event)
                    } else {
                        Err(e)
                    }
                })
                .map_err(|e| format!("cannot watch listener: {e}"))
        } else if let Some(conn) = self.tcp_connections.get(&id) {
            // SAFETY: see above — the connection outlives its registration.
            unsafe { poller.add(conn, event) }
                .or_else(|e| {
                    if e.kind() == ErrorKind::AlreadyExists {
                        poller.modify(conn, event)
                    } else {
                        Err(e)
                    }
                })
                .map_err(|e| format!("cannot watch connection: {e}"))
        } else if let Some(pending) = self.pending_connects.get(&id) {
            // SAFETY: see above — the pending socket stays in `pending_connects`
            // until its completion handler removes it.
            unsafe { poller.add(pending, event) }
                .or_else(|e| {
                    if e.kind() == ErrorKind::AlreadyExists {
                        poller.modify(pending, event)
                    } else {
                        Err(e)
                    }
                })
                .map_err(|e| format!("cannot watch connecting socket: {e}"))
        } else {
            Err("Invalid socket. This is likely a compiler bug.".to_string())
        }
    }

    /// Move parked processes whose I/O is ready or whose timer has expired
    /// back onto the run queue.
    ///
    /// With `block`, performs one interruptible wait — I/O readiness, the
    /// nearest timer deadline, or a `notify()` from another scheduler all end
    /// it — then returns so the caller can re-check for remote work.
    fn poll_parked(&mut self, block: bool) -> VmResult<()> {
        if self.parked.is_empty() {
            return Ok(());
        }

        // Wake any timers that are already due.
        let woke = self.wake_due_timers();

        let waiting_on_io = self
            .parked
            .iter()
            .any(|(w, _)| matches!(w, Wait::Readable(_) | Wait::Writable(_)));

        if !block || woke {
            // Non-blocking (or something already woke): just drain any ready
            // I/O events and return.
            if waiting_on_io {
                self.drain_io_events(Some(Duration::ZERO))?;
            }
            return Ok(());
        }

        // One blocking wait, bounded by the nearest timer deadline. The poller
        // is used even for timer-only waits so `notify()` can interrupt it.
        let timeout = self
            .parked
            .iter()
            .filter_map(|(w, _)| match w {
                Wait::Until(t) => Some(*t),
                _ => None,
            })
            .min()
            .map(|d| d.saturating_duration_since(Instant::now()));
        self.ensure_poller()?;
        self.drain_io_events(timeout)?;
        self.wake_due_timers();
        Ok(())
    }

    /// Wake parked processes whose deadline has passed.
    fn wake_due_timers(&mut self) -> bool {
        let now = Instant::now();
        let mut woke = false;
        let mut i = 0;
        while i < self.parked.len() {
            if matches!(&self.parked[i].0, Wait::Until(t) if *t <= now) {
                let (_, p) = self.parked.swap_remove(i);
                self.run_queue.push_back(p);
                woke = true;
            } else {
                i += 1;
            }
        }
        woke
    }

    /// Wait on the poller for at most `timeout` (None = until something
    /// happens) and wake every process parked on a socket that became ready.
    fn drain_io_events(&mut self, timeout: Option<Duration>) -> VmResult<()> {
        let Some(poller) = &self.poller else {
            return Ok(());
        };
        let mut events = polling::Events::new();
        poller
            .wait(&mut events, timeout)
            .map_err(|e| format!("scheduler poll failed: {e}"))?;
        for ev in events.iter() {
            let id = ev.key as i32;
            let mut i = 0;
            while i < self.parked.len() {
                let matches_event = match &self.parked[i].0 {
                    Wait::Readable(sid) | Wait::Writable(sid) | Wait::Connecting(sid) => *sid == id,
                    Wait::Until(_) => false,
                };
                if matches_event {
                    let (wait, mut p) = self.parked.swap_remove(i);
                    // A finished connect delivers its result directly onto the
                    // woken process's stack; the connect instruction is not
                    // re-run.
                    if let Wait::Connecting(sid) = wait {
                        let result = self.finish_connect(sid);
                        p.stack.push(result);
                    }
                    self.run_queue.push_back(p);
                } else {
                    i += 1;
                }
            }
        }
        Ok(())
    }

    /// Complete a pending non-blocking connect whose socket became writable:
    /// either adopt the connection or report the error it ended with.
    fn finish_connect(&mut self, id: i32) -> Value {
        let Some(socket) = self.pending_connects.remove(&id) else {
            return self.make_err(Value::str("Failed to connect: unknown socket"));
        };
        if let Some(poller) = &self.poller {
            let _ = poller.delete(&socket);
        }
        // Writability after EINPROGRESS means the connect finished; SO_ERROR
        // says whether it succeeded.
        match socket.take_error() {
            Ok(None) => {
                let stream: TcpStream = socket.into();
                match stream.peer_addr() {
                    Ok(peer) => self.adopt_connection(stream, peer),
                    Err(e) => self.make_err(Value::str(format!("Failed to connect: {e}"))),
                }
            }
            Ok(Some(e)) | Err(e) => self.make_err(Value::str(format!("Failed to connect: {e}"))),
        }
    }
}

/// Entry point for worker scheduler threads (indices 1..N), started by
/// `sched::boot`. Each worker runs a VM of its own against a private
/// hydration of the shared program, pulling seeds from the runtime's injector
/// until the whole program finishes.
fn worker_main(runtime: Arc<Runtime>, index: usize) {
    let program = transfer::program_in(&runtime.program);
    let mut vm = new_vm(program);
    vm.scheduler_index = index;
    vm.poller = Some(Arc::clone(&runtime.pollers[index]));
    vm.runtime = Some(runtime);

    // Workers have no main process; scheduler_loop starts by acquiring work.
    if let Err(e) = vm.scheduler_loop() {
        // A VM error indicates a compiler bug, not a user error; it is fatal
        // to the whole program.
        eprintln!("al: scheduler {index} failed: {e}");
        std::process::exit(1);
    }
}

/// Extract the underlying `SocketValue` from a `Socket` record value without
/// consuming it. `Socket` is the AL record `{ conn Connection, peer ... }`;
/// the raw handle is its first payload field.
/// Resolve `host:port` to a socket address. IP literals never block; hostnames
/// go through the system resolver, which does.
fn resolve_addr(host: &str, port: u16) -> std::io::Result<std::net::SocketAddr> {
    use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, port));
    }
    (host, port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| std::io::Error::new(ErrorKind::NotFound, "host not found"))
}

/// The two ways a non-blocking connect can begin.
enum ConnectStart {
    /// Completed immediately (common for loopback).
    Connected(TcpStream),
    /// In flight; completion is signalled by writability.
    Pending(socket2::Socket),
}

/// Begin a non-blocking TCP connect.
fn start_connect(addr: &std::net::SocketAddr) -> std::io::Result<ConnectStart> {
    let socket = socket2::Socket::new(
        socket2::Domain::for_address(*addr),
        socket2::Type::STREAM,
        None,
    )?;
    socket.set_nonblocking(true)?;
    match socket.connect(&(*addr).into()) {
        Ok(()) => Ok(ConnectStart::Connected(socket.into())),
        // EINPROGRESS (unix) / WouldBlock (windows): completion comes later.
        Err(e) if e.raw_os_error() == Some(libc::EINPROGRESS) => Ok(ConnectStart::Pending(socket)),
        Err(e) if e.kind() == ErrorKind::WouldBlock => Ok(ConnectStart::Pending(socket)),
        Err(e) => Err(e),
    }
}

fn connection_socket(v: &Value, op: &str) -> VmResult<SocketValue> {
    if let Some(e) = v.as_enum()
        && let Some(HeapValue::Socket(s)) = e.payload.first().and_then(|c| c.as_heap())
    {
        return Ok(*s);
    }
    Err(format!("{op} requires a Socket"))
}

/// Construct a Nil enum value without a `VM`. Retained for tests that build
/// values outside an interpreter; hot paths go through `VM::make_nil`.
#[cfg(test)]
fn nil_value(nil_id: i32) -> Value {
    Value::enum_(EnumValue {
        type_id: nil_id,
        enum_name: "Nil".into(),
        variant_name: "Nil".into(),
        field_labels: Rc::from(Vec::<Rc<str>>::new()),
        payload: vec![],
        hash: enum_name_prefix_hash("Nil", "Nil"),
    })
}

/// Consume `v` into an owned `Seq` for in-place push_front/push_back/append.
///
/// Fast path: when `v` is the sole owner of its `Array` — the overwhelmingly
/// common case, e.g. the freshly-built result of a recursive `[f(h), ..rec(t)]`
/// sitting alone on the operand stack — both the outer `Rc<HeapValue>` and the
/// inner `Rc<Seq>` are uniquely owned, so the `Seq` is *moved* out with no
/// structural clone and the caller's `push_*` is amortized O(1) (turning
/// `list.map`/`filter`/`reverse` and repeated `++` from O(n log n) to O(n)).
///
/// `v` must be consumed (`into_heap`) for this to work: while `v` is still
/// borrowed the inner `Rc` strong count is always >= 2, forcing the structural
/// clone + copy-on-write spine copy on every element. An aliased array still
/// falls back to that O(1)-structural-clone + COW path — unchanged, no
/// regression.
fn into_seq(v: Value) -> VmResult<Seq> {
    // Classify without consuming so the non-array / Range paths keep their
    // exact original behavior and error text.
    match v.as_heap() {
        Some(HeapValue::Array(_)) => {}
        Some(HeapValue::Range(s, e)) => {
            return Ok(Seq::from((*s..*e).map(Value::int).collect::<Vec<_>>()));
        }
        _ => {
            return Err(format!(
                "Cannot concatenate non-array type '{}'",
                value_type_name(&v)
            ));
        }
    }
    // `v` is a heap Array; the borrow above has ended so we can consume it.
    // Sole owner (the common case): both Rc layers are unique, so the inner
    // `Seq` is moved out with no structural clone. Aliased: cloning the outer
    // `HeapValue` leaves the inner `Rc` at count 2, so `unwrap_or_clone` takes
    // the existing O(1)-structural-clone + COW path (unchanged, no regression).
    match v.into_heap().map(Rc::unwrap_or_clone) {
        Some(HeapValue::Array(inner)) => Ok(Rc::unwrap_or_clone(inner)),
        // Unreachable: the `as_heap` match above already proved `v` is an
        // `Array`. Return an empty `Seq` rather than panic (the crate denies
        // `clippy::unreachable`).
        _ => Ok(Seq::new()),
    }
}

fn expect_string_array(v: Value) -> VmResult<Rc<[Rc<str>]>> {
    match v.as_array() {
        Some(items) => {
            let mut out: Vec<Rc<str>> = Vec::with_capacity(items.len());
            for it in items.iter() {
                match it.as_str() {
                    // `s` is the constant-pool `Rc<str>`; clone bumps the
                    // refcount instead of copying the label bytes.
                    Some(s) => out.push(s.clone()),
                    None => return Err("Field label must be string".to_string()),
                }
            }
            Ok(out.into())
        }
        None => Err("Field labels must be an array".to_string()),
    }
}

#[inline(always)]
fn float_op(af: f64, bf: f64, op: Op) -> VmResult<Value> {
    // Float arithmetic is TOTAL: AL has no NaN or Infinity in its value space
    // and the VM never exits early. `/0.0` and any non-finite result (overflow
    // to ±Inf, `0.0/0.0` NaN, etc.) collapse to `0.0` — the float analogue of
    // the integer `x / 0 = 0` total-function convention.
    let r = match op {
        Op::Add => af + bf,
        Op::Sub => af - bf,
        Op::Mul => af * bf,
        Op::Div => {
            if bf == 0.0 {
                0.0
            } else {
                af / bf
            }
        }
        // `%` is TOTAL on floats too: `x % 0.0 = x` mirrors the integer
        // `x % 0 = x` convention (preserving `a = (a/b)*b + a%b`); the
        // remainder of finite operands is finite, and `Value::float` collapses
        // any non-finite result to `0.0`.
        Op::Mod => {
            if bf == 0.0 {
                af
            } else {
                af % bf
            }
        }
        _ => return Err("Unknown binary op. This is likely a compiler bug.".to_string()),
    };
    Ok(Value::float(r))
}

fn binary_op(a: Value, b: Value, op: Op) -> VmResult<Value> {
    match (a.kind(), b.kind()) {
        (ValueView::Int(ai), ValueView::Int(bi)) => {
            // Integer arithmetic is TOTAL: every case yields a value and the VM
            // never exits early. `x / 0 = 0`, `x % 0 = x` (preserves the
            // identity `a = (a/b)*b + a%b`), and overflow wraps with defined
            // two's-complement semantics. This is the Lean/Coq convention for
            // keeping division a total function. (`wrapping_div`/`wrapping_rem`
            // still panic on a zero divisor, so the `bi == 0` guard is load-
            // bearing, not just the sentinel.)
            let v = match op {
                Op::Add => ai.wrapping_add(bi),
                Op::Sub => ai.wrapping_sub(bi),
                Op::Mul => ai.wrapping_mul(bi),
                Op::Div => {
                    if bi == 0 {
                        0
                    } else {
                        ai.wrapping_div(bi)
                    }
                }
                Op::Mod => {
                    if bi == 0 {
                        ai
                    } else {
                        ai.wrapping_rem(bi)
                    }
                }
                _ => return Err("Unknown binary op. This is likely a compiler bug.".to_string()),
            };
            Ok(Value::int(v))
        }
        (ValueView::Float(af), ValueView::Float(bf)) => float_op(af, bf, op),
        (ValueView::Int(ai), ValueView::Float(bf)) => float_op(ai as f64, bf, op),
        (ValueView::Float(af), ValueView::Int(bi)) => float_op(af, bi as f64, op),
        (ValueView::Heap(HeapValue::Str(sa)), ValueView::Heap(HeapValue::Str(sb)))
            if op == Op::Add =>
        {
            Ok(concat_str(sa, sb))
        }
        _ => Err(format!(
            "Cannot perform arithmetic on '{}' and '{}'. This is likely a compiler bug.",
            value_type_name(&a),
            value_type_name(&b)
        )),
    }
}

#[inline]
fn range_elem(start: i64, end: i64, idx: i64) -> Option<i64> {
    start.checked_add(idx).filter(|e| *e < end)
}

#[inline]
fn range_len(s: i64, e: i64) -> i64 {
    e.saturating_sub(s).max(0)
}

#[inline]
fn concat_str(a: &str, b: &str) -> Value {
    let mut s = String::with_capacity(a.len() + b.len());
    s.push_str(a);
    s.push_str(b);
    Value::str(s)
}

#[inline]
fn slices_equal(a: &[Value], b: &[Value]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| values_equal(x, y))
}

fn values_equal(a: &Value, b: &Value) -> bool {
    use HeapValue as H;
    match (a.kind(), b.kind()) {
        (ValueView::Int(x), ValueView::Int(y)) => x == y,
        (ValueView::Float(x), ValueView::Float(y)) => x == y,
        (ValueView::Bool(x), ValueView::Bool(y)) => x == y,
        (ValueView::Heap(H::Str(x)), ValueView::Heap(H::Str(y))) => x == y,
        (ValueView::Heap(H::Enum(ae)), ValueView::Heap(H::Enum(be))) => {
            ae.hash == be.hash
                && ae.type_id == be.type_id
                && ae.variant_name == be.variant_name
                && slices_equal(&ae.payload, &be.payload)
        }
        (ValueView::Heap(H::Array(aa)), ValueView::Heap(H::Array(ba))) => {
            aa.len() == ba.len() && aa.iter().zip(ba.iter()).all(|(x, y)| values_equal(x, y))
        }
        (ValueView::Heap(H::Range(as_, ae)), ValueView::Heap(H::Range(bs, be))) => {
            // Normalised: empty ranges compare equal regardless of endpoints.
            let alen = range_len(*as_, *ae);
            let blen = range_len(*bs, *be);
            (alen == 0 && blen == 0) || (as_ == bs && ae == be)
        }
        (ValueView::Heap(H::Range(s, e)), ValueView::Heap(H::Array(arr)))
        | (ValueView::Heap(H::Array(arr)), ValueView::Heap(H::Range(s, e))) => {
            let len = range_len(*s, *e) as usize;
            if arr.len() != len {
                return false;
            }
            for (i, av) in arr.iter().enumerate() {
                if !values_equal(av, &Value::int(s + i as i64)) {
                    return false;
                }
            }
            true
        }
        (ValueView::Heap(H::Binary(a)), ValueView::Heap(H::Binary(b))) => a == b,
        (ValueView::Heap(H::Tuple(at)), ValueView::Heap(H::Tuple(bt))) => {
            slices_equal(&at.elements, &bt.elements)
        }
        (ValueView::Heap(H::Socket(asv)), ValueView::Heap(H::Socket(bsv))) => {
            asv.id == bsv.id && asv.is_listener == bsv.is_listener
        }
        _ => false,
    }
}

fn compare(a: Value, b: Value, op: Op) -> VmResult<bool> {
    match (a.kind(), b.kind()) {
        (ValueView::Int(ai), ValueView::Int(bi)) => Ok(match op {
            Op::Lt => ai < bi,
            Op::Gt => ai > bi,
            Op::Lte => ai <= bi,
            Op::Gte => ai >= bi,
            _ => return Err("Unknown compare op. This is likely a compiler bug.".to_string()),
        }),
        (ValueView::Float(af), ValueView::Float(bf)) => Ok(match op {
            Op::Lt => af < bf,
            Op::Gt => af > bf,
            Op::Lte => af <= bf,
            Op::Gte => af >= bf,
            _ => return Err("Unknown compare op. This is likely a compiler bug.".to_string()),
        }),
        _ => Err(format!(
            "Cannot compare '{}' with '{}'. This is likely a compiler bug.",
            value_type_name(&a),
            value_type_name(&b)
        )),
    }
}

fn is_truthy(v: &Value) -> bool {
    v.as_bool().unwrap_or(false)
}

fn value_type_name(v: &Value) -> String {
    match v.kind() {
        ValueView::Int(_) => "Int".to_string(),
        ValueView::Float(_) => "Float".to_string(),
        ValueView::Bool(_) => "Bool".to_string(),
        ValueView::Heap(HeapValue::Str(_)) => "String".to_string(),
        ValueView::Heap(HeapValue::Array(_) | HeapValue::Range(..)) => "Array".to_string(),
        ValueView::Heap(HeapValue::Binary(..)) => "Binary".to_string(),
        ValueView::Heap(HeapValue::Tuple(_)) => "Tuple".to_string(),
        ValueView::Heap(HeapValue::Closure(_)) => "Function".to_string(),
        ValueView::Heap(HeapValue::Enum(e)) => e.enum_name.to_string(),
        ValueView::Heap(HeapValue::Socket(_)) => "Socket".to_string(),
        ValueView::Heap(HeapValue::BigInt(_)) => "Int".to_string(),
        ValueView::Nil => "Nil".to_string(),
    }
}

pub fn inspect(v: &Value) -> String {
    inspect_impl(v, Some(0))
}

fn is_simple_value(v: &Value) -> bool {
    match v.kind() {
        ValueView::Int(_) | ValueView::Float(_) | ValueView::Bool(_) => true,
        ValueView::Heap(HeapValue::Closure(_)) => true,
        ValueView::Heap(HeapValue::Str(s)) => s.len() < 20,
        ValueView::Heap(HeapValue::Binary(b)) => b.bytes.len() <= 8,
        ValueView::Heap(HeapValue::Enum(e)) => e.payload.is_empty(),
        _ => false,
    }
}

/// A constructor named after its own type that carries field labels is the
/// record-shorthand form (`type T { a Int  b Bool }` → `T{ a: .., b: .. }`).
/// Sum-type variants and prelude constructors (`Some`, `Ok`, `Err`, …) have a
/// distinct variant name and keep the positional `Variant(..)` rendering.
fn is_record(e: &EnumValue) -> bool {
    !e.field_labels.is_empty()
        && e.field_labels.len() == e.payload.len()
        && e.enum_name == e.variant_name
}

fn f64_str(f: f64) -> String {
    // Match V's f64.str() — always include a decimal point for finite values.
    let s = format!("{}", f);
    if f.is_finite() && !s.contains('.') && !s.contains('e') && !s.contains('E') {
        format!("{}.0", s)
    } else {
        s
    }
}

fn join_inline(xs: &[Value]) -> String {
    xs.iter()
        .map(|v| inspect_impl(v, None))
        .collect::<Vec<_>>()
        .join(", ")
}

fn block(open: &str, close: char, lines: Vec<String>, n: usize) -> String {
    let inner = "  ".repeat(n + 1);
    let body = lines
        .into_iter()
        .map(|l| format!("{}{}", inner, l))
        .collect::<Vec<_>>()
        .join(",\n");
    format!("{}\n{}\n{}{}", open, body, "  ".repeat(n), close)
}

/// Render a value. `indent = None` forces a single-line rendering; `Some(n)`
/// pretty-prints, expanding containers across lines at indent depth `n` when
/// their children aren't all simple leaves.
fn inspect_impl(v: &Value, indent: Option<usize>) -> String {
    let pretty = |xs: &[Value], n: usize| -> Vec<String> {
        xs.iter().map(|v| inspect_impl(v, Some(n + 1))).collect()
    };
    match v.kind() {
        ValueView::Int(i) => i.to_string(),
        ValueView::Float(f) => f64_str(f),
        ValueView::Bool(b) => if b { "True" } else { "False" }.to_string(),
        ValueView::Heap(HeapValue::Str(s)) => s.to_string(),
        ValueView::Heap(HeapValue::Binary(b)) => binary::inspect(&b.bytes, b.bit_len),
        ValueView::Heap(HeapValue::Closure(c)) => format!("<fn#{}>", c.name),
        ValueView::Heap(HeapValue::Socket(s)) => {
            let kind = if s.is_listener { "listener" } else { "socket" };
            format!("<{}#{}>", kind, s.id)
        }
        ValueView::Heap(HeapValue::Range(a, z)) => {
            inspect_impl(&Value::array((*a..*z).map(Value::int).collect()), indent)
        }
        ValueView::Heap(HeapValue::BigInt(i)) => i.to_string(),
        ValueView::Nil => "Nil".to_string(),
        ValueView::Heap(HeapValue::Enum(e)) if e.payload.is_empty() => e.variant_name.to_string(),
        ValueView::Heap(HeapValue::Enum(e)) if is_record(e) => {
            let fields = |mode: Option<usize>| -> Vec<String> {
                e.field_labels
                    .iter()
                    .zip(&e.payload)
                    .map(|(l, v)| format!("{}: {}", l, inspect_impl(v, mode)))
                    .collect()
            };
            match indent {
                Some(n) if !e.payload.iter().all(is_simple_value) => block(
                    &format!("{} {{", e.variant_name),
                    '}',
                    fields(Some(n + 1)),
                    n,
                ),
                _ => format!("{}{{ {} }}", e.variant_name, fields(None).join(", ")),
            }
        }
        ValueView::Heap(HeapValue::Enum(e)) => match indent {
            Some(n) if !e.payload.iter().all(is_simple_value) => block(
                &format!("{}(", e.variant_name),
                ')',
                pretty(&e.payload, n),
                n,
            ),
            _ => format!("{}({})", e.variant_name, join_inline(&e.payload)),
        },
        ValueView::Heap(HeapValue::Tuple(t)) => match indent {
            None => format!("({})", join_inline(&t.elements)),
            Some(_) if t.elements.is_empty() => "()".into(),
            Some(n) => {
                if t.elements.iter().all(is_simple_value) {
                    let flat = format!("({})", join_inline(&t.elements));
                    if flat.len() <= 80 {
                        return flat;
                    }
                }
                block("(", ')', pretty(&t.elements, n), n)
            }
        },
        ValueView::Heap(HeapValue::Array(arr)) => {
            // imbl Vector lacks slice APIs (chunks/&[Value]); materialise once
            // so the formatting body below stays byte-identical to the old
            // `Rc<Vec<Value>>` behaviour (this is a cold path).
            let arr: Vec<Value> = arr.iter().cloned().collect();
            let arr: &[Value] = &arr;
            match indent {
                None => format!("[{}]", join_inline(arr)),
                Some(_) if arr.is_empty() => "[]".into(),
                Some(n) if arr.iter().all(is_simple_value) => {
                    let flat = format!("[{}]", join_inline(arr));
                    if flat.len() <= 80 {
                        return flat;
                    }
                    // Long array of leaves: wrap 6-per-line rather than one-per-line.
                    let inner = "  ".repeat(n + 1);
                    let body = arr
                        .chunks(6)
                        .map(join_inline)
                        .collect::<Vec<_>>()
                        .join(&format!(", \n{}", inner));
                    format!("[\n{}{}\n{}]", inner, body, "  ".repeat(n))
                }
                Some(n) => block("[", ']', pretty(arr, n), n),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use al_core::bytecode::{Instruction, op, op_ab, op_arg};

    #[test]
    fn test_push_add_halt() {
        let program = Program {
            constants: vec![Value::int(1), Value::int(2)],
            functions: vec![Function {
                name: "main".into(),
                arity: 0,
                locals: 0,
                capture_count: 0,
                code_start: 0,
                code_len: 4,
            }],
            code: vec![
                op_arg(Op::PushConst, 0),
                op_arg(Op::PushConst, 1),
                op(Op::Add),
                op(Op::Halt),
            ],
            entry: 0,
        };

        let mut vm = new_vm(program);
        let result = vm.run().expect("vm should run cleanly");
        assert_eq!(
            result.as_int(),
            Some(3),
            "expected Int(3), got {:?}",
            result
        );
    }

    #[test]
    fn test_inspect_basic() {
        assert_eq!(inspect(&Value::int(42)), "42");
        assert_eq!(inspect(&Value::bool(true)), "True");
        assert_eq!(inspect(&nil_value(1)), "Nil");
        assert_eq!(
            inspect(&Value::array(vec![Value::int(1), Value::int(2)])),
            "[1, 2]"
        );
        assert_eq!(inspect(&Value::range(1, 4)), "[1, 2, 3]");
    }

    #[test]
    fn test_values_equal() {
        assert!(values_equal(&Value::int(5), &Value::int(5)));
        assert!(!values_equal(&Value::int(5), &Value::int(6)));
        assert!(values_equal(&nil_value(1), &nil_value(1)));
        assert!(!values_equal(&Value::int(5), &Value::str("5")));
        assert!(values_equal(
            &Value::range(0, 3),
            &Value::array(vec![Value::int(0), Value::int(1), Value::int(2)])
        ));
    }

    // Regression: the Range arms of the safe-indexing opcodes computed
    // `start + idx` (and lengths as `end - start`) with unchecked i64
    // arithmetic. A non-zero-start range indexed near i64::MAX overflowed:
    // debug panicked ("attempt to add with overflow"), release wrapped and
    // returned a wrong, in-bounds-looking value instead of None. Indexing is a
    // total Option op, so an overflowing index must read as out-of-bounds, and
    // range length must saturate rather than wrap.
    #[test]
    fn range_index_and_len_do_not_overflow() {
        fn run_ops(constants: Vec<Value>, code: Vec<Instruction>) -> Value {
            let code_len = code.len() as i32;
            let program = Program {
                constants,
                functions: vec![Function {
                    name: "main".into(),
                    arity: 0,
                    locals: 0,
                    capture_count: 0,
                    code_start: 0,
                    code_len,
                }],
                code,
                entry: 0,
            };
            new_vm(program)
                .run()
                .expect("vm must run without panicking or erroring")
        }

        // `(5..10)[idx]` via Op::Index.
        let index_at = |idx: i64| -> Value {
            run_ops(
                vec![Value::int(5), Value::int(10), Value::int(idx)],
                vec![
                    op_arg(Op::PushConst, 0),
                    op_arg(Op::PushConst, 1),
                    op(Op::MakeRange),
                    op_arg(Op::PushConst, 2),
                    op(Op::Index),
                    op(Op::Halt),
                ],
            )
        };
        // In-bounds indexing is unchanged.
        let some = index_at(2);
        let some = some.as_enum().expect("index returns an Option enum");
        assert_eq!(some.payload[0].as_int(), Some(7));
        // 5 + i64::MAX overflows i64; the result must be None, not a wrapped
        // value that spuriously passes the `< end` bound.
        let none = index_at(i64::MAX);
        assert!(none.as_enum().expect("Option enum").payload.is_empty());

        // `len(i64::MIN..i64::MAX)` via Op::ArrayLen: `end - start` overflows,
        // so the length must saturate to i64::MAX instead of panicking/wrapping.
        let len = run_ops(
            vec![Value::int(i64::MIN), Value::int(i64::MAX)],
            vec![
                op_arg(Op::PushConst, 0),
                op_arg(Op::PushConst, 1),
                op(Op::MakeRange),
                op(Op::ArrayLen),
                op(Op::Halt),
            ],
        );
        assert_eq!(len.as_int(), Some(i64::MAX));

        // values_equal's Range length arithmetic must not overflow on extreme
        // bounds either.
        assert!(values_equal(
            &Value::range(i64::MIN, i64::MAX),
            &Value::range(i64::MIN, i64::MAX),
        ));
    }

    // Regression: wrapping a range in a constructor (`Some(0..n)`) eagerly folds
    // the payload hash via `enum_hash_with_payload`. Hashing the range must be
    // O(1), and must still match the hash of the array it materialises to, so
    // the `ae.hash == be.hash` fast-reject in `values_equal` never drops an
    // equal pair. Pre-fix this hashed every element and hung on a large range.
    #[test]
    fn enum_with_range_payload_equals_materialized_and_builds_fast() {
        let build_some = |payload: Value| -> Value {
            let hash = enum_hash_with_payload(
                enum_name_prefix_hash("Option", "Some"),
                std::slice::from_ref(&payload),
            );
            Value::enum_(EnumValue {
                type_id: 7,
                enum_name: "Option".into(),
                variant_name: "Some".into(),
                field_labels: Rc::from(vec![Rc::from("value")]),
                payload: vec![payload],
                hash,
            })
        };

        let from_range = build_some(Value::range(0, 4));
        let from_array = build_some(Value::array((0..4).map(Value::int).collect()));
        assert!(
            values_equal(&from_range, &from_array),
            "Some(0..4) must equal Some([0, 1, 2, 3])"
        );

        // Building this hung before the fix: the range payload was hashed
        // element by element across ~9.2e18 values. Reaching the asserts proves
        // construction is bounded.
        let huge = build_some(Value::range(0, i64::MAX));
        assert!(values_equal(&huge, &huge));
        assert!(!values_equal(&huge, &from_range));
    }

    // Regression (signed-zero congruence): `+0.0` and `-0.0` are `values_equal`
    // (`0.0 == -0.0` in IEEE 754), so congruence (`a == b => f(a) == f(b)`)
    // demands `Some(0.0) == Some(-0.0)`. The enum-equality arm gates on the
    // cached payload hash (`ae.hash == be.hash` as a necessary precondition);
    // hashing a signed zero by its raw bits fast-rejected the equal pair. `-0.0`
    // is reachable from ordinary code via unary `-` (`Op::NegFloat`).
    #[test]
    fn enum_with_signed_zero_payload_is_congruent() {
        // Premise: the scalar payloads compare equal.
        assert!(values_equal(&Value::float(0.0), &Value::float(-0.0)));

        let build_some = |payload: Value| -> Value {
            let hash = enum_hash_with_payload(
                enum_name_prefix_hash("Option", "Some"),
                std::slice::from_ref(&payload),
            );
            Value::enum_(EnumValue {
                type_id: 7,
                enum_name: "Option".into(),
                variant_name: "Some".into(),
                field_labels: Rc::from(vec![Rc::from("value")]),
                payload: vec![payload],
                hash,
            })
        };

        let pos = build_some(Value::float(0.0));
        let neg = build_some(Value::float(-0.0));
        assert!(
            values_equal(&pos, &neg),
            "Some(0.0) must equal Some(-0.0): a == b => f(a) == f(b)"
        );
        assert!(values_equal(&neg, &pos), "equality must be symmetric");
        // A genuinely distinct payload must still compare unequal.
        assert!(!values_equal(&pos, &build_some(Value::float(1.0))));
    }

    // Build a single-function `Program` with `locals` preallocated local slots,
    // run it to completion, and return the value left on top of the stack.
    // Mirrors the entry-frame setup in `VM::run` (base_slot 0, locals zeroed), so
    // `local[i]` lives at stack slot `i`.
    fn run_fn(constants: Vec<Value>, code: Vec<Instruction>, locals: i32) -> Value {
        let code_len = code.len() as i32;
        let program = Program {
            constants,
            functions: vec![Function {
                name: "main".into(),
                arity: 0,
                locals,
                capture_count: 0,
                code_start: 0,
                code_len,
            }],
            code,
            entry: 0,
        };
        new_vm(program)
            .run()
            .expect("vm must run without panicking or erroring")
    }

    // The peephole pass fuses `PushLocal a; PushConst b; AddInt` into a single
    // `AddIntLC` whose result is `local[a] + constants[b]`. Loop goldens exercise
    // it only when fusion fires; nothing pins the fused op's actual result or its
    // overflow edge. AddIntLC uses `wrapping_add`, so overflow is total — it wraps
    // rather than panicking.
    #[test]
    fn superinstr_add_int_lc_adds_local_and_const_with_wrapping() {
        // local[0] = constants[0]; AddIntLC pushes local[0] + constants[1].
        let add = |local: i64, c: i64| -> Value {
            run_fn(
                vec![Value::int(local), Value::int(c)],
                vec![
                    op_arg(Op::PushConst, 0),
                    op_arg(Op::StoreLocal, 0),
                    op_ab(Op::AddIntLC, 0, 1, 0),
                    op(Op::Halt),
                ],
                1,
            )
        };
        assert_eq!(add(40, 2).as_int(), Some(42));
        assert_eq!(add(-5, 8).as_int(), Some(3));
        assert_eq!(add(7, -7).as_int(), Some(0));
        // wrapping_add edge: i64::MAX + 1 wraps to i64::MIN, never panics.
        assert_eq!(add(i64::MAX, 1).as_int(), Some(i64::MIN));
    }

    // `PushLocal a; PushConst b; SubInt` fuses to `SubIntLC` = `local[a] -
    // constants[b]`. SubIntLC uses `wrapping_sub`; the underflow edge must wrap.
    #[test]
    fn superinstr_sub_int_lc_subtracts_const_from_local_with_wrapping() {
        // local[0] = constants[0]; SubIntLC pushes local[0] - constants[1].
        let sub = |local: i64, c: i64| -> Value {
            run_fn(
                vec![Value::int(local), Value::int(c)],
                vec![
                    op_arg(Op::PushConst, 0),
                    op_arg(Op::StoreLocal, 0),
                    op_ab(Op::SubIntLC, 0, 1, 0),
                    op(Op::Halt),
                ],
                1,
            )
        };
        assert_eq!(sub(50, 8).as_int(), Some(42));
        assert_eq!(sub(3, 10).as_int(), Some(-7));
        // wrapping_sub edge: i64::MIN - 1 wraps to i64::MAX, never panics.
        assert_eq!(sub(i64::MIN, 1).as_int(), Some(i64::MAX));
    }

    // `PushLocal a; PushConst b; LtInt; JumpIfFalse t` fuses to `JumpGeIntLC`: a
    // zero-stack-traffic branch taken iff `local[a] >= constants[b]`, jumping to
    // the absolute target in `operand` (the VM computes `operand - code_start`).
    // Pins both the `>=` semantics (incl. the equal boundary) and the retarget.
    #[test]
    fn superinstr_jump_ge_int_lc_branches_and_retargets() {
        // Branch on `local[0] >= constants[1]`. The taken target (ip 5) and the
        // fall-through (ip 3) push distinct markers, so the return value reveals
        // which path executed.
        let taken = |local: i64, c: i64| -> bool {
            let r = run_fn(
                vec![
                    Value::int(local), // 0: local value
                    Value::int(c),     // 1: compared constant
                    Value::int(0),     // 2: fall-through marker (not taken)
                    Value::int(1),     // 3: jump-target marker (taken)
                ],
                vec![
                    op_arg(Op::PushConst, 0),
                    op_arg(Op::StoreLocal, 0),
                    op_ab(Op::JumpGeIntLC, 0, 1, 5),
                    op_arg(Op::PushConst, 2),
                    op(Op::Halt),
                    op_arg(Op::PushConst, 3),
                    op(Op::Halt),
                ],
                1,
            );
            r.as_int() == Some(1)
        };
        assert!(taken(10, 5), "10 >= 5 takes the branch");
        assert!(taken(5, 5), "5 >= 5: the equal boundary is taken");
        assert!(!taken(4, 5), "4 >= 5 is false: falls through");
        assert!(taken(i64::MAX, i64::MIN), "MAX >= MIN");
        assert!(!taken(i64::MIN, i64::MAX), "MIN >= MAX is false");
    }

    // `PushLocal a; PushConst b; EqInt; JumpIfFalse t` fuses to `JumpNeIntLC`:
    // branch taken iff `local[a] != constants[b]`.
    #[test]
    fn superinstr_jump_ne_int_lc_branches_and_retargets() {
        let taken = |local: i64, c: i64| -> bool {
            let r = run_fn(
                vec![
                    Value::int(local), // 0: local value
                    Value::int(c),     // 1: compared constant
                    Value::int(0),     // 2: fall-through marker (not taken)
                    Value::int(1),     // 3: jump-target marker (taken)
                ],
                vec![
                    op_arg(Op::PushConst, 0),
                    op_arg(Op::StoreLocal, 0),
                    op_ab(Op::JumpNeIntLC, 0, 1, 5),
                    op_arg(Op::PushConst, 2),
                    op(Op::Halt),
                    op_arg(Op::PushConst, 3),
                    op(Op::Halt),
                ],
                1,
            );
            r.as_int() == Some(1)
        };
        assert!(taken(10, 5), "10 != 5 takes the branch");
        assert!(
            !taken(5, 5),
            "5 != 5 is false: the equal case falls through"
        );
        assert!(taken(-1, 1), "-1 != 1 takes the branch");
    }

    // Build an enum Value with the same payload-folded hash the VM computes, so
    // it is usable with `values_equal` as well as `inspect`.
    fn ev(type_id: i32, en: &str, vn: &str, labels: &[&str], payload: Vec<Value>) -> Value {
        let hash = enum_hash_with_payload(enum_name_prefix_hash(en, vn), &payload);
        let field_labels: Rc<[Rc<str>]> = Rc::from(
            labels
                .iter()
                .map(|s| Rc::from(*s))
                .collect::<Vec<Rc<str>>>(),
        );
        Value::enum_(EnumValue {
            type_id,
            enum_name: en.into(),
            variant_name: vn.into(),
            field_labels,
            payload,
            hash,
        })
    }

    // A function value renders as `<fn#name>` regardless of arity/captures.
    #[test]
    fn inspect_closure_renders_name() {
        let c = Value::closure(ClosureValue {
            func_idx: 3,
            captures: Vec::new().into(),
            name: "myfn".into(),
        });
        assert_eq!(inspect(&c), "<fn#myfn>");
    }

    // A socket value renders with its kind and id.
    #[test]
    fn inspect_socket_renders_kind_and_id() {
        let conn = Value::socket(SocketValue {
            id: 7,
            is_listener: false,
        });
        let listener = Value::socket(SocketValue {
            id: 2,
            is_listener: true,
        });
        assert_eq!(inspect(&conn), "<socket#7>");
        assert_eq!(inspect(&listener), "<listener#2>");
    }

    // An array of all-simple leaves whose single-line form would exceed 80
    // columns wraps six values per line (not one-per-line, and not flat). Pins
    // the exact 6-per-line layout in `inspect_impl`.
    #[test]
    fn inspect_long_simple_array_wraps_six_per_line() {
        let arr = Value::array((1..=30).map(Value::int).collect());
        let s = inspect(&arr);
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines[0], "[");
        assert_eq!(lines[1], "  1, 2, 3, 4, 5, 6, ");
        assert_eq!(lines[2], "  7, 8, 9, 10, 11, 12, ");
        assert_eq!(lines[5], "  25, 26, 27, 28, 29, 30");
        assert_eq!(*lines.last().unwrap(), "]");
        // "[" + ceil(30/6)=5 content rows + "]".
        assert_eq!(lines.len(), 7);
    }

    // A short all-simple array stays on one line (the <= 80 fast path).
    #[test]
    fn inspect_short_array_stays_flat() {
        let arr = Value::array(vec![Value::int(1), Value::int(2), Value::int(3)]);
        assert_eq!(inspect(&arr), "[1, 2, 3]");
    }

    // An array whose children are themselves containers (not "simple") expands
    // one element per line via `block`.
    #[test]
    fn inspect_nested_array_expands_per_line() {
        let arr = Value::array(vec![
            Value::array(vec![Value::int(1), Value::int(2)]),
            Value::array(vec![Value::int(3), Value::int(4)]),
        ]);
        assert_eq!(inspect(&arr), "[\n  [1, 2],\n  [3, 4]\n]");
    }

    // A record (constructor named after its type, with field labels) prints
    // `T{ a: .., b: .. }` inline when its fields are simple, and as a multiline
    // `T {` block when a field is itself a record.
    #[test]
    fn inspect_record_inline_and_multiline() {
        let point = |x: i64, y: i64| {
            ev(
                1,
                "Point",
                "Point",
                &["x", "y"],
                vec![Value::int(x), Value::int(y)],
            )
        };
        assert_eq!(inspect(&point(1, 2)), "Point{ x: 1, y: 2 }");

        let line = ev(
            2,
            "Line",
            "Line",
            &["a", "b"],
            vec![point(1, 2), point(3, 4)],
        );
        assert_eq!(
            inspect(&line),
            "Line {\n  a: Point{ x: 1, y: 2 },\n  b: Point{ x: 3, y: 4 }\n}"
        );
    }

    // A positional sum-type variant prints `Variant(..)`, expanding to a
    // multiline block when a payload element is itself a non-simple value.
    #[test]
    fn inspect_variant_inline_and_multiline() {
        let leaf = |v: i64| ev(3, "Tree", "Leaf", &[], vec![Value::int(v)]);
        assert_eq!(inspect(&leaf(1)), "Leaf(1)");

        let node = ev(3, "Tree", "Node", &[], vec![leaf(1), leaf(2)]);
        assert_eq!(inspect(&node), "Node(\n  Leaf(1),\n  Leaf(2)\n)");
    }

    // Tuples: empty renders `()`; a tuple containing a non-simple element
    // expands one element per line.
    #[test]
    fn inspect_tuple_empty_and_multiline() {
        assert_eq!(inspect(&Value::tuple(vec![])), "()");
        let t = Value::tuple(vec![
            Value::int(1),
            Value::array(vec![Value::int(2), Value::int(3), Value::int(4)]),
        ]);
        assert_eq!(inspect(&t), "(\n  1,\n  [2, 3, 4]\n)");
    }

    // `values_equal` treats a Range and an Array as equal iff they enumerate the
    // same elements: a length difference and an element difference must both
    // compare unequal, in either operand order.
    #[test]
    fn values_equal_range_vs_array() {
        let ints = |xs: &[i64]| Value::array(xs.iter().copied().map(Value::int).collect());
        // 0..3 == [0, 1, 2]
        assert!(values_equal(&Value::range(0, 3), &ints(&[0, 1, 2])));
        assert!(values_equal(&ints(&[0, 1, 2]), &Value::range(0, 3)));
        // length mismatch.
        assert!(!values_equal(&Value::range(0, 3), &ints(&[0, 1, 2, 3])));
        assert!(!values_equal(&ints(&[0, 1, 2, 3]), &Value::range(0, 3)));
        // same length, differing element.
        assert!(!values_equal(&Value::range(0, 3), &ints(&[0, 9, 2])));
        assert!(!values_equal(&ints(&[0, 9, 2]), &Value::range(0, 3)));
    }

    // `values_equal` on Binary compares the bytes; equal-length differing bytes
    // are unequal.
    #[test]
    fn values_equal_binary() {
        assert!(values_equal(
            &Value::binary(vec![1, 2, 3]),
            &Value::binary(vec![1, 2, 3])
        ));
        assert!(!values_equal(
            &Value::binary(vec![1, 2, 3]),
            &Value::binary(vec![1, 2, 4])
        ));
        assert!(!values_equal(
            &Value::binary(vec![1, 2]),
            &Value::binary(vec![1, 2, 3])
        ));
    }

    // `values_equal` on Socket compares (id, is_listener).
    #[test]
    fn values_equal_socket() {
        let sock = |id, is_listener| Value::socket(SocketValue { id, is_listener });
        assert!(values_equal(&sock(5, false), &sock(5, false)));
        assert!(!values_equal(&sock(5, false), &sock(6, false)));
        assert!(!values_equal(&sock(5, false), &sock(5, true)));
    }

    // Slicing a Range past its length is a runtime error (the indices are not
    // bounds-checked at compile time), not a silent wrap or empty result. The
    // VM aborts with a message naming the range length.
    #[test]
    fn array_slice_range_out_of_bounds_errors() {
        // (0..5)[2..99]
        let code = vec![
            op_arg(Op::PushConst, 0),
            op_arg(Op::PushConst, 1),
            op(Op::MakeRange),
            op_arg(Op::PushConst, 2),
            op_arg(Op::PushConst, 3),
            op(Op::ArraySlice),
            op(Op::Halt),
        ];
        let program = Program {
            constants: vec![Value::int(0), Value::int(5), Value::int(2), Value::int(99)],
            functions: vec![Function {
                name: "main".into(),
                arity: 0,
                locals: 0,
                capture_count: 0,
                code_start: 0,
                code_len: code.len() as i32,
            }],
            code,
            entry: 0,
        };
        let err = new_vm(program)
            .run()
            .expect_err("slicing a range out of bounds must error");
        assert!(
            err.contains("Slice indices out of bounds") && err.contains("range length is 5"),
            "unexpected error: {err}"
        );
    }

    #[allow(dead_code)]
    fn _instruction_is_copy(_: Instruction) {}
}
