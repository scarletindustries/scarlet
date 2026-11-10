#![allow(dead_code)]
// Helpers shared by the integration-test binaries. They live in a `tests/`
// subdirectory so Cargo treats them as a module, not a test target.

pub mod graph;
pub mod net;

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::time::{Duration, Instant};

use scarlet::{ast, parser, scanner};

/// Stable 64-bit hash of a string, used to derive per-thread temp-file names.
fn hash_str(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// Write `source` to a uniquely-named temp `.sca` file and return its path.
/// The pid separates test binaries and the thread-id hash separates parallel
/// threads; same-thread tests run sequentially, so that pair is enough.
fn write_temp(source: &str) -> std::path::PathBuf {
    let mut tmp = std::env::temp_dir();
    let pid = std::process::id();
    let tid = format!("{:?}", std::thread::current().id());
    tmp.push(format!("scarlet_{pid}_{}.sca", hash_str(&tid)));
    let mut f = std::fs::File::create(&tmp).expect("create temp file");
    f.write_all(source.as_bytes()).expect("write temp file");
    tmp
}

/// Captured output of one `scarlet` subprocess invocation.
pub struct AlOutput {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
    pub code: Option<i32>,
}

impl AlOutput {
    /// Returns stdout followed by stderr, because diagnostics can land on either stream.
    pub fn combined(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }
}

/// The longest any `scarlet` subprocess a test spawns may run. It's generous on
/// purpose, because under a parallel `cargo test` a tight bound turns load into a
/// spurious kill. It only has to make a wedged child fail one test instead of
/// hanging the whole run.
pub const CHILD_TIMEOUT_SECS: u64 = 120;

/// Kills its child on drop, so a panicking or early-returning test still reaps
/// the `scarlet` it spawned. A SIGKILLed parent runs no `Drop`, so this can still miss.
pub struct ChildGuard(pub Option<Child>);

impl ChildGuard {
    pub fn new(child: Child) -> Self {
        ChildGuard(Some(child))
    }
    /// Take the child back, cancelling the kill-on-drop.
    pub fn release(mut self) -> Child {
        self.0.take().expect("child already taken")
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut c) = self.0.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// Runs `al <subcommand> <path>` and kills it after [`CHILD_TIMEOUT_SECS`].
///
/// It doesn't use `Command::output()`, which waits forever, so a program that blocks (a
/// server example, a `net.connect` to a listener a miscompile never accepted)
/// would wedge the whole test binary.
pub fn run_al(subcommand: &str, path: &Path) -> AlOutput {
    let bin = env!("CARGO_BIN_EXE_scarlet");
    let child = Command::new(bin)
        .arg(subcommand)
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn al");
    let out = wait_or_kill(child, CHILD_TIMEOUT_SECS);
    // No exit code means the child died by signal, either a reaped wedge or a
    // VM crash. Both would otherwise look like "produced no output".
    assert!(
        out.status.code().is_some(),
        "`al {subcommand} {}` died by signal (wedged past {CHILD_TIMEOUT_SECS}s, or crashed)\n\
         stdout: {}\nstderr: {}",
        path.display(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    AlOutput {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        success: out.status.success(),
        code: out.status.code(),
    }
}

/// Wait up to `secs` for a self-terminating `child` to exit, then collect its
/// output.
///
/// The pipes MUST be drained while waiting, not after. A pipe holds about 64 KiB,
/// and a child that writes more blocks in `write` until someone reads, so a parent
/// polling `try_wait` without reading deadlocks against its own child. `al
/// check` on a 60k-term `else if` chain renders a 1.1 MB diagnostic and hits
/// this instantly.
pub fn wait_or_kill(mut child: Child, secs: u64) -> Output {
    let drain = |s: Option<_>| -> Option<std::thread::JoinHandle<Vec<u8>>> {
        s.map(|mut r: Box<dyn std::io::Read + Send>| {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = r.read_to_end(&mut buf);
                buf
            })
        })
    };
    let out_t = drain(
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
    );
    let err_t = drain(
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
    );

    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => break,
        }
    }
    // This does nothing if the child already exited. Killing it closes the
    // pipes, so the drain threads see EOF and join.
    child.kill().ok();
    let status = child.wait().expect("wait for child");
    Output {
        status,
        stdout: out_t
            .map(|t| t.join().unwrap_or_default())
            .unwrap_or_default(),
        stderr: err_t
            .map(|t| t.join().unwrap_or_default())
            .unwrap_or_default(),
    }
}

