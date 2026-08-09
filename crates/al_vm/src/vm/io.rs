//! The blocking-capable opcodes: files, TCP sockets, DNS, timers, spawn.
//!
//! Sockets are non-blocking and an op that would block parks the process with
//! a [`Wait`] instead of stalling the scheduler thread; [`super::poll`] owns
//! the wake side. Three resume protocols, told apart by the `ip` the park
//! stores:
//!
//! - **Re-run** (accept, socket reads and writes): re-push the operands,
//!   possibly transformed (`tcp_write` re-pushes a view over the unwritten
//!   tail), and rewind `ip` so the instruction runs again on readiness.
//! - **Resume-after, result delivered** (file reads and writes, getaddrinfo):
//!   nothing readiness-signalled to wait on, so the op ships to the blocking
//!   pool ([`super::sched`]) and the wake side pushes the result. The
//!   [`BlockingOp`] rides inside the `Wait` and reaches the pool only when the
//!   scheduler registers the park, so a completion can never arrive before its
//!   process is parked.
//! - **Resume-after, result built on wake** (pending connect): the in-flight
//!   socket sits in `pending_connects` until writability, when the wake side
//!   finishes the connect and pushes the result.
//!
//! `sleep` is the degenerate resume-after case: the Nil result is pushed
//! before parking and the deadline alone wakes it.
//!
//! Socket bookkeeping:
//!
//! - Each scheduler owns private fd tables. Socket ids carry the scheduler
//!   index in their top bits, so a socket can migrate in a spawn seed without
//!   colliding.
//! - A listener is ONE kernel socket, program-wide, owned by
//!   `Runtime.shared_listeners`. Any scheduler that accepts registers that
//!   same fd with its own poller and drains the one accept queue. Nothing
//!   re-binds; `net.close` retires the id everywhere.
//! - Connections are adopted into the accepting scheduler's table and move
//!   only via migration's fd re-homing ([`super::migrate`]).
//!
//! Failures become typed stdlib values — `NetError`, path-tagged `IoError`,
//! and a typed `Errno(code)` residual — never strings.

use std::collections::HashMap;
use std::io::{ErrorKind, IoSlice, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, Socket, Type};

use crate::abi::{self, AbiSlot};
use crate::bytecode::{BinaryRef, SocketValue, Value};

use super::poll::{EPOCH, Wait, monotonic_now_ms};
use super::sched::BlockingOp;
use super::{IO_REDUCTION_COST, Step, VM, VmError, VmResult, bin_ref, lock, str_ref};

impl VM {
    // Method contract for this family: `ip` is the already-advanced
    // instruction pointer. A park that must re-run its instruction stores
    // `ip - 1`; one that resumes after it stores `ip`. `Some(step)` stops the
    // slice (the process parked), `None` continues; ops that never park
    // return `()`.

    pub(super) fn file_read(&mut self, ip: i32, reds: &mut i32) -> VmResult<Option<Step>> {
        *reds -= IO_REDUCTION_COST;
        let path_v = self.pop_str("io.read_file")?;
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
            self.reject_unaligned(bin_ref(&bin_v).bit_len(), AbiSlot::FsUnalignedBinary)?
        {
            self.stack.push(v);
            return Ok(None);
        }
        let bytes = bin_ref(&bin_v).full_bytes().into_owned();
        self.frame_mut().ip = ip;
        Ok(Some(Step::Parked(Wait::offloaded(BlockingOp::WriteFile(
            str_ref(&path_v).to_string(),
            bytes,
        )))))
    }

    pub(super) fn tcp_listen(&mut self) -> VmResult<()> {
        let addr_v = self.pop()?;
        let (ip, port) = decode_socket_addr(&addr_v, "net.listen_addr")?;
        let Some(addr) = valid_port(port).map(|p| SocketAddr::new(ip, p)) else {
            return self.push_invalid_port();
        };
        let res = bind_listener(addr).and_then(|listener| {
            // The one and only bind for this Server: one id, one kernel
            // socket, one accept queue. A connection can never be queued on a
            // socket nobody accepts from.
            let listener = std::sync::Arc::new(listener);
            let socket_id = self.alloc_socket_id();
            self.track_listener(socket_id, listener.clone())?;
            lock(&self.runtime.shared_listeners).insert(socket_id, listener);
            Ok(Value::socket(SocketValue {
                id: socket_id,
                is_listener: true,
            }))
        });
        self.push_net(res)?;
        Ok(())
    }

