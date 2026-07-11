//! The virtual machine: lightweight processes and the schedulers that run
//! them.
//!
//! This module is the runtime's front door. It owns the [`VM`] — one per
//! scheduler thread — whose dispatch loop (`execute_slice`) runs thousands
//! of cooperatively preempted processes over a handful of OS threads
//! without ever letting one block another. The memory those processes run
//! on is [`al_core::heap`]'s: each process owns its reference-counted value
//! graph, and this module is the opcode-side counterpart of that design — it
//! decides when to run, park, and move a process, while the heap decides how
//! to allocate and free. The scheduling shape: reduction budgets, run
//! queues, an injector for spawns, work stealing, and donation-based
//! migration.
//!
//! # The process lifecycle in one diagram
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
//! # The six ideas (everything else is consequence)
//!
//! 1. **A context switch is a few pointer moves.** The running process
//!    lives directly in the VM's `heap`/`stack`/`frames` fields — the
//!    dispatch loop never indirects through a process handle — and a
//!    suspended one is those same fields packed into a [`Process`].
//!    `suspend_current`/`resume` just swap them.
//! 2. **Preemption is budgeted.** A slice runs until
//!    its [`REDUCTION_BUDGET`] is spent: a call costs one reduction, a
//!    syscall [`IO_REDUCTION_COST`] — so accept loops are preempted like
//!    everything else, and no process rides free.
//! 3. **Blocking parks the process, never the thread.** An op that would
//!    block returns [`Step::Parked`] carrying a [`Wait`] — socket
//!    readiness, a timer, or a blocking-pool job. The scheduler
//!    shelves the process, arms the OS poller and the timer heap, and runs
//!    something else; the wake re-runs the instruction or completes the
//!    pending connect ([`poll::WakeAction`]).
//! 4. **Work moves between schedulers as owned memory.** The runtime
//!    ([`sched`]) exists from VM construction; the first spawn summons its
//!    worker threads, one scheduler per core. Each spawn copies its
//!    closure graph into a fresh heap and submits the result as a
//!    [`sched::Seed`]; load balancing donates whole queued processes the
//!    same way ([`migrate`]). Plain moves, in both cases — the receiver
//!    adopts the arriving values as-is.
//! 5. **Memory is reference-counted.** A `Value` is not `Copy`: `Clone`
//!    increments an object's count, `Drop` frees it at zero (see
//!    [`al_core::heap`]). Nothing moves, so there is no rooting rule and no
//!    allocation reservation — an opcode just pops its operands and builds
//!    its result. A large cascading free is billed to the running process at
//!    the next call checkpoint (`VM::charge_reclamation`) so it cannot stall
//!    the scheduler.
//! 6. **A value and its memory travel together.** A process owns every
//!    heap value it can reach, which is what makes seeds, migrants, and
//!    suspended processes `Send` by construction — and why main's result
//!    is stashed as a `(value, heap)` pair until `run()`'s caller is done
//!    with it.
//!
//! # Reading order
//!
//! | file              | the one thing it does                              |
//! |-------------------|----------------------------------------------------|
//! | this file         | the [`VM`] and the scheduling story: `run`,        |
//! |                   | `scheduler_loop`, `acquire_work`, suspend/resume,  |
//! |                   | donation policy, spawn/seed glue                   |
//! | [`exec`]          | the dispatch loop (`execute_slice`): inline arms,  |
//! |                   | family-handler routing, stack helpers, and the     |
//! |                   | call-checkpoint reclamation-fairness charge        |
//! | [`collections`]   | array/tuple/range/field-access opcodes             |
//! | [`text`]          | string/binary builtins and HTTP-scanner opcodes    |
//! | [`io`]            | file/socket/DNS/sleep/spawn opcodes — everything   |
//! |                   | that can park — and the per-scheduler fd tables    |
//! | [`poll`]          | parking and wake-up: [`Wait`], the OS poller, the  |
//! |                   | timer heap, blocking-pool completion delivery      |
//! | [`sched`]         | the scheduler runtime: worker threads, the         |
//! |                   | seed injector and inboxes, the blocking pool,      |
//! |                   | park/notify                                        |
//! | [`migrate`]       | cross-scheduler process movement: donation glue    |
//! |                   | and fd re-homing                                   |
//! | [`freeze`]        | publishing top-level bindings into the program-    |
//! |                   | wide frozen area                                   |
//! | [`templates`]     | precomputed frozen enum templates for prelude and  |
//! |                   | stdlib value construction                          |
//! | [`binary`]        | bit-granular reads and writes on `Binary` values   |
//! | [`http`]          | the HTTP/1.1 byte-scanning hot paths behind        |
//! |                   | `al/http`                                          |
//! | [`inspect()`]     | value rendering for `Print`/`ToString` and the     |
//! |                   | CLI/REPL result line (`vm::inspect`)               |
//!
//! # The life of a process
//!
//! `spawn(handler)` in a server: the opcode deep-copies the handler closure's
//! graph into a fresh heap and submits the pair as a seed,
//! waking an idle scheduler. That scheduler adopts the process, frames the
//! closure, and resumes it; the child's
//! `accept` would block, so it parks — interests armed in the poller, the
//! suspended [`Process`] shelved under a wait id — and the scheduler runs
//! its next process. A connection arrives: the poller wakes the process,
//! the accept re-runs and succeeds. Later it exhausts a reduction budget
//! mid-computation and yields to the back of the run queue; a peer
//! scheduler goes idle, and at this scheduler's next yield it claims
//! that peer and donates the queued process — values, stack, frames,
//! socket fds — which moves over in one piece and keeps running there.
//! Its final `Ret` leaves the result on top of the stack: the process
//! is done, its values are freed as it drops, and the runtime's live
//! count falls. When the count reaches zero, `acquire_work` returns
//! false on every scheduler, the workers exit, and scheduler 0 hands
//! main's stashed result back to `run()`'s caller.

