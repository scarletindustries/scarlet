//! Parking and wake-up: how a blocked process sleeps and what brings it back.
//!
//! An opcode that cannot progress returns `Step::Parked` with a [`Wait`]. The
//! process is stashed under a fresh wait id and [`VM::poll_parked`] delivers
//! whatever became ready — I/O events, due timers, blocking-pool completions —
//! back onto the run queue.
//!
//! Parking arms nothing. A socket is registered with the poller once, when it
//! enters this scheduler's tables, with every interest it can ever park on.
//!
//! PARK-AFTER-PROBE, the reason edge-triggering loses no wakeups: a park on an
//! fd is always immediately preceded, on this same OS thread, by a syscall on
//! that fd returning `WouldBlock`, with no `Poll::poll` in between, and only
//! this thread drains this poller. An edge firing after the probe then sits in
//! the kernel's ready list until a drain that runs after the park is
//! registered. A dropped edge is NOT re-announced later, so do not add a
//! `poll_parked` between a `WouldBlock` and its park, and do not drain this
//! poller from another thread. Either reopens the classic lost wakeup.
//!
//! Invariants:
//!
//! - A wait wakes exactly once; the first of fd readiness or deadline removes
//!   the park.
//! - Timer entries are lazily deleted. A popped entry whose id is gone or
//!   re-keyed is discarded, so the nearest live deadline is an O(log n) peek.
//! - Wake-time value construction runs with the woken process swapped in, so
//!   allocation lands in its heap with its stack and frames as GC roots.
//! - A socket stays registered while it is in the VM's tables; close
//!   deregisters before dropping.
//!
//! [`EPOCH`] is the process-global monotonic origin shared by `Op::Monotonic`
//! and `socket.read_within`, so an absolute deadline means the same instant on
//! every scheduler.

use std::cmp::Reverse;
use std::io::{self, ErrorKind};
use std::net::{TcpListener, TcpStream};
use std::os::fd::{AsRawFd, RawFd};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use mio::Token;
use mio::unix::SourceFd;

use crate::abi::AbiSlot;
use crate::bytecode::Value;

use super::sched::{BlockingOp, BlockingResult, Completion};
use super::{Process, VM, VmError, VmResult, lock};

/// How a parked I/O wait resumes once one of its sockets is ready.
#[derive(Debug, Clone, Copy)]
pub(super) enum WakeAction {
    /// Resume wherever the op left `ip`. Retry waits set `ip - 1` to re-run;
    /// `Sleep` leaves `ip` past itself with its result already on the stack.
    Rerun,
    /// `VM::finish_connect` builds the result and pushes it onto the process's
    /// stack instead of re-running the instruction.
    CompleteConnect,
}

/// What a parked process is waiting for. Exactly one wake source per wait.
#[derive(Debug)]
pub(super) enum Wait {
    /// Socket readiness on `fd`, optionally bounded by a deadline. Whichever
    /// fires first wakes the process.
    Io {
        fd: i32,
        deadline: Option<Instant>,
        action: WakeAction,
    },
    /// A pure timer: wake at this instant, resume at the next instruction.
    Timer(Instant),
    /// A blocking-pool job. `Some` on the way into [`VM::park`], which hands
    /// the op to the pool keyed by the wait id and leaves `None` behind.
    Offload(Option<BlockingOp>),
}

impl Wait {
    /// Park until socket `id` becomes ready, then re-run the instruction.
    /// Interest direction was fixed at registration; a wait only names the fd.
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

    /// Park until socket `id` is readable or `deadline` passes, then re-run the
    /// instruction, which re-derives which of the two happened.
    pub(super) fn read_with_deadline(id: i32, deadline: Instant) -> Self {
        Wait::Io {
            fd: id,
            deadline: Some(deadline),
            action: WakeAction::Rerun,
        }
    }

