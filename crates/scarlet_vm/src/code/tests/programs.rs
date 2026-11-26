//! Every function of the programs in the repository, for the tests of the
//! passes, and a test of the functions themselves.

use scarlet_ir::core_ir::{CoreFn, FuncIdx, LoweredFn, Program};

use super::super::translate::{Draft, translate};
use super::super::{Source, Tables, literals};
use super::{SHAPES, program};
use ownership::Body;

/// The checker that `scarlet_lower`'s tests have.
#[path = "../../../../scarlet_lower/src/perceus/tests/ownership.rs"]
mod ownership;

/// Give `check` the draft of every function of `program`, which says whether it
/// fits. One that does not is given again with its literals in pieces.
fn each_draft(program: &Program, what: &str, check: &mut impl FnMut(Draft, &str) -> bool) -> usize {
    let source = &Source::new(program).expect("a whole program");
    let mut checked = 0;
    let mut one = |f: &LoweredFn, this: Option<FuncIdx>| {
        let what = format!("{what}: {}.{}", f.module, f.name);
        let mut tables = Tables::default();
        let Ok(d) = translate(source, &f.core, this, &mut tables) else {
            return;
        };
        if !check(d, &what) {
            let pieces = literals::in_pieces(&f.core);
            let core = pieces.unwrap_or_else(|| panic!("{what} does not fit"));
            let d = translate(source, &core, this, &mut tables);
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

/// Give `each` every program in the repository that is one file, with its name,
/// and the one of [`SHAPES`].
fn every_program(each: &mut impl FnMut(&Program, &str)) {
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
    let mut programs = 0;
    for path in files {
        if path.extension().is_none_or(|e| e != "sca") {
            continue;
        }
        let Ok(src) = std::fs::read_to_string(&path) else {
            continue;
        };
        let result = scarlet_core::compile::compile_text(&src, None);
        if let Some(program) = result.into_runnable() {
            programs += 1;
            each(&program, &path.display().to_string());
        }
    }
    assert!(programs > 60, "{programs} programs");
    each(&program(SHAPES), "shapes");
}

/// Give `check` the draft of every function of every program in the repository
/// that is one file, and of the stdlib modules they import.
pub(in super::super) fn every_draft(check: &mut impl FnMut(Draft, &str) -> bool) {
    let mut functions = 0;
    every_program(&mut |program, what| functions += each_draft(program, what, check));
    assert!(functions > 3000, "{functions} functions");
}

/// Every function that the compiler makes gives up each value it owns once, on
/// every way through it, and every jump goes to a join around it with what that
/// takes. The same of its literals in pieces.
#[test]
fn every_program_gives_up_each_value_once() {
    let (mut functions, mut joins, mut cut) = (0, 0, 0);
    every_program(&mut |program, what| {
        let pool = &program.pool;
        let fields = |ty| {
            let names = program.types.get(&ty);
            let variants = names.map_or(&[][..], |names| &names.variants);
            variants.iter().map(|v| v.fields.len()).collect()
        };
        let check = |f: &CoreFn, body: Body, what: &str| {
            ownership::check(pool, &fields, f, body)
                .unwrap_or_else(|why| panic!("{what}: {why}\n{f}"));
        };
        let tops = program.inits.iter().chain([&program.toplevel]);
        let tops = tops.map(|f| (f, Body::Toplevel));
        let all = (&program.fns).into_iter().map(|f| (f, Body::Function));
        for (f, body) in all.chain(tops) {
            let what = format!("{what}: {}.{}", f.module, f.name);
            check(&f.core, body, &what);
            functions += 1;
            joins += f.core.to_string().matches("join j").count();
            if let Some(pieces) = literals::in_pieces(&f.core) {
                check(&pieces, body, &format!("{what} in pieces"));
                cut += 1;
            }
        }
    });
    assert!(
        functions > 3000 && joins > 1000 && cut > 0,
        "{functions} {joins} {cut}"
    );
}
