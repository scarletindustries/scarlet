//! The blocking-capable opcodes: files, TCP sockets, DNS, timers, spawn —
//! every op whose syscall may refuse to complete now, plus the socket
//! tables they run against.
//!
//! The discipline: sockets are non-blocking, and an op that would block
//! never stalls the scheduler thread — it parks the process with a [`Wait`]
//! describing what must happen before it can continue ([`super::poll`] owns
//! the wake side). Three resume protocols, distinguished by the `ip` the
//! park stores (the `ip - 1` / `ip` convention is the method contract
//! below):
//!
//! - **Re-run** (accept, socket reads and writes): the op re-pushes its
//!   operands — possibly transformed, e.g. `tcp_write` re-pushes a view
//!   over the unwritten tail — and rewinds `ip` so the whole instruction
//!   re-runs when the socket signals readiness.
//! - **Resume-after, result delivered** (file reads and writes,
//!   getaddrinfo): no readiness signal exists to wait on, so the op is
//!   shipped to the blocking pool ([`super::sched`]) and the wake side
//!   pushes the result before resuming at the already-advanced `ip`. The
//!   [`BlockingOp`] rides inside the `Wait` and is handed to the pool only
//!   when the scheduler registers the park, so the pool job's id is the
//!   wait id and a completion can never arrive before its process is
//!   parked. The pool is its own elastic set of `al-blocking` threads,
//!   spawned lazily per job and independent of the worker schedulers —
//!   which is what keeps the semantics identical at every scheduler count:
//!   a one-scheduler program still offloads a file read to a pool thread
//!   and keeps running other processes instead of blocking on the syscall.
//! - **Resume-after, result built on wake** (pending connect): the
//!   in-flight socket is stashed in `pending_connects`; when it signals
//!   writability the wake side finishes the connect and pushes the result
//!   (`WakeAction::CompleteConnect`) — no re-run, no operand re-push.
//!
//! `sleep` is the degenerate resume-after case: its Nil result is pushed
//! before parking and the deadline alone wakes it.
//!
//! Socket bookkeeping lives here too:
//!
//! - Each scheduler owns private fd tables (`tcp_listeners`,
//!   `tcp_connections`, `pending_connects`). Socket ids are unique across
//!   schedulers — the scheduler index rides in the id's top bits — so a
//!   socket value can migrate inside a spawn seed without colliding.
//! - A listener is ONE kernel socket, program-wide, owned by
//!   `Runtime.shared_listeners`: any scheduler that accepts registers the
//!   same fd with its own poller (`ensure_listener`), and every acceptor
//!   drains the socket's single accept queue. Nothing re-binds — `net.close`
//!   retires the id everywhere (`Runtime::retire_listener`).
//! - Connections are adopted into the accepting scheduler's table
//!   (`adopt_connection`) and move between tables only via migration's fd
//!   re-homing ([`super::migrate`]).
//!
//! Allocation is reference-counted, so an arm just pops its operands and
//! builds its result. Failures become typed stdlib values — `NetError` from
//! socket errnos, path-tagged `IoError` from file errnos, with a typed
//! `Errno(code)` residual — never strings.

use std::collections::HashMap;
use std::io::{ErrorKind, IoSlice, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, Socket, Type};

use al_core::bytecode::{BinaryRef, SocketValue, Value};
use al_core::static_ir::VariantTemplate;

use super::poll::{EPOCH, Wait, monotonic_now_ms};
use super::sched::BlockingOp;
use super::{IO_REDUCTION_COST, Step, VM, VmError, VmResult, bin_ref, lock, str_ref};
use crate::stdlib;

impl VM {
    // Method contract for this family: `ip` is the already-advanced
    // instruction pointer — a park that must re-run its instruction on wake
    // stores `ip - 1` in the frame, one that resumes after it stores `ip`.
    // `reds` is the slice's remaining reduction budget; syscall-backed ops
    // charge [`IO_REDUCTION_COST`]. A method returns `Some(step)` when the
    // slice must stop (the process parked), `None` to continue with the
    // next instruction; ops that can never park return plain `()`.

    pub(super) fn file_read(&mut self, ip: i32, reds: &mut i32) -> VmResult<Option<Step>> {
        *reds -= IO_REDUCTION_COST;
        let path_v = self.pop_str("io.read_file")?;
        // Offload to the blocking pool and park so the syscall never stalls
        // this scheduler; the completion delivers the result.
        let path = str_ref(&path_v).to_string();
        self.frame_mut().ip = ip;
        Ok(Some(Step::Parked(Wait::offloaded(BlockingOp::ReadFile(
            path,
        )))))
    }

    pub(super) fn file_write(&mut self, ip: i32, reds: &mut i32) -> VmResult<Option<Step>> {
        *reds -= IO_REDUCTION_COST;
        let bin_v = self.pop_binary("io.write_file")?;
        let path_v = self.pop_str("io.write_file")?;
        if let Some(v) =
            self.reject_unaligned(bin_ref(&bin_v).bit_len(), &stdlib::io::UNALIGNED_BINARY)
        {
            self.stack.push(v);
            return Ok(None);
        }
        // Offload to the blocking pool and park so the syscall never stalls
        // this scheduler.
        let bytes = bin_ref(&bin_v).full_bytes().into_owned();
        self.frame_mut().ip = ip;
        Ok(Some(Step::Parked(Wait::offloaded(BlockingOp::WriteFile(
            str_ref(&path_v).to_string(),
            bytes,
        )))))
    }

    pub(super) fn tcp_listen(&mut self) -> VmResult<()> {
        // Socket handle + Ok, or a NetError on bind failure.
        let addr_v = self.pop()?;
        let (ip, port) = decode_socket_addr(&addr_v, "net.listen_addr")?;
        let Some(addr) = valid_port(port).map(|p| SocketAddr::new(ip, p)) else {
            return self.push_invalid_port();
        };
        let res = bind_listener(addr).and_then(|listener| {
            // The one and only bind for this Server. The socket is shared:
            // `Runtime.shared_listeners` owns it, and any scheduler that
            // accepts registers this same fd with its own poller. One id,
            // one kernel socket, one accept queue — a connection can never
            // be queued on a socket nobody accepts from.
            let listener = std::sync::Arc::new(listener);
            let socket_id = self.alloc_socket_id();
            self.track_listener(socket_id, listener.clone())?;
            lock(&self.runtime.shared_listeners).insert(socket_id, listener);
            Ok(Value::socket(SocketValue {
                id: socket_id,
                is_listener: true,
            }))
        });
        self.push_net(res);
        Ok(())
    }

