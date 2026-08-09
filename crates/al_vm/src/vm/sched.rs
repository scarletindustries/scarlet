//! The multi-core scheduler runtime.
//!
//! A [`Runtime`] is built before any code runs, with only scheduler 0's poller.
//! Worker threads, one per core beyond the main thread, are summoned by the
//! first `submit` ([`Runtime::ensure_workers`]).
//!
//! A spawned process starts as a [`Seed`]: an owned mini-heap holding a deep
//! copy of the closure graph plus the root pointing into it. The receiving
//! scheduler adopts that heap whole; nothing is rebuilt. A process's heap is
//! only ever touched by the scheduler currently running it.
//!
//! Idle schedulers park inside their poller and are woken via
//! [`mio::Waker::wake`].

use std::collections::{HashMap, VecDeque};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, Once, OnceLock};

use crate::bytecode::{Program, Value};
use crate::heap::ProcHeap;

use super::freeze::FrozenValue;
use super::lock;
use super::migrate::{DetachedFds, Migrant};

/// How many seeds a scheduler takes from the injector per visit. One at a time
/// maximizes spread: k seeds across k idle schedulers is one each.
pub(super) const SEED_BATCH: usize = 1;

/// A spawned-but-not-yet-started process, the unit of cross-scheduler work
/// distribution. `heap` is owned memory and `root` points only into it or the
/// frozen area, so handing a seed to another scheduler is a plain move.
pub(super) struct Seed {
    /// The child's process id, minted early so the detached connections can be
    /// stamped with their new controlling process before they travel.
    pub pid: u64,
    /// The spawned closure, as a pointer into the child heap.
    pub root: Value,
    /// Connections the closure captured; the spawner loses them. Listeners do
    /// not travel — the socket is shared program-wide and the destination
    /// registers the same fd on first accept.
    pub connections: DetachedFds,
    /// The child's allocator handle. Zero-sized: allocation goes to mimalloc's
    /// per-thread default heap and `mi_free` works from any thread.
    pub heap: ProcHeap,
}

// A seed crosses scheduler threads, so if it ever regains a non-`Send` field
// spawning must fail to build.
const _: () = crate::assert_send::<Seed>();

/// Work delivered to a scheduler's private inbox. Migrants are always
/// direct-handed to a chosen peer, never queued to the shared injector, which
/// stays `Seed`-only.
pub(super) enum Inbound {
    Seed(Seed),
    Migrant(Migrant),
}

/// A blocking operation handed to the [`BlockingPool`] so it runs on a worker
/// thread instead of stalling a scheduler.
#[derive(Debug)]
pub(super) enum BlockingOp {
    ReadFile(String),
    WriteFile(String, Vec<u8>),
    ResolveDns(String),
}

/// The result of a [`BlockingOp`], delivered back to the originating scheduler.
/// Only raw data crosses the pool boundary, never a `Value`, so no worker
/// thread touches a process heap.
pub(super) enum BlockingResult {
    ReadFile {
        path: String,
        result: std::io::Result<Vec<u8>>,
    },
    WriteFile {
        path: String,
        result: std::io::Result<()>,
    },
    ResolveDns {
        result: std::io::Result<std::net::IpAddr>,
    },
}

/// A finished blocking job routed back to the scheduler that issued it.
/// `job_id` is the parked process's wait id.
pub(super) struct Completion {
    pub job_id: u64,
    pub result: BlockingResult,
}

/// The error completion for a job the pool could not run at all.
fn failed_blocking_result(op: BlockingOp) -> BlockingResult {
    let err = || std::io::Error::other("blocking pool unavailable");
    match op {
        BlockingOp::ReadFile(path) => BlockingResult::ReadFile {
            path,
            result: Err(err()),
        },
        BlockingOp::WriteFile(path, _) => BlockingResult::WriteFile {
            path,
            result: Err(err()),
        },
        BlockingOp::ResolveDns(_) => BlockingResult::ResolveDns { result: Err(err()) },
    }
}

