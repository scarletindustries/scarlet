//! The virtual machine: lightweight processes and the schedulers that run
//! them.
//!
//! This module is the runtime's front door. It owns the [`VM`] — one per
//! scheduler thread — whose dispatch loop (`execute_slice`) runs thousands
//! of cooperatively preempted processes over a handful of OS threads
//! without ever letting one block another. The memory those processes run
//! on is [`al_core::heap`]'s: each process owns an arena, and this module
//! is the opcode-side counterpart of that design — it decides when to run,
//! park, move, and collect, while the heap decides how to allocate and
//! evacuate. The scheduling shape: reduction budgets, run
//! queues, an injector for spawns, work stealing, and donation-based
//! migration.
//!
//! # The process lifecycle in one diagram
//!
//! ```text
//!   spawn(f) — the closure graph is copied into a fresh mini-arena
//!      │
//!      ▼
//!   SEED ──────── submit ───────> injector / a scheduler's inbox
//!      │  take_inbound / steal_inbound: the receiver adopts the arena wholesale
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
//!   DONE       the result is its top-of-stack; the arena drops with
//!              the process (main's is stashed until run() returns it)
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
//!    syscall [`IO_REDUCTION_COST`], collection work one per
//!    [`gc::GC_WORDS_PER_REDUCTION`] words copied — so accept loops and
//!    GC-heavy processes are preempted like everything else, and no
//!    process rides free.
//! 3. **Blocking parks the process, never the thread.** An op that would
//!    block returns [`Step::Parked`] carrying a [`Wait`] — socket
//!    interests, a deadline, and/or a blocking-pool job. The scheduler
//!    shelves the process, arms the OS poller and the timer heap, and runs
//!    something else; the wake re-runs the instruction or completes the
//!    pending connect ([`poll::WakeAction`]).
//! 4. **Work moves between schedulers as owned memory.** The first spawn
//!    boots one scheduler per core ([`sched`]); each spawn copies its
//!    closure graph into a fresh mini-arena and submits the result as a
//!    [`sched::Seed`]; load balancing donates whole queued processes the
//!    same way ([`migrate`]). Plain moves, in both cases — the receiver
//!    adopts the arriving memory as-is.
//! 5. **The rooting rule.** Every allocating opcode computes its
//!    worst-case need from the [`cost`] tables BEFORE popping operands —
//!    while they still sit on the VM stack, where the collector can see
//!    and rewrite them — calls `ensure(need)`, the one safepoint
//!    ([`gc`]), and only then pops and allocates, collection-free.
//! 6. **A value and its arena travel together.** A process owns every
//!    heap value it can reach, which is what makes seeds, migrants, and
//!    suspended processes `Send` by construction — and why main's result
//!    is stashed as a `(value, heap)` pair until `run()`'s caller is done
//!    with it. No value is ever handed anywhere without the memory it
//!    points into.
//!
//! # Reading order
//!
//! | file              | the one thing it does                              |
//! |-------------------|----------------------------------------------------|
//! | this file         | the [`VM`] and the scheduling story: `run`,        |
//! |                   | `scheduler_loop`, `acquire_work`, suspend/resume,  |
//! |                   | donation policy, spawn/seed glue                   |
//! | [`exec`]          | the dispatch loop (`execute_slice`): inline arms,  |
//! |                   | family-handler routing, stack + constructor helpers|
//! | [`cost`]          | the opcode side of the rooting rule: worst-case    |
//! |                   | word budgets for everything the VM allocates       |
//! | [`gc`]            | the collection side: `ensure`/`collect`, the root  |
//! |                   | set, GC reduction charging, budget peeks           |
//! | [`collections`]   | array/tuple/range/field-access opcodes             |
//! | [`text`]          | string/binary builtins and HTTP-scanner opcodes    |
//! | [`io`]            | file/socket/DNS/sleep/spawn opcodes — everything   |
//! |                   | that can park — and the per-scheduler fd tables    |
//! | [`poll`]          | parking and wake-up: [`Wait`], the OS poller, the  |
//! |                   | timer heap, blocking-pool completion delivery      |
//! | [`sched`]         | the multi-core runtime: scheduler threads, the     |
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
//! `spawn(handler)` in a server: the opcode copies the handler closure's
//! graph into a mini-arena sized to fit and submits the pair as a seed,
//! waking an idle scheduler. That scheduler adopts the arena as the
//! child's young space, frames the closure, and resumes it; the child's
//! `accept` would block, so it parks — interests armed in the poller, the
//! suspended [`Process`] shelved under a wait id — and the scheduler runs
//! its next process. A connection arrives: the poller wakes the process,
//! the accept re-runs and succeeds. Later it exhausts a reduction budget
//! mid-computation and yields to the back of the run queue; a peer
//! scheduler goes idle, and at this scheduler's next yield it claims
//! that peer and donates the queued process — arena, stack, frames,
//! socket fds — which moves over in one piece and keeps running there.
//! Its final `Ret` leaves the result on top of the stack: the process
//! is done, its arena is dropped in one shot, and the runtime's live
//! count falls. When the count reaches zero, `acquire_work` returns
//! false on every scheduler, the workers exit, and scheduler 0 hands
//! main's stashed result back to `run()`'s caller.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Instant;

