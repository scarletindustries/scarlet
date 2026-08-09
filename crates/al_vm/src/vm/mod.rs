//! The virtual machine: lightweight processes and the schedulers that run
//! them. One [`VM`] per scheduler thread, running thousands of cooperatively
//! preempted processes over a handful of OS threads without letting one block
//! another.
//!
//! ```text
//!   spawn(f) — the closure graph is deep-copied into a fresh heap
//!      │
//!      ▼
//!   SEED ──────── submit ───────> injector / a scheduler's inbox
//!      │  take_inbound / steal_inbound: the receiver adopts the process whole
//!      ▼
//!   RUNNABLE   run_queue, round-robin ◄────────────────────────────┐
//!      │  resume: the process's heap/stack/frames swap into the VM │
//!      ▼                                                           │
//!   RUNNING    execute_slice, one reduction budget                 │
//!      │                                                           │
//!      ├─ budget spent ──── Yield ── back of the run queue ────────┤
//!      │                                                           │
//!      ├─ would-block op ── Parked(Wait) ── poller + timer heap    │
//!      │                       │ fd ready / deadline / completion  │
//!      │                       └──────────── wake ─────────────────┤
//!      │                                                           │
//!      │     (a busy scheduler that sees an idle peer claims it    │
//!      │      and donates a queued process — MIGRATE: the whole    │
//!      │      Process moves, heap and all, fds re-homed, into the  │
//!      │      peer's inbox and onto its run queue) ────────────────┘
//!      ▼
//!   DONE       the result is its top-of-stack; the process's values are
//!              freed as it drops (main's result is stashed until run() returns)
//! ```
//!
//! The design in five facts:
//!
//! - A context switch is a few pointer moves. The running process lives
//!   directly in the VM's `heap`/`stack`/`frames` fields, and a suspended one
//!   is those same fields packed into a [`Process`].
//! - Preemption is budgeted. A slice runs until [`REDUCTION_BUDGET`] is spent:
//!   a call costs one reduction, a syscall [`IO_REDUCTION_COST`], so an accept
//!   loop is preempted like everything else.
//! - Blocking parks the process, never the thread. An op that would block
//!   returns [`Step::Parked`] with a [`Wait`]; the wake re-runs the
//!   instruction.
//! - Work moves between schedulers as owned memory. Spawns copy the closure
//!   graph into a fresh heap; load balancing donates whole queued processes.
//!   Plain moves in both cases.
//! - Memory is reference-counted, so there is no rooting rule and no
//!   allocation reservation. A process owns every heap value it can reach,
//!   which is what makes seeds, migrants, and suspended processes `Send` by
//!   construction — and why main's result is stashed as a `(value, heap)`
//!   pair until `run()`'s caller is done with it.

use std::borrow::Cow;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::fmt;
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use crate::bytecode::value::range_len;
use crate::bytecode::{BinaryRef, Program, Value};
#[cfg(test)]
use crate::bytecode::{Function, Op, op};
use crate::frozen::FrozenBuilder;
use crate::heap::ProcHeap;
use smallvec::SmallVec;

mod binary;
mod collections;
mod exec;
mod freeze;
mod http;
mod inspect;
mod io;
/// JIT module construction and the runtime-symbol resolution seam.
pub mod jit;
mod map;
mod migrate;
/// Public because the JIT finalize step registers
/// [`native::al_rt_enter_interp`] by symbol.
pub mod native;
/// Public because the JIT finalize step registers these symbols with the
/// builder and generated code calls them.
pub mod native_shims;
/// Dispatch counters. Absent from a default build, which leaves stderr
/// untouched.
#[cfg(feature = "op-histogram")]
pub mod op_histogram;
/// The `AL_PERF_MAP=1` perf-map writer: one `/tmp/perf-<pid>.map` symbol line
/// per JIT-compiled body.
pub mod perf_map;
mod poll;
mod sched;
mod templates;
#[cfg(test)]
mod tests;
mod text;

pub use inspect::inspect;
use inspect::{f64_str, value_type_name};
use migrate::Migrant;
use native::NativePending;
use poll::Wait;
use sched::{Inbound, Runtime, Seed};
use templates::Templates;
use text::{int_to_ascii, parse_uint_ascii};

/// A VM-level failure. The variants separate user-visible runtime errors from
/// broken type-system invariants and from infrastructure failures, because
/// [`VM::run`] and [`worker_main`] act differently on each.
#[derive(Debug)]
pub enum VmError {
    /// An operand had the wrong runtime tag for `op`.
    TypeMismatch {
        op: &'static str,
        expected: &'static str,
        got: String,
    },
    /// `[lo..hi]` on a sequence of length `len`.
    SliceOutOfBounds { lo: i64, hi: i64, len: i64 },
    /// `what[idx]` on something of length `len`.
    IndexOutOfBounds {
        idx: i64,
        len: i64,
        what: &'static str,
    },
    /// Type-system invariant broken: a compiler bug, not user error. The
    /// runtime behind an `Internal`-errored run is leaked (see [`VM::run`]).
    Internal(Cow<'static, str>),
    /// mio poll / fd registration / OS resource failure.
    Io(std::io::Error),
}

impl VmError {
    #[cold]
    pub(super) fn internal(msg: impl Into<Cow<'static, str>>) -> Self {
        Self::Internal(msg.into())
    }
    #[cold]
    pub(super) fn type_mismatch(op: &'static str, expected: &'static str, got: &Value) -> Self {
        Self::TypeMismatch {
            op,
            expected,
            got: value_type_name(got),
        }
    }
}

impl fmt::Display for VmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TypeMismatch { op, expected, got } => {
                write!(f, "{op}: expected {expected}, got '{got}'")
            }
            Self::SliceOutOfBounds { lo, hi, len } => {
                write!(
                    f,
                    "Slice indices out of bounds: [{lo}..{hi}] (length {len})"
                )
            }
            Self::IndexOutOfBounds { idx, len, what } => {
                write!(f, "{what} index {idx} out of bounds (length {len})")
            }
            Self::Internal(s) => {
                write!(f, "internal VM error (likely a compiler bug): {s}")
            }
            Self::Io(e) => write!(f, "scheduler I/O failed: {e}"),
        }
    }
}