/// Write `source` to a temp file, run `al <cmd>` on it, and remove the file.
pub fn run_source(cmd: &str, source: &str) -> AlOutput {
    let path = write_temp(source);
    let out = run_al(cmd, &path);
    let _ = std::fs::remove_file(&path);
    out
}

/// Formats `source` and the captured streams for an assertion failure message.
fn dump(source: &str, out: &AlOutput) -> String {
    format!(
        "{source}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        out.stdout, out.stderr
    )
}

/// Asserts `out` is a *clean* rejection. It must exit with failure and a real exit
/// code (never a signal), not panic, and print a diagnostic matching one of
/// `expected_diags` on either stream. An empty entry checks only the clean rejection.
pub fn assert_rejects(out: &AlOutput, cmd: &str, source: &str, expected_diags: &[&str]) {
    assert!(
        !out.success,
        "expected `al {cmd}` to REJECT:\n{}",
        dump(source, out)
    );
    assert!(
        out.code.is_some(),
        "process was killed by a signal instead of rejecting cleanly:\n{}",
        dump(source, out)
    );
    let combined = out.combined();
    assert!(
        !combined.contains("panicked"),
        "panicked instead of rejecting cleanly:\n{}",
        dump(source, out)
    );
    assert!(
        expected_diags
            .iter()
            .any(|d| d.is_empty() || combined.contains(d)),
        "expected output to contain one of {expected_diags:?} for:\n{source}\n--- output ---\n{combined}"
    );
}

/// Assert `al check` rejects `source` cleanly with a diagnostic containing
/// `expected_diag`. See [`assert_rejects`] for the full contract.
pub fn check_rejects(source: &str, expected_diag: &str) -> AlOutput {
    let out = run_source("check", source);
    assert_rejects(&out, "check", source, &[expected_diag]);
    out
}