/// An elastic pool of OS threads for blocking syscalls that must never run on a
/// scheduler thread. A burst grows the pool up to `max_total`; as work drains,
/// idle workers above `max_warm` exit.
struct BlockingPool {
    /// Pending jobs: `(job_id, origin scheduler, op)`.
    queue: Mutex<VecDeque<(u64, usize, BlockingOp)>>,
    cond: Condvar,
    /// Live worker threads, busy plus parked.
    total: AtomicUsize,
    idle: AtomicUsize,
    max_total: usize,
    /// Idle workers kept warm; extras exit on going idle.
    max_warm: usize,
    shutdown: AtomicBool,
}

impl BlockingPool {
    fn new() -> Self {
        let max_total = std::env::var("AL_BLOCKING_THREADS")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|&n| (1..=4096).contains(&n))
            .unwrap_or(512);
        BlockingPool {
            queue: Mutex::new(VecDeque::new()),
            cond: Condvar::new(),
            total: AtomicUsize::new(0),
            idle: AtomicUsize::new(0),
            max_total,
            max_warm: 8,
            shutdown: AtomicBool::new(false),
        }
    }
}

/// Per-scheduler shared state: everything other schedulers and the blocking
/// pool touch to place work on, wake, or read the load of one scheduler. One
/// struct rather than parallel `Vec`s that could end up different lengths.
pub(super) struct SchedSlot {
    /// Work inbox. `submit` hands a seed straight into a chosen idle
    /// scheduler's inbox, so placement is decided at submit time instead of
    /// raced through a shared queue. The handoff is a preference, not
    /// ownership: an idle peer with nothing to run may steal from here
    /// ([`Runtime::steal_inbound`]) and the woken owner just re-parks.
    pub(super) inbox: Mutex<VecDeque<Inbound>>,
    /// Listener ids retired by `net.close` on some scheduler, drained by this
    /// slot's owner. See [`Runtime::retire_listener`].
    pub(super) retired_listeners: Mutex<Vec<i32>>,
    /// Fast-path gate for `retired_listeners`, checked every poll cycle so the
    /// owner need not take the lock.
    pub(super) retired_pending: AtomicBool,
    /// Seeds that run on exactly this scheduler and are never stolen. Used by
    /// the accept fan-out to spread acceptors one per core. A listener is one
    /// shared kernel socket, so this is locality, not correctness.
    pub(super) pinned: Mutex<VecDeque<Seed>>,
    /// Poller waker, registered under [`super::poll::WAKER_TOKEN`]. Filled only
    /// once a thread exists, so an empty slot means a worker that never
    /// spawned. That doubles as the liveness check for donation targeting,
    /// which is the one placement path not gated on `parked`.
    pub(super) waker: OnceLock<Arc<mio::Waker>>,
    /// Whether this scheduler is currently parked, idle or on I/O.
    ///
    /// A raised flag is a promise: whoever wins the CAS in `claim_idle_peer`
    /// may push into this inbox and notify, and a live thread will take the
    /// work. What keeps the promise:
    ///
    /// - Flags start down, before any worker thread exists.
    /// - A worker's flag is first raised inside `ensure_workers`' one-shot
    ///   section, either by `ensure_workers` after the waker slot is filled or
    ///   by the new worker parking itself. No claimant can run before
    ///   `call_once` returns, by which point every spawned worker's waker slot
    ///   is filled. Seeding workers "idle" makes the first wave of submits hand
    ///   seeds straight to them.
    /// - Thereafter each scheduler owns its flag: raised before parking,
    ///   lowered on waking. A claimant lowers it by CAS, hiding the sleeper
    ///   from other placements, and must end in a notify (see [`Claim`]).
    /// - A worker whose thread failed to spawn keeps its flag down forever, so
    ///   no flag-gated path targets it; its work drains elsewhere.
    pub(super) parked: AtomicBool,
    /// Published load: runnable processes last reported, plus one per directed
    /// inbound still in flight. Donation reads these to level queue lengths
    /// while every scheduler is busy.
    ///
    /// Advisory. Each scheduler overwrites its own slot after every scheduling
    /// step (past the inbox drain, so in-flight bumps are folded in) and again
    /// when it parks empty. That park-time republish is what clears a bump for
    /// work stolen out of a sleeping owner's inbox. A stale read mistunes one
    /// donation decision at worst, so relaxed ordering is enough.
    pub(super) run_len: AtomicUsize,
    /// A blocking worker pushes a finished job here and notifies this
    /// scheduler's poller.
    pub(super) completions: Mutex<VecDeque<Completion>>,
}

