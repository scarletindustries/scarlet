//! Parking and wake-up: how a blocked process sleeps and what brings it
//! back.
//!
//! When an opcode cannot make progress (an `accept` with no pending
//! connection, a `read` on an empty socket, a `sleep`, a blocking-pool
//! offload), it returns `Step::Parked` carrying a [`Wait`] — the complete
//! description of what the process is waiting for. The scheduler stashes
//! the suspended process under a fresh wait id, records its deadline (if
//! any) in the VM's lazy-deletion timer heap, and on each scheduling pause
//! this module delivers whatever became ready — I/O events, due timers,
//! blocking-pool completions — back onto the run queue
//! ([`VM::poll_parked`]).
//!
//! Parking arms nothing: every socket is registered with the OS poller
//! (kqueue/epoll via `mio`) once, when it enters this scheduler's tables
//! (`track_listener`/`track_connection`/`track_pending`), with every
//! interest it can ever park on, and stays registered until it leaves
//! them. The poller is edge-triggered, so a registration with no parked
//! wait behind it costs nothing in steady state; its events are dropped
//! by the drain.
//!
//! Edge-triggering loses no wakeups because of PARK-AFTER-PROBE: **for
//! every fd, a park on it is immediately preceded, on this same OS thread,
//! by a syscall on it that returned `WouldBlock`, with no `Poll::poll` on
//! this thread's poller in between — and this poller is drained only by
//! this thread.** The kernel's ready list is the latch: an edge that fires
//! after the probe sits in this poller until the drain, which runs strictly
//! after the park is registered. That sequencing is the whole proof. (It is
//! NOT the case that a dropped edge is "re-announced at the next
//! transition" — kernels re-sample level at report time, so a dropped
//! event's readiness may never be announced again. Do not add a
//! `poll_parked` call between a `WouldBlock` and its park, and do not let
//! any other thread drain this poller: either edit re-opens the classic
//! lost-wakeup, which Go/Tokio need a per-fd readiness latch to close.)
//!
//! Invariants this module maintains:
//!
//! - **A wait wakes exactly once.** Whichever of its conditions fires
//!   first (fd readiness or deadline) removes the park.
//! - **Timer entries are lazily deleted.** A park with a deadline pushes
//!   one `(deadline, id)` entry and never removes it eagerly; a popped
//!   entry whose id is gone (or re-keyed) is discarded. The nearest live
//!   deadline is therefore an O(log n) peek, and stale entries cost one
//!   pop each.
//! - **Wake-time construction runs in the woken process's context.** A
//!   completion or finished connect builds its result value in that
//!   process's own heap with its own stack/frames as roots —
//!   `drain_completions`/`drain_io_events` swap the process in around the
//!   construction, exactly like a context switch.
//! - **Registered fds outlive their registration.** Sockets stay in the
//!   VM's tables while armed; close deregisters before dropping.
//!
//! [`EPOCH`] also lives here: the process-global monotonic origin that
//! `Op::Monotonic` readings and `socket.read_within` deadlines share, so
//! an absolute deadline means the same instant on every scheduler.

use std::cmp::Reverse;
use std::io::{self, ErrorKind};
use std::net::{TcpListener, TcpStream};
use std::os::fd::{AsRawFd, RawFd};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use mio::Token;
use mio::unix::SourceFd;

use al_core::bytecode::Value;

use super::sched::{BlockingOp, BlockingResult, Completion};
use super::{Process, VM, VmError, VmResult, lock};
use crate::stdlib;

/// How a parked I/O wait resumes once one of its sockets is ready.
#[derive(Debug, Clone, Copy)]
pub(super) enum WakeAction {
    /// Resume wherever the op left `ip`. Covers both the syscall-retry waits
    /// (accept/read/write set `ip - 1` to re-run) and `Sleep` (which leaves
    /// `ip` at the next instruction with its result already on the stack).
    Rerun,
    /// Complete the pending non-blocking connect on the wait's fd: on wake
    /// `VM::finish_connect` builds the result and pushes it directly onto the
    /// process's stack instead of re-running the instruction.
    CompleteConnect,
}