use al_core::bytecode::{BinaryRef, Program, Value};
#[cfg(test)]
use al_core::bytecode::{Function, Op, op};
use al_core::frozen::FrozenBuilder;
use al_core::heap::ProcHeap;

mod binary;
mod collections;
mod cost;
mod exec;
mod freeze;
mod gc;
mod http;
mod inspect;
mod io;
mod migrate;
mod poll;
mod sched;
mod templates;
#[cfg(test)]
mod tests;
mod text;

use gc::GcDebt;
pub use inspect::inspect;
use inspect::{f64_str, value_type_name};
use migrate::Migrant;
use poll::Wait;
use sched::{Inbound, Runtime, Seed};
use templates::{EnumTemplate, PreludeTemplates, enum_template};
use text::{int_to_ascii, parse_int_ascii};

type VmResult<T> = Result<T, String>;

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
    heap: ProcHeap,
    stack: Vec<Value>,
    frames: Vec<CallFrame>,
    is_main: bool,
}

// A migrant crosses an OS-thread boundary as a plain move; this must hold by
// construction (owned arena + plain-data frames), never via unsafe impls.
const _: () = {
    const fn assert_send<T: Send>() {}
    assert_send::<Process>();
};

/// Borrow the string contents of a value `pop_str` already type-checked.
#[inline]
fn str_ref(v: &Value) -> &str {
    v.as_str().unwrap_or_default()
}

/// Borrow the binary view of a value `pop_binary` already type-checked.
#[inline]
#[allow(clippy::unwrap_used)]
fn bin_ref(v: &Value) -> BinaryRef<'_> {
    v.as_binary().unwrap()
}