impl SchedSlot {
    fn new(is_main: bool) -> Self {
        SchedSlot {
            inbox: Mutex::new(VecDeque::new()),
            retired_listeners: Mutex::new(Vec::new()),
            retired_pending: AtomicBool::new(false),
            pinned: Mutex::new(VecDeque::new()),
            waker: OnceLock::new(),
            // Raised only once the worker's thread exists.
            parked: AtomicBool::new(false),
            // Scheduler 0 is running the main process; workers start empty.
            run_len: AtomicUsize::new(usize::from(is_main)),
            completions: Mutex::new(VecDeque::new()),
        }
    }
}

/// The exclusive right to hand work into slot `i`'s inbox and wake it. The
/// claimed scheduler sleeps with its flag down, invisible to every other
/// submitter and donor, so a claim MUST end in a notify: [`Runtime::hand`] or
/// [`Runtime::release`]. Dropping one leaves a scheduler asleep and unwakeable.
#[must_use = "a claimed scheduler must be handed work or released"]
pub(super) struct Claim(usize);

/// State shared by every scheduler in the program.
pub(super) struct Runtime {
    /// The shared program. Each worker runs a private clone; constants are
    /// frozen words pointing into the shared `program.frozen`.
    pub program: Arc<Program>,
    /// Entrypoint path followed by every argument after it. Read by `Op::Argv`.
    pub argv: Vec<String>,
    /// Top-level bindings as frozen value words, published when main writes
    /// them. Workers copy them into their own table, gated by
    /// `globals_version`.
    pub globals: Mutex<Vec<Option<FrozenValue>>>,
    /// Bumped on every publish so schedulers can skip syncing.
    pub globals_version: AtomicU64,
    /// Every live listener, keyed by socket id.
    ///
    /// One kernel socket per `Server`, program-wide. Storing an address here
    /// instead would make `ensure_listener` bind a second `SO_REUSEPORT`
    /// socket on whichever scheduler accepted, and a connection routed to a
    /// socket nobody accepts on sits in its backlog forever. On macOS
    /// `SO_REUSEPORT` does not balance at all: the last binder takes
    /// everything.
    pub shared_listeners: Mutex<HashMap<i32, Arc<TcpListener>>>,
    next_pid: AtomicU64,
    /// Per-scheduler shared slots, indexed by scheduler id.
    pub(super) slots: Vec<SchedSlot>,
    /// Overflow queue: seeds submitted while every scheduler was busy.
    pub injector: Mutex<VecDeque<Seed>>,
    /// Rotates the start of every idle-scheduler scan so consecutive placements
    /// spread across schedulers.
    pub submit_cursor: AtomicUsize,
    /// Live processes across all schedulers, counting undelivered seeds and the
    /// main process. The program is over at zero. Private so all accounting
    /// goes through [`Runtime::process_started`] / [`Runtime::process_finished`].
    live: AtomicUsize,
    /// Worker threads, joined by scheduler 0 at shutdown.
    pub workers: Mutex<Vec<std::thread::JoinHandle<()>>>,
    workers_started: Once,
    blocking: BlockingPool,
    /// Raised when any scheduler hits a `VmError`: every other scheduler
    /// drains out through `acquire_work`'s check instead of waiting for
    /// `live` to reach zero (which an errored run never manages).
    faulted: AtomicBool,
    /// The first fault's origin and error, surfaced by scheduler 0 after the
    /// workers are joined.
    fault: Mutex<Option<(usize, super::VmError)>>,
}