use std::borrow::Cow;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::fmt;
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use al_core::bytecode::value::range_len;
use al_core::bytecode::{BinaryRef, Program, Value};
#[cfg(test)]
use al_core::bytecode::{Function, Op, op};
use al_core::frozen::FrozenBuilder;
use al_core::heap::ProcHeap;
use smallvec::SmallVec;

mod binary;
mod collections;
mod exec;
mod freeze;
mod http;
mod inspect;
mod io;
mod map;
mod migrate;
mod poll;
mod sched;
mod templates;
#[cfg(test)]
mod tests;
mod text;

pub use inspect::inspect;
use inspect::{f64_str, value_type_name};
use migrate::Migrant;
use poll::Wait;
use sched::{Inbound, Runtime, Seed};
use templates::{EnumTemplate, PreludeTemplates, enum_template};
use text::{int_to_ascii, parse_uint_ascii};

/// A VM-level failure. The variant distinguishes user-visible runtime
/// errors (out-of-bounds, type mismatch — surfaced to AL as a panic
/// message) from broken type-system invariants (`Internal`: a compiler
/// bug, not user error) and infrastructure failures (`Io`: mio poll / fd
/// exhaustion). The top-level handler ([`VM::run`], [`worker_main`]) acts
/// on that distinction rather than parsing a string.
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
    /// Type-system invariant broken — indicates a compiler bug, not user
    /// error. The runtime behind an `Internal`-errored run is leaked (see
    /// [`VM::run`]).
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
    // The closure this frame is executing, as a single `Value`. Captures are
    // read through it (`ClosureRef::captures`), `Op::PushSelf` is a plain
    // copy of it, and pushing a child frame on every
    // `Op::Call`/`Op::TailCall`/`Op::CallSelf` copies one word-sized handle.
    // Keeping the whole closure here (rather than a separate captures slice)
    // makes the frame plain data and gives the GC exactly one root per frame:
    // the closure and everything it captures stay reachable while the frame
    // is live.
    captures: Value,
}

/// How many function applications a process may make before it is preempted.
const REDUCTION_BUDGET: i32 = 4000;

/// What one I/O operation (a syscall) costs in reductions. Charging I/O keeps
/// an accept/read loop from monopolizing its scheduler: ~40 I/O ops fill a
/// budget, after which the process is preempted at its next call.
const IO_REDUCTION_COST: i32 = 100;

/// How deep a scheduler's run queue may grow through yield-time injector
/// pickup. Overflow seeds are
/// admitted to a busy scheduler only while its queue is shallow, so they
/// late-bind to whichever scheduler has capacity instead of piling onto
/// the first one to yield. Idle pickup (acquire_work) stays unconditional,
/// as does inbox drain — directed work (donated migrants, direct-handed
/// seeds) already has a chosen destination and is never made to wait.
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

