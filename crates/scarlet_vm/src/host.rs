//! What a program sees of the world outside it: its arguments, its
//! environment, its output, the clocks, random bytes and files.
//!
//! The VM reaches the world only through [`Host`], so the same interpreter
//! can run on an operating system ([`crate::OsHost`]), inside a test that
//! hands in a world of its own, or one day with no operating system under it
//! at all. Nothing else in the VM calls the OS.

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