pub type VmResult<T> = Result<T, VmError>;

#[derive(Debug, Clone)]
struct CallFrame {
    func_idx: i32,
    code_start: i32,
    ip: i32,
    base_slot: usize,
    // The whole closure, not a separate captures slice, so the frame stays
    // plain data with exactly one root: everything it captures is reachable
    // while the frame is live.
    captures: Value,
}

/// How many function applications a process may make before it is preempted.
const REDUCTION_BUDGET: i32 = 4000;

/// What one I/O operation (a syscall) costs in reductions. Charging I/O keeps
/// an accept/read loop from monopolizing its scheduler: ~40 I/O ops fill a
/// budget, after which the process is preempted at its next call.
const IO_REDUCTION_COST: i32 = 100;

/// How deep a scheduler's run queue may grow through yield-time injector
/// pickup. Overflow seeds late-bind to whichever scheduler has capacity
/// instead of piling onto the first one to yield. Idle pickup and inbox drain
/// stay unconditional: directed work already has a chosen destination.
const YIELD_PICKUP_QUEUE_LIMIT: usize = 4;

/// Why an execution slice ended.
enum Step {
    /// The current process ran to completion; its result is its top-of-stack.
    Done,
    /// The reduction budget ran out; the process is resumable as-is.
    Yield,
    /// The process must wait for I/O readiness or a timer.
    Parked(Wait),
}

/// A suspended lightweight process: a complete resumable continuation. The
/// RUNNING process lives directly in `VM.heap`/`VM.stack`/`VM.frames`; a
/// context switch swaps those fields with a `Process`.
///
/// A process owns its values, which is what makes it `Send` by construction —
/// migrating it to another scheduler is a plain move of this struct.
struct Process {
    // Field order is not load-bearing here: `heap` is a zero-sized marker and
    // `mi_free` is global, so a `Value` dropping after `heap` is fine.
    stack: Vec<Value>,
    frames: Vec<CallFrame>,
    is_main: bool,
    heap: ProcHeap,
    /// Program-unique process id, travelling with the process across
    /// schedulers. Connections record their owning pid so they close when the
    /// process ends — the BEAM controlling-process rule.
    pid: u64,
    /// The native-boundary scratch, suspended. Process state, not scheduler
    /// state, so it must travel and must never leak to the next process the
    /// scheduler runs. The VM fields of the same names are the running
    /// process's copies; `suspend_current`/`resume` swap them with these.
    native_floor: usize,
    native_reds: i32,
    native_pending: Option<Box<NativePending>>,
}

/// A tabled TCP connection: the stream plus its controlling process.
///
/// Ownership lives ON the entry, not in a parallel map, so the two cannot
/// disagree, and it never moves implicitly — the process that adopted the
/// connection controls it until an explicit transfer. Transferring on capture
/// instead would make a split reader/writer program's fate depend on spawn
/// order and would miss toplevel-bound sockets, since a global is not a
/// capture.
struct Conn {
    stream: TcpStream,
    owner: u64,
}

/// Lock a mutex, recovering the data if a holder thread died (the VM never
/// panics, so poisoning is effectively unreachable).
pub(super) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    match m.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

// A migrant crosses an OS-thread boundary as a plain move. This must hold by
// construction, never via an unsafe Send impl on `Process` itself.
const _: () = crate::assert_send::<Process>();

/// Borrow the string contents of a value `pop_str` already type-checked.
#[inline]
#[allow(clippy::expect_used)]
fn str_ref(v: &Value) -> &str {
    v.as_str().expect("type-checked by pop_str")
}

/// Borrow the binary view of a value `pop_binary` already type-checked.
#[inline]
#[allow(clippy::unwrap_used)]
fn bin_ref(v: &Value) -> BinaryRef<'_> {
    v.as_binary().unwrap()
}

