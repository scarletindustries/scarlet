use super::super::ins::{Ins, Role};
use super::super::tests::{body, entries, instructions, named, program};
use super::super::{Code, Op, Reg, load};
use super::*;
use crate::Audit;
use scarlet_ir::TypeId;
use scarlet_ir::core_ir::{CoreBind, LoweredFn, Program, TypeNames, VariantNames, VariantRef};
use scarlet_ir::rty::RTy;

fn code(src: &str) -> Code {
    load(&program(src), Audit::Never).expect("the program loads")
}

fn ops(code: &Code, name: &str) -> Vec<Op> {
    body(code, named(code, name)).iter().map(Ins::op).collect()
}

/// Whether each `Move` in `ins` takes its source, in order.
fn moves(ins: &[Ins<Reg>]) -> Vec<bool> {
    let moved = |i: &Ins<Reg>| match i {
        Ins::Move { src, .. } => Some(src.moved),
        _ => None,
    };
    ins.iter().filter_map(moved).collect()
}

/// `array::put` and `array::set` load as one write each. `put` gives
/// the array back on a miss, and `set` builds `Ok` or `Err(Nil)`. The
/// write takes an array that is not read again, and shares one that is.
#[test]
fn the_loader_makes_one_write_of_a_put_and_of_a_set() {
    let code = code(
        "import scarlet/array\n\
         fn last_use(xs Array(Int), i Int) Array(Int) {\n\
         \tarray::put(xs, i, 7)\n\
         }\n\
         fn read_again(xs Array(Int), i Int) Array(Int) {\n\
         \tys = array::put(xs, i, 7)\n\
         \t[..ys, ..xs]\n\
         }\n\
         fn checked(xs Array(Int), i Int) Result(Array(Int), Nil) {\n\
         \tarray::set(xs, i, 7)\n\
         }\n\
         pub fn main() {\n\
         \tprintln(last_use([1], 0))\n\
         \tprintln(read_again([1], 0))\n\
         \tprintln(checked([1], 0))\n\
         }\n",
    );
    let taken = |name: &str| -> Vec<bool> {
        let taken = |i: &Ins<Reg>| match i {
            Ins::ArraySet { src, .. } => Some(src.moved),
            _ => None,
        };
        let ins = body(&code, named(&code, name));
        ins.iter().filter_map(taken).collect()
    };
    assert_eq!(
        ops(&code, ".last_use"),
        [Op::Const, Op::ArraySet, Op::Jump, Op::Move, Op::Ret]
    );
    assert_eq!(taken(".last_use"), [true]);
    // The copy a miss makes takes the array too.
    assert_eq!(moves(body(&code, named(&code, ".last_use"))), [true]);
    assert_eq!(taken(".read_again"), [false]);
    assert_eq!(
        ops(&code, ".checked"),
        [
            Op::Const,
            Op::ArraySet,
            Op::Ctor1,
            Op::Jump,
            Op::Const,
            Op::Ctor1,
            Op::Ret
        ]
    );
    assert_eq!(taken(".checked"), [true]);
}

/// A value copied to a new name and never read again under the old one
/// is taken: at the end of a branch, in a binding, and in a pattern that
/// binds the whole value. One that is read again is shared. A copy from a
/// register to itself is no instruction.
#[test]
fn the_loader_marks_the_moves_of_a_compiled_program() {
    let code = code(
        "fn fresh(n Int) Array(Int) { [n, n + 1] }\n\
         fn joined(c Bool, n Int) Array(Int) {\n\
         \tys = if c {\n\
         \t\tt = fresh(n)\n\
         \t\tprintln(n)\n\
         \t\tt\n\
         \t} else {\n\
         \t\tfresh(0)\n\
         \t}\n\
         \t[..ys, 9]\n\
         }\n\
         fn bound(n Int) Array(Int) {\n\
         \tmatch fresh(n) {\n\
         \t\tys -> [..ys, 9]\n\
         \t}\n\
         }\n\
         fn kept(n Int) Array(Int) {\n\
         \tt = fresh(n)\n\
         \tys = t\n\
         \t[..ys, ..t]\n\
         }\n\
         pub fn main() {\n\
         \tprintln(joined(True, 1))\n\
         \tprintln(bound(1))\n\
         \tprintln(kept(1))\n\
         }\n",
    );
    let moves = |name: &str| moves(body(&code, named(&code, name)));
    // `println`'s copy of its argument and the branch's own are each from a
    // register to itself, or take their source.
    assert!(
        moves(".joined").iter().all(|m| *m),
        "{:?}",
        moves(".joined")
    );
    // Here the copy is dropped after, and takes the register over.
    assert_eq!(moves(".bound"), [false; 0]);
    assert_eq!(moves(".kept"), [false]);
}

/// The kinds of the instructions each function of `src` is loaded as, by the
/// function's name.
fn loaded(src: &str) -> HashMap<String, Vec<Op>> {
    let code = code(src);
    entries(&code)
        .into_iter()
        .map(|e| {
            let name = e.name.rsplit('.').next().unwrap_or_default().to_string();
            (name, body(&code, e).iter().map(Ins::op).collect())
        })
        .collect()
}

/// An operator's instruction holds a constant on whichever side there is an
/// instruction for. That is in tail position too, and when other work comes
/// between the constant and the operator.
#[test]
fn an_operator_holds_a_constant_it_can() {
    let ops = loaded(
        "import scarlet/float\n\
         fn right(x Int) Int { x - 1 }\n\
         fn left(x Int) Int { 1 + x }\n\
         fn from(x Int) Int { 100 - x }\n\
         fn over(x Int) Int { 100 / x }\n\
         fn past(x Int) Int { x + 1125899906842624 }\n\
         fn half(x Float) Float { x / 2.0 }\n\
         fn inverse(x Float) Float { 1.0 / x }\n\
         fn less(x Float) Float { 1.5 - x }\n\
         fn rest(x Float) Float { x % 2.0 }\n\
         fn apart(n Int) Float { 2.0 * float::from_int(n) }\n\
         fn both(x Int) Int { { 2 - x } * { 3 - x } + 5 }\n\
         fn stored(x Int) Bool { 3 < x }\n\
         fn stored_float(x Float) Bool { 3.5 >= x }\n\
         fn branch(c Bool, x Int) Int {\n\
         \ty = if c { x * 3 } else { x }\n\
         \ty - x\n\
         }\n\
         pub fn main() { Nil }\n",
    );
    let is = |name: &str, want: &[Op]| assert_eq!(ops[name], want, "{name}");
    is("right", &[Op::IntSubK, Op::Ret]);
    is("left", &[Op::IntAddK, Op::Ret]);
    is("from", &[Op::IntRSubK, Op::Ret]);
    // No instruction divides a constant, and none holds an Int that is not
    // small.
    is("over", &[Op::Const, Op::IntDiv, Op::Ret]);
    is("past", &[Op::BigInt, Op::IntAdd, Op::Ret]);
    is("half", &[Op::FloatDivK, Op::Ret]);
    is("inverse", &[Op::FloatRDivK, Op::Ret]);
    is("less", &[Op::FloatRSubK, Op::Ret]);
    is("rest", &[Op::Const, Op::FloatRem, Op::Ret]);
    is("apart", &[Op::FloatFromInt, Op::FloatMulK, Op::Ret]);
    is(
        "both",
        &[Op::IntRSubK, Op::IntRSubK, Op::IntMul, Op::IntAddK, Op::Ret],
    );
    is("stored", &[Op::IntGtK, Op::Ret]);
    is("stored_float", &[Op::FloatLeK, Op::Ret]);
    assert_eq!(ops["branch"][..2], [Op::JumpIfFalse, Op::IntMulK]);
}

