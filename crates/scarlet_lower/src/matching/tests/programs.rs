//! Every match in the repository's programs, and in the modules of the standard library
//! that they import.

use std::collections::BTreeSet;
use std::path::PathBuf;

use scarlet_core::build::Analysis;
use scarlet_core::compile::{Compiled, compile_text};
use scarlet_ir::core_ir::FuncIdx;
use scarlet_types::def::TypeDef;
use scarlet_types::name::TypeName;

use super::oracle::{BODIES, Dice, SIZES};
use super::{Case, agrees, usual_and_none};
use crate::program::{TypeDefs, typed};
use crate::typed_ir::{
    BindingId, RTy, TypedArm, TypedBinPatSeg, TypedBind, TypedCallee, TypedExpr, TypedFn, TypedPat,
    ValueRef, each_expr, each_read, pat_binds,
};

struct Defs<'a>(&'a Analysis);

impl TypeDefs for Defs<'_> {
    fn type_def(&self, name: &TypeName) -> Option<&TypeDef> {
        let module = self.0.module(name.module())?;
        module.checked.interface.types.get(name.name())
    }
}

/// The programs that are one file.
fn files() -> Vec<PathBuf> {
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
    files.retain(|f| f.extension().is_some_and(|e| e == "sca") && f.exists());
    files.sort();
    files
}

/// A match by itself: a function of the matched value, with a call in place of each
/// guard, body and size, which is given the names that were read there.
fn by_itself(of: &TypedFn, scrut: &TypedExpr, arms: &[TypedArm], int: RTy, bool: RTy) -> TypedFn {
    let param = TypedBind {
        id: BindingId(of.binds),
        named: true,
        ty: scrut.ty(),
        global: None,
    };
    let mut sizes = 0;
    let mut own = Vec::with_capacity(arms.len());
    for (i, arm) in (0u32..).zip(arms) {
        let mut pat = arm.pat.clone();
        let names = pat_binds(&arm.pat);
        each_size(&mut pat, &mut |size| {
            if !matches!(size, TypedExpr::Const { .. }) {
                sizes += 1;
                *size = call(SIZES + sizes, size, &names, int);
            }
        });
        own.push(TypedArm {
            pat,
            guard: arm.guard.as_ref().map(|g| call(i, g, &names, bool)),
            body: call(BODIES + i, &arm.body, &names, int),
        });
    }
    let value = TypedExpr::Var {
        ty: param.ty,
        place: ValueRef::Local(param.id),
    };
    TypedFn {
        params: vec![param],
        ret: int,
        body: TypedExpr::Match {
            ty: int,
            scrut: Box::new(value),
            arms: own,
        },
        binds: of.binds + 1,
    }
}

/// A call of `f` with those of `names` that `e` reads.
fn call(f: u32, e: &TypedExpr, names: &[TypedBind], ty: RTy) -> TypedExpr {
    let mut read = BTreeSet::new();
    each_read(e, |b| {
        read.insert(b);
    });
    let args = names.iter().filter(|b| read.contains(&b.id));
    let args = args.map(|b| TypedExpr::Var {
        ty: b.ty,
        place: ValueRef::Local(b.id),
    });
    TypedExpr::Call {
        ty,
        callee: TypedCallee::Known(FuncIdx(f)),
        args: args.collect(),
    }
}

/// Call `f` with each size that a segment in `p` gives.
fn each_size(p: &mut TypedPat, f: &mut impl FnMut(&mut TypedExpr)) {
    match p {
        TypedPat::Wild { .. }
        | TypedPat::Bind(_)
        | TypedPat::Lit { .. }
        | TypedPat::Range { .. } => {}
        TypedPat::Ctor { fields: ps, .. }
        | TypedPat::Tuple { elems: ps, .. }
        | TypedPat::Array { prefix: ps, .. }
        | TypedPat::Or { alts: ps, .. } => ps.iter_mut().for_each(|p| each_size(p, f)),
        TypedPat::Bin { segs, .. } => {
            for seg in segs {
                match seg {
                    TypedBinPatSeg::Utf8Literal { .. } => {}
                    TypedBinPatSeg::Utf8 { value } => each_size(value, f),
                    TypedBinPatSeg::Int { bits, value } => {
                        f(bits);
                        each_size(value, f);
                    }
                    TypedBinPatSeg::Binary { bits, value } => {
                        bits.iter_mut().for_each(&mut *f);
                        each_size(value, f);
                    }
                }
            }
        }
    }
}

#[test]
fn every_match_in_the_repository_finds_what_its_rows_say() {
    let (mut seen, mut matches, mut found) = (BTreeSet::new(), 0usize, 0usize);
    for file in files() {
        let src = std::fs::read_to_string(&file).expect("a file");
        let Compiled::Runnable { analysis, .. } = compile_text(&src, None) else {
            continue;
        };
        let clean = analysis.modules().map(|m| m.checked.clean.as_ref());
        let clean: Vec<_> = clean.map(|m| m.expect("it is clean").module()).collect();
        let defs = Defs(&analysis);
        let typed = typed(&clean, &defs, None).expect("it is translated");
        let shapes = typed.shapes();
        let around = typed.around(&shapes);
        let temps = around.temps;
        let all = typed
            .fns
            .iter()
            .map(|f| (f.module.as_str(), f.name.as_str(), &f.body));
        let tops = typed.tops.iter().map(|f| ("main", "", f));
        for (n, (module, name, f)) in all.chain(tops).enumerate() {
            // One of the standard library is the same in every program.
            let own = module == "main";
            if !own && !seen.insert(format!("{module}.{name}.{:?}", f.binds)) {
                continue;
            }
            each_expr(&f.body, |e| {
                let TypedExpr::Match { scrut, arms, .. } = e else {
                    return;
                };
                let f = by_itself(f, scrut, arms, temps.int, temps.bool);
                let case = Case {
                    f: &f,
                    pool: typed.pool(),
                    shapes: &shapes,
                    consts: around.consts,
                    temps,
                    im: typed.immediates(),
                };
                let what = format!("{}: {module}.{name}", file.display());
                let mut dice = Dice(u64::try_from(n).expect("a few") << 20);
                for allowance in usual_and_none() {
                    found += agrees(&mut dice, &case, allowance, 20, &what).len();
                }
                matches += 1;
            });
        }
    }
    assert!(matches > 700 && found > 3500, "{matches} {found}");
}