impl Runtime {
    /// Build the runtime for `count` schedulers. No OS threads start here;
    /// worker pollers are created when their threads spawn.
    ///
    /// The only failure is scheduler 0's poller. Its `mio::Poll` is returned
    /// rather than stored, since a poller is owned by its scheduler thread;
    /// only the waker goes into the shared slot table.
    pub fn new(
        program: Arc<Program>,
        argv: Vec<String>,
        count: usize,
    ) -> std::io::Result<(Arc<Runtime>, mio::Poll)> {
        let poll = mio::Poll::new()?;
        let waker = mio::Waker::new(poll.registry(), super::poll::WAKER_TOKEN)?;
        let slots: Vec<SchedSlot> = (0..count).map(|i| SchedSlot::new(i == 0)).collect();
        let _ = slots[0].waker.set(Arc::new(waker));
        let runtime = Arc::new(Runtime {
            program,
            argv,
            globals: Mutex::new(Vec::new()),
            globals_version: AtomicU64::new(0),
            shared_listeners: Mutex::new(HashMap::new()),
            next_pid: AtomicU64::new(1),
            slots,
            injector: Mutex::new(VecDeque::new()),
            submit_cursor: AtomicUsize::new(0),
            // The main process is live.
            live: AtomicUsize::new(1),
            blocking: BlockingPool::new(),
            workers: Mutex::new(Vec::new()),
            workers_started: Once::new(),
            faulted: AtomicBool::new(false),
            fault: Mutex::new(None),
        });
        Ok((runtime, poll))
    }

    /// Spawn the worker scheduler threads (1..N), exactly once, on the first
    /// [`Runtime::submit`]. Waker slot and parked flag are set only after a
    /// successful spawn, so both always have a live thread behind them. They
    /// may trail the spawn because nothing can target a worker until
    /// `call_once` returns: every other submitter is blocked in it and the new
    /// workers hold no work.
    ///
    /// A worker whose poller or thread cannot be created is skipped. Its flag
    /// stays down and its waker slot empty forever, so no placement path
    /// targets it and its share drains through the other schedulers.
    pub fn ensure_workers(self: &Arc<Self>) {
        self.workers_started.call_once(|| {
            let count = self.slots.len();
            let mut handles = lock(&self.workers);
            'workers: for index in 1..count {
                // Retry transient failures (fd pressure, EAGAIN) before
                // permanently running on fewer cores than requested.
                let mut attempt = 0;
                let (poll, waker) = loop {
                    match mio::Poll::new().and_then(|poll| {
                        let waker = mio::Waker::new(poll.registry(), super::poll::WAKER_TOKEN)?;
                        Ok((poll, Arc::new(waker)))
                    }) {
                        Ok(pair) => break pair,
                        Err(e) if attempt < 2 => {
                            attempt += 1;
                            eprintln!(
                                "warning: cannot create scheduler {index}'s poller ({e}); retrying"
                            );
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                        Err(e) => {
                            eprintln!(
                                "warning: cannot create scheduler {index}'s poller ({e}); \
                                 running without this core"
                            );
                            continue 'workers;
                        }
                    }
                };
                let rt = Arc::clone(self);
                let spawned = std::thread::Builder::new()
                    .name(format!("al-scheduler-{index}"))
                    .spawn(move || super::worker_main(rt, index, poll));
                match spawned {
                    Ok(handle) => {
                        let _ = self.slots[index].waker.set(waker);
                        handles.push(handle);
                        self.slots[index].parked.store(true, Ordering::Release);
                    }
                    Err(e) => {
                        eprintln!("warning: cannot start scheduler {index} ({e})");
                    }
                }
            }
        });
    }

    /// Notify scheduler `i`'s poller. An empty waker slot is a worker that
    /// never spawned, so there is no thread to wake and the notify is dropped.
    fn notify(&self, i: usize) {
        if let Some(waker) = self.slots[i].waker.get() {
            let _ = waker.wake();
        }
    }

    /// Allocate a program-unique process id. Never reused.
    pub(super) fn alloc_pid(&self) -> u64 {
        self.next_pid.fetch_add(1, Ordering::Relaxed)
    }