pub struct VM {
    program: Program,
    templates: PreludeTemplates,
    /// Builder over the program's frozen area, kept for runtime freezing of
    /// stdlib templates (`stdlib_template`) built on demand (error values).
    frozen: FrozenBuilder,
    /// Frozen stdlib enum templates memoized by `VariantTemplate` identity.
    template_cache: HashMap<usize, EnumTemplate>,
    /// The *running* process's arena. Swapped in/out with `stack`/`frames` on
    /// every context switch (`suspend_current`/`resume`); between processes it
    /// is the empty placeholder (`ProcHeap::default()`), which owns nothing.
    heap: ProcHeap,
    stack: Vec<Value>,
    frames: Vec<CallFrame>,
    /// Reductions owed for collection work. The slot is scheduler-wide, but
    /// the debt never crosses processes: `execute_slice` drops any leftover
    /// at slice entry. See `VM::collect`/`VM::take_gc_reds`.
    gc: GcDebt,
    /// Memoized field-label tuples, one entry per non-prelude ctor site,
    /// keyed by the address of that site's pooled labels-array constant. The
    /// labels are statically constant per constructor, so each site freezes
    /// its labels Tuple once; every later construction stores one reference
    /// word.
    label_cache: HashMap<usize, Value>,
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
    /// Monotonically increasing id handed to each park; the key into `parked`
    /// and the identity (and tie-breaker) recorded in `timer_heap`.
    next_wait_id: u64,
    /// Lazy-deletion min-heap of `(deadline, wait id)`. A park with a deadline
    /// pushes one entry and never eagerly removes it: when the park instead
    /// wakes early on I/O, the entry is discarded on pop once its id is gone
    /// from `parked` (or its live deadline no longer matches). This keeps the
    /// nearest deadline an O(log n) peek instead of an O(n) scan of `parked`.
    timer_heap: BinaryHeap<Reverse<(Instant, u64)>>,
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
        heap: ProcHeap::default(),
        stack: Vec::new(),
        frames: Vec::new(),
        gc: GcDebt::default(),
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
        next_wait_id: 0,
        timer_heap: BinaryHeap::new(),
        poller: None,
        runtime: None,
        scheduler_index: 0,
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
        self.ensure(cost::closure(0));
        let entry_closure = Value::closure_in(&mut self.heap, entry, &[]);

        self.frames.push(CallFrame {
            func_idx: entry,
            code_start,
            ip: 0,
            base_slot: 0,
            captures: entry_closure,
        });

        for _ in 0..locals {
            self.stack.push(Value::small_int(0));
        }

        self.current_is_main = true;
        let result = self.scheduler_loop();