/// What a parked process is waiting for. The variants are the exhaustive set
/// of wake sources — a wait is exactly one of them, so an offload can never
/// carry socket interests or a deadline, and a pure timer never has fds.
#[derive(Debug)]
pub(super) enum Wait {
    /// Socket readiness on `fd`, optionally bounded by a deadline; whichever
    /// fires first wakes the process, and `action` says how it resumes.
    Io {
        fd: i32,
        deadline: Option<Instant>,
        action: WakeAction,
    },
    /// A pure timer: wake at this instant, resume at the next instruction.
    Timer(Instant),
    /// A blocking-pool job. The op is `Some` on the way into [`VM::park`],
    /// which hands it to the pool keyed by the fresh wait id and stores the
    /// spent `None` as the wake marker; only the job's completion can wake it.
    Offload(Option<BlockingOp>),
}

impl Wait {
    /// Park until socket `id` becomes ready, then re-run the instruction.
    /// Interest direction was fixed when the socket was registered with the
    /// poller; the wait itself only names the fd.
    pub(super) fn rerun_on(id: i32) -> Self {
        Wait::Io {
            fd: id,
            deadline: None,
            action: WakeAction::Rerun,
        }
    }

    /// Park until socket `id` becomes readable, then re-run the instruction.
    #[inline]
    pub(super) fn readable(id: i32) -> Self {
        Self::rerun_on(id)
    }

    /// Park until socket `id` becomes writable, then re-run the instruction.
    #[inline]
    pub(super) fn writable(id: i32) -> Self {
        Self::rerun_on(id)
    }

    /// Park until the pending connect on socket `id` completes (signalled by
    /// writability); on wake, finish the connect and push its result.
    pub(super) fn connecting(id: i32) -> Self {
        Wait::Io {
            fd: id,
            deadline: None,
            action: WakeAction::CompleteConnect,
        }
    }

    /// Park until `deadline`, then resume at the next instruction (the result
    /// is already on the stack).
    pub(super) fn until(deadline: Instant) -> Self {
        Wait::Timer(deadline)
    }

    /// Park until socket `id` becomes readable or `deadline` passes, whichever
    /// comes first, then re-run the instruction (which re-derives whether data
    /// arrived or the read timed out).
    pub(super) fn read_with_deadline(id: i32, deadline: Instant) -> Self {
        Wait::Io {
            fd: id,
            deadline: Some(deadline),
            action: WakeAction::Rerun,
        }
    }

    /// Park until the blocking pool finishes a job, with no I/O interest and no
    /// deadline — only the job's completion can wake it. The op is handed to the
    /// pool when the park is registered, so its job id matches the wait id.
    pub(super) fn offloaded(op: BlockingOp) -> Self {
        Wait::Offload(Some(op))
    }

    /// The instant this wait must wake by, if it has one. Timer-heap entries
    /// are validated against this to discard stale ones.
    fn deadline(&self) -> Option<Instant> {
        match self {
            Wait::Io { deadline, .. } => *deadline,
            Wait::Timer(d) => Some(*d),
            Wait::Offload(_) => None,
        }
    }
}

/// The token every scheduler's [`mio::Waker`] registers under: `notify()`
/// events carry it, and the event drains skip it — a wake delivers no I/O,
/// it only ends the wait. Socket ids are small positive `i32`s, so the
/// token can never collide with a socket registration.
pub(super) const WAKER_TOKEN: Token = Token(usize::MAX);

/// Event-buffer capacity for one poll call, same for the parked-I/O drain
/// and the idle wait.
pub(super) const EVENTS_CAPACITY: usize = 1024;

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

impl VM {
    /// Register `fd` with the poller under socket `id`, with every interest
    /// the socket can ever park on. Registration is per-socket, not per-park:
    /// it is made once when the fd enters this scheduler's tables and lives
    /// until the fd leaves them ([`VM::poller_deregister`]); parking arms
    /// nothing. An fd number that is somehow still registered (a close that
    /// bypassed deregistration, then kernel fd reuse) is re-registered under
    /// the new id.
    fn poller_register(&self, fd: RawFd, id: i32, interests: mio::Interest) -> io::Result<()> {
        let registry = self.poll.registry();
        registry
            .register(&mut SourceFd(&fd), Token(id as usize), interests)
            .or_else(|e| {
                if e.kind() == ErrorKind::AlreadyExists {
                    registry.reregister(&mut SourceFd(&fd), Token(id as usize), interests)
                } else {
                    Err(e)
                }
            })
    }