/// A suspended lightweight process — a complete resumable continuation.
/// The *running* process lives directly in `VM.heap`/`VM.stack`/`VM.frames`
/// so the dispatch loop is untouched; a context switch swaps those fields
/// with a `Process` (a few pointer moves).
///
/// A process *owns* its values: `heap` is the process-private arena that
/// every heap-backed `Value` in `stack`/`frames` lives in (idea 6 above).
/// Owning the memory is what makes a suspended process `Send` by
/// construction — migrating it to another scheduler is a plain move of this
/// struct, heap and all.
struct Process {
    // Field order is not load-bearing: `heap` is a zero-sized allocator marker
    // (allocation goes to mimalloc's per-thread default heap), and `mi_free` is
    // global and thread-safe, so a `Value` dropping after `heap` is fine.
    stack: Vec<Value>,
    frames: Vec<CallFrame>,
    is_main: bool,
    heap: ProcHeap,
}

/// Lock a mutex, recovering the data if a holder thread died (the VM never
/// panics, so poisoning is effectively unreachable).
pub(super) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    match m.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

// A migrant crosses an OS-thread boundary as a plain move; this must hold by
// construction (owned arena + plain-data frames), never via unsafe impls.
const _: () = al_core::assert_send::<Process>();

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
    /// This scheduler's private copy of the program tables, cloned from the
    /// runtime's shared [`Runtime::program`] at construction (see
    /// [`vm_for_runtime`]). Kept inline — not behind the `Arc` — because the
    /// dispatch loop reads `code`/`constants`/`functions` on every
    /// instruction and the extra pointer hop is measurable there; the copy
    /// is shallow where it matters (constants are frozen words pointing
    /// into the shared `program.frozen`).
    program: Program,
    templates: PreludeTemplates,
    /// Frozen stdlib enum templates memoized by `VariantTemplate` identity.
    template_cache: HashMap<usize, EnumTemplate>,
    /// Builder over the program's frozen area, kept for runtime freezing of
    /// stdlib templates (`stdlib_template`) built on demand (error values).
    /// Field order is not load-bearing: frozen `Value`s are immortal, so their
    /// `Drop` never reads the frozen area (see `VALUE_IMMORTAL`).
    frozen: FrozenBuilder,
    stack: Vec<Value>,
    frames: Vec<CallFrame>,
    /// The *running* process's arena. Swapped in/out with `stack`/`frames` on
    /// every context switch (`suspend_current`/`resume`); between processes it
    /// is the empty placeholder (`ProcHeap::new()`), which owns nothing.
    ///
    /// Declared AFTER `stack`/`frames`: under reference counting their `Value`s
    /// free into this heap on drop, so the heap must outlive them at teardown
    /// (Rust drops fields top-to-bottom).
    heap: ProcHeap,
    /// Memoized field-label tuples, one entry per non-prelude ctor site,
    /// keyed by the address of that site's pooled labels-array constant. The
    /// labels are statically constant per constructor, so each site freezes
    /// its labels Tuple once; every later construction stores one reference
    /// word.
    label_cache: HashMap<usize, Value>,
    next_socket_id: i32,
    /// This scheduler's clones of shared listeners — "registered with MY
    /// poller", not "I bound this". The socket itself lives in
    /// `Runtime.shared_listeners`; the fd closes when the last clone drops.
    tcp_listeners: HashMap<i32, Arc<TcpListener>>,
    tcp_connections: HashMap<i32, TcpStream>,
    /// Outbound connections whose non-blocking connect is still in flight,
    /// keyed by their already-allocated socket id.
    pending_connects: HashMap<i32, socket2::Socket>,
    /// Reusable scratch buffer for socket reads, grown on demand, so each
    /// read allocates only its result.
    read_scratch: Vec<u8>,
    /// Whether the currently-running process is the main (root) process.
    current_is_main: bool,
    /// The main process's result, stashed at its `Step::Done` if it finishes
    /// while other processes are still running. The value points into main's
    /// arena, so the (value, heap) pair travels together (idea 6 above):
    /// the heap is re-adopted into `self.heap` when the scheduler loop
    /// hands the value out, keeping it alive for the VM's lifetime — which
    /// covers the caller's consumption (print/inspect) of the result.
    main_result: Option<(Value, ProcHeap)>,
    /// Runnable processes in round-robin order (the running one is not here).
    run_queue: VecDeque<Process>,
    /// Processes waiting on I/O readiness or timers, keyed by a unique wait id
    /// so the timer heap can refer to a park without owning it.
    parked: HashMap<u64, (Wait, Process)>,
    /// Reverse index from socket id to the wait ids parked on it, kept in
    /// lockstep with `parked` by `park`/`park_remove`. Lets an I/O
    /// event find its waiters in O(1) instead of scanning every park, and
    /// makes "is anything waiting on I/O" a non-emptiness check. A socket id
    /// maps to multiple waits only transiently (e.g. a reader and a writer
    /// parked on the same connection), so the one-element inline case
    /// dominates.
    io_waiters: HashMap<i32, SmallVec<[u64; 1]>>,
    /// Monotonically increasing id handed to each park; the key into `parked`
    /// and the identity (and tie-breaker) recorded in `timer_heap`.
    next_wait_id: u64,
    /// Lazy-deletion min-heap of `(deadline, wait id)`. A park with a deadline
    /// pushes one entry and never eagerly removes it: when the park instead
    /// wakes early on I/O, the entry is discarded on pop once its id is gone
    /// from `parked` (or its live deadline no longer matches). This keeps the
    /// nearest deadline an O(log n) peek instead of an O(n) scan of `parked`.
    timer_heap: BinaryHeap<Reverse<(Instant, u64)>>,
    /// This scheduler's OS event queue (kqueue/epoll). Owned by this
    /// scheduler alone; other schedulers reach it only through the waker
    /// in the runtime's `slots[scheduler_index].waker`.
    poll: mio::Poll,
    /// Reusable event buffer for `poll` (mio clears it on each `poll()`),
    /// allocated once per VM instead of per poll call — the parked-I/O
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
    /// The shared-area version this scheduler last hydrated from.
    globals_synced_version: u64,
}