        // Multi-core shutdown: by the time scheduler 0's loop returns Ok, the
        // global live count is zero, so workers are exiting; join them. On
        // error the program is dying — process exit reaps the workers.
        if result.is_ok()
            && let Some(rt) = &self.runtime
        {
            rt.shutdown_blocking();
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
                        self.heap = ProcHeap::default();
                    }
                    self.stack.clear();
                    self.frames.clear();
                    self.current_is_main = false;
                    if let Some(rt) = &self.runtime {
                        rt.process_finished();
                    }
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
                    if !self.parked.is_empty() {
                        self.poll_parked(false)?;
                    }
                    self.take_directed()?;
                    let peer_idle = self.others_idle();
                    if !peer_idle && self.run_queue.len() < YIELD_PICKUP_QUEUE_LIMIT {
                        self.take_overflow_seed()?;
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
                Step::Parked(mut wait) => {
                    // Allocate this park's id, record its deadline (if any) in
                    // the timer heap, arm its I/O interests, then stash the
                    // suspended process under the id.
                    let id = self.next_wait_id;
                    self.next_wait_id += 1;
                    if let Some(deadline) = wait.deadline {
                        self.timer_heap.push(Reverse((deadline, id)));
                    }
                    self.register_wait(&wait)?;
                    let outgoing = self.suspend_current();
                    // A completion park hands its blocking op to the pool now,
                    // keyed by this wait id so the worker's result resumes it.
                    if let Some(op) = wait.offload.take()
                        && let Some(rt) = &self.runtime
                    {
                        rt.offload(self.scheduler_index, id, op);
                    }
                    self.parked.insert(id, (wait, outgoing));
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
        let Some(rt) = self.runtime.clone() else {
            return;
        };
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
        let (peer, claimed) = match busy_peer {
            Some(peer) => (peer, false),
            // The idle peer seen by the probe may have been claimed or woken
            // meanwhile; the next yield retries.
            None => match rt.claim_idle_peer() {
                Some(peer) => (peer, true),
                None => return,
            },
        };
        let Some(victim) = self.run_queue.pop_back() else {
            // Unreachable (the queue is local and non-empty), but a claimed
            // peer must still be woken so it can re-park.
            if claimed {
                rt.abort_donation(peer);
            }
            return;
        };
        match self.detach_fds(&victim) {
            Some((listeners, connections)) => rt.donate(
                peer,
                Migrant {
                    process: victim,
                    listeners,
                    connections,
                },
            ),
            None => {
                // Fd detach aborted; `detach_fds` restored the donor's
                // socket tables, so keep the process local and wake the
                // claimed peer.
                self.run_queue.push_back(victim);
                if claimed {
                    rt.abort_donation(peer);
                }
            }
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
            if self.take_inbound()? {
                continue;
            }
            if self.steal_inbound()? {
                continue;
            }

            // 3. Local parked I/O / timers: wait for them, but stay wakeable
            //    by other schedulers (seed submissions, program end).
            if !self.parked.is_empty() {
                self.set_parked_flag(true);
                // Re-check for seeds after publishing the flag: a submitter
                // who scanned before the flag was visible may have pushed to
                // the overflow queue (or straight into our inbox) expecting
                // someone to pick it up.
                if self.take_inbound()? {
                    self.set_parked_flag(false);
                    continue;
                }
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
            if self.take_inbound()? {
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
    fn take_inbound(&mut self) -> VmResult<bool> {
        let batch = match &self.runtime {
            Some(rt) => rt.take_inbound(self.scheduler_index),
            None => return Ok(false),
        };
        self.admit(batch)
    }

    /// Take only the work directed at this scheduler (its inbox: donated
    /// migrants and direct-handed seeds) into the local run queue. Called at
    /// every yield — directed work has a chosen destination and must not
    /// wait, unlike overflow seeds. Returns whether any arrived.
    fn take_directed(&mut self) -> VmResult<bool> {
        let batch = match &self.runtime {
            Some(rt) => rt.take_directed(self.scheduler_index),
            None => return Ok(false),
        };
        self.admit(batch)
    }

    /// Pick up undirected overflow seeds (the shared injector) into the
    /// local run queue. Returns whether any arrived.
    fn take_overflow_seed(&mut self) -> VmResult<bool> {
        let batch: Vec<Inbound> = match &self.runtime {
            Some(rt) => rt.take_overflow().into_iter().map(Inbound::Seed).collect(),
            None => return Ok(false),
        };
        self.admit(batch)
    }

    /// Admit a batch of inbound work into the local run queue. Returns
    /// whether anything was admitted.
    ///
    /// Inbound work — seed or migrant — may reference top-level bindings
    /// published after our last sync; the global area is refreshed before
    /// hydrating either kind. (Publish happens-before submit, submit
    /// happens-before take, so this can never be stale.)
    fn admit(&mut self, batch: Vec<Inbound>) -> VmResult<bool> {
        if batch.is_empty() {
            return Ok(false);
        }
        self.sync_globals();
        for inbound in batch {
            match inbound {
                Inbound::Seed(seed) => self.hydrate_seed(seed)?,
                Inbound::Migrant(m) => self.adopt_migrant(m),
            }
        }
        Ok(true)
    }

    /// Steal one undelivered unit of inbound work from a peer scheduler's
    /// inbox. Only called when this scheduler has nothing local to run.
    fn steal_inbound(&mut self) -> VmResult<bool> {
        let Some(rt) = &self.runtime else {
            return Ok(false);
        };
        let Some(inbound) = rt.steal_inbound(self.scheduler_index) else {
            return Ok(false);
        };
        self.admit(vec![inbound])
    }

    /// Bring this scheduler's global area up to date with the runtime's
    /// shared (published) globals. The shared table holds frozen value
    /// words — pointers into the program-wide frozen area, or immediates —
    /// so syncing is a plain word copy per slot: no decode, no allocation
    ///. The `Acquire` load of `globals_version`
    /// pairs with the `Release` bump in `publish_global`, making the frozen
    /// segment contents visible before the words are read.
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

    /// Whether the whole program has finished (multi-core mode only).
    fn runtime_finished(&self) -> bool {
        match &self.runtime {
            Some(rt) => rt.is_finished(),
            None => false,
        }
    }

    /// Report this scheduler's runnable count (running process + queue) to
    /// the shared load board peers read when picking a donation target.
    fn publish_load(&self) {
        if let Some(rt) = &self.runtime {
            let load = usize::from(!self.frames.is_empty()) + self.run_queue.len();
            rt.publish_load(self.scheduler_index, load);
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

    // --- Lightweight processes (al/experiments/scheduler) --------------------

    /// `scheduler.spawn(f)`: start a new process running the closure `f`.
    ///
    /// The first spawn boots the multi-core runtime (one scheduler per CPU
    /// core). With the runtime up, the process is shipped as a `Send` seed
    /// that whichever scheduler is free will run; otherwise it is created
    /// directly on this scheduler.
    fn spawn_process(&mut self, f: Value) -> VmResult<()> {
        match f.as_closure() {
            Some(cl) => {
                if self.program.functions[cl.func_idx() as usize].arity != 0 {
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
                    let program = Arc::new(self.program.clone());
                    match sched::boot(program, count, Arc::clone(poller)) {
                        Ok(rt) => {
                            // Publish every global written before the runtime
                            // existed so worker schedulers can load them. The
                            // table already holds frozen words (StoreLocal
                            // freezes at store time), so no copying happens
                            // here.
                            for (slot, value) in self.globals.iter().enumerate() {
                                rt.publish_global(slot, freeze::FrozenValue::new(*value));
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
    /// Per-process heaps hold even without the multi-core runtime: the child
    /// gets its own mini-arena (a `copy_graph` copy of the closure graph,
    /// exactly like a cross-scheduler seed), so its allocation, GC, and
    /// lifetime stay independent of the spawner's.
    fn spawn_process_local(&mut self, f: Value) -> VmResult<()> {
        let (heap, root) = self.heap.spawn_copy(&f);
        self.spawn_process_with_heap(heap, root)
    }

    /// Create a process whose initial young space is `heap` — the seeded
    /// mini-arena a cross-scheduler spawn copied the closure graph into. The
    /// closure `f` points into that heap.
    fn spawn_process_with_heap(&mut self, heap: ProcHeap, f: Value) -> VmResult<()> {
        let Some(cl) = f.as_closure() else {
            return Err("spawn requires a function. This is likely a compiler bug.".to_string());
        };
        let func = &self.program.functions[cl.func_idx() as usize];
        let (func_idx, code_start, locals) = (cl.func_idx(), func.code_start, func.locals);

        let mut stack = Vec::with_capacity(locals as usize + 8);
        for _ in 0..locals {
            stack.push(Value::small_int(0));
        }

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
        Ok(())
    }

    /// Copy a closure into a seed the child adopts wholesale: a fresh
    /// mini-arena sized to the closure's graph, filled by `copy_graph`
    /// spawn-side (sharing preserved via forwarding pointers; no value is
    /// shared with this scheduler afterwards), so handing the seed to another
    /// OS thread is a plain move. `Binary` `Arc` backings are shared
    /// zero-copy — only the arena box is copied, never the bytes. Captured
    /// sockets transfer with it: connections move (this scheduler loses
    /// them), listeners are dup'd so both sides keep accepting.
    fn build_seed(&mut self, f: &Value) -> VmResult<Seed> {
        // The heap copy knows nothing about fd tables, so the captured
        // sockets are gathered by their own walk and moved/dup'd alongside
        // the arena.
        let mut captured_sockets = Vec::new();
        migrate::for_each_socket(f, &mut |s| captured_sockets.push(s.id));

        // The child's initial young space: a fresh arena sized to the
        // closure graph, with the graph evacuated into it. `root` points
        // into that arena, so `(heap, root)` is self-contained and `Send`
        // as a unit.
        let (heap, root) = self.heap.spawn_copy(f);

        // Same fd transfer as donation (`detach_socket_ids` rolls back on
        // failure), so an undupable listener fails the spawn cleanly instead
        // of producing a child that silently cannot accept.
        let (listeners, connections) = self
            .detach_socket_ids(captured_sockets, "spawn")
            .ok_or_else(|| "spawn: cannot share captured listener with child".to_string())?;

        Ok(Seed {
            heap,
            root,
            listeners,
            connections,
        })
    }

    /// Build a runnable process on this scheduler from a seed: adopt its
    /// sockets, adopt its heap (the spawn-side copy of the closure graph) as
    /// the child's initial young space, and queue it. Top-level bindings come
    /// from the global area (synced in `take_inbound`), so the seed itself
    /// carries none.
    fn hydrate_seed(&mut self, seed: Seed) -> VmResult<()> {
        for (id, l) in seed.listeners {
            // The destination may already hold this listener id (adopted via
            // an earlier seed or `ensure_listener`). The incoming dup is the
            // same underlying socket; replacing the entry would drop — and
            // close — an fd this scheduler's poller may have registered. Keep
            // the existing entry and let the dup drop instead.
            self.tcp_listeners.entry(id).or_insert(l);
        }
        for (id, c) in seed.connections {
            self.tcp_connections.insert(id, c);
        }

        self.spawn_process_with_heap(seed.heap, seed.root)
    }

    /// Pre-side-effect donor guard for process migration: may `victim`'s
    /// connection fds safely leave this scheduler?
    ///
    /// Donating a process moves every connection socket it references out of
    /// this scheduler's `tcp_connections` table. That is only safe while the
    /// fd is quiescent here, so donation must abort if any referenced
    /// connection id is
    ///
    /// - in `pending_connects`: a non-blocking connect on that id is in
    ///   flight on this scheduler and its completion handler will look the
    ///   socket up in this table; or
    /// - armed in a parked sibling's `Wait.interests`: the sleeper's poller
    ///   registration belongs to this scheduler, and moving the fd away would
    ///   silently break the sleeper — its wake/re-arm path resolves the id
    ///   through this scheduler's tables.
    ///
    /// This check is read-only and runs strictly before any side effect of
    /// donation (peer claim, fd detach, socket-table mutation), so an
    /// abort here costs nothing. Listener ids never need guarding: listeners
    /// are dup'd on transfer, the donor keeps its entry and any registration.
    fn can_donate_fds(&self, victim: &Process) -> bool {
        let mut ids = Vec::new();
        migrate::for_each_process_socket(victim, &mut |s| {
            if !s.is_listener {
                ids.push(s.id);
            }
        });
        if ids.is_empty() {
            return true;
        }
        ids.sort_unstable();
        ids.dedup();

        if ids.iter().any(|id| self.pending_connects.contains_key(id)) {
            return false;
        }
        // The victim itself is runnable (in run_queue, not parked), so its own
        // interests cannot appear here — any hit is a sibling's armed fd.
        !self.parked.values().any(|(wait, _)| {
            wait.interests
                .iter()
                .any(|(fd, _)| ids.binary_search(fd).is_ok())
        })
    }
}

/// Entry point for worker scheduler threads (indices 1..N), started by
/// `sched::boot`. Each worker runs a VM of its own against a private clone
/// of the shared program (`Program` is `Send`; its constants are frozen
/// words shared across schedulers, never re-built), pulling seeds from the
/// runtime's injector until the whole program finishes.
fn worker_main(runtime: Arc<Runtime>, index: usize) {
    let mut vm = new_vm((*runtime.program).clone());
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

/// Construct a Nil enum value in `h` without a `VM`. Retained for tests
/// that build values outside an interpreter; hot paths go through
/// `VM::make_nil`.
#[cfg(test)]
fn nil_value(h: &mut ProcHeap, nil_id: i32) -> Value {
    Value::enum_with_names_in(h, nil_id, "Nil", "Nil", &[], &[])
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
}

#[inline]
fn range_len(s: i64, e: i64) -> i64 {
    e.saturating_sub(s).max(0)
}