    /// Retire listener `id` program-wide: drop it from `shared_listeners`, then
    /// queue the id on every live scheduler and wake them. Each owner
    /// deregisters the fd from its own poller and drops its `Arc` clone, so the
    /// fd closes only after the last poller registration is gone.
    ///
    /// Asynchronous: `net.close` returns before every peer has drained, so the
    /// port is released within a scheduling slice. A `net.listen` on the same
    /// port racing that window can see `AddrInUse`, and an already-woken accept
    /// can still hand out a connection that arrived before the close.
    pub(super) fn retire_listener(&self, from: usize, id: i32) {
        if super::lock(&self.shared_listeners).remove(&id).is_none() {
            return; // already retired (double close): peers were queued then
        }
        for (i, slot) in self.slots.iter().enumerate() {
            // A never-spawned worker never drains its queue, so the Vec would
            // grow forever.
            if !self.is_live_scheduler(i) {
                continue;
            }
            super::lock(&slot.retired_listeners).push(id);
            slot.retired_pending.store(true, Ordering::Release);
            // The caller drains its own queue right after this call.
            if i != from {
                self.notify(i);
            }
        }
    }

    /// Submit a seed: hand it directly to an idle scheduler when one exists,
    /// otherwise push it onto the shared overflow queue.
    ///
    /// Direct handoff is what spreads work across cores. A seed in a shared
    /// queue is raced for by every scheduler, and an already-awake one nearly
    /// always wins because a lock is faster than waking a parked thread. Under
    /// a connection flood that pins every process onto one or two schedulers.
    pub fn submit(self: &Arc<Self>, seed: Seed) {
        self.ensure_workers();
        self.process_started();

        if let Some(claim) = self.claim_idle_peer() {
            self.hand(claim, Inbound::Seed(seed));
            return;
        }

        // Overflow, then wake one in case a scheduler parked between the scan
        // above and this push and would otherwise sleep on the stranded seed.
        {
            let mut q = lock(&self.injector);
            q.push_back(seed);
        }
        self.wake_one();
    }

    /// Number of schedulers the runtime was built with (live or not).
    pub fn scheduler_count(&self) -> usize {
        self.slots.len()
    }

    /// Whether scheduler `i` has a live thread behind it.
    pub fn is_live_scheduler(&self, i: usize) -> bool {
        self.slots[i].waker.get().is_some()
    }

    /// Submit a seed pinned to scheduler `i`, the placement primitive behind
    /// the accept fan-out. The seed goes to the pinned queue and is never
    /// stolen. Callers must `ensure_workers` first and only target live
    /// schedulers ([`Runtime::is_live_scheduler`]).
    pub fn submit_to(&self, i: usize, seed: Seed) {
        self.process_started();
        let slot = &self.slots[i];
        slot.run_len.fetch_add(1, Ordering::Relaxed);
        lock(&slot.pinned).push_back(seed);
        self.notify(i);
    }

    /// Claim one idle scheduler for a direct handoff: scan the parked flags
    /// from a rotating offset and CAS the first set flag `true -> false`.
    ///
    /// A donor must claim BEFORE detaching the victim process's fds, since the
    /// detach moves fds out of the donor's tables and must not start until a
    /// destination is guaranteed. The cheap eligibility checks run first.
    pub fn claim_idle_peer(&self) -> Option<Claim> {
        let n = self.slots.len();
        let start = self.submit_cursor.fetch_add(1, Ordering::Relaxed);
        for off in 0..n {
            let i = (start + off) % n;
            if self.slots[i]
                .parked
                .compare_exchange(true, false, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return Some(Claim(i));
            }
        }
        None
    }

    /// Complete a [`Claim`] by handing `work` to the claimed scheduler. The
    /// only way to deliver work to a claimed peer, so the mandatory notify
    /// cannot be forgotten.
    pub fn hand(&self, c: Claim, work: Inbound) {
        let slot = &self.slots[c.0];
        slot.run_len.fetch_add(1, Ordering::Relaxed);
        lock(&slot.inbox).push_back(work);
        self.notify(c.0);
    }