/// A constant that something reads from a register is still put in one, and
/// an operator beside it holds it all the same.
#[test]
fn a_constant_something_reads_is_put_in_a_register() {
    let ops = loaded(
        "const kept = 7\n\
         fn pair(x Int) (Int, Int) {\n\
         \tk = 7\n\
         \t(k, x + k)\n\
         }\n\
         fn twice(x Int) Int {\n\
         \tk = 7\n\
         \tx * k + k\n\
         }\n\
         fn alone() Int {\n\
         \tk = 7\n\
         \tk * k\n\
         }\n\
         fn global(x Int) Int { x + kept }\n\
         pub fn main() { Nil }\n",
    );
    assert_eq!(ops["pair"], [Op::Const, Op::IntAddK, Op::Tuple2, Op::Ret]);
    assert_eq!(ops["twice"], [Op::IntMulK, Op::IntAddK, Op::Ret]);
    assert_eq!(ops["alone"], [Op::Const, Op::IntMulK, Op::Ret]);
    assert_eq!(ops["global"], [Op::GetGlobal, Op::IntAdd, Op::Ret]);
}

/// A constant that nothing reads is no instruction, even one no operator
/// could hold.
#[test]
fn a_constant_nothing_reads_is_not_loaded() {
    let ops = loaded(
        "fn unread() Int {\n\
         \t_a = 3\n\
         \t_c = 2.5\n\
         \t4\n\
         }\n\
         pub fn main() { Nil }\n",
    );
    assert_eq!(ops["unread"], [Op::Const, Op::Ret]);
}

fn bind(id: u32) -> CoreBind {
    CoreBind {
        id: LocalId(id),
        ty: RTy(0),
        global: None,
    }
}

fn function(params: u32, body: CoreExpr) -> CoreFn {
    CoreFn {
        params: (0..params).map(bind).collect(),
        body,
        ret_ty: RTy(0),
    }
}

fn empty_program() -> Program {
    program("pub fn main() { Nil }\n")
}

/// The type of [`program_with_fields`] that its other types cannot be.
const WITH_FIELDS: TypeId = TypeId(-1);

/// [`empty_program`], with a type whose variant `n` has `n` fields, up to ten.
fn program_with_fields() -> Program {
    let mut program = empty_program();
    let variants = (0..=10)
        .map(|n| VariantNames {
            name: format!("With{n}"),
            fields: (0..n).map(|i| format!("f{i}")).collect(),
        })
        .collect();
    let name = "WithFields".to_string();
    program
        .types
        .insert(WITH_FIELDS, TypeNames { name, variants });
    program
}

/// The variant of [`WITH_FIELDS`] that has `n` fields.
fn with_fields(n: usize) -> VariantRef {
    VariantRef {
        type_id: WITH_FIELDS,
        variant_idx: u16::try_from(n).expect("a few fields"),
    }
}

/// A constructor or a pattern with another number of fields than the program
/// lists, or of a constructor it does not list, is in a function that does not
/// load. One it lists is told by the top of its word.
#[test]
fn a_constructor_the_program_does_not_list_does_not_load() {
    let program = program_with_fields();
    let source = Source::new(&program).expect("a whole program");
    let load = |core: &CoreFn| {
        let draft = translate(
            &source,
            core,
            false,
            Some(FuncIdx(0)),
            &mut Tables::default(),
        );
        draft.map(|d| instructions(&d))
    };
    let builds = |variant, n: u32| {
        function(
            n,
            CoreExpr::Tail(Atom::Ctor {
                variant,
                fields: (0..n).map(LocalId).collect(),
            }),
        )
    };
    let matches = |variant, n: u32| {
        let pat = CorePat::Ctor {
            variant,
            fields: (1..=n).map(bind).collect(),
        };
        let arm = CoreExpr::Tail(Atom::Local(LocalId(0)));
        function(
            1,
            CoreExpr::Match {
                scrut: LocalId(0),
                arms: vec![(pat, arm)],
                ty: RTy(0),
            },
        )
    };
    let wrong = |ctor, got, want| Some(Unfit::Broken(Broken::CtorFields { ctor, got, want }));
    for (variant, got, want) in [(2, 1, 2), (2, 3, 2), (2, 0, 2), (0, 1, 0)] {
        let ctor = with_fields(variant);
        let core = builds(ctor, got);
        assert_eq!(load(&core).err(), wrong(ctor, got as usize, want));
        let core = matches(ctor, got);
        assert_eq!(load(&core).err(), wrong(ctor, got as usize, want));
    }
    let past = with_fields(11);
    let no_type = VariantRef {
        type_id: TypeId(-2),
        variant_idx: 0,
    };
    for ctor in [past, no_type] {
        let unlisted = || Some(Unfit::Broken(Broken::NoCtor(ctor)));
        assert_eq!(load(&builds(ctor, 1)).err(), unlisted());
        assert_eq!(load(&builds(ctor, 0)).err(), unlisted());
        assert_eq!(load(&matches(ctor, 1)).err(), unlisted());
    }
    let ins = load(&matches(with_fields(3), 3)).expect("it loads");
    let [
        Ins::JumpUnlessTag { tag, .. },
        Ins::Field { index: 0, .. },
        ..,
    ] = ins[..]
    else {
        panic!("{ins:?}");
    };
    assert_eq!(tag.parts(), (3, true));
    let ins = load(&matches(with_fields(0), 0)).expect("it loads");
    let [Ins::JumpUnlessTag { tag, .. }, ..] = ins[..] else {
        panic!("{ins:?}");
    };
    assert_eq!(tag.parts(), (0, false));
    let ins = load(&builds(with_fields(0), 0)).expect("it loads");
    let [Ins::Const { value, .. }, ..] = ins[..] else {
        panic!("{ins:?}");
    };
    assert_eq!(value.bits(), Value::nullary(with_fields(0)).bits());
    let ins = load(&builds(with_fields(4), 4)).expect("it loads");
    let [Ins::Ctor { ctor, .. }, ..] = ins[..] else {
        panic!("{ins:?}");
    };
    assert_eq!(source.ctors.name(ctor), Some(with_fields(4)));
}