    pub(super) fn tcp_accept(&mut self, ip: i32, reds: &mut i32) -> VmResult<Option<Step>> {
        *reds -= IO_REDUCTION_COST;
        // Adopted Ok(Socket) result, a NetError, or the re-pushed
        // listener handle on the park path.
        let sv = self.pop_listener("net.accept")?;
        let accept_res = self.listener(sv.id).and_then(TcpListener::accept);
        match accept_res {
            Ok((conn, peer)) => {
                let v = self.adopt_connection(conn, peer);
                self.stack.push(v);
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                // No pending connection: park until the listener is
                // readable, then re-run this instruction.
                self.stack.push(Value::socket(sv));
                self.frame_mut().ip = ip - 1;
                return Ok(Some(Step::Parked(Wait::readable(sv.id))));
            }
            Err(e) => self.push_net(Err(e)),
        }
        Ok(None)
    }

    pub(super) fn tcp_connect(&mut self, ip: i32, reds: &mut i32) -> VmResult<Option<Step>> {
        *reds -= IO_REDUCTION_COST;
        // Adopted Ok(Socket) result or a NetError; the pending
        // path parks and allocates nothing.
        let addr_v = self.pop()?;
        // The hostname was already resolved off-scheduler by
        // al/net.connect; decode the typed SocketAddress and connect.
        let (ip_addr, port) = decode_socket_addr(&addr_v, "net.connect_addr")?;
        let Some(addr) = valid_port(port).map(|p| SocketAddr::new(ip_addr, p)) else {
            self.push_invalid_port()?;
            return Ok(None);
        };

        match start_connect(&addr) {
            // Local connects can complete immediately.
            Ok(ConnectStart::Connected(stream)) => {
                let v = self.adopt_connection(stream, addr);
                self.stack.push(v);
            }
            Ok(ConnectStart::Pending(socket)) => {
                // Park until writability signals the connect finished;
                // the wake side (`VM::finish_connect`) builds and pushes
                // the result, and execution resumes after this
                // instruction.
                let id = self.alloc_socket_id();
                match self.track_pending(id, socket) {
                    Ok(()) => {
                        self.frame_mut().ip = ip;
                        return Ok(Some(Step::Parked(Wait::connecting(id))));
                    }
                    // Unwatchable means unwakeable: report instead of parking.
                    Err(e) => self.push_net(Err(e)),
                }
            }
            Err(e) => self.push_net(Err(e)),
        }
        Ok(None)
    }

