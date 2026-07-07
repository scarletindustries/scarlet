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
//! - Listeners are bound with `SO_REUSEPORT` and shared by address, not by
//!   fd (`share_listener_addr`): a listener bound at top level may be
//!   accepted on from any scheduler, each binding its own reuseport socket to
//!   the shared address on first accept (`ensure_listener`), so the kernel
//!   load-balances incoming connections across the per-core sockets.
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
use super::{IO_REDUCTION_COST, Step, VM, VmError, VmResult, bin_ref, sched, str_ref};
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
        // Nothing allocates before the park: the path is copied off-heap and
        // the result graph is budgeted at completion delivery.
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
        // Budget while the operands are rooted: only the unaligned-binary
        // reject allocates here — the write itself is offloaded and its
        // result graph is budgeted at completion delivery.
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
        let res = bind_reuseport(addr).and_then(|listener| {
            // Pin the *resolved* address (a port of 0 becomes the
            // kernel-assigned one) so every other scheduler binds its own
            // reuseport socket to the same address — that is what makes
            // the kernel load-balance connections across cores.
            let addr = listener.local_addr()?;
            let socket_id = self.alloc_socket_id();
            self.track_listener(socket_id, listener)?;
            self.share_listener_addr(socket_id, addr);
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
        // Ok(Binary) (bytes are off-heap; the box is constant
        // sized) or a NetError; the park path re-pushes existing
        // values only.
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
            // A zero-byte read is a peer close, reported as Ok(<<>>),
            // exactly as `socket.read` does.
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
            return Err(VmError::internal(
                "socket.write_parts requires an Array(Binary)",
            ));
        };

        // Collect the parts, rejecting non-byte-aligned binaries.
        // The part values stay reachable through `parts_val`
        // (rooted until the method ends; nothing here collects).
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
                    return Err(VmError::internal(
                        "socket.write_parts requires an Array(Binary)",
                    ));
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
        if let Some(conn) = self.tcp_connections.remove(&sv.id) {
            // Deregister before dropping; ignore "wasn't registered".
            self.poller_deregister(conn.as_raw_fd());
        }
        let nil = self.make_nil();
        let v = self.make_ok(nil);
        self.stack.push(v);
        Ok(())
    }

    pub(super) fn tcp_close_server(&mut self) -> VmResult<()> {
        // Ok(Nil) only.
        let sv = self.pop_listener("net.close")?;
        if let Some(listener) = self.tcp_listeners.remove(&sv.id) {
            self.poller_deregister(listener.as_raw_fd());
        }
        // Closing a server retires it everywhere.
        sched::lock(&self.runtime.shared_listeners).remove(&sv.id);
        let nil = self.make_nil();
        let v = self.make_ok(nil);
        self.stack.push(v);
        Ok(())
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
        // Budget for the IP-literal fast path's Ok(IpAddress); the offload
        // path parks and its result is budgeted at completion delivery.
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

    pub(super) fn process_spawn(&mut self, reds: &mut i32) -> VmResult<()> {
        let f = self.pop()?;
        self.spawn_process(f)?;
        let nil = self.make_nil();
        self.stack.push(nil);
        // Spawning deep-copies the closure and dups captured fds;
        // charge it like I/O so a spawn loop cannot monopolize
        // the scheduler.
        *reds -= IO_REDUCTION_COST;
        Ok(())
    }

    pub(super) fn process_spawn_local(&mut self, reds: &mut i32) -> VmResult<()> {
        let f = self.pop()?;
        self.spawn_local(f)?;
        let nil = self.make_nil();
        self.stack.push(nil);
        *reds -= IO_REDUCTION_COST;
        Ok(())
    }

    pub(super) fn process_spawn_on_each(&mut self, reds: &mut i32) -> VmResult<()> {
        let f = self.pop()?;
        self.spawn_on_each(f)?;
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
        if !self.ensure_listener(id) {
            return Err(stale_socket());
        }
        self.tcp_listeners.get(&id).ok_or_else(stale_socket)
    }

    /// Build an `al/net/error.NetError` from a socket/connect
    /// `std::io::Error`, mapping the raw OS errno to the matching variant.
    /// Anything without a named variant becomes the typed `Errno(code)`
    /// residual — never a string. The caller has ensured `cost::NET_ERR`.
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
    /// path-bearing variants with `path`. Unnamed kinds become
    /// `Errno(code)`. The caller has ensured `cost::io_err(path.len())`.
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

    /// Push `Ok(Binary)` over the first `n` bytes just read into the scratch
    /// buffer (copied out). The caller ensured `cost::BINARY + cost::WRAP`.
    #[inline]
    fn push_read_ok(&mut self, n: usize) {
        let data = Value::binary_from_slice_in(&mut self.heap, &self.read_scratch[..n]);
        let ok = self.make_ok(data);
        self.stack.push(ok);
    }

    /// Zero-copy view box over `bin` minus its first `skip` bytes — the
    /// unwritten tail a parked write resumes with. The binary is byte-aligned
    /// (writes reject unaligned input); the caller ensured `cost::BINARY`.
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
    /// The caller's ensured budget covers `cost::WRAP` plus, on the error
    /// path, `cost::NET_ERR` (it ensured before popping).
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
    /// caller's `unaligned` template (`NetError`/`IoError`). The caller's
    /// ensured budget covers the variant + wrapper.
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

    /// Record a listener's bound address program-wide, keyed by socket id, so
    /// any scheduler that needs to accept on it can bind its own reuseport
    /// socket to the same address ([`VM::ensure_listener`]).
    fn share_listener_addr(&mut self, id: i32, addr: SocketAddr) {
        sched::lock(&self.runtime.shared_listeners).insert(id, addr);
    }

    /// The bound address of listener `id`: this scheduler's own socket if it
    /// holds one, otherwise the address shared at listen time. Reading the
    /// shared address never binds a socket — only an actual accept does — so a
    /// process that merely queries the address does not leave an unaccepted
    /// reuseport queue behind.
    fn listener_addr(&self, id: i32) -> std::io::Result<SocketAddr> {
        if let Some(l) = self.tcp_listeners.get(&id) {
            return l.local_addr();
        }
        sched::lock(&self.runtime.shared_listeners)
            .get(&id)
            .copied()
            .ok_or_else(|| std::io::Error::from(ErrorKind::NotConnected))
    }

    /// Resolve `id` to a listener in this scheduler's table, binding this
    /// scheduler's own `SO_REUSEPORT` socket to the shared address on a local
    /// miss. Each scheduler thus accepts from its own kernel queue; the kernel
    /// spreads incoming connections across the per-scheduler sockets. Binding
    /// happens here, on first accept, so only a scheduler that actually
    /// accepts ever holds a socket in the reuseport group.
    fn ensure_listener(&mut self, id: i32) -> bool {
        if self.tcp_listeners.contains_key(&id) {
            return true;
        }
        let Some(addr) = sched::lock(&self.runtime.shared_listeners)
            .get(&id)
            .copied()
        else {
            return false;
        };
        match bind_reuseport(addr).and_then(|l| self.track_listener(id, l)) {
            Ok(()) => true,
            Err(e) => {
                eprintln!("warning: cannot bind reuseport listener {id}: {e}");
                false
            }
        }
    }

    /// Take ownership of a connected stream (from accept or connect):
    /// configure it, register it in the connection table and with the
    /// poller, and build the AL `Ok(Socket)` result — or `Err(NetError)`
    /// when the poller refuses the fd (a connection that cannot wake its
    /// parks must not be adopted).
    /// Allocates `cost::ADOPT` (or `cost::NET_ERR`); callers ensure both
    /// before popping.
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
        if let Err(e) = self.track_connection(id, stream) {
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
/// per listening socket before new connections are dropped. The platform caps
/// this at `somaxconn`; a deeper request than the default keeps a connection
/// burst from overflowing the queue (the std default of 128 is shallow for a
/// server). Each `SO_REUSEPORT` socket gets its own queue of this depth.
const LISTEN_BACKLOG: i32 = 1024;

/// Bind a non-blocking TCP listener with `SO_REUSEADDR` + `SO_REUSEPORT`, so
/// every scheduler can hold its own listening socket on the same address. The
/// kernel then load-balances incoming connections across the per-scheduler
/// sockets: each core accepts from its own queue with no shared accept lock
/// and no cross-core handoff. Non-blocking so accept parks the calling process
/// rather than stalling the scheduler.
fn bind_reuseport(addr: SocketAddr) -> std::io::Result<TcpListener> {
    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    socket.set_reuse_address(true)?;
    socket.set_reuse_port(true)?;
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
            Ok(0) => {
                return Err(std::io::Error::new(
                    ErrorKind::WriteZero,
                    "connection closed",
                ));
            }
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
fn connection_mut(conns: &mut HashMap<i32, TcpStream>, id: i32) -> std::io::Result<&mut TcpStream> {
    conns.get_mut(&id).ok_or_else(stale_socket)
}

#[cold]
fn stale_socket() -> std::io::Error {
    std::io::Error::new(ErrorKind::NotConnected, "socket closed")
}