/// Build the VM that runs `program` as scheduler 0. The runtime is
/// constructed here, before any code runs; its worker threads start lazily
/// on the first spawn, so tooling callers (REPL, tests) pay only the
/// allocations, one copy of the program tables, and one OS poller.
///
/// Fails only when scheduler 0's poller cannot be created (fd exhaustion):
/// the calling thread could never park, so there is no VM to build.
///
/// Built with no command-line arguments; `process.argv` returns an empty
/// array. Use [`new_vm_with_argv`] to make program arguments visible.
pub fn new_vm(program: Program) -> VmResult<VM> {
    new_vm_with_argv(program, Vec::new())
}

/// Build the VM that runs `program` as scheduler 0, exposing `argv` (the
/// entrypoint path followed by the arguments passed after it) to
/// `process.argv`.
pub fn new_vm_with_argv(program: Program, argv: Vec<String>) -> VmResult<VM> {
    let (runtime, poll) =
        Runtime::new(Arc::new(program), argv, sched::scheduler_count()).map_err(VmError::Io)?;
    Ok(vm_for_runtime(runtime, 0, poll))
}

/// Build the VM for scheduler `index` over an existing runtime: scheduler 0
/// via [`new_vm`], workers via [`worker_main`]. `poll` is this scheduler's
/// own OS poller (created alongside the runtime for scheduler 0, alongside
/// the worker thread by `ensure_workers`); its waker lives in the runtime's
/// waker slot so other schedulers can interrupt the wait.
fn vm_for_runtime(runtime: Arc<Runtime>, index: usize, poll: mio::Poll) -> VM {
    // Every scheduler runs against a private copy of the program tables
    // (the constants are frozen words pointing into `program.frozen`, which
    // stays shared — only the vectors copy).
    let program = (*runtime.program).clone();
    let mut frozen = program.frozen.builder();
    let templates = PreludeTemplates::new(&mut frozen);
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
        frozen,
        template_cache: HashMap::new(),
        heap: ProcHeap::new(),
        stack: Vec::new(),
        frames: Vec::new(),
        label_cache: HashMap::new(),
        next_socket_id: 1,
        tcp_listeners: HashMap::new(),
        tcp_connections: HashMap::new(),
        pending_connects: HashMap::new(),
        read_scratch: Vec::new(),
        current_is_main: false,
        main_result: None,
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
        globals_synced_version: 0,
    }
}

impl VM {
    /// The program this VM runs. Callers that print values after `run()`
    /// (CLI, REPL) need it to resolve closure names via
    /// `program.functions[func_idx]`.
    pub fn program(&self) -> &Program {
        &self.program
    }

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