    pub(super) fn tcp_read(&mut self, ip: i32, reds: &mut i32) -> VmResult<Option<Step>> {
        *reds -= IO_REDUCTION_COST;
        // Ok(Read) — Data(Binary) (bytes are off-heap; the box is
        // constant sized) or the frozen Closed — or a NetError;
        // the park path re-pushes existing values only.
        let max = self.pop_int("socket.read")?;
        let sock_val = self.pop()?;
        let sv = connection_socket(&sock_val, "socket.read")?;
        let (max, read_res) = self.socket_read(sv.id, max);
        match read_res {
            Ok(n) => self.push_read_ok(n),
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                // Nothing to read yet: park until readable, then
                // re-run this instruction.
                self.stack.push(sock_val);
                self.stack.push(Value::small_int(max as i64));
                self.frame_mut().ip = ip - 1;
                return Ok(Some(Step::Parked(Wait::readable(sv.id))));
            }
            Err(e) => self.push_net(Err(e)),
        }
        Ok(None)
    }

    pub(super) fn tcp_read_until(&mut self, ip: i32, reds: &mut i32) -> VmResult<Option<Step>> {
        *reds -= IO_REDUCTION_COST;
        // Ok(Binary), the TimedOut error, or a NetError; the park
        // path re-pushes existing values only.
        // Args on the stack, top first: the absolute monotonic
        // deadline in ms, the max byte count, then the socket. The
        // deadline is captured once in AL as `time.monotonic() +
        // timeout_ms`, so it rides the stack as a plain Int: a re-run
        // after a wake re-reads the same absolute value and never
        // resets the clock.
        let deadline_ms = self.pop_int("socket.read_within")?;
        let max = self.pop_int("socket.read_within")?;
        let sock_val = self.pop()?;
        let sv = connection_socket(&sock_val, "socket.read_within")?;
        let (max, read_res) = self.socket_read(sv.id, max);
        match read_res {
            // The read happens before the deadline check, so bytes
            // that arrived as the clock ran out are never discarded.
            // A zero-byte read is a peer close, reported as
            // Ok(Closed), exactly as `socket.read` does.
            Ok(n) => self.push_read_ok(n),
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                if monotonic_now_ms() >= deadline_ms {
                    // The deadline passed with nothing to read.
                    let timed_out = self.stdlib_enum(&stdlib::net::error::TIMED_OUT);
                    let err = self.make_err(timed_out);
                    self.stack.push(err);
                } else {
                    // Re-push the args unchanged and re-run on wake.
                    // Park until the socket is readable or the
                    // deadline passes, whichever comes first; the
                    // re-run re-derives which one happened. The
                    // deadline instant is reconstructed from the
                    // absolute ms against the shared monotonic epoch,
                    // so it is identical on every re-run.
                    self.stack.push(sock_val);
                    self.stack.push(Value::small_int(max as i64));
                    self.stack.push(Value::small_int(deadline_ms));
                    self.frame_mut().ip = ip - 1;
                    let deadline = *EPOCH.get_or_init(Instant::now)
                        + Duration::from_millis(deadline_ms.max(0) as u64);
                    return Ok(Some(Step::Parked(Wait::read_with_deadline(
                        sv.id, deadline,
                    ))));
                }
            }
            Err(e) => self.push_net(Err(e)),
        }
        Ok(None)
    }

    pub(super) fn tcp_write(&mut self, ip: i32, reds: &mut i32) -> VmResult<Option<Step>> {
        *reds -= IO_REDUCTION_COST;
        // Ok(Nil), a NetError, the unaligned reject, or the park
        // path's zero-copy view box over the unwritten tail.
        let bin_v = self.pop_binary("socket.write")?;
        let sock_val = self.pop()?;
        let sv = connection_socket(&sock_val, "socket.write")?;
        if let Some(v) = self.reject_unaligned(
            bin_ref(&bin_v).bit_len(),
            &stdlib::net::error::UNALIGNED_BINARY,
        ) {
            self.stack.push(v);
            return Ok(None);
        }
        let bin = bin_ref(&bin_v);
        let bytes = bin.full_bytes();
        // Write what the socket will take. If it fills up mid-way,
        // park and resume this instruction with the remaining bytes.
        let result = connection_mut(&mut self.tcp_connections, sv.id)
            .and_then(|conn| drain_write(conn, std::slice::from_ref(&bytes)));
        if let Ok(Drain::Park { offset, .. }) = result {
            self.stack.push(sock_val);
            // Resume with a zero-copy view over the unwritten tail;
            // the binary is byte-aligned (rejected otherwise above).
            let tail = self.tail_view(bin, offset);
            self.stack.push(tail);
            self.frame_mut().ip = ip - 1;
            return Ok(Some(Step::Parked(Wait::writable(sv.id))));
        }
        let nil = self.make_nil();
        self.push_net(result.map(|_| nil));
        Ok(None)
    }

    pub(super) fn tcp_write_parts(&mut self, ip: i32, reds: &mut i32) -> VmResult<Option<Step>> {
        *reds -= IO_REDUCTION_COST;
        let parts_val = self.pop()?;
        let sock_val = self.pop()?;
        let sv = connection_socket(&sock_val, "socket.write_parts")?;
        let Some(parts) = parts_val.as_array() else {
            return Err(VmError::type_mismatch(
                "socket.write_parts",
                "Array(Binary)",
                &parts_val,
            ));
        };

        // Collect the parts, rejecting non-byte-aligned binaries.
        let mut bins: Vec<Value> = Vec::with_capacity(parts.len());
        let mut unaligned = false;
        for p in parts.iter() {
            match p.as_binary() {
                Some(b) => {
                    if !b.bit_len().is_multiple_of(8) {
                        unaligned = true;
                        break;
                    }
                    bins.push(p);
                }
                None => {
                    return Err(VmError::type_mismatch("socket.write_parts", "Binary", &p));
                }
            }
        }
        if unaligned {
            let e = self.stdlib_enum(&stdlib::net::error::UNALIGNED_BINARY);
            let err = self.make_err(e);
            self.stack.push(err);
            return Ok(None);
        }

        // Logical bytes per part: borrows the backing with no copy
        // when byte-aligned (the common case), re-aligns otherwise.
        // All parts are byte-aligned in length (rejected above).
        let logical: Vec<_> = bins.iter().map(|b| bin_ref(b).full_bytes()).collect();

        let result = connection_mut(&mut self.tcp_connections, sv.id)
            .and_then(|conn| drain_write(conn, &logical));
        if let Ok(Drain::Park { idx, offset }) = result {
            // Re-run with only the unwritten tail — zero-copy views
            // over the same backings (parts are byte-aligned).
            drop(logical);
            let mut remaining: Vec<Value> = Vec::with_capacity(bins.len() - idx);
            let head_view = self.tail_view(bin_ref(&bins[idx]), offset);
            remaining.push(head_view);
            for b in &bins[idx + 1..] {
                let view = self.tail_view(bin_ref(b), 0);
                remaining.push(view);
            }
            self.stack.push(sock_val);
            let arr = Value::array_in(&mut self.heap, &remaining);
            self.stack.push(arr);
            self.frame_mut().ip = ip - 1;
            return Ok(Some(Step::Parked(Wait::writable(sv.id))));
        }
        let nil = self.make_nil();
        self.push_net(result.map(|_| nil));
        Ok(None)
    }

    pub(super) fn tcp_close(&mut self, reds: &mut i32) -> VmResult<()> {
        *reds -= IO_REDUCTION_COST;
        // Ok(Nil) only.
        let sv = self.pop_connection("socket.close")?;
        drop(self.evict_connection(sv.id));
        let nil = self.make_nil();
        let v = self.make_ok(nil);
        self.stack.push(v);
        Ok(())
    }

    pub(super) fn tcp_close_server(&mut self) -> VmResult<()> {
        // Ok(Nil) only.
        let sv = self.pop_listener("net.close")?;
        // Retire program-wide (see `Runtime::retire_listener` for the full
        // sequence), then drain our own queue synchronously so a local parked
        // accept fails before `net.close` even returns.
        self.runtime.retire_listener(self.scheduler_index, sv.id);
        self.process_retired_listeners();
        let nil = self.make_nil();
        let v = self.make_ok(nil);
        self.stack.push(v);
        Ok(())
    }

    /// Drain this scheduler's retired-listener queue: deregister each fd
    /// from this poller, drop this scheduler's `Arc` clone, and wake every
    /// accept parked on the id. A woken accept re-runs, misses the shared
    /// map, and surfaces `NetError` — the same path a stale id always took —
    /// so `accept_loop` exits and `Runtime::live` drains instead of hanging.
    ///
    /// Returns whether anything was woken: the caller must not enter a
    /// blocking poll wait over a process this just made runnable. The flag
    /// check keeps the steady-state cost of the hot-path call sites to one
    /// relaxed load; closes are rare.
    pub(super) fn process_retired_listeners(&mut self) -> bool {
        let slot = &self.runtime.slots[self.scheduler_index];
        if !slot
            .retired_pending
            .swap(false, std::sync::atomic::Ordering::AcqRel)
        {
            return false;
        }
        let retired: Vec<i32> = std::mem::take(&mut *lock(&slot.retired_listeners));
        let mut woke = false;
        for id in retired {
            if let Some(listener) = self.tcp_listeners.remove(&id) {
                self.poller_deregister(listener.as_raw_fd());
            }
            woke |= self.fail_io_waiters(id);
        }
        woke
    }

    /// Take connection `id` out of this scheduler's world: remove it from
    /// the table, drop its poller registration, and fail every park waiting
    /// on it. The ONE eviction path — `socket.close` drops the returned
    /// connection, owner-death release drops it, and `detach_socket_ids`
    /// keeps it (the fd travels to another scheduler, owner and all).
    pub(super) fn evict_connection(&mut self, id: i32) -> Option<super::Conn> {
        let conn = self.tcp_connections.remove(&id);
        if let Some(c) = &conn {
            self.poller_deregister(c.stream.as_raw_fd());
        }
        self.fail_io_waiters(id);
        conn
    }

    /// Fail every park waiting on socket `id`: the socket is gone (closed,
    /// retired, or its fd re-homed to another scheduler), so each waiter is
    /// made runnable and its re-run resolves to the stale-socket `NetError` —
    /// the same result a use-after-close always produced. Without this, a
    /// sibling parked on the id stays parked forever and the program hangs.
    /// Returns whether anything was woken (a caller about to enter a blocking
    /// poll wait must know).
    pub(super) fn fail_io_waiters(&mut self, id: i32) -> bool {
        let Some(waiters) = self.io_waiters.remove(&id) else {
            return false;
        };
        let mut woke = false;
        for wid in waiters {
            if let Some((_, p)) = self.park_remove(wid) {
                self.run_queue.push_back(p);
                woke = true;
            }
        }
        woke
    }

    pub(super) fn tcp_local_addr(&mut self) -> VmResult<()> {
        // Ok(SocketAddress) or a NetError.
        let sv = self.pop_listener("net.local_addr")?;
        let res = self.listener_addr(sv.id);
        let res = res.map(|a| self.templates.socket_address(&mut self.heap, a));
        self.push_net(res);
        Ok(())
    }

    pub(super) fn ip_parse(&mut self) -> VmResult<()> {
        // Some(IpAddress) or the frozen None.
        let s_v = self.pop_str("address.parse")?;
        let v = match str_ref(&s_v).parse::<std::net::IpAddr>() {
            Ok(ip) => {
                let ip_val = self.templates.ip_address(&mut self.heap, ip);
                self.make_some(ip_val)
            }
            Err(_) => self.make_none(),
        };
        self.stack.push(v);
        Ok(())
    }

    pub(super) fn dns_resolve(&mut self, ip: i32, reds: &mut i32) -> VmResult<Option<Step>> {
        *reds -= IO_REDUCTION_COST;
        let host_v = self.pop_str("net.resolve")?;
        let host = str_ref(&host_v);
        if let Ok(addr) = host.parse::<std::net::IpAddr>() {
            // An IP literal needs no resolution.
            let ip_val = self.templates.ip_address(&mut self.heap, addr);
            let v = self.make_ok(ip_val);
            self.stack.push(v);
            return Ok(None);
        }
        // Offload getaddrinfo to the blocking pool and park, so it never
        // stalls this scheduler.
        let host = host.to_string();
        self.frame_mut().ip = ip;
        Ok(Some(Step::Parked(Wait::offloaded(BlockingOp::ResolveDns(
            host,
        )))))
    }

    #[inline(never)]
    pub(super) fn process_spawn(&mut self, reds: &mut i32) -> VmResult<()> {
        self.spawn_op(reds, Self::spawn_process)
    }

    #[inline(never)]
    pub(super) fn process_spawn_local(&mut self, reds: &mut i32) -> VmResult<()> {
        self.spawn_op(reds, Self::spawn_local)
    }

    #[inline(never)]
    pub(super) fn process_spawn_on_each(&mut self, reds: &mut i32) -> VmResult<()> {
        self.spawn_op(reds, Self::spawn_on_each)
    }

    /// Pop the closure, spawn via `spawn`, push Nil. Spawning deep-copies the
    /// closure and dups captured fds; charge it like I/O so a spawn loop
    /// cannot monopolize the scheduler.
    #[inline]
    fn spawn_op(
        &mut self,
        reds: &mut i32,
        spawn: fn(&mut Self, Value) -> VmResult<()>,
    ) -> VmResult<()> {
        let f = self.pop()?;
        spawn(self, f)?;
        let nil = self.make_nil();
        self.stack.push(nil);
        *reds -= IO_REDUCTION_COST;
        Ok(())
    }

    pub(super) fn sleep(&mut self, ip: i32) -> VmResult<Option<Step>> {
        let ms = self.pop_int("scheduler.sleep")?;
        let nil = self.make_nil();
        self.stack.push(nil);
        if ms > 0 {
            // The Nil result is already pushed, so on wake the
            // process resumes at the next instruction.
            self.frame_mut().ip = ip;
            let deadline = Instant::now() + Duration::from_millis(ms as u64);
            return Ok(Some(Step::Parked(Wait::until(deadline))));
        }
        Ok(None)
    }

    #[inline]
    fn pop_listener(&mut self, op: &'static str) -> VmResult<SocketValue> {
        let v = self.pop()?;
        match v.as_socket() {
            Some(s) => Ok(s),
            None => Err(VmError::type_mismatch(op, "Server", &v)),
        }
    }

    /// `Socket` is the AL record `{ conn Connection, peer SocketAddress }`;
    /// the underlying `SocketValue` is the first payload field.
    fn pop_connection(&mut self, op: &'static str) -> VmResult<SocketValue> {
        connection_socket(&self.pop()?, op)
    }

    /// Resolve `id` to a listener in this scheduler's table, hydrating from
    /// the runtime's shared listeners on a local miss. A stale id is a
    /// user-visible `NetError` (routed through [`VM::push_net`]), never a
    /// VM halt: closing then using a listener is an ordinary program bug.
    #[inline]
    fn listener(&mut self, id: i32) -> std::io::Result<&TcpListener> {
        self.ensure_listener(id)?;
        self.tcp_listeners
            .get(&id)
            .map(|l| l.as_ref())
            .ok_or_else(stale_socket)
    }

    /// Build an `al/net/error.NetError` from a socket/connect
    /// `std::io::Error`, mapping the raw OS errno to the matching variant.
    /// Anything without a named variant becomes the typed `Errno(code)`
    /// residual — never a string.
    pub(super) fn net_error_value(&mut self, e: &std::io::Error) -> Value {
        let t = match e.raw_os_error() {
            Some(libc::ETIMEDOUT) => &stdlib::net::error::TIMED_OUT,
            Some(libc::ECONNREFUSED) => &stdlib::net::error::CONNECTION_REFUSED,
            Some(libc::ECONNRESET) => &stdlib::net::error::CONNECTION_RESET,
            Some(libc::ECONNABORTED) => &stdlib::net::error::CONNECTION_ABORTED,
            Some(libc::ENOTCONN) => &stdlib::net::error::NOT_CONNECTED,
            Some(libc::EPIPE) => &stdlib::net::error::BROKEN_PIPE,
            Some(libc::EADDRINUSE) => &stdlib::net::error::ADDR_IN_USE,
            Some(libc::EADDRNOTAVAIL) => &stdlib::net::error::ADDR_NOT_AVAILABLE,
            Some(libc::ENETDOWN) => &stdlib::net::error::NETWORK_DOWN,
            Some(libc::ENETUNREACH) => &stdlib::net::error::NETWORK_UNREACHABLE,
            Some(libc::EHOSTUNREACH) => &stdlib::net::error::HOST_UNREACHABLE,
            Some(libc::EACCES) => &stdlib::net::error::PERMISSION_DENIED,
            _ => {
                let errno = errno_of(e);
                let tpl = self.stdlib_template(&stdlib::net::error::ERRNO);
                return tpl.instantiate(&mut self.heap, &[errno]);
            }
        };
        self.stdlib_enum(t)
    }

    /// Build an `al/io.IoError` from a filesystem `std::io::Error`, tagging
    /// path-bearing variants with `path`. Unnamed kinds become `Errno(code)`.
    pub(super) fn io_error_value(&mut self, e: &std::io::Error, path: &str) -> Value {
        let t = match e.raw_os_error() {
            Some(libc::ENOENT) => &stdlib::io::NOT_FOUND,
            Some(libc::EACCES) => &stdlib::io::PERMISSION_DENIED,
            Some(libc::EEXIST) => &stdlib::io::ALREADY_EXISTS,
            Some(libc::ENOTDIR) => &stdlib::io::NOT_ADIRECTORY,
            Some(libc::EISDIR) => &stdlib::io::IS_ADIRECTORY,
            Some(libc::EROFS) => &stdlib::io::READ_ONLY_FILESYSTEM,
            Some(libc::ELOOP) => &stdlib::io::FILESYSTEM_LOOP,
            Some(libc::ENOSPC) => return self.stdlib_enum(&stdlib::io::STORAGE_FULL),
            Some(libc::EDQUOT) => return self.stdlib_enum(&stdlib::io::QUOTA_EXCEEDED),
            Some(libc::EFBIG) => &stdlib::io::FILE_TOO_LARGE,
            _ => {
                let errno = errno_of(e);
                let tpl = self.stdlib_template(&stdlib::io::ERRNO);
                return tpl.instantiate(&mut self.heap, &[errno]);
            }
        };
        let tpl = self.stdlib_template(t);
        let path_v = Value::str_in(&mut self.heap, path);
        tpl.instantiate(&mut self.heap, &[path_v])
    }

    // --- I/O helpers ---------------------------------------------------------

    /// Clamp `max` to [1 byte, 8 MiB], size the scratch buffer, and read from
    /// connection `id`; returns the clamped max so a parking caller can re-push it.
    #[inline]
    fn socket_read(&mut self, id: i32, max: i64) -> (usize, std::io::Result<usize>) {
        let max = (max.max(1) as usize).min(8 * 1024 * 1024);
        if self.read_scratch.len() < max {
            self.read_scratch.resize(max, 0);
        }
        let read_res = match connection_mut(&mut self.tcp_connections, id) {
            Ok(conn) => conn.read(&mut self.read_scratch[..max]),
            Err(e) => Err(e),
        };
        (max, read_res)
    }

    /// Push `Ok(socket.Read)` for a syscall-level read of `n` bytes: `n == 0`
    /// is the POSIX peer-close signal and becomes the frozen `Closed` value;
    /// otherwise the first `n` bytes of the scratch buffer are copied out and
    /// wrapped as `Data(bin)`.
    #[inline]
    fn push_read_ok(&mut self, n: usize) {
        let read = if n == 0 {
            self.templates.read_closed.clone()
        } else {
            let bin = Value::binary_from_slice_in(&mut self.heap, &self.read_scratch[..n]);
            self.templates
                .read_data
                .clone()
                .instantiate(&mut self.heap, &[bin])
        };
        let ok = self.make_ok(read);
        self.stack.push(ok);
    }

    /// Zero-copy view box over `bin` minus its first `skip` bytes — the
    /// unwritten tail a parked write resumes with. The binary is byte-aligned
    /// (writes reject unaligned input).
    #[inline]
    fn tail_view(&mut self, bin: BinaryRef<'_>, skip: usize) -> Value {
        let backing = bin.backing_arc();
        let off = bin.bit_offset() + (skip as u64) * 8;
        let len = bin.bit_len() - (skip as u64) * 8;
        Value::binary_view_in(&mut self.heap, backing, off, len)
    }

    /// Push `Err(NetError::InvalidPort)` — a port outside 0..=65535 was handed
    /// to listen/connect (an `Int` port unchecked-`as u16` would silently wrap).
    #[cold]
    fn push_invalid_port(&mut self) -> VmResult<()> {
        let e = self.stdlib_enum(&stdlib::net::error::INVALID_PORT);
        let err = self.make_err(e);
        self.stack.push(err);
        Ok(())
    }

    /// Push `Ok(v)` or a typed `NetError` built from the socket `io::Error`.
    #[inline]
    fn push_net(&mut self, r: std::io::Result<Value>) {
        let v = match r {
            Ok(v) => self.make_ok(v),
            Err(err) => {
                let e = self.net_error_value(&err);
                self.make_err(e)
            }
        };
        self.stack.push(v);
    }

    /// If `bits` is not byte-aligned, the error to push — built from the
    /// caller's `unaligned` template (`NetError`/`IoError`).
    #[inline]
    fn reject_unaligned(
        &mut self,
        bits: u64,
        unaligned: &'static VariantTemplate,
    ) -> Option<Value> {
        if bits.is_multiple_of(8) {
            return None;
        }
        let e = self.stdlib_enum(unaligned);
        Some(self.make_err(e))
    }

    /// Allocate a socket id that is unique across every scheduler: the
    /// scheduler index lives in the top byte and the per-scheduler sequence
    /// in the low 24 bits, so sockets can move between schedulers (inside
    /// spawn seeds) without colliding. The sequence is masked so it can
    /// never spill into another scheduler's tag on wrap.
    #[inline]
    fn alloc_socket_id(&mut self) -> i32 {
        debug_assert!(
            self.scheduler_index < 256,
            "scheduler index overflows socket-id tag"
        );
        let seq = (self.next_socket_id as u32) & 0x00FF_FFFF;
        self.next_socket_id = self.next_socket_id.wrapping_add(1);
        (((self.scheduler_index as u32) << 24) | seq) as i32
    }

    /// The bound address of listener `id` — read off the shared socket (or
    /// this scheduler's clone of it, same fd either way).
    fn listener_addr(&self, id: i32) -> std::io::Result<SocketAddr> {
        if let Some(l) = self.tcp_listeners.get(&id) {
            return l.local_addr();
        }
        lock(&self.runtime.shared_listeners)
            .get(&id)
            .ok_or_else(stale_socket)?
            .local_addr()
    }

    /// Make listener `id` accept-able from this scheduler: clone the shared
    /// socket and register the SAME fd with this scheduler's poller. This
    /// function cannot construct a socket — `bind_listener` has exactly one
    /// call site (`tcp_listen`) — so one id can never denote two kernel
    /// sockets. (Its predecessor re-*bound* a second `SO_REUSEPORT` socket
    /// here, and a connection routed to the unaccepted twin deadlocked the
    /// program.) A miss means the listener was closed or never existed:
    /// stale-socket, surfaced as an ordinary `NetError`.
    ///
    /// The registration is the load-bearing half: the socket is global but
    /// each `mio::Poll` is thread-confined, so an accept parked here is woken
    /// only by THIS poller seeing the fd — each poller gets its own
    /// independent readiness edge for a shared fd (measured on both epoll and
    /// kqueue).
    fn ensure_listener(&mut self, id: i32) -> std::io::Result<()> {
        if self.tcp_listeners.contains_key(&id) {
            return Ok(());
        }
        let Some(l) = lock(&self.runtime.shared_listeners).get(&id).cloned() else {
            return Err(stale_socket());
        };
        self.track_listener(id, l)
    }

    /// Take ownership of a connected stream (from accept or connect):
    /// configure it, register it in the connection table and with the
    /// poller, and build the AL `Ok(Socket)` result — or `Err(NetError)`
    /// when the poller refuses the fd (a connection that cannot wake its
    /// parks must not be adopted).
    pub(super) fn adopt_connection(
        &mut self,
        stream: TcpStream,
        peer: std::net::SocketAddr,
    ) -> Value {
        // A blocking socket in a nonblocking event loop stalls the whole
        // scheduler on the first read/write; refuse it as `Err(NetError)`.
        if let Err(e) = stream.set_nonblocking(true) {
            let err = self.net_error_value(&e);
            return self.make_err(err);
        }
        // Small request/response exchanges should not sit in Nagle's buffer
        // waiting for an ACK.
        let _ = stream.set_nodelay(true);
        let id = self.alloc_socket_id();
        // The adopting process controls the connection (BEAM's rule): when it
        // ends, the connection closes. Ownership never moves implicitly.
        let owner = self.current_pid;
        if let Err(e) = self.track_connection(id, stream, owner) {
            let err = self.net_error_value(&e);
            return self.make_err(err);
        }
        let handle = Value::socket(SocketValue {
            id,
            is_listener: false,
        });
        let v = self.templates.make_socket(&mut self.heap, handle, peer);
        self.make_ok(v)
    }
}

