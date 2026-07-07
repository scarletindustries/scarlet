use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use indexmap::IndexMap;

use crate::bytecode::Watermark;
use crate::reference::ModuleReferences;
use crate::type_def::TypeId;
use crate::types::{Scheme, TypeInfo};

pub mod stdlib;

pub type ModulePath = Vec<String>;

pub fn path_key(path: &ModulePath) -> String {
    path.join("/")
}

/// `true` for the standard library / prelude / `@vm` intrinsics: any module
/// whose path is rooted at `al`. Those declarations are immutable from a
/// user's project and must never be rewritten.
pub fn is_stdlib(path: &ModulePath) -> bool {
    path.first().map(String::as_str) == Some("al")
}

pub fn al_prelude() -> ModulePath {
    vec!["al".to_string()]
}

pub fn main_module() -> ModulePath {
    vec!["main".to_string()]
}

const STDLIB_MARKER: &str = include_str!("../std/.al-stdlib-root");

/// If `file` is a stdlib source file inside the AL compiler repo itself,
/// return its module path so the compiler can analyse it *as* that module
/// (allowing `@vm`/external and suppressing prelude-redefinition errors).
///
/// Detection: walk up from `file` looking for `src/std/.al-stdlib-root` whose
/// content matches the value embedded at build time. The marker is a fixed
/// UUID that never changes, so editing stdlib files doesn't break detection
/// and a non-AL project can't accidentally match.
pub fn detect_stdlib_module(file: &Path) -> Option<ModulePath> {
    let file = file.canonicalize().ok()?;
    let mut dir = file.parent()?.to_path_buf();
    loop {
        let marker = dir.join("src/std/.al-stdlib-root");
        if let Ok(s) = std::fs::read_to_string(&marker)
            && s.trim() == STDLIB_MARKER.trim()
        {
            let std_root = dir.join("src/std");
            let rel = file.strip_prefix(&std_root).ok()?;
            // src/std/al.al -> ["al"]; src/std/al/net.al -> ["al","net"]
            let mut segs: ModulePath = rel
                .with_extension("")
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            // The marker file itself is not a module.
            if segs.last().map(|s| s.starts_with('.')) == Some(true) {
                return None;
            }
            if segs.is_empty() {
                segs = al_prelude();
            }
            return Some(segs);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Recursively collect every `.al` source file under `dir` into `out`.
/// Uses each entry's `file_type()` (no extra stat; symlinks are never
/// followed, so a symlink cycle can't wedge the walk), skips VCS/build/dep
/// directories (dotdirs, `target`, `node_modules`), and silently ignores
/// unreadable entries instead of aborting the whole walk.
pub fn collect_al_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if ft.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || matches!(name.as_ref(), "target" | "node_modules") {
                continue;
            }
            collect_al_files(&path, out);
        } else if ft.is_file() && path.extension().and_then(|e| e.to_str()) == Some("al") {
            out.push(path);
        }
    }
}

/// A module's exported value: its type scheme plus, for functions/consts
/// compiled into the shared `Program`, the local slot in the entrypoint frame
/// that holds the value.
#[derive(Debug, Clone)]
pub struct ExportedValue {
    pub scheme: Scheme,
    pub local_slot: Option<i32>,
}

/// What an importer sees of a compiled module: its `pub` types and values,
/// and the names of its non-`pub` items so we can give a "private" error
/// rather than "not found" when one is referenced.
#[derive(Debug, Clone)]
pub struct ModuleInterface {
    pub path: ModulePath,
    pub types: IndexMap<String, TypeInfo>,
    pub values: IndexMap<String, ExportedValue>,
    pub private_names: HashSet<String>,
}

impl ModuleInterface {
    pub fn new(path: ModulePath) -> Self {
        ModuleInterface {
            path,
            types: IndexMap::new(),
            values: IndexMap::new(),
            private_names: HashSet::new(),
        }
    }
}

/// Width of the type-id range reserved for each user module. A module's
/// nominal type ids are allocated from `[id_base, id_base + RANGE)`; the
/// next module starts at the next multiple. This keeps type ids stable when
/// an unrelated earlier module is recompiled — see `ModuleTable::id_base_for`.
pub const MODULE_TYPE_ID_RANGE: i32 = 256;

