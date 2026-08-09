use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use indexmap::IndexMap;

use crate::ast::{ImportPath, RelSeg};
use crate::bytecode::Watermark;
use crate::reference::ModuleReferences;
use crate::type_def::TypeId;
use crate::typed_ir::GlobalSlot;
use crate::types::{Scheme, TypeInfo};

pub mod stdlib;

// Module identity lives in `al_syntax::module_path` because a `Diagnostic`
// carries the key of the module it points into. Re-exported here, where
// resolution mints the keys.
pub use al_syntax::module_path::{
    ModuleKey, ModulePath, ResolveError, al_prelude, file_module_path, is_resolved_file, is_stdlib,
    main_module,
};

const STDLIB_MARKER: &str = include_str!("../std/.al-stdlib-root");

/// Walk up from `near` for a `src/std/.al-stdlib-root` marker matching the one
/// embedded at build time; on a hit return the `src/std` directory. The marker
/// is a fixed UUID, so editing stdlib files cannot break detection and an
/// unrelated project cannot accidentally match.
pub fn find_stdlib_root(near: &Path) -> Option<PathBuf> {
    let near = near.canonicalize().ok()?;
    let mut dir = near.parent()?.to_path_buf();
    loop {
        let marker = dir.join("src/std/.al-stdlib-root");
        if let Ok(s) = std::fs::read_to_string(&marker)
            && s.trim() == STDLIB_MARKER.trim()
        {
            return Some(dir.join("src/std"));
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// If `file` is a stdlib source file inside the AL compiler repo itself, its
/// module path, so the compiler can analyse it *as* that module: `@vm`/external
/// allowed, prelude-redefinition errors suppressed.
pub fn detect_stdlib_module(file: &Path) -> Option<ModulePath> {
    let file = file.canonicalize().ok()?;
    let std_root = find_stdlib_root(&file)?;
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
    Some(segs)
}

/// Recursively collect every `.al` source file under `dir` into `out`. Skips
/// dotdirs, `target` and `node_modules`, and ignores unreadable entries rather
/// than aborting. Reads `file_type()` so symlinks are never followed and a
/// symlink cycle cannot wedge the walk.
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
    pub local_slot: Option<GlobalSlot>,
    /// A function's parameter names, in order. Empty for anything else.
    ///
    /// Documentation, not semantics: al rejects labelled arguments outside a
    /// constructor call, so a parameter name never reaches a call site and must
    /// stay out of `TypeNode::Fun` — otherwise unification would have to decide
    /// whether `fn(path String)` equals `fn(p String)`. Constructor field
    /// labels *are* semantic and live in the type instead.
    pub param_names: Vec<String>,
    /// The declaration's own doc comment, carried across module boundaries.
    pub doc: Option<String>,
}

/// What an importer sees of a compiled module: its `pub` types and values, plus
/// the names of its non-`pub` items so a reference to one gets a "private"
/// error rather than "not found".
#[derive(Debug, Clone)]
pub struct ModuleInterface {
    pub path: ModulePath,
    pub types: IndexMap<String, TypeInfo>,
    pub values: IndexMap<String, ExportedValue>,
    /// `BTreeSet` so iteration is sorted: `static_ir::flatten` interns these
    /// in iteration order into the reproducible stdlib blob.
    pub private_names: BTreeSet<String>,
    /// The module's own doc comment: the `/** */` block at line 0 of its
    /// source. Unlike every other doc, this one is carried through the
    /// precompiled stdlib blob so hovering `al/scheduler` shows its prose.
    pub doc: Option<String>,
}

impl ModuleInterface {
    pub fn new(path: ModulePath) -> Self {
        ModuleInterface {
            path,
            types: IndexMap::new(),
            values: IndexMap::new(),
            private_names: BTreeSet::new(),
            doc: None,
        }
    }
}

/// Width of the type-id range reserved for each user module: its nominal type
/// ids come from `[id_base, id_base + RANGE)`. Keeps type ids stable when an
/// unrelated earlier module is recompiled.
pub const MODULE_TYPE_ID_RANGE: i32 = 256;

/// Round `n` up to the next multiple of `MODULE_TYPE_ID_RANGE`.
const fn align_to_range(n: i32) -> i32 {
    n + (MODULE_TYPE_ID_RANGE - n % MODULE_TYPE_ID_RANGE) % MODULE_TYPE_ID_RANGE
}

/// Proof that [`ModuleTable::id_base_for`] reserved a type-id range, carrying
/// the range start and whether an existing assignment was reused so the two
/// cannot be mismatched. [`Self::note_usage`] consumes the token.
#[must_use = "hand the reservation back via note_usage once ids are allocated"]
pub struct IdRangeReservation {
    base: TypeId,
    /// `true` when an existing assignment was reused. A fresh allocation sits
    /// at `id_high_water`, so overflowing it only spills into unallocated
    /// space; overflowing a reused range may collide with a sibling's block.
    reused: bool,
}

impl IdRangeReservation {
    /// Start of the reserved range: the module's first nominal type id.
    pub fn base(&self) -> TypeId {
        self.base
    }

    /// Record that the module allocated `used` type ids from this range.
    /// Overflowing a reused range raises the overflow flag; either way the
    /// high-water mark is bumped past the spillover.
    pub fn note_usage(self, table: &mut ModuleTable, used: i32) {
        if used > MODULE_TYPE_ID_RANGE {
            if self.reused {
                table.id_range_overflow = true;
            }
            table.id_high_water = table
                .id_high_water
                .max(TypeId(align_to_range(self.base.0 + used)));
        }
    }
}

/// Where a cached module came from, and so which incremental bookkeeping it
/// has: only `File` modules have a source path to re-hash and a watermark to
/// truncate to. `Embedded` stdlib comes from `&'static str` and never changes;
/// `Hydrated` stdlib came from the precompiled blob and has no source.
///
/// `refs` holds the module's definitions and occurrences. Storing them here is
/// what lets a cross-module reference survive an unrelated recompile, and
/// dropping the `CachedModule` drops them, so a rebuilt workspace graph cannot
/// have a dangling reverse edge. `Hydrated` modules carry none: their
/// definitions are synthesised from the interface at graph-build time.
#[derive(Debug)]
// One instance per cached module, moved only at insert; boxing would add
// derefs and save nothing.
#[allow(clippy::large_enum_variant)]
pub enum ModuleOrigin {
    Hydrated,
    Embedded {
        refs: Rc<ModuleReferences>,
    },
    File {
        /// FNV-1a of the source bytes this interface was built from.
        source_hash: u64,
        /// `(mtime, len)` as of the last time the content hashed equal to
        /// `source_hash`; `None` until then. Lives next to the hash it gates so
        /// evicting the `CachedModule` drops the gate with it. See
        /// [`ModuleTable::source_changed`].
        stat: Option<FileStat>,
        /// Every arena/pool length immediately *before* this module's body was
        /// analysed.
        watermark: Watermark,
        /// Resolved on-disk path.
        path: PathBuf,
        refs: Rc<ModuleReferences>,
    },
}

/// A loaded module plus the bookkeeping needed for incremental recompilation.
///
/// The per-module type-id range is deliberately not stored here.
/// `ModuleTable::id_bases` owns it, because it must survive eviction:
/// recompiling a module has to hand out the same type ids it had before.
#[derive(Debug)]
pub struct CachedModule {
    pub iface: ModuleInterface,
    pub origin: ModuleOrigin,
    /// Direct importers of this module (reverse edges of the import graph).
    pub dependents: HashSet<ModuleKey>,
}

impl CachedModule {
    fn hydrated(iface: ModuleInterface) -> Self {
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

    /// Arena watermark captured before this module's body was analysed. `None`
    /// for embedded/hydrated modules, which are never invalidated.
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
            ModuleOrigin::Embedded { refs } | ModuleOrigin::File { refs, .. } => Some(refs),
            ModuleOrigin::Hydrated => None,
        }
    }
}

#[derive(Debug)]
pub struct ModuleTable {
    /// Insertion order is compilation order; `into_loaded` preserves it so the
    /// precompiled-stdlib emit is deterministic.
    loaded: IndexMap<ModuleKey, CachedModule>,
    loading: HashSet<ModuleKey>,
    /// When the binary carries a static stdlib, `get_or_hydrate` falls through
    /// to it on a `loaded` miss and caches the result.
    static_fallback: Option<&'static crate::static_ir::StaticStdlib>,
    /// In-memory document overrides (LSP unsaved buffers), preferred over
    /// `fs::read_to_string`.
    overlays: HashMap<PathBuf, String>,
    /// Module bodies actually compiled, not counting cache hits. Telemetry only.
    compile_count: u32,
    /// Per-module type-id range starts, retained across cache eviction so a
    /// recompile hands out the same nominal type ids. Only `reset_id_bases`
    /// clears it.
    id_bases: HashMap<ModuleKey, TypeId>,
    /// Lowest type id not covered by any allocated range.
    id_high_water: TypeId,
    /// Set when a recompiled module allocated past its reserved range and may
    /// have collided with a later module's ids. `IncrementalSession` reacts with
    /// a full invalidate on the next `check`.
    id_range_overflow: bool,
}

/// Hand-written because `TypeId` has no `Default`: `id_high_water` starts at
/// the `NONE` sentinel, the same state `reset_id_bases` restores.
impl Default for ModuleTable {
    fn default() -> Self {
        ModuleTable {
            loaded: IndexMap::new(),
            loading: HashSet::new(),
            static_fallback: None,
            overlays: HashMap::new(),
            compile_count: 0,
            id_bases: HashMap::new(),
            id_high_water: TypeId::NONE,
            id_range_overflow: false,
        }
    }
}

impl ModuleTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn into_loaded(self) -> IndexMap<String, ModuleInterface> {
        self.loaded
            .into_iter()
            .map(|(k, v)| (k.as_str().to_string(), v.iface))
            .collect()
    }

    pub fn is_loading(&self, key: &ModuleKey) -> bool {
        self.loading.contains(key)
    }

    pub fn mark_loading(&mut self, key: &ModuleKey) {
        self.loading.insert(key.clone());
    }

    pub fn unmark_all_loading(&mut self) {
        self.loading.clear();
    }

    pub fn insert_hydrated(&mut self, key: ModuleKey, iface: ModuleInterface) {
        self.loading.remove(&key);
        self.loaded.insert(key, CachedModule::hydrated(iface));
    }

    pub fn insert_cached(&mut self, key: ModuleKey, cm: CachedModule) {
        self.loading.remove(&key);
        self.loaded.insert(key, cm);
    }

    pub fn get(&self, key: &ModuleKey) -> Option<&ModuleInterface> {
        self.loaded.get(key).map(|c| &c.iface)
    }

    /// `get`, falling through to `static_fallback` and caching the hydrate.
    pub fn get_or_hydrate(&mut self, key: &ModuleKey) -> Option<&ModuleInterface> {
        if !self.loaded.contains_key(key)
            && let Some(s) = self.static_fallback
            && let Some(iface) = s.lookup_module(key.as_str())
        {
            self.loaded
                .insert(key.clone(), CachedModule::hydrated(iface));
        }
        self.loaded.get(key).map(|c| &c.iface)
    }

    /// Record that `importer` directly depends on `importee` so a change to
    /// `importee` cascades to `importer` on invalidate.
    pub fn record_dependent(&mut self, importee: &ModuleKey, importer: &ModuleKey) {
        if let Some(cm) = self.loaded.get_mut(importee) {
            cm.dependents.insert(importer.clone());
        }
    }

    /// Read the on-disk (or overlaid) content for a previously-resolved file.
    pub fn read_source(&self, path: &Path) -> std::io::Result<String> {
        if let Some(t) = self.overlays.get(path) {
            return Ok(t.clone());
        }
        std::fs::read_to_string(path)
    }

    pub fn set_overlay(&mut self, path: PathBuf, src: String) {
        self.overlays.insert(path, src);
    }

    pub fn clear_overlay(&mut self, path: &Path) {
        self.overlays.remove(path);
    }

    pub fn set_static_fallback(&mut self, s: &'static crate::static_ir::StaticStdlib) {
        self.static_fallback = Some(s);
    }

    /// Iterate cached user modules (those compiled from a file on disk).
    pub fn user_modules(&self) -> impl Iterator<Item = (&ModuleKey, &CachedModule)> {
        self.loaded
            .iter()
            .filter(|(_, cm)| matches!(cm.origin, ModuleOrigin::File { .. }))
    }

    /// Has cached module `key`'s source changed since its interface was built?
    ///
    /// Runs per LSP keystroke for every cached user module, so the unchanged
    /// case is stat-gated: an unmoved `(mtime, len)` skips the read and hash.
    /// An edit that preserves both is missed here and covered instead by the
    /// LSP's `didChangeWatchedFiles` -> `invalidate_path`.
    pub fn source_changed(&mut self, key: &ModuleKey) -> bool {
        let (path, expected_hash, cached_stat) = match self.loaded.get(key).map(|cm| &cm.origin) {
            Some(ModuleOrigin::File {
                path,
                source_hash,
                stat,
                ..
            }) => (path.clone(), *source_hash, *stat),
            _ => return false,
        };

        // An unsaved buffer has no stable mtime to gate on, and the in-memory
        // copy is authoritative.
        if let Some(t) = self.overlays.get(&path) {
            return source_hash(t) != expected_hash;
        }

        let stat = file_stat(&path);
        if stat.is_some() && cached_stat == stat {
            return false;
        }

        match std::fs::read_to_string(&path) {
            Ok(t) => {
                let changed = source_hash(&t) != expected_hash;
                if !changed && stat.is_some() {
                    self.set_stat(key, stat);
                }
                changed
            }
            Err(_) => {
                self.set_stat(key, None);
                true // file vanished — invalidate
            }
        }
    }

    /// Update the stat gate recorded on `key`'s `ModuleOrigin::File`.
    fn set_stat(&mut self, key: &ModuleKey, s: Option<FileStat>) {
        if let Some(cm) = self.loaded.get_mut(key)
            && let ModuleOrigin::File { stat, .. } = &mut cm.origin
        {
            *stat = s;
        }
    }

    /// Iterate every cached module, user and hydrated stdlib alike.
    pub fn loaded_modules(&self) -> impl Iterator<Item = (&ModuleKey, &CachedModule)> {
        self.loaded.iter()
    }

    /// Persisted reference data for the module whose *canonical* path is
    /// `path`. Lookup, not mint: a non-canonical path simply misses. `None`
    /// for hydrated stdlib modules, which carry none.
    pub fn module_refs_by_path(&self, path: &ModulePath) -> Option<&ModuleReferences> {
        self.loaded
            .get(&ModuleKey::of(path))
            .and_then(|c| c.module_refs())
            .map(Rc::as_ref)
    }

    /// Reserve or re-find the type-id range for `key`, allocating a fresh
    /// 256-aligned range on first request. `floor` is the caller's current
    /// `next_type_id`, so the first user module lands past every stdlib id.
    /// Hand the returned token back via [`IdRangeReservation::note_usage`] once
    /// the module's body has allocated its ids.
    pub fn id_base_for(&mut self, key: &ModuleKey, floor: TypeId) -> IdRangeReservation {
        if let Some(&b) = self.id_bases.get(key) {
            return IdRangeReservation {
                base: b,
                reused: true,
            };
        }
        let base = TypeId(align_to_range(floor.0.max(self.id_high_water.0)));
        self.id_bases.insert(key.clone(), base);
        self.id_high_water = TypeId(base.0 + MODULE_TYPE_ID_RANGE);
        IdRangeReservation {
            base,
            reused: false,
        }
    }

    /// Lowest type id not covered by any allocated range. The entry module's
    /// `next_type_id` is bumped to at least this after `process_imports`, so it
    /// clears blocks whose owners were cache hits and contributed nothing to
    /// `next_type_id` this pass.
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
    pub fn id_base_of(&self, key: &ModuleKey) -> Option<TypeId> {
        self.id_bases.get(key).copied()
    }

    /// Module bodies actually compiled. Telemetry only.
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
    /// the earliest watermark among them, the truncation target.
    ///
    /// Modules compiled *after* that watermark are also dropped, because the
    /// caller truncates every arena to it and their cached `ArenaSlice`/`Ty`
    /// indices would dangle. Their recompile is still id-stable: `id_bases`
    /// survives eviction.
    pub fn invalidate(&mut self, key: &ModuleKey) -> Option<Watermark> {
        use std::collections::VecDeque;
        let mut closure: HashSet<ModuleKey> = HashSet::new();
        let mut q: VecDeque<ModuleKey> = VecDeque::new();
        q.push_back(key.clone());
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
        // `Watermark::earlier`, not `Iterator::min`: equal pool lengths tie
        // under `Ord`, and `earlier` merges the env payloads on a tie rather
        // than keeping whichever came first.
        let min_wm = closure
            .iter()
            .filter_map(|k| self.loaded.get(k).and_then(|cm| cm.watermark()))
            .reduce(Watermark::earlier)?;
        self.loaded.retain(|k, cm| {
            if closure.contains(k) {
                return false;
            }
            cm.watermark().is_none_or(|w| w < min_wm)
        });
        Some(min_wm)
    }

    /// Evict every user module (those with a watermark) and return the
    /// earliest watermark among them. Used as the overflow fallback when a
    /// recompiled module no longer fits its reserved id range.
    pub fn invalidate_all(&mut self) -> Option<Watermark> {
        let min_wm = self
            .loaded
            .values()
            .filter_map(|cm| cm.watermark())
            .reduce(Watermark::earlier)?;
        self.loaded
            .retain(|_, cm| !matches!(cm.origin, ModuleOrigin::File { .. }));
        Some(min_wm)
    }
}

