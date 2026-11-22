//! The [`Host`] of a program run on an operating system. This is the one
//! place in the VM that calls the OS.
//!
//! There is no scheduler yet, so every wait here blocks the thread, and with
//! it the one process there is. A deadline becomes the socket's own timeout.

mod tls;

use std::collections::HashMap;
use std::io::{ErrorKind, IoSlice, Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::host::{
    Deadline, Handle, Host, IoError, IpAddr, NetError, NoRandomness, OutputClosed, Received,
    SocketAddr, TlsError,
};

/// The world of this OS process, printing to `out`.
pub struct OsHost<W> {
    argv: Vec<String>,
    env: Vec<(String, String)>,
    /// What `monotonic_ms` counts from.
    started: Instant,
    out: W,
    /// What each handle the program holds names.
    open: HashMap<Handle, Resource>,
    /// The number the next handle gets.
    next: u64,
    /// The TLS client settings, made the first time a handshake needs them.
    tls: Option<tls::Config>,
}

/// What a handle names.
enum Resource {
    Listener(TcpListener),
    Stream(TcpStream),
    Tls(Box<tls::Session>),
}

/// The most one read asks the OS for. A read gives up to its `max` bytes, so
/// a smaller one is still a read, and no read sets aside more than this.
const READ_CHUNK: usize = 64 * 1024;

impl<W: Write> OsHost<W> {
    pub fn new(argv: Vec<String>, env: Vec<(String, String)>, out: W) -> Self {
        OsHost {
            argv,
            env,
            started: Instant::now(),
            out,
            open: HashMap::new(),
            next: 0,
            tls: None,
        }
    }

    /// This process's world, with its environment as it is now and the
    /// given `argv`.
    pub fn of_this_process(argv: Vec<String>, out: W) -> Self {
        let env = std::env::vars_os()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
            .collect();
        OsHost::new(argv, env, out)
    }

    /// Where the program printed to, for flushing or reading back.
    pub fn out(&mut self) -> &mut W {
        &mut self.out
    }

    /// Push what the program printed out before a wait, so whoever reads it
    /// sees it while the program waits, like a server's "listening on" line.
    /// A failed flush is the reader gone, which the next print reports.
    fn before_waiting(&mut self) {
        let _ = self.out.flush();
    }

    fn hold(&mut self, r: Resource) -> Handle {
        let h = Handle(self.next);
        self.next = self.next.saturating_add(1);
        self.open.insert(h, r);
        h
    }

    fn stream(&mut self, h: Handle) -> Result<&mut TcpStream, NetError> {
        match self.open.get_mut(&h) {
            Some(Resource::Stream(s)) => Ok(s),
            Some(Resource::Listener(_) | Resource::Tls(_)) | None => Err(NetError::NotConnected),
        }
    }

    /// How long is left before `deadline`, or `None` to wait for ever.
    fn left(&self, deadline: Option<Deadline>) -> Result<Option<Duration>, NetError> {
        let Some(Deadline(at)) = deadline else {
            return Ok(None);
        };
        match at.checked_sub(self.monotonic_ms()) {
            Some(ms) if ms > 0 => Ok(Some(Duration::from_millis(
                u64::try_from(ms).unwrap_or(u64::MAX),
            ))),
            Some(_) | None => Err(NetError::TimedOut),
        }
    }
}

impl<W: Write> Host for OsHost<W> {
    fn argv(&self) -> &[String] {
        &self.argv
    }

    fn env(&self) -> &[(String, String)] {
        &self.env
    }

    fn print(&mut self, text: &[u8]) -> Result<(), OutputClosed> {
        self.out.write_all(text).map_err(|_| OutputClosed)
    }

    fn monotonic_ms(&self) -> u128 {
        self.started.elapsed().as_millis()
    }

    // Before 1970, `duration_since` gives how far before as an error, and we
    // put the sign back on. A `Duration` is under 2^64 seconds, so its
    // milliseconds fit an `i128` with room for the sign.
    fn epoch_ms(&self) -> i128 {
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(after) => after.as_millis().cast_signed(),
            Err(before) => before.duration().as_millis().cast_signed().saturating_neg(),
        }
    }

    // There is no scheduler yet, so this blocks the thread, and with it the
    // one process there is.
    fn sleep_ms(&mut self, ms: u64) {
        self.before_waiting();
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }

    // The bytes come from the OS's own generator, never one of ours.
    fn random_bytes(&mut self, into: &mut [u8]) -> Result<(), NoRandomness> {
        getrandom::fill(into).map_err(|_| NoRandomness)
    }

    fn terminal_size(&self) -> Option<(u16, u16)> {
        let size = rustix::termios::tcgetwinsize(std::io::stdout()).ok()?;
        (size.ws_col > 0 && size.ws_row > 0).then_some((size.ws_col, size.ws_row))
    }

    fn read_file(&mut self, path: &[u8]) -> Result<Vec<u8>, IoError> {
        std::fs::read(path_of(path)).map_err(|e| io_error(&e))
    }

    fn write_file(&mut self, path: &[u8], bytes: &[u8]) -> Result<(), IoError> {
        std::fs::write(path_of(path), bytes).map_err(|e| io_error(&e))
    }

    // getaddrinfo has no timeout of its own, so a deadline runs the lookup
    // on a thread and stops waiting for it. A lookup can't be cancelled, so a
    // late one runs to the end and its answer is thrown away. A spent budget
    // gives up before starting one.
    fn resolve(&mut self, name: &str, deadline: Option<Deadline>) -> Result<IpAddr, NetError> {
        if let Ok(ip) = name.parse::<IpAddr>() {
            return Ok(ip);
        }
        self.before_waiting();
        let Some(wait) = self.left(deadline)? else {
            return lookup(name);
        };
        let (send, answer) = std::sync::mpsc::channel();
        let name = name.to_string();
        std::thread::spawn(move || {
            let _ = send.send(lookup(&name));
        });
        answer.recv_timeout(wait).unwrap_or(Err(NetError::TimedOut))
    }

    fn listen(&mut self, addr: SocketAddr) -> Result<Handle, NetError> {
        let listener = TcpListener::bind(addr).map_err(|e| net_error(&e))?;
        Ok(self.hold(Resource::Listener(listener)))
    }

    fn accept(&mut self, listener: Handle) -> Result<Option<(Handle, SocketAddr)>, NetError> {
        self.before_waiting();
        let Some(Resource::Listener(l)) = self.open.get(&listener) else {
            return Ok(None);
        };
        let (stream, peer) = l.accept().map_err(|e| net_error(&e))?;
        Ok(Some((self.hold(Resource::Stream(stream)), peer)))
    }

    fn local_addr(&mut self, listener: Handle) -> Result<SocketAddr, NetError> {
        match self.open.get(&listener) {
            Some(Resource::Listener(l)) => l.local_addr().map_err(|e| net_error(&e)),
            Some(Resource::Stream(_) | Resource::Tls(_)) | None => Err(NetError::NotConnected),
        }
    }

    fn connect(
        &mut self,
        addr: SocketAddr,
        deadline: Option<Deadline>,
    ) -> Result<Handle, NetError> {
        self.before_waiting();
        let stream = match self.left(deadline)? {
            Some(wait) => TcpStream::connect_timeout(&addr, wait),
            None => TcpStream::connect(addr),
        }
        .map_err(|e| net_error(&e))?;
        Ok(self.hold(Resource::Stream(stream)))
    }

    fn read(
        &mut self,
        conn: Handle,
        max: usize,
        deadline: Option<Deadline>,
    ) -> Result<Received, NetError> {
        self.before_waiting();
        let wait = self.left(deadline)?;
        let stream = self.stream(conn)?;
        stream.set_read_timeout(wait).map_err(|e| net_error(&e))?;
        let mut buf = vec![0u8; max.clamp(1, READ_CHUNK)];
        loop {
            match stream.read(&mut buf) {
                Ok(0) => return Ok(Received::Closed),
                Ok(n) => {
                    buf.truncate(n);
                    return Ok(Received::Data(buf));
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(net_error(&e)),
            }
        }
    }

    fn write(&mut self, conn: Handle, parts: &[&[u8]]) -> Result<(), NetError> {
        let stream = self.stream(conn)?;
        let mut slices: Vec<IoSlice<'_>> = parts.iter().map(|p| IoSlice::new(p)).collect();
        let mut left = slices.as_mut_slice();
        while !left.is_empty() {
            match stream.write_vectored(left) {
                Ok(0) => return Err(NetError::BrokenPipe),
                Ok(n) => IoSlice::advance_slices(&mut left, n),
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(net_error(&e)),
            }
        }
        Ok(())
    }

    fn close(&mut self, h: Handle) -> Result<(), NetError> {
        if let Some(Resource::Stream(s)) = self.open.remove(&h) {
            match s.shutdown(std::net::Shutdown::Both) {
                Err(e) if e.kind() != ErrorKind::NotConnected => return Err(net_error(&e)),
                Ok(()) | Err(_) => {}
            }
        }
        Ok(())
    }

    fn tls_handshake(
        &mut self,
        conn: Handle,
        server_name: &str,
        deadline: Option<Deadline>,
    ) -> Result<Handle, TlsError> {
        self.before_waiting();
        let stream = match self.open.remove(&conn) {
            Some(Resource::Stream(s)) => s,
            Some(r @ (Resource::Listener(_) | Resource::Tls(_))) => {
                self.open.insert(conn, r);
                return Err(TlsError::Transport(NetError::NotConnected));
            }
            None => return Err(TlsError::Transport(NetError::NotConnected)),
        };
        let wait = self.left(deadline).map_err(TlsError::Transport)?;
        let config = match &self.tls {
            Some(c) => c.clone(),
            None => {
                let c = tls::Config::new()?;
                self.tls = Some(c.clone());
                c
            }
        };
        let session = tls::Session::handshake(&config, stream, server_name, wait)?;
        Ok(self.hold(Resource::Tls(Box::new(session))))
    }

    fn tls_read(
        &mut self,
        conn: Handle,
        max: usize,
        deadline: Option<Deadline>,
    ) -> Result<Received, TlsError> {
        self.before_waiting();
        let wait = self.left(deadline).map_err(TlsError::Transport)?;
        match self.open.get_mut(&conn) {
            Some(Resource::Tls(s)) => s.read(max.clamp(1, READ_CHUNK), wait),
            Some(Resource::Listener(_) | Resource::Stream(_)) | None => {
                Err(TlsError::Transport(NetError::NotConnected))
            }
        }
    }

    fn tls_write(&mut self, conn: Handle, data: &[u8]) -> Result<(), TlsError> {
        match self.open.get_mut(&conn) {
            Some(Resource::Tls(s)) => s.write(data),
            Some(Resource::Listener(_) | Resource::Stream(_)) | None => {
                Err(TlsError::Transport(NetError::NotConnected))
            }
        }
    }

    fn tls_close(&mut self, conn: Handle) -> Result<(), TlsError> {
        match self.open.remove(&conn) {
            Some(Resource::Tls(mut s)) => s.close(),
            Some(r @ (Resource::Listener(_) | Resource::Stream(_))) => {
                self.open.insert(conn, r);
                Err(TlsError::Transport(NetError::NotConnected))
            }
            None => Ok(()),
        }
    }

    fn release(&mut self, h: Handle) {
        self.open.remove(&h);
    }
}

/// The first address getaddrinfo gives for `name`.
fn lookup(name: &str) -> Result<IpAddr, NetError> {
    (name, 0u16)
        .to_socket_addrs()
        .map_err(|e| net_error(&e))?
        .next()
        .map(|a| a.ip())
        .ok_or(NetError::Errno(-1))
}

fn net_error(e: &std::io::Error) -> NetError {
    match e.raw_os_error() {
        Some(libc::ETIMEDOUT) => NetError::TimedOut,
        Some(libc::ECONNREFUSED) => NetError::ConnectionRefused,
        Some(libc::ECONNRESET) => NetError::ConnectionReset,
        Some(libc::ECONNABORTED) => NetError::ConnectionAborted,
        Some(libc::ENOTCONN) => NetError::NotConnected,
        Some(libc::EPIPE) => NetError::BrokenPipe,
        Some(libc::EADDRINUSE) => NetError::AddrInUse,
        Some(libc::EADDRNOTAVAIL) => NetError::AddrNotAvailable,
        Some(libc::ENETDOWN) => NetError::NetworkDown,
        Some(libc::ENETUNREACH) => NetError::NetworkUnreachable,
        Some(libc::EHOSTUNREACH) => NetError::HostUnreachable,
        Some(libc::EACCES) => NetError::PermissionDenied,
        // A socket timeout reads as "try again".
        Some(libc::EAGAIN) => NetError::TimedOut,
        Some(code) => NetError::Errno(code),
        None => match e.kind() {
            ErrorKind::TimedOut | ErrorKind::WouldBlock => NetError::TimedOut,
            ErrorKind::UnexpectedEof => NetError::UnexpectedEof,
            // `-1` for an error that did not come from the OS.
            _ => NetError::Errno(-1),
        },
    }
}

/// A Scarlet string is UTF-8, so this is the path it spells.
fn path_of(text: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(text).into_owned())
}

fn io_error(e: &std::io::Error) -> IoError {
    match e.raw_os_error() {
        Some(libc::ENOENT) => IoError::NotFound,
        Some(libc::EACCES) => IoError::PermissionDenied,
        Some(libc::EEXIST) => IoError::AlreadyExists,
        Some(libc::ENOTDIR) => IoError::NotADirectory,
        Some(libc::EISDIR) => IoError::IsADirectory,
        Some(libc::EROFS) => IoError::ReadOnlyFilesystem,
        Some(libc::ELOOP) => IoError::FilesystemLoop,
        Some(libc::EFBIG) => IoError::FileTooLarge,
        Some(libc::ENOSPC) => IoError::StorageFull,
        Some(libc::EDQUOT) => IoError::QuotaExceeded,
        Some(code) => IoError::Errno(code),
        // `-1` for an error that did not come from the OS.
        None => IoError::Errno(-1),
    }
}
