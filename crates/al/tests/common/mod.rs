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
/// The pid separates test binaries and the thread-id hash separates parallel
/// test threads (ThreadIds are never reused); same-thread tests run
/// sequentially, so that pair is enough to keep names unique.
pub fn write_temp(source: &str) -> std::path::PathBuf {
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

/// Write `source` to a temp file, run `al <cmd>` on it, and remove the file.
fn run_source(cmd: &str, source: &str) -> AlOutput {
    let path = write_temp(source);
    let out = run_al(cmd, &path);
    let _ = std::fs::remove_file(&path);
    out
}

/// `source` followed by the captured streams — the body shared by the
/// assertion-failure messages below.
fn dump(source: &str, out: &AlOutput) -> String {
    format!(
        "{source}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        out.stdout, out.stderr
    )
}

/// Assert the exit status matches `want_success`, panicking with `what` and
/// the standard dump otherwise.
fn assert_status(out: &AlOutput, want_success: bool, what: &str, source: &str) {
    assert!(
        out.success == want_success,
        "{what}:\n{}",
        dump(source, out)
    );
}

/// Assert `al check` rejects `source` (failure exit, code 1) and that the
/// combined stdout+stderr contains `expected_substring`.
pub fn check_fails(source: &str, expected_substring: &str) {
    let out = run_source("check", source);
    assert_status(&out, false, "expected `al check` to REJECT", source);
    assert_eq!(
        out.code,
        Some(1),
        "expected exit code 1 for:\n{}",
        dump(source, &out)
    );
    let combined = out.combined();
    assert!(
        combined.contains(expected_substring),
        "expected output to contain {expected_substring:?} for:\n{source}\n--- output ---\n{combined}"
    );
}

/// Assert `al check` rejects `source` (failure exit). Diagnostic text and the
/// exact exit code are not pinned.
pub fn check_rejects(source: &str) {
    let out = run_source("check", source);
    assert_status(&out, false, "expected `al check` to REJECT", source);
}

/// Assert `al check` rejects `source` cleanly: failure exit, no Rust panic in
/// the output, and a diagnostic containing `expected_diag` on either stream.
pub fn check_rejects_cleanly(source: &str, expected_diag: &str) {
    let out = run_source("check", source);
    assert_status(&out, false, "expected `al check` to REJECT", source);
    let combined = out.combined();
    assert!(
        !combined.contains("panicked"),
        "compiler panicked instead of rejecting cleanly:\n{}",
        dump(source, &out)
    );
    assert!(
        combined.contains(expected_diag),
        "expected output to contain {expected_diag:?} for:\n{source}\n--- output ---\n{combined}"
    );
}

/// Assert `al check` accepts `source` (success exit).
pub fn check_ok(source: &str) {
    let out = run_source("check", source);
    assert_status(&out, true, "expected `al check` to ACCEPT", source);
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

use al::bytecode::IncrementalSession;
use al::module::{self, ModulePath};
use al::reference::{DefId, Definition, EntityKind, ModuleId, ReferenceGraph};
use al::span::Span;

/// A document/workspace symbol projected from a graph `Definition`.
#[derive(Debug, Clone)]
pub struct SymbolInfo {
    pub name: String,
    pub kind: EntityKind,
    pub module: ModulePath,
    pub span: Span,
}

fn module_for(g: &ReferenceGraph, module_or_uri: &str) -> Option<ModuleId> {
    g.module_id_by_key(module_or_uri)
        .or_else(|| g.module_id_by_key(&module::path_key(&module::main_module())))
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

/// String-keyed LSP-style queries (goto-def / prepare-rename / find-refs /
/// rename / symbols) answered from [`IncrementalSession::reference_graph`].
/// The production LSP server queries the graph directly with resolved
/// `ModuleId`s; these helpers key by module path, falling back to the entry
/// `main` module for the single-open-file case.
pub trait SessionQueryExt {
    fn definition(&self, module_or_uri: &str, line: i32, col: i32) -> Option<(ModulePath, Span)>;
    fn prepare_rename(&self, module_or_uri: &str, line: i32, col: i32) -> Option<(DefId, Span)>;
    fn references(&self, def: DefId) -> Vec<(ModulePath, Span)>;
    fn rename(&self, def: DefId) -> Vec<(ModulePath, Span)>;
    fn document_symbols(&self, module_or_uri: &str) -> Vec<SymbolInfo>;
    fn workspace_symbols(&self, query: &str) -> Vec<SymbolInfo>;
}

impl SessionQueryExt for IncrementalSession {
    /// goto-definition: the occurrence under the cursor resolved (through any
    /// import alias) to its canonical declaration.
    fn definition(&self, module_or_uri: &str, line: i32, col: i32) -> Option<(ModulePath, Span)> {
        let g = self.reference_graph();
        let m = module_for(g, module_or_uri)?;
        let id = g.canonical(g.def_id_at(m, line, col)?);
        Some((g.module_path(id.module)?.clone(), id.span))
    }

    /// The raw (alias-preserving) `DefId` under the cursor plus the span of
    /// the smallest enclosing occurrence.
    fn prepare_rename(&self, module_or_uri: &str, line: i32, col: i32) -> Option<(DefId, Span)> {
        let g = self.reference_graph();
        let m = module_for(g, module_or_uri)?;
        let id = g.def_id_at(m, line, col)?;
        let cursor = g
            .module_refs(m)
            .and_then(|mr| {
                mr.occurrences()
                    .iter()
                    .filter(|o| o.span.contains(line, col))
                    .min_by_key(|o| o.span.width())
                    .map(|o| o.span)
            })
            .unwrap_or(id.span);
        Some((id, cursor))
    }

    /// find-references: the declaration (its self-occurrence de-duplicated)
    /// plus every occurrence workspace-wide.
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