/// `(mtime, len)` of `path`, or `None` if either is unavailable. `None` makes
/// `source_changed` fall back to the read + hash path, so it is always safe.
fn file_stat(path: &Path) -> Option<FileStat> {
    let meta = std::fs::metadata(path).ok()?;
    #[cfg(unix)]
    let ino = std::os::unix::fs::MetadataExt::ino(&meta);
    #[cfg(not(unix))]
    let ino = 0;
    Some((meta.modified().ok()?, meta.len(), ino))
}

/// The stat gate for [`ModuleTable::source_changed`]: modification time,
/// length, and (on unix) inode. The inode catches the atomic-replace save
/// editors do, where a same-second edit can preserve both time and length;
/// an in-place write preserving all three in the same instant falls to the
/// LSP's `didChangeWatchedFiles` -> `invalidate_path` backstop.
type FileStat = (std::time::SystemTime, u64, u64);

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

/// A resolved import: where the module's source lives, plus its canonical
/// identity. Every downstream consumer must key on that identity, never on the
/// path as written.
#[derive(Debug, Clone)]
pub struct ResolvedModule {
    pub source: ModuleSource,
    /// [`file_module_path`] of the file an on-disk module resolved to; for
    /// embedded stdlib the written `al/...` path, which *is* its identity.
    pub canon: ModulePath,
    pub key: ModuleKey,
}