/// `core` translated, as function 0 of a program with nothing in it but `main`.
fn translated(core: &CoreFn) -> Result<Vec<Op>, Unfit> {
    let program = empty_program();
    let source = Source::new(&program).expect("a whole program");
    let draft = translate(
        &source,
        core,
        false,
        Some(FuncIdx(0)),
        &mut Tables::default(),
    );
    draft.map(|d| instructions(&d).iter().map(Ins::op).collect())
}

/// A constant that a module keeps in a global is stored there, even when only
/// an operator that could hold it reads it. The compiler reads a global back
/// with a load of its own, so this makes the read by hand.
#[test]
fn a_constant_kept_in_a_global_is_stored() {
    let program = program("const kept = 7\npub fn main() { Nil }\n");
    let CoreExpr::Let { bind, rhs, .. } = &program.toplevel.core.body else {
        panic!("the toplevel binds the constant first");
    };
    assert!(bind.global.is_some() && matches!(rhs, Atom::Const(_)));
    let x = CoreBind {
        id: LocalId(bind.id.0 + 1),
        global: None,
        ..bind.clone()
    };
    let loaded_with = |bind: CoreBind| -> Vec<Op> {
        let sum = Atom::PrimOp {
            op: PrimOp::IntAdd,
            args: vec![x.id, bind.id],
        };
        let core = CoreFn {
            params: vec![x.clone()],
            body: CoreExpr::Let {
                bind,
                rhs: rhs.clone(),
                body: Box::new(CoreExpr::Tail(sum)),
            },
            ret_ty: program.toplevel.core.ret_ty,
        };
        let mut with = program.clone();
        with.toplevel = LoweredFn::new("main".into(), "toplevel".into(), core, false);
        let code = load(&with, Audit::Never).expect("the program loads");
        let top = code.toplevels.last().expect("a toplevel");
        body(&code, top).iter().map(Ins::op).collect()
    };
    let stored = [Op::Const, Op::SetGlobal, Op::IntAdd, Op::Ret];
    assert_eq!(loaded_with(bind.clone()), stored);
    let local = CoreBind {
        global: None,
        ..bind.clone()
    };
    assert_eq!(loaded_with(local), [Op::IntAddK, Op::Ret]);
}

/// An `if` on a comparison of numbers, read nowhere else, is one jump. One
/// whose answer is read again is stored first.
#[test]
fn a_branch_on_a_comparison_is_one_jump() {
    let ops = loaded(
        "fn lt(a Int, b Int) Int { if a < b { a } else { b } }\n\
         fn ge(a Int, b Int) Int { if a >= b { a } else { b } }\n\
         fn ne(a Int) Int { if a != 3 { a } else { 0 } }\n\
         fn from(a Int) Int { if 3 < a { a } else { 0 } }\n\
         fn gt(a Float, b Float) Float { if a > b { a } else { b } }\n\
         fn above(a Float) Float { if a > 4.0 { a } else { 0.0 } }\n\
         fn below(a Float) Float { if 4.0 >= a { a } else { 0.0 } }\n\
         fn kept(a Float) (Bool, Int) {\n\
         \tc = a > 4.0\n\
         \t(c, if c { 1 } else { 2 })\n\
         }\n\
         fn far(a Int, b Int) Int {\n\
         \tc = a < b\n\
         \tk = 5\n\
         \tif c { a + k } else { b - k }\n\
         }\n\
         fn farther(a Float, b Float) (Int, Int) {\n\
         \tc = a < b\n\
         \tx = 5\n\
         \ty = 6\n\
         \tif c { (x, y) } else { (y, x) }\n\
         }\n\
         fn between(a Int, b Int) Int {\n\
         \tc = a < b\n\
         \td = a + b\n\
         \tif c { d } else { b }\n\
         }\n\
         pub fn main() { Nil }\n",
    );
    let first = |name: &str| ops[name][0];
    assert_eq!(first("lt"), Op::JumpUnlessIntLt);
    assert_eq!(first("ge"), Op::JumpUnlessIntLe);
    assert_eq!(first("ne"), Op::JumpUnlessIntNeK);
    assert_eq!(first("from"), Op::JumpUnlessIntGtK);
    assert_eq!(first("gt"), Op::JumpUnlessFloatLt);
    assert_eq!(first("above"), Op::JumpUnlessFloatGtK);
    assert_eq!(first("below"), Op::JumpUnlessFloatLeK);
    assert_eq!(ops["kept"][..2], [Op::FloatGtK, Op::JumpIfFalse]);
    // A constant that stands between the two is made before the jump, when
    // anything needs it in a register.
    let far = [
        Op::JumpUnlessIntLt,
        Op::IntAddK,
        Op::Ret,
        Op::IntSubK,
        Op::Ret,
    ];
    assert_eq!(ops["far"], far);
    let farther = [Op::Const, Op::Const, Op::JumpUnlessFloatLt];
    assert_eq!(ops["farther"][..3], farther);
    // Anything else between them keeps them apart.
    assert_eq!(
        ops["between"][..3],
        [Op::IntLt, Op::IntAdd, Op::JumpIfFalse]
    );
}

/// An `if` right after a branch that gives its condition is not the branch's
/// last comparison, because the other side jumps to the `if`.
#[test]
fn a_branch_on_what_a_branch_gives_reads_it() {
    let ops = loaded(
        "fn either(c Bool, a Int, b Int) Int {\n\
         \tif { if c { a == b } else { a < b } } { 1 } else { 2 }\n\
         }\n\
         pub fn main() { Nil }\n",
    );
    let jumps = ops["either"].iter().filter(|op| **op == Op::JumpIfFalse);
    assert_eq!(jumps.count(), 2, "{:?}", ops["either"]);
}

/// The loader is told which values are numbers and strings by their types, and
/// passes on what Perceus left undropped.
#[test]
fn a_draft_says_what_its_values_types_say() {
    let program = program(
        "fn plain(n Int, x Float, s String, b Bool, xs Array(Int)) Int {\n\
         \tif b { plain(n, x, s, b, xs) } else { n }\n}\n\
         fn any(x a, n Int) a { if n > 0 { any(x, n - 1) } else { x } }\n\
         fn lost(x a, n Int) Int { if n > 0 { lost(x, n - 1) } else { n } }\n\
         pub fn main() {\n\
         \tprintln(plain(1, 2.0, '', False, []))\n\
         \tprintln(any(1, 1) + lost(1, 1))\n\
         }\n",
    );
    let source = Source::new(&program).expect("a whole program");
    let draft = |name: &str| {
        let mut all = (&program.fns).into_iter();
        let f = all.find(|f| f.name == name).expect("the function");
        let this = Some(FuncIdx(0));
        let d = translate(&source, &f.core, f.undropped, this, &mut Tables::default());
        let d = d.expect("it translates");
        let prim = |p: &VReg| d.prims.get(p.0 as usize).copied().flatten();
        (d.params.iter().map(prim).collect::<Vec<_>>(), d.undropped)
    };
    let (int, float, string) = (Some(Prim::Int), Some(Prim::Float), Some(Prim::String));
    let plain = vec![int, float, string, None, None];
    assert_eq!(draft("plain"), (plain, false));
    assert_eq!(draft("any"), (vec![None, int], false));
    assert_eq!(draft("lost"), (vec![None, int], true));
}