    pub(super) fn tcp_accept(&mut self, ip: i32, reds: &mut i32) -> VmResult<Option<Step>> {
        *reds -= IO_REDUCTION_COST;
        let sv = self.pop_listener("net.accept")?;
        let accept_res = self.listener(sv.id).and_then(TcpListener::accept);
        match accept_res {
            Ok((conn, peer)) => {
                let v = self.adopt_connection(conn, peer)?;
                self.stack.push(v);
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                // Park until readable, then re-run this instruction.
                self.stack.push(Value::socket(sv));
                self.frame_mut().ip = ip - 1;
                return Ok(Some(Step::Parked(Wait::readable(sv.id))));
            }
            Err(e) => self.push_net(Err(e))?,
        }
        Ok(None)
    }

    pub(super) fn tcp_connect(&mut self, ip: i32, reds: &mut i32) -> VmResult<Option<Step>> {
        *reds -= IO_REDUCTION_COST;
        // The hostname was already resolved off-scheduler by al/net.connect.
        let addr_v = self.pop()?;
        let (ip_addr, port) = decode_socket_addr(&addr_v, "net.connect_addr")?;
        let Some(addr) = valid_port(port).map(|p| SocketAddr::new(ip_addr, p)) else {
            self.push_invalid_port()?;
            return Ok(None);
        };

        match start_connect(&addr) {
            // Local connects can complete immediately.
            Ok(ConnectStart::Connected(stream)) => {
                let v = self.adopt_connection(stream, addr)?;
                self.stack.push(v);
            }
            Ok(ConnectStart::Pending(socket)) => {
                // Park until writability signals the connect finished;
                // `VM::finish_connect` pushes the result on wake.
                let id = self.alloc_socket_id();
                match self.track_pending(id, socket) {
                    Ok(()) => {
                        self.frame_mut().ip = ip;
                        return Ok(Some(Step::Parked(Wait::connecting(id))));
                    }
                    // Unwatchable means unwakeable: report instead of parking.
                    Err(e) => self.push_net(Err(e))?,
                }
            }
            Err(e) => self.push_net(Err(e))?,
        }
        Ok(None)
    }

