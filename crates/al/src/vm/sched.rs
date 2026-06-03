//! The multi-core scheduler runtime.
//!
//! The first `spawn()` in a program boots one scheduler per CPU core: the main
//! thread becomes scheduler 0 and the rest are OS threads owned by the
//! runtime. A spawned process starts life as a [`Seed`] — an owned mini-heap
//! holding a deep copy of the closure graph plus the root `Value` pointing
//! into it — pushed onto a shared injector queue or handed to an idle
//! scheduler. Whichever scheduler takes it adopts the heap as the child
//! process's initial young space: the arena moves as a unit, nothing is
//! rebuilt on the receiving side. A process's heap is only ever touched by
//! the scheduler currently running it.
//!
//! Idle schedulers park inside their OS poller; submitting a seed (or the last
//! process finishing) wakes them via [`polling::Poller::notify`].

use std::collections::{HashMap, VecDeque};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

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
    /// The child's initial young space: a fresh mini-arena sized to the
    /// spawned closure's graph, filled by `copy_graph` spawn-side (sharing
    /// preserved via forwarding pointers; `Binary` `Arc` backings are shared
    /// zero-copy — only the arena box is copied, never the bytes).
    pub heap: ProcHeap,
    /// The spawned closure, as a pointer into `heap`.
    pub root: Value,
    /// Listening sockets the closure captured (dup'd file descriptors —
    /// the spawner keeps accepting too).
    pub listeners: Vec<(i32, TcpListener)>,
    /// Connections the closure captured (moved — the spawner loses them).
    pub connections: Vec<(i32, TcpStream)>,
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

/// State shared by every scheduler in the program.
pub(super) struct Runtime {
    /// The shared program; each worker scheduler runs a private clone of it
    /// (constants are frozen words pointing into `program.frozen`, shared).
    pub program: Arc<Program>,
    /// The program's global (literal) area: top-level bindings as frozen
    /// value words,
    /// published once when main writes them. Workers copy the words into
    /// their own globals table, gated by `globals_version`.
    pub globals: Mutex<Vec<Option<FrozenValue>>>,
    /// Bumped on every publish; lets schedulers skip syncing when nothing
    /// changed.
    pub globals_version: AtomicU64,
    /// Listening sockets shared program-wide, so a listener stored in a
    /// top-level binding works from any scheduler: each scheduler that needs
    /// one dups the fd from here into its own table.
    pub shared_listeners: Mutex<HashMap<i32, TcpListener>>,
    /// Per-scheduler work inboxes. `submit` hands a seed directly to a chosen
    /// idle scheduler's inbox so an already-running scheduler can never race
    /// it away from the one that was woken for it; a migrating process is
    /// handed to a claimed idle peer the same way.
    pub inboxes: Vec<Mutex<VecDeque<Inbound>>>,
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
    /// One OS poller per scheduler. `notify()` wakes that scheduler whether it
    /// is parked on I/O, on a timer, or waiting for work.
    pub pollers: Vec<Arc<polling::Poller>>,
    /// Which schedulers are currently parked (idle or waiting on I/O).
    pub parked_flags: Vec<AtomicBool>,
    /// Per-scheduler published load: how many runnable processes (running +
    /// queued) each scheduler last reported, plus one for every directed
    /// inbound (donated migrant or direct-handed seed) still in flight to it.
    /// Donation reads these to find the least-loaded peer, so queue-length
    /// imbalances are leveled while every scheduler is busy — an overloaded
    /// scheduler must not wait for a peer to drain completely before any
    /// work moves.
    ///
    /// Advisory, not authoritative: each scheduler overwrites its own slot
    /// (`publish_load`) every time its runnable count changes, after draining
    /// its inbox, so in-flight bumps are folded into the owner's next report.
    /// A stale read can only mistune one donation decision, and the next
    /// yield self-corrects, so plain relaxed loads/stores suffice.
    pub run_lens: Vec<AtomicUsize>,
    /// Worker threads, joined by scheduler 0 at shutdown.
    pub workers: Mutex<Vec<std::thread::JoinHandle<()>>>,
    /// Elastic worker pool for blocking syscalls (file I/O, …) that must not
    /// run on a scheduler thread.
    blocking: BlockingPool,
    /// Per-scheduler completion queues: a blocking worker pushes a finished job
    /// here and `notify()`s that scheduler's poller to deliver the result.
    pub completions: Vec<Mutex<VecDeque<Completion>>>,
}

