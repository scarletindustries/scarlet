//! The [`Host`] of a program run on an operating system. This is the one
//! place in the VM that calls the OS.

use std::io::Write;
use std::path::PathBuf;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::host::{Host, IoError, NoRandomness, OutputClosed};

/// The world of this OS process, printing to `out`.
pub struct OsHost<W> {
    argv: Vec<String>,
    env: Vec<(String, String)>,
    /// What `monotonic_ms` counts from.
    started: Instant,
    out: W,
}

impl<W: Write> OsHost<W> {
    pub fn new(argv: Vec<String>, env: Vec<(String, String)>, out: W) -> Self {
        OsHost {
            argv,
            env,
            started: Instant::now(),
            out,
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
