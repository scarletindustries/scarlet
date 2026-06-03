//! Parking and wake-up: how a blocked process sleeps and what brings it
//! back.
//!
//! When an opcode cannot make progress (an `accept` with no pending
//! connection, a `read` on an empty socket, a `sleep`, a blocking-pool
//! offload), it returns `Step::Parked` carrying a [`Wait`] — the complete
//! description of what the process is waiting for. The scheduler stashes
//! the suspended process under a fresh wait id and this module takes over:
//! it arms the OS poller (kqueue/epoll via `polling`) with the wait's fd
//! [`Interest`]s, records its deadline in the VM's lazy-deletion timer
//! heap, and on each scheduling pause delivers whatever became ready —
//! I/O events, due timers, blocking-pool completions — back onto the run
//! queue ([`VM::poll_parked`]).
//!
//! Invariants this module maintains:
//!
//! - **A wait wakes exactly once.** Whichever of its conditions fires
//!   first removes the park; later events for sibling fds find nothing,
//!   and the wait's other still-armed interests are explicitly released
//!   (the poller is oneshot, so only the fd that fired auto-disarmed).
//! - **Timer entries are lazily deleted.** A park with a deadline pushes
//!   one `(deadline, id)` entry and never removes it eagerly; a popped
//!   entry whose id is gone (or re-keyed) is discarded. The nearest live
//!   deadline is therefore an O(log n) peek, and stale entries cost one
//!   pop each.
//! - **Wake-time construction runs in the woken process's context.** A
//!   completion or finished connect builds its result value in that
//!   process's own arena with its own stack/frames as GC roots —
//!   `drain_completions`/`drain_io_events` swap the process in around the
//!   construction, exactly like a context switch.
//! - **Registered fds outlive their registration.** Sockets stay in the
//!   VM's tables while armed; close deregisters before dropping.
//!
//! [`EPOCH`] also lives here: the process-global monotonic origin that
//! `Op::Monotonic` readings and `socket.read_within` deadlines share, so
//! an absolute deadline means the same instant on every scheduler.

use std::cmp::Reverse;
use std::io::ErrorKind;
use std::net::TcpStream;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use al_core::bytecode::Value;
use smallvec::{SmallVec, smallvec};

use super::sched::{BlockingOp, BlockingResult, Completion};
use super::{VM, VmResult, cost, sched};
use crate::stdlib;

/// A socket-readiness condition a parked process can wait on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Interest {
    /// The socket (listener or connection) must become readable.
    Readable,
    /// The connection must become writable.
    Writable,
}

/// How a parked process resumes once its `Wait` is satisfied.
#[derive(Debug, Clone, Copy)]
pub(super) enum WakeAction {
    /// Resume wherever the op left `ip`. Covers both the syscall-retry waits
    /// (accept/read/write set `ip - 1` to re-run) and `Sleep` (which leaves
    /// `ip` at the next instruction with its result already on the stack).
    Rerun,
    /// Complete the pending non-blocking connect on this socket id: on wake the
    /// completion handler pushes the result directly onto the process's stack
    /// instead of re-running the instruction.
    CompleteConnect(i32),
}

/// What a parked process is waiting for: any of a set of socket-readiness
/// interests and/or a deadline. Whichever fires first wakes the process, after
/// which `action` decides how it resumes.
#[derive(Debug)]
pub(super) struct Wait {
    /// `(socket id, condition)` pairs; the process wakes when any one is ready.
    /// Single-fd parks are overwhelmingly the common case, so this stays inline
    /// and allocation-free for them.
    pub(super) interests: SmallVec<[(i32, Interest); 1]>,
    /// Wake no later than this instant, if set. A `Wait` with no interests and
    /// a deadline is a pure timer (e.g. `Sleep`).
    pub(super) deadline: Option<Instant>,
    /// How the process resumes once woken.
    action: WakeAction,
    /// A blocking op to hand to the pool when this park is registered (so the
    /// job id matches the wait id). The process wakes when its completion is
    /// delivered; until then it has no interests and no deadline, so only a
    /// completion can wake it. `None` for ordinary I/O / timer parks.
    pub(super) offload: Option<BlockingOp>,
}

impl Wait {
    /// Park until socket `id` becomes readable, then re-run the instruction.
    pub(super) fn readable(id: i32) -> Self {
        Wait {
            interests: smallvec![(id, Interest::Readable)],
            deadline: None,
            action: WakeAction::Rerun,
            offload: None,
        }
    }

    /// Park until socket `id` becomes writable, then re-run the instruction.
    pub(super) fn writable(id: i32) -> Self {
        Wait {
            interests: smallvec![(id, Interest::Writable)],
            deadline: None,
            action: WakeAction::Rerun,
            offload: None,
        }
    }