pub struct VM {
    /// This scheduler's private copy of the program tables. Kept inline rather
    /// than behind the `Arc` because the dispatch loop reads
    /// `code`/`constants`/`functions` on every instruction and the extra
    /// pointer hop is measurable. The copy is shallow where it matters:
    /// constants are frozen words pointing into the shared `program.frozen`.
    program: Program,
    templates: Templates,
    /// Builder over the program's frozen area, kept for runtime freezing of
    /// published globals. Frozen `Value`s are immortal, so their `Drop` never
    /// reads the frozen area and field order does not matter here.
    frozen: FrozenBuilder,
    stack: Vec<Value>,
    frames: Vec<CallFrame>,
    /// The RUNNING process's arena. Swapped in and out with `stack`/`frames`
    /// on every context switch; between processes it is an empty placeholder
    /// that owns nothing.
    ///
    /// Must stay declared AFTER `stack`/`frames`: their `Value`s free into
    /// this heap on drop, and Rust drops fields top-to-bottom.
    heap: ProcHeap,
    /// Memoized field-label tuples, one per non-prelude ctor site, keyed by the
    /// address of that site's pooled labels-array constant. Labels are constant
    /// per constructor, so each site freezes its tuple once.
    label_cache: HashMap<usize, Value>,
    next_socket_id: i32,
    /// This scheduler's clones of shared listeners: "registered with MY
    /// poller", not "I bound this". The socket lives in
    /// `Runtime.shared_listeners`; the fd closes when the last clone drops.
    tcp_listeners: HashMap<i32, Arc<TcpListener>>,
    tcp_connections: HashMap<i32, Conn>,
    /// Connection ids by owning pid — the death path's index, kept in
    /// lockstep with `tcp_connections` by track/evict. A stale id (already
    /// evicted) is skipped on drain, so eviction need not scrub this map.
    conns_by_owner: HashMap<u64, Vec<i32>>,
    /// Outbound connections whose non-blocking connect is still in flight,
    /// keyed by their already-allocated socket id.
    pending_connects: HashMap<i32, socket2::Socket>,
    /// Reusable scratch buffer for socket reads, so each read allocates only
    /// its result.
    read_scratch: Vec<u8>,
    /// Whether the currently-running process is the main (root) process.
    current_is_main: bool,
    /// The running process's id ([`Process::pid`]).
    current_pid: u64,
    /// The main process's result, stashed at its `Step::Done` if it finishes
    /// while other processes are still running. The value points into main's
    /// arena, so the pair travels together: the heap is re-adopted into
    /// `self.heap` when the scheduler loop hands the value out, keeping it
    /// alive while the caller prints or inspects it.
    main_result: Option<(Value, ProcHeap)>,
    // The three fields below are the RUNNING process's native-boundary
    // scratch, mirrored into the VM for the duration of a slice and moved in
    // and out by `suspend_current`/`resume`. They are NOT scheduler state: a
    // value left here by one process must never be observable by the next.
    //
    // Today `native_floor` is back to 0 and `native_pending` back to `None`
    // by the time a non-`Done` status reaches `scheduler_loop`, because every
    // unwind runs to completion before the process suspends (asserted at slice
    // entry in `exec.rs`). A suspension that ever stops an unwind partway makes
    // the raised floor part of the suspended state and it travels.
    /// The payload of a non-`Done`
    /// [`NativeStatus`](crate::bytecode::NativeStatus) currently unwinding
    /// native frames, held here so the status word crossing the `extern "C"`
    /// boundary stays a single machine word. At most one per process.
    native_pending: Option<Box<NativePending>>,
    /// What the pinned register points at while compiled code runs. `vm` is
    /// re-published by every [`VM::call_native`], so compiled frames re-derive
    /// scheduler state instead of carrying it. Scheduler-owned, never part of
    /// a suspended process.
    native_ctx: crate::bytecode::NativeCtx,
    /// The interpreter's frame floor: `execute_slice` ends its slice when a
    /// `Ret` pops the frame stack back to exactly this depth. 0 everywhere
    /// except inside a native→interpreter re-entry, which raises it so control
    /// returns to the native caller, and restores it on the way out.
    native_floor: usize,
    /// The reduction budget on the native side of the backend boundary.
    /// `execute_slice` keeps its budget in a hot-loop local that compiled code
    /// cannot reach, so native checkpoints decrement this instead. Every
    /// crossing must hand the budget over in both directions, so one budget
    /// governs the whole slice no matter which backend spends it.
    native_reds: i32,
    /// Runnable processes in round-robin order (the running one is not here).
    run_queue: VecDeque<Process>,
    /// Processes waiting on I/O readiness or timers, keyed by a unique wait id
    /// so the timer heap can refer to a park without owning it.
    parked: HashMap<u64, (Wait, Process)>,
    /// Reverse index from socket id to the wait ids parked on it. Must be kept
    /// in lockstep with `parked` by `park`/`park_remove`. A socket id maps to
    /// more than one wait only transiently, so the inline case dominates.
    io_waiters: HashMap<i32, SmallVec<[u64; 1]>>,
    /// Monotonically increasing id handed to each park; the key into `parked`
    /// and the identity (and tie-breaker) recorded in `timer_heap`.
    next_wait_id: u64,
    /// Lazy-deletion min-heap of `(deadline, wait id)`. Entries are never
    /// eagerly removed; a stale one is discarded on pop, once its id is gone
    /// from `parked` or its live deadline no longer matches. Keeps the nearest
    /// deadline an O(log n) peek instead of an O(n) scan of `parked`.
    timer_heap: BinaryHeap<Reverse<(Instant, u64)>>,
    /// This scheduler's OS event queue. Owned by this scheduler alone; others
    /// reach it only through the runtime's waker slot.
    poll: mio::Poll,
    /// Reusable event buffer for `poll`, allocated once per VM: the parked-I/O
    /// drain runs every scheduler slice under I/O load.
    poll_events: mio::Events,
    /// The scheduler runtime, present from construction. Worker threads
    /// inside it start lazily on the first spawn.
    runtime: Arc<Runtime>,
    /// This VM's scheduler index; 0 is the main thread.
    scheduler_index: usize,
    /// The global (literal) area: top-level bindings, addressed by
    /// `Op::PushGlobal`. On scheduler 0 it mirrors main's frame; workers
    /// hydrate it from the runtime's shared area.
    globals: Vec<Value>,
    /// Which `globals` slots scheduler 0 has published, so an identical
    /// re-store skips the frozen-area copy. Meaningful on scheduler 0 only.
    globals_published: Vec<bool>,
    /// The shared-area version this scheduler last hydrated from.
    globals_synced_version: u64,
}

