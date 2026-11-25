use super::ins::{Label, MaybeDst, VReg};
use super::translate::{Draft, Step, TailTo};
use super::*;
use crate::heap::{Ctor, Numbering};
use scarlet_ir::core_ir::VariantRef;

pub(super) mod programs;
pub(super) mod random;

/// A program with calls, a loop by tail call, a `match` with several
/// arms, an `if` and a map lookup that jumps, so its functions have
/// blocks of every shape.
pub(super) const SHAPES: &str = "import scarlet/map\n\
        type L {\n\tCons(h Int, t L)\n\tEnd\n}\n\
        fn build(n Int, acc L) L {\n\
        \tif n == 0 { acc } else { build(n - 1, Cons(n, acc)) }\n\
        }\n\
        fn sum(l L, acc Int) Int {\n\
        \tmatch l {\n\
        \t\tCons(h, t) -> sum(t, acc + h)\n\
        \t\tEnd -> acc\n\
        \t}\n\
        }\n\
        fn name(n Int) String {\n\
        \tmatch n {\n\
        \t\t0 -> 'none'\n\
        \t\t1 -> 'one'\n\
        \t\t_ -> if n < 0 { 'fewer' } else { 'more' }\n\
        \t}\n\
        }\n\
        fn found(m map::Map(Int, Int), k Int) Int {\n\
        \tmatch map::get(m, k) {\n\
        \t\tSome(v) -> v\n\
        \t\tNone -> 0\n\
        \t}\n\
        }\n\
        pub fn main() {\n\
        \tprintln(sum(build(10, End), 0))\n\
        \tprintln(name(3))\n\
        \tprintln(found(map::set(map::new(), 1, 2), 1))\n\
        }\n";

pub(super) fn program(src: &str) -> Program {
    let result = scarlet_core::compile::compile_text(src, None);
    assert!(result.success(), "{:?}", result.diagnostics());
    result.into_runnable().expect("a clean compile is runnable")
}

/// A constructor of one field, for an instruction to name.
pub(super) fn a_ctor() -> Ctor {
    let v = VariantRef {
        type_id: scarlet_ir::TypeId(1),
        variant_idx: 5,
    };
    Numbering::default().add(v, 1).expect("the first one")
}

/// Every function and toplevel of `code`, in the order their code is.
pub(super) fn entries(code: &Code) -> Vec<&Entry> {
    (&code.funcs).into_iter().chain(&code.toplevels).collect()
}

/// The instructions of `entry`.
pub(super) fn body<'c>(code: &'c Code, entry: &Entry) -> &'c [Ins<Reg>] {
    &code.ins[entry.pc.index()..entry.end.index()]
}

/// The function of `code` whose name ends in `name`.
pub(super) fn named<'c>(code: &'c Code, name: &str) -> &'c Entry {
    let mut all = entries(code).into_iter();
    all.find(|e| e.name.ends_with(name))
        .unwrap_or_else(|| panic!("no function called {name}"))
}

impl From<Ins<VReg>> for Step {
    fn from(ins: Ins<VReg>) -> Step {
        Step::Ins(ins)
    }
}

/// A function's steps before its values have registers, for a test that writes
/// them out. `labels` are where each label stands.
pub(super) fn draft(
    values: u32,
    params: u32,
    steps: impl IntoIterator<Item: Into<Step>>,
    labels: &[usize],
) -> Draft {
    Draft {
        steps: steps.into_iter().map(Into::into).collect(),
        lists: Lists::default(),
        labels: labels.to_vec(),
        values,
        params: (0..params).map(VReg).collect(),
        prims: Vec::new(),
        undropped: false,
        closure: ReadsClosure::Nothing,
    }
}

/// The instructions of a draft that makes no tail call.
pub(super) fn instructions(d: &Draft) -> Vec<Ins<VReg>> {
    let ins = |step: &Step| match step {
        Step::Ins(ins) => *ins,
        Step::TailCall { .. } => panic!("{step:?}"),
    };
    d.steps.iter().map(ins).collect()
}

pub(super) fn same_step(step: &Step) -> Step {
    match step {
        Step::Ins(ins) => Step::Ins(*ins),
        Step::TailCall { to, args } => Step::TailCall {
            to: *to,
            args: args.clone(),
        },
    }
}

pub(super) fn copy_of(d: &Draft) -> Draft {
    Draft {
        steps: d.steps.iter().map(same_step).collect(),
        lists: Lists {
            regs: d.lists.regs.clone(),
            args: d.lists.args.clone(),
            dsts: d.lists.dsts.clone(),
        },
        labels: d.labels.clone(),
        values: d.values,
        params: d.params.clone(),
        prims: d.prims.clone(),
        undropped: d.undropped,
        closure: d.closure,
    }
}