/// A value whose type the program does not have is an error and not a guess,
/// wherever it is bound, and a local with the highest number there can be is
/// refused before anything is made for it.
#[test]
fn a_type_the_program_does_not_have_is_refused() {
    let mut program = program("fn f(n Int) Int { n + n + n }\npub fn main() { println(f(1)) }\n");
    let f = (&program.fns).into_iter().find(|f| f.name == "f");
    let core = f.expect("f").core.clone();
    let why = |program: &Program, core: &CoreFn| {
        let source = Source::new(program).expect("a whole program");
        translate(&source, core, false, None, &mut Tables::default()).err()
    };
    assert_eq!(why(&program, &core), None);
    let foreign = RTy(1_000_000);
    let mut param = core.clone();
    param.params[0].ty = foreign;
    assert_eq!(why(&program, &param), Some(Unfit::Broken(Broken::NoType)));
    let mut bound = core.clone();
    let CoreExpr::Let { bind, .. } = &mut bound.body else {
        panic!("{}", bound.body);
    };
    bind.ty = foreign;
    assert_eq!(why(&program, &bound), Some(Unfit::Broken(Broken::NoType)));
    let mut last = core;
    last.params[0].id = LocalId(u32::MAX);
    assert_eq!(why(&program, &last), Some(Unfit::TooMany(Limit::Values)));
    // The function stops with it when it is called, and the rest loads.
    let fns = std::mem::take(&mut program.fns);
    program.fns = fns
        .into_iter()
        .map(|mut f| {
            if f.name == "f" {
                f.core = param.clone();
            }
            f
        })
        .collect();
    let code = load(&program, Audit::Never).expect("the program loads");
    let f = named(&code, ".f");
    let [Ins::Bad { why }] = body(&code, f) else {
        panic!("one instruction");
    };
    assert_eq!(code.why(*why), crate::Stop::broken(Broken::NoType));
}

/// A translator that has made nothing, of a function that names values up to 2 and
/// reads its locals as `reads` says.
fn translator<'c>(
    source: &'c Source<'c>,
    tables: &'c mut Tables,
    reads: HashMap<LocalId, u32>,
) -> Translator<'c> {
    Translator {
        source,
        this: None,
        tables,
        steps: Vec::new(),
        lists: Lists::default(),
        labels: Vec::new(),
        landed: None,
        conts: HashMap::new(),
        values: 3,
        closure: ReadsClosure::Nothing,
        reads,
        numbers: HashMap::new(),
    }
}

/// A comparison is not taken back when something jumps to the `if` after it,
/// because the jump would then go past the test.
#[test]
fn a_comparison_something_jumps_past_is_kept() {
    let program = empty_program();
    let source = Source::new(&program).expect("a whole program");
    let mut tables = Tables::default();
    let mut t = translator(&source, &mut tables, HashMap::from([(LocalId(2), 1)]));
    let compare = |t: &mut Translator<'_>| {
        let (a, b) = (src(LocalId(0)), src(LocalId(1)));
        t.push(int_compare(Compare::LT, Dst(VReg(2)), a, b));
    };
    let to = t.label().expect("a label");
    compare(&mut t);
    let jump = t.take_comparison(LocalId(2), to);
    assert_eq!(jump.map(|j| j.op()), Some(Op::JumpUnlessIntLt));
    assert!(t.steps.is_empty());
    compare(&mut t);
    let here = t.label().expect("a label");
    t.place(here);
    assert!(t.take_comparison(LocalId(2), to).is_none());
    assert_eq!(t.steps.len(), 1);
    // Nor when the label has constants before it, after it or around it, and
    // those are left as they were.
    let constant = |t: &mut Translator<'_>, n: u32| {
        t.push(Ins::Const {
            dst: Dst(VReg(n)),
            value: Value::NIL,
        });
    };
    for before in 0..3 {
        for after in 0..3 {
            t.steps.clear();
            compare(&mut t);
            (0..before).for_each(|n| constant(&mut t, 10 + n));
            let here = t.label().expect("a label");
            t.place(here);
            (0..after).for_each(|n| constant(&mut t, 20 + n));
            let was = format!("{:?}", t.steps);
            assert!(t.take_comparison(LocalId(2), to).is_none());
            assert_eq!(format!("{:?}", t.steps), was);
        }
    }
    // With no label there, what is between the two stays in its order.
    t.steps.clear();
    t.landed = None;
    (0..2).for_each(|n| constant(&mut t, 30 + n));
    let was = format!("{:?}", t.steps);
    t.steps.clear();
    compare(&mut t);
    (0..2).for_each(|n| constant(&mut t, 30 + n));
    let jump = t.take_comparison(LocalId(2), to);
    assert_eq!(jump.map(|j| j.op()), Some(Op::JumpUnlessIntLt));
    assert_eq!(format!("{:?}", t.steps), was);
    // A label before the comparison does not matter.
    t.steps.clear();
    let before = t.label().expect("a label");
    t.place(before);
    compare(&mut t);
    assert!(t.take_comparison(LocalId(2), to).is_some());
}

/// `match %0 { C(%1, .., %n) -> arm }`, in a function that is given `%0` and
/// `%20`, as the instructions before the last: the one that says no arm fits.
fn matched(n: u32, arm: CoreExpr) -> Vec<String> {
    let variant = with_fields(n as usize);
    let fields = (1..=n).map(bind).collect();
    in_arm(CorePat::Ctor { variant, fields }, arm)
}

/// The same, of an arm with any pattern.
fn in_arm(pat: CorePat, arm: CoreExpr) -> Vec<String> {
    let mut core = function(
        1,
        CoreExpr::Match {
            scrut: LocalId(0),
            arms: vec![(pat, arm)],
            ty: RTy(0),
        },
    );
    core.params.push(bind(20));
    let program = program_with_fields();
    let source = Source::new(&program).expect("a whole program");
    let draft = translate(
        &source,
        &core,
        false,
        Some(FuncIdx(0)),
        &mut Tables::default(),
    );
    let draft = draft.expect("it translates");
    // Nothing may jump between a test and what is read out after it.
    let all = instructions(&draft);
    for (at, ins) in all.iter().enumerate() {
        if ins.op().name().starts_with("Open") {
            assert!(!draft.labels.contains(&at), "{all:?}");
        }
    }
    let ins = &all[..all.len() - 1];
    ins.iter().map(|i| shown(i, &draft.lists)).collect()
}