impl Runtime {
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
    pub fn submit(&self, seed: Seed) {
        self.live.fetch_add(1, Ordering::AcqRel);

        // Prefer a parked (idle) scheduler, starting the scan at a rotating
        // offset so back-to-back submissions land on different schedulers.
        let n = self.parked_flags.len();
        let start = self.submit_cursor.fetch_add(1, Ordering::Relaxed);
        for off in 0..n {
            let i = (start + off) % n;
            if self.parked_flags[i]
                .compare_exchange(true, false, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                self.run_lens[i].fetch_add(1, Ordering::Relaxed);
                lock(&self.inboxes[i]).push_back(Inbound::Seed(seed));
                let _ = self.pollers[i].notify();
                return;
            }
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

    /// Claim one idle scheduler for a direct handoff: scan `parked_flags`
    /// from a rotating offset (so back-to-back placements fan out across
    /// schedulers) and CAS the first set flag `true -> false` — the same
    /// pattern as [`Runtime::submit`]. A claimed scheduler is invisible to
    /// every other submitter and donor (its flag is down), so whatever is
    /// pushed into its inbox next cannot be raced away by another placement
    /// decision; it keeps sleeping until notified.
    ///
    /// Two protocol invariants for callers:
    ///
    /// - A successful claim MUST be followed by a `notify` of the claimed
    ///   scheduler's poller — by handing it work ([`Runtime::donate`]) or,
    ///   when the handoff falls through, by [`Runtime::abort_donation`]. A
    ///   claimed-but-never-notified scheduler sleeps with its flag down,
    ///   unwakeable by submitters, until unrelated I/O happens to fire on
    ///   its poller.
    /// - Donors claim BEFORE detaching the victim process's fds. The detach
    ///   has side effects (it moves connection fds out of the donor's
    ///   tables), so it must not start until a destination is guaranteed;
    ///   the local eligibility checks, which are cheap and effect-free, run
    ///   before the claim.
    pub fn claim_idle_peer(&self) -> Option<usize> {
        let n = self.parked_flags.len();
        let start = self.submit_cursor.fetch_add(1, Ordering::Relaxed);
        for off in 0..n {
            let i = (start + off) % n;
            if self.parked_flags[i]
                .compare_exchange(true, false, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return Some(i);
            }
        }
        None
    }

    /// Find the least-loaded peer worth donating to when no scheduler is
    /// idle: the one whose published load ([`Runtime::run_lens`]) is
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
    pub fn pick_underloaded_peer(&self, me: usize, my_load: usize) -> Option<usize> {
        let n = self.run_lens.len();
        let start = self.submit_cursor.fetch_add(1, Ordering::Relaxed);
        let mut best: Option<(usize, usize)> = None;
        for off in 0..n {
            let i = (start + off) % n;
            if i == me {
                continue;
            }
            let len = self.run_lens[i].load(Ordering::Relaxed);
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
    /// own report rather than double-counted.
    pub fn publish_load(&self, me: usize, len: usize) {
        self.run_lens[me].store(len, Ordering::Relaxed);
    }

    /// Hand a migrating process to `peer` — an idle scheduler previously
    /// claimed via [`Runtime::claim_idle_peer`], or a busy one chosen by
    /// [`Runtime::pick_underloaded_peer`] — and wake it. The wake matters in
    /// both cases: a claimed peer is asleep, and a busy peer may have parked
    /// on I/O between being chosen and the push (`notify` is sticky, so the
    /// worst case for a peer that stayed busy is one spurious poll wakeup).
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
        self.run_lens[peer].fetch_add(1, Ordering::Relaxed);
        lock(&self.inboxes[peer]).push_back(Inbound::Migrant(m));
        let _ = self.pollers[peer].notify();
    }

    /// A donation fell through after `peer` was already claimed (the victim's
    /// fd detach failed, e.g. it references a listener fd that cannot be
    /// dup'd).
    /// Notify the claimed peer anyway: it wakes, finds nothing, re-parks,
    /// and republishes its parked flag. Skipping this would leave the peer
    /// asleep with its flag down — invisible to every future submit/donate.
    pub fn abort_donation(&self, peer: usize) {
        let _ = self.pollers[peer].notify();
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

    /// Drain scheduler `me`'s inbox: work that was explicitly placed here —
    /// seeds direct-handed by `submit`, migrants donated by peers. Directed
    /// work is taken unconditionally (a busy scheduler drains its inbox at
    /// every yield); only the undirected overflow queue is subject to the
    /// pickup limit.
    pub fn take_directed(&self, me: usize) -> Vec<Inbound> {
        lock(&self.inboxes[me]).drain(..).collect()
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
        let n = self.inboxes.len();
        for off in 1..n {
            let i = (me + off) % n;
            if let Some(inbound) = lock(&self.inboxes[i]).pop_front() {
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
        self.parked_flags
            .iter()
            .enumerate()
            .any(|(i, flag)| i != me && flag.load(Ordering::Acquire))
    }

    /// Publish a top-level binding's frozen root to the shared global area.
    /// This is not at-most-once per slot: `Op::StoreLocal` republishes on
    /// every entry-frame store, and binary-pattern cursor temps and
    /// or-pattern alternative bindings store the same slot several times.
    /// All of a slot's stores happen inside the single top-level statement
    /// that owns it, before any closure that could read the slot exists, so
    /// each readable slot reaches its final published value before any
    /// reader can observe it. Runtime boot also replays the table once, for
    /// bindings stored before the runtime existed; those words are already
    /// frozen, so the replay copies nothing.
    ///
    /// The caller has already deep-copied the value graph into the frozen
    /// area (`freeze::freeze_global` — the shared `copy_graph` with the
    /// frozen builder as destination), so `frozen` is an immediate or points
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
        for (i, flag) in self.parked_flags.iter().enumerate() {
            if flag
                .compare_exchange(true, false, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                let _ = self.pollers[i].notify();
                return;
            }
        }
    }

    /// Wake every scheduler.
    fn wake_all(&self) {
        for poller in &self.pollers {
            let _ = poller.notify();
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
        if std::thread::Builder::new()
            .name("al-blocking".into())
            .spawn(move || blocking_worker_main(rt))
            .is_err()
        {
            // Couldn't spawn: undo the count and let an existing worker take it.
            self.blocking.total.fetch_sub(1, Ordering::AcqRel);
            self.blocking.cond.notify_one();
        }
    }

    /// Route a finished blocking job back to its scheduler and wake it.
    pub fn deliver_completion(&self, origin: usize, c: Completion) {
        lock(&self.completions[origin]).push_back(c);
        let _ = self.pollers[origin].notify();
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

/// Create the runtime and start `count - 1` worker scheduler threads.
/// `scheduler0_poller` becomes the poller for the calling thread (scheduler 0)
/// so any I/O it has already registered keeps working.
pub(super) fn boot(
    program: Arc<Program>,
    count: usize,
    scheduler0_poller: Arc<polling::Poller>,
) -> std::io::Result<Arc<Runtime>> {
    let mut pollers = Vec::with_capacity(count);
    pollers.push(scheduler0_poller);
    for _ in 1..count {
        pollers.push(Arc::new(polling::Poller::new()?));
    }

    let runtime = Arc::new(Runtime {
        program,
        globals: Mutex::new(Vec::new()),
        globals_version: AtomicU64::new(0),
        shared_listeners: Mutex::new(HashMap::new()),
        inboxes: (0..count).map(|_| Mutex::new(VecDeque::new())).collect(),
        injector: Mutex::new(VecDeque::new()),
        submit_cursor: AtomicUsize::new(0),
        // The main process is live.
        live: AtomicUsize::new(1),
        blocking: BlockingPool::new(),
        completions: (0..count).map(|_| Mutex::new(VecDeque::new())).collect(),
        pollers,
        // Workers start flagged idle — they are about to be — so that seeds
        // submitted while they boot are handed straight to them rather than
        // hoarded by the schedulers that are already running.
        parked_flags: (0..count).map(|i| AtomicBool::new(i != 0)).collect(),
        // Scheduler 0 is about to run the main process; workers start empty.
        run_lens: (0..count)
            .map(|i| AtomicUsize::new(usize::from(i == 0)))
            .collect(),
        workers: Mutex::new(Vec::new()),
    });

    let mut handles = Vec::with_capacity(count.saturating_sub(1));
    for index in 1..count {
        let rt = Arc::clone(&runtime);
        let handle = std::thread::Builder::new()
            .name(format!("al-scheduler-{index}"))
            .spawn(move || super::worker_main(rt, index))?;
        handles.push(handle);
    }
    *lock(&runtime.workers) = handles;

    Ok(runtime)
}