/// Resolve an import path as written to its source and canonical identity.
/// `base_dir` is the directory of the importing file, used for `./` and `../`
/// relative imports.
pub fn resolve(path: &ImportPath, base_dir: Option<&Path>) -> Result<ResolvedModule, ResolveError> {
    if path.is_relative() {
        let Some(base) = base_dir else {
            return Err(ResolveError::NoBaseDir);
        };
        let mut p: PathBuf = base.to_path_buf();
        for seg in &path.leading {
            match seg {
                RelSeg::CurrentDir => {}
                RelSeg::ParentDir => {
                    p.pop();
                }
            }
        }
        for name in &path.names {
            p.push(name);
        }
        // Append `.al` rather than `set_extension`, which would replace
        // anything after a dot in the module name (`./b.v2` -> `b.al`).
        if let Some(name) = p.file_name().map(|n| n.to_string_lossy().into_owned()) {
            p.set_file_name(format!("{name}.al"));
        }
        if p.is_file() {
            let canon = file_module_path(&p)?;
            let key = ModuleKey::of(&canon);
            return Ok(ResolvedModule {
                source: ModuleSource::File(p),
                canon,
                key,
            });
        }
        return Err(ResolveError::FileNotFound(p));
    }

    resolve_canonical(&path.names)
}