    /// Park until the pending connect on socket `id` completes (signalled by
    /// writability); on wake, finish the connect and push its result.
    pub(super) fn connecting(id: i32) -> Self {
        Wait {
            interests: smallvec![(id, Interest::Writable)],
            deadline: None,
            action: WakeAction::CompleteConnect(id),
            offload: None,
        }
    }

    /// Park until `deadline`, then resume at the next instruction (the result
    /// is already on the stack).
    pub(super) fn until(deadline: Instant) -> Self {
        Wait {
            interests: SmallVec::new(),
            deadline: Some(deadline),
            action: WakeAction::Rerun,
            offload: None,
        }
    }

    /// Park until socket `id` becomes readable or `deadline` passes, whichever
    /// comes first, then re-run the instruction (which re-derives whether data
    /// arrived or the read timed out).
    pub(super) fn read_with_deadline(id: i32, deadline: Instant) -> Self {
        Wait {
            interests: smallvec![(id, Interest::Readable)],
            deadline: Some(deadline),
            action: WakeAction::Rerun,
            offload: None,
        }
    }

    /// Park until the blocking pool finishes a job, with no I/O interest and no
    /// deadline — only the job's completion can wake it. The op is handed to the
    /// pool when the park is registered, so its job id matches the wait id.
    pub(super) fn offloaded(op: BlockingOp) -> Self {
        Wait {
            interests: SmallVec::new(),
            deadline: None,
            action: WakeAction::Rerun,
            offload: Some(op),
        }
    }
}

/// Process-global monotonic epoch, lazily pinned to the first `Instant::now()`
/// any monotonic reading observes so every reading shares one origin. Reused by
/// `Op::Monotonic` and the deadline math behind `socket.read_within`.
pub(super) static EPOCH: OnceLock<Instant> = OnceLock::new();

/// Milliseconds elapsed since the process-global monotonic [`EPOCH`], clamped
/// into `i64`. Saturating rather than panicking on overflow — the epoch would
/// have to predate the process by ~292 million years to exceed `i64::MAX`.
pub(super) fn monotonic_now_ms() -> i64 {
    let ms = EPOCH.get_or_init(Instant::now).elapsed().as_millis();
    ms.min(i64::MAX as u128) as i64
}

/// Arm `src` with the poller. polling 3.x is oneshot, so re-arming an fd that
/// is still registered surfaces as `AlreadyExists` and is upgraded to a
/// `modify`.
///
/// # Safety
///
/// `src` must outlive its poller registration: it must stay alive until it is
/// deleted from the poller (or the poller is dropped).
unsafe fn add_or_modify<S>(
    poller: &polling::Poller,
    src: &S,
    event: polling::Event,
    what: &str,
) -> VmResult<()>
where
    for<'a> &'a S: polling::AsRawSource + polling::AsSource,
{
    // SAFETY: forwarded — the caller guarantees `src` outlives its
    // registration.
    unsafe { poller.add(src, event) }
        .or_else(|e| {
            if e.kind() == ErrorKind::AlreadyExists {
                poller.modify(src, event)
            } else {
                Err(e)
            }
        })
        .map_err(|e| format!("cannot watch {what}: {e}"))
}

impl VM {
    /// Register every fd interest of a `wait` with the OS poller before
    /// parking. A timer-only wait (no interests) registers nothing.
    pub(super) fn register_wait(&mut self, wait: &Wait) -> VmResult<()> {
        if wait.interests.is_empty() {
            return Ok(());
        }
        self.ensure_poller()?;
        if self.poller.is_none() {
            return Ok(());
        }
        for &(id, interest) in &wait.interests {
            self.register_fd(id, interest)?;
        }
        Ok(())
    }

    /// Arm a single `(socket id, interest)` with the poller, looking the fd up
    /// in whichever socket table holds it (see [`add_or_modify`]).
    fn register_fd(&self, id: i32, interest: Interest) -> VmResult<()> {
        let Some(poller) = &self.poller else {
            return Ok(());
        };
        let event = match interest {
            Interest::Readable => polling::Event::readable(id as usize),
            Interest::Writable => polling::Event::writable(id as usize),
        };

        // SAFETY: the fd lives in one of the socket tables; sockets stay in
        // those tables (and thus alive) for as long as they are registered —
        // close deregisters before dropping. That upholds `add_or_modify`'s
        // contract that each source outlives its registration.
        if let Some(listener) = self.tcp_listeners.get(&id) {
            unsafe { add_or_modify(poller, listener, event, "listener") }
        } else if let Some(conn) = self.tcp_connections.get(&id) {
            unsafe { add_or_modify(poller, conn, event, "connection") }
        } else if let Some(pending) = self.pending_connects.get(&id) {
            unsafe { add_or_modify(poller, pending, event, "connecting socket") }
        } else {
            Err("Invalid socket. This is likely a compiler bug.".to_string())
        }
    }