/// An instruction as its kind, what it writes and what it reads. A field that
/// goes to no register is `_`.
fn shown(ins: &Ins<VReg>, lists: &Lists<VReg>) -> String {
    let wide = match ins {
        Ins::OpenWide { dsts, .. } => dsts.dsts(lists).to_vec(),
        Ins::Open1 { dsts, .. } => dsts.to_vec(),
        Ins::Open2 { dsts, .. } => dsts.to_vec(),
        Ins::Open3 { dsts, .. } => dsts.to_vec(),
        Ins::Open4 { dsts, .. } => dsts.to_vec(),
        Ins::Open5 { dsts, .. } => dsts.to_vec(),
        Ins::Open6 { dsts, .. } => dsts.to_vec(),
        Ins::Open7 { dsts, .. } => dsts.to_vec(),
        Ins::Open8 { dsts, .. } => dsts.to_vec(),
        _ => Vec::new(),
    };
    let reg = |v: VReg| v.0.to_string();
    let mut dsts: Vec<String> = wide
        .iter()
        .map(|dst| dst.dst().map_or("_".to_string(), |dst| reg(dst.0)))
        .collect();
    let mut reads = Vec::new();
    ins.operands(lists, &mut |v, role| match role {
        Role::Write | Role::WriteOnHit if wide.is_empty() => dsts.push(reg(v)),
        Role::Write | Role::WriteOnHit => {}
        Role::Look | Role::Share | Role::Pass { .. } | Role::Take | Role::Drop => {
            reads.push(reg(v));
        }
    });
    let name = ins.op().name();
    format!("{} {name} {}", dsts.join(" "), reads.join(" "))
        .trim()
        .to_string()
}

fn dropping(locals: &[u32], rest: CoreExpr) -> CoreExpr {
    locals.iter().rev().fold(rest, |body, l| CoreExpr::Drop {
        local: LocalId(*l),
        body: Box::new(body),
    })
}

fn gives(l: u32) -> CoreExpr {
    CoreExpr::Tail(Atom::Local(LocalId(l)))
}

/// An arm that begins by dropping what it matched takes the cell apart where
/// its drop stands among those the arm begins with. A field that nothing reads
/// has no register, whether or not it is dropped there, and neither it nor the
/// cell is dropped besides.
#[test]
fn an_arm_that_drops_its_scrutinee_takes_it_apart() {
    let test = "JumpUnlessTag 0";
    let tuple = |of: &[u32]| {
        let args = of.iter().copied().map(LocalId).collect();
        CoreExpr::Tail(Atom::prim(PrimOp::MakeTuple, args))
    };
    let both = || tuple(&[1, 2]);
    for (drops, rest, want) in [
        (&[0][..], both(), &["1 2 Open2 0"][..]),
        (&[0], gives(2), &["_ 2 Open2 0"]),
        (&[0, 1], gives(2), &["_ 2 Open2 0"]),
        (&[0, 2], gives(1), &["1 _ Open2 0"]),
        // Others' drops stay on the side they were on, in their order.
        (&[20, 0], both(), &["Drop 20", "1 2 Open2 0"]),
        (&[0, 20], both(), &["1 2 Open2 0", "Drop 20"]),
        (&[20, 0, 2], gives(1), &["Drop 20", "1 _ Open2 0"]),
        (&[0, 1, 20], gives(2), &["_ 2 Open2 0", "Drop 20"]),
        // With nothing to keep there is nothing to take apart.
        (&[0], gives(20), &["Drop 0"]),
        (&[0, 1, 2], gives(20), &["Drop 0"]),
        (&[20, 0, 2], tuple(&[]), &["Drop 20", "Drop 0"]),
    ] {
        let got = matched(2, dropping(drops, rest));
        assert_eq!(got[0], test);
        assert_eq!(got[1..=want.len()], *want, "{drops:?}: {got:?}");
        let more = got[want.len() + 1..].iter();
        let again = more.filter(|i| i.contains("Drop") || i.contains("Field"));
        assert_eq!(again.count(), 0, "{got:?}");
    }
}

/// Every number of fields is taken apart, those that do not fit in the
/// instruction through a list. One with no fields is only dropped.
#[test]
fn a_constructor_of_any_number_of_fields_is_taken_apart() {
    for n in 0..=10u32 {
        let fields: Vec<_> = (1..=n).map(LocalId).collect();
        let rest = CoreExpr::Tail(Atom::prim(PrimOp::MakeTuple, fields));
        let got = matched(n, dropping(&[0], rest));
        let dsts: Vec<_> = (1..=n).map(|i| i.to_string()).collect();
        let want = match n {
            0 => "Drop 0".to_string(),
            1..=8 => format!("{} Open{n} 0", dsts.join(" ")),
            _ => format!("{} OpenWide 0", dsts.join(" ")),
        };
        assert_eq!(got[1], want);
    }
}

/// Anything else reads each field out, and drops what the arm says where it
/// says it: an arm that does not begin with drops, that drops a field before the
/// value it is a field of, or that does not drop the value at all, and one whose
/// pattern is no constructor's.
#[test]
fn an_arm_that_does_not_begin_so_reads_the_fields_out() {
    let fields = ["1 Field 0", "2 Field 0"];
    let make = |dst: u32, body: CoreExpr| CoreExpr::Let {
        bind: bind(dst),
        rhs: Atom::Nil,
        body: Box::new(body),
    };
    let in_a_case = |body: CoreExpr| CoreExpr::LetCont {
        id: JoinId(1),
        cont: Box::new(gives(20)),
        body: Box::new(body),
    };
    let in_a_join = |body: CoreExpr| CoreExpr::LetJoin {
        bind: bind(9),
        join: Box::new(body),
        body: Box::new(gives(9)),
    };
    for (arm, want) in [
        (gives(1), &["Ret 1"][..]),
        (
            dropping(&[20, 2], gives(1)),
            &["Drop 20", "Drop 2", "Ret 1"],
        ),
        (dropping(&[2, 0], gives(1)), &["Drop 2", "Drop 0", "Ret 1"]),
        (
            dropping(&[20, 1, 0], gives(2)),
            &["Drop 20", "Drop 1", "Drop 0", "Ret 2"],
        ),
        (
            make(8, dropping(&[0], gives(1))),
            &["8 Const", "Drop 0", "Ret 1"],
        ),
        (
            dropping(&[20], make(8, dropping(&[0], gives(1)))),
            &["Drop 20", "8 Const", "Drop 0", "Ret 1"],
        ),
        (
            in_a_case(dropping(&[0], gives(1))),
            &["Drop 0", "Ret 1", "Ret 20"],
        ),
        (
            in_a_join(dropping(&[0], gives(1))),
            &["Drop 0", "9 Move 1", "Jump", "Ret 9"],
        ),
    ] {
        let got = matched(2, arm);
        assert_eq!(got[1..3], fields, "{got:?}");
        assert_eq!(got[3..], *want);
    }
    // A field with the scrutinee's own number is no field of a program's.
    let variant = with_fields(2);
    let fields = vec![bind(0), bind(2)];
    let got = in_arm(CorePat::Ctor { variant, fields }, dropping(&[0], gives(2)));
    assert_eq!(got[1..], ["0 Field 0", "2 Field 0", "Drop 0", "Ret 2"]);
    let got = in_arm(CorePat::Bind(bind(1)), dropping(&[0], gives(1)));
    assert_eq!(got, ["1 Move 0", "Drop 0", "Ret 1"]);
    let got = in_arm(CorePat::Wild, dropping(&[0], gives(20)));
    assert_eq!(got, ["Drop 0", "Ret 20"]);
}

