use super::*;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};

/// Resolve an import path as written to its source and canonical identity.
/// `base_dir` is the directory of the importing file, used for `./` and `../`
/// relative imports.
fn resolve(path: &ImportPath, base_dir: Option<&Path>) -> Result<ResolvedModule, ResolveError> {
    resolve_with(path, base_dir, None)
}

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
    let array = Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/std/scarlet/array.sca"
    ));
    assert_eq!(
        detect_stdlib_module(array),
        Some(vec!["scarlet".to_string(), "array".to_string()])
    );

    let prelude = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src/std/scarlet.sca"));
    assert_eq!(
        detect_stdlib_module(prelude),
        Some(vec!["scarlet".to_string()])
    );

    // The `.scarlet-stdlib-root` marker is not itself a module.
    let marker = Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/std/.scarlet-stdlib-root"
    ));
    assert_eq!(detect_stdlib_module(marker), None);

    // A path with no stdlib root above it resolves to nothing.
    let outside = unique_dir("detect_outside").join("foo.sca");
    std::fs::write(&outside, "x = 1").unwrap();
    assert_eq!(detect_stdlib_module(&outside), None);
}

#[test]
fn collect_source_files_filters_and_recurses() {
    let root = unique_dir("collect");
    std::fs::write(root.join("a.sca"), "").unwrap();
    std::fs::write(root.join("b.txt"), "").unwrap();
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("sub/c.sca"), "").unwrap();
    std::fs::create_dir_all(root.join(".hidden")).unwrap();
    std::fs::write(root.join(".hidden/d.sca"), "").unwrap();
    std::fs::create_dir_all(root.join("target")).unwrap();
    std::fs::write(root.join("target/e.sca"), "").unwrap();
    std::fs::create_dir_all(root.join("node_modules")).unwrap();
    std::fs::write(root.join("node_modules/f.sca"), "").unwrap();

    let mut out = Vec::new();
    collect_source_files(&root, &mut out);
    let names: HashSet<String> = out
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        HashSet::from(["a.sca".to_string(), "c.sca".to_string()]),
        "should collect only a.sca and sub/c.sca; got {out:?}"
    );

    let mut missing = Vec::new();
    collect_source_files(&root.join("does-not-exist"), &mut missing);
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
    match resolve(&rel(&[], &["scarlet", "array"]), None) {
        Ok(r) => {
            assert!(matches!(r.source, ModuleSource::Embedded(_)));
            assert_eq!(r.key.as_str(), "scarlet/array");
        }
        other => panic!("expected scarlet/array to resolve embedded, got {other:?}"),
    }
    assert!(matches!(
        resolve(&rel(&[], &["scarlet", "no_such_mod"]), None),
        Err(ResolveError::NoSuchStdlibModule(_))
    ));

    assert!(matches!(
        resolve(&rel(&[RelSeg::CurrentDir], &["foo"]), None),
        Err(ResolveError::NoBaseDir)
    ));

    let base = unique_dir("resolve");
    std::fs::write(base.join("foo.sca"), "").unwrap();
    std::fs::write(base.join("bar.sca"), "").unwrap();
    std::fs::write(base.join("b.v2.sca"), "").unwrap();
    std::fs::create_dir_all(base.join("sub")).unwrap();

    match resolve(&rel(&[RelSeg::CurrentDir], &["foo"]), Some(&base)) {
        Ok(ResolvedModule {
            source: ModuleSource::File(p),
            key,
        }) => {
            assert_eq!(p, base.join("foo.sca"));
            assert_eq!(key, ModuleKey::for_file(&base.join("foo.sca")).unwrap());
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
        }) => assert_eq!(p, base.join("bar.sca")),
        other => panic!("expected ../bar to resolve, got {other:?}"),
    }
    // A dot in the module name is part of the name.
    match resolve(&rel(&[RelSeg::CurrentDir], &["b.v2"]), Some(&base)) {
        Ok(ResolvedModule {
            source: ModuleSource::File(p),
            ..
        }) => assert_eq!(p, base.join("b.v2.sca")),
        other => panic!("expected ./b.v2 to resolve to b.v2.sca, got {other:?}"),
    }
    match resolve(&rel(&[RelSeg::CurrentDir], &["ghost"]), Some(&base)) {
        Err(ResolveError::FileNotFound(p)) => assert_eq!(p, base.join("ghost.sca")),
        other => panic!("expected ./ghost to be FileNotFound, got {other:?}"),
    }
    assert!(matches!(
        resolve(&rel(&[], &["somepkg"]), None),
        Err(ResolveError::BareName(_))
    ));

    match resolve_canonical(&file_module_path(&base.join("foo.sca")).unwrap()) {
        Ok(ResolvedModule {
            source: ModuleSource::File(p),
            ..
        }) => assert_eq!(p, std::path::absolute(base.join("foo.sca")).unwrap()),
        other => panic!("expected canonical path to resolve, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&base);
}