/// The raw OS errno (or 0 if the error carries none), as the `Errno(code)`
/// residual's payload. An errno always fits the small-int range.
#[inline]
fn errno_of(e: &std::io::Error) -> Value {
    Value::small_int(e.raw_os_error().unwrap_or(0) as i64)
}

/// `Some(port as u16)` iff `port` is a valid TCP/UDP port. AL's `Int` is
/// arbitrary-width; unchecked `as u16` would silently wrap (100000 → 34464).
#[inline]
fn valid_port(port: i64) -> Option<u16> {
    u16::try_from(port).ok()
}

/// Decode an `al/net/address.SocketAddress` — the record `{ ip IpAddress, port
/// Int }` — into a `(std::net::IpAddr, i64)`. The port is returned unvalidated
/// so the caller can surface an out-of-range value as `NetError::InvalidPort`.
fn decode_socket_addr(v: &Value, op: &'static str) -> VmResult<(std::net::IpAddr, i64)> {
    let e = v
        .as_enum()
        .ok_or_else(|| VmError::type_mismatch(op, "SocketAddress", v))?;
    let payload = e.payload();
    let ip = payload
        .first()
        .map(decode_ip)
        .ok_or_else(|| VmError::type_mismatch(op, "SocketAddress", v))??;
    let port = payload
        .get(1)
        .and_then(Value::as_int)
        .ok_or_else(|| VmError::type_mismatch(op, "SocketAddress", v))?;
    Ok((ip, port))
}