/// Build the VM that runs `program` as scheduler 0. Worker threads start
/// lazily on the first spawn, so tooling callers pay only one copy of the
/// program tables and one OS poller.
///
/// Fails only when scheduler 0's poller cannot be created.
///
/// `process.argv` is empty; use [`new_vm_with_argv`] to pass arguments.
pub fn new_vm(program: Program) -> VmResult<VM> {
    new_vm_with_argv(program, Vec::new())
}

/// Build the VM that runs `program` as scheduler 0, exposing `argv` (the
/// entrypoint path followed by the arguments passed after it) to
/// `process.argv`. The program's `templates`/`abi` tables are the runtime's
/// only knowledge of a front end's stdlib; they are validated here, once.
pub fn new_vm_with_argv(program: Program, argv: Vec<String>) -> VmResult<VM> {
    program
        .abi
        .validate(&program.templates)
        .map_err(|e| VmError::Internal(Cow::Owned(e)))?;
    let (runtime, poll) =
        Runtime::new(Arc::new(program), argv, sched::scheduler_count()).map_err(VmError::Io)?;
    Ok(vm_for_runtime(runtime, 0, poll))
}

/// Build the VM for scheduler `index` over an existing runtime: scheduler 0
/// via [`new_vm`], workers via [`worker_main`]. `poll` is this scheduler's own
/// OS poller; its waker lives in the runtime's waker slot so other schedulers
/// can interrupt the wait.
fn vm_for_runtime(runtime: Arc<Runtime>, index: usize, poll: mio::Poll) -> VM {
    // Only the vectors copy: the constants are frozen words pointing into
    // `program.frozen`, which stays shared.
    let program = (*runtime.program).clone();
    let mut frozen = program.frozen.builder();
    let templates = Templates::resolve(&program.abi, &program.templates, &mut frozen);
    // The entry function sizes the global area: top-level bindings are its
    // "locals", mirrored into this table as they are written.
    let globals_len = program
        .functions
        .get(program.entry as usize)
        .map(|f| f.locals as usize)
        .unwrap_or(0);
    VM {
        program,
        templates,
        frozen,
        heap: ProcHeap::new(),
        stack: Vec::new(),
        frames: Vec::new(),
        label_cache: HashMap::new(),
        next_socket_id: 1,
        tcp_listeners: HashMap::new(),
        tcp_connections: HashMap::new(),
        conns_by_owner: HashMap::new(),
        pending_connects: HashMap::new(),
        read_scratch: Vec::new(),
        current_is_main: false,
        current_pid: 0,
        main_result: None,
        native_pending: None,
        native_ctx: crate::bytecode::NativeCtx::new(),
        native_floor: 0,
        native_reds: 0,
        run_queue: VecDeque::new(),
        parked: HashMap::new(),
        io_waiters: HashMap::new(),
        next_wait_id: 0,
        timer_heap: BinaryHeap::new(),
        poll,
        poll_events: mio::Events::with_capacity(poll::EVENTS_CAPACITY),
        runtime,
        scheduler_index: index,
        globals: vec![Value::nil(); globals_len],
        globals_published: Vec::new(),
        globals_synced_version: 0,
    }
}

impl VM {
    /// The program this VM runs. Callers that print values after `run()` need
    /// it to resolve closure names.
    pub fn program(&self) -> &Program {
        &self.program
    }

    // The interpreter pushes the entry frame before the loop and never pops the
    // last one while running, so a frame is always live. These two accessors
    // are the only place that relies on it.
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

    /// Run the program to completion and return the main process's result.
    ///
    /// The returned value may point into main's arena, which this VM keeps
    /// alive. It must not outlive the VM.
    ///
    /// An `Err` is an internal invariant breach, and the runtime behind this VM
    /// is leaked rather than shut down; see the shutdown comment in the body.
    pub fn run(&mut self) -> VmResult<Value> {
        let entry = self.program.entry;
        let main_func = &self.program.functions[entry as usize];
        let (code_start, locals) = (main_func.code_start, main_func.locals);
        // Main owns a heap like every other process; it starts here rather
        // than through `resume`.
        self.heap = ProcHeap::new();
        // The entry function captures nothing, but the frame still carries a
        // closure so every frame is shaped the same.
        let entry_closure = Value::closure_in(&mut self.heap, entry, &[]);

        self.frames.push(CallFrame {
            func_idx: entry,
            code_start,
            ip: 0,
            base_slot: 0,
            captures: entry_closure,
        });

        self.stack
            .extend(std::iter::repeat_n(Value::small_int(0), locals as usize));

        self.current_is_main = true;
        self.current_pid = self.runtime.alloc_pid();
        let mut result = self.scheduler_loop();

        // On Ok the global live count is zero, so workers exit on their own.
        // On Err the count never reaches zero (the errored main cannot
        // decrement it), so the fault flag forces every scheduler out of its
        // wait instead — either way the workers can be joined and the runtime
        // reclaimed, which is what keeps a long-lived embedder (the REPL)
        // from leaking a thread set per errored evaluation.
        let rt = Arc::clone(&self.runtime);
        if result.is_err() {
            rt.raise_fault_flag();
        }
        rt.shutdown_blocking();
        let workers = std::mem::take(&mut *lock(&rt.workers));
        for handle in workers {
            if handle.join().is_err() {
                eprintln!("scheduler: a worker thread terminated abnormally");
                result = Err(VmError::internal("a scheduler thread panicked"));
            }
        }
        // A worker's fault outranks a clean main result: the program did not
        // actually finish, part of it was aborted.
        if result.is_ok()
            && let Some((index, e)) = rt.take_fault()
        {
            eprintln!("al: scheduler {index} failed");
            result = Err(e);
        }
        result
    }