    /// Park until the blocking pool finishes a job. Nothing else can wake it.
    /// The job id is the wait id.
    pub(super) fn offloaded(op: BlockingOp) -> Self {
        Wait::Offload(Some(op))
    }

    /// The instant this wait must wake by. Timer-heap entries are validated
    /// against it to spot stale ones.
    fn deadline(&self) -> Option<Instant> {
        match self {
            Wait::Io { deadline, .. } => *deadline,
            Wait::Timer(d) => Some(*d),
            Wait::Offload(_) => None,
        }
    }
}

/// The token every scheduler's [`mio::Waker`] registers under. Drains skip it:
/// a wake ends the wait but delivers no I/O. Socket ids are small positive
/// `i32`s, so it cannot collide with a socket registration.
pub(super) const WAKER_TOKEN: Token = Token(usize::MAX);

/// Event-buffer capacity for one poll call.
pub(super) const EVENTS_CAPACITY: usize = 1024;

/// Process-global monotonic origin, pinned to the first reading taken.
pub(super) static EPOCH: OnceLock<Instant> = OnceLock::new();

/// Milliseconds since [`EPOCH`], saturating at `i64::MAX`.
pub(super) fn monotonic_now_ms() -> i64 {
    let ms = EPOCH.get_or_init(Instant::now).elapsed().as_millis();
    ms.min(i64::MAX as u128) as i64
}

impl VM {
    /// Register `fd` under socket `id` with every interest it can ever park on.
    /// An fd number still registered from a stale entry (a close that skipped
    /// deregistration, then kernel fd reuse) is re-registered under the new id.
    fn poller_register(&self, fd: RawFd, id: i32, interests: mio::Interest) -> io::Result<()> {
        let registry = self.poll.registry();
        registry
            .register(&mut SourceFd(&fd), Token(id as usize), interests)
            .or_else(|e| {
                if e.kind() == ErrorKind::AlreadyExists {
                    // Reachable only when a close path skipped deregistration
                    // and the kernel reused the fd number. Recover by
                    // re-pointing the registration, but fail loudly in debug
                    // so the skipping path gets found — until then, events
                    // could have been delivered against the stale id.
                    debug_assert!(
                        false,
                        "fd {fd} already registered: a close path skipped poller_deregister"
                    );
                    registry.reregister(&mut SourceFd(&fd), Token(id as usize), interests)
                } else {
                    Err(e)
                }
            })
    }

    /// Drop `fd` from the poller, ignoring an fd that was never registered.
    /// Call before closing or handing off a socket.
    pub(super) fn poller_deregister(&self, fd: RawFd) {
        let _ = self.poll.registry().deregister(&mut SourceFd(&fd));
    }

    /// Register the shared listener with this scheduler's poller. Idempotent.
    /// Each scheduler's poller holds its own readiness for the shared fd, so an
    /// accept parked here wakes here.
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

