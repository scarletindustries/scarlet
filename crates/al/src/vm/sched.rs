//! The multi-core scheduler runtime.
//!
//! A [`Runtime`] exists for every program run, constructed before any code
//! executes. Construction is cheap: queues, flags, and counters, plus one OS
//! poller for scheduler 0 (the calling thread) — no OS threads, and no other
//! pollers. The worker scheduler threads — one per CPU core beyond the main
//! thread — are summoned by the first `submit`
//! ([`Runtime::ensure_workers`]). A spawned process starts life as a
//! [`Seed`] — an owned mini-heap holding a deep copy of the closure graph
//! plus the root `Value` pointing into it — pushed onto a shared injector
//! queue or handed to an idle scheduler. Whichever scheduler takes it
//! adopts the heap as the child
//! process's initial heap: the heap moves as a unit, nothing is
//! rebuilt on the receiving side. A process's heap is only ever touched by
//! the scheduler currently running it.
//!
//! Idle schedulers park inside their OS poller; submitting a seed (or the last
//! process finishing) wakes them via [`mio::Waker::wake`].

use std::collections::{HashMap, VecDeque};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, Once, OnceLock};

use al_core::bytecode::{Program, Value};
use al_core::heap::ProcHeap;

use super::freeze::FrozenValue;
use super::migrate::Migrant;

/// How many seeds a scheduler takes from the injector per visit. One at a
/// time maximizes spread: with k seeds and k idle schedulers, every scheduler
/// gets exactly one.
pub(super) const SEED_BATCH: usize = 1;

/// A spawned-but-not-yet-started process: the unit of cross-scheduler work
/// distribution. `Send` by construction — `heap` is owned memory and `root`
/// points only into it (or into the frozen area), so handing a seed to
/// another scheduler is a plain move.
pub(super) struct Seed {
    /// The spawned closure, as a pointer into the child heap.
    pub root: Value,
    /// Connections the closure captured (moved — the spawner loses them).
    /// Listeners do not travel: each scheduler binds its own reuseport socket
    /// from the shared address on first accept (`VM::ensure_listener`).
    pub connections: Vec<(i32, TcpStream)>,
    /// The child's allocator handle. A zero-sized marker (allocation goes to
    /// mimalloc's per-thread default heap); field order is not load-bearing,
    /// since `mi_free` is global and frees `root`'s graph from any thread.
    pub heap: ProcHeap,
}

// A seed crosses scheduler threads (inbox handoff or the shared injector), so
// the "`Send` by construction" claim above is compiler-checked here, exactly
// like `Process`/`Migrant`: the heap owns its slabs, `root` is one machine
// word into them, and the fd handles are inherently `Send`. If `Seed` ever
// regains a field that cannot move across threads, spawning must not build.
const _: () = {
    const fn assert_send<T: Send>() {}
    assert_send::<Seed>();
};

/// A unit of work delivered to a scheduler's private inbox: a process that
/// has not started yet (a seed) or a process moving between schedulers
/// mid-execution (a migrant). Migrants are only ever direct-handed to a
/// chosen peer — a claimed idle scheduler, or the least-loaded busy one —
/// never queued to the shared overflow injector, which stays `Seed`-only.
pub(super) enum Inbound {
    Seed(Seed),
    Migrant(Migrant),
}

/// A blocking operation handed to the [`BlockingPool`] so it runs on a worker
/// thread instead of stalling a scheduler. The payload is fully `Send`.
#[derive(Debug)]
pub(super) enum BlockingOp {
    ReadFile(String),
    WriteFile(String, Vec<u8>),
    ResolveDns(String),
}

/// The `Send` result of a [`BlockingOp`], delivered back to the originating
/// scheduler. Only raw data crosses the pool boundary — never a `Value`; the
/// scheduler constructs the result `Value` into the *resuming process's*
/// arena at delivery, so no worker thread ever
/// touches a process heap.
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
/// `job_id` is the parked process's wait id, so the scheduler resumes it
/// directly without re-running the instruction (the result `Value` is built
/// into that process's arena at delivery; see `VM::drain_completions`).
pub(super) struct Completion {
    pub job_id: u64,
    pub result: BlockingResult,
}

