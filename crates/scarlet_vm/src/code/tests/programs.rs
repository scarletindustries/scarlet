//! Every function of the programs in the repository, for the tests of the
//! passes.

use scarlet_ir::core_ir::{FuncIdx, LoweredFn, Program};

use super::super::translate::{Draft, translate};
use super::super::{Source, Tables, literals};
use super::{SHAPES, program};

/// Give `check` the draft of every function of `program`, which says whether it
/// fits. One that does not is given again with its literals in pieces.
fn each_draft(program: &Program, what: &str, check: &mut impl FnMut(Draft, &str) -> bool) -> usize {
    let source = &Source::new(program).expect("a whole program");
    let mut checked = 0;
    let mut one = |f: &LoweredFn, this: Option<FuncIdx>| {
        let what = format!("{what}: {}.{}", f.module, f.name);
        let mut tables = Tables::default();
        let Ok(d) = translate(source, &f.core, f.undropped, this, &mut tables) else {
            return;
        };
        if !check(d, &what) {
            let pieces = literals::in_pieces(&f.core);
            let core = pieces.unwrap_or_else(|| panic!("{what} does not fit"));
            let d = translate(source, &core, f.undropped, this, &mut tables);
            assert!(check(d.expect("it loads"), &what), "{what} does not fit");
        }
        checked += 1;
    };
    for (f, i) in (&program.fns).into_iter().zip(0..) {
        one(f, Some(FuncIdx(i)));
    }
    for f in program.inits.iter().chain([&program.toplevel]) {
        one(f, None);
    }
    checked
}

/// Give `check` the draft of every function of every program in the repository
/// that is one file, and of the stdlib modules they import.
pub(in super::super) fn every_draft(check: &mut impl FnMut(Draft, &str) -> bool) {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    for dir in ["examples", "crates/scarlet/tests/programs"] {
        let all = std::fs::read_dir(root.join(dir)).expect("a directory");
        files.extend(all.map(|f| f.expect("a file").path()));
    }
    let benches = std::fs::read_dir(root.join("benches")).expect("a directory");
    for dir in benches.map(|f| f.expect("a file").path()) {
        if let Some(name) = dir.file_name() {
            files.push(dir.join(name).with_extension("sca"));
        }
    }
    let (mut programs, mut functions) = (0, 0);
    for path in files {
        if path.extension().is_none_or(|e| e != "sca") {
            continue;
        }
        let Ok(src) = std::fs::read_to_string(&path) else {
            continue;
        };
        let result = scarlet_core::compile::compile_text(&src, None);
        let Some(program) = result.into_runnable() else {
            continue;
        };
        programs += 1;
        functions += each_draft(&program, &path.display().to_string(), check);
    }
    assert!(programs > 60, "{programs} programs");
    assert!(functions > 3000, "{functions} functions");
    assert!(each_draft(&program(SHAPES), "shapes", check) > 4);
}
