use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use indexmap::IndexMap;

use crate::bytecode::Watermark;
use crate::reference::ModuleReferences;
use crate::type_def::TypeId;
use crate::typed_ir::GlobalSlot;
use crate::types::{Scheme, TypeInfo};

pub mod stdlib;

pub type ModulePath = Vec<String>;

/// A module's canonical cache key. Unique because a resolved on-disk module's
/// identity is its canonical file path (see [`file_module_path`]).
///
/// Keying on the import *as written* was a wrong-answer bug: `./b` from two
/// different directories both key as `"b"`, so the second import silently
/// received the first module. That is why this is a type and not a `String`:
/// a `ModuleKey` can only be minted from a canonical identity — a resolved
/// on-disk file, a stdlib path (whose written form *is* its identity), the
/// prelude, or the entry `main` module — so keying [`ModuleTable`] or an id
/// range with an unresolved written path no longer typechecks.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModuleKey(String);

impl ModuleKey {
    /// The one place segments become a key. `pub(crate)` and no wider: the
    /// reference interner keys its path↔id bijection with it (the graph
    /// stores only canonical paths, so joining them *is* their key form);
    /// every other caller must go through a canonicalizing constructor
    /// below, so a key can never be built from a path as the user wrote it.
    pub(crate) fn of(path: &ModulePath) -> Self {
        ModuleKey(path.join("/"))
    }

    /// Key of the on-disk module at `p` (see [`file_module_path`]).
    pub fn for_file(p: &Path) -> Self {
        Self::of(&file_module_path(p))
    }

    /// Key of a stdlib module addressed by its written `al/...` path — the
    /// one written form that *is* canonical (there is no on-disk identity to
    /// resolve to; [`resolve`]'s embedded branch mints from it verbatim).
    pub fn for_stdlib(path: &ModulePath) -> Self {
        debug_assert!(is_stdlib(path), "not a stdlib path: {path:?}");
        Self::of(path)
    }

    /// Key of the prelude module ([`al_prelude`]).
    pub fn prelude() -> Self {
        Self::of(&al_prelude())
    }

    /// Key of the entry module ([`main_module`]).
    pub fn main() -> Self {
        Self::of(&main_module())
    }