/// The instructions a draft is written out with.
pub(super) mod of {
    use super::super::ins::{LastReg, OwnRoom};
    use super::*;
    use crate::value::Value;

    pub(crate) fn constant(dst: u32) -> Ins<VReg> {
        Ins::Const {
            dst: Dst(VReg(dst)),
            value: Value::NIL,
        }
    }

    pub(crate) fn mv(dst: u32, src: u32) -> Ins<VReg> {
        Ins::Move {
            dst: Dst(VReg(dst)),
            src: Arg::shared(VReg(src)),
        }
    }

    pub(crate) fn ret(src: u32) -> Ins<VReg> {
        Ins::Ret {
            src: Taken(VReg(src)),
            last: LastReg,
        }
    }

    pub(crate) fn drop(reg: u32) -> Ins<VReg> {
        Ins::Drop {
            reg: Dead(VReg(reg)),
        }
    }

    /// Take a constructor of two fields in `src` apart, with a register for
    /// each that is to have one.
    pub(crate) fn open(dsts: [Option<u32>; 2], src: u32) -> Ins<VReg> {
        Ins::Open2 {
            dsts: dsts.map(|dst| MaybeDst::new(dst.map(VReg))),
            src: Taken(VReg(src)),
            ctor: a_ctor(),
        }
    }

    pub(crate) fn len(dst: u32, src: u32) -> Ins<VReg> {
        Ins::ArrayLen {
            dst: Dst(VReg(dst)),
            src: Src(VReg(src)),
        }
    }

    pub(crate) fn jump(to: u32) -> Ins<VReg> {
        Ins::Jump { to: Label(to) }
    }

    pub(crate) fn unless(cond: u32, to: u32) -> Ins<VReg> {
        Ins::JumpIfFalse {
            cond: Src(VReg(cond)),
            to: Label(to),
        }
    }

    pub(crate) fn add(dst: u32, a: u32, b: u32) -> Ins<VReg> {
        Ins::IntAdd {
            dst: Dst(VReg(dst)),
            a: Src(VReg(a)),
            b: Src(VReg(b)),
        }
    }

    pub(crate) fn append(dst: u32, rest: u32, item: u32) -> Ins<VReg> {
        Ins::ArrayAppend {
            dst: Dst(VReg(dst)),
            rest: Arg::shared(VReg(rest)),
            back: Shares::new(&[VReg(item)]).expect("room for one"),
        }
    }

    /// `dst` gets the array in `src` with `value` at `index`, or the run
    /// continues at `miss`.
    pub(crate) fn write(dst: u32, [src, index, value]: [u32; 3], miss: u32) -> Ins<VReg> {
        Ins::ArraySet {
            dst: Hit(VReg(dst)),
            src: Arg::shared(VReg(src)),
            index: Src(VReg(index)),
            value: Arg::shared(VReg(value)),
            miss: Label(miss),
        }
    }

    pub(crate) fn find(dst: u32, [map, key]: [u32; 2], to: u32) -> Ins<VReg> {
        Ins::MapFind {
            dst: Hit(VReg(dst)),
            map: Src(VReg(map)),
            key: Src(VReg(key)),
            to: Label(to),
        }
    }

    /// A tail call of the function to itself.
    pub(crate) fn again(args: &[u32]) -> Step {
        let args = args.iter().copied().map(VReg).collect();
        let to = TailTo::Own;
        Step::TailCall { to, args }
    }

    /// A tail call of function `f`.
    pub(crate) fn enter(f: u32, args: &[u32]) -> Step {
        let args = args.iter().copied().map(VReg).collect();
        let to = TailTo::Func(FuncIdx(f));
        Step::TailCall { to, args }
    }

    pub(crate) fn call(dst: u32, args: &[u32]) -> Ins<VReg> {
        let args: Vec<VReg> = args.iter().copied().map(VReg).collect();
        Ins::Call {
            dst: Dst(VReg(dst)),
            to: super::super::ins::Callee(FuncIdx(0)),
            args: Args::new(&args).expect("a few arguments"),
            own: OwnRoom,
        }
    }
}