/// Round `n` up to the next multiple of `MODULE_TYPE_ID_RANGE`.
const fn align_to_range(n: i32) -> i32 {
    n + (MODULE_TYPE_ID_RANGE - n % MODULE_TYPE_ID_RANGE) % MODULE_TYPE_ID_RANGE
}

/// Where a cached module came from. The variant determines which pieces of
/// incremental-recompilation bookkeeping exist: only on-disk `File` modules
/// have a source path to re-hash and a watermark to truncate to; `Embedded`
/// stdlib compiled from `&'static str` never changes; `Hydrated` stdlib was
/// deserialised from the precompiled blob and has no source at all.
///
/// Reference-graph data (`refs`): every definition declared in the module and
/// every name occurrence inside it, filled by the typecheck pass. Persisting
/// it here — alongside `iface`/`dependents` — is what lets a cross-module
/// reference into this module survive an unrelated recompile, and dropping the
/// `CachedModule` on invalidation drops its references with it so a rebuilt
/// workspace graph can never have a dangling reverse edge. `Rc` so
/// `build_reference_graph` re-inserting an unchanged module into the workspace
/// graph on every `check` is a refcount bump, not a deep copy. `Hydrated`
/// modules carry none — their definitions are synthesised lazily from the
/// hydrated `ModuleInterface` at workspace-graph build time (the stdlib's
/// precompiled `Scheme.def` already carries the real declaration span).
#[derive(Debug)]
pub enum ModuleOrigin {
    Hydrated,
    Embedded {
        source_hash: u64,
        refs: Rc<ModuleReferences>,
    },
    File {
        /// FNV-1a of the source bytes this interface was built from.
        source_hash: u64,
        /// Snapshot of every arena/pool length immediately *before* this
        /// module's body was analysed. On invalidation the engine truncates
        /// back to the minimum watermark across the invalidated set, which by
        /// construction also discards every module compiled after that point.
        watermark: Watermark,
        /// Resolved on-disk path; re-hashed by `IncrementalSession` on the
        /// next check.
        path: PathBuf,
        refs: Rc<ModuleReferences>,
    },
}

/// A loaded module plus the bookkeeping needed for incremental recompilation.
///
/// The per-module nominal type-id range is deliberately *not* stored here.
/// `ModuleTable::id_bases` is the single source of truth for it, because that
/// map must survive `CachedModule` eviction: recompiling a module after an
/// unrelated module's closure has been dropped still has to hand out the same
/// type ids, which a field dropped on invalidation could never guarantee.
#[derive(Debug)]
pub struct CachedModule {
    pub iface: ModuleInterface,
    pub origin: ModuleOrigin,
    /// Direct importers of this module (reverse edges of the import graph).
    pub dependents: HashSet<String>,
}

impl CachedModule {
    fn from_static(iface: ModuleInterface) -> Self {
        CachedModule {
            iface,
            origin: ModuleOrigin::Hydrated,
            dependents: HashSet::new(),
        }
    }

    /// Resolved on-disk path this module was compiled from, if any.
    pub fn source_path(&self) -> Option<&Path> {
        match &self.origin {
            ModuleOrigin::File { path, .. } => Some(path),
            _ => None,
        }
    }

    /// Arena watermark captured before this module's body was analysed.
    /// `None` for embedded/hydrated modules — they are never invalidated.
    pub fn watermark(&self) -> Option<Watermark> {
        match &self.origin {
            ModuleOrigin::File { watermark, .. } => Some(*watermark),
            _ => None,
        }
    }

    /// Reference-graph data collected while this module's body was analysed.
    /// `None` for hydrated stdlib modules.
    pub fn module_refs(&self) -> Option<&Rc<ModuleReferences>> {
        match &self.origin {
            ModuleOrigin::Embedded { refs, .. } | ModuleOrigin::File { refs, .. } => Some(refs),
            ModuleOrigin::Hydrated => None,
        }
    }
}