    /// Run the program to completion and return the main process's result.
    ///
    /// The returned value may point into main's arena, which this VM keeps
    /// alive (the scheduler loop re-adopts it as `self.heap` on exit): the
    /// value is valid for as long as the VM is, so callers may print or
    /// inspect it after `run` returns but must not let it outlive the VM.
    ///
    /// An `Err` is an internal invariant breach, and the runtime behind this
    /// VM is leaked rather than shut down (see the shutdown comment in the
    /// body): an embedder that keeps the process alive after an errored run
    /// — the REPL — accepts that leak; a one-shot embedder — the CLI —
    /// exits, which reaps everything.
    pub fn run(&mut self) -> VmResult<Value> {
        let entry = self.program.entry;
        let main_func = &self.program.functions[entry as usize];
        let (code_start, locals) = (main_func.code_start, main_func.locals);
        // The main process owns a heap like every other process; it starts
        // here rather than through `resume`.
        self.heap = ProcHeap::new();
        // The entry function captures nothing; the frame still carries a
        // closure value for it so every frame is shaped the same. Budgeted
        // like any other allocation — the stack and frames are empty, so
        // the root set is trivially consistent.
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
        let mut result = self.scheduler_loop();

        // Shutdown: by the time scheduler 0's loop returns Ok, the global
        // live count is zero, so workers are exiting; join them.
        //
        // On Err no shutdown is attempted, by contract. An Err is an
        // internal invariant breach (the "likely a compiler bug" failures),
        // and the errored main never decrements `live`, so the count cannot
        // reach zero: the workers cannot be joined — they would park
        // forever — and the runtime (worker threads, warm blocking-pool
        // threads, the runtime Arc) is deliberately leaked instead. A
        // one-shot embedder (the CLI) exits right after, reaping
        // everything; a long-lived embedder that swallows the error (the
        // REPL) pays one leaked runtime per errored evaluation. The
        // symmetric worker-side breach exits the whole process — see
        // `worker_main`.
        if result.is_ok() {
            let rt = &self.runtime;
            rt.shutdown_blocking();
            let workers = std::mem::take(&mut *lock(&rt.workers));
            for handle in workers {
                if handle.join().is_err() {
                    eprintln!("scheduler: a worker thread terminated abnormally");
                    result = Err(VmError::internal("a scheduler thread panicked"));
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
                // Program over, nothing current: the heap slot holds the empty
                // placeholder. Re-adopt main's arena into it before handing
                // the value out — the value points into that heap, which now
                // lives as long as the VM, covering the caller's
                // print/inspect of the result. Workers (and an errored main)
                // have no stashed pair and return Nil, which needs no arena.
                let result = match self.main_result.take() {
                    Some((value, heap)) => {
                        self.heap = heap;
                        value
                    }
                    None => self.make_nil(),
                };
                return Ok(result);
            }

            match self.execute_slice()? {
                Step::Done => {
                    // The finished process's result is its top-of-stack.
                    if self.current_is_main {
                        // Main's result points into main's arena: stash the
                        // (value, heap) pair together until the loop's exit
                        // hands the value to `run`'s caller (idea 6 in the
                        // front door) — dropping the heap here would dangle
                        // the result. The take leaves the empty placeholder
                        // current.
                        let result = self.stack.pop().unwrap_or_else(|| self.make_nil());
                        self.main_result = Some((result, std::mem::take(&mut self.heap)));
                    } else {
                        // Drop the finished process's arena with its
                        // stack/frames; the placeholder owns nothing until
                        // the next `resume`.
                        self.heap = ProcHeap::new();
                    }
                    self.stack.clear();
                    self.frames.clear();
                    self.current_is_main = false;
                    self.runtime.process_finished();
                }
                Step::Yield => {
                    // Preempted. Wake any ready parked processes, take the
                    // work directed at this scheduler (donated migrants,
                    // direct-handed seeds), then make one balancing decision
                    // off a single idleness scan:
                    //
                    // - every peer busy + shallow local queue: pick up an
                    //   overflow seed (one per yield) so queued spawns get
                    //   time-sliced in rather than starving until a local
                    //   process finishes;
                    // - a peer idle or trailing this scheduler's queue depth:
                    //   donate a queued process to it (migration) instead of
                    //   letting work wait many slices here.
                    // Unconditional: `poll_parked` early-outs when nothing
                    // is parked, but only AFTER draining this scheduler's
                    // retired-listener queue — a busy scheduler that skipped
                    // it here would keep a closed listener's fd registered
                    // (and the port bound) until it next idled.
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
                    // Shelve the suspended process under a fresh wait id and
                    // register its wake conditions (fd index / timer heap /
                    // blocking pool). The wait's sockets have been registered
                    // with the poller since adoption; parking arms nothing.
                    let outgoing = self.suspend_current();
                    self.park(wait, outgoing);
                }
            }
            // Every step may have changed this scheduler's runnable count
            // (finish, park, pickup, donation); report it for peers'
            // donation decisions.
            self.publish_load();
        }
    }

    /// Detach the running process's state into a `Process`. The heap moves
    /// with the stack and frames — the values they hold point into it — and
    /// the VM is left with the empty placeholder heap until the next
    /// `resume` installs an owner.
    fn suspend_current(&mut self) -> Process {
        Process {
            heap: std::mem::take(&mut self.heap),
            stack: std::mem::take(&mut self.stack),
            frames: std::mem::take(&mut self.frames),
            is_main: std::mem::take(&mut self.current_is_main),
        }
    }

    /// Make `p` the running process.
    fn resume(&mut self, p: Process) {
        self.heap = p.heap;
        self.stack = p.stack;
        self.frames = p.frames;
        self.current_is_main = p.is_main;
    }

    /// Donation policy, run at most once per yield: give the coldest queued
    /// process to the peer that needs it most. The machinery (`detach_fds`/
    /// `adopt_migrant`, `Runtime::claim_idle_peer`/`pick_underloaded_peer`/
    /// `donate`) is policy-independent; the decisions live here:
    ///
    /// - Victim: the BACK of the run queue — the process that would wait
    ///   longest for a slice here, with the coldest cache footprint.
    /// - Target: an idle peer first (`peer_idle`, claimed by flag CAS) — a
    ///   sleeping core is the worst imbalance. Otherwise the least-loaded
    ///   busy peer, taken only when this scheduler is ahead by at least two
    ///   runnable processes so each move strictly narrows the gap (see
    ///   `Runtime::pick_underloaded_peer`). Busy-peer donation is what
    ///   levels queue depths mid-run: long CPU-bound processes can sit
    ///   five-deep on one scheduler and alone on another, and with only
    ///   idle-driven donation nothing moves until a scheduler drains
    ///   completely — completion times then spread by whole multiples of a
    ///   process's runtime.
    /// - Threshold for an idle target: queue length >= 1. The donor keeps
    ///   the process it is running and the idle core gains a whole process —
    ///   strictly better balance. Requiring >= 2 would strand every
    ///   scheduler's last queued process at the end of a run, leaving the
    ///   finish tail wide while peers sit idle.
    /// - Eligibility: never the main process (its result must surface on the
    ///   scheduler that owns it). Runnable-only is sufficient beyond that —
    ///   anything with an in-flight blocking job or armed fds is in `parked`,
    ///   not `run_queue`.
    ///
    /// Ordering is part of the claim protocol (see `Runtime::claim_idle_peer`):
    /// the effect-free checks run first — and the fd walk
    /// (`can_donate_fds`) only after the target probe, so the common
    /// nothing-to-do yield never traverses the victim — then the peer
    /// claim, and the fd detach, which moves connection fds out of the
    /// donor's tables, only once a destination is guaranteed. If the
    /// donation aborts after a successful claim, the claimed peer is
    /// notified anyway so it wakes, finds nothing, re-parks, and republishes
    /// its parked flag; otherwise it would sleep unwakeable by submitters.
    /// An unclaimed busy target needs no such wake — it never slept.
    fn try_donate(&mut self, peer_idle: bool) {
        let Some(victim) = self.run_queue.back() else {
            return;
        };
        if victim.is_main {
            return;
        }
        let rt = Arc::clone(&self.runtime);
        // Effect-free target probe before anything costly: most yields have
        // no one worth donating to.
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
            // 1. Local runnable processes.
            if let Some(p) = self.run_queue.pop_front() {
                self.resume(p);
                return Ok(true);
            }

            // 2. Remote seeds; then seeds handed to a peer's inbox that the
            //    peer has not taken yet — stealing one here starts it sooner
            //    than waiting for its assigned scheduler to wake.
            if self.take_inbound() {
                continue;
            }
            if self.steal_inbound() {
                continue;
            }

            // Nothing runnable here: republish the (zero) load before any
            // park below. A direct-handed seed a peer stole out of this
            // scheduler's inbox leaves `submit`'s in-flight bump in our
            // published-load slot, and nothing else corrects it while we sleep.
            self.publish_load();

            // 3. Local parked I/O / timers: wait for them, but stay wakeable
            //    by other schedulers (seed submissions, program end).
            if !self.parked.is_empty() {
                self.set_parked_flag(true);
                // Re-check for seeds after publishing the flag: a submitter
                // who scanned before the flag was visible may have pushed to
                // the overflow queue (or straight into our inbox) expecting
                // someone to pick it up.
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
            // Wait for a seed or for the program to end. Set the parked flag
            // first, then re-check (a submitter who missed our flag may have
            // pushed in between; `notify` is sticky so the reverse race is
            // safe).
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

    /// Take only the work directed at this scheduler (its inbox: donated
    /// migrants and direct-handed seeds) into the local run queue. Called at
    /// every yield — directed work has a chosen destination and must not
    /// wait, unlike overflow seeds. Returns whether any arrived.
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

    /// Admit a batch of inbound work into the local run queue. Returns
    /// whether anything was admitted.
    ///
    /// Inbound work — seed or migrant — may reference top-level bindings
    /// published after our last sync; the global area is refreshed before
    /// hydrating either kind. (Publish happens-before submit, submit
    /// happens-before take, so this can never be stale.)
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

    /// Bring this scheduler's global area up to date with the runtime's
    /// shared (published) globals. The shared table holds frozen value
    /// words — pointers into the program-wide frozen area, or immediates —
    /// so syncing is a plain word copy per slot: no decode, no allocation.
    /// The `Acquire` load of `globals_version` pairs with the `Release`
    /// bump in `publish_global`, making the frozen
    /// segment contents visible before the words are read.
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

    /// Whether the whole program — every process on every scheduler — has
    /// finished (the runtime's live count reached zero).
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

    /// Mark this scheduler as parked/unparked so seed submitters know who to
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
        // A retire notify wakes an idle scheduler with no seed to run; its
        // registration and Arc clone must still be dropped here, or the
        // shared fd never closes.
        let _ = self.process_retired_listeners();
        Ok(())
    }

    // --- Lightweight processes (al/scheduler) --------------------

    /// `scheduler.spawn(f)`: start a new process running the closure `f`.
    ///
    /// The process is shipped as a `Send` seed that whichever scheduler is
    /// free will run; the first submit summons the runtime's worker threads
    /// (one scheduler per CPU core). When no scheduler is idle — including
    /// when scheduler 0 is the only one — the seed overflows to the shared
    /// injector and whichever scheduler frees up first picks it up at its
    /// next yield or idle scan.
    fn spawn_process(&mut self, f: Value) -> VmResult<()> {
        self.check_spawnable(&f)?;
        let seed = self.build_seed(&f);
        self.runtime.submit(seed);
        Ok(())
    }

    /// A spawned closure must be a nullary function; both are compiler
    /// invariants, so a violation is an internal error rather than a user one.
    fn check_spawnable(&self, f: &Value) -> VmResult<()> {
        let Some(cl) = f.as_closure() else {
            return Err(VmError::internal("spawn requires a function"));
        };
        if self.program.functions[cl.func_idx() as usize].arity != 0 {
            return Err(VmError::internal("spawned functions take no arguments"));
        }
        Ok(())
    }

    /// Spawn `f` pinned to this scheduler: the child runs on the core that
    /// spawned it, so any socket it captured stays in this scheduler's tables —
    /// no fd detach, no cross-core handoff. This is the shared-nothing half of
    /// the accept fan-out: an acceptor handles each connection on the core it
    /// accepted on, keeping the connection's buffers and fd local to one core.
    fn spawn_local(&mut self, f: Value) -> VmResult<()> {
        self.check_spawnable(&f)?;
        // Copy the closure graph into the child's own heap (processes share no
        // heap), but leave captured fds in place — parent and child run on the
        // same scheduler and reference the same per-scheduler socket tables.
        let (heap, root) = ProcHeap::spawn(&f);
        self.runtime.process_started();
        self.spawn_process_with_heap(heap, root);
        Ok(())
    }

    /// Spawn one copy of `f` pinned to every live scheduler — the fan-out that
    /// turns a single accept loop into one acceptor per core. All copies drain
    /// the same shared accept queue (a listener is one kernel socket), so the
    /// spread is a locality preference: connections get accepted on the core
    /// that will run them. The current scheduler runs its copy locally; every
    /// other live scheduler gets one pinned to its inbox.
    fn spawn_on_each(&mut self, f: Value) -> VmResult<()> {
        self.check_spawnable(&f)?;
        // Summon the worker threads so every scheduler slot is live before we
        // place a copy on it. A never-spawned worker is skipped — safe, since
        // any live acceptor drains the shared queue.
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

    /// Create a process whose initial heap is `heap` — the seeded
    /// heap a cross-scheduler spawn copied the closure graph into. The
    /// closure `f` points into that heap. Every caller has already run
    /// `check_spawnable` on the source closure (`ProcHeap::spawn` preserves the
    /// value kind), so `f` is a nullary closure by construction.
    #[allow(clippy::expect_used)]
    fn spawn_process_with_heap(&mut self, heap: ProcHeap, f: Value) {
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
        });
    }

    /// Copy a closure into a seed the child adopts wholesale: a fresh heap
    /// holding a deep copy of the closure's graph (sharing preserved via a
    /// `src → dst` map; no value is shared with this scheduler afterwards), so
    /// handing the seed to another OS thread is a plain move. `Binary` `Arc`
    /// backings are shared zero-copy — only the box is copied, never the bytes.
    /// Captured sockets transfer with it: connections move (this scheduler
    /// loses them); a listener id needs no transfer at all — the socket is
    /// shared in `Runtime.shared_listeners`, and the destination registers
    /// the same fd with its own poller on first accept.
    fn build_seed(&mut self, f: &Value) -> Seed {
        // The heap copy knows nothing about fd tables, so the captured
        // sockets are gathered by their own walk and moved/dup'd alongside
        // the values.
        let mut captured_sockets = Vec::new();
        migrate::for_each_socket(f, &mut |s| captured_sockets.push(s.id));

        // The child's initial heap: a fresh heap holding a deep copy of the
        // closure graph. `root` points into it, so `(heap, root)` is
        // self-contained and `Send` as a unit.
        let (heap, root) = ProcHeap::spawn(f);

        // Same fd transfer as donation: captured connections move to the
        // child; captured listeners stay put (the child binds its own
        // reuseport socket from the shared address on first accept).
        let connections = self.detach_socket_ids(captured_sockets);

        Seed {
            heap,
            root,
            connections,
        }
    }

    /// Build a runnable process on this scheduler from a seed: adopt its
    /// sockets, adopt its heap (the spawn-side copy of the closure graph) as
    /// the child's initial heap, and queue it. Top-level bindings come
    /// from the global area (synced in `admit`, the intake gate every arrival
    /// path — inbox drain, overflow pickup, steal — funnels through), so the
    /// seed itself carries none.
    fn hydrate_seed(&mut self, seed: Seed) {
        self.adopt_connections(seed.connections);
        self.spawn_process_with_heap(seed.heap, seed.root);
    }
}

/// Entry point for worker scheduler threads (indices 1..N), spawned by
/// `Runtime::ensure_workers` on the first submit. Each worker runs a VM of
/// its own over the shared runtime (its poller was created alongside the
/// thread; the poller's waker sits in the runtime's waker table for
/// `notify`),
/// acquiring work like any scheduler — seeds direct-handed to its inbox by
/// `submit` and migrants donated by peers first, falling back to the shared
/// overflow injector and to stealing undelivered work from peers' inboxes —
/// until the whole program finishes.
fn worker_main(runtime: Arc<Runtime>, index: usize, poll: mio::Poll) {
    let mut vm = vm_for_runtime(runtime, index, poll);

    // Workers have no main process; scheduler_loop starts by acquiring work.
    if let Err(e) = vm.scheduler_loop() {
        // A VM error indicates a compiler bug, not a user error: program
        // state is unreliable and there is no path to hand the error to
        // scheduler 0, so it is fatal to the whole process — even a
        // long-lived embedder (the REPL) goes down with it.
        eprintln!("al: scheduler {index} failed: {e}");
        std::process::exit(1);
    }
}

/// Construct a Nil enum value in `h` without a `VM`. Retained for tests
/// that build values outside an interpreter; hot paths go through
/// `VM::make_nil`.
#[cfg(test)]
fn nil_value(h: &mut ProcHeap, nil_id: al_core::TypeId) -> Value {
    Value::enum_with_names_in(h, nil_id, 0, "Nil", "Nil", &[], &[])
}

/// A VM over a minimal halt-only program ("main", arity 0). For tests that
/// drive the VM's side tables directly — timer heap, parked store, socket
/// tables, run queue — and never execute the program.
#[cfg(test)]
fn halt_test_vm() -> VM {
    new_vm(Program {
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
        frozen: Arc::new(al_core::frozen::FrozenArea::new()),
    })
    .expect("test VM construction must succeed")
}