/// Decode an `al/net/address.IpAddress` value (`V4(s)` / `V6(s)`) into a
/// `std::net::IpAddr`. Values reaching here via the public AL API are always
/// well-formed — `address.parse` and `net.resolve` are the only supported
/// constructors — so a parse failure indicates a caller that ignored that
/// contract and hand-built a variant.
fn decode_ip(v: &Value) -> VmResult<std::net::IpAddr> {
    let payload = v.as_enum().and_then(|e| e.payload().first().cloned());
    let s = payload.as_ref().and_then(|p| p.as_str());
    match s {
        Some(s) => s
            .parse::<std::net::IpAddr>()
            .map_err(|_| VmError::internal("malformed IpAddress: use address.parse")),
        None => Err(VmError::internal("malformed IpAddress: use address.parse")),
    }
}

/// The two ways a non-blocking connect can begin.
enum ConnectStart {
    /// Completed immediately (common for loopback).
    Connected(TcpStream),
    /// In flight; completion is signalled by writability.
    Pending(socket2::Socket),
}

/// Begin a non-blocking TCP connect.
fn start_connect(addr: &std::net::SocketAddr) -> std::io::Result<ConnectStart> {
    let socket = socket2::Socket::new(
        socket2::Domain::for_address(*addr),
        socket2::Type::STREAM,
        None,
    )?;
    socket.set_nonblocking(true)?;
    match socket.connect(&(*addr).into()) {
        Ok(()) => Ok(ConnectStart::Connected(socket.into())),
        // EINPROGRESS (unix) / WouldBlock (windows): completion comes later.
        Err(e) if e.raw_os_error() == Some(libc::EINPROGRESS) => Ok(ConnectStart::Pending(socket)),
        Err(e) if e.kind() == ErrorKind::WouldBlock => Ok(ConnectStart::Pending(socket)),
        Err(e) => Err(e),
    }
}