/// A function that needs something the VM does not run, or that is not one the
/// compiler should have made, is one instruction that says so, and the others
/// load as they do.
#[test]
fn a_function_that_does_not_load_is_one_instruction() {
    let src = "import scarlet/internal\n\
               fn later(a Int, b Int) Int { internal::live_subjects() + a + b }\n\
               pub fn main() { println(1) }\n";
    let code = load(&program(src), Audit::Never).expect("the program loads");
    let later = named(&code, ".later");
    let [Ins::Bad { why }] = body(&code, later) else {
        panic!("one instruction: {:?}", body(&code, later));
    };
    let missing = Missing::Intrinsic(Intrinsic::InternalLiveSubjects);
    assert_eq!(code.why(*why), Stop::missing(missing));
    // Its arguments still arrive.
    assert_eq!((later.params, later.own), (2, Room(4)));
    let main = named(&code, "main.main");
    assert!(body(&code, main).len() > 1);
    let lost = Stop::broken(Broken::NoPlace);
    assert_eq!(code.why(WhyId(99)), lost);
    assert_eq!(code.text(TextId(99)), Err(Stop::broken(Broken::NoPlace)));
    assert_eq!(code.binary(BinaryId(99)).err(), Some(lost));
}

/// A function that is taken through a second time leaves nothing of its first
/// try in the tables.
#[test]
fn a_string_is_in_the_table_once() {
    let texts: Vec<String> = (0..300).map(|i| format!("'s{i}'")).collect();
    let src = format!(
        "fn f() Array(String) {{ [{}] }}\npub fn main() {{ println(f()) }}\n",
        texts.join(", ")
    );
    let code = load(&program(&src), Audit::Never).expect("the program loads");
    let wide = body(&code, named(&code, ".f"))
        .iter()
        .filter(|i| i.op() == Op::ArrayAppendWide);
    assert_eq!(wide.count(), 9, "the literal was made in pieces");
    for n in [0, 150, 299] {
        let name = format!("s{n}");
        let same = code.texts.iter().filter(|t| ***t == *name.as_bytes());
        assert_eq!(same.count(), 1, "{name}");
    }
}

/// The program's `main` is run as no closure, so one that reads captures is
/// refused before anything runs. One that only reads itself is not.
#[test]
fn a_main_that_reads_captures_is_refused() {
    let src = "fn through(f fn(Int) Int, n Int) Int { if n > 0 { f(n - 1) } else { n } }\n\
               pub fn main() {\n\
               \tkept = 7\n\
               \titself = fn(n) { through(itself, n) }\n\
               \tprintln(through(fn(n) n + kept, itself(3)))\n\
               }\n";
    let mut program = program(src);
    let code = load(&program, Audit::Never).expect("the program loads");
    let mut seen = Vec::new();
    for (func, entry) in (0..).map(FuncIdx).zip(&code.funcs) {
        program.main = Some(func);
        let stop = load(&program, Audit::Never).err();
        let func = entry.name.clone();
        let refused = Stop::broken(Broken::CalledByName { func });
        let reads = entry.closure == ReadsClosure::Captures;
        assert_eq!(stop, reads.then_some(refused), "{}", entry.name);
        seen.push(entry.closure);
    }
    seen.sort();
    let (nothing, itself) = (ReadsClosure::Nothing, ReadsClosure::Itself);
    assert_eq!(seen, [nothing, nothing, itself, ReadsClosure::Captures]);
}

/// Every instruction but the first is some function's, and knows which.
#[test]
fn every_instruction_is_in_one_function() {
    let code = load(&program(SHAPES), Audit::Never).expect("the program loads");
    assert!(code.function_at(0).is_none());
    let mut next = 1;
    for entry in entries(&code) {
        assert_eq!(entry.pc.index(), next, "{}", entry.name);
        assert!(entry.end.index() > next, "{}", entry.name);
        for pc in next..entry.end.index() {
            let found = code.function_at(pc).expect("a function");
            assert_eq!(found.name, entry.name);
        }
        next = entry.end.index();
    }
    assert_eq!(next, code.ins.len());
    assert!(code.function_at(next).is_none());
}

/// A function never runs on into the one after it.
#[test]
fn every_function_ends_where_nothing_follows() {
    let code = load(&program(SHAPES), Audit::Never).expect("the program loads");
    for entry in entries(&code) {
        let last = body(&code, entry).last().expect("an instruction");
        assert!(!last.falls_through(), "{} ends in {last:?}", entry.name);
    }
}

