use super::super::ins::{Ins, Role};
use super::super::tests::{body, entries, named, program};
use super::super::{Code, Op, Reg, load};
use super::*;
use crate::Audit;
use scarlet_ir::core_ir::{CoreBind, LoweredFn, VariantRef};
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
            Op::Ctor,
            Op::Jump,
            Op::Const,
            Op::Ctor,
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
    assert_eq!(moves(".bound"), [true]);
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
    assert_eq!(ops["pair"], [Op::Const, Op::IntAddK, Op::Tuple, Op::Ret]);
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

/// `core` translated, as function 0 of a program with nothing in it but `main`.
fn translated(core: &CoreFn) -> Result<Vec<Op>, Unfit> {
    let program = empty_program();
    let draft = translate(&program, core, Some(FuncIdx(0)), &mut Tables::default());
    draft.map(|d| d.ins.iter().map(Ins::op).collect())
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
        with.toplevel = LoweredFn::new("main".into(), "toplevel".into(), core);
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

/// A comparison is not taken back when something jumps to the `if` after it,
/// because the jump would then go past the test.
#[test]
fn a_comparison_something_jumps_past_is_kept() {
    let program = empty_program();
    let mut tables = Tables::default();
    let mut t = Translator {
        program: &program,
        this: None,
        tables: &mut tables,
        ins: Vec::new(),
        lists: Lists::default(),
        labels: Vec::new(),
        landed: None,
        conts: HashMap::new(),
        values: 3,
        kept: BTreeSet::new(),
        carried: BTreeSet::new(),
        reads: HashMap::from([(LocalId(2), 1)]),
        numbers: HashMap::new(),
    };
    let compare = |t: &mut Translator<'_>| {
        let (a, b) = (src(LocalId(0)), src(LocalId(1)));
        t.push(int_compare(Compare::LT, Dst(VReg(2)), a, b));
    };
    let to = t.label().expect("a label");
    compare(&mut t);
    let jump = t.take_comparison(LocalId(2), to);
    assert_eq!(jump.map(|j| j.op()), Some(Op::JumpUnlessIntLt));
    assert!(t.ins.is_empty());
    compare(&mut t);
    let here = t.label().expect("a label");
    t.place(here);
    assert!(t.take_comparison(LocalId(2), to).is_none());
    assert_eq!(t.ins.len(), 1);
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
            t.ins.clear();
            compare(&mut t);
            (0..before).for_each(|n| constant(&mut t, 10 + n));
            let here = t.label().expect("a label");
            t.place(here);
            (0..after).for_each(|n| constant(&mut t, 20 + n));
            let was = format!("{:?}", t.ins);
            assert!(t.take_comparison(LocalId(2), to).is_none());
            assert_eq!(format!("{:?}", t.ins), was);
        }
    }
    // With no label there, what is between the two stays in its order.
    t.ins.clear();
    t.landed = None;
    (0..2).for_each(|n| constant(&mut t, 30 + n));
    let was = format!("{:?}", t.ins);
    t.ins.clear();
    compare(&mut t);
    (0..2).for_each(|n| constant(&mut t, 30 + n));
    let jump = t.take_comparison(LocalId(2), to);
    assert_eq!(jump.map(|j| j.op()), Some(Op::JumpUnlessIntLt));
    assert_eq!(format!("{:?}", t.ins), was);
    // A label before the comparison does not matter.
    t.ins.clear();
    let before = t.label().expect("a label");
    t.place(before);
    compare(&mut t);
    assert!(t.take_comparison(LocalId(2), to).is_some());
}

/// An instruction with more operands than it has room for takes them from a
/// list, and reads the same ones.
#[test]
fn operands_that_do_not_fit_are_in_a_list() {
    let list = |n: usize| (0..n).map(|i| LocalId(i as u32 % 5)).collect::<Vec<_>>();
    let prim = |op: PrimOp, n: usize| Atom::prim(op, list(n));
    let variant = VariantRef {
        type_id: scarlet_ir::TypeId(1),
        variant_idx: 0,
    };
    let ctor = |n: usize, reuse: Option<u32>| Atom::Ctor {
        variant,
        fields: list(n),
        reuse: reuse.map(LocalId),
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
        (ctor(6, None), Op::Ctor),
        (ctor(7, None), Op::CtorWide),
        (ctor(5, Some(0)), Op::CtorOver),
        (ctor(6, Some(0)), Op::CtorOverWide),
        (closure(8), Op::Closure),
        (closure(9), Op::ClosureWide),
        (call(Callee::Local(LocalId(0)), 6), Op::CallValue),
        (call(Callee::Local(LocalId(0)), 7), Op::CallValueWide),
    ];
    for (atom, want) in cases {
        let mut reads = Vec::new();
        atom.for_each_operand(|l| reads.push(of(l)));
        reads.extend(match &atom {
            Atom::Ctor { reuse, .. } => reuse.map(of),
            _ => None,
        });
        reads.sort();
        let core = function(
            5,
            CoreExpr::Let {
                bind: bind(5),
                rhs: atom,
                body: Box::new(CoreExpr::Tail(Atom::Local(LocalId(5)))),
            },
        );
        let program = empty_program();
        let draft = translate(&program, &core, Some(FuncIdx(0)), &mut Tables::default())
            .expect("it translates");
        assert_eq!(draft.ins[0].op(), want);
        let mut named = Vec::new();
        draft.ins[0].operands(&draft.lists, &mut |v, role| {
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
    let main = program.main.expect("a main");
    let why = |core: &CoreFn, this| {
        let mut tables = Tables::default();
        let draft = translate(&program, core, this, &mut tables).expect("it translates");
        assert_eq!(draft.ins.first().map(Ins::op), Some(Op::Bad));
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
    translate(&program, &goes_back, None, &mut tables).expect("it translates");
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