/// A pattern with very many fields and very many drops is gone over a fixed number
/// of times. Here 30,000 fields are read, 30,000 are only dropped, and 60,000 other
/// values are dropped on both sides of the scrutinee. Going over the drops for each
/// field, or over the fields for each drop, is billions of compares, which nobody
/// waits for twice: each such scan makes this test take eight seconds and not a
/// third of one.
#[test]
fn a_pattern_of_many_fields_is_gone_over_a_few_times() {
    const N: u32 = 60_000;
    let fields: Vec<_> = (1..=N).map(bind).collect();
    let others = |from: u32| from..from + N / 2;
    let unread = || (1..=N).filter(|l| l % 2 == 0);
    let kept: Vec<_> = (1..=N).filter(|l| l % 2 == 1).map(LocalId).collect();
    let mut arm = CoreExpr::Tail(Atom::prim(PrimOp::MakeTuple, kept.clone()));
    let drops = others(N + 1)
        .chain([0])
        .chain(unread())
        .chain(others(2 * N));
    for l in drops.collect::<Vec<_>>().into_iter().rev() {
        arm = dropping(&[l], arm);
    }
    let reads = unread()
        .chain(others(N + 1))
        .chain(others(2 * N))
        .chain(kept.iter().map(|l| l.0));
    let program = empty_program();
    let source = Source::new(&program).expect("a whole program");
    let mut tables = Tables::default();
    let reads = reads.map(|l| (LocalId(l), 1)).collect();
    let t = translator(&source, &mut tables, reads);
    let opened = t.opened(LocalId(0), &fields, &arm);
    let opened = opened.map(|o| (o.before, o.dsts, o.after));
    // Freed from the outside in, because as a tree it is deeper than the stack.
    while let CoreExpr::Drop { body, .. } = arm {
        arm = *body;
    }
    let (before, dsts, after) = opened.expect("it begins so");
    let locals = |of: Vec<LocalId>| of.iter().map(|l| l.0).collect::<Vec<_>>();
    assert_eq!(locals(before), others(N + 1).collect::<Vec<_>>());
    assert_eq!(locals(after), others(2 * N).collect::<Vec<_>>());
    assert_eq!(dsts.len(), fields.len());
    let dsts: Vec<_> = dsts.iter().filter_map(|d| d.dst()).collect();
    assert_eq!(dsts, kept.iter().map(|l| Dst(of(*l))).collect::<Vec<_>>());
}

/// A field that is dropped where the arm begins and read besides, which no
/// compiler makes, still has a register and its drop.
#[test]
fn a_field_read_after_its_drop_keeps_both() {
    let got = matched(2, dropping(&[0, 1], gives(1)));
    assert_eq!(got[1..], ["1 _ Open2 0", "Drop 1", "Ret 1"]);
}

/// An instruction with more operands than it has room for takes them from a
/// list, and reads the same ones.
#[test]
fn operands_that_do_not_fit_are_in_a_list() {
    let list = |n: usize| (0..n).map(|i| LocalId(i as u32 % 5)).collect::<Vec<_>>();
    let prim = |op: PrimOp, n: usize| Atom::prim(op, list(n));
    let ctor = |n: usize| Atom::Ctor {
        variant: with_fields(n),
        fields: list(n),
    };
    let call = |callee: Callee, n: usize| Atom::Call {
        callee,
        args: list(n),
    };
    let closure = |n: usize| Atom::Closure {
        func_idx: FuncIdx(0),
        captures: list(n),
    };
    let cases = [
        (prim(PrimOp::MakeTuple, 0), Op::Tuple),
        (prim(PrimOp::MakeTuple, 1), Op::Tuple),
        (prim(PrimOp::MakeTuple, 2), Op::Tuple2),
        (prim(PrimOp::MakeTuple, 3), Op::Tuple3),
        (prim(PrimOp::MakeTuple, 4), Op::Tuple),
        (prim(PrimOp::MakeTuple, 8), Op::Tuple),
        (prim(PrimOp::MakeTuple, 9), Op::TupleWide),
        (prim(PrimOp::MakeArray, 8), Op::Array),
        (prim(PrimOp::MakeArray, 9), Op::ArrayWide),
        (prim(PrimOp::StringConcatMany, 8), Op::Concat),
        (prim(PrimOp::StringConcatMany, 9), Op::ConcatWide),
        (prim(PrimOp::BinaryConcatMany, 8), Op::Bits),
        (prim(PrimOp::BinaryConcatMany, 9), Op::BitsWide),
        (prim(PrimOp::ArrayPrepend, 9), Op::ArrayPrepend),
        (prim(PrimOp::ArrayPrepend, 10), Op::ArrayPrependWide),
        (prim(PrimOp::ArrayAppend, 9), Op::ArrayAppend),
        (prim(PrimOp::ArrayAppend, 10), Op::ArrayAppendWide),
        (ctor(8), Op::Ctor),
        (ctor(9), Op::CtorWide),
        (closure(8), Op::Closure),
        (closure(9), Op::ClosureWide),
        (call(Callee::Local(LocalId(0)), 6), Op::CallValue),
        (call(Callee::Local(LocalId(0)), 7), Op::CallValueWide),
    ];
    for (atom, want) in cases {
        let mut reads = Vec::new();
        atom.for_each_operand(|l| reads.push(of(l)));
        reads.sort();
        let core = function(
            5,
            CoreExpr::Let {
                bind: bind(5),
                rhs: atom,
                body: Box::new(CoreExpr::Tail(Atom::Local(LocalId(5)))),
            },
        );
        let program = program_with_fields();
        let source = Source::new(&program).expect("a whole program");
        let draft = translate(
            &source,
            &core,
            false,
            Some(FuncIdx(0)),
            &mut Tables::default(),
        )
        .expect("it translates");
        let first = instructions(&draft)[0];
        assert_eq!(first.op(), want);
        let mut named = Vec::new();
        first.operands(&draft.lists, &mut |v, role| {
            if role != Role::Write {
                named.push(v);
            }
        });
        named.sort();
        assert_eq!(named, reads, "{want:?}");
    }
    // In tail position too.
    let tail = |n| function(5, CoreExpr::Tail(call(Callee::Local(LocalId(0)), n)));
    assert_eq!(translated(&tail(6)), Ok(vec![Op::TailCallValue]));
    assert_eq!(translated(&tail(7)), Ok(vec![Op::TailCallValueWide]));
}

