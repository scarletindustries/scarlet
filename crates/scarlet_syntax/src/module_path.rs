//! Module identity: the canonical path segments a module is known by and the
//! [`ModuleKey`] every cache and graph must key on. It lives here because a
//! [`Diagnostic`](crate::diagnostic::Diagnostic) carries the key of the
//! module it points into. Module *resolution* stays in `scarlet_core::module`.

use std::path::{Path, PathBuf};

pub type ModulePath = Vec<String>;

/// A module's canonical cache key: for an on-disk module, its canonical file
/// path (see [`file_module_path`]).
///
/// It's a type rather than a `String`, so it can only be made from a canonical
/// identity. Keying on the import as written gives wrong answers: `./b` from
/// two different directories both key as `"b"`, and the second import
/// silently gets the first module.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModuleKey(String);

impl ModuleKey {
    /// The one place segments become a key. Only call it with segments that
    /// are already canonical, and go through one of the canonicalising
    /// constructors below otherwise. Debug builds check the segments really
    /// are canonical, so the collision this type exists to prevent can't come
    /// back through here unnoticed.
    pub fn of(path: &ModulePath) -> Self {
        debug_assert!(
            path.iter().enumerate().all(|(i, s)| {
                // A canonical absolute file path stores its leading `/` as one
                // empty first segment. Anywhere else, an empty segment, a dot
                // segment or an embedded separator means the caller skipped
                // canonicalisation.
                (i == 0 || !s.is_empty()) && s != "." && s != ".." && !s.contains('/')
            }),
            "ModuleKey::of on unnormalised segments {path:?}"
        );
        ModuleKey(path.join("/"))
    }

    /// Key of the on-disk module at `p` (see [`file_module_path`]).
    ///
    /// Panics if `p` has no absolute form. Where that can happen, use
    /// [`file_module_path`], which returns the error instead.
    #[allow(clippy::expect_used)]
    pub fn for_file(p: &Path) -> Self {
        Self::of(&file_module_path(p).expect("on-disk module path has an absolute form"))
    }

    /// Key of a stdlib module, from its `scarlet/...` path as written. That's
    /// the one written form that is already canonical.
    pub fn for_stdlib(path: &ModulePath) -> Self {
        debug_assert!(is_stdlib(path), "not a stdlib path: {path:?}");
        Self::of(path)
    }

    /// Key of the entry module ([`main_module`]).
    pub fn main() -> Self {
        Self::of(&main_module())
    }

    /// An escape hatch for strings coming in from the LSP. It wraps the string
    /// as it is, without canonicalising it, so use it only for lookups. Never
    /// insert into a cache under a key built here.
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

/// The extension of a Scarlet source file, without its dot.
pub const SOURCE_EXTENSION: &str = "sca";

/// The identity of an on-disk module: the segments of its absolute path with
/// `.sca` stripped. `path.last()` is still the module's name.
///
/// The first segment is the root marker (empty on Unix, the drive/UNC prefix
/// on Windows) and keeps a resolved path distinct from a stdlib path or a
/// bare name. Do not drop it: without it `C:\proj\b.sca` and `D:\proj\b.sca`
/// collide on one key.
///
/// Returns [`ResolveError::UnresolvablePath`] when `p` has no absolute form,
/// because falling back to the path as given would make a relative identity.
pub fn file_module_path(p: &Path) -> Result<ModulePath, ResolveError> {
    // Normalise lexically, and deliberately not with `canonicalize()`, which
    // resolves symlinks. On macOS a temp dir is `/var/...` but canonically
    // `/private/var/...`, which made the LSP hand the editor two different
    // URIs for one project. The identity must match the path the editor uses.
    let abs =
        std::path::absolute(p).map_err(|_| ResolveError::UnresolvablePath(p.to_path_buf()))?;
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
        && let Some(stem) = last
            .strip_suffix(SOURCE_EXTENSION)
            .and_then(|s| s.strip_suffix('.'))
    {
        *last = stem.to_string();
    }
    Ok(out)
}

/// Whether `path` came from [`file_module_path`] rather than being an import
/// path as written. The marker is the first segment: empty (the Unix root),
/// ending in `:` (a Windows drive prefix), or starting with `\\` (a UNC or
/// device path).
pub fn is_resolved_file(path: &ModulePath) -> bool {
    path.first()
        .is_some_and(|s| s.is_empty() || s.ends_with(':') || s.starts_with(r"\\"))
}

/// Whether the module is rooted at `scarlet`: the stdlib, the prelude or the
/// `@vm` intrinsics. A user's project must never rewrite those declarations.
pub fn is_stdlib(path: &ModulePath) -> bool {
    path.first().map(String::as_str) == Some("scarlet")
}

pub fn scarlet_prelude() -> ModulePath {
    vec!["scarlet".to_string()]
}

pub fn main_module() -> ModulePath {
    vec!["main".to_string()]
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// No file at the resolved location. Carries the path probed.
    FileNotFound(PathBuf),
    /// The file exists but has no absolute form, so there's no canonical
    /// identity to key on. See [`file_module_path`].
    UnresolvablePath(PathBuf),
    /// A `scarlet/...` path naming no embedded stdlib module.
    NoSuchStdlibModule(ModulePath),
    NoBaseDir,
    /// A bare module name, neither `scarlet/*` nor relative to `./`. It's kept
    /// apart from the not-found errors so the diagnostic can suggest a relative
    /// path like `./name`.
    BareName(String),
}

/// The shared user-facing wording for each resolution failure. The compiler
/// gives `BareName` and `NoBaseDir` their own wording, which knows about import
/// syntax.
impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolveError::FileNotFound(p) => write!(f, "file not found: {}", p.display()),
            ResolveError::UnresolvablePath(p) => {
                write!(f, "cannot determine an absolute path for {}", p.display())
            }
            ResolveError::NoSuchStdlibModule(p) => {
                write!(f, "no such stdlib module {}", p.join("/"))
            }
            ResolveError::NoBaseDir => write!(f, "no base directory to resolve against"),
            ResolveError::BareName(n) => write!(f, "bare module name `{n}` has no source file"),
        }
    }
}