    /// Drop `fd` from the poller. An fd that was never registered (or is
    /// already deregistered) is silently ignored. Paths that close or hand
    /// off a socket call this before dropping it so a stale registration
    /// never outlives its fd.
    pub(super) fn poller_deregister(&self, fd: RawFd) {
        let _ = self.poll.registry().deregister(&mut SourceFd(&fd));
    }

    /// Register the shared listener with this scheduler's poller and remember
    /// the clone. Idempotent per scheduler: an id already present keeps its
    /// existing entry — it is the same `Arc`, the same fd, the same
    /// registration. Each scheduler's poller holds its own independent
    /// readiness for the shared fd, so an accept parked here wakes here.
    pub(super) fn track_listener(
        &mut self,
        id: i32,
        listener: std::sync::Arc<TcpListener>,
    ) -> io::Result<()> {
        if self.tcp_listeners.contains_key(&id) {
            return Ok(());
        }
        self.poller_register(listener.as_raw_fd(), id, mio::Interest::READABLE)?;
        self.tcp_listeners.insert(id, listener);
        Ok(())
    }

    /// Adopt a connection into this scheduler's table, watched for both
    /// readability and writability — the interests its reads and writes can
    /// park on. On registration failure the connection is dropped (closed),
    /// never tabled: a socket that cannot wake its parks must not exist.
    pub(super) fn track_connection(
        &mut self,
        id: i32,
        conn: TcpStream,
        owner: u64,
    ) -> io::Result<()> {
        self.poller_register(
            conn.as_raw_fd(),
            id,
            mio::Interest::READABLE | mio::Interest::WRITABLE,
        )?;
        self.tcp_connections.insert(
            id,
            super::Conn {
                stream: conn,
                owner,
            },
        );
        Ok(())
    }

    /// Adopt an in-progress non-blocking connect, watched for the
    /// writability that signals completion. Failure drops the socket,
    /// exactly as [`VM::track_connection`].
    pub(super) fn track_pending(&mut self, id: i32, socket: socket2::Socket) -> io::Result<()> {
        self.poller_register(socket.as_raw_fd(), id, mio::Interest::WRITABLE)?;
        self.pending_connects.insert(id, socket);
        Ok(())
    }

    /// Stash a suspended process under a fresh wait id and register every side
    /// effect its `Wait` implies: I/O fds are indexed in `io_waiters` (so an
    /// event finds its waiters by lookup, not a scan), a deadline is pushed
    /// onto the lazy-deletion timer heap, and an offload is dispatched to the
    /// blocking pool keyed by the same id. Returns the allocated wait id. This
    /// is the sole entry into `parked`, so those side effects cannot be
    /// forgotten by a caller.
    pub(super) fn park(&mut self, mut wait: Wait, p: Process) -> u64 {
        let id = self.next_wait_id;
        self.next_wait_id += 1;
        match &mut wait {
            Wait::Io { fd, deadline, .. } => {
                let waiters = self.io_waiters.entry(*fd).or_default();
                if !waiters.contains(&id) {
                    waiters.push(id);
                }
                if let Some(d) = *deadline {
                    self.timer_heap.push(Reverse((d, id)));
                }
            }
            Wait::Timer(d) => self.timer_heap.push(Reverse((*d, id))),
            Wait::Offload(op) => {
                // Invariant: `Wait::offloaded` is the only constructor and always
                // yields `Some`; `None` here would mean no job is dispatched and
                // the process hangs forever, so fail loudly rather than swallow.
                #[allow(clippy::expect_used)]
                let op = op.take().expect("offload park carries an op");
                self.runtime.offload(self.scheduler_index, id, op);
            }
        }
        self.parked.insert(id, (wait, p));
        id
    }