    /// Drive processes to completion, round-robin with reduction-budget
    /// preemption. Scheduler 0 returns the main process's result once every
    /// process in the program (across all schedulers) has finished; worker
    /// schedulers return Nil when the program is over.
    fn scheduler_loop(&mut self) -> VmResult<Value> {
        loop {
            if self.frames.is_empty() && !self.acquire_work()? {
                // Program over. Re-adopt main's arena before handing the value
                // out: the value points into that heap, which must then live as
                // long as the VM. Workers and an errored main have no stashed
                // pair and return Nil, which needs no arena.
                let result = match self.main_result.take() {
                    Some((value, heap)) => {
                        self.heap = heap;
                        value
                    }
                    None => self.make_nil()?,
                };
                return Ok(result);
            }

            match self.run_slice()? {
                Step::Done => {
                    // The ended process's connections die with it.
                    self.release_connections_of(self.current_pid);
                    // The finished process's result is its top-of-stack.
                    if self.current_is_main {
                        // Stash the (value, heap) pair together until the
                        // loop's exit hands the value out; dropping the heap
                        // here would dangle the result.
                        let result = match self.stack.pop() {
                            Some(v) => v,
                            None => self.make_nil()?,
                        };
                        self.main_result = Some((result, std::mem::take(&mut self.heap)));
                    } else {
                        self.heap = ProcHeap::new();
                    }
                    self.stack.clear();
                    self.frames.clear();
                    self.current_is_main = false;
                    self.runtime.process_finished();
                }
                Step::Yield => {
                    // One balancing decision per yield, off a single idleness
                    // scan: pick up an overflow seed when every peer is busy
                    // and the local queue is shallow, otherwise donate.
                    //
                    // `poll_parked` runs unconditionally even though it
                    // early-outs when nothing is parked, because it first
                    // drains the retired-listener queue. Skipping it would keep
                    // a closed listener's fd registered, and its port bound,
                    // until this scheduler next idled.
                    self.poll_parked(false)?;
                    self.take_directed();
                    let peer_idle = self.others_idle();
                    if !peer_idle && self.run_queue.len() < YIELD_PICKUP_QUEUE_LIMIT {
                        self.take_overflow_seed();
                    }
                    self.try_donate(peer_idle);
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
                    // The wait's sockets have been registered with the poller
                    // since adoption, so parking arms nothing.
                    let outgoing = self.suspend_current();
                    self.park(wait, outgoing);
                }
            }
            // Any step may have changed this scheduler's runnable count; peers
            // read it for their donation decisions.
            self.publish_load();
        }
    }

    /// Detach the running process's state into a `Process`. The heap moves with
    /// the stack and frames, since the values they hold point into it.
    fn suspend_current(&mut self) -> Process {
        Process {
            heap: std::mem::take(&mut self.heap),
            stack: std::mem::take(&mut self.stack),
            frames: std::mem::take(&mut self.frames),
            is_main: std::mem::take(&mut self.current_is_main),
            pid: self.current_pid,
            native_floor: std::mem::take(&mut self.native_floor),
            native_reds: std::mem::take(&mut self.native_reds),
            native_pending: self.native_pending.take(),
        }
    }

    /// Make `p` the running process.
    fn resume(&mut self, p: Process) {
        self.heap = p.heap;
        self.stack = p.stack;
        self.frames = p.frames;
        self.current_is_main = p.is_main;
        self.current_pid = p.pid;
        self.native_floor = p.native_floor;
        self.native_reds = p.native_reds;
        self.native_pending = p.native_pending;
    }

