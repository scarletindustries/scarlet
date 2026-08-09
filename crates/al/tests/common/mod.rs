#![allow(dead_code)]
// Shared helpers for the integration-test binaries. Lives in a `tests/`
// subdirectory so Cargo treats it as a module, not a test target.

pub mod net;

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::time::{Duration, Instant};

use al::{ast, parser, scanner};

/// Stable 64-bit hash of a string, used to derive per-thread temp-file names.
fn hash_str(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// Write `source` to a uniquely-named temp `.al` file and return its path.
/// The pid separates test binaries and the thread-id hash separates parallel
/// threads; same-thread tests run sequentially, so that pair is enough.
fn write_temp(source: &str) -> std::path::PathBuf {
    let mut tmp = std::env::temp_dir();
    let pid = std::process::id();
    let tid = format!("{:?}", std::thread::current().id());
    tmp.push(format!("al_{pid}_{}.al", hash_str(&tid)));
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

/// Ceiling on any `al` subprocess a test spawns. Deliberately generous: under
/// a parallel `cargo test` a tight bound turns load into a spurious kill. The
/// point is only that a wedged child fails one test instead of hanging the run.
pub const CHILD_TIMEOUT_SECS: u64 = 120;

/// Kills its child on drop, so a panicking or early-returning test still reaps
/// the `al` it spawned. A SIGKILLed parent runs no `Drop`, so this is not total.
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

/// `al <subcommand> <path>`, bounded by [`CHILD_TIMEOUT_SECS`].
///
/// Not `Command::output()`: that waits forever, so a program that blocks (a
/// server example, a `net.connect` to a listener a miscompile never accepted)
/// would wedge the whole test binary.
pub fn run_al(subcommand: &str, path: &Path) -> AlOutput {
    let bin = env!("CARGO_BIN_EXE_al");
    let child = Command::new(bin)
        .arg(subcommand)
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn al");
    let out = wait_or_kill(child, CHILD_TIMEOUT_SECS);
    // No exit code means the child died by signal — a reaped wedge or a VM
    // crash. Both would otherwise read as "produced no output".
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
/// The pipes MUST be drained while waiting, not after. A pipe holds ~64 KiB; a
/// child that writes more blocks in `write` until someone reads, so a parent
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
    // No-op if it already exited. Killing closes the pipes, so the drain
    // threads see EOF and join.
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

/// `source` followed by the captured streams, for assertion-failure messages.
fn dump(source: &str, out: &AlOutput) -> String {
    format!(
        "{source}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        out.stdout, out.stderr
    )
}

/// Assert `out` is a *clean* rejection: failure exit, a real exit code (never a
/// signal), no Rust panic, and a diagnostic matching one of `expected_diags` on
/// either stream. An empty entry pins only the clean-rejection contract.
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

/// The `reject_case!` sibling for runtime rejections, via [`run_rejects`].
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

/// `al <cmd>` the project file `entry` and assert it fails cleanly with a
/// diagnostic matching one of `msgs`.
pub fn project_rejects(proj: &Project, cmd: &str, entry: &str, msgs: &[&str]) {
    let r = run_al(cmd, &proj.dir.join(entry));
    assert_rejects(&r, cmd, entry, msgs);
}

/// `al <cmd>` the project file `entry`, asserting exactly `expected` on stdout.
pub fn run_project_outputs(proj: &Project, cmd: &str, entry: &str, expected: &str) {
    let r = run_al(cmd, &proj.dir.join(entry));
    assert!(r.success, "stdout: {}\nstderr: {}", r.stdout, r.stderr);
    assert_eq!(r.stdout, expected, "stderr: {}", r.stderr);
}

/// The accepting counterpart of `reject_case!`, via [`check_ok`].
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
/// the block as the `Expression` `IncrementalSession` consumes.
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

/// A throwaway on-disk project directory for `IncrementalSession` tests.
/// Uniquely named so concurrent tests never collide; `Drop` removes it.
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
    /// Write `src` to `prog.al` inside the project and `al run` it.
    pub fn run(&self, src: &str) -> AlOutput {
        let p = self.dir.join("prog.al");
        std::fs::write(&p, src).unwrap();
        run_al("run", &p)
    }
    pub fn write(&self, name: &str, src: &str) {
        let path = self.dir.join(name);
        fs::write(&path, src).unwrap();
        // Force a strictly-increasing mtime. `source_changed` is stat-gated
        // on `(mtime, len)`, so a same-length edit is silently dropped on a
        // coarse-mtime FS when the sequence finishes within one tick.
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

use al::bytecode::{CompileResult, IncrementalSession};
use al::module::{self, ModulePath};
use al::reference::{DefId, Definition, EntityKind, ModuleId, ReferenceGraph};
use al::span::Span;

/// Fresh session checking `entry` against project `p`, asserting success.
pub fn checked_with(p: &Project, entry: &str) -> IncrementalSession {
    let mut s = IncrementalSession::new(&al::STDLIB);
    let r = s.check(&parse(entry), Some(&p.dir));
    assert!(r.success(), "compile failed: {:?}", r.diagnostics);
    s
}

/// Re-check `entry` against project `p` in the existing session, returning the
/// full result so callers can assert success or failure.
pub fn recheck(s: &mut IncrementalSession, p: &Project, entry: &str) -> CompileResult {
    s.check(&parse(entry), Some(&p.dir))
}

/// A document/workspace symbol projected from a graph `Definition`.
#[derive(Debug, Clone)]
pub struct SymbolInfo {
    pub name: String,
    pub kind: EntityKind,
    pub module: ModulePath,
    pub span: Span,
}

/// (name, kind) pairs — the debug tail for `assert!`s over a symbol list.
pub fn sym_names(syms: &[SymbolInfo]) -> Vec<(&str, EntityKind)> {
    syms.iter().map(|s| (s.name.as_str(), s.kind)).collect()
}

/// Assert `syms` includes a `name` of `kind`; dumps every (name, kind) on failure.
pub fn assert_has_sym(syms: &[SymbolInfo], name: &str, kind: EntityKind) {
    assert!(
        syms.iter().any(|s| s.name == name && s.kind == kind),
        "missing symbol `{name}` ({kind:?}) in {:?}",
        sym_names(syms)
    );
}

/// Assert at least one of `msgs` contains `needle`.
pub fn assert_has_msg(msgs: &[String], needle: &str) {
    assert!(
        msgs.iter().any(|m| m.contains(needle)),
        "expected a message containing `{needle}`: {msgs:?}"
    );
}

/// Assert at least one of `msgs` is exactly `exact` (not a substring match).
pub fn assert_msg_eq(msgs: &[String], exact: &str) {
    assert!(
        msgs.iter().any(|m| m == exact),
        "expected a message == `{exact}`: {msgs:?}"
    );
}

/// Assert no message in `msgs` contains `needle`.
pub fn assert_no_msg(msgs: &[String], needle: &str) {
    assert!(
        !msgs.iter().any(|m| m.contains(needle)),
        "unexpected message containing `{needle}`: {msgs:?}"
    );
}

/// The canonical key of an on-disk module. A module is the file it resolved
/// to, so tests that address one by name must go through here.
pub fn module_key(dir: &std::path::Path, file: &str) -> module::ModuleKey {
    module::ModuleKey::for_file(&dir.join(file))
}

fn module_for(g: &ReferenceGraph, module_or_uri: &str) -> Option<ModuleId> {
    // 1. The canonical key, exactly (lookup-not-mint, like the LSP boundary).
    if let Some(id) = g.module_id_by_key(&module::ModuleKey::from_lookup_str(module_or_uri)) {
        return Some(id);
    }
    // 2. A path to the source file — what the LSP passes, via a URI.
    let p = std::path::Path::new(module_or_uri);
    if p.is_file()
        && let Some(id) = g.module_id_by_key(&module::ModuleKey::for_file(p))
    {
        return Some(id);
    }
    // 3. Test ergonomics only: a written name (`./c`, `b`). A module's name is
    // NOT its identity, so this matches the last path segment and panics on
    // ambiguity rather than silently picking one.
    let want = module_or_uri.rsplit('/').next().unwrap_or(module_or_uri);
    let hits: Vec<ModuleId> = g
        .modules()
        .filter(|(_, path)| path.last().map(String::as_str) == Some(want))
        .map(|(id, _)| id)
        .collect();
    match hits.as_slice() {
        [one] => return Some(*one),
        [] => {}
        many => panic!(
            "module name {module_or_uri:?} is ambiguous across {} modules;              pass the source path instead",
            many.len()
        ),
    }
    g.module_id_by_key(&module::ModuleKey::main())
}

fn symbol_of(g: &ReferenceGraph, d: &Definition) -> Option<SymbolInfo> {
    if !d.is_symbol_listable() {
        return None;
    }
    Some(SymbolInfo {
        name: d.name.clone(),
        kind: d.entity(),
        module: g.module_path(d.defid.module)?.clone(),
        span: d.span(),
    })
}

/// String-keyed LSP-style queries answered from
/// [`IncrementalSession::reference_graph`]. The production server queries the
/// graph directly with resolved `ModuleId`s; these key by module path and fall
/// back to the entry `main` module for the single-open-file case.
pub trait SessionQueryExt {
    fn definition(&self, module_or_uri: &str, line: i32, col: i32) -> Option<(ModulePath, Span)>;
    fn prepare_rename(&self, module_or_uri: &str, line: i32, col: i32) -> Option<(DefId, Span)>;
    fn references(&self, def: DefId) -> Vec<(ModulePath, Span)>;
    fn rename(&self, def: DefId) -> Vec<(ModulePath, Span)>;
    fn document_symbols(&self, module_or_uri: &str) -> Vec<SymbolInfo>;
    fn workspace_symbols(&self, query: &str) -> Vec<SymbolInfo>;
}

impl SessionQueryExt for IncrementalSession {
    /// goto-definition: the occurrence under the cursor, resolved through any
    /// import alias to its canonical declaration.
    fn definition(&self, module_or_uri: &str, line: i32, col: i32) -> Option<(ModulePath, Span)> {
        let g = self.reference_graph();
        let m = module_for(g, module_or_uri)?;
        let id = g.canonical(g.def_id_at(m, line, col)?);
        Some((g.module_path(id.module)?.clone(), id.span))
    }

    /// The alias-preserving `DefId` under the cursor plus the span of the
    /// smallest enclosing occurrence.
    fn prepare_rename(&self, module_or_uri: &str, line: i32, col: i32) -> Option<(DefId, Span)> {
        let g = self.reference_graph();
        let m = module_for(g, module_or_uri)?;
        let id = g.def_id_at(m, line, col)?;
        let cursor = g
            .module_refs(m)
            .and_then(|mr| {
                mr.occurrences()
                    .iter()
                    .map(|o| o.reference)
                    .filter(|r| r.span.contains(line, col))
                    .min_by_key(|r| r.span.width())
                    .map(|r| r.span)
            })
            .unwrap_or(id.span);
        Some((id, cursor))
    }

    /// find-references: the declaration plus every occurrence workspace-wide,
    /// with the declaration's self-occurrence de-duplicated.
    fn references(&self, def: DefId) -> Vec<(ModulePath, Span)> {
        let g = self.reference_graph();
        let mut out: Vec<(ModulePath, Span)> = Vec::new();
        if let Some(p) = g.module_path(def.module) {
            out.push((p.clone(), def.span));
        }
        for rr in g.references_to(def) {
            if (rr.module, rr.span.start_line, rr.span.start_column)
                == (def.module, def.span.start_line, def.span.start_column)
            {
                continue;
            }
            if let Some(p) = g.module_path(rr.module) {
                out.push((p.clone(), rr.span));
            }
        }
        out
    }

    /// Every span a rename rewrites: identical to find-references.
    fn rename(&self, def: DefId) -> Vec<(ModulePath, Span)> {
        self.references(def)
    }

    /// Every symbol-listable definition declared in the module.
    fn document_symbols(&self, module_or_uri: &str) -> Vec<SymbolInfo> {
        let g = self.reference_graph();
        match module_for(g, module_or_uri) {
            Some(m) => g.defs_in(m).filter_map(|d| symbol_of(g, d)).collect(),
            None => Vec::new(),
        }
    }

    /// Case-insensitive substring search over every workspace declaration.
    fn workspace_symbols(&self, query: &str) -> Vec<SymbolInfo> {
        let g = self.reference_graph();
        let needle = query.to_lowercase();
        g.all_defs()
            .filter(|d| needle.is_empty() || d.name.to_lowercase().contains(needle.as_str()))
            .filter_map(|d| symbol_of(g, d))
            .collect()
    }
}