#[derive(Debug, Default)]
pub struct ModuleTable {
    /// Insertion order is compilation order; `into_loaded` preserves it so the
    /// precompiled-stdlib emit is deterministic.
    loaded: IndexMap<String, CachedModule>,
    loading: HashSet<String>,
    /// When the binary carries a static stdlib, `get_or_hydrate` falls through
    /// to `s.lookup_module(key)` on a `loaded` miss and caches the result.
    pub static_fallback: Option<&'static crate::static_ir::StaticStdlib>,
    /// In-memory document overrides (LSP unsaved buffers). Module resolution
    /// prefers these over `fs::read_to_string`.
    pub overlays: HashMap<PathBuf, String>,
    /// Per-module `(mtime, len)` of the on-disk source as of the last time
    /// its content was hashed and found to match the cached `source_hash`.
    /// `IncrementalSession::check` stat-gates the per-keystroke staleness scan
    /// against this: an unchanged tuple means the bytes are unchanged, so the
    /// full read + FNV hash is skipped. The hash stays the source of truth —
    /// a stat miss falls through to read + hash and then refreshes the tuple.
    stat_cache: HashMap<String, (std::time::SystemTime, u64)>,
    /// Incremented every time a module body is actually compiled (not on cache
    /// hit). Test/telemetry only.
    compile_count: u32,
    /// Per-module type-id range starts. Assigned on a module's first compile
    /// and *retained across cache eviction* so a recompile hands out the same
    /// nominal type ids. Only cleared by `reset_id_bases` (overflow fallback).
    id_bases: HashMap<String, TypeId>,
    /// Lowest type id not covered by any allocated `id_base` range; the next
    /// fresh `id_base_for` rounds up from `max(floor, id_high_water)`.
    id_high_water: TypeId,
    /// Set when a recompiled module allocated past its reserved range and may
    /// have collided with a later module's ids. `IncrementalSession` reacts by
    /// performing a full invalidate on the next `check`.
    id_range_overflow: bool,
}

