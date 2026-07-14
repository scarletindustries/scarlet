//! `AL_PERF_MAP=1` perf-map writer: symbol lines for JIT-compiled code.
//!
//! Linux `perf` (and compatible profilers) symbolise samples landing in
//! anonymous executable mappings by reading `/tmp/perf-<pid>.map`, one line
//! per JIT symbol: `HEXSTART HEXSIZE name` (the wasmtime perfmap pattern).
//! [`record`] appends one such line per finalized function, named
//! `al::<fnname>`, so profiles of JIT'd AL code attribute to the AL function
//! instead of `[unknown]` — the seam the IPC measurements need.
//!
//! Enabled by `AL_PERF_MAP=1` exactly; unset or any other value keeps it off
//! and [`record`] free. The file is truncated on first record (a recycled
//! pid must not inherit a stale map) and appended to afterwards, so
//! multi-unit programs (REPL sessions) accumulate one line per compiled
//! body. The handle stays open for the process lifetime, matching the
//! immortal code mapping the addresses point into.

use std::fs::File;
use std::io::Write;
use std::sync::{Mutex, OnceLock};

/// Whether `AL_PERF_MAP=1` asked for a perf map. Read once per process.
pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("AL_PERF_MAP").is_ok_and(|v| v == "1"))
}

/// The per-process map path perf reads: `/tmp/perf-<pid>.map`. Literal
/// `/tmp`, not the platform temp dir — the path is perf's contract.
pub fn path() -> String {
    format!("/tmp/perf-{}.map", std::process::id())
}

/// One map line: `HEXSTART HEXSIZE al::<name>` (no `0x` prefixes).
fn line(start: usize, size: usize, name: &str) -> String {
    format!("{start:x} {size:x} al::{name}")
}

/// The open map file, created on first use. `None` when disabled or the open
/// failed (warned once on stderr; recording then stays a no-op — the map is
/// best-effort diagnostics, never worth failing the program over).
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

/// Append the symbol line for one finalized function body. Called by the JIT
/// finalize step with the published code address, the machine-code size the
/// definer captured, and the source-level function name.
pub fn record(start: usize, size: usize, name: &str) {
    if let Some(file) = file() {
        // A poisoned lock means a panic mid-write elsewhere; the map is
        // best-effort diagnostics, so keep appending regardless.
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