    /// Drop a socket id from the poller, whichever interest it was armed for.
    /// Used to release a woken wait's *other* still-armed fds — oneshot already
    /// disarmed the one that fired. A socket no longer in any table, or never
    /// registered, is silently ignored.
    fn deregister_fd(&self, id: i32) {
        let Some(poller) = &self.poller else {
            return;
        };
        if let Some(listener) = self.tcp_listeners.get(&id) {
            let _ = poller.delete(listener);
        } else if let Some(conn) = self.tcp_connections.get(&id) {
            let _ = poller.delete(conn);
        } else if let Some(pending) = self.pending_connects.get(&id) {
            let _ = poller.delete(pending);
        }
    }

    /// Move parked processes whose I/O is ready or whose timer has expired
    /// back onto the run queue.
    ///
    /// With `block`, performs one interruptible wait — I/O readiness, the
    /// nearest timer deadline, or a `notify()` from another scheduler all end
    /// it — then returns so the caller can re-check for remote work.
    pub(super) fn poll_parked(&mut self, block: bool) -> VmResult<()> {
        if self.parked.is_empty() {
            return Ok(());
        }

        // Deliver any finished blocking-pool jobs, then wake due timers.
        let mut woke = self.drain_completions();
        woke |= self.wake_due_timers();

        let waiting_on_io = self.parked.values().any(|(w, _)| !w.interests.is_empty());

        if !block || woke {
            // Non-blocking (or something already woke): just drain any ready
            // I/O events and return.
            if waiting_on_io {
                self.drain_io_events(Some(Duration::ZERO))?;
            }
            return Ok(());
        }

        // One blocking wait, bounded by the nearest live timer deadline. The
        // poller is used even for timer-only waits so `notify()` can interrupt
        // it. Stale heap tops (ids already woken on I/O) are dropped as we look.
        let next_deadline = loop {
            let Some(&Reverse((deadline, id))) = self.timer_heap.peek() else {
                break None;
            };
            if matches!(self.parked.get(&id), Some((w, _)) if w.deadline == Some(deadline)) {
                break Some(deadline);
            }
            self.timer_heap.pop();
        };
        let timeout = next_deadline.map(|d| d.saturating_duration_since(Instant::now()));
        self.ensure_poller()?;
        self.drain_io_events(timeout)?;
        self.wake_due_timers();
        // A completion notify may be what ended the wait above.
        self.drain_completions();
        Ok(())
    }

    /// Deliver finished blocking-pool jobs: for each completion, resume the
    /// process parked under its `job_id` with the result constructed
    /// scheduler-side into that process's own arena (the rooting rule).
    /// Returns whether anything was woken.
    ///
    /// The woken process is made *current* for the construction, so its heap
    /// is the allocation target and its own stack/frames are the GC roots if
    /// `ensure` collects. Whatever was current when the drain ran — a yielded
    /// process at a `Step::Yield` poll, or the empty placeholder between
    /// slices — is detached around the delivery and restored after.
    pub(super) fn drain_completions(&mut self) -> bool {
        let Some(rt) = self.runtime.clone() else {
            return false;
        };
        let drained: Vec<Completion> = {
            let mut q = sched::lock(&rt.completions[self.scheduler_index]);
            if q.is_empty() {
                return false;
            }
            q.drain(..).collect()
        };
        let mut woke = false;
        for c in drained {
            let Some((_wait, p)) = self.parked.remove(&c.job_id) else {
                continue;
            };
            let interrupted = self.suspend_current();
            self.resume(p);
            let value = self.completion_result(c.result);
            self.stack.push(value);
            let woken = self.suspend_current();
            self.run_queue.push_back(woken);
            self.resume(interrupted);
            woke = true;
        }
        woke
    }

    /// Construct a blocking-pool result in the current process's arena — the
    /// woken process `drain_completions` just installed. A completion carries
    /// only `Send` raw data (bytes, `io::Error`s), never a `Value`, so there
    /// is nothing to root across the safepoint: each arm ensures its whole
    /// result graph up front, then allocates. `make_ok`/`make_err` charge their own
    /// `cost::WRAP`; the inner construction is charged here.
    fn completion_result(&mut self, result: BlockingResult) -> Value {
        match result {
            BlockingResult::ReadFile { path, result } => match result {
                Ok(bytes) => {
                    self.ensure(cost::WRAP + cost::BINARY);
                    let bin = Value::binary_in(&mut self.heap, bytes);
                    self.make_ok(bin)
                }
                Err(e) => {
                    self.ensure(cost::WRAP + cost::io_err(path.len()));
                    let err = self.io_error_value(&e, &path);
                    self.make_err(err)
                }
            },
            BlockingResult::WriteFile { path, result } => match result {
                Ok(()) => {
                    // `make_nil` clones a prebuilt template (frozen-area in
                    // the end state) and allocates nothing; only the wrapper
                    // needs budget.
                    self.ensure(cost::WRAP);
                    let nil = self.make_nil();
                    self.make_ok(nil)
                }
                Err(e) => {
                    self.ensure(cost::WRAP + cost::io_err(path.len()));
                    let err = self.io_error_value(&e, &path);
                    self.make_err(err)
                }
            },
            BlockingResult::ResolveDns { result } => match result {
                Ok(addr) => {
                    self.ensure(cost::WRAP + cost::IP_ADDR);
                    let ip = self.templates.ip_address(&mut self.heap, addr);
                    self.make_ok(ip)
                }
                Err(e) => {
                    self.ensure(cost::WRAP + cost::NET_ERR);
                    let err = self.net_error_value(&e);
                    self.make_err(err)
                }
            },
        }
    }