    /// Donation policy, run at most once per yield. The machinery is
    /// policy-independent; the decisions live here.
    ///
    /// The victim is the BACK of the run queue: it would wait longest for a
    /// slice here and has the coldest cache footprint. Never the main process,
    /// whose result must surface on the scheduler that owns it.
    ///
    /// The target is an idle peer first, since a sleeping core is the worst
    /// imbalance; one queued process is enough to justify the move. Otherwise
    /// the least-loaded busy peer, taken only when this scheduler is ahead by
    /// two, so each move strictly narrows the gap. Without busy-peer donation,
    /// long CPU-bound processes sit five-deep on one scheduler and alone on
    /// another until one drains completely.
    ///
    /// Order is part of the claim protocol: effect-free checks first (the fd
    /// walk only after the target probe, so the common nothing-to-do yield
    /// never traverses the victim), then the peer claim, then the fd detach
    /// once a destination is guaranteed. A donation that aborts after a
    /// successful claim must still notify the claimed peer, or it sleeps
    /// unwakeable by submitters.
    fn try_donate(&mut self, peer_idle: bool) {
        let Some(victim) = self.run_queue.back() else {
            return;
        };
        if victim.is_main {
            return;
        }
        let rt = Arc::clone(&self.runtime);
        // Most yields have no one worth donating to, so probe first.
        let busy_peer = if peer_idle {
            None
        } else {
            let my_load = 1 + self.run_queue.len();
            rt.pick_underloaded_peer(self.scheduler_index, my_load)
        };
        if !peer_idle && busy_peer.is_none() {
            return;
        }
        if !self.can_donate_fds(victim) {
            return;
        }
        enum Target {
            Claimed(sched::Claim),
            Busy(usize),
        }
        let target = match busy_peer {
            Some(peer) => Target::Busy(peer),
            // The idle peer seen by the probe may have been claimed or woken
            // meanwhile; the next yield retries.
            None => match rt.claim_idle_peer() {
                Some(claim) => Target::Claimed(claim),
                None => return,
            },
        };
        let Some(victim) = self.run_queue.pop_back() else {
            // Unreachable (the queue is local and non-empty), but a claimed
            // peer must still be woken so it can re-park.
            if let Target::Claimed(c) = target {
                rt.release(c);
            }
            return;
        };
        let connections = self.detach_fds(&victim);
        let m = Migrant {
            process: victim,
            connections,
        };
        match target {
            Target::Claimed(c) => rt.hand(c, Inbound::Migrant(m)),
            Target::Busy(peer) => rt.donate(peer, m),
        }
    }