/// Expand each `name: (src, msg)` entry to a `#[test]` fn asserting `al check`
/// rejects `src` with a diagnostic containing `msg`.
#[macro_export]
macro_rules! reject_case {
    ( $( $(#[$m:meta])* $name:ident: ($src:expr, $msg:expr $(,)?) ),* $(,)? ) => {
        $(
            $(#[$m])*
            #[test]
            fn $name() { $crate::common::check_rejects($src, $msg); }
        )*
    };
}

/// Works like `reject_case!`, but for runtime rejections, via [`run_rejects`].
#[macro_export]
macro_rules! run_reject_case {
    ( $( $(#[$m:meta])* $name:ident: ($src:expr, $msg:expr $(,)?) ),* $(,)? ) => {
        $(
            $(#[$m])*
            #[test]
            fn $name() { $crate::common::run_rejects($src, $msg); }
        )*
    };
}

/// Assert `al run` rejects `source` cleanly with a diagnostic containing
/// `expected_diag`. See [`assert_rejects`] for the full contract.
pub fn run_rejects(source: &str, expected_diag: &str) -> AlOutput {
    let out = run_source("run", source);
    assert_rejects(&out, "run", source, &[expected_diag]);
    out
}

/// Runs `al <cmd>` on the project file `entry` and asserts it fails cleanly with
/// a diagnostic matching one of `msgs`.
pub fn project_rejects(proj: &Project, cmd: &str, entry: &str, msgs: &[&str]) {
    let r = run_al(cmd, &proj.dir.join(entry));
    assert_rejects(&r, cmd, entry, msgs);
}

/// Runs `al <cmd>` on the project file `entry` and asserts stdout is exactly `expected`.
pub fn run_project_outputs(proj: &Project, cmd: &str, entry: &str, expected: &str) {
    let r = run_al(cmd, &proj.dir.join(entry));
    assert!(r.success, "stdout: {}\nstderr: {}", r.stdout, r.stderr);
    assert_eq!(r.stdout, expected, "stderr: {}", r.stderr);
}

/// Works like `reject_case!`, but asserts `al check` accepts each source, via [`check_ok`].
#[macro_export]
macro_rules! ok_case {
    ( $( $(#[$m:meta])* $name:ident: ($src:expr $(,)?) ),* $(,)? ) => {
        $(
            $(#[$m])*
            #[test]
            fn $name() { $crate::common::check_ok($src); }
        )*
    };
}

/// Assert `al check` accepts `source` (success exit).
pub fn check_ok(source: &str) {
    let out = run_source("check", source);
    assert!(
        out.success,
        "expected `al check` to ACCEPT:\n{}",
        dump(source, &out)
    );
}

/// Assert `al run` succeeds (exit 0) and its stdout is exactly `expected`.
pub fn run_outputs(source: &str, expected: &str) {
    let out = run_source("run", source);
    let (stdout, stderr, code) = (&out.stdout, &out.stderr, out.code);
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

/// Expand each `name: (src, expected)` entry to a `#[test]` fn asserting `al
/// run` on `src` prints exactly `expected`.
#[macro_export]
macro_rules! run_case {
    ( $( $(#[$m:meta])* $name:ident: ($src:expr, $expected:expr $(,)?) ),* $(,)? ) => {
        $(
            $(#[$m])*
            #[test]
            fn $name() { $crate::common::run_outputs($src, $expected); }
        )*
    };
}

/// Parse `src` as a whole program, asserting it is diagnostic-free, and wrap
/// the block as an `Expression`.
pub fn parse(src: &str) -> ast::Expression {
    let mut sc = scanner::new_scanner(src.to_string());
    let p = parser::new_parser(&mut sc);
    let r = p.parse_program();
    assert!(
        r.diagnostics.is_empty(),
        "parse errors: {:?}\n---\n{src}",
        r.diagnostics
    );
    ast::Expression::BlockExpression(r.ast)
}

/// Line-by-line diff of a golden `want` against `got`, for a snapshot
/// mismatch panic. Lines are debug-printed so trailing whitespace is visible.
pub fn diff_lines(want: &str, got: &str) -> String {
    let w: Vec<_> = want.lines().collect();
    let g: Vec<_> = got.lines().collect();
    let mut out = String::new();
    for i in 0..w.len().max(g.len()) {
        let wl = w.get(i).copied().unwrap_or("<missing>");
        let gl = g.get(i).copied().unwrap_or("<missing>");
        if wl != gl {
            out.push_str(&format!(
                "  line {}: - want {wl:?}\n           + got  {gl:?}\n",
                i + 1
            ));
        }
    }
    // Equal line counts and no differing line means the strings differ only
    // by a trailing newline, which `lines()` does not surface.
    if out.is_empty() {
        out.push_str("  (line content matches but trailing newline differs)\n");
    }
    out
}

/// The canonical key of an on-disk module. A module is the file it resolved
/// to, so tests that address one by name must go through here.
pub fn module_key(dir: &Path, file: &str) -> scarlet::module::ModuleKey {
    scarlet::module::ModuleKey::for_file(&dir.join(file))
}

/// 0-based `(line, col)` of the `nth` (1-based) occurrence of `needle`, nudged
/// `into` columns right so the cursor lands *inside* the identifier's span.
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

/// A throwaway on-disk project directory for tests that check several files.
/// Each one gets a unique name so concurrent tests never collide, and `Drop` removes it.
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
    /// Write `src` to `prog.sca` inside the project and `al run` it.
    pub fn run(&self, src: &str) -> AlOutput {
        let p = self.dir.join("prog.sca");
        std::fs::write(&p, src).unwrap();
        run_al("run", &p)
    }
    pub fn write(&self, name: &str, src: &str) {
        let path = self.dir.join(name);
        fs::write(&path, src).unwrap();
        // Give each write a strictly later mtime. `source_changed` only looks at
        // `(mtime, len)`, so on a filesystem with coarse mtimes it would silently
        // miss a same-length edit made within the same tick.
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
