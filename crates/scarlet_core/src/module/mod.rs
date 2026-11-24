//! Where modules come from: resolving an import as written to the module it
//! names, and the stdlib embedded in the compiler.

use std::path::{Path, PathBuf};

use crate::ast::{ImportPath, RelSeg};

mod stdlib;

// Module identity lives in `scarlet_syntax::module_path` because a `Diagnostic`
// carries the key of the module it points into. We re-export it here, where
// resolution mints the keys.
pub use scarlet_syntax::module_path::{
    ModuleKey, ModulePath, ResolveError, SOURCE_EXTENSION, file_module_path, is_resolved_file,
    is_stdlib, main_module, scarlet_prelude,
};

/// Every module the embedded stdlib holds, sorted by the path an import names
/// it with.
pub fn stdlib_modules() -> Vec<ModulePath> {
    stdlib::module_paths()
}

const STDLIB_MARKER: &str = include_str!("../std/.scarlet-stdlib-root");

/// Walk up from `near` looking for a `src/std/.scarlet-stdlib-root` marker that
/// matches the one embedded at build time, and return that `src/std` directory.
/// The marker is a fixed UUID, so editing stdlib files cannot break detection
/// and an unrelated project cannot match by accident.
pub fn find_stdlib_root(near: &Path) -> Option<PathBuf> {
    let near = near.canonicalize().ok()?;
    let mut dir = near.parent()?.to_path_buf();
    loop {
        let marker = dir.join("src/std/.scarlet-stdlib-root");
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

/// The module path of `file`, if it is a stdlib source file inside the Scarlet
/// compiler repo itself. The compiler then analyses it *as* that module, which
/// allows `@vm` and external functions and turns off prelude-redefinition
/// errors.
pub fn detect_stdlib_module(file: &Path) -> Option<ModulePath> {
    let file = file.canonicalize().ok()?;
    let std_root = find_stdlib_root(&file)?;
    let rel = file.strip_prefix(&std_root).ok()?;
    // src/std/scarlet.sca -> ["scarlet"]; src/std/scarlet/net.sca -> ["scarlet","net"]
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
        segs = scarlet_prelude();
    }
    Some(segs)
}

/// Collect every `.sca` source file under `dir` into `out`, recursively. It
/// skips dot directories, `target` and `node_modules`, and ignores entries it
/// can't read instead of stopping. It reads `file_type()`, so it never follows
/// a symlink and a symlink cycle cannot hang the walk.
pub fn collect_source_files(dir: &Path, out: &mut Vec<PathBuf>) {
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
            collect_source_files(&path, out);
        } else if ft.is_file()
            && path.extension().and_then(|e| e.to_str()) == Some(SOURCE_EXTENSION)
        {
            out.push(path);
        }
    }
}

#[derive(Debug, Clone)]
pub enum ModuleSource {
    Embedded(&'static str),
    File(PathBuf),
}

/// A resolved import, which says where the module's source lives and what its
/// canonical identity is. Everything downstream must key on that identity,
/// never on the path as written.
#[derive(Debug, Clone)]
pub struct ResolvedModule {
    pub(crate) source: ModuleSource,
    pub(crate) key: ModuleKey,
}

/// [`resolve_with`] takes an optional stdlib source root on disk. If it is set
/// and the path names a stdlib module whose `.sca` file exists under that root,
/// the module resolves as an ordinary `File` source with the same identity. So
/// a session that edits the stdlib itself compiles, caches and invalidates
/// stdlib modules just like any other module on disk, instead of reading the
/// copy embedded at build time.
pub(crate) fn resolve_with(
    path: &ImportPath,
    base_dir: Option<&Path>,
    stdlib_root: Option<&Path>,
) -> Result<ResolvedModule, ResolveError> {
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
        // Append `.sca` rather than `set_extension`, which would replace
        // anything after a dot in the module name (`./b.v2` -> `b.sca`).
        if let Some(name) = p.file_name().map(|n| n.to_string_lossy().into_owned()) {
            p.set_file_name(format!("{name}.{SOURCE_EXTENSION}"));
        }
        if p.is_file() {
            let key = ModuleKey::of(&file_module_path(&p)?);
            return Ok(ResolvedModule {
                source: ModuleSource::File(p),
                key,
            });
        }
        return Err(ResolveError::FileNotFound(p));
    }

    resolve_canonical_with(&path.names, stdlib_root)
}

/// Resolve a canonical, non-relative module path: a [`file_module_path`]
/// identity, a stdlib path, or a bare name. This is the form paths take in the
/// reference graph, so the LSP's module-to-URI translation enters here.
pub(crate) fn resolve_canonical(path: &ModulePath) -> Result<ResolvedModule, ResolveError> {
    resolve_canonical_with(path, None)
}

/// [`resolve_canonical`] with an optional stdlib root on disk. See
/// [`resolve_with`].
fn resolve_canonical_with(
    path: &ModulePath,
    stdlib_root: Option<&Path>,
) -> Result<ResolvedModule, ResolveError> {
    if is_resolved_file(path) {
        let key = ModuleKey::of(path);
        // Rebuild the path with the platform separator. The key joins with '/',
        // but that form is only the cache identity.
        let p = PathBuf::from(format!(
            "{}.{SOURCE_EXTENSION}",
            path.join(std::path::MAIN_SEPARATOR_STR)
        ));
        return if p.is_file() {
            Ok(ResolvedModule {
                source: ModuleSource::File(p),
                key,
            })
        } else {
            Err(ResolveError::FileNotFound(p))
        };
    }

    if is_stdlib(path) {
        let key = ModuleKey::of(path);
        // When you edit the stdlib in this repo, the source on disk is the real
        // one and the embedded copy is stale from build time. The identity
        // (`scarlet/...`) is the same either way.
        if let Some(root) = stdlib_root {
            let mut p = root.to_path_buf();
            for seg in path {
                p.push(seg);
            }
            p.set_extension(SOURCE_EXTENSION);
            if p.is_file() {
                return Ok(ResolvedModule {
                    source: ModuleSource::File(p),
                    key,
                });
            }
        }
        return match stdlib::lookup(key.as_str()) {
            Some(src) => Ok(ResolvedModule {
                source: ModuleSource::Embedded(src),
                key,
            }),
            None => Err(ResolveError::NoSuchStdlibModule(path.clone())),
        };
    }

    // Bare names other than `scarlet` are reserved for a future package manager.
    Err(ResolveError::BareName(path.join("/")))
}

#[cfg(test)]
mod tests;