impl ModuleTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn into_loaded(self) -> IndexMap<String, ModuleInterface> {
        self.loaded.into_iter().map(|(k, v)| (k, v.iface)).collect()
    }

    pub fn is_loading(&self, key: &str) -> bool {
        self.loading.contains(key)
    }

    pub fn mark_loading(&mut self, key: &str) {
        self.loading.insert(key.to_string());
    }

    pub fn unmark_all_loading(&mut self) {
        self.loading.clear();
    }

    pub fn insert(&mut self, key: String, iface: ModuleInterface) {
        self.loading.remove(&key);
        self.loaded.insert(key, CachedModule::from_static(iface));
    }

    pub fn insert_cached(&mut self, key: String, cm: CachedModule) {
        self.loading.remove(&key);
        self.loaded.insert(key, cm);
    }

    pub fn get(&self, key: &str) -> Option<&ModuleInterface> {
        self.loaded.get(key).map(|c| &c.iface)
    }

    /// `get`, falling through to `static_fallback` and caching the hydrate.
    pub fn get_or_hydrate(&mut self, key: &str) -> Option<&ModuleInterface> {
        if !self.loaded.contains_key(key)
            && let Some(s) = self.static_fallback
            && let Some(iface) = s.lookup_module(key)
        {
            self.loaded
                .insert(key.to_string(), CachedModule::from_static(iface));
        }
        self.loaded.get(key).map(|c| &c.iface)
    }

    /// Record that `importer` directly depends on `importee` so a change to
    /// `importee` cascades to `importer` on invalidate.
    pub fn record_dependent(&mut self, importee: &str, importer: &str) {
        if let Some(cm) = self.loaded.get_mut(importee) {
            cm.dependents.insert(importer.to_string());
        }
    }

    /// Read the on-disk (or overlaid) content for a previously-resolved file.
    pub fn read_source(&self, path: &Path) -> Option<String> {
        if let Some(t) = self.overlays.get(path) {
            return Some(t.clone());
        }
        std::fs::read_to_string(path).ok()
    }

    /// Iterate cached user modules (those compiled from a file on disk).
    pub fn user_modules(&self) -> impl Iterator<Item = (&String, &CachedModule)> {
        self.loaded
            .iter()
            .filter(|(_, cm)| matches!(cm.origin, ModuleOrigin::File { .. }))
    }

    /// Has cached module `key`'s source changed since its interface was built?
    ///
    /// Called for every cached user module on every `IncrementalSession::check`
    /// (i.e. per LSP keystroke), so it must be cheap when nothing changed. An
    /// unsaved-buffer overlay has no stable mtime, so it is hashed directly
    /// (the in-memory copy is small and authoritative). The disk path is
    /// stat-gated: when `(mtime, len)` is byte-identical to the tuple recorded
    /// the last time the content was hashed equal, the file is taken to be
    /// unchanged and the read + FNV hash is skipped — turning an
    /// O(sum of all dependency file sizes) read+hash per keystroke into
    /// O(#dependencies) `stat` calls. On a stat miss the hash is still the
    /// source of truth: the file is read and hashed, the tuple refreshed so a
    /// content-preserving `touch` doesn't re-read forever, and the rare
    /// same-`(mtime, len)` content edit is additionally covered by the LSP
    /// `didChangeWatchedFiles` -> `invalidate_path` path.
    pub fn source_changed(&mut self, key: &str) -> bool {
        let (path, expected_hash) = match self.loaded.get(key).map(|cm| &cm.origin) {
            Some(ModuleOrigin::File {
                path, source_hash, ..
            }) => (path.clone(), *source_hash),
            _ => return false,
        };

        // Unsaved editor buffer: in-memory copy is authoritative and small;
        // there is no stable on-disk mtime to gate on.
        if let Some(t) = self.overlays.get(&path) {
            return source_hash(t) != expected_hash;
        }

        // Disk-backed: skip the read + hash when (mtime, len) is unchanged.
        let stat = file_stat(&path);
        if let Some(s) = stat
            && self.stat_cache.get(key) == Some(&s)
        {
            return false;
        }

        match std::fs::read_to_string(&path) {
            Ok(t) => {
                if let Some(s) = stat {
                    self.stat_cache.insert(key.to_string(), s);
                }
                source_hash(&t) != expected_hash
            }
            Err(_) => {
                self.stat_cache.remove(key);
                true // file vanished — invalidate
            }
        }
    }

    /// Iterate every cached module — user *and* hydrated stdlib. Used to merge
    /// all surviving modules' references into the workspace `ReferenceGraph`.
    pub fn loaded_modules(&self) -> impl Iterator<Item = (&String, &CachedModule)> {
        self.loaded.iter()
    }

    /// The persisted per-module reference data for `key`, if it was compiled
    /// from source this session. `None` for static/hydrated stdlib modules
    /// (their definitions are synthesised lazily from the interface).
    pub fn module_refs(&self, key: &str) -> Option<&ModuleReferences> {
        self.loaded
            .get(key)
            .and_then(|c| c.module_refs())
            .map(Rc::as_ref)
    }

    /// Return the type-id range start for `key`, allocating a fresh
    /// 256-aligned range on first request. `floor` is the caller's current
    /// `next_type_id` so the first user module's range sits past every stdlib
    /// id. The second tuple element is `true` when an existing assignment was
    /// reused — a fresh allocation is always at `id_high_water`, so a
    /// first-compile overflow (deps allocate before importer) only spills into
    /// unallocated space and `note_id_usage` simply pushes `id_high_water`
    /// past it; a reused-range overflow may collide with a sibling's already-
    /// assigned block and so raises `id_range_overflow`.
    pub fn id_base_for(&mut self, key: &str, floor: TypeId) -> (TypeId, bool) {
        if let Some(&b) = self.id_bases.get(key) {
            return (b, true);
        }
        let base = TypeId(align_to_range(floor.0.max(self.id_high_water.0)));
        self.id_bases.insert(key.to_string(), base);
        self.id_high_water = TypeId(base.0 + MODULE_TYPE_ID_RANGE);
        (base, false)
    }

    /// Record that a module at `base` allocated `used` type ids. On a reused
    /// range, allocating past `MODULE_TYPE_ID_RANGE` may collide with a
    /// sibling's already-assigned block, so the overflow flag is raised; on a
    /// fresh range the spillover is into unallocated space (the importer's
    /// base is always `id_high_water` at the time of allocation, deps having
    /// allocated first). Either way the high-water mark is bumped past actual
    /// usage so the next fresh allocation skips the spillover.
    pub fn note_id_usage(&mut self, base: TypeId, used: i32, reused: bool) {
        if used > MODULE_TYPE_ID_RANGE {
            if reused {
                self.id_range_overflow = true;
            }
            self.id_high_water = self
                .id_high_water
                .max(TypeId(align_to_range(base.0 + used)));
        }
    }

    /// Lowest type id not covered by any allocated range. The entry module's
    /// `next_type_id` is bumped to at least this after `process_imports` so it
    /// allocates past every reserved block — including those whose owners were
    /// cache hits and so contributed nothing to `next_type_id` this pass.
    pub fn id_high_water(&self) -> TypeId {
        self.id_high_water
    }

    /// Drop every per-module id assignment. Called on overflow fallback so the
    /// subsequent full recompile re-allocates ranges sized to current usage.
    pub fn reset_id_bases(&mut self) {
        self.id_bases.clear();
        self.id_high_water = TypeId::NONE;
        self.id_range_overflow = false;
    }

    /// Look up a previously assigned id_base without allocating.
    pub fn id_base_of(&self, key: &str) -> Option<TypeId> {
        self.id_bases.get(key).copied()
    }

    /// Number of module bodies actually compiled (test/telemetry only).
    pub fn compile_count(&self) -> u32 {
        self.compile_count
    }

    pub(crate) fn bump_compile_count(&mut self) {
        self.compile_count += 1;
    }

    /// Did a recompiled module allocate past its reserved type-id range?
    pub fn id_range_overflow(&self) -> bool {
        self.id_range_overflow
    }

    /// Remove `key` and every transitive dependent from the cache and return
    /// the earliest watermark among them (the truncation target).
    ///
    /// Only the dependent closure is *logically* invalidated: every module's
    /// `id_base` survives in `id_bases`, so when an unrelated later module is
    /// recompiled it receives the same type ids it had before. Those later
    /// modules must still be dropped from `loaded` here — the caller truncates
    /// every arena to `min_wm`, after which their cached `ArenaSlice`/`Ty`
    /// indices would dangle — but that recompile is id-stable.
    ///
    /// Reference-graph coherence: an evicted `CachedModule` takes its
    /// `module_refs` (definitions + occurrences) with it. The workspace
    /// `ReferenceGraph` is rebuilt from the surviving `loaded` set on every
    /// `IncrementalSession::check`, so an invalidated module's reverse edges
    /// are recomputed from scratch and can never dangle into evicted state.
    pub fn invalidate(&mut self, key: &str) -> Option<Watermark> {
        use std::collections::VecDeque;
        let mut closure: HashSet<String> = HashSet::new();
        let mut q: VecDeque<String> = VecDeque::new();
        q.push_back(key.to_string());
        while let Some(k) = q.pop_front() {
            if !closure.insert(k.clone()) {
                continue;
            }
            if let Some(cm) = self.loaded.get(&k) {
                for d in &cm.dependents {
                    q.push_back(d.clone());
                }
            }
        }
        let min_wm = closure
            .iter()
            .filter_map(|k| self.loaded.get(k).and_then(|cm| cm.watermark()))
            .min()?;
        self.loaded.retain(|k, cm| {
            if closure.contains(k) {
                return false;
            }
            cm.watermark().is_none_or(|w| w < min_wm)
        });
        self.stat_cache.retain(|k, _| self.loaded.contains_key(k));
        Some(min_wm)
    }

    /// Evict every user module (those with a watermark) and return the
    /// earliest watermark among them. Used as the overflow fallback when a
    /// recompiled module no longer fits its reserved id range.
    pub fn invalidate_all(&mut self) -> Option<Watermark> {
        let min_wm = self.loaded.values().filter_map(|cm| cm.watermark()).min()?;
        self.loaded
            .retain(|_, cm| !matches!(cm.origin, ModuleOrigin::File { .. }));
        self.stat_cache.retain(|k, _| self.loaded.contains_key(k));
        Some(min_wm)
    }
}