    /// Complete a [`Claim`] with no work, so the peer wakes, finds nothing, and
    /// re-raises its parked flag. For a handoff that fell through after the
    /// claim, and for `wake_one`.
    pub fn release(&self, c: Claim) {
        self.notify(c.0);
    }

    /// Mark scheduler `me` parked or unparked. Only `me` may call this; see
    /// [`SchedSlot::parked`].
    pub fn set_parked(&self, me: usize, parked: bool) {
        self.slots[me].parked.store(parked, Ordering::Release);
    }

    /// Find the least-loaded peer worth donating to when no scheduler is idle.
    /// Requires `my_load >= peer + 2`, so the move strictly narrows the gap
    /// instead of swapping the imbalance back and forth. This is what levels
    /// run queues while every scheduler is busy.
    ///
    /// No claim is taken; the loads are advisory and `donate`'s send-time bump
    /// keeps simultaneous donors off the same peer.
    ///
    /// Never-spawned workers are excluded by their empty waker slot. Their
    /// published load is 0 forever, so without that check a migrant would sit
    /// in an inbox no thread drains.
    pub fn pick_underloaded_peer(&self, me: usize, my_load: usize) -> Option<usize> {
        let n = self.slots.len();
        let start = self.submit_cursor.fetch_add(1, Ordering::Relaxed);
        let mut best: Option<(usize, usize)> = None;
        for off in 0..n {
            let i = (start + off) % n;
            if i == me || self.slots[i].waker.get().is_none() {
                continue;
            }
            let len = self.slots[i].run_len.load(Ordering::Relaxed);
            if best.is_none_or(|(_, b)| len < b) {
                best = Some((i, len));
            }
        }
        let (peer, len) = best?;
        (my_load >= len + 2).then_some(peer)
    }

    /// Publish scheduler `me`'s runnable count for donors to read. Must be
    /// called after draining the inbox, so in-flight bumps are folded in rather
    /// than double-counted, and again before parking empty.
    pub fn publish_load(&self, me: usize, len: usize) {
        self.slots[me].run_len.store(len, Ordering::Relaxed);
    }

    /// Hand a migrating process to a busy `peer` chosen by
    /// [`Runtime::pick_underloaded_peer`]. Use [`Runtime::hand`] for a claimed
    /// idle peer instead, so the [`Claim`] is consumed. The load is bumped here
    /// so a freshly fed peer is not dog-piled before it reports its own count.
    ///
    /// Must NOT go through [`Runtime::submit`]: a migrant is already counted in
    /// `live`, and counting it twice would keep `live` above zero and the
    /// program would never shut down. That count is also what stops a scheduler
    /// observing program-end while a process sits in transit.
    pub fn donate(&self, peer: usize, m: Migrant) {
        let slot = &self.slots[peer];
        slot.run_len.fetch_add(1, Ordering::Relaxed);
        lock(&slot.inbox).push_back(Inbound::Migrant(m));
        self.notify(peer);
    }

    /// Take the work destined for scheduler `me`, plus up to [`SEED_BATCH`]
    /// overflow seeds when its own inbox is empty.
    pub fn take_inbound(&self, me: usize) -> Vec<Inbound> {
        let mut out = self.take_directed(me);
        if out.is_empty() {
            out.extend(self.take_overflow().into_iter().map(Inbound::Seed));
        }
        out
    }

    /// Drain scheduler `me`'s pinned seeds and inbox. Directed work is taken
    /// whole; only the overflow queue has a pickup limit.
    pub fn take_directed(&self, me: usize) -> Vec<Inbound> {
        let slot = &self.slots[me];
        let mut out: Vec<Inbound> = lock(&slot.pinned).drain(..).map(Inbound::Seed).collect();
        out.extend(lock(&slot.inbox).drain(..));
        out
    }

    /// Take up to [`SEED_BATCH`] seeds from the shared overflow queue.
    pub fn take_overflow(&self) -> Vec<Seed> {
        let mut q = lock(&self.injector);
        let take = q.len().min(SEED_BATCH);
        q.drain(..take).collect()
    }