    /// Escape hatch for the LSP string boundary (`module_or_uri` parameters):
    /// wraps the caller's string verbatim, with no canonicalization. This is
    /// lookup-not-mint — it exists so an externally supplied string can
    /// *address* entries keyed by real `ModuleKey`s; never insert into a
    /// cache under a key built here.
    pub fn from_lookup_str(s: &str) -> Self {
        ModuleKey(s.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ModuleKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The identity of an on-disk module: the segments of its canonicalized path,
/// `.al` stripped. [`ModuleKey`] joins them back into an absolute path, so the
/// key is globally unique; the last segment is still the module's name, so
/// `path.last()` reads `util` and not a temp directory.
///
/// The first segment marks it as resolved and keeps it distinct from a
/// stdlib path (`["al", "io"]`) or a written relative one (`[".", "b"]`):
/// on Unix an absolute path yields an empty first segment
/// (`"/a/b" -> ["", "a", "b"]`); on Windows it is the drive/UNC prefix
/// (`"C:\a\b" -> ["C:", "a", "b"]`). The prefix MUST be kept — dropping it
/// would merge `C:\proj\b.al` with `D:\proj\b.al`, the same
/// different-files-one-key collision this identity exists to prevent.
pub fn file_module_path(p: &Path) -> ModulePath {
    // Absolute and lexically normalised — deliberately NOT `canonicalize()`,
    // which resolves symlinks. On macOS a temp dir is `/var/...` but canonical
    // `/private/var/...`, so canonicalising here made the LSP hand the editor
    // two different URIs for one project: the entry as opened, its imports as
    // resolved. Identity must agree with the path the editor is using.
    let abs = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    let mut out: Vec<String> = Vec::new();
    for c in abs.components() {
        match c {
            std::path::Component::RootDir => out.push(String::new()),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if out.len() > 1 {
                    out.pop();
                }
            }
            std::path::Component::Normal(seg) => out.push(seg.to_string_lossy().into_owned()),
            std::path::Component::Prefix(pre) => {
                out.push(pre.as_os_str().to_string_lossy().into_owned());
            }
        }
    }
    if let Some(last) = out.last_mut()
        && let Some(stem) = last.strip_suffix(".al")
    {
        *last = stem.to_string();
    }
    out
}

/// `true` for a [`file_module_path`] — the identity of an already-resolved
/// on-disk module, as opposed to an import path as the user wrote it. The
/// marker is the first segment: empty (Unix root), ending in `:` (a Windows
/// drive prefix, `C:` / `\\?\C:`), or starting with `\\` (UNC / device).
/// A written import's first segment (`.`, `..`, a name) never matches any.
pub fn is_resolved_file(path: &ModulePath) -> bool {
    path.first()
        .is_some_and(|s| s.is_empty() || s.ends_with(':') || s.starts_with(r"\\"))
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

/// Walk up from `near` looking for `src/std/.al-stdlib-root` whose content
/// matches the value embedded at build time; on a hit, return the `src/std`
/// directory (the stdlib source root). The marker is a fixed UUID that never
/// changes, so editing stdlib files doesn't break detection and a non-AL
/// project can't accidentally match. Shared by [`detect_stdlib_module`]
/// (file → module path) and the LSP's inverse mapping (module path → file).
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

/// If `file` is a stdlib source file inside the AL compiler repo itself,
/// return its module path so the compiler can analyse it *as* that module
/// (allowing `@vm`/external and suppressing prelude-redefinition errors).
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
    pub local_slot: Option<GlobalSlot>,
    /// A function's parameter names, in order. Empty for anything else.
    ///
    /// Documentation, not semantics: al rejects labelled arguments outside a
    /// constructor call, so a parameter's name never reaches a call site.
    /// Putting it in `TypeNode::Fun` would force unification to decide whether
    /// `fn(path String)` equals `fn(p String)` — and either the label is dead
    /// weight in every unification, or renaming a parameter breaks callers.
    /// Constructor field labels *are* semantic, and live in the type
    /// (`ValueKind::Constructor { field_labels }`). These do not.
    pub param_names: Vec<String>,
    /// The declaration's own doc comment. Carried across module boundaries so
    /// hovering `io.read_text` in another file shows its prose.
    pub doc: Option<String>,
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
    /// The module's own doc comment: the `/** */` block at line 0 of its
    /// source. `None` for a module without one. Unlike every other doc in the
    /// language, this one is carried through the precompiled stdlib blob
    /// (`SModule::doc`) and re-seeded onto the reference graph by
    /// `synth_refs_from_interface`, so hovering `al/scheduler` shows its prose.
    pub doc: Option<String>,
}

impl ModuleInterface {
    pub fn new(path: ModulePath) -> Self {
        ModuleInterface {
            path,
            types: IndexMap::new(),
            values: IndexMap::new(),
            private_names: HashSet::new(),
            doc: None,
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

/// Proof that [`ModuleTable::id_base_for`] reserved a type-id range. Carries
/// the range start together with whether an existing assignment was reused,
/// so the pair can never be mismatched across the gap between reserving the
/// range and recording its usage: [`Self::note_usage`] consumes the token.
#[must_use = "hand the reservation back via note_usage once ids are allocated"]
pub struct IdRangeReservation {
    base: TypeId,
    /// `true` when an existing assignment was reused. A fresh allocation is
    /// always at `id_high_water`, so a first-compile overflow (deps allocate
    /// before importer) only spills into unallocated space and `note_usage`
    /// simply pushes `id_high_water` past it; a reused-range overflow may
    /// collide with a sibling's already-assigned block and so raises
    /// `id_range_overflow`.
    reused: bool,
}

impl IdRangeReservation {
    /// Start of the reserved range: the module's first nominal type id.
    pub fn base(&self) -> TypeId {
        self.base
    }

    /// Record that the module allocated `used` type ids from this range. On a
    /// reused range, allocating past `MODULE_TYPE_ID_RANGE` may collide with a
    /// sibling's already-assigned block, so the overflow flag is raised; on a
    /// fresh range the spillover is into unallocated space. Either way the
    /// high-water mark is bumped past actual usage so the next fresh
    /// allocation skips the spillover.
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
    /// to `s.lookup_module(key)` on a `loaded` miss and caches the result.
    static_fallback: Option<&'static crate::static_ir::StaticStdlib>,
    /// In-memory document overrides (LSP unsaved buffers). Module resolution
    /// prefers these over `fs::read_to_string`.
    overlays: HashMap<PathBuf, String>,
    /// Per-module `(mtime, len)` of the on-disk source as of the last time
    /// its content was hashed and found to match the cached `source_hash`.
    /// `IncrementalSession::check` stat-gates the per-keystroke staleness scan
    /// against this: an unchanged tuple means the bytes are unchanged, so the
    /// full read + FNV hash is skipped. The hash stays the source of truth —
    /// a stat miss falls through to read + hash and then refreshes the tuple.
    stat_cache: HashMap<ModuleKey, (std::time::SystemTime, u64)>,
    /// Incremented every time a module body is actually compiled (not on cache
    /// hit). Test/telemetry only.
    compile_count: u32,
    /// Per-module type-id range starts. Assigned on a module's first compile
    /// and *retained across cache eviction* so a recompile hands out the same
    /// nominal type ids. Only cleared by `reset_id_bases` (overflow fallback).
    id_bases: HashMap<ModuleKey, TypeId>,
    /// Lowest type id not covered by any allocated `id_base` range; the next
    /// fresh `id_base_for` rounds up from `max(floor, id_high_water)`.
    id_high_water: TypeId,
    /// Set when a recompiled module allocated past its reserved range and may
    /// have collided with a later module's ids. `IncrementalSession` reacts by
    /// performing a full invalidate on the next `check`.
    id_range_overflow: bool,
}

/// Hand-written (`TypeId` deliberately has no `Default`): `id_high_water`
/// starts at the `NONE` sentinel — no id range reserved yet, the same state
/// [`reset_id_bases`](ModuleTable::reset_id_bases) restores.
impl Default for ModuleTable {
    fn default() -> Self {
        ModuleTable {
            loaded: IndexMap::new(),
            loading: HashSet::new(),
            static_fallback: None,
            overlays: HashMap::new(),
            stat_cache: HashMap::new(),
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
            .map(|(k, v)| (k.0, v.iface))
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
    pub fn source_changed(&mut self, key: &ModuleKey) -> bool {
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
                let changed = source_hash(&t) != expected_hash;
                if !changed && let Some(s) = stat {
                    self.stat_cache.insert(key.clone(), s);
                }
                changed
            }
            Err(_) => {
                self.stat_cache.remove(key);
                true // file vanished — invalidate
            }
        }
    }

    /// Iterate every cached module — user *and* hydrated stdlib. Used to merge
    /// all surviving modules' references into the workspace `ReferenceGraph`.
    pub fn loaded_modules(&self) -> impl Iterator<Item = (&ModuleKey, &CachedModule)> {
        self.loaded.iter()
    }

    /// The persisted per-module reference data for the module whose canonical
    /// path is `path` (e.g. an interface's `path`), if it was compiled from
    /// source this session. `None` for static/hydrated stdlib modules (their
    /// definitions are synthesised lazily from the interface). Lookup-not-mint:
    /// a non-canonical path simply misses.
    pub fn module_refs_by_path(&self, path: &ModulePath) -> Option<&ModuleReferences> {
        self.loaded
            .get(&ModuleKey::of(path))
            .and_then(|c| c.module_refs())
            .map(Rc::as_ref)
    }

    /// Reserve (or re-find) the type-id range for `key`, allocating a fresh
    /// 256-aligned range on first request. `floor` is the caller's current
    /// `next_type_id` so the first user module's range sits past every stdlib
    /// id. Returns a token carrying the range start plus whether an existing
    /// assignment was reused; once the module's body has allocated its ids,
    /// hand the token back via [`IdRangeReservation::note_usage`].
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
    pub fn id_base_of(&self, key: &ModuleKey) -> Option<TypeId> {
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    NotFound(String),
    NoBaseDir,
    /// A bare module name that is neither `al/*` nor `./`/`../`-relative.
    /// Reserved for a future package manager; distinct from `NotFound` so the
    /// diagnostic can suggest `use "./name"` instead of "not found".
    BareName(String),
}

/// A resolved import: where the module's source lives, plus its canonical
/// identity — the segments every downstream consumer (module cache, reference
/// interner, qualifier map) must key on, never the path as written.
#[derive(Debug, Clone)]
pub struct ResolvedModule {
    pub source: ModuleSource,
    /// Canonical identity segments: [`file_module_path`] of the file an
    /// on-disk module resolved to; the `al/...` path itself for embedded
    /// stdlib (its written form *is* its identity).
    pub canon: ModulePath,
    pub key: ModuleKey,
}

/// Resolve a module path as written to its source and canonical identity.
/// `base_dir` is the directory of the importing file, used for `./` and `../`
/// relative imports.
pub fn resolve(path: &ModulePath, base_dir: Option<&Path>) -> Result<ResolvedModule, ResolveError> {
    // Already resolved: a canonical file identity, independent of `base_dir`.
    if is_resolved_file(path) {
        let key = ModuleKey::of(path);
        let p = PathBuf::from(format!("{key}.al"));
        return if p.is_file() {
            Ok(ResolvedModule {
                source: ModuleSource::File(p),
                canon: path.clone(),
                key,
            })
        } else {
            Err(ResolveError::NotFound(p.display().to_string()))
        };
    }

    // Stdlib: any path rooted at `al`.
    if is_stdlib(path) {
        let key = ModuleKey::of(path);
        return match stdlib::lookup(key.as_str()) {
            Some(src) => Ok(ResolvedModule {
                source: ModuleSource::Embedded(src),
                canon: path.clone(),
                key,
            }),
            None => Err(ResolveError::NotFound(key.0)),
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
        // Append `.al` rather than `set_extension`, which *replaces* anything
        // after a dot in the module name (`./b.v2` would look up `b.al`);
        // appending is the inverse of `file_module_path`'s
        // `strip_suffix(".al")`, so the name round-trips.
        if let Some(name) = p.file_name().map(|n| n.to_string_lossy().into_owned()) {
            p.set_file_name(format!("{name}.al"));
        }
        if p.is_file() {
            let canon = file_module_path(&p);
            let key = ModuleKey::of(&canon);
            return Ok(ResolvedModule {
                source: ModuleSource::File(p),
                canon,
                key,
            });
        }
        return Err(ResolveError::NotFound(p.display().to_string()));
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

    // A real stdlib source file inside this very repo is detected as its module
    // path so the compiler can analyse it *as* that module. The marker file
    // itself is not a module, and an unrelated path matches nothing.
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
        // Embedded stdlib: source, canonical path, and key all line up.
        match resolve(&vec!["al".into(), "array".into()], None) {
            Ok(r) => {
                assert!(matches!(r.source, ModuleSource::Embedded(_)));
                assert_eq!(r.canon, vec!["al".to_string(), "array".to_string()]);
                assert_eq!(r.key.as_str(), "al/array");
            }
            other => panic!("expected al/array to resolve embedded, got {other:?}"),
        }
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
        std::fs::write(base.join("b.v2.al"), "").unwrap();
        std::fs::create_dir_all(base.join("sub")).unwrap();

        // `./foo` from base; the canonical key comes from the resolved file.
        match resolve(&vec![".".into(), "foo".into()], Some(&base)) {
            Ok(ResolvedModule {
                source: ModuleSource::File(p),
                canon,
                key,
            }) => {
                assert_eq!(p, base.join("foo.al"));
                assert_eq!(canon, file_module_path(&base.join("foo.al")));
                assert_eq!(key, ModuleKey::for_file(&base.join("foo.al")));
            }
            other => panic!("expected ./foo to resolve to a file, got {other:?}"),
        }
        // `../bar` from base/sub climbs to base/bar.al.
        match resolve(&vec!["..".into(), "bar".into()], Some(&base.join("sub"))) {
            Ok(ResolvedModule {
                source: ModuleSource::File(p),
                ..
            }) => assert_eq!(p, base.join("bar.al")),
            other => panic!("expected ../bar to resolve, got {other:?}"),
        }
        // A dot in the module name is part of the name: `./b.v2` resolves to
        // `b.v2.al` (appending `.al`), not `b.al` (replacing the suffix).
        match resolve(&vec![".".into(), "b.v2".into()], Some(&base)) {
            Ok(ResolvedModule {
                source: ModuleSource::File(p),
                ..
            }) => assert_eq!(p, base.join("b.v2.al")),
            other => panic!("expected ./b.v2 to resolve to b.v2.al, got {other:?}"),
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
        let foo = ModuleKey::of(&vec!["foo".to_string()]);
        assert!(!t.is_loading(&foo));
        t.mark_loading(&foo);
        assert!(t.is_loading(&foo));

        t.insert_hydrated(foo.clone(), ModuleInterface::new(vec!["foo".to_string()]));
        // insert_hydrated clears the loading mark and makes the interface visible.
        assert!(!t.is_loading(&foo));
        assert!(t.get(&foo).is_some());

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
        assert!(!t.source_changed(&ModuleKey::of(&vec!["unknown".to_string()])));

        // Hydrated module never changes.
        let stat = ModuleKey::of(&vec!["static".to_string()]);
        t.insert_hydrated(
            stat.clone(),
            ModuleInterface::new(vec!["static".to_string()]),
        );
        assert!(!t.source_changed(&stat));

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
        let m = ModuleKey::of(&vec!["m".to_string()]);
        t.insert_cached(m.clone(), cm);
        assert!(!t.source_changed(&m), "identical content is unchanged");

        std::fs::remove_file(&path).unwrap();
        assert!(t.source_changed(&m), "a vanished file invalidates");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