/// The compiler refuses a function of too many parameters. If one gets here
/// all the same, it waits for what it needs, and so does a call that passes
/// as many. One of as many as there may be loads.
#[test]
fn a_function_takes_at_most_250_parameters() {
    let param = |i| format!("{}p{i} Int", if i == 0 || i == 249 { "" } else { "_" });
    let params: Vec<String> = (0..250).map(param).collect();
    let args: Vec<String> = (0..250).map(|i| format!("{}", i % 7)).collect();
    let src = format!(
        "fn many({}) Int {{ p0 + p249 }}\n\
         fn calls() Int {{ many({}) }}\n\
         pub fn main() {{ println(calls()) }}\n",
        params.join(", "),
        args.join(", "),
    );
    let why = |code: &Code, name: &str| match body(code, named(code, name)) {
        [.., Ins::Bad { why }] => Some(code.why(*why)),
        _ => None,
    };
    let mut program = program(&src);
    let code = load(&program, Audit::Never).expect("the program loads");
    assert_eq!(why(&code, ".many"), None);
    assert_eq!(why(&code, ".calls"), None);
    assert_eq!(named(&code, ".many").params, 250);

    let (many, calls) = (FuncIdx(0), FuncIdx(1));
    let f = program.fns.get_mut(many).expect("many");
    let mut last = f.core.params[249].clone();
    last.id = scarlet_ir::core_ir::LocalId(10_000);
    f.core.params.push(last);
    let code = load(&program, Audit::Never).expect("the program loads");
    let stops = |missing| Some(Stop::missing(missing));
    assert_eq!(
        why(&code, ".many"),
        stops(Missing::TooMany {
            func: "main.many".into(),
            of: Limit::Parameters
        })
    );
    // The call passes too few now.
    assert!(
        body(&code, named(&code, ".calls"))
            .iter()
            .any(|i| i.op() == Op::Bad)
    );

    let mut e = &mut program.fns.get_mut(calls).expect("calls").core.body;
    while let scarlet_ir::core_ir::CoreExpr::Let { body, .. } = e {
        e = body;
    }
    let scarlet_ir::core_ir::CoreExpr::Tail(scarlet_ir::core_ir::Atom::Call { args, .. }) = e
    else {
        panic!("a call: {e}");
    };
    args.push(args[0]);
    let code = load(&program, Audit::Never).expect("the program loads");
    assert_eq!(
        why(&code, ".calls"),
        stops(Missing::TooMany {
            func: "main.calls".into(),
            of: Limit::Arguments
        })
    );
}

/// A constructor the VM builds on its own has to be one the program lists, with
/// as many fields as the VM gives it, and the program's constructors have to
/// fit in a header. A program that breaks either does not load at all.
#[test]
fn a_program_whose_constructors_are_not_as_the_vm_needs_does_not_load() {
    let whole = program("pub fn main() { Nil }\n");
    assert!(load(&whole, Audit::Never).is_ok());
    let stop = |change: fn(&mut Program)| {
        let mut broken = whole.clone();
        change(&mut broken);
        load(&broken, Audit::Never).err()
    };
    let abi = whole.abi;
    let unlisted = stop(|p| {
        let option = p.abi.some.type_id;
        p.types.remove(&option);
    });
    assert_eq!(unlisted, Some(Stop::broken(Broken::NoCtor(abi.some))));
    let with_fields = |v: VariantRef, want: usize| {
        let mut other = whole.clone();
        let of = other.types.get_mut(&v.type_id).expect("it is listed");
        let labels = vec!["f".to_string(); want];
        of.variants[usize::from(v.variant_idx)].fields = labels;
        load(&other, Audit::Never).err()
    };
    let wrong = |ctor, got, want| Some(Stop::broken(Broken::CtorFields { ctor, got, want }));
    for ctor in [abi.some, abi.ok, abi.err] {
        assert_eq!(with_fields(ctor, 1), None);
        for want in [0, 2, 3] {
            assert_eq!(with_fields(ctor, want), wrong(ctor, 1, want));
        }
    }
    assert_eq!(with_fields(abi.none, 1), wrong(abi.none, 0, 1));
    let too_many = stop(|p| {
        let many = p.types.get_mut(&p.abi.err.type_id).expect("Result");
        let one = many.variants[usize::from(p.abi.err.variant_idx)].clone();
        many.variants.resize(65_520, one);
    });
    assert_eq!(too_many, Some(Stop::TooManyConstructors));
}

/// The stdlib's types have the constructors the VM builds and reads, each with
/// as many fields as `Abi` says. A program that loads the modules of all of
/// them would not load if one did not.
#[test]
fn the_stdlibs_constructors_are_as_the_vm_needs() {
    let modules = [
        "binary",
        "io",
        "json",
        "http/h1",
        "http/headers",
        "net/address",
        "net/error",
        "net/socket",
        "net/tls",
    ];
    let imports: String = modules
        .iter()
        .map(|m| format!("import scarlet/{m}\n"))
        .collect();
    let program = program(&format!("{imports}pub fn main() {{ Nil }}\n"));
    let code = load(&program, Audit::Never).expect("it loads");
    let abi = code.abi;
    let there = [
        abi.radix.is_some(),
        abi.io.is_some(),
        abi.json.is_some(),
        abi.http.is_some(),
        abi.address.is_some(),
        abi.net.is_some(),
        abi.tls.is_some(),
    ];
    assert_eq!(there, [true; 7]);
}
