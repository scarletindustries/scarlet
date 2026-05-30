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
use std::sync::{Arc, Mutex};

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
    /// Spawned processes waiting for a scheduler to take them.
    pub injector: Mutex<VecDeque<Seed>>,
    /// Live processes across all schedulers, counting injector seeds and the
    /// main process. The program is over when this reaches zero.
    pub live: AtomicUsize,
    /// One OS poller per scheduler. `notify()` wakes that scheduler whether it
    /// is parked on I/O, on a timer, or waiting for work.
    pub pollers: Vec<Arc<polling::Poller>>,
    /// Which schedulers are currently parked (idle or waiting on I/O).
    pub parked_flags: Vec<AtomicBool>,
    /// Worker threads, joined by scheduler 0 at shutdown.
    pub workers: Mutex<Vec<std::thread::JoinHandle<()>>>,
}

impl Runtime {
    /// Submit a seed for any scheduler to run, and wake one to take it.
    pub fn submit(&self, seed: Seed) {
        self.live.fetch_add(1, Ordering::AcqRel);
        {
            let mut q = lock(&self.injector);
            q.push_back(seed);
        }
        self.wake_one();
    }

    /// Take up to [`SEED_BATCH`] seeds from the injector.
    pub fn take_seeds(&self) -> Vec<Seed> {
        let mut q = lock(&self.injector);
        let take = q.len().min(SEED_BATCH);
        q.drain(..take).collect()
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
        injector: Mutex::new(VecDeque::new()),
        // The main process is live.
        live: AtomicUsize::new(1),
        pollers,
        // Workers start flagged idle — they are about to be — so that seeds
        // submitted while they boot are left for them rather than hoarded by
        // the schedulers that are already running.
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