/// A call of a function the program does not have, or with another number of
/// arguments than it takes, is the instruction that says so.
#[test]
fn a_call_that_cannot_be_right_stops_the_run() {
    let program = empty_program();
    let source = Source::new(&program).expect("a whole program");
    let main = program.main.expect("a main");
    let why = |core: &CoreFn, this| {
        let mut tables = Tables::default();
        let draft = translate(&source, core, false, this, &mut tables).expect("it translates");
        assert_eq!(instructions(&draft).first().map(Ins::op), Some(Op::Bad));
        tables.whys.remove(0)
    };
    let call = |callee, n: u32| {
        let args = (0..n).map(LocalId).collect();
        function(n, CoreExpr::Tail(Atom::Call { callee, args }))
    };
    let missing = FuncIdx(10_000);
    assert_eq!(
        why(&call(Callee::Known(missing), 0), Some(main)),
        Why::Missing(Missing::NoFunc(missing))
    );
    let arity = |got| {
        Why::Broken(Broken::CallArity {
            func: "main.main".into(),
            got,
            want: 0,
        })
    };
    assert_eq!(why(&call(Callee::Known(main), 2), Some(main)), arity(2));
    assert_eq!(why(&call(Callee::Self_, 1), Some(main)), arity(1));
    assert_eq!(
        why(&call(Callee::Self_, 0), None),
        Why::Broken(Broken::ToplevelCallsSelf)
    );
    let load = |load| function(0, CoreExpr::Tail(Atom::Load(load)));
    let capture = |i| load(Load::Capture(scarlet_ir::core_ir::CaptureIdx(i)));
    let toplevel = Why::Broken(Broken::ToplevelCapture);
    assert_eq!(why(&capture(0), None), toplevel);
    let negative = Why::Broken(Broken::NegativeCapture);
    assert_eq!(why(&capture(-1), Some(main)), negative);
    let itself = Why::Broken(Broken::ToplevelSelf);
    assert_eq!(why(&load(Load::SelfClosure), None), itself);
    let wild = function(0, CoreExpr::Goto(JoinId(3)));
    assert_eq!(why(&wild, Some(main)), Why::Broken(Broken::UndeclaredCase));
    // A case is gone once its own code has been reached.
    let goes_back = function(
        0,
        CoreExpr::LetCont {
            id: JoinId(3),
            cont: Box::new(CoreExpr::Goto(JoinId(3))),
            body: Box::new(CoreExpr::Goto(JoinId(3))),
        },
    );
    let mut tables = Tables::default();
    translate(&source, &goes_back, false, None, &mut tables).expect("it translates");
    assert_eq!(tables.whys, [Why::Broken(Broken::UndeclaredCase)]);
}

/// A built-in the VM does not run, or is called with another number of
/// arguments, leaves the function waiting for it.
#[test]
fn a_built_in_with_the_wrong_arguments_is_missing() {
    let call = |intrinsic, n: u32| {
        let args = (0..n).map(LocalId).collect();
        translated(&function(
            n,
            CoreExpr::Tail(Atom::Intrinsic { intrinsic, args }),
        ))
    };
    let missing = |i| Err(Unfit::Missing(Missing::Intrinsic(i)));
    assert_eq!(call(Intrinsic::MapSize, 1), Ok(vec![Op::Builtin, Op::Ret]));
    assert_eq!(call(Intrinsic::MapSize, 2), missing(Intrinsic::MapSize));
    assert_eq!(call(Intrinsic::MapSize, 9), missing(Intrinsic::MapSize));
    let subjects = Intrinsic::InternalLiveSubjects;
    assert_eq!(call(subjects, 0), missing(subjects));
}

/// A function that reads its captures, or itself, keeps the closure it runs as:
/// it gives the closure up when it returns, and passes it on when it calls
/// itself. Any other is called by name from itself too, and has none to give
/// up.
#[test]
fn a_function_keeps_the_closure_it_reads() {
    let code = code(
        "fn applied(f fn(Int) Int, n Int) Int { f(n) }\n\
         fn plain(n Int) Int { if n == 0 { 0 } else { plain(n - 1) + 1 } }\n\
         pub fn main() {\n\
         \tkept = 7\n\
         \treads = fn(n) { if n == 0 { kept } else { reads(n - 1) + 1 } }\n\
         \treads_not = fn(n) { if n == 0 { 0 } else { reads_not(n - 1) + 1 } }\n\
         \titself = fn(n) { if n == 0 { 0 } else { applied(itself, n - 1) + itself(0) } }\n\
         \tprintln(plain(reads(3) + reads_not(4) + itself(2)))\n\
         }\n",
    );
    let mut seen = Vec::new();
    for e in entries(&code) {
        let ops: Vec<Op> = body(&code, e).iter().map(Ins::op).collect();
        let has = |of: &[Op]| ops.iter().any(|op| of.contains(op));
        let reads = match (has(&[Op::GetCapture]), has(&[Op::GetSelf])) {
            (true, _) => ReadsClosure::Captures,
            (false, true) => ReadsClosure::Itself,
            (false, false) => ReadsClosure::Nothing,
        };
        assert_eq!(e.closure, reads, "{}", e.name);
        let keeps = reads != ReadsClosure::Nothing;
        assert_eq!(reads.keeps(), keeps);
        assert_eq!(has(&[Op::RetClosure]), keeps, "{}", e.name);
        assert!(!(keeps && has(&[Op::Ret])), "{}", e.name);
        if has(&[Op::CallSelf, Op::Call1]) && !e.name.ends_with(".main") {
            seen.push((keeps, has(&[Op::CallSelf]), has(&[Op::Call1])));
        }
    }
    seen.sort_unstable();
    let by_name = (false, false, true);
    let as_itself = (true, true, false);
    assert_eq!(seen, [by_name, by_name, as_itself, as_itself]);
}

