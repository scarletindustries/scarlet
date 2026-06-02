//! The multi-core scheduler runtime.
//!
//! The first `spawn()` in a program boots one scheduler per CPU core: the main
//! thread becomes scheduler 0 and the rest are OS threads owned by the
//! runtime. A spawned process starts life as a [`Seed`] — a fully `Send`
//! deep copy of the closure and everything it can see — pushed onto a shared
//! injector queue. Whichever scheduler is free takes it, rebuilds it on its own
//! `Rc` heap, and runs it to completion; once started, a process is pinned to
//! its scheduler (heaps never cross threads).
//!
//! Idle schedulers park inside their OS poller; submitting a seed (or the last
//! process finishing) wakes them via [`polling::Poller::notify`].

use std::collections::{HashMap, VecDeque};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use al_core::bytecode::transfer::{SendProgram, SendValue};

/// How many seeds a scheduler takes from the injector per visit. One at a
/// time maximizes spread: with k seeds and k idle schedulers, every scheduler
/// gets exactly one.
pub(super) const SEED_BATCH: usize = 1;

/// A spawned-but-not-yet-started process in `Send` form: the unit of
/// cross-scheduler work distribution.
pub(super) struct Seed {
    /// The deep-copied closure to run.
    pub closure: SendValue,
    /// Listening sockets the closure captured (dup'd file descriptors —
    /// the spawner keeps accepting too).
    pub listeners: Vec<(i32, TcpListener)>,
    /// Connections the closure captured (moved — the spawner loses them).
    pub connections: Vec<(i32, TcpStream)>,
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
/// scheduler, which turns it into a `Value` on its own `Rc` heap.
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
/// directly without re-running the instruction.
pub(super) struct Completion {
    pub job_id: u64,
    pub result: BlockingResult,
}

/// An elastic pool of OS threads for blocking syscalls (file I/O today, DNS
/// next) that must never run on a scheduler thread. Idle workers park *warm*
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
    /// The program in `Send` form; each scheduler hydrates its own `Rc` copy.
    pub program: Arc<SendProgram>,
    /// The program's global (literal) area in `Send` form: top-level bindings,
    /// published once when main writes them. Workers hydrate from here into
    /// their own `Rc` globals table, gated by `globals_version`.
    pub globals: Mutex<Vec<Option<SendValue>>>,
    /// Bumped on every publish; lets schedulers skip syncing when nothing
    /// changed.
    pub globals_version: AtomicU64,
    /// Listening sockets shared program-wide, so a listener stored in a
    /// top-level binding works from any scheduler: each scheduler that needs
    /// one dups the fd from here into its own table.
    pub shared_listeners: Mutex<HashMap<i32, TcpListener>>,
    /// Per-scheduler seed inboxes. `submit` hands a seed directly to a chosen
    /// idle scheduler's inbox so an already-running scheduler can never race
    /// it away from the one that was woken for it.
    pub inboxes: Vec<Mutex<VecDeque<Seed>>>,
    /// Overflow queue: seeds submitted while every scheduler was busy. Taken
    /// (one per visit) by whichever scheduler frees up first.
    pub injector: Mutex<VecDeque<Seed>>,
    /// Rotates the starting point of `submit`'s idle-scheduler search so
    /// consecutive submissions spread across different schedulers.
    pub submit_cursor: AtomicUsize,
    /// Live processes across all schedulers, counting undelivered seeds and the
    /// main process. The program is over when this reaches zero.
    pub live: AtomicUsize,
    /// One OS poller per scheduler. `notify()` wakes that scheduler whether it
    /// is parked on I/O, on a timer, or waiting for work.
    pub pollers: Vec<Arc<polling::Poller>>,
    /// Which schedulers are currently parked (idle or waiting on I/O).
    pub parked_flags: Vec<AtomicBool>,
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
                lock(&self.inboxes[i]).push_back(seed);
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

    /// Take the seeds destined for scheduler `me`: everything handed to its
    /// inbox, plus (when the inbox is empty) up to [`SEED_BATCH`] from the
    /// shared overflow queue.
    pub fn take_seeds(&self, me: usize) -> Vec<Seed> {
        let mut out: Vec<Seed> = {
            let mut inbox = lock(&self.inboxes[me]);
            inbox.drain(..).collect()
        };
        if out.is_empty() {
            let mut q = lock(&self.injector);
            let take = q.len().min(SEED_BATCH);
            out.extend(q.drain(..take));
        }
        out
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

    /// Publish a top-level binding to the shared global area. Globals are
    /// write-once, so each slot is published at most once (plus once at boot
    /// for bindings that existed before the runtime did).
    pub fn publish_global(&self, slot: usize, value: SendValue) {
        {
            let mut g = lock(&self.globals);
            if g.len() <= slot {
                g.resize_with(slot + 1, || None);
            }
            g[slot] = Some(value);
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
    program: Arc<SendProgram>,
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
