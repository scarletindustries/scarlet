//! Module *identity*: the canonical path segments a module is known by and
//! the [`ModuleKey`] every cache and graph must key on. Identity lives in
//! `al_syntax` because a [`Diagnostic`](crate::diagnostic::Diagnostic)
//! carries the key of the module it points into; module *resolution* (the
//! table, caching, import handling) stays in `al_core::module` and builds
//! on these types.

use std::path::{Path, PathBuf};

pub type ModulePath = Vec<String>;

/// A module's canonical cache key. Unique because a resolved on-disk module's
/// identity is its canonical file path (see [`file_module_path`]).
///
/// Keying on the import *as written* was a wrong-answer bug: `./b` from two
/// different directories both key as `"b"`, so the second import silently
/// received the first module. That is why this is a type and not a `String`:
/// a `ModuleKey` can only be minted from a canonical identity — a resolved
/// on-disk file, a stdlib path (whose written form *is* its identity), the
/// prelude, or the entry `main` module — so keying `al_core`'s
/// `ModuleTable` or an id range with an unresolved written path no longer
/// typechecks.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModuleKey(String);

impl ModuleKey {
    /// The one place segments become a key. Public for exactly two callers
    /// — `al_core`'s module resolver (which mints from canonical segments)
    /// and its reference interner (whose graph stores only canonical paths,
    /// so joining them *is* their key form); every other caller must go
    /// through a canonicalizing constructor below, so a key can never be
    /// built from a path as the user wrote it.
    pub fn of(path: &ModulePath) -> Self {
        ModuleKey(path.join("/"))
    }

    /// Key of the on-disk module at `p` (see [`file_module_path`]).
    ///
    /// Panics if `p` has no absolute form (empty path / unfetchable cwd);
    /// callers hand in real on-disk paths, for which that cannot happen.
    /// Fallible resolution goes through [`resolve`] instead.
    #[allow(clippy::expect_used)] // asserting the documented precondition above
    pub fn for_file(p: &Path) -> Self {
        Self::of(&file_module_path(p).expect("on-disk module path has an absolute form"))
    }

    /// Key of a stdlib module addressed by its written `al/...` path — the
    /// one written form that *is* canonical (there is no on-disk identity to
    /// resolve to; [`resolve_canonical`]'s embedded branch mints from it
    /// verbatim).
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
/// stdlib path (`["al", "io"]`) or a bare name (`["b"]`):
/// on Unix an absolute path yields an empty first segment
/// (`"/a/b" -> ["", "a", "b"]`); on Windows it is the drive/UNC prefix
/// (`"C:\a\b" -> ["C:", "a", "b"]`). The prefix MUST be kept — dropping it
/// would merge `C:\proj\b.al` with `D:\proj\b.al`, the same
/// different-files-one-key collision this identity exists to prevent.
///
/// Errs with [`ResolveError::UnresolvablePath`] when `p` has no absolute
/// form (an empty path, or an unfetchable cwd) — such a file has no
/// canonical identity, and falling back to the path as given would mint a
/// relative "identity" that violates the invariant above.
pub fn file_module_path(p: &Path) -> Result<ModulePath, ResolveError> {
    // Absolute and lexically normalised — deliberately NOT `canonicalize()`,
    // which resolves symlinks. On macOS a temp dir is `/var/...` but canonical
    // `/private/var/...`, so canonicalising here made the LSP hand the editor
    // two different URIs for one project: the entry as opened, its imports as
    // resolved. Identity must agree with the path the editor is using.
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
        && let Some(stem) = last.strip_suffix(".al")
    {
        *last = stem.to_string();
    }
    Ok(out)
}

/// `true` for a [`file_module_path`] — the identity of an already-resolved
/// on-disk module, as opposed to an import path as the user wrote it. The
/// marker is the first segment: empty (Unix root), ending in `:` (a Windows
/// drive prefix, `C:` / `\\?\C:`), or starting with `\\` (UNC / device).
/// A written import's first name segment never matches any (its relative
/// markers are typed `ast::RelSeg`s and cannot appear here at all).
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// The import resolved to an on-disk location, but no file is there.
    /// Carries the filesystem path that was probed.
    FileNotFound(PathBuf),
    /// The file exists but `std::path::absolute` failed for it (an empty
    /// path, or an unfetchable cwd), so it has no canonical identity to key
    /// on. See [`file_module_path`].
    UnresolvablePath(PathBuf),
    /// An `al/...` path that names no embedded stdlib module. Carries the
    /// module path as written (there is no filesystem path to show).
    NoSuchStdlibModule(ModulePath),
    NoBaseDir,
    /// A bare module name that is neither `al/*` nor `./`/`../`-relative.
    /// Reserved for a future package manager; distinct from the not-found
    /// errors so the diagnostic can suggest `use "./name"` instead.
    BareName(String),
}

/// The one user-facing rendering of each resolution failure; every consumer
/// (compiler diagnostics, `ModuleUriError`) delegates here rather than
/// wording its own copy.
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