/// A call by name of up to three arguments has an instruction of its own, and
/// another when it shares one of them. The same argument twice is shared once.
#[test]
fn a_call_is_the_one_for_its_arguments() {
    let code = code(
        "fn f0() Int { 1 }\n\
         fn f1(a Int) Int { a }\n\
         fn f2(a Int, b Int) Int { a + b }\n\
         fn f3(a Int, b Int, c Int) Int { a + b + c }\n\
         fn f4(a Int, b Int, c Int, d Int) Int { a + b + c + d }\n\
         fn f7(a Int, b Int, c Int, d Int, e Int, f Int, g Int) Int {\n\
         \ta + b + c + d + e + f + g\n\
         }\n\
         fn take0() Int { f0() + 1 }\n\
         fn take1(a Int) Int { f1(a) + 1 }\n\
         fn take2(a Int, b Int) Int { f2(a, b) + 1 }\n\
         fn take3(a Int, b Int, c Int) Int { f3(a, b, c) + 1 }\n\
         fn take4(a Int, b Int, c Int, d Int) Int { f4(a, b, c, d) + 1 }\n\
         fn take7(a Int) Int { f7(1, 2, 3, 4, 5, 6, a) + 1 }\n\
         fn share1(a Int) Int { f1(a) + a }\n\
         fn share2(a Int, b Int) Int { f2(a, b) + b }\n\
         fn share3(a Int, b Int, c Int) Int { f3(a, b, c) + a }\n\
         fn twice(a Int) Int { f2(a, a) + 1 }\n\
         pub fn main() {\n\
         \tprintln(take0() + take1(1) + take2(1, 2) + take3(1, 2, 3) + take4(1, 2, 3, 4))\n\
         \tprintln(take7(7) + share1(1) + share2(1, 2) + share3(1, 2, 3) + twice(1))\n\
         }\n",
    );
    let first = |name: &str| -> (String, Vec<Role>) {
        let ins = body(&code, named(&code, name));
        let call = ins.iter().find(|i| i.op().name().starts_with("Call"));
        let call = call.expect("a call");
        let mut roles = Vec::new();
        call.operands(&code.lists, &mut |_, role| roles.push(role));
        (call.op().name().to_string(), roles[1..].to_vec())
    };
    let (share, pass) = (Role::Pass { moved: false }, Role::Pass { moved: true });
    let calls = [
        ("take0", "Call0", vec![]),
        ("take1", "Call1", vec![Role::Take]),
        ("take2", "Call2", vec![Role::Take; 2]),
        ("take3", "Call3", vec![Role::Take; 3]),
        ("take4", "Call", vec![pass; 4]),
        ("take7", "CallWide", vec![pass; 7]),
        ("share1", "Call1Share", vec![share]),
        ("share2", "Call2Share", vec![pass, share]),
        ("share3", "Call3Share", vec![share, pass, pass]),
        ("twice", "Call2Share", vec![share, pass]),
    ];
    for (name, op, roles) in calls {
        let want = (op.to_string(), roles);
        assert_eq!(first(&format!(".{name}")), want, "{name}");
    }
}

/// A constructor is loaded as the one for its number of fields when it takes
/// them all, as the one that counts through them when it shares one, and as the
/// one with a list past eight. A tuple is loaded by its number of elements, and
/// it and a closure share everything they are given.
#[test]
fn a_new_cell_has_an_instruction_for_what_it_is_given() {
    let widths = 1..=9usize;
    let each = |n: usize, f: &dyn Fn(usize) -> String| -> String {
        (0..n).map(f).collect::<Vec<_>>().join(", ")
    };
    let mut src = String::new();
    let mut main = String::new();
    for n in widths.clone() {
        let params = each(n, &|i| format!("a{i} Int"));
        let fields = each(n, &|i| format!("f{i} Int"));
        let args = each(n, &|i| format!("a{i}"));
        let types = each(n, &|_| "Int".to_string());
        src += &format!(
            "type T{n} {{\n\tT{n}({fields})\n}}\n\
             fn take{n}({params}) T{n} {{ T{n}({args}) }}\n\
             fn share{n}({params}) T{n} {{\n\
             \tt = T{n}({args})\n\
             \tprintln(a0)\n\
             \tt\n\
             }}\n\
             fn same{n}(a Int) T{n} {{ T{n}({}) }}\n\
             fn over{n}({params}) fn(Int) Int {{ fn(x) x + {} }}\n",
            each(n, &|_| "a".to_string()),
            args.replace(", ", " + "),
        );
        let ints = each(n, &|i| format!("{i}"));
        main += &format!(
            "\tprintln(take{n}({ints}))\n\
             \tprintln(share{n}({ints}))\n\
             \tprintln(same{n}(1))\n\
             \tprintln(over{n}({ints})(1))\n"
        );
        if n > 1 {
            src += &format!("fn pair{n}({params}) ({types}) {{ ({args}) }}\n");
            main += &format!("\tprintln(pair{n}({ints}))\n");
        }
    }
    let code = code(&format!("{src}pub fn main() {{\n{main}}}\n"));
    let made = |name: String| -> (String, Vec<Role>) {
        let ins = body(&code, named(&code, &format!(".{name}")));
        let makes = ["Ctor", "Tuple", "Closure"];
        let kind = |i: &&Ins<Reg>| makes.iter().any(|m| i.op().name().starts_with(m));
        let ins = ins.iter().find(kind).expect("a cell is made");
        let mut roles = Vec::new();
        ins.operands(&code.lists, &mut |_, role| roles.push(role));
        (ins.op().name().to_string(), roles[1..].to_vec())
    };
    let (share, pass) = (Role::Pass { moved: false }, Role::Pass { moved: true });
    for n in widths {
        let taking = if n > 8 {
            ("CtorWide".to_string(), vec![pass; n])
        } else {
            (format!("Ctor{n}"), vec![Role::Take; n])
        };
        assert_eq!(made(format!("take{n}")), taking);
        let sharing = if n > 8 { "CtorWide" } else { "Ctor" };
        let mut roles = vec![pass; n];
        roles[0] = share;
        assert_eq!(made(format!("share{n}")), (sharing.to_string(), roles));
        // Of one register's operands the last takes it.
        let mut roles = vec![share; n];
        roles[n - 1] = pass;
        let same = if n == 1 {
            ("Ctor1".to_string(), vec![Role::Take])
        } else {
            (sharing.to_string(), roles)
        };
        assert_eq!(made(format!("same{n}")), same);
        let closure = if n > 8 { "ClosureWide" } else { "Closure" };
        let shared = vec![Role::Share; n];
        let over = (closure.to_string(), shared.clone());
        assert_eq!(made(format!("over{n}")), over);
        if n > 1 {
            let tuple = match n {
                2 => "Tuple2",
                3 => "Tuple3",
                9 => "TupleWide",
                _ => "Tuple",
            };
            assert_eq!(made(format!("pair{n}")), (tuple.to_string(), shared));
        }
    }
}