/// An elastic pool of OS threads for blocking syscalls (file reads and
/// writes, DNS resolution) that must never run on a scheduler thread. Idle workers park *warm*
/// and are reused with no spawn cost; a burst grows the pool up to `max_total`;
/// as work drains, idle workers above `max_warm` exit so threads aren't leaked.
struct BlockingPool {
    /// Pending jobs: `(job_id, origin scheduler, op)`.
    queue: Mutex<VecDeque<(u64, usize, BlockingOp)>>,
    /// Workers park here when there is no work.
    cond: Condvar,
    /// Live worker threads (busy + parked).
    total: AtomicUsize,
    /// Workers currently parked on `cond`.
    idle: AtomicUsize,
    /// Hard ceiling on worker threads.
    max_total: usize,
    /// Most idle workers kept warm; extras exit on going idle.
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

/// Per-scheduler shared state: everything other schedulers (and the blocking
/// pool) touch to place work on, wake, or read the load of one scheduler.
/// Collected into one struct so a scheduler slot cannot be half-initialized
/// — adding a per-scheduler field means adding it here, not another parallel
/// `Vec` on `Runtime` that could end up a different length.
pub(super) struct SchedSlot {
    /// Work inbox. `submit` hands a seed directly to a chosen idle
    /// scheduler's inbox so the placement decision is made at submit time
    /// instead of being raced through a shared queue (where the scheduler
    /// that is already awake almost always wins); a migrating process is
    /// handed to a claimed idle peer the same way. Direct handoff is a
    /// placement preference, not exclusive ownership: a peer with nothing at
    /// all to run may still take undelivered work out of another scheduler's
    /// inbox ([`Runtime::steal_inbound`]) — the woken owner then finds an
    /// empty inbox and re-parks.
    pub(super) inbox: Mutex<VecDeque<Inbound>>,
    /// Pinned seeds: work that must run on exactly this scheduler and is never
    /// stolen ([`Runtime::steal_inbound`] ignores it). This is how the accept
    /// fan-out ([`super::VM::spawn_on_each`]) keeps one acceptor per core:
    /// each must bind its own `SO_REUSEPORT` socket, so an acceptor stolen
    /// onto a core that already has the listener bound would leave its own
    /// core's socket unbound and starve it of connections. The owner drains
    /// these like directed work; no other scheduler ever does.
    pub(super) pinned: Mutex<VecDeque<Seed>>,
    /// Poller waker. `notify()` wakes this scheduler whether it is parked on
    /// I/O, on a timer, or waiting for work (the waker is registered with
    /// this scheduler's `mio::Poll` under [`super::poll::WAKER_TOKEN`]).
    /// Slot 0 is filled at construction (created by [`Runtime::new`] for the
    /// calling thread); a worker's slot is filled by `ensure_workers` only
    /// once its thread has spawned, so the slot is empty exactly when its
    /// worker never spawned. That makes the slot double as the liveness
    /// check: donation targeting skips empty slots
    /// ([`Runtime::pick_underloaded_peer`]), the flag invariant below keeps
    /// every other placement path away, and a stray notify aimed at an empty
    /// slot is dropped harmlessly.
    pub(super) waker: OnceLock<Arc<mio::Waker>>,
    /// Whether this scheduler is currently parked (idle or waiting on I/O).
    ///
    /// Flag lifecycle. A raised flag is a promise: whoever claims it (the
    /// CAS `true -> false` in `claim_idle_peer`) may push work into this
    /// scheduler's inbox and notify its poller, and a live thread will take
    /// the work. The invariants that keep the promise:
    ///
    /// - Every flag starts DOWN at construction, when no worker thread
    ///   exists yet.
    /// - A worker's flag is first raised inside `ensure_workers`' one-shot
    ///   critical section — by `ensure_workers` itself after the worker's
    ///   waker slot is filled, or by the freshly spawned worker parking idle
    ///   (a new thread can reach its idle wait, and raise its own flag,
    ///   before its waker slot is set). Either way no claimant can act on
    ///   the flag before `call_once` returns: every submitter is blocked in
    ///   `call_once`, the just-spawned workers hold no work and so never
    ///   submit, donate, or wake anyone during the window, and no other
    ///   placement source exists yet — and by return every spawned worker's
    ///   waker slot is filled. So a claimable flag always has a poller to
    ///   notify and a live thread to wake. Workers are seeded "idle" this
    ///   way so the first wave of submits hands seeds straight to them
    ///   instead of hoarding work on the schedulers already running.
    /// - Thereafter each scheduler owns its flag: it raises it before
    ///   parking (idle wait or I/O poll) and lowers it on waking. Claimants
    ///   lower it via the CAS, which makes the parked scheduler invisible to
    ///   every other placement while a handoff is in flight; a claim must
    ///   end in a notify (see [`Claim`]) so the scheduler wakes and
    ///   re-raises it.
    /// - A worker whose thread failed to spawn keeps its flag down forever,
    ///   so the flag-gated paths (`submit`, `claim_idle_peer`, `wake_one`)
    ///   never target it; the flag-blind path, donation to a busy peer,
    ///   skips it by its empty waker slot instead
    ///   ([`Runtime::pick_underloaded_peer`]). The seeds it would have taken
    ///   drain through the other schedulers and the shared injector.
    pub(super) parked: AtomicBool,
    /// Published load: how many runnable processes (running + queued) this
    /// scheduler last reported, plus one for every directed inbound (donated
    /// migrant or direct-handed seed) still in flight to it. Donation reads
    /// these to find the least-loaded peer, so queue-length imbalances are
    /// leveled while every scheduler is busy — an overloaded scheduler must
    /// not wait for a peer to drain completely before any work moves.
    ///
    /// Advisory, not authoritative: each scheduler overwrites its own slot
    /// (`publish_load`) after every scheduling step — past the inbox drain,
    /// so an in-flight bump is folded into the owner's next report — and
    /// again whenever it parks with nothing to run. The park-time republish
    /// is what corrects the bump for work that never arrives: a
    /// direct-handed seed stolen out of a parked owner's inbox would
    /// otherwise leave the +1 in place for as long as the owner sleeps.
    /// A stale read can only mistune one donation decision, briefly, so
    /// plain relaxed loads/stores suffice.
    pub(super) run_len: AtomicUsize,
    /// Completion queue: a blocking worker pushes a finished job here and
    /// `notify()`s this scheduler's poller to deliver the result.
    pub(super) completions: Mutex<VecDeque<Completion>>,
}

impl SchedSlot {
    fn new(is_main: bool) -> Self {
        SchedSlot {
            inbox: Mutex::new(VecDeque::new()),
            pinned: Mutex::new(VecDeque::new()),
            waker: OnceLock::new(),
            // All flags start down; `ensure_workers` raises a worker's flag
            // only once its thread exists (see the `parked` field's doc).
            parked: AtomicBool::new(false),
            // Scheduler 0 is running the main process; workers start empty.
            run_len: AtomicUsize::new(usize::from(is_main)),
            completions: Mutex::new(VecDeque::new()),
        }
    }
}

/// A claimed idle scheduler: the exclusive right to hand work into slot `i`'s
/// inbox and wake it, obtained by [`Runtime::claim_idle_peer`]'s CAS lowering
/// the parked flag. The claimed scheduler is asleep with its flag down —
/// invisible to every other submitter and donor — so a claim MUST end in a
/// notify: consume it via [`Runtime::hand`] (push work, then wake) or
/// [`Runtime::release`] (wake with no work, when a handoff fell through). A
/// dropped `Claim` is the "sleeps forever, unwakeable" bug the `#[must_use]`
/// exists to catch.
#[must_use = "a claimed scheduler must be handed work or released"]
pub(super) struct Claim(usize);

/// State shared by every scheduler in the program.
pub(super) struct Runtime {
    /// The shared program; each worker scheduler runs a private clone of it
    /// (constants are frozen words pointing into `program.frozen`, shared).
    pub program: Arc<Program>,
    /// The program's command-line arguments: the entrypoint path followed by
    /// every argument passed after it. Read once per `Op::Argv`; shared by all
    /// schedulers so any process can call `process.argv`.
    pub argv: Vec<String>,
    /// The program's global (literal) area: top-level bindings as frozen
    /// value words,
    /// published once when main writes them. Workers copy the words into
    /// their own globals table, gated by `globals_version`.
    pub globals: Mutex<Vec<Option<FrozenValue>>>,
    /// Bumped on every publish; lets schedulers skip syncing when nothing
    /// changed.
    pub globals_version: AtomicU64,
    /// Bound addresses of every live listener, keyed by socket id. A listener
    /// is a shared, addr-keyed resource: each scheduler that needs to accept
    /// on one binds its own `SO_REUSEPORT` socket to this address
    /// (`VM::ensure_listener`), so the kernel load-balances connections across
    /// per-core sockets instead of every core sharing one accept queue.
    pub shared_listeners: Mutex<HashMap<i32, SocketAddr>>,
    /// Per-scheduler shared slots — inbox, pinned queue, waker, parked flag,
    /// published load, completion queue — indexed by scheduler id. See
    /// [`SchedSlot`] for the per-field invariants.
    pub(super) slots: Vec<SchedSlot>,
    /// Overflow queue: seeds submitted while every scheduler was busy. Taken
    /// (one per visit) by whichever scheduler frees up first.
    pub injector: Mutex<VecDeque<Seed>>,
    /// Rotates the starting point of the idle-scheduler scans — both
    /// `submit`'s seed placement and donation's peer claim — so consecutive
    /// placements spread across different schedulers.
    pub submit_cursor: AtomicUsize,
    /// Live processes across all schedulers, counting undelivered seeds and the
    /// main process. The program is over when this reaches zero.
    pub live: AtomicUsize,
    /// Worker threads, joined by scheduler 0 at shutdown.
    pub workers: Mutex<Vec<std::thread::JoinHandle<()>>>,
    /// One-shot guard for `ensure_workers`: worker threads spawn exactly
    /// once, on the first submit.
    workers_started: Once,
    /// Elastic worker pool for blocking syscalls (file I/O, …) that must not
    /// run on a scheduler thread.
    blocking: BlockingPool,
}

impl Runtime {
    /// Build the runtime for `count` schedulers. Construction is cheap:
    /// queues, flags, and counters are set up, plus scheduler 0's OS poller
    /// (created here so the calling thread can park and be notified from
    /// the start); no OS threads start, and worker pollers are created when
    /// their threads spawn ([`Runtime::ensure_workers`]). `count` comes from
    /// [`scheduler_count`], so `AL_SCHEDULERS` is read at construction,
    /// never later.
    ///
    /// The only failure is scheduler 0's poller creation (fd exhaustion):
    /// without it the calling thread could never park, so the runtime — and
    /// the VM over it — cannot be built at all. The poller itself is
    /// returned to the caller (`mio::Poll` is owned by its scheduler
    /// thread); only its waker goes into the shared slot table.
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
            slots,
            injector: Mutex::new(VecDeque::new()),
            submit_cursor: AtomicUsize::new(0),
            // The main process is live.
            live: AtomicUsize::new(1),
            blocking: BlockingPool::new(),
            workers: Mutex::new(Vec::new()),
            workers_started: Once::new(),
        });
        Ok((runtime, poll))
    }

    /// Spawn the worker scheduler threads (indices 1..N), exactly once,
    /// triggered by the first [`Runtime::submit`]. Per worker: create its
    /// poller, spawn the thread, and only on success store the waker in
    /// its slot, record the join handle, and raise its parked flag — the
    /// order that keeps both invariants (a claimable flag, and a filled
    /// waker slot, always have a live thread behind them). The slot and
    /// flag can trail the spawn because nothing can target this worker
    /// until `call_once` returns: every concurrent submitter is blocked in
    /// `call_once`, the freshly spawned workers hold no work — there is no
    /// seed anywhere yet — so they cannot submit, donate, or wake anyone
    /// during the window (a new worker may park and raise its own flag
    /// before its slot is set; harmless, since no claimant runs until the
    /// section ends, by which point the slot is filled), and no other
    /// placement source exists yet. The thread itself never reads its own
    /// waker slot — it holds its poller directly; the slot exists for
    /// *other* schedulers to notify through.
    ///
    /// A worker whose poller or thread cannot be created is skipped: its
    /// flag stays down and its waker slot stays empty forever, so no
    /// placement path targets it — seeds pass over the down flag and drain
    /// through the remaining schedulers and the shared injector, and
    /// donation skips the empty slot ([`Runtime::pick_underloaded_peer`]),
    /// which matters because migrants never enter the injector.
    pub fn ensure_workers(self: &Arc<Self>) {
        self.workers_started.call_once(|| {
            let count = self.slots.len();
            let mut handles = lock(&self.workers);
            for index in 1..count {
                let (poll, waker) = match mio::Poll::new().and_then(|poll| {
                    let waker = mio::Waker::new(poll.registry(), super::poll::WAKER_TOKEN)?;
                    Ok((poll, Arc::new(waker)))
                }) {
                    Ok(pair) => pair,
                    Err(e) => {
                        eprintln!("warning: cannot create scheduler {index}'s poller ({e})");
                        continue;
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

    /// Notify scheduler `i`'s poller. An empty waker slot belongs to a
    /// worker that never spawned (the slot is filled only after a
    /// successful thread spawn); no placement path targets such a worker,
    /// so dropping the notify is correct — there is no thread to wake.
    fn notify(&self, i: usize) {
        if let Some(waker) = self.slots[i].waker.get() {
            let _ = waker.wake();
        }
    }

    /// Submit a seed: hand it directly to an idle scheduler when one exists,
    /// otherwise push it onto the shared overflow queue for whichever busy
    /// scheduler frees up first.
    ///
    /// Direct handoff is what spreads work across cores. A seed in a shared
    /// queue is raced for by every scheduler — and the one that is already
    /// awake almost always wins, because taking a queue lock is microseconds
    /// faster than waking a parked thread. Under a connection flood that race
    /// pins every connection process onto the first one or two schedulers
    /// while the rest of the machine sits idle. Claiming a parked scheduler's
    /// flag and writing into its private inbox makes the placement decision
    /// at submit time, where it cannot be raced.
    pub fn submit(self: &Arc<Self>, seed: Seed) {
        // The first submit summons the worker threads; by the time
        // `ensure_workers` returns, every spawned worker's flag is raised,
        // so the scan below can hand even the very first seed straight to
        // one.
        self.ensure_workers();
        self.live.fetch_add(1, Ordering::AcqRel);

        // Prefer a parked (idle) scheduler, starting the scan at a rotating
        // offset so back-to-back submissions land on different schedulers.
        if let Some(claim) = self.claim_idle_peer() {
            self.hand(claim, Inbound::Seed(seed));
            return;
        }

        // Every scheduler is busy: overflow. Then re-wake one in case a
        // scheduler parked between the scan above and this push (it would
        // otherwise sleep until its own I/O fires, with the seed stranded).
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

    /// Whether scheduler `i` has a live thread behind it. Scheduler 0 always
    /// does; a worker's waker slot is filled only once its thread spawns, so an
    /// empty slot is a worker that never started ([`Runtime::ensure_workers`]).
    pub fn is_live_scheduler(&self, i: usize) -> bool {
        self.slots[i].waker.get().is_some()
    }

    /// Submit a seed pinned to a specific scheduler — the placement primitive
    /// behind [`super::VM::spawn_on_each`]'s accept fan-out, where each
    /// scheduler must run an acceptor against its own reuseport socket. Unlike
    /// [`Runtime::submit`] there is no idle-peer scan (the target is chosen by
    /// the caller) and the seed goes to the pinned queue, so it is never stolen
    /// onto another core — an acceptor must bind *this* core's socket or the
    /// core gets no connections. Callers must `ensure_workers` first and only
    /// target live schedulers ([`Runtime::is_live_scheduler`]).
    pub fn submit_to(self: &Arc<Self>, i: usize, seed: Seed) {
        self.live.fetch_add(1, Ordering::AcqRel);
        let slot = &self.slots[i];
        slot.run_len.fetch_add(1, Ordering::Relaxed);
        lock(&slot.pinned).push_back(seed);
        self.notify(i);
    }

    /// Claim one idle scheduler for a direct handoff: scan the parked flags
    /// from a rotating offset (so back-to-back placements fan out across
    /// schedulers) and CAS the first set flag `true -> false` — the
    /// placement scan shared by [`Runtime::submit`] and donors. A claimed
    /// scheduler is invisible to every other submitter and donor (its flag
    /// is down), so whatever is pushed into its inbox next cannot be raced
    /// away by another placement decision; it keeps sleeping until notified.
    ///
    /// The returned [`Claim`] is `#[must_use]`: it must be consumed via
    /// [`Runtime::hand`] (push work, then wake) or [`Runtime::release`]
    /// (wake with no work, when a handoff fell through). A dropped `Claim`
    /// is a scheduler asleep with its flag down — unwakeable by submitters
    /// until unrelated I/O happens to fire on its poller.
    ///
    /// Donors claim BEFORE detaching the victim process's fds. The detach
    /// has side effects (it moves connection fds out of the donor's tables),
    /// so it must not start until a destination is guaranteed; the local
    /// eligibility checks, which are cheap and effect-free, run before the
    /// claim.
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

    /// Complete a [`Claim`] by handing `work` to the claimed scheduler:
    /// bump its published load, push into its inbox, and wake it. This is
    /// the only way to deliver work to a claimed idle peer — the claim is
    /// consumed, so the mandatory notify cannot be forgotten.
    pub fn hand(&self, c: Claim, work: Inbound) {
        let slot = &self.slots[c.0];
        slot.run_len.fetch_add(1, Ordering::Relaxed);
        lock(&slot.inbox).push_back(work);
        self.notify(c.0);
    }

    /// Complete a [`Claim`] with no work: notify the claimed scheduler so
    /// it wakes, finds nothing, re-parks, and republishes its parked flag.
    /// Used when a handoff falls through after the claim (fd detach failed)
    /// and by `wake_one`'s bare wake. Skipping this would leave the peer
    /// asleep with its flag down — invisible to every future submit/donate.
    pub fn release(&self, c: Claim) {
        self.notify(c.0);
    }

    /// Mark scheduler `me` as parked/unparked so submitters and donors know
    /// who to wake. Owned by scheduler `me` alone; see [`SchedSlot::parked`]
    /// for the flag lifecycle.
    pub fn set_parked(&self, me: usize, parked: bool) {
        self.slots[me].parked.store(parked, Ordering::Release);
    }

    /// Find the least-loaded peer worth donating to when no scheduler is
    /// idle: the one whose published load ([`SchedSlot::run_len`]) is
    /// smallest, provided moving one process strictly narrows the imbalance
    /// (`my_load >= peer + 2` — after the move the donor has shed one and
    /// the peer holds one more, so a smaller gap would only swap the
    /// imbalance back and forth). This is what levels run-queue lengths
    /// while every scheduler is busy: long-running CPU-bound processes can
    /// otherwise sit 5-deep on one scheduler and alone on another for the
    /// whole run, with the idle-driven path never firing because nobody
    /// drains.
    ///
    /// Ties break from a rotating offset (the [`Runtime::submit`] pattern)
    /// so concurrent donors fan out across equally starved peers. No claim
    /// is taken — the loads are advisory and the racing window between
    /// choosing and donating is one yield at worst; `donate`'s send-time
    /// bump is what keeps simultaneous donors from all picking the same
    /// peer.
    ///
    /// Never-spawned workers (skipped by `ensure_workers` on poller or
    /// thread-creation failure) are excluded by their empty poller slot.
    /// This path is the one placement decision not gated on the parked
    /// flag, and a dead worker's published load is 0 forever — without the
    /// check it would look maximally underloaded and the migrant would sit
    /// in an inbox no thread drains, recoverable only by an idle
    /// scheduler's steal, which under sustained load never comes.
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

    /// Publish scheduler `me`'s runnable count (running + queued) for
    /// donors' [`Runtime::pick_underloaded_peer`] scans. Called after every
    /// change to the count — and always after draining the inbox, so the
    /// in-flight bumps from `donate`/`submit` are folded into the owner's
    /// own report rather than double-counted — and once more before the
    /// owner parks empty, clearing a bump whose work a peer stole away
    /// (see the [`SchedSlot::run_len`] doc).
    pub fn publish_load(&self, me: usize, len: usize) {
        self.slots[me].run_len.store(len, Ordering::Relaxed);
    }

    /// Hand a migrating process to a busy `peer` chosen by
    /// [`Runtime::pick_underloaded_peer`] — the load-leveling path when no
    /// scheduler is idle. A busy peer may have parked on I/O between being
    /// chosen and the push, so it is notified (`notify` is sticky, so the
    /// worst case for a peer that stayed busy is one spurious poll wakeup).
    /// For a claimed idle peer, use [`Runtime::hand`] instead so the
    /// [`Claim`] is consumed.
    ///
    /// The destination's published load is bumped here, when the migrant
    /// becomes visible to other donors, so a freshly fed peer is not
    /// dog-piled before it can report its own count.
    ///
    /// Deliberately NOT routed through [`Runtime::submit`]: a migrant is an
    /// already-running process, so it is already counted in `live`, and
    /// counting it again would leave `live` permanently above zero — the
    /// program would never shut down. The same fact closes the shutdown
    /// race: a migrant in transit holds `live > 0`, so no scheduler can
    /// observe program-end and exit while a process sits in transit in an
    /// inbox.
    ///
    /// Migrants are only ever direct-handed like this, never queued to the
    /// shared injector (which stays `Seed`-only): with no chosen
    /// destination, a migrant could strand in the overflow queue behind
    /// busy schedulers — exactly the imbalance donation exists to fix.
    pub fn donate(&self, peer: usize, m: Migrant) {
        let slot = &self.slots[peer];
        slot.run_len.fetch_add(1, Ordering::Relaxed);
        lock(&slot.inbox).push_back(Inbound::Migrant(m));
        self.notify(peer);
    }

    /// Take the work destined for scheduler `me`: everything handed to its
    /// inbox (seeds and migrants), plus (when the inbox is empty) up to
    /// [`SEED_BATCH`] seeds from the shared overflow queue.
    pub fn take_inbound(&self, me: usize) -> Vec<Inbound> {
        let mut out = self.take_directed(me);
        if out.is_empty() {
            out.extend(self.take_overflow().into_iter().map(Inbound::Seed));
        }
        out
    }

    /// Drain scheduler `me`'s directed work: pinned seeds that must run here
    /// ([`Runtime::submit_to`]), plus its inbox — seeds direct-handed by
    /// `submit` and migrants donated by peers. Directed work is taken
    /// unconditionally (a busy scheduler drains it at every yield); only the
    /// undirected overflow queue is subject to the pickup limit.
    pub fn take_directed(&self, me: usize) -> Vec<Inbound> {
        let slot = &self.slots[me];
        let mut out: Vec<Inbound> = lock(&slot.pinned).drain(..).map(Inbound::Seed).collect();
        out.extend(lock(&slot.inbox).drain(..));
        out
    }

    /// Take up to [`SEED_BATCH`] seeds from the shared overflow queue:
    /// undirected work raced for by whichever scheduler has capacity first.
    pub fn take_overflow(&self) -> Vec<Seed> {
        let mut q = lock(&self.injector);
        let take = q.len().min(SEED_BATCH);
        q.drain(..take).collect()
    }

    /// Steal one unit of inbound work from another scheduler's inbox: handed
    /// off but not yet taken. Called only by an idle scheduler with nothing
    /// local, so the work starts immediately instead of waiting for its
    /// assigned scheduler to wake; the owner finding an empty inbox simply
    /// parks again. This applies to donated migrants as much as seeds — a
    /// stolen migrant's live count rides with it (counted since its original
    /// submit), so the handoff is race-free wherever it lands.
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

    /// A process finished. When it was the last one, wake every scheduler so
    /// they can observe `live == 0` and shut down.
    pub fn process_finished(&self) {
        if self.live.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.wake_all();
        }
    }

    /// Whether any scheduler other than `me` is idle (parked waiting for
    /// work). Busy schedulers leave injector seeds to idle ones.
    pub fn any_other_idle(&self, me: usize) -> bool {
        self.slots
            .iter()
            .enumerate()
            .any(|(i, s)| i != me && s.parked.load(Ordering::Acquire))
    }

    /// Publish a top-level binding's frozen root to the shared global area.
    /// This is not at-most-once per slot: `Op::StoreLocal` republishes on
    /// every entry-frame store, and binary-pattern cursor temps and
    /// or-pattern alternative bindings store the same slot several times.
    /// All of a slot's stores happen inside the single top-level statement
    /// that owns it, before any closure that could read the slot exists, so
    /// each readable slot reaches its final published value before any
    /// reader can observe it.
    ///
    /// The caller has already deep-copied the value graph into the frozen
    /// area (`freeze::freeze_global` — `rc_publish_graph` with the frozen
    /// builder as destination), so `frozen` is an immediate or points
    /// at fully written frozen segments. The table store happens-before the
    /// `Release` bump of `globals_version`, and readers `Acquire`-load the
    /// version before touching the table, so a frozen pointer is only ever
    /// observed after its segment contents are visible.
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

    /// Wake one parked scheduler, if any. The parked flag is consumed
    /// (cleared) by the wake so that back-to-back submissions fan out across
    /// different schedulers instead of all notifying the same one.
    fn wake_one(&self) {
        if let Some(claim) = self.claim_idle_peer() {
            self.release(claim);
        }
    }

    /// Wake every scheduler (empty waker slots — never-spawned workers —
    /// have no thread to wake and are skipped).
    fn wake_all(&self) {
        for i in 0..self.slots.len() {
            self.notify(i);
        }
    }

    /// Hand a blocking op to the pool, to be delivered back to scheduler
    /// `origin` under `job_id`. Reuses a warm worker, else spawns one up to the
    /// cap; at the cap the job waits for a busy worker to free up (a worker
    /// re-checks the queue before parking, so it is never stranded).
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
            // Couldn't spawn: undo the count and let an existing worker take it.
            eprintln!("warning: cannot spawn blocking worker ({e})");
            self.blocking.total.fetch_sub(1, Ordering::AcqRel);
            self.blocking.cond.notify_one();
        }
    }

    /// Route a finished blocking job back to its scheduler and wake it.
    /// `origin` issued the job, so its thread and poller exist.
    pub fn deliver_completion(&self, origin: usize, c: Completion) {
        lock(&self.slots[origin].completions).push_back(c);
        self.notify(origin);
    }

    /// Signal blocking workers to exit at program end. Idle workers wake and
    /// return; with `live == 0` none are mid-op, so nothing is interrupted.
    pub fn shutdown_blocking(&self) {
        self.blocking.shutdown.store(true, Ordering::Release);
        self.blocking.cond.notify_all();
    }
}

/// A blocking-pool worker: pull a job, run it, deliver the result, repeat.
/// Parks *warm* between jobs; exits if it would be the `max_warm + 1`-th idle
/// worker (reaping a burst) or once shutdown is signalled.
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
                // No work: keep at most `max_warm` workers parked, reap the rest.
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

/// Run one blocking op (no locks held). The error is captured `Send` and turned
/// into a typed `IoError` by the scheduler on delivery.
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

/// Resolve a hostname to an IP address via the system resolver (`getaddrinfo`).
/// Runs on a blocking-pool worker, never a scheduler thread. Returns the first
/// address the resolver yields.
pub(super) fn resolve_host(host: &str) -> std::io::Result<std::net::IpAddr> {
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

/// Lock a mutex, recovering the data if a holder thread died (the VM never
/// panics, so poisoning is effectively unreachable).
pub(super) fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match m.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// How many schedulers to run: `AL_SCHEDULERS` env override, else one per CPU
/// core.
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