    /// Remove a park, dropping its entries from the socket reverse index.
    /// Every wake path goes through here so `io_waiters` stays in lockstep
    /// with `parked`.
    pub(super) fn park_remove(&mut self, id: u64) -> Option<(Wait, Process)> {
        let (wait, p) = self.parked.remove(&id)?;
        if let Wait::Io { fd, .. } = &wait
            && let Some(waiters) = self.io_waiters.get_mut(fd)
        {
            waiters.retain(|w| *w != id);
            if waiters.is_empty() {
                self.io_waiters.remove(fd);
            }
        }
        Some((wait, p))
    }

    /// Wake a parked process with a value built in its own context: swap `p`
    /// in as current so `build` allocates in its arena with its stack/frames
    /// as GC roots, push the result, then queue it and restore whatever was
    /// current before. The one place the "wake-time construction runs in the
    /// woken process's context" invariant is enforced.
    fn wake_with(&mut self, p: Process, build: impl FnOnce(&mut Self) -> Value) {
        let interrupted = self.suspend_current();
        self.resume(p);
        let value = build(self);
        self.stack.push(value);
        let woken = self.suspend_current();
        self.run_queue.push_back(woken);
        self.resume(interrupted);
    }

    /// Move parked processes whose I/O is ready or whose timer has expired
    /// back onto the run queue.
    ///
    /// With `block`, performs one interruptible wait — I/O readiness, the
    /// nearest timer deadline, or a `notify()` from another scheduler all end
    /// it — then returns so the caller can re-check for remote work.
    pub(super) fn poll_parked(&mut self, block: bool) -> VmResult<()> {
        // Before the parked-empty early-out: retiring a listener matters even
        // with nothing parked locally (this scheduler's registration and Arc
        // clone must go so the shared fd can actually close).
        let retired_woke = self.process_retired_listeners();
        if self.parked.is_empty() {
            return Ok(());
        }

        // Deliver any finished blocking-pool jobs, then wake due timers. A
        // retire wake counts: it put a runnable process on `run_queue`, and a
        // blocking wait here would strand it behind an idle poller.
        let mut woke = retired_woke | self.drain_completions();
        woke |= self.wake_due_timers();

        // The reverse index is non-empty exactly when some park has a socket
        // interest, so this check is O(1) rather than a scan of every park.
        let waiting_on_io = !self.io_waiters.is_empty();

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
            if matches!(self.parked.get(&id), Some((w, _)) if w.deadline() == Some(deadline)) {
                break Some(deadline);
            }
            self.timer_heap.pop();
        };
        let timeout = next_deadline.map(|d| d.saturating_duration_since(Instant::now()));
        self.drain_io_events(timeout)?;
        self.wake_due_timers();
        // A completion notify may be what ended the wait above.
        self.drain_completions();
        Ok(())
    }

    /// Deliver finished blocking-pool jobs: for each completion, resume the
    /// process parked under its `job_id` with the result constructed
    /// scheduler-side into that process's own heap.
    /// Returns whether anything was woken.
    ///
    /// The woken process is made *current* for the construction, so its heap
    /// is the allocation target. Whatever was current when the drain ran — a
    /// yielded process at a `Step::Yield` poll, or the empty placeholder
    /// between slices — is detached around the delivery and restored after.
    pub(super) fn drain_completions(&mut self) -> bool {
        let drained: Vec<Completion> = {
            let mut q = lock(&self.runtime.slots[self.scheduler_index].completions);
            if q.is_empty() {
                return false;
            }
            q.drain(..).collect()
        };
        let mut woke = false;
        for c in drained {
            let Some((_wait, p)) = self.park_remove(c.job_id) else {
                continue;
            };
            self.wake_with(p, |vm| vm.completion_result(c.result));
            woke = true;
        }
        woke
    }

    /// Construct a blocking-pool result in the current process's heap — the
    /// woken process `drain_completions` just installed. A completion carries
    /// only `Send` raw data (bytes, `io::Error`s), never a `Value`.
    fn completion_result(&mut self, result: BlockingResult) -> Value {
        match result {
            BlockingResult::ReadFile { path, result } => match result {
                Ok(bytes) => {
                    let bin = Value::binary_in(&mut self.heap, bytes);
                    self.make_ok(bin)
                }
                Err(e) => {
                    let err = self.io_error_value(&e, &path);
                    self.make_err(err)
                }
            },
            BlockingResult::WriteFile { path, result } => match result {
                Ok(()) => {
                    let nil = self.make_nil();
                    self.make_ok(nil)
                }
                Err(e) => {
                    let err = self.io_error_value(&e, &path);
                    self.make_err(err)
                }
            },
            BlockingResult::ResolveDns { result } => match result {
                Ok(addr) => {
                    let ip = self.templates.ip_address(&mut self.heap, addr);
                    self.make_ok(ip)
                }
                Err(e) => {
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
        while let Some(&Reverse((deadline, id))) = self.timer_heap.peek() {
            if deadline > now {
                break;
            }
            self.timer_heap.pop();
            if !matches!(self.parked.get(&id), Some((w, _)) if w.deadline() == Some(deadline)) {
                // Stale: the park already woke on I/O (or was re-keyed). Drop it.
                continue;
            }
            let Some((_wait, p)) = self.park_remove(id) else {
                continue;
            };
            // The deadline beat the fd; its registration belongs to the
            // socket, not this wait, so there is nothing to disarm.
            self.run_queue.push_back(p);
            woke = true;
        }
        woke
    }

    /// Wait on the poller for at most `timeout` (None = until something
    /// happens) and wake every process parked on a socket that became ready.
    fn drain_io_events(&mut self, timeout: Option<Duration>) -> VmResult<()> {
        // Take the VM's reusable buffer out for the iteration (the wake paths
        // below need `&mut self`); `poll()` clears it, so no per-call
        // allocation. The capacity-0 placeholder left behind allocates
        // nothing.
        let mut events = std::mem::replace(&mut self.poll_events, mio::Events::with_capacity(0));
        if let Err(e) = self.poll.poll(&mut events, timeout) {
            self.poll_events = events;
            return Err(VmError::Io(e));
        }
        for ev in events.iter() {
            if ev.token() == WAKER_TOKEN {
                // A `notify()` from another scheduler: it only ends the
                // wait, there is no I/O behind it.
                continue;
            }
            let key = ev.token().0 as i32;
            // The reverse index hands over this socket's waiters directly.
            // Clone first — `park_remove` edits the index.
            let Some(woken) = self.io_waiters.get(&key).cloned() else {
                continue;
            };
            for wid in woken {
                let Some((wait, p)) = self.park_remove(wid) else {
                    continue;
                };
                match wait {
                    // A finished connect delivers its result directly onto the
                    // woken process's stack; the connect instruction is not
                    // re-run. `wake_with` swaps the process in so the result
                    // is built in its own arena with its stack/frames as GC
                    // roots, exactly as for blocking-pool completions.
                    Wait::Io {
                        fd,
                        action: WakeAction::CompleteConnect,
                        ..
                    } => self.wake_with(p, |vm| vm.finish_connect(fd)),
                    Wait::Io {
                        action: WakeAction::Rerun,
                        ..
                    }
                    | Wait::Timer(_)
                    | Wait::Offload(_) => self.run_queue.push_back(p),
                }
                // The fd's registration stays armed: it belongs to the
                // socket, which remains in the tables. An event with no
                // parked wait behind it lands in this loop and is dropped.
            }
        }
        self.poll_events = events;
        Ok(())
    }

    /// Complete a pending non-blocking connect whose socket became writable:
    /// either adopt the connection or report the error it ended with.
    fn finish_connect(&mut self, id: i32) -> Value {
        // Budget the whole result up front in the (just-resumed) woken
        // process's arena: the adopted Ok(Socket) graph or a NetError.
        let Some(socket) = self.pending_connects.remove(&id) else {
            let aborted = self.stdlib_enum(&stdlib::net::error::CONNECTION_ABORTED);
            return self.make_err(aborted);
        };
        self.poller_deregister(socket.as_raw_fd());
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
