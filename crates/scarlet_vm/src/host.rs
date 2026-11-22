//! What a program sees of the world outside it: its arguments, its
//! environment, its output, the clocks, random bytes and files.
//!
//! The VM reaches the world only through [`Host`], so the same interpreter
//! can run on an operating system ([`crate::OsHost`]), inside a test that
//! hands in a world of its own, or one day with no operating system under it
//! at all. Nothing else in the VM calls the OS.
//!
//! A socket, a listener or a TLS session lives in the host, and the program
//! holds a [`Handle`] to it. The VM tells the host with [`Host::release`] when
//! nothing holds a handle any more, so the host can close what it names.

pub use core::net::{IpAddr, SocketAddr};

/// The world one run sees. The driver makes one and hands it to
/// [`crate::run`].
pub trait Host {
    /// `os.argv`: the path of the entrypoint that was run, then the arguments
    /// after it.
    fn argv(&self) -> &[String];

    /// `os.env`: each variable whose name and value are both UTF-8, in the
    /// order the world lists them. A name may be listed twice.
    fn env(&self) -> &[(String, String)];

    /// Write what `print` and `println` show.
    fn print(&mut self, text: &[u8]) -> Result<(), OutputClosed>;

    /// Milliseconds since the run began, on a clock that only goes forward.
    fn monotonic_ms(&self) -> u128;

    /// Milliseconds since 1970 began in UTC, negative before it.
    fn epoch_ms(&self) -> i128;

    /// Wait `ms` milliseconds.
    fn sleep_ms(&mut self, ms: u64);

    /// Fill `into` from a generator fit for keys. Never fill it any other way.
    fn random_bytes(&mut self, into: &mut [u8]) -> Result<(), NoRandomness>;

    /// The output's terminal size as (columns, rows), or `None` when it is
    /// not a terminal or reports no size.
    fn terminal_size(&self) -> Option<(u16, u16)>;

    /// The whole file at `path`.
    fn read_file(&mut self, path: &[u8]) -> Result<Vec<u8>, IoError>;

    /// Make the file at `path` hold `bytes` and nothing else.
    fn write_file(&mut self, path: &[u8], bytes: &[u8]) -> Result<(), IoError>;

    /// The address `name` stands for. An IP literal is itself.
    fn resolve(&mut self, name: &str, deadline: Option<Deadline>) -> Result<IpAddr, NetError>;

    /// A TCP listener bound to `addr`.
    fn listen(&mut self, addr: SocketAddr) -> Result<Handle, NetError>;

    /// The next connection to `listener`, and who it came from. `None` means
    /// the listener was closed.
    fn accept(&mut self, listener: Handle) -> Result<Option<(Handle, SocketAddr)>, NetError>;

    /// The address `listener` is bound to.
    fn local_addr(&mut self, listener: Handle) -> Result<SocketAddr, NetError>;

    /// A TCP connection to `addr`.
    fn connect(&mut self, addr: SocketAddr, deadline: Option<Deadline>)
    -> Result<Handle, NetError>;

    /// Up to `max` bytes from `conn`, at least one, or the news that the peer
    /// closed. `max` is at least 1.
    fn read(
        &mut self,
        conn: Handle,
        max: usize,
        deadline: Option<Deadline>,
    ) -> Result<Received, NetError>;

    /// Write every byte of `parts`, in order, to `conn`.
    fn write(&mut self, conn: Handle, parts: &[&[u8]]) -> Result<(), NetError>;

    /// Close the listener or connection `h`. Closing what is already closed
    /// is not an error.
    fn close(&mut self, h: Handle) -> Result<(), NetError>;

    /// Run a TLS handshake over `conn`, checking the peer's certificate is
    /// for `server_name`. It takes `conn` over, whatever the outcome, so a
    /// cleartext call on it fails with [`NetError::NotConnected`] afterwards.
    fn tls_handshake(
        &mut self,
        conn: Handle,
        server_name: &str,
        deadline: Option<Deadline>,
    ) -> Result<Handle, TlsError>;

    /// [`Host::read`], of plaintext from the TLS session `conn`.
    fn tls_read(
        &mut self,
        conn: Handle,
        max: usize,
        deadline: Option<Deadline>,
    ) -> Result<Received, TlsError>;

    /// Write `data` to the TLS session `conn`, returning once its ciphertext
    /// reaches the network.
    fn tls_write(&mut self, conn: Handle, data: &[u8]) -> Result<(), TlsError>;

    /// Tell the peer the session is ending, and close it. It closes either
    /// way: an `Err` only means the peer was not told.
    fn tls_close(&mut self, conn: Handle) -> Result<(), TlsError>;

    /// Nothing in the program holds `h` any more.
    fn release(&mut self, h: Handle);
}

/// Something the host holds for the program, like a socket. The host picks
/// the number, and never gives the same one twice in a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Handle(pub u64);

/// When a wait gives up: a time on the clock [`Host::monotonic_ms`] reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Deadline(pub u128);

/// What a read got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Received {
    /// Some bytes, never none.
    Data(Vec<u8>),
    /// The peer closed.
    Closed,
}

/// Why a network call failed. These are `scarlet/net/error`'s `NetError`s
/// that the world gives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetError {
    /// The deadline passed, or the OS gave up.
    TimedOut,
    ConnectionRefused,
    ConnectionReset,
    ConnectionAborted,
    NotConnected,
    BrokenPipe,
    AddrInUse,
    AddrNotAvailable,
    NetworkDown,
    NetworkUnreachable,
    HostUnreachable,
    PermissionDenied,
    /// The peer closed before the call was done.
    UnexpectedEof,
    /// Any other error, by the world's own number for it.
    Errno(i32),
}

/// Why a TLS call failed. These are `scarlet/net/tls`'s `TlsError`s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsError {
    CertificateUnknownIssuer,
    CertificateExpired,
    CertificateNotYetValid,
    CertificateRevoked,
    HostnameMismatch,
    BadCertificate,
    ProtocolError,
    HandshakeFailed,
    InvalidServerName,
    Transport(NetError),
}

/// The output was closed, like a pipe into `head` that has read enough.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputClosed;

/// The world could not give random bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoRandomness;

/// Why a file could not be read or written. These are `scarlet/io`'s
/// `IoError`s that the world gives; the VM adds the path, and the ones it
/// finds itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoError {
    NotFound,
    PermissionDenied,
    AlreadyExists,
    NotADirectory,
    IsADirectory,
    ReadOnlyFilesystem,
    /// Too many symbolic links.
    FilesystemLoop,
    FileTooLarge,
    StorageFull,
    QuotaExceeded,
    /// Any other error, by the world's own number for it.
    Errno(i32),
}