    /// Get something to run into `stack`/`frames`. Blocks while there is
    /// nothing local to run but the program is still alive somewhere.
    /// Returns false once the whole program has finished.
    fn acquire_work(&mut self) -> VmResult<bool> {
        loop {
            // 0. A fault anywhere ends the program: drain out, dropping local
            // work. `raise_fault` woke every scheduler so this check is seen.
            if self.runtime.is_faulted() {
                return Ok(false);
            }
            // 1. Local runnable processes.
            if let Some(p) = self.run_queue.pop_front() {
                self.resume(p);
                return Ok(true);
            }

            // 2. Remote seeds, then seeds sitting untaken in a peer's inbox:
            //    stealing one starts it sooner than waiting for its assigned
            //    scheduler to wake.
            if self.take_inbound() {
                continue;
            }
            if self.steal_inbound() {
                continue;
            }

            // Republish the (zero) load before parking below. A seed a peer
            // stole out of this inbox leaves `submit`'s in-flight bump in our
            // published-load slot, and nothing else corrects it while we sleep.
            self.publish_load();

            // 3. Local parked I/O and timers: wait, but stay wakeable by other
            //    schedulers.
            if !self.parked.is_empty() {
                self.set_parked_flag(true);
                // Re-check after publishing the flag: a submitter who scanned
                // before it was visible may already have pushed work.
                if self.take_inbound() {
                    self.set_parked_flag(false);
                    continue;
                }
                let poll_result = self.poll_parked(true);
                self.set_parked_flag(false);
                poll_result?;
                continue;
            }

            // 4. Nothing local at all.
            if self.runtime_finished() {
                return Ok(false);
            }
            // Wait for a seed or for the program to end. Flag first, then
            // re-check; `notify` is sticky, so the reverse race is safe.
            self.set_parked_flag(true);
            if self.take_inbound() {
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

    /// Move inbound work destined for this scheduler (its inbox, falling back
    /// to the shared overflow queue) into the local run queue. Returns whether
    /// any arrived.
    fn take_inbound(&mut self) -> bool {
        let batch = self.runtime.take_inbound(self.scheduler_index);
        self.admit(batch)
    }

    /// Take only the work directed at this scheduler's inbox. Called at every
    /// yield: unlike overflow seeds, directed work has a chosen destination and
    /// must not wait. Returns whether any arrived.
    fn take_directed(&mut self) -> bool {
        let batch = self.runtime.take_directed(self.scheduler_index);
        self.admit(batch)
    }

    /// Pick up undirected overflow seeds (the shared injector) into the
    /// local run queue. Returns whether any arrived.
    fn take_overflow_seed(&mut self) -> bool {
        let batch: Vec<Inbound> = self
            .runtime
            .take_overflow()
            .into_iter()
            .map(Inbound::Seed)
            .collect();
        self.admit(batch)
    }

    /// Admit a batch of inbound work into the local run queue. Returns whether
    /// anything was admitted.
    ///
    /// Inbound work may reference top-level bindings published after our last
    /// sync, so the global area is refreshed first. Publish happens-before
    /// submit and submit happens-before take, so this can never be stale.
    fn admit(&mut self, batch: Vec<Inbound>) -> bool {
        if batch.is_empty() {
            return false;
        }
        self.sync_globals();
        for inbound in batch {
            match inbound {
                Inbound::Seed(seed) => self.hydrate_seed(seed),
                Inbound::Migrant(m) => self.adopt_migrant(m),
            }
        }
        true
    }

    /// Steal one undelivered unit of inbound work from a peer scheduler's
    /// inbox. Only called when this scheduler has nothing local to run.
    fn steal_inbound(&mut self) -> bool {
        let Some(inbound) = self.runtime.steal_inbound(self.scheduler_index) else {
            return false;
        };
        self.admit(vec![inbound])
    }

    /// Bring this scheduler's global area up to date with the runtime's shared
    /// globals. The shared table holds frozen words, so syncing is a plain word
    /// copy per slot. The `Acquire` load pairs with the `Release` bump in
    /// `publish_global`, making the frozen segment visible before the read.
    fn sync_globals(&mut self) {
        let version = self
            .runtime
            .globals_version
            .load(std::sync::atomic::Ordering::Acquire);
        if version == self.globals_synced_version {
            return;
        }
        let shared = lock(&self.runtime.globals);
        if self.globals.len() < shared.len() {
            self.globals.resize(shared.len(), Value::nil());
        }
        for (slot, entry) in shared.iter().enumerate() {
            if let Some(fv) = entry {
                self.globals[slot] = fv.value();
            }
        }
        drop(shared);
        self.globals_synced_version = version;
    }

    /// Whether every process on every scheduler has finished.
    fn runtime_finished(&self) -> bool {
        self.runtime.is_finished()
    }

    /// Report this scheduler's runnable count (running process + queue) to
    /// the shared load board peers read when picking a donation target.
    fn publish_load(&self) {
        let load = usize::from(!self.frames.is_empty()) + self.run_queue.len();
        self.runtime.publish_load(self.scheduler_index, load);
    }

    /// Whether some other scheduler is idle and able to take injector seeds.
    fn others_idle(&self) -> bool {
        self.runtime.any_other_idle(self.scheduler_index)
    }

    /// Mark this scheduler parked or unparked, so seed submitters know who to
    /// wake.
    fn set_parked_flag(&self, parked: bool) {
        self.runtime.set_parked(self.scheduler_index, parked);
    }

    /// Block until another scheduler notifies this one (seed submitted or
    /// program finished).
    fn wait_for_notify(&mut self) -> VmResult<()> {
        self.poll
            .poll(&mut self.poll_events, None)
            .map_err(VmError::Io)?;
        // A retire notify wakes an idle scheduler with no seed to run. Its
        // registration and Arc clone must still be dropped here, or the shared
        // fd never closes.
        let _ = self.process_retired_listeners();
        Ok(())
    }

    /// `scheduler.spawn(f)`: start a new process running the closure `f`.
    ///
    /// The first submit summons the runtime's worker threads, one scheduler per
    /// core. With no scheduler idle the seed overflows to the shared injector,
    /// and whichever frees up first picks it up.
    fn spawn_process(&mut self, f: Value) -> VmResult<()> {
        self.check_spawnable(&f)?;
        let seed = self.build_seed(&f);
        self.runtime.submit(seed);
        Ok(())
    }

    /// A spawned closure must be a nullary function. Both are compiler
    /// invariants, so a violation is an internal error, not a user one.
    fn check_spawnable(&self, f: &Value) -> VmResult<()> {
        let Some(cl) = f.as_closure() else {
            return Err(VmError::internal("spawn requires a function"));
        };
        if self.program.functions[cl.func_idx() as usize].arity != 0 {
            return Err(VmError::internal("spawned functions take no arguments"));
        }
        Ok(())
    }

    /// Spawn `f` pinned to this scheduler, so any socket it captured stays in
    /// this scheduler's tables: no fd detach, no cross-core handoff. The
    /// shared-nothing half of the accept fan-out.
    fn spawn_local(&mut self, f: Value) -> VmResult<()> {
        self.check_spawnable(&f)?;
        // Processes share no heap, so the closure graph is copied, but captured
        // fds stay in place: parent and child run on the same scheduler and
        // reference the same socket tables.
        let (heap, root) = ProcHeap::spawn(&f);
        self.runtime.process_started();
        // No ownership change. A handler that should close the socket when it
        // finishes must do so explicitly.
        self.spawn_process_with_heap(heap, root);
        Ok(())
    }

    /// Spawn one copy of `f` pinned to every live scheduler, turning a single
    /// accept loop into one acceptor per core. A listener is one kernel socket,
    /// so all copies drain the same queue; the spread is only a locality
    /// preference.
    fn spawn_on_each(&mut self, f: Value) -> VmResult<()> {
        self.check_spawnable(&f)?;
        // Every scheduler slot must be live before a copy is placed on it.
        // Skipping a never-spawned worker is safe: any live acceptor drains
        // the shared queue.
        self.runtime.ensure_workers();
        for i in 0..self.runtime.scheduler_count() {
            if i == self.scheduler_index {
                let (heap, root) = ProcHeap::spawn(&f);
                self.runtime.process_started();
                self.spawn_process_with_heap(heap, root);
            } else if self.runtime.is_live_scheduler(i) {
                let seed = self.build_seed(&f);
                self.runtime.submit_to(i, seed);
            }
        }
        Ok(())
    }

    /// Close every connection the just-ended process `pid` controls — the BEAM
    /// rule that a socket lives as long as its controlling process. Anything
    /// parked on one fails with the stale-socket `NetError` instead of sleeping
    /// forever.
    ///
    /// Scans every live connection on this scheduler per process death, behind
    /// an emptiness gate so compute-only programs pay one branch.
    fn release_connections_of(&mut self, pid: u64) {
        let Some(ids) = self.conns_by_owner.remove(&pid) else {
            return;
        };
        for id in ids {
            // Skip ids already evicted (close, migration): the index is
            // append-only between deaths.
            if self
                .tcp_connections
                .get(&id)
                .is_some_and(|c| c.owner == pid)
            {
                drop(self.evict_connection(id));
            }
        }
    }

    /// Create a process whose initial heap is `heap` and whose closure `f`
    /// points into it. `f` is a nullary closure by construction: every caller
    /// has run `check_spawnable`, and `ProcHeap::spawn` preserves value kind.
    #[allow(clippy::expect_used)]
    fn spawn_process_with_heap(&mut self, heap: ProcHeap, f: Value) -> u64 {
        let pid = self.runtime.alloc_pid();
        self.spawn_process_with_heap_as(pid, heap, f);
        pid
    }

    #[allow(clippy::expect_used)]
    fn spawn_process_with_heap_as(&mut self, pid: u64, heap: ProcHeap, f: Value) {
        let cl = f
            .as_closure()
            .expect("spawn: caller-checked nullary closure");
        let func = &self.program.functions[cl.func_idx() as usize];
        let (func_idx, code_start, locals) = (cl.func_idx(), func.code_start, func.locals);

        let mut stack = Vec::with_capacity(locals as usize + 8);
        stack.extend(std::iter::repeat_n(Value::small_int(0), locals as usize));

        let frames = vec![CallFrame {
            func_idx,
            code_start,
            ip: 0,
            base_slot: 0,
            captures: f,
        }];

        self.run_queue.push_back(Process {
            heap,
            stack,
            frames,
            is_main: false,
            pid,
            native_floor: 0,
            native_reds: 0,
            native_pending: None,
        });
    }

    /// Copy a closure into a seed the child adopts wholesale. No value is
    /// shared with this scheduler afterwards, so handing the seed to another OS
    /// thread is a plain move. `Binary` `Arc` backings are shared zero-copy:
    /// only the box is copied, never the bytes.
    fn build_seed(&mut self, f: &Value) -> Seed {
        // The heap copy knows nothing about fd tables, so captured sockets need
        // their own walk.
        let mut captured_sockets = Vec::new();
        migrate::for_each_socket(f, &mut |s| captured_sockets.push(s.id));

        // `root` points into `heap`, so the pair is self-contained and `Send`.
        let (heap, root) = ProcHeap::spawn(f);

        // Captured connections move to the child; captured listeners stay put,
        // since the shared socket needs no transfer. The move IS the ownership
        // handoff: once the fd leaves this scheduler the child is the only
        // process that can use it. This is the one place ownership changes
        // hands, and it follows the fd's forced move, not a capture heuristic.
        let pid = self.runtime.alloc_pid();
        let mut connections = self.detach_socket_ids(captured_sockets);
        for (_, _, owner) in &mut connections {
            *owner = pid;
        }

        Seed {
            pid,
            heap,
            root,
            connections,
        }
    }

    /// Build a runnable process on this scheduler from a seed. The seed carries
    /// no top-level bindings: those come from the global area, which `admit`
    /// syncs on every arrival path.
    fn hydrate_seed(&mut self, seed: Seed) {
        self.spawn_process_with_heap_as(seed.pid, seed.heap, seed.root);
        self.adopt_connections(seed.connections);
    }
}

/// Entry point for worker scheduler threads (indices 1..N), spawned by
/// `Runtime::ensure_workers` on the first submit. Each worker runs its own VM
/// over the shared runtime and acquires work like any scheduler until the
/// whole program finishes.
fn worker_main(runtime: Arc<Runtime>, index: usize, poll: mio::Poll) {
    let mut vm = vm_for_runtime(runtime, index, poll);

    // Workers have no main process; scheduler_loop starts by acquiring work.
    if let Err(e) = vm.scheduler_loop() {
        // A VM error means a compiler bug. Hand it to scheduler 0 through the
        // runtime's fault slot — which also drains every other scheduler —
        // so an embedder can report it instead of losing the whole process.
        vm.runtime.raise_fault(vm.scheduler_index, e);
    }
}

/// Construct a Nil enum value in `h` without a `VM`, for tests that build
/// values outside an interpreter.
#[cfg(test)]
fn nil_value(h: &mut ProcHeap, nil_id: crate::TypeId) -> Value {
    Value::enum_with_names_in(h, nil_id, 0, "Nil", "Nil", &[], &[])
}

/// A VM over a minimal halt-only program, for tests that drive the VM's side
/// tables directly and never execute the program.
#[cfg(test)]
fn halt_test_vm() -> VM {
    let frozen = Arc::new(crate::frozen::FrozenArea::new());
    let mut fb = frozen.builder();
    let (templates, abi) = crate::template::test_fixture::build(&mut fb);
    drop(fb);
    let program = Program {
        constants: Vec::new(),
        functions: vec![Function {
            name: "main".into(),
            arity: 0,
            locals: 0,
            capture_count: 0,
            code_start: 0,
            code_len: 1,
        }],
        code: vec![op(Op::Halt)],
        entry: 0,
        frozen,
        native: Default::default(),
        templates,
        abi,
    };
    new_vm(program).expect("test VM construction must succeed")
}