/// `(mtime, len)` of `path`, or `None` if either is unavailable (a missing
/// file, or a platform whose filesystem doesn't expose mtime). `None` makes
/// `source_changed` fall back to the read + hash path, so it is always safe.
fn file_stat(path: &Path) -> Option<(std::time::SystemTime, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// FNV-1a 64-bit hash over source bytes for cheap change-detection.
pub fn source_hash(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h = (h ^ b as u64).wrapping_mul(0x100000001b3);
    }
    h
}

#[derive(Debug, Clone)]
pub enum ModuleSource {
    Embedded(&'static str),
    File(PathBuf),
}

#[derive(Debug, Clone)]
pub enum ResolveError {
    NotFound(String),
    NoBaseDir,
    /// A bare module name that is neither `al/*` nor `./`/`../`-relative.
    /// Reserved for a future package manager; distinct from `NotFound` so the
    /// diagnostic can suggest `use "./name"` instead of "not found".
    BareName(String),
}

/// Resolve a module path to its source. `base_dir` is the directory of the
/// importing file, used for `./` and `../` relative imports.
pub fn resolve(path: &ModulePath, base_dir: Option<&Path>) -> Result<ModuleSource, ResolveError> {
    let key = path_key(path);

    // Stdlib: any path rooted at `al`.
    if is_stdlib(path) {
        return match stdlib::lookup(&key) {
            Some(src) => Ok(ModuleSource::Embedded(src)),
            None => Err(ResolveError::NotFound(key)),
        };
    }

    // Relative: `.` or `..` segments.
    if matches!(path.first().map(|s| s.as_str()), Some(".") | Some("..")) {
        let Some(base) = base_dir else {
            return Err(ResolveError::NoBaseDir);
        };
        let mut p: PathBuf = base.to_path_buf();
        for seg in path {
            match seg.as_str() {
                "." => {}
                ".." => {
                    p.pop();
                }
                other => p.push(other),
            }
        }
        p.set_extension("al");
        if p.is_file() {
            return Ok(ModuleSource::File(p));
        }
        return Err(ResolveError::NotFound(p.display().to_string()));
    }

    // Bare names other than `al` are reserved for a future package manager.
    Err(ResolveError::BareName(key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn unique_dir(tag: &str) -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let d = std::env::temp_dir().join(format!(
            "al_modtest_{tag}_{}_{n}_{nanos}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    // A real stdlib source file inside this very repo is detected as its module
    // path so the compiler can analyse it *as* that module. The marker file
    // itself is not a module, and an unrelated path matches nothing.
    #[test]
    fn detect_stdlib_module_resolves_repo_std_files() {
        let list = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src/std/al/list.al"));
        assert_eq!(
            detect_stdlib_module(list),
            Some(vec!["al".to_string(), "list".to_string()])
        );

        let prelude = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src/std/al.al"));
        assert_eq!(detect_stdlib_module(prelude), Some(vec!["al".to_string()]));

        // The `.al-stdlib-root` marker is not itself a module.
        let marker = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/std/.al-stdlib-root"
        ));
        assert_eq!(detect_stdlib_module(marker), None);

        // A path with no stdlib root above it resolves to nothing.
        let outside = unique_dir("detect_outside").join("foo.al");
        std::fs::write(&outside, "x = 1").unwrap();
        assert_eq!(detect_stdlib_module(&outside), None);
    }

    // `collect_al_files` recurses into normal subdirectories, takes only `.al`
    // files, and skips dotdirs / `target` / `node_modules`. A missing directory
    // yields nothing rather than erroring.
    #[test]
    fn collect_al_files_filters_and_recurses() {
        let root = unique_dir("collect");
        std::fs::write(root.join("a.al"), "").unwrap();
        std::fs::write(root.join("b.txt"), "").unwrap();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/c.al"), "").unwrap();
        std::fs::create_dir_all(root.join(".hidden")).unwrap();
        std::fs::write(root.join(".hidden/d.al"), "").unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join("target/e.al"), "").unwrap();
        std::fs::create_dir_all(root.join("node_modules")).unwrap();
        std::fs::write(root.join("node_modules/f.al"), "").unwrap();

        let mut out = Vec::new();
        collect_al_files(&root, &mut out);
        let names: HashSet<String> = out
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            HashSet::from(["a.al".to_string(), "c.al".to_string()]),
            "should collect only a.al and sub/c.al; got {out:?}"
        );

        // Unreadable / missing directory: silent, leaves `out` untouched.
        let mut missing = Vec::new();
        collect_al_files(&root.join("does-not-exist"), &mut missing);
        assert!(missing.is_empty());

        let _ = std::fs::remove_dir_all(&root);
    }

    // `resolve` routes `al/*` to embedded source, relative paths against a base
    // dir (with `.`/`..` navigation), and errors clearly otherwise.
    #[test]
    fn resolve_stdlib_relative_and_errors() {
        // Embedded stdlib.
        assert!(matches!(
            resolve(&vec!["al".into(), "list".into()], None),
            Ok(ModuleSource::Embedded(_))
        ));
        assert!(matches!(
            resolve(&vec!["al".into(), "no_such_mod".into()], None),
            Err(ResolveError::NotFound(_))
        ));

        // Relative import with no base dir cannot be resolved.
        assert!(matches!(
            resolve(&vec![".".into(), "foo".into()], None),
            Err(ResolveError::NoBaseDir)
        ));

        let base = unique_dir("resolve");
        std::fs::write(base.join("foo.al"), "").unwrap();
        std::fs::write(base.join("bar.al"), "").unwrap();
        std::fs::create_dir_all(base.join("sub")).unwrap();

        // `./foo` from base.
        match resolve(&vec![".".into(), "foo".into()], Some(&base)) {
            Ok(ModuleSource::File(p)) => assert_eq!(p, base.join("foo.al")),
            other => panic!("expected ./foo to resolve to a file, got {other:?}"),
        }
        // `../bar` from base/sub climbs to base/bar.al.
        match resolve(&vec!["..".into(), "bar".into()], Some(&base.join("sub"))) {
            Ok(ModuleSource::File(p)) => assert_eq!(p, base.join("bar.al")),
            other => panic!("expected ../bar to resolve, got {other:?}"),
        }
        // A relative path to a non-existent file is NotFound.
        assert!(matches!(
            resolve(&vec![".".into(), "ghost".into()], Some(&base)),
            Err(ResolveError::NotFound(_))
        ));
        // Bare names (no `al` root, not relative) are reserved -> BareName.
        assert!(matches!(
            resolve(&vec!["somepkg".into()], None),
            Err(ResolveError::BareName(_))
        ));

        let _ = std::fs::remove_dir_all(&base);
    }

    // `ModuleTable` loading/insert/iterate lifecycle.
    #[test]
    fn module_table_loading_and_insert_lifecycle() {
        let mut t = ModuleTable::new();
        assert!(!t.is_loading("foo"));
        t.mark_loading("foo");
        assert!(t.is_loading("foo"));

        t.insert(
            "foo".to_string(),
            ModuleInterface::new(vec!["foo".to_string()]),
        );
        // insert clears the loading mark and makes the interface visible.
        assert!(!t.is_loading("foo"));
        assert!(t.get("foo").is_some());

        let loaded = t.into_loaded();
        assert!(loaded.contains_key("foo"));
        assert_eq!(loaded.len(), 1);
    }

    // `source_changed`: false for unknown keys and for hydrated (no-path)
    // modules; for a disk-backed module, false when the bytes match the cached
    // hash and true once the backing file is deleted.
    #[test]
    fn source_changed_paths() {
        let mut t = ModuleTable::new();

        // Unknown key.
        assert!(!t.source_changed("unknown"));

        // Hydrated module never changes.
        t.insert(
            "static".to_string(),
            ModuleInterface::new(vec!["static".to_string()]),
        );
        assert!(!t.source_changed("static"));

        // A disk-backed module: unchanged then vanished.
        let dir = unique_dir("srcchanged");
        let path = dir.join("m.al");
        let body = "x = 1\n";
        std::fs::write(&path, body).unwrap();
        let cm = CachedModule {
            iface: ModuleInterface::new(vec!["m".to_string()]),
            origin: ModuleOrigin::File {
                source_hash: source_hash(body),
                watermark: Watermark::default(),
                path: path.clone(),
                refs: Rc::new(ModuleReferences::new(crate::reference::ModuleId(0))),
            },
            dependents: HashSet::new(),
        };
        t.insert_cached("m".to_string(), cm);
        assert!(!t.source_changed("m"), "identical content is unchanged");

        std::fs::remove_file(&path).unwrap();
        assert!(t.source_changed("m"), "a vanished file invalidates");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
