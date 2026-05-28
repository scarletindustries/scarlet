#![allow(dead_code)]
// Shared helpers for the integration-test binaries. Lives in a `tests/`
// subdirectory so Cargo treats it as a module (`mod common;`) rather than a
// standalone test target.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use al::{ast, parser, scanner};

/// Stable 64-bit hash of a string, used to derive per-thread temp-file names.
fn hash_str(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// Write `source` to a uniquely-named temp `.al` file and return its path.
/// `tag` separates concurrent tests within a binary, the pid separates test
/// binaries, and the thread-id hash separates parallel test threads.
pub fn write_temp(tag: &str, source: &str) -> std::path::PathBuf {
    let mut tmp = std::env::temp_dir();
    let pid = std::process::id();
    let tid = format!("{:?}", std::thread::current().id());
    tmp.push(format!("al_{tag}_{pid}_{}.al", hash_str(&tid)));
    let mut f = std::fs::File::create(&tmp).expect("create temp file");
    f.write_all(source.as_bytes()).expect("write temp file");
    tmp
}

/// Captured output of one `al` subprocess invocation.
pub struct AlOutput {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
    pub code: Option<i32>,
}

impl AlOutput {
    /// stdout followed by stderr — diagnostics may land on either stream.
    pub fn combined(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }
}

/// Spawn `al <subcommand> <path>` and capture its output.
pub fn run_al(subcommand: &str, path: &Path) -> AlOutput {
    let bin = env!("CARGO_BIN_EXE_al");
    let out = Command::new(bin)
        .arg(subcommand)
        .arg(path)
        .output()
        .expect("spawn al");
    AlOutput {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        success: out.status.success(),
        code: out.status.code(),
    }
}

/// Assert `al check` rejects `source` (failure exit, code 1) and that the
/// combined stdout+stderr contains `expected_substring`.
pub fn check_fails(tag: &str, source: &str, expected_substring: &str) {
    let path = write_temp(tag, source);
    let out = run_al("check", &path);
    let _ = std::fs::remove_file(&path);

    let stdout = &out.stdout;
    let stderr = &out.stderr;
    let combined = out.combined();

    assert!(
        !out.success,
        "expected `al check` to REJECT:\n{source}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
    assert_eq!(
        out.code,
        Some(1),
        "expected exit code 1 for:\n{source}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
    assert!(
        combined.contains(expected_substring),
        "expected output to contain {expected_substring:?} for:\n{source}\n--- output ---\n{combined}"
    );
}

/// Assert `al check` rejects `source` (failure exit). Diagnostic text and the
/// exact exit code are not pinned.
pub fn check_rejects(tag: &str, source: &str) {
    let path = write_temp(tag, source);
    let out = run_al("check", &path);
    let _ = std::fs::remove_file(&path);

    let stdout = &out.stdout;
    let stderr = &out.stderr;

    assert!(
        !out.success,
        "expected `al check` to REJECT:\n{source}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
}

/// Assert `al check` accepts `source` (success exit).
pub fn check_ok(tag: &str, source: &str) {
    let path = write_temp(tag, source);
    let out = run_al("check", &path);
    let _ = std::fs::remove_file(&path);

    let stdout = &out.stdout;
    let stderr = &out.stderr;

    assert!(
        out.success,
        "expected `al check` to ACCEPT:\n{source}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
}

/// Assert `al run` succeeds (exit 0) and its stdout is exactly `expected`.
pub fn run_outputs(tag: &str, source: &str, expected: &str) {
    let path = write_temp(tag, source);
    let out = run_al("run", &path);
    let _ = std::fs::remove_file(&path);

    let stdout = &out.stdout;
    let stderr = &out.stderr;
    let code = out.code;

    assert!(
        out.success,
        "expected `al run` to succeed for:\n{source}\n\
         --- exit ---\n{code:?}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
    assert_eq!(
        out.stdout, expected,
        "wrong runtime output for:\n{source}\n\
         --- expected ---\n{expected}\n--- got ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
}

/// Parse `src` as a whole program, asserting it is diagnostic-free, and wrap
/// the resulting block as the `Expression` that `IncrementalSession` and the
/// reference-graph query API consume.
pub fn parse(src: &str) -> ast::Expression {
    let mut sc = scanner::new_scanner(src.to_string());
    let mut p = parser::new_parser(&mut sc);
    let r = p.parse_program();
    assert!(
        r.diagnostics.is_empty(),
        "parse errors: {:?}\n---\n{src}",
        r.diagnostics
    );
    ast::Expression::BlockExpression(r.ast)
}

/// 0-based `(line, col)` of the `nth` (1-based) occurrence of `needle`,
/// nudged `into` columns to the right so the cursor lands *inside* the
/// identifier's span (matching the editor convention the graph expects).
/// Pass `into = 0` to point at the first char of the match.
pub fn cursor(src: &str, needle: &str, nth: usize, into: i32) -> (i32, i32) {
    let mut from = 0usize;
    let mut at = None;
    for _ in 0..nth {
        let rel = src[from..].find(needle).expect("needle present");
        at = Some(from + rel);
        from += rel + needle.len();
    }
    let b = at.expect("requested occurrence exists");
    let line = src[..b].matches('\n').count() as i32;
    let line_start = src[..b].rfind('\n').map(|i| i + 1).unwrap_or(0);
    (line, (b - line_start) as i32 + into)
}

/// A throwaway on-disk project directory for `IncrementalSession` tests.
/// `new` makes a uniquely-named temp dir (process id, the caller's `tag`, a
/// per-process sequence counter and a nanosecond stamp, so concurrent tests
/// never collide); `Drop` removes it.
pub struct Project {
    pub dir: PathBuf,
}

impl Project {
    pub fn new(tag: &str) -> Self {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "al_proj_{}_{tag}_{n}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Project { dir }
    }
    pub fn write(&self, name: &str, src: &str) {
        let path = self.dir.join(name);
        fs::write(&path, src).unwrap();
        // Force a strictly-increasing mtime independent of filesystem clock
        // resolution. `source_changed` is stat-gated on `(mtime, len)`, so a
        // same-length content edit (e.g. `{ 1 }` -> `{ 2 }` in
        // `three_module_incremental`) is silently dropped on a coarse-mtime FS
        // when the surrounding sequence completes within one tick. A monotonic
        // per-write counter spaces each rewrite's mtime well past any FS
        // granularity, so the gate is deterministically exercised everywhere.
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        fs::File::open(&path)
            .unwrap()
            .set_modified(
                std::time::SystemTime::now() + std::time::Duration::from_secs(3 * (n + 1)),
            )
            .unwrap();
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}
