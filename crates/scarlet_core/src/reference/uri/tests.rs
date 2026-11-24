use super::*;
use crate::reference::ReferenceGraphBuilder;
use crate::reference::tests::mp;

#[test]
fn path_to_uri_percent_encodes() {
    assert_eq!(
        path_to_uri(Path::new("/My Proj/foo.sca")),
        "file:///My%20Proj/foo.sca"
    );
    assert_eq!(
        path_to_uri(Path::new("/plain/foo.sca")),
        "file:///plain/foo.sca"
    );
    assert_eq!(
        path_to_uri(Path::new("/a+b/c:d(e)'!.sca")),
        "file:///a%2Bb/c%3Ad%28e%29%27%21.sca"
    );
}

#[test]
fn module_uri_stdlib_is_embedded_error() {
    let mut g = ReferenceGraphBuilder::new();
    let std_mod = g.intern_module(&mp(&["scarlet", "array"]));
    let bare = g.intern_module(&mp(&["whatever"]));
    let g = g.finish();
    assert_eq!(module_uri(&g, std_mod), Err(ModuleUriError::Embedded));
    assert!(matches!(
        module_uri(&g, bare),
        Err(ModuleUriError::Resolve(_))
    ));
    assert_eq!(module_uri(&g, ModuleId(99)), Err(ModuleUriError::NoPath));
}

#[test]
fn module_uri_resolves_canonical_file() {
    let dir = std::env::temp_dir().join(format!("al_uri_test_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let file = dir.join("helpers.sca");
    std::fs::write(&file, "pub fn x() { 1 }\n").expect("write temp module");

    let mut g = ReferenceGraphBuilder::new();
    let canon = crate::module::file_module_path(&file).unwrap();
    let m = g.intern_module(&canon);
    let g = g.finish();
    let uri = module_uri(&g, m).expect("canonical module resolves");
    assert!(uri.starts_with("file://"));
    assert!(uri.ends_with("helpers.sca"));
    assert_eq!(uri, path_to_uri(&std::path::absolute(&file).unwrap()));

    let _ = std::fs::remove_file(&file);
    let _ = std::fs::remove_dir(&dir);
}