/// `listen(2)` backlog: the depth of the kernel's completed-connection queue
/// before new connections are dropped. The platform caps this at `somaxconn`;
/// a deeper request than the default keeps a connection burst from
/// overflowing the queue (the std default of 128 is shallow for a server).
const LISTEN_BACKLOG: i32 = 1024;

/// Bind THE non-blocking TCP listener for a `Server` — one socket, shared by
/// every scheduler that accepts on it. Exactly one call site (`tcp_listen`).
///
/// Deliberately no `SO_REUSEPORT`: a second bind to the same address is
/// `EADDRINUSE`, so a regression that reintroduces a per-scheduler lazy bind
/// fails loudly instead of silently splitting the accept queue. (The old
/// per-scheduler reuseport group deadlocked whenever a member had no
/// acceptor — and on macOS `SO_REUSEPORT` never balanced anyway: the last
/// binder received every connection.) Non-blocking so accept parks the
/// calling process rather than stalling the scheduler.
fn bind_listener(addr: SocketAddr) -> std::io::Result<TcpListener> {
    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    socket.set_reuse_address(true)?;
    socket.bind(&addr.into())?;
    socket.listen(LISTEN_BACKLOG)?;
    socket.set_nonblocking(true)?;
    Ok(socket.into())
}

/// Outcome of [`drain_write`]: everything written, or the socket filled up
/// at byte `offset` of `bufs[idx]`.
enum Drain {
    Done,
    Park { idx: usize, offset: usize },
}