    /// Wake parked processes whose deadline has passed, draining the
    /// lazy-deletion timer heap. A popped entry whose id is no longer parked —
    /// or whose live deadline no longer matches — was already woken on I/O and
    /// is silently discarded.
    pub(super) fn wake_due_timers(&mut self) -> bool {
        let now = Instant::now();
        let mut woke = false;
        loop {
            let Some(&Reverse((deadline, id))) = self.timer_heap.peek() else {
                break;
            };
            if deadline > now {
                break;
            }
            self.timer_heap.pop();
            if !matches!(self.parked.get(&id), Some((w, _)) if w.deadline == Some(deadline)) {
                // Stale: the park already woke on I/O (or was re-keyed). Drop it.
                continue;
            }
            let Some((wait, p)) = self.parked.remove(&id) else {
                continue;
            };
            // The deadline beat the fds: deregister any interests still armed in
            // the poller (oneshot only auto-disarms fds that actually fired).
            if let Some(poller) = &self.poller {
                for &(sid, _) in &wait.interests {
                    if let Some(l) = self.tcp_listeners.get(&sid) {
                        let _ = poller.delete(l);
                    } else if let Some(c) = self.tcp_connections.get(&sid) {
                        let _ = poller.delete(c);
                    } else if let Some(s) = self.pending_connects.get(&sid) {
                        let _ = poller.delete(s);
                    }
                }
            }
            self.run_queue.push_back(p);
            woke = true;
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
            let key = ev.key as i32;
            // Collect first — a parked entry can't be removed while the map is
            // iterated. Each wait wakes at most once: once removed, a later
            // event for one of its sibling fds finds nothing left to wake.
            let woken: Vec<u64> = self
                .parked
                .iter()
                .filter_map(|(wid, (w, _))| {
                    w.interests
                        .iter()
                        .any(|&(sid, _)| sid == key)
                        .then_some(*wid)
                })
                .collect();
            for wid in woken {
                let Some((wait, p)) = self.parked.remove(&wid) else {
                    continue;
                };
                match wait.action {
                    // A finished connect delivers its result directly onto the
                    // woken process's stack; the connect instruction is not
                    // re-run. The result is built in the woken process's own
                    // context (its arena, its stack and frames as GC roots),
                    // so it is swapped in around the
                    // construction exactly as `drain_completions` swaps in
                    // completion targets.
                    WakeAction::CompleteConnect(sid) => {
                        let interrupted = self.suspend_current();
                        self.resume(p);
                        let result = self.finish_connect(sid);
                        self.stack.push(result);
                        let woken_p = self.suspend_current();
                        self.run_queue.push_back(woken_p);
                        self.resume(interrupted);
                    }
                    WakeAction::Rerun => self.run_queue.push_back(p),
                }
                // Oneshot already disarmed the fd that fired; release the wait's
                // other still-armed interests so they don't fire unwatched.
                for &(sid, _) in &wait.interests {
                    if sid != key {
                        self.deregister_fd(sid);
                    }
                }
            }
        }
        Ok(())
    }

    /// Complete a pending non-blocking connect whose socket became writable:
    /// either adopt the connection or report the error it ended with.
    fn finish_connect(&mut self, id: i32) -> Value {
        // Budget the whole result up front in the (just-resumed) woken
        // process's arena: the adopted Ok(Socket) graph or a NetError.
        self.ensure(cost::ADOPT + cost::NET_ERR);
        let Some(socket) = self.pending_connects.remove(&id) else {
            let aborted = self.stdlib_enum(&stdlib::net::error::CONNECTION_ABORTED);
            return self.make_err(aborted);
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
                    Err(e) => {
                        let err = self.net_error_value(&e);
                        self.make_err(err)
                    }
                }
            }
            Ok(Some(e)) | Err(e) => {
                let err = self.net_error_value(&e);
                self.make_err(err)
            }
        }
    }
}