/// Resolve a canonical, non-relative module path: a [`file_module_path`]
/// identity, a stdlib path, or a bare name. This is the form paths take in the
/// reference graph, so the LSP's module-to-URI translation enters here.
pub fn resolve_canonical(path: &ModulePath) -> Result<ResolvedModule, ResolveError> {
    if is_resolved_file(path) {
        let key = ModuleKey::of(path);
        // Rebuild with the platform separator; the key's '/'-join form is
        // cache identity only.
        let p = PathBuf::from(format!("{}.al", path.join(std::path::MAIN_SEPARATOR_STR)));
        return if p.is_file() {
            Ok(ResolvedModule {
                source: ModuleSource::File(p),
                canon: path.clone(),
                key,
            })
        } else {
            Err(ResolveError::FileNotFound(p))
        };
    }

    if is_stdlib(path) {
        let key = ModuleKey::of(path);
        return match stdlib::lookup(key.as_str()) {
            Some(src) => Ok(ResolvedModule {
                source: ModuleSource::Embedded(src),
                canon: path.clone(),
                key,
            }),
            None => Err(ResolveError::NoSuchStdlibModule(path.clone())),
        };
    }

    // Bare names other than `al` are reserved for a future package manager.
    Err(ResolveError::BareName(path.join("/")))
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

    #[test]
    fn detect_stdlib_module_resolves_repo_std_files() {
        let array = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src/std/al/array.al"));
        assert_eq!(
            detect_stdlib_module(array),
            Some(vec!["al".to_string(), "array".to_string()])
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

        let mut missing = Vec::new();
        collect_al_files(&root.join("does-not-exist"), &mut missing);
        assert!(missing.is_empty());

        let _ = std::fs::remove_dir_all(&root);
    }

    fn rel(leading: &[RelSeg], names: &[&str]) -> ImportPath {
        ImportPath {
            leading: leading.to_vec(),
            names: names.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn resolve_stdlib_relative_and_errors() {
        match resolve(&rel(&[], &["al", "array"]), None) {
            Ok(r) => {
                assert!(matches!(r.source, ModuleSource::Embedded(_)));
                assert_eq!(r.canon, vec!["al".to_string(), "array".to_string()]);
                assert_eq!(r.key.as_str(), "al/array");
            }
            other => panic!("expected al/array to resolve embedded, got {other:?}"),
        }
        assert!(matches!(
            resolve(&rel(&[], &["al", "no_such_mod"]), None),
            Err(ResolveError::NoSuchStdlibModule(_))
        ));

        assert!(matches!(
            resolve(&rel(&[RelSeg::CurrentDir], &["foo"]), None),
            Err(ResolveError::NoBaseDir)
        ));

        let base = unique_dir("resolve");
        std::fs::write(base.join("foo.al"), "").unwrap();
        std::fs::write(base.join("bar.al"), "").unwrap();
        std::fs::write(base.join("b.v2.al"), "").unwrap();
        std::fs::create_dir_all(base.join("sub")).unwrap();

        match resolve(&rel(&[RelSeg::CurrentDir], &["foo"]), Some(&base)) {
            Ok(ResolvedModule {
                source: ModuleSource::File(p),
                canon,
                key,
            }) => {
                assert_eq!(p, base.join("foo.al"));
                assert_eq!(canon, file_module_path(&base.join("foo.al")).unwrap());
                assert_eq!(key, ModuleKey::for_file(&base.join("foo.al")));
            }
            other => panic!("expected ./foo to resolve to a file, got {other:?}"),
        }
        match resolve(
            &rel(&[RelSeg::ParentDir], &["bar"]),
            Some(&base.join("sub")),
        ) {
            Ok(ResolvedModule {
                source: ModuleSource::File(p),
                ..
            }) => assert_eq!(p, base.join("bar.al")),
            other => panic!("expected ../bar to resolve, got {other:?}"),
        }
        // A dot in the module name is part of the name.
        match resolve(&rel(&[RelSeg::CurrentDir], &["b.v2"]), Some(&base)) {
            Ok(ResolvedModule {
                source: ModuleSource::File(p),
                ..
            }) => assert_eq!(p, base.join("b.v2.al")),
            other => panic!("expected ./b.v2 to resolve to b.v2.al, got {other:?}"),
        }
        match resolve(&rel(&[RelSeg::CurrentDir], &["ghost"]), Some(&base)) {
            Err(ResolveError::FileNotFound(p)) => assert_eq!(p, base.join("ghost.al")),
            other => panic!("expected ./ghost to be FileNotFound, got {other:?}"),
        }
        assert!(matches!(
            resolve(&rel(&[], &["somepkg"]), None),
            Err(ResolveError::BareName(_))
        ));

        match resolve_canonical(&file_module_path(&base.join("foo.al")).unwrap()) {
            Ok(ResolvedModule {
                source: ModuleSource::File(p),
                ..
            }) => assert_eq!(p, std::path::absolute(base.join("foo.al")).unwrap()),
            other => panic!("expected canonical path to resolve, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn module_table_loading_and_insert_lifecycle() {
        let mut t = ModuleTable::new();
        let foo = ModuleKey::of(&vec!["foo".to_string()]);
        assert!(!t.is_loading(&foo));
        t.mark_loading(&foo);
        assert!(t.is_loading(&foo));

        t.insert_hydrated(foo.clone(), ModuleInterface::new(vec!["foo".to_string()]));
        assert!(!t.is_loading(&foo));
        assert!(t.get(&foo).is_some());

        let loaded = t.into_loaded();
        assert!(loaded.contains_key("foo"));
        assert_eq!(loaded.len(), 1);
    }

    #[test]
    fn source_changed_paths() {
        let mut t = ModuleTable::new();

        // Unknown key.
        assert!(!t.source_changed(&ModuleKey::of(&vec!["unknown".to_string()])));

        // Hydrated module never changes.
        let stat = ModuleKey::of(&vec!["static".to_string()]);
        t.insert_hydrated(
            stat.clone(),
            ModuleInterface::new(vec!["static".to_string()]),
        );
        assert!(!t.source_changed(&stat));

        let dir = unique_dir("srcchanged");
        let path = dir.join("m.al");
        let body = "x = 1\n";
        std::fs::write(&path, body).unwrap();
        let cm = CachedModule {
            iface: ModuleInterface::new(vec!["m".to_string()]),
            origin: ModuleOrigin::File {
                source_hash: source_hash(body),
                stat: None,
                watermark: Watermark::default(),
                path: path.clone(),
                refs: Rc::new(ModuleReferences::new(crate::reference::ModuleId(0))),
            },
            dependents: HashSet::new(),
        };
        let m = ModuleKey::of(&vec!["m".to_string()]);
        t.insert_cached(m.clone(), cm);
        assert!(!t.source_changed(&m), "identical content is unchanged");

        std::fs::remove_file(&path).unwrap();
        assert!(t.source_changed(&m), "a vanished file invalidates");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