/// Drain `bufs` into a non-blocking stream: write until everything is gone,
/// the socket fills up (`Park`), or the write fails. A single remaining
/// buffer goes through plain `write` (no per-pass allocation — the
/// `socket.write` hot path); multiple remaining buffers go to the kernel in
/// one vectored write per pass. Drained buffers are skipped up front, so an
/// all-empty tail finishes without a syscall and `Ok(0)` means peer close.
fn drain_write<B: AsRef<[u8]>>(conn: &mut TcpStream, bufs: &[B]) -> std::io::Result<Drain> {
    let mut idx = 0usize;
    let mut offset = 0usize;
    loop {
        while idx < bufs.len() && offset == bufs[idx].as_ref().len() {
            idx += 1;
            offset = 0;
        }
        if idx == bufs.len() {
            return Ok(Drain::Done);
        }
        let head = &bufs[idx].as_ref()[offset..];
        let wrote = if idx + 1 == bufs.len() {
            conn.write(head)
        } else {
            let mut slices: Vec<IoSlice> = Vec::with_capacity(bufs.len() - idx);
            slices.push(IoSlice::new(head));
            for c in &bufs[idx + 1..] {
                slices.push(IoSlice::new(c.as_ref()));
            }
            conn.write_vectored(&slices)
        };
        match wrote {
            Ok(0) => return Err(std::io::Error::from_raw_os_error(libc::EPIPE)),
            Ok(mut n) => {
                // Advance (idx, offset) past the bytes the kernel took.
                while n > 0 && idx < bufs.len() {
                    let left = bufs[idx].as_ref().len() - offset;
                    if n >= left {
                        n -= left;
                        idx += 1;
                        offset = 0;
                    } else {
                        offset += n;
                        n = 0;
                    }
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(Drain::Park { idx, offset }),
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

/// Extract the underlying `SocketValue` from a `Socket` record value without
/// consuming it. `Socket` is the AL record `{ conn Connection, peer ... }`;
/// the raw handle is its first payload field.
fn connection_socket(v: &Value, op: &'static str) -> VmResult<SocketValue> {
    if let Some(e) = v.as_enum()
        && let Some(s) = e.payload().first().and_then(Value::as_socket)
    {
        return Ok(s);
    }
    Err(VmError::type_mismatch(op, "Socket", v))
}

/// Resolve `id` in the connection table. A free function over the field (not
/// a `&mut self` method) so callers keep split borrows of other VM fields.
/// A stale id (closed then used) surfaces as an AL `NetError` via
/// [`VM::push_net`], never a VM halt.
#[inline]
fn connection_mut(
    conns: &mut HashMap<i32, super::Conn>,
    id: i32,
) -> std::io::Result<&mut TcpStream> {
    conns
        .get_mut(&id)
        .map(|c| &mut c.stream)
        .ok_or_else(stale_socket)
}

#[cold]
fn stale_socket() -> std::io::Error {
    // Built from the raw errno so `net_error_value` (which routes on
    // `raw_os_error`) maps use-after-close to `NotConnected`, not `Errno(0)`.
    std::io::Error::from_raw_os_error(libc::ENOTCONN)
}

#[cfg(test)]
mod tests {
    //! The listener-identity invariant, at the unit level: one `Server` id
    //! denotes one kernel socket, on every scheduler, for the program's
    //! whole lifetime — and retiring it reaches every scheduler.
    //!
    //! The end-to-end deadlock regression (accept on a scheduler that did
    //! not listen) lives in `tests/vm_io.rs`; these pin the mechanism.

    use std::net::SocketAddr;
    use std::os::fd::AsRawFd;
    use std::sync::Arc;

    use super::super::{sched, vm_for_runtime};
    use super::*;
    use al_core::bytecode::{Function, Instruction, Op, Program};

    fn halt_program() -> Program {
        Program {
            constants: Vec::new(),
            functions: vec![Function {
                name: "main".into(),
                arity: 0,
                locals: 0,
                capture_count: 0,
                code_start: 0,
                code_len: 1,
            }],
            code: vec![Instruction {
                op: Op::Halt,
                a: 0,
                b: 0,
                operand: 0,
            }],
            entry: 0,
            frozen: Arc::new(al_core::frozen::FrozenArea::new()),
        }
    }

    /// `ensure_listener` on a scheduler that did not listen registers the
    /// SAME kernel socket — never a second one. This is the fd-level form of
    /// the invariant; the old code bound a fresh `SO_REUSEPORT` socket here
    /// and deadlocked whenever a connection landed on the unaccepted twin.
    #[test]
    fn a_foreign_scheduler_registers_the_same_kernel_socket() {
        let (rt, poll0) = sched::Runtime::new(Arc::new(halt_program()), Vec::new(), 2)
            .expect("runtime must construct");
        let mut vm0 = vm_for_runtime(Arc::clone(&rt), 0, poll0);
        let mut vm1 = vm_for_runtime(
            Arc::clone(&rt),
            1,
            mio::Poll::new().expect("poller must construct"),
        );

        // Mirror `tcp_listen`'s ownership steps (minus the AL value plumbing).
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = Arc::new(bind_listener(addr).expect("bind"));
        let id = vm0.alloc_socket_id();
        vm0.track_listener(id, Arc::clone(&listener))
            .expect("track");
        super::super::lock(&rt.shared_listeners).insert(id, listener);

        vm1.ensure_listener(id).expect("foreign registration");
        assert_eq!(
            vm0.tcp_listeners[&id].as_raw_fd(),
            vm1.tcp_listeners[&id].as_raw_fd(),
            "one Server id must denote one kernel socket on every scheduler"
        );
    }

    /// Retiring a listener empties every scheduler's table and makes a later
    /// `ensure_listener` a stale-socket error — it can never re-create the
    /// socket, because it cannot bind.
    #[test]
    fn retiring_a_listener_reaches_every_scheduler_and_cannot_revive() {
        let (rt, poll0) = sched::Runtime::new(Arc::new(halt_program()), Vec::new(), 2)
            .expect("runtime must construct");
        let mut vm0 = vm_for_runtime(Arc::clone(&rt), 0, poll0);
        let mut vm1 = vm_for_runtime(
            Arc::clone(&rt),
            1,
            mio::Poll::new().expect("poller must construct"),
        );

        // `retire_listener` queues only on live schedulers (a never-spawned
        // worker has no thread to drain the queue); mark slot 1 live the way
        // `ensure_workers` would, by filling its waker.
        let waker =
            mio::Waker::new(vm1.poll.registry(), super::super::poll::WAKER_TOKEN).expect("waker");
        let _ = rt.slots[1].waker.set(Arc::new(waker));

        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = Arc::new(bind_listener(addr).expect("bind"));
        let id = vm0.alloc_socket_id();
        vm0.track_listener(id, Arc::clone(&listener))
            .expect("track");
        super::super::lock(&rt.shared_listeners).insert(id, listener);
        vm1.ensure_listener(id).expect("foreign registration");

        rt.retire_listener(usize::MAX, id);
        vm0.process_retired_listeners();
        vm1.process_retired_listeners();

        assert!(!vm0.tcp_listeners.contains_key(&id));
        assert!(!vm1.tcp_listeners.contains_key(&id));
        assert!(
            vm1.ensure_listener(id).is_err(),
            "a retired id must resolve to stale-socket, never to a new bind"
        );
    }

    /// A retire wake must not be followed by a blocking poll wait: the woken
    /// accept sits in `run_queue`, and with a second park keeping `parked`
    /// non-empty, a blocking `poll_parked` that ignored the wake would strand
    /// it behind an idle poller — a hang, found in review of this very fix.
    #[test]
    fn a_retire_wake_prevents_a_blocking_poll_wait() {
        use super::super::poll::Wait;
        use super::super::{Process, halt_test_vm};
        use al_core::heap::ProcHeap;
        use std::time::{Duration, Instant};

        let mut vm = halt_test_vm();
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = Arc::new(bind_listener(addr).expect("bind"));
        let id = vm.alloc_socket_id();
        vm.track_listener(id, Arc::clone(&listener)).expect("track");
        super::super::lock(&vm.runtime.shared_listeners).insert(id, listener);

        let parked = || Process {
            heap: ProcHeap,
            stack: Vec::new(),
            frames: Vec::new(),
            is_main: false,
            pid: 0,
        };
        // The accept park that retire will wake…
        vm.park(Wait::readable(id), parked());
        // …and a far-future timer park that keeps `parked` non-empty, so the
        // blocking branch is reachable.
        vm.park(
            Wait::Timer(Instant::now() + Duration::from_secs(30)),
            parked(),
        );

        // `from` = a foreign index, so the notify for slot 0 fires as it
        // would from a real cross-scheduler close.
        vm.runtime.retire_listener(usize::MAX, id);

        let t0 = Instant::now();
        vm.poll_parked(true).expect("poll");
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "blocking poll ignored the retire wake and slept toward the timer"
        );
        assert_eq!(
            vm.run_queue.len(),
            1,
            "the retired accept must be runnable, not parked"
        );
    }

    /// The kernel assumption the shared-listener design rests on, pinned into
    /// CI: one listening fd registered in two pollers delivers an independent
    /// readiness edge to each (holds on both epoll and kqueue).
    #[test]
    fn one_fd_in_two_pollers_wakes_both() {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = bind_listener(addr).expect("bind");
        let bound = listener.local_addr().expect("addr");
        let fd = listener.as_raw_fd();

        let mut p1 = mio::Poll::new().expect("poll 1");
        let mut p2 = mio::Poll::new().expect("poll 2");
        for p in [&p1, &p2] {
            p.registry()
                .register(
                    &mut mio::unix::SourceFd(&fd),
                    mio::Token(7),
                    mio::Interest::READABLE,
                )
                .expect("register");
        }

        let _conn = std::net::TcpStream::connect(bound).expect("connect");

        let saw = |p: &mut mio::Poll| {
            let mut evs = mio::Events::with_capacity(4);
            p.poll(&mut evs, Some(std::time::Duration::from_secs(5)))
                .expect("poll");
            evs.iter().any(|e| e.token() == mio::Token(7))
        };
        assert!(saw(&mut p1), "poller 1 must see the shared fd's readiness");
        assert!(saw(&mut p2), "poller 2 must see the shared fd's readiness");
    }
}