    pub(super) fn tcp_read(&mut self, ip: i32, reds: &mut i32) -> VmResult<Option<Step>> {
        *reds -= IO_REDUCTION_COST;
        let max = self.pop_int("socket.read")?;
        let sock_val = self.pop()?;
        let sv = connection_socket(&sock_val, "socket.read")?;
        let (max, read_res) = self.socket_read(sv.id, max);
        match read_res {
            Ok(n) => self.push_read_ok(n)?,
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                // Park until readable, then re-run this instruction.
                self.stack.push(sock_val);
                self.stack.push(Value::small_int(max as i64));
                self.frame_mut().ip = ip - 1;
                return Ok(Some(Step::Parked(Wait::readable(sv.id))));
            }
            Err(e) => self.push_net(Err(e))?,
        }
        Ok(None)
    }

    pub(super) fn tcp_read_until(&mut self, ip: i32, reds: &mut i32) -> VmResult<Option<Step>> {
        *reds -= IO_REDUCTION_COST;
        // Stack, top first: the deadline as an absolute monotonic ms, the max
        // byte count, then the socket. AL captures the deadline once, so a
        // re-run after a wake re-reads it and never resets the clock.
        let deadline_ms = self.pop_int("socket.read_within")?;
        let max = self.pop_int("socket.read_within")?;
        let sock_val = self.pop()?;
        let sv = connection_socket(&sock_val, "socket.read_within")?;
        let (max, read_res) = self.socket_read(sv.id, max);
        match read_res {
            // The read runs before the deadline check, so bytes that arrived
            // as the clock ran out are never discarded.
            Ok(n) => self.push_read_ok(n)?,
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                if monotonic_now_ms() >= deadline_ms {
                    let timed_out = self.abi_nullary(AbiSlot::NetEtimedout)?;
                    let err = self.make_err(timed_out)?;
                    self.stack.push(err);
                } else {
                    // Park until readable or the deadline passes, whichever
                    // comes first; the re-run re-derives which happened. The
                    // instant is rebuilt from the absolute ms against the
                    // shared epoch, so it is the same on every re-run.
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
            Err(e) => self.push_net(Err(e))?,
        }
        Ok(None)
    }

    pub(super) fn tcp_write(&mut self, ip: i32, reds: &mut i32) -> VmResult<Option<Step>> {
        *reds -= IO_REDUCTION_COST;
        let bin_v = self.pop_binary("socket.write")?;
        let sock_val = self.pop()?;
        let sv = connection_socket(&sock_val, "socket.write")?;
        if let Some(v) =
            self.reject_unaligned(bin_ref(&bin_v).bit_len(), AbiSlot::NetUnalignedBinary)?
        {
            self.stack.push(v);
            return Ok(None);
        }
        let bin = bin_ref(&bin_v);
        let bytes = bin.full_bytes();
        // Write what the socket takes; if it fills up, park and resume this
        // instruction with the remaining bytes.
        let result = connection_mut(&mut self.tcp_connections, sv.id)
            .and_then(|conn| drain_write(conn, std::slice::from_ref(&bytes)));
        if let Ok(Drain::Park { offset, .. }) = result {
            self.stack.push(sock_val);
            // The binary is byte-aligned; unaligned was rejected above.
            let tail = self.tail_view(bin, offset);
            self.stack.push(tail);
            self.frame_mut().ip = ip - 1;
            return Ok(Some(Step::Parked(Wait::writable(sv.id))));
        }
        let nil = self.make_nil()?;
        self.push_net(result.map(|_| nil))?;
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
            let e = self.abi_nullary(AbiSlot::NetUnalignedBinary)?;
            let err = self.make_err(e)?;
            self.stack.push(err);
            return Ok(None);
        }

        // Borrows each backing with no copy when byte-aligned, which every
        // part is by the check above.
        let logical: Vec<_> = bins.iter().map(|b| bin_ref(b).full_bytes()).collect();

        let result = connection_mut(&mut self.tcp_connections, sv.id)
            .and_then(|conn| drain_write(conn, &logical));
        if let Ok(Drain::Park { idx, offset }) = result {
            // Re-run with only the unwritten tail: zero-copy views over the
            // same backings.
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
        let nil = self.make_nil()?;
        self.push_net(result.map(|_| nil))?;
        Ok(None)
    }

    pub(super) fn tcp_close(&mut self, reds: &mut i32) -> VmResult<()> {
        *reds -= IO_REDUCTION_COST;
        let sv = self.pop_connection("socket.close")?;
        drop(self.evict_connection(sv.id));
        let nil = self.make_nil()?;
        let v = self.make_ok(nil)?;
        self.stack.push(v);
        Ok(())
    }

    pub(super) fn tcp_close_server(&mut self) -> VmResult<()> {
        let sv = self.pop_listener("net.close")?;
        // Retire program-wide, then drain our own queue synchronously so a
        // local parked accept fails before `net.close` returns.
        self.runtime.retire_listener(self.scheduler_index, sv.id);
        self.process_retired_listeners();
        let nil = self.make_nil()?;
        let v = self.make_ok(nil)?;
        self.stack.push(v);
        Ok(())
    }

    /// Drain this scheduler's retired-listener queue: deregister each fd, drop
    /// this scheduler's `Arc` clone, and wake every accept parked on the id. A
    /// woken accept re-runs, misses the shared map and surfaces `NetError`, so
    /// `accept_loop` exits instead of hanging.
    ///
    /// Returns whether anything was woken. A caller must not enter a blocking
    /// poll wait over a process this just made runnable.
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

    /// Take connection `id` out of this scheduler: remove it from the table,
    /// drop its poller registration, and fail every park waiting on it. The
    /// ONE eviction path. `detach_socket_ids` keeps the connection instead —
    /// the fd travels to another scheduler.
    pub(super) fn evict_connection(&mut self, id: i32) -> Option<super::Conn> {
        let conn = self.tcp_connections.remove(&id);
        if let Some(c) = &conn {
            self.poller_deregister(c.stream.as_raw_fd());
        }
        self.fail_io_waiters(id);
        conn
    }

    /// Fail every park waiting on socket `id`. The socket is gone, so each
    /// waiter is made runnable and its re-run resolves to the stale-socket
    /// `NetError`. Without this a sibling parked on the id hangs forever.
    /// Returns whether anything was woken.
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
        let sv = self.pop_listener("net.local_addr")?;
        let res = match self.listener_addr(sv.id) {
            Ok(a) => Ok(self.templates.socket_address(&mut self.heap, a)?),
            Err(e) => Err(e),
        };
        self.push_net(res)?;
        Ok(())
    }

    pub(super) fn ip_parse(&mut self) -> VmResult<()> {
        let s_v = self.pop_str("address.parse")?;
        let v = match str_ref(&s_v).parse::<std::net::IpAddr>() {
            Ok(ip) => {
                let ip_val = self.templates.ip_address(&mut self.heap, ip)?;
                self.make_some(ip_val)?
            }
            Err(_) => self.make_none()?,
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
            let ip_val = self.templates.ip_address(&mut self.heap, addr)?;
            let v = self.make_ok(ip_val)?;
            self.stack.push(v);
            return Ok(None);
        }
        // getaddrinfo has no readiness signal, so offload it to the pool.
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

    /// Pop the closure, spawn it, push Nil. Spawning deep-copies the closure
    /// and dups captured fds, so it is charged like I/O to stop a spawn loop
    /// monopolizing the scheduler.
    #[inline]
    fn spawn_op(
        &mut self,
        reds: &mut i32,
        spawn: fn(&mut Self, Value) -> VmResult<()>,
    ) -> VmResult<()> {
        let f = self.pop()?;
        spawn(self, f)?;
        let nil = self.make_nil()?;
        self.stack.push(nil);
        *reds -= IO_REDUCTION_COST;
        Ok(())
    }

    pub(super) fn sleep(&mut self, ip: i32) -> VmResult<Option<Step>> {
        let ms = self.pop_int("scheduler.sleep")?;
        let nil = self.make_nil()?;
        self.stack.push(nil);
        if ms > 0 {
            // The Nil result is already pushed, so the wake resumes at the
            // next instruction.
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
    /// the runtime's shared listeners on a miss. A stale id is a user-visible
    /// `NetError`, never a VM halt.
    #[inline]
    fn listener(&mut self, id: i32) -> std::io::Result<&TcpListener> {
        self.ensure_listener(id)?;
        self.tcp_listeners
            .get(&id)
            .map(|l| l.as_ref())
            .ok_or_else(stale_socket)
    }

    /// Build the `NetError` for a socket/connect `std::io::Error`. Errnos
    /// without a dedicated class become the typed `Errno(code)` residual —
    /// never a string.
    pub(super) fn net_error_value(&mut self, e: &std::io::Error) -> VmResult<Value> {
        match abi::classify_net(e) {
            abi::NetFailure::Bare(slot) => self.abi_nullary(slot),
            abi::NetFailure::Errno(code) => {
                self.abi_make(AbiSlot::NetErrnoOther, &[Value::small_int(code as i64)])
            }
        }
    }

    /// Build the `IoError` for a filesystem `std::io::Error`, tagging
    /// path-bearing classes with `path`.
    pub(super) fn io_error_value(&mut self, e: &std::io::Error, path: &str) -> VmResult<Value> {
        match abi::classify_fs(e) {
            abi::FsFailure::Path(slot) => {
                let path_v = Value::str_in(&mut self.heap, path);
                self.abi_make(slot, &[path_v])
            }
            abi::FsFailure::Bare(slot) => self.abi_nullary(slot),
            abi::FsFailure::Errno(code) => {
                self.abi_make(AbiSlot::FsErrnoOther, &[Value::small_int(code as i64)])
            }
        }
    }

    /// Clamp `max` to [1 byte, 8 MiB], size the scratch buffer, and read from
    /// connection `id`. Returns the clamped max so a parking caller re-pushes
    /// the same value.
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

    /// Push `Ok(socket.Read)` for a read of `n` bytes. `n == 0` is the POSIX
    /// peer-close signal and becomes `Closed`.
    #[inline]
    fn push_read_ok(&mut self, n: usize) -> VmResult<()> {
        let read = if n == 0 {
            self.abi_nullary(AbiSlot::ReadClosed)?
        } else {
            let bin = Value::binary_from_slice_in(&mut self.heap, &self.read_scratch[..n]);
            self.abi_make(AbiSlot::ReadData, &[bin])?
        };
        let ok = self.make_ok(read)?;
        self.stack.push(ok);
        Ok(())
    }

    /// Zero-copy view over `bin` minus its first `skip` bytes: the unwritten
    /// tail a parked write resumes with. `bin` must be byte-aligned.
    #[inline]
    fn tail_view(&mut self, bin: BinaryRef<'_>, skip: usize) -> Value {
        let backing = bin.backing_arc();
        let off = bin.bit_offset() + (skip as u64) * 8;
        let len = bin.bit_len() - (skip as u64) * 8;
        Value::binary_view_in(&mut self.heap, backing, off, len)
    }

    /// Push `Err(NetError::InvalidPort)`. An `Int` port cast `as u16` would
    /// silently wrap instead.
    #[cold]
    fn push_invalid_port(&mut self) -> VmResult<()> {
        let e = self.abi_nullary(AbiSlot::NetInvalidPort)?;
        let err = self.make_err(e)?;
        self.stack.push(err);
        Ok(())
    }

    /// Push `Ok(v)` or a typed `NetError` built from the socket `io::Error`.
    #[inline]
    fn push_net(&mut self, r: std::io::Result<Value>) -> VmResult<()> {
        let v = match r {
            Ok(v) => self.make_ok(v)?,
            Err(err) => {
                let e = self.net_error_value(&err)?;
                self.make_err(e)?
            }
        };
        self.stack.push(v);
        Ok(())
    }

    /// If `bits` is not byte-aligned, the error to push, built from the
    /// caller's unaligned-binary slot.
    #[inline]
    fn reject_unaligned(&mut self, bits: u64, unaligned: AbiSlot) -> VmResult<Option<Value>> {
        if bits.is_multiple_of(8) {
            return Ok(None);
        }
        let e = self.abi_nullary(unaligned)?;
        Ok(Some(self.make_err(e)?))
    }

    /// Allocate a socket id unique across every scheduler: index in the top
    /// byte, per-scheduler sequence in the low 24 bits, so sockets can move
    /// between schedulers without colliding. The sequence is masked so a wrap
    /// cannot spill into another scheduler's tag.
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

    /// The bound address of listener `id`.
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
    /// socket and register the SAME fd with this scheduler's poller. It must
    /// never bind a socket of its own — a connection routed to an unaccepted
    /// twin deadlocks the program. A miss means the listener was closed and
    /// surfaces as a `NetError`.
    ///
    /// The registration is load-bearing: the socket is global but each
    /// `mio::Poll` is thread-confined, so an accept parked here is only woken
    /// by THIS poller seeing the fd. Each poller gets its own readiness edge
    /// for a shared fd, on both epoll and kqueue.
    fn ensure_listener(&mut self, id: i32) -> std::io::Result<()> {
        if self.tcp_listeners.contains_key(&id) {
            return Ok(());
        }
        let Some(l) = lock(&self.runtime.shared_listeners).get(&id).cloned() else {
            return Err(stale_socket());
        };
        self.track_listener(id, l)
    }

    /// Take ownership of a connected stream and build the AL `Ok(Socket)`, or
    /// `Err(NetError)` when the poller refuses the fd — a connection that
    /// cannot wake its parks must not be adopted.
    pub(super) fn adopt_connection(
        &mut self,
        stream: TcpStream,
        peer: std::net::SocketAddr,
    ) -> VmResult<Value> {
        // A blocking socket in a nonblocking event loop stalls the whole
        // scheduler on its first read or write.
        if let Err(e) = stream.set_nonblocking(true) {
            let err = self.net_error_value(&e)?;
            return self.make_err(err);
        }
        // Small request/response exchanges should not sit in Nagle's buffer
        // waiting for an ACK.
        let _ = stream.set_nodelay(true);
        let id = self.alloc_socket_id();
        // The adopting process owns the connection: when it ends, the
        // connection closes. Ownership never moves implicitly.
        let owner = self.current_pid;
        if let Err(e) = self.track_connection(id, stream, owner) {
            let err = self.net_error_value(&e)?;
            return self.make_err(err);
        }
        let handle = Value::socket(SocketValue {
            id,
            is_listener: false,
        });
        let v = self.templates.make_socket(&mut self.heap, handle, peer)?;
        self.make_ok(v)
    }
}

/// `Some(port as u16)` iff `port` is a valid TCP/UDP port. AL's `Int` is
/// arbitrary-width, so `as u16` would silently wrap (100000 → 34464).
#[inline]
fn valid_port(port: i64) -> Option<u16> {
    u16::try_from(port).ok()
}

/// Decode an `al/net/address.SocketAddress`. The port comes back unvalidated
/// so the caller can surface an out-of-range one as `NetError::InvalidPort`.
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

/// Decode an `al/net/address.IpAddress` into a `std::net::IpAddr`.
/// `address.parse` and `net.resolve` are the only supported constructors, so
/// a parse failure means the caller hand-built the variant.
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

/// `listen(2)` backlog: how many completed connections the kernel queues
/// before dropping new ones. Capped at `somaxconn`. The std default of 128 is
/// shallow enough that a connection burst overflows it.
const LISTEN_BACKLOG: i32 = 1024;

/// Bind THE non-blocking TCP listener for a `Server` — one socket, shared by
/// every scheduler that accepts on it. Exactly one call site (`tcp_listen`).
///
/// Deliberately no `SO_REUSEPORT`: a second bind to the same address is then
/// `EADDRINUSE`, so a per-scheduler lazy bind fails loudly instead of quietly
/// splitting the accept queue. A reuseport group deadlocks whenever a member
/// has no acceptor, and on macOS it never balanced anyway — the last binder
/// received every connection.
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
/// the socket fills up (`Park`), or the write fails. One remaining buffer
/// takes plain `write` to avoid a per-pass allocation on the `socket.write`
/// hot path; several take one vectored write per pass. Drained buffers are
/// skipped up front, so `Ok(0)` always means peer close.
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

/// Extract the `SocketValue` from a `Socket` record without consuming it.
/// `Socket` is `{ conn Connection, peer ... }`, so the handle is field 0.
fn connection_socket(v: &Value, op: &'static str) -> VmResult<SocketValue> {
    if let Some(e) = v.as_enum()
        && let Some(s) = e.payload().first().and_then(Value::as_socket)
    {
        return Ok(s);
    }
    Err(VmError::type_mismatch(op, "Socket", v))
}

/// Resolve `id` in the connection table. A free function over the field, not
/// a `&mut self` method, so callers keep split borrows of other VM fields. A
/// stale id surfaces as an AL `NetError`, never a VM halt.
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
    // A raw errno, because the NetError mapping routes on `raw_os_error`: an
    // errno-less error would map use-after-close to `Errno(0)`.
    std::io::Error::from_raw_os_error(libc::ENOTCONN)
}

#[cfg(test)]
mod tests {
    //! The listener-identity invariant: one `Server` id denotes one kernel
    //! socket on every scheduler, and retiring it reaches every scheduler.
    //! The end-to-end deadlock regression lives in `tests/vm_io.rs`.

    use std::net::SocketAddr;
    use std::os::fd::AsRawFd;
    use std::sync::Arc;

    use super::super::{sched, vm_for_runtime};
    use super::*;
    use crate::bytecode::{Function, Instruction, Op, Program};

    fn halt_program() -> Program {
        let frozen = Arc::new(crate::frozen::FrozenArea::new());
        let mut fb = frozen.builder();
        let (templates, abi) = crate::template::test_fixture::build(&mut fb);
        drop(fb);
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
            frozen,
            native: Default::default(),
            templates,
            abi,
        }
    }

    /// `ensure_listener` on a scheduler that did not listen registers the
    /// SAME kernel socket, never a second one. Binding a fresh
    /// `SO_REUSEPORT` socket here deadlocks on the unaccepted twin.
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

        // Mirror `tcp_listen`'s ownership steps.
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

        // `retire_listener` queues only on live schedulers, so mark slot 1
        // live the way `ensure_workers` would: fill its waker.
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
    /// accept sits in `run_queue`, and a `poll_parked` that ignored the wake
    /// would strand it behind an idle poller.
    #[test]
    fn a_retire_wake_prevents_a_blocking_poll_wait() {
        use super::super::poll::Wait;
        use super::super::{Process, halt_test_vm};
        use crate::heap::ProcHeap;
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
            native_floor: 0,
            native_reds: 0,
            native_pending: None,
        };
        // The accept park that retire will wake…
        vm.park(Wait::readable(id), parked());
        // …and a far-future timer park that keeps `parked` non-empty, so the
        // blocking branch is reachable.
        vm.park(
            Wait::Timer(Instant::now() + Duration::from_secs(30)),
            parked(),
        );

        // A foreign `from` index, so slot 0's notify fires as it would from a
        // real cross-scheduler close.
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

    /// The kernel assumption the shared-listener design rests on: one fd
    /// registered in two pollers gives each an independent readiness edge.
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