    /// Steal one inbound work item from another scheduler's inbox. Only for an
    /// idle scheduler with nothing local; the owner finding an empty inbox just
    /// parks again. A stolen migrant's `live` count rides with it.
    pub fn steal_inbound(&self, me: usize) -> Option<Inbound> {
        let n = self.slots.len();
        for off in 1..n {
            let i = (me + off) % n;
            if let Some(inbound) = lock(&self.slots[i].inbox).pop_front() {
                return Some(inbound);
            }
        }
        None
    }

    /// A process entered the live set. Paired with
    /// [`Runtime::process_finished`]; every path that adds a process goes here.
    pub fn process_started(&self) {
        self.live.fetch_add(1, Ordering::AcqRel);
    }

    /// A process finished. When it was the last one, wake every scheduler so
    /// they can observe `live == 0` and shut down.
    pub fn process_finished(&self) {
        if self.live.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.wake_all();
        }
    }

    /// Whether any scheduler other than `me` is parked. Busy schedulers leave
    /// injector seeds to idle ones.
    pub fn any_other_idle(&self, me: usize) -> bool {
        self.slots
            .iter()
            .enumerate()
            .any(|(i, s)| i != me && s.parked.load(Ordering::Acquire))
    }

    /// Publish a top-level binding's frozen root to the shared global area.
    ///
    /// A slot may be published several times: `Op::StoreLocal` republishes on
    /// every entry-frame store. All of a slot's stores happen inside the one
    /// top-level statement that owns it, before any closure that could read it
    /// exists, so a reader only ever sees the final value.
    ///
    /// The caller must have already deep-copied the graph into the frozen area,
    /// so `frozen` is an immediate or points at fully written segments. The
    /// table store happens-before the `Release` bump of `globals_version` that
    /// readers `Acquire`-load.
    pub fn publish_global(&self, slot: usize, frozen: FrozenValue) {
        {
            let mut g = lock(&self.globals);
            if g.len() <= slot {
                g.resize_with(slot + 1, || None);
            }
            g[slot] = Some(frozen);
        }
        self.globals_version.fetch_add(1, Ordering::Release);
    }

    /// Whether the whole program has finished.
    pub fn is_finished(&self) -> bool {
        self.live.load(Ordering::Acquire) == 0
    }

    /// Record a scheduler's `VmError` (first one wins) and force every
    /// scheduler out of its wait so the runtime can be torn down.
    pub(super) fn raise_fault(&self, index: usize, e: super::VmError) {
        {
            let mut slot = lock(&self.fault);
            if slot.is_none() {
                *slot = Some((index, e));
            }
        }
        self.raise_fault_flag();
    }

    /// The teardown half of [`Runtime::raise_fault`], for when the caller
    /// keeps the error itself (scheduler 0's own failure).
    pub(super) fn raise_fault_flag(&self) {
        self.faulted.store(true, Ordering::Release);
        self.shutdown_blocking();
        self.wake_all();
    }

    pub(super) fn is_faulted(&self) -> bool {
        self.faulted.load(Ordering::Acquire)
    }

    pub(super) fn take_fault(&self) -> Option<(usize, super::VmError)> {
        lock(&self.fault).take()
    }

    /// Wake one parked scheduler. The wake clears its flag, so back-to-back
    /// submissions fan out instead of all notifying the same one.
    fn wake_one(&self) {
        if let Some(claim) = self.claim_idle_peer() {
            self.release(claim);
        }
    }

    /// Wake every scheduler that has a thread.
    fn wake_all(&self) {
        for i in 0..self.slots.len() {
            self.notify(i);
        }
    }

    /// Hand a blocking op to the pool, delivered back to scheduler `origin`
    /// under `job_id`. At the worker cap the job waits; a worker re-checks the
    /// queue before parking, so it is never stranded.
    pub fn offload(self: &Arc<Self>, origin: usize, job_id: u64, op: BlockingOp) {
        lock(&self.blocking.queue).push_back((job_id, origin, op));
        if self.blocking.idle.load(Ordering::Acquire) > 0 {
            self.blocking.cond.notify_one();
        } else if self.blocking.total.load(Ordering::Acquire) < self.blocking.max_total {
            self.spawn_blocking_worker();
        } else {
            self.blocking.cond.notify_one();
        }
    }

    fn spawn_blocking_worker(self: &Arc<Self>) {
        self.blocking.total.fetch_add(1, Ordering::AcqRel);
        let rt = Arc::clone(self);
        if let Err(e) = std::thread::Builder::new()
            .name("al-blocking".into())
            .spawn(move || blocking_worker_main(rt))
        {
            eprintln!("warning: cannot spawn blocking worker ({e})");
            self.blocking.total.fetch_sub(1, Ordering::AcqRel);
            if self.blocking.total.load(Ordering::Acquire) == 0 {
                // No worker exists and none could be made: a queued job has
                // nobody to run it and its process would park forever. Fail
                // each back to its origin as an error completion.
                let stranded: Vec<_> = lock(&self.blocking.queue).drain(..).collect();
                for (job_id, origin, op) in stranded {
                    self.deliver_completion(
                        origin,
                        Completion {
                            job_id,
                            result: failed_blocking_result(op),
                        },
                    );
                }
            } else {
                self.blocking.cond.notify_one();
            }
        }
    }

    /// Route a finished blocking job back to its scheduler and wake it.
    pub fn deliver_completion(&self, origin: usize, c: Completion) {
        lock(&self.slots[origin].completions).push_back(c);
        self.notify(origin);
    }

    /// Signal blocking workers to exit at program end. With `live == 0` none
    /// are mid-op, so nothing is interrupted.
    pub fn shutdown_blocking(&self) {
        self.blocking.shutdown.store(true, Ordering::Release);
        self.blocking.cond.notify_all();
    }
}

