//! `AL_PERF_MAP=1` perf-map writer: symbol lines for JIT-compiled code.
//!
//! `perf` symbolises samples in anonymous executable mappings by reading
//! `/tmp/perf-<pid>.map`, one `HEXSTART HEXSIZE name` line per JIT symbol.
//! [`record`] appends one per finalized function so JIT'd AL code attributes
//! to its AL function instead of `[unknown]`.
//!
//! Enabled by `AL_PERF_MAP=1` exactly. The file is truncated on first record,
//! so a recycled pid cannot inherit a stale map, then appended to. The handle
//! stays open for the process lifetime, like the code mapping itself.

use std::fs::File;
use std::io::Write;
use std::sync::{Mutex, OnceLock};

/// Whether `AL_PERF_MAP=1` asked for a perf map. Read once per process.
pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("AL_PERF_MAP").is_ok_and(|v| v == "1"))
}

/// The per-process map path perf reads. Literal `/tmp`, not the platform
/// temp dir: the path is perf's contract.
pub fn path() -> String {
    format!("/tmp/perf-{}.map", std::process::id())
}

/// One map line: `HEXSTART HEXSIZE al::<name>` (no `0x` prefixes).
fn line(start: usize, size: usize, name: &str) -> String {
    format!("{start:x} {size:x} al::{name}")
}

/// The open map file, created on first use. `None` when disabled or the open
/// failed, in which case recording stays a no-op: the map is best-effort.
fn file() -> Option<&'static Mutex<File>> {
    static FILE: OnceLock<Option<Mutex<File>>> = OnceLock::new();
    FILE.get_or_init(|| {
        if !enabled() {
            return None;
        }
        let path = path();
        match File::create(&path) {
            Ok(f) => Some(Mutex::new(f)),
            Err(err) => {
                eprintln!("al: AL_PERF_MAP: cannot create {path}: {err}");
                None
            }
        }
    })
    .as_ref()
}

/// Append the symbol line for one finalized function body.
pub fn record(start: usize, size: usize, name: &str) {
    if let Some(file) = file() {
        // A poisoned lock means a panic mid-write; the map is best-effort,
        // so keep appending regardless.
        let mut file = match file.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let _ = writeln!(file, "{}", line(start, size, name));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_is_hex_start_hex_size_prefixed_name() {
        assert_eq!(line(0x1234, 0x56, "fib"), "1234 56 al::fib");
        assert_eq!(line(0, 0, "x"), "0 0 al::x");
        assert_eq!(
            line(0xdead_beef_0000, 0x1c0, "count"),
            "deadbeef0000 1c0 al::count"
        );
    }

    #[test]
    fn path_is_the_perf_convention_for_this_pid() {
        assert_eq!(path(), format!("/tmp/perf-{}.map", std::process::id()));
    }
}