    /// Adopt a connection into this scheduler's table, watched for read and
    /// write readiness. A registration failure drops the connection rather than
    /// tabling a socket that could never wake its parks.
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
        self.conns_by_owner.entry(owner).or_default().push(id);
        Ok(())
    }

    /// Adopt an in-progress non-blocking connect, watched for the writability
    /// that signals completion.
    pub(super) fn track_pending(&mut self, id: i32, socket: socket2::Socket) -> io::Result<()> {
        self.poller_register(socket.as_raw_fd(), id, mio::Interest::WRITABLE)?;
        self.pending_connects.insert(id, socket);
        Ok(())
    }

    /// Stash a suspended process under a fresh wait id and register the side
    /// effects its `Wait` implies: the `io_waiters` reverse index, the timer
    /// heap, the blocking-pool dispatch. The only way into `parked`.
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
                // `Wait::offloaded` is the only constructor and always yields
                // `Some`. A `None` would dispatch no job and hang the process
                // forever, so panic instead of swallowing it.
                #[allow(clippy::expect_used)]
                let op = op.take().expect("offload park carries an op");
                self.runtime.offload(self.scheduler_index, id, op);
            }
        }
        self.parked.insert(id, (wait, p));
        id
    }

    /// Remove a park and its socket reverse-index entries. Every wake path goes
    /// through here so `io_waiters` stays in lockstep with `parked`.
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

    /// Wake a parked process with a value built in its own context: `p` is made
    /// current so `build` allocates in its arena with its stack and frames as
    /// GC roots. The one place that invariant is enforced.
    fn wake_with(
        &mut self,
        p: Process,
        build: impl FnOnce(&mut Self) -> VmResult<Value>,
    ) -> VmResult<()> {
        let interrupted = self.suspend_current();
        self.resume(p);
        let result = build(self);
        // Restore scheduler state before propagating a build failure so the
        // error surfaces through the normal top-level path.
        let value = match result {
            Ok(v) => v,
            Err(e) => {
                let woken = self.suspend_current();
                self.run_queue.push_back(woken);
                self.resume(interrupted);
                return Err(e);
            }
        };
        self.stack.push(value);
        let woken = self.suspend_current();
        self.run_queue.push_back(woken);
        self.resume(interrupted);
        Ok(())
    }

    /// Move parked processes whose I/O is ready or whose timer has expired
    /// back onto the run queue.
    ///
    /// With `block`, performs one interruptible wait — I/O readiness, the
    /// nearest timer deadline, or a `notify()` from another scheduler all end
    /// it — then returns so the caller can re-check for remote work.
    pub(super) fn poll_parked(&mut self, block: bool) -> VmResult<()> {
        // Must run before the parked-empty early-out: this scheduler's
        // registration and Arc clone have to go before the shared fd can close,
        // even with nothing parked here.
        let retired_woke = self.process_retired_listeners();
        if self.parked.is_empty() {
            return Ok(());
        }

        // A retire wake counts as a wake: it queued a runnable process, and
        // blocking below would strand it behind an idle poller.
        let mut woke = retired_woke | self.drain_completions()?;
        woke |= self.wake_due_timers();

        let waiting_on_io = !self.io_waiters.is_empty();

        if !block || woke {
            if waiting_on_io {
                self.drain_io_events(Some(Duration::ZERO))?;
            }
            return Ok(());
        }

        // One blocking wait, bounded by the nearest live timer deadline. The
        // poller is used even for timer-only waits so `notify()` can interrupt
        // it. Stale heap tops are dropped while looking.
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
        self.drain_completions()?;
        Ok(())
    }

    /// Deliver finished blocking-pool jobs, waking the process parked under
    /// each `job_id`. Returns whether anything was woken. Whatever process was
    /// current is detached around the delivery and restored after.
    pub(super) fn drain_completions(&mut self) -> VmResult<bool> {
        let drained: Vec<Completion> = {
            let mut q = lock(&self.runtime.slots[self.scheduler_index].completions);
            if q.is_empty() {
                return Ok(false);
            }
            q.drain(..).collect()
        };
        let mut woke = false;
        for c in drained {
            let Some((_wait, p)) = self.park_remove(c.job_id) else {
                continue;
            };
            self.wake_with(p, |vm| vm.completion_result(c.result))?;
            woke = true;
        }
        Ok(woke)
    }

    /// Construct a blocking-pool result in the current process's heap. A
    /// completion carries only `Send` raw data, never a `Value`.
    fn completion_result(&mut self, result: BlockingResult) -> VmResult<Value> {
        match result {
            BlockingResult::ReadFile { path, result } => match result {
                Ok(bytes) => {
                    let bin = Value::binary_in(&mut self.heap, bytes);
                    self.make_ok(bin)
                }
                Err(e) => {
                    let err = self.io_error_value(&e, &path)?;
                    self.make_err(err)
                }
            },
            BlockingResult::WriteFile { path, result } => match result {
                Ok(()) => {
                    let nil = self.make_nil()?;
                    self.make_ok(nil)
                }
                Err(e) => {
                    let err = self.io_error_value(&e, &path)?;
                    self.make_err(err)
                }
            },
            BlockingResult::ResolveDns { result } => match result {
                Ok(addr) => {
                    let ip = self.templates.ip_address(&mut self.heap, addr)?;
                    self.make_ok(ip)
                }
                Err(e) => {
                    let err = self.net_error_value(&e)?;
                    self.make_err(err)
                }
            },
        }
    }

    /// Wake parked processes whose deadline has passed. A popped entry whose id
    /// is gone, or whose live deadline no longer matches, already woke on I/O.
    pub(super) fn wake_due_timers(&mut self) -> bool {
        let now = Instant::now();
        let mut woke = false;
        while let Some(&Reverse((deadline, id))) = self.timer_heap.peek() {
            if deadline > now {
                break;
            }
            self.timer_heap.pop();
            if !matches!(self.parked.get(&id), Some((w, _)) if w.deadline() == Some(deadline)) {
                continue;
            }
            let Some((_wait, p)) = self.park_remove(id) else {
                continue;
            };
            // Nothing to disarm: an fd's registration belongs to the socket,
            // not to this wait.
            self.run_queue.push_back(p);
            woke = true;
        }
        woke
    }

    /// Wait on the poller for at most `timeout` (None = until something
    /// happens) and wake every process parked on a socket that became ready.
    fn drain_io_events(&mut self, timeout: Option<Duration>) -> VmResult<()> {
        // Take the reusable buffer out because the wake paths below need
        // `&mut self`. The capacity-0 placeholder allocates nothing.
        let mut events = std::mem::replace(&mut self.poll_events, mio::Events::with_capacity(0));
        if let Err(e) = self.poll.poll(&mut events, timeout) {
            self.poll_events = events;
            return Err(VmError::Io(e));
        }
        for ev in events.iter() {
            if ev.token() == WAKER_TOKEN {
                // A `notify()` from another scheduler. No I/O behind it.
                continue;
            }
            let key = ev.token().0 as i32;
            // Clone first: `park_remove` edits the index.
            let Some(woken) = self.io_waiters.get(&key).cloned() else {
                continue;
            };
            for wid in woken {
                let Some((wait, p)) = self.park_remove(wid) else {
                    continue;
                };
                match wait {
                    // A finished connect pushes its result straight onto the
                    // woken process's stack; the instruction is not re-run.
                    Wait::Io {
                        fd,
                        action: WakeAction::CompleteConnect,
                        ..
                    } => self.wake_with(p, |vm| vm.finish_connect(fd))?,
                    Wait::Io {
                        action: WakeAction::Rerun,
                        ..
                    }
                    | Wait::Timer(_)
                    | Wait::Offload(_) => self.run_queue.push_back(p),
                }
                // The fd stays registered: the registration belongs to the
                // socket, which is still in the tables.
            }
        }
        self.poll_events = events;
        Ok(())
    }

    /// Complete a pending non-blocking connect whose socket became writable:
    /// either adopt the connection or report the error it ended with.
    fn finish_connect(&mut self, id: i32) -> VmResult<Value> {
        let Some(socket) = self.pending_connects.remove(&id) else {
            let aborted = self.abi_nullary(AbiSlot::NetEconnaborted)?;
            return self.make_err(aborted);
        };
        self.poller_deregister(socket.as_raw_fd());
        // Writability after EINPROGRESS means the connect finished. SO_ERROR
        // says whether it succeeded.
        match socket.take_error() {
            Ok(None) => {
                let stream: TcpStream = socket.into();
                match stream.peer_addr() {
                    Ok(peer) => self.adopt_connection(stream, peer),
                    Err(e) => {
                        let err = self.net_error_value(&e)?;
                        self.make_err(err)
                    }
                }
            }
            Ok(Some(e)) | Err(e) => {
                let err = self.net_error_value(&e)?;
                self.make_err(err)
            }
        }
    }
}