/// A blocking-pool worker: pull a job, run it, deliver the result, repeat.
/// Exits if it would be the `max_warm + 1`-th idle worker, or on shutdown.
fn blocking_worker_main(rt: Arc<Runtime>) {
    loop {
        let (job_id, origin, op) = {
            let mut q = lock(&rt.blocking.queue);
            loop {
                if rt.blocking.shutdown.load(Ordering::Acquire) {
                    rt.blocking.total.fetch_sub(1, Ordering::AcqRel);
                    return;
                }
                if let Some(job) = q.pop_front() {
                    break job;
                }
                if rt.blocking.idle.load(Ordering::Acquire) >= rt.blocking.max_warm {
                    rt.blocking.total.fetch_sub(1, Ordering::AcqRel);
                    return;
                }
                rt.blocking.idle.fetch_add(1, Ordering::AcqRel);
                q = rt.blocking.cond.wait(q).unwrap_or_else(|e| e.into_inner());
                rt.blocking.idle.fetch_sub(1, Ordering::AcqRel);
            }
        };
        let result = run_blocking(op);
        rt.deliver_completion(origin, Completion { job_id, result });
    }
}

/// Run one blocking op, no locks held.
fn run_blocking(op: BlockingOp) -> BlockingResult {
    match op {
        BlockingOp::ReadFile(path) => {
            let result = std::fs::read(&path);
            BlockingResult::ReadFile { path, result }
        }
        BlockingOp::WriteFile(path, bytes) => {
            let result = std::fs::write(&path, &bytes);
            BlockingResult::WriteFile { path, result }
        }
        BlockingOp::ResolveDns(host) => BlockingResult::ResolveDns {
            result: resolve_host(&host),
        },
    }
}

/// Resolve a hostname via `getaddrinfo`, taking the first address it yields.
/// Blocking-pool workers only, never a scheduler thread.
fn resolve_host(host: &str) -> std::io::Result<std::net::IpAddr> {
    use std::net::ToSocketAddrs;
    (host, 0u16)
        .to_socket_addrs()?
        .next()
        .map(|sa| sa.ip())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "name resolution returned no addresses",
            )
        })
}

/// How many schedulers to run: `AL_SCHEDULERS`, else one per CPU core.
pub(super) fn scheduler_count() -> usize {
    let from_env = std::env::var("AL_SCHEDULERS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&n| (1..=256).contains(&n));
    match from_env {
        Some(n) => n,
        None => std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1),
    }
}
