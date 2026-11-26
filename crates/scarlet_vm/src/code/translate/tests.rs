use super::super::ins::{Ins, Role};
use super::super::tests::{body, copy_of, entries, instructions, named, program, targets};
use super::super::{Code, Op, Reg, load};
use super::*;
use crate::Audit;
use scarlet_ir::TypeId;
use scarlet_ir::core_ir::{CoreBind, Program, TypeNames, VariantNames, VariantRef};
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
/// the array back on a miss, which takes no instruction but the return, because what
/// the write makes is in the array's register. `set` builds `Ok` or `Err(Nil)`. The
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
        [Op::Const, Op::ArraySet, Op::Ret, Op::Ret]
    );
    assert_eq!(taken(".last_use"), [true]);
    assert_eq!(taken(".read_again"), [false]);
    assert_eq!(
        ops(&code, ".checked"),
        [
            Op::Const,
            Op::ArraySet,
            Op::Ctor1,
            Op::Ret,
            Op::Drop,
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

/// A map that is given up behind a set or a delete gives its reference to it,
/// wherever its drop is among those there: the key's and the value's may come
/// first, and do in generic code and for a map that is no parameter.
#[test]
fn a_map_given_up_behind_a_write_gives_it_its_reference() {
    let all = loaded(
        "import scarlet/map\n\
         type Task {\n\
         \tidentity Int\n\
         \tnames Array(String)\n\
         }\n\
         fn put(k a, v b, m map::Map(a, b)) (map::Map(a, b), Int) {\n\
         \tr = map::set(m, k, v)\n\
         \tn = map::size(r)\n\
         \t(r, n)\n\
         }\n\
         fn mixed(k a, m map::Map(a, b), v b) (map::Map(a, b), Int) {\n\
         \tr = map::set(m, k, v)\n\
         \tn = map::size(r)\n\
         \t(r, n)\n\
         }\n\
         fn del(k a, m map::Map(a, b)) (map::Map(a, b), Int) {\n\
         \tr = map::delete(m, k)\n\
         \tn = map::size(r)\n\
         \t(r, n)\n\
         }\n\
         fn both(m map::Map(a, b), j a, k a, v b) (map::Map(a, b), Int) {\n\
         \tr = m |> map::delete(j) |> map::set(k, v)\n\
         \tn = map::size(r)\n\
         \t(r, n)\n\
         }\n\
         fn tasks(t Task, m map::Map(Int, Task)) (map::Map(Int, Task), Int) {\n\
         \tr = map::set(m, t.identity, t)\n\
         \tn = map::size(r)\n\
         \t(r, n)\n\
         }\n\
         fn kept(k a, v b, m map::Map(a, b)) (map::Map(a, b), map::Map(a, b)) {\n\
         \tr = map::set(m, k, v)\n\
         \ts = map::delete(m, k)\n\
         \t(r, s)\n\
         }\n\
         pub fn main() {\n\
         \tm = map::set(map::new(), 1, 2)\n\
         \tprintln(put(1, 2, m))\n\
         \tprintln(mixed(1, m, 2))\n\
         \tprintln(del(1, m))\n\
         \tprintln(both(m, 1, 2, 3))\n\
         \tprintln(tasks(Task(1, []), map::new()))\n\
         \tprintln(kept(1, 2, m))\n\
         }\n",
    );
    let owned = |name: &str| {
        let ops = all[name].iter();
        let writes =
            ops.filter(|op| matches!(op, Op::MapSetOwned | Op::MapDeleteOwned | Op::Builtin));
        writes.copied().collect::<Vec<Op>>()
    };
    // The last `Builtin` is the size.
    let set = [Op::MapSetOwned, Op::Builtin];
    assert_eq!(owned("put"), set);
    assert_eq!(owned("mixed"), set);
    assert_eq!(owned("tasks"), set);
    assert_eq!(owned("del"), [Op::MapDeleteOwned, Op::Builtin]);
    let both = [Op::MapDeleteOwned, Op::MapSetOwned, Op::Builtin];
    assert_eq!(owned("both"), both);
    assert_eq!(owned("kept"), [Op::Builtin, Op::MapDeleteOwned]);
}

/// A write's key or value that is the map itself keeps it, and everything given up
/// behind the write is given up once.
#[test]
fn a_write_to_a_map_gives_up_the_rest_behind_it() {
    let write = |intrinsic: Intrinsic, args: &[u32], drops: &[u32]| {
        let rhs = Atom::Intrinsic {
            intrinsic,
            args: args.iter().copied().map(LocalId).collect(),
        };
        let body = Box::new(dropping(drops, gives(9)));
        let body = CoreExpr::Let {
            bind: bind(9),
            rhs,
            body,
        };
        let core = function(3, body);
        let program = empty_program();
        let source = Source::new(&program).expect("a whole program");
        let d = translate(&source, &core, None, &mut Tables::default()).expect("it loads");
        let ins = instructions(&d);
        let ins = ins.iter().map(|i| shown(i, &d.lists));
        ins.take(1 + drops.len()).collect::<Vec<String>>()
    };
    let set = Intrinsic::MapSet;
    let delete = Intrinsic::MapDelete;
    assert_eq!(
        write(set, &[0, 1, 2], &[1, 0, 2])[..3],
        ["9 MapSetOwned 0 1 2", "Drop 1", "Drop 2"]
    );
    assert_eq!(
        write(set, &[2, 0, 1], &[0, 1, 2])[..3],
        ["9 MapSetOwned 2 0 1", "Drop 0", "Drop 1"]
    );
    assert_eq!(
        write(delete, &[1, 0], &[0, 1, 2])[..3],
        ["9 MapDeleteOwned 1 0", "Drop 0", "Drop 2"]
    );
    // The map is not given up, or is one of the other two besides.
    assert_eq!(
        write(set, &[0, 1, 2], &[1, 2]),
        ["9 Builtin 0 1 2", "Drop 1", "Drop 2"]
    );
    for args in [[0, 0, 1], [0, 1, 0]] {
        let got = write(set, &args, &[1, 0]);
        assert!(got[0].starts_with("9 Builtin"), "{got:?}");
        assert_eq!(got[1..], ["Drop 1", "Drop 0"]);
    }
    let got = write(delete, &[0, 0], &[0]);
    assert!(got[0].starts_with("9 Builtin"), "{got:?}");
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
    CoreBind::new(LocalId(id), RTy(0))
}

fn gives(l: u32) -> CoreExpr {
    CoreExpr::Ret(LocalId(l))
}

/// Return what `rhs` gives.
fn returning(rhs: Atom) -> CoreExpr {
    let bind = bind(200_000);
    CoreExpr::Let {
        bind,
        rhs,
        body: Box::new(CoreExpr::Ret(bind.id)),
    }
}

fn jump(to: u32, args: &[u32]) -> CoreExpr {
    CoreExpr::Jump {
        to: JoinId(to),
        args: args.iter().copied().map(LocalId).collect(),
    }
}

fn join(id: u32, params: &[u32], body: CoreExpr, cont: CoreExpr) -> CoreExpr {
    CoreExpr::Join {
        id: JoinId(id),
        params: params.iter().copied().map(bind).collect(),
        cont: Box::new(cont),
        body: Box::new(body),
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

/// A constructor with another number of fields than the program lists, or that it
/// does not list, is in a function that does not load, whether it is built, switched
/// on or taken apart. One it lists is told by the top of its word.
#[test]
fn a_constructor_the_program_does_not_list_does_not_load() {
    let program = program_with_fields();
    let source = Source::new(&program).expect("a whole program");
    let load = |core: &CoreFn| {
        let draft = translate(&source, core, Some(FuncIdx(0)), &mut Tables::default());
        draft.map(|d| instructions(&d))
    };
    let builds = |variant, n: u32| {
        function(
            n,
            returning(Atom::Ctor {
                variant,
                fields: (0..n).map(LocalId).collect(),
            }),
        )
    };
    let matches = |variant: VariantRef| {
        let arms = vec![(vec![variant.variant_idx], gives(0))];
        let ty = variant.type_id;
        function(1, switch(0, Cases::Variant { ty, arms }, Some(gives(0))))
    };
    let opens = |variant, fields: &[u16]| {
        let fields = fields.iter().map(|i| (*i, bind(u32::from(*i) + 1)));
        function(
            1,
            CoreExpr::Open {
                cell: LocalId(0),
                variant,
                fields: fields.collect(),
                body: Box::new(gives(1)),
            },
        )
    };
    let wrong = |ctor, got, want| Some(Unfit::Broken(Broken::CtorFields { ctor, got, want }));
    for (variant, got, want) in [(2, 1, 2), (2, 3, 2), (2, 0, 2), (0, 1, 0)] {
        let ctor = with_fields(variant);
        let core = builds(ctor, got);
        assert_eq!(load(&core).err(), wrong(ctor, got as usize, want));
    }
    // No field, one that it does not have, one twice and two out of order.
    for (variant, fields) in [
        (2, &[][..]),
        (2, &[2]),
        (2, &[0, 2]),
        (2, &[0, 0]),
        (2, &[1, 0]),
        (0, &[0]),
        (0, &[]),
        (9, &[9]),
    ] {
        let ctor = with_fields(variant);
        let odd = Some(Unfit::Broken(Broken::OpenFields(ctor)));
        assert_eq!(
            load(&opens(ctor, fields)).err(),
            odd,
            "{variant} {fields:?}"
        );
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
        assert_eq!(load(&matches(ctor)).err(), unlisted());
        assert_eq!(load(&opens(ctor, &[0])).err(), unlisted());
    }
    let ins = load(&matches(with_fields(3))).expect("it loads");
    let [Ins::JumpUnlessTag { tag, .. }, ..] = ins[..] else {
        panic!("{ins:?}");
    };
    assert_eq!(tag.parts(), (3, true));
    let ins = load(&matches(with_fields(0))).expect("it loads");
    let [Ins::JumpUnlessTag { tag, .. }, ..] = ins[..] else {
        panic!("{ins:?}");
    };
    assert_eq!(tag.parts(), (0, false));
    let ins = load(&opens(with_fields(4), &[0, 3])).expect("it loads");
    let [Ins::Open4 { dsts, src, ctor }, ..] = ins[..] else {
        panic!("{ins:?}");
    };
    assert_eq!(
        dsts.map(|d| d.dst()),
        [Some(Dst(VReg(1))), None, None, Some(Dst(VReg(4)))]
    );
    assert_eq!(
        (src, source.ctors.name(ctor)),
        (Taken(VReg(0)), Some(with_fields(4)))
    );
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
    let draft = translate(&source, core, Some(FuncIdx(0)), &mut Tables::default());
    draft.map(|d| instructions(&d).iter().map(Ins::op).collect())
}

/// A constant that a module keeps in a global is made and stored there, also
/// when the other operator that reads it holds it. The compiler reads a global
/// back with a load of its own, so this makes the read by hand.
#[test]
fn a_constant_kept_in_a_global_is_stored() {
    let program = program("const kept = 7\npub fn main() { Nil }\n");
    let CoreExpr::Let {
        bind,
        rhs,
        body: stored,
    } = &program.toplevel.core.body
    else {
        panic!("the toplevel binds the constant first");
    };
    let CoreExpr::SetGlobal { slot, value, .. } = **stored else {
        panic!("and stores it straight after");
    };
    assert!(value == bind.id && matches!(rhs, Atom::Const(_)));
    let x = CoreBind::new(LocalId(bind.id.0 + 1), bind.ty);
    let loaded_with = |keeps: bool| -> Vec<Op> {
        let sum = Atom::PrimOp {
            op: PrimOp::IntAdd,
            args: vec![x.id, bind.id],
        };
        let mut rest = returning(sum);
        if keeps {
            rest = CoreExpr::SetGlobal {
                slot,
                value,
                body: Box::new(rest),
            };
        }
        let core = CoreFn {
            params: vec![x],
            body: CoreExpr::Let {
                bind: *bind,
                rhs: rhs.clone(),
                body: Box::new(rest),
            },
            ret_ty: program.toplevel.core.ret_ty,
        };
        let mut with = program.clone();
        with.toplevel.core = core;
        let code = load(&with, Audit::Never).expect("the program loads");
        let top = code.toplevels.last().expect("a toplevel");
        body(&code, top).iter().map(Ins::op).collect()
    };
    let stored = [Op::Const, Op::SetGlobal, Op::IntAddK, Op::Ret];
    assert_eq!(loaded_with(true), stored);
    assert_eq!(loaded_with(false), [Op::IntAddK, Op::Ret]);
}

/// An `if` on a comparison of numbers, read nowhere else, is one jump. One
/// whose answer is read again is stored first.
#[test]
fn a_branch_on_a_comparison_is_one_jump() {
    let ops = loaded(
        "import scarlet/array\n\
         fn lt(a Int, b Int) Int { if a < b { a } else { b } }\n\
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
         fn written(a Int, xs Array(Bool)) Array(Bool) {\n\
         \tc = a > 4\n\
         \tif c { array::put(xs, 0, c) } else { xs }\n\
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
    assert_eq!(ops["written"][..2], [Op::IntGtK, Op::JumpIfFalse]);
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

/// A draft says of each value what its type does.
#[test]
fn a_draft_says_what_its_values_types_say() {
    let program = program(
        "fn plain(n Int, x Float, s String, b Bool, xs Array(Int)) Int {\n\
         \tif b { plain(n, x, s, b, xs) } else { n }\n}\n\
         fn any(x a, n Int) a { if n > 0 { any(x, n - 1) } else { x } }\n\
         pub fn main() {\n\
         \tprintln(plain(1, 2.0, '', False, []))\n\
         \tprintln(any(1, 1))\n\
         }\n",
    );
    let source = Source::new(&program).expect("a whole program");
    let draft = |name: &str| {
        let mut all = (&program.fns).into_iter();
        let f = all.find(|f| f.name == name).expect("the function");
        let this = Some(FuncIdx(0));
        let d = translate(&source, &f.core, this, &mut Tables::default());
        let d = d.expect("it translates");
        assert_eq!(d.kinds.len(), d.values as usize);
        let kind = |p: &VReg| d.kinds.get(p.0 as usize).copied().flatten();
        d.params.iter().map(kind).collect::<Vec<_>>()
    };
    let (word, int) = (Some(Makes::NoCell), Some(Makes::Int));
    let any = Some(Makes::Any);
    let plain = vec![int, word, any, word, any];
    assert_eq!(draft("plain"), plain);
    assert_eq!(draft("any"), [any, int]);
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
        translate(&source, core, None, &mut Tables::default()).err()
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

/// A translator that has made nothing, of a function that reads its locals as `reads`
/// says.
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
        joins: HashMap::new(),
        words: Vec::new(),
        closure: ReadsClosure::Nothing,
        reads,
        numbers: HashMap::new(),
        misses: Vec::new(),
        no_arm: None,
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

fn switch(scrut: u32, cases: Cases, default: Option<CoreExpr>) -> CoreExpr {
    CoreExpr::Switch {
        scrut: LocalId(scrut),
        cases,
        default: default.map(Box::new),
    }
}

/// The draft of `core`, in a program that has the types the tests ask for.
fn drafted(core: &CoreFn) -> Draft {
    let mut program = program_with_fields();
    let plain = |n: usize| VariantNames {
        name: format!("V{n}"),
        fields: Vec::new(),
    };
    for (id, n) in [(ONE, 1), (TWO, 2), (EIGHT, 8), (MANY, 40_000)] {
        let name = format!("T{n}");
        let mut variants: Vec<_> = (0..n).map(plain).collect();
        // The first has a field, and nothing else does.
        variants[0].fields.push("f".to_string());
        program.types.insert(id, TypeNames { name, variants });
    }
    let source = Source::new(&program).expect("a whole program");
    let draft = translate(&source, core, Some(FuncIdx(0)), &mut Tables::default());
    let draft = draft.expect("it translates");
    crate::code::live::settle(&mut copy_of(&draft)).expect("it loads");
    draft
}

/// Types of one, two, eight and very many variants, the first of which alone has a
/// field.
const ONE: TypeId = TypeId(-11);
const TWO: TypeId = TypeId(-12);
const EIGHT: TypeId = TypeId(-13);
const MANY: TypeId = TypeId(-14);

/// `core`'s instructions, each with where it may jump as a number.
fn jumps(core: &CoreFn) -> Vec<String> {
    let draft = drafted(core);
    let place = |l: &Label| draft.labels[l.0 as usize].to_string();
    let line = |ins: &Ins<VReg>| {
        let to: Vec<String> = targets(ins, &draft.lists).iter().map(place).collect();
        let tag = match ins {
            Ins::JumpUnlessTag { tag, .. } => format!(" .{}", tag.variant()),
            Ins::JumpUnlessInt { n, .. } => format!(" {n}"),
            Ins::JumpUnlessFloat { x, .. } => format!(" {x:?}"),
            _ => String::new(),
        };
        let arrow = if to.is_empty() { "" } else { " -> " };
        format!("{}{tag}{arrow}{}", shown(ins, &draft.lists), to.join(" "))
    };
    instructions(&draft).iter().map(line).collect()
}

/// A function of four parameters that switches on the first, each case of which
/// returns what it is given.
fn switching(ty: TypeId, arms: &[(&[u16], u32)], default: Option<u32>) -> CoreFn {
    let arms = arms
        .iter()
        .map(|(of, l)| (of.to_vec(), gives(*l)))
        .collect();
    function(
        4,
        switch(0, Cases::Variant { ty, arms }, default.map(gives)),
    )
}

/// A switch on a variant with one outcome is nothing, and one with two is a test of
/// the one that a test can be for. With more it is a jump by a table, one entry for
/// each variant. What has no case goes to the default, when there is one.
#[test]
fn a_switch_is_a_test_or_a_table() {
    let made = |ty, arms: &[(&[u16], u32)], default| jumps(&switching(ty, arms, default));
    // One outcome, however it is written.
    assert_eq!(made(TWO, &[(&[0, 1], 1)], None), ["Ret 1"]);
    assert_eq!(made(TWO, &[(&[1, 0], 1)], Some(2)), ["Ret 1"]);
    assert_eq!(made(TWO, &[], Some(1)), ["Ret 1"]);
    assert_eq!(made(ONE, &[(&[0], 1)], None), ["Ret 1"]);
    // Two.
    let two = |tag: u16, yes: u32, no: u32| {
        [
            format!("JumpUnlessTag 0 .{tag} -> 2"),
            format!("Ret {yes}"),
            format!("Ret {no}"),
        ]
    };
    assert_eq!(made(TWO, &[(&[0], 1), (&[1], 2)], None), two(0, 1, 2));
    assert_eq!(made(TWO, &[(&[1], 1), (&[0], 2)], None), two(1, 1, 2));
    assert_eq!(made(TWO, &[(&[1], 1)], Some(2)), two(1, 1, 2));
    assert_eq!(made(EIGHT, &[(&[5], 1)], Some(2)), two(5, 1, 2));
    let most: &[u16] = &[0, 1, 2, 4, 5, 6, 7];
    assert_eq!(made(EIGHT, &[(most, 1), (&[3], 2)], None), two(3, 2, 1));
    // The one variant that no case names.
    assert_eq!(made(EIGHT, &[(most, 1)], Some(2)), two(3, 2, 1));
    assert_eq!(made(MANY, &[(&[39_999], 1)], Some(2)), two(39_999, 1, 2));
    // What no case names, and nothing is for, stops the run.
    let stops = made(TWO, &[(&[1], 1)], None);
    assert_eq!(stops, ["JumpUnlessTag 0 .1 -> 2", "Ret 1", "Bad"]);
    assert_eq!(made(TWO, &[], None), ["Bad"]);
    // Two of several each, and three.
    assert_eq!(
        made(EIGHT, &[(&[0, 1, 2], 1), (&[3, 4], 2)], Some(2))[0],
        "Switch 0 -> 1 1 1 2 2 3 3 3"
    );
    assert_eq!(
        made(EIGHT, &[(&[0, 1, 2, 3], 1), (&[4, 5, 6, 7], 2)], None),
        ["Switch 0 -> 1 1 1 1 2 2 2 2", "Ret 1", "Ret 2"]
    );
    assert_eq!(
        made(EIGHT, &[(&[7], 1), (&[0], 2)], Some(3)),
        ["Switch 0 -> 2 3 3 3 3 3 3 1", "Ret 1", "Ret 2", "Ret 3"]
    );
    assert_eq!(
        made(EIGHT, &[(&[7], 1), (&[0], 2), (&[1], 3)], None),
        [
            "Switch 0 -> 2 3 4 4 4 4 4 1",
            "Ret 1",
            "Ret 2",
            "Ret 3",
            "Bad"
        ]
    );
    // Of a type that has many more variants than there are cases, a test for each.
    assert_eq!(
        made(MANY, &[(&[7, 9, 11], 1), (&[0], 2)], Some(3)),
        [
            "JumpUnlessTag 0 .7 -> 2",
            "Jump -> 5",
            "JumpUnlessTag 0 .9 -> 4",
            "Jump -> 5",
            "JumpUnlessTag 0 .11 -> 6",
            "Ret 1",
            "JumpUnlessTag 0 .0 -> 8",
            "Ret 2",
            "Ret 3",
        ]
    );
}

/// A table is made when at least one entry in four has a case, or the type is small, so
/// that tables take little more room than cases do. Of 40,000 that is 9,998 cases and the
/// 8 entries there may be in any case.
#[test]
fn a_table_is_no_longer_than_its_cases_are_many() {
    let first = |named: u16| {
        let arms: Vec<(Vec<u16>, CoreExpr)> = (0..named).map(|v| (vec![v], gives(1))).collect();
        let cases = Cases::Variant { ty: MANY, arms };
        let core = function(4, switch(0, cases, Some(gives(2))));
        let draft = drafted(&core);
        (instructions(&draft)[0].op(), draft.lists.tables.len())
    };
    assert_eq!(first(9_997), (Op::JumpUnlessTag, 0));
    assert_eq!(first(9_998), (Op::Switch, 40_000));
    let small = |named: u16| {
        let arms: Vec<(&[u16], u32)> =
            [(&[0][..], 1), (&[1], 2), (&[2], 3)][..usize::from(named)].to_vec();
        drafted(&switching(EIGHT, &arms, Some(0)))
            .lists
            .tables
            .len()
    };
    assert_eq!([small(1), small(2), small(3)], [0, 8, 8]);
}

/// Where only constructors with no fields go, the value is known to be no cell.
#[test]
fn a_case_for_constructors_with_no_fields_is_noted() {
    let noted = |core: &CoreFn| {
        let draft = drafted(core);
        let mut at: Vec<_> = draft
            .words
            .iter()
            .map(|(l, v)| (draft.labels[l.0 as usize], v.0))
            .collect();
        at.sort_unstable();
        at
    };
    // Past a test that is not for the one with a field, and not past one that is.
    assert_eq!(noted(&switching(TWO, &[(&[0], 1)], Some(2))), [(2, 0)]);
    assert_eq!(noted(&switching(TWO, &[(&[1], 1)], Some(2))), []);
    assert_eq!(noted(&switching(EIGHT, &[(&[3], 1)], Some(2))), []);
    assert_eq!(noted(&switching(EIGHT, &[(&[0], 1)], Some(2))), [(2, 0)]);
    // In a table each case by itself, and the default.
    let arms: &[(&[u16], u32)] = &[(&[0, 1], 1), (&[2], 2), (&[3, 4], 3)];
    assert_eq!(
        noted(&switching(EIGHT, arms, Some(2))),
        [(2, 0), (3, 0), (4, 0)]
    );
    let arms: &[(&[u16], u32)] = &[(&[1], 1), (&[2], 2), (&[3, 4], 3)];
    assert_eq!(
        noted(&switching(EIGHT, arms, Some(2))),
        [(1, 0), (2, 0), (3, 0)]
    );
}

/// A switch on an Int with one key or two is a test for each, and so is one with a few
/// that are far from each other. One with more is a jump by a table. Floats and texts
/// are tested one after another.
#[test]
fn a_switch_on_a_literal_is_tests_or_a_table() {
    let ints = |arms: &[(&[i64], u32)], default| {
        let arms = arms
            .iter()
            .map(|(of, l)| (of.to_vec(), gives(*l)))
            .collect();
        jumps(&function(4, switch(0, Cases::Int(arms), default)))
    };
    let default = || Some(gives(3));
    assert_eq!(
        ints(&[(&[5], 1), (&[i64::MIN], 2)], default()),
        [
            "JumpUnlessInt 0 5 -> 2".to_string(),
            "Ret 1".to_string(),
            format!("JumpUnlessInt 0 {} -> 4", i64::MIN),
            "Ret 2".to_string(),
            "Ret 3".to_string(),
        ]
    );
    assert_eq!(
        ints(&[(&[5, 7], 1)], default()),
        [
            "JumpUnlessInt 0 5 -> 2",
            "Jump -> 3",
            "JumpUnlessInt 0 7 -> 4",
            "Ret 1",
            "Ret 3",
        ]
    );
    assert_eq!(
        ints(&[(&[5, 7], 1), (&[6], 2)], default()),
        ["SwitchInt 0 -> 1 2 1 3", "Ret 1", "Ret 2", "Ret 3"]
    );
    assert_eq!(
        ints(&[(&[5, 8], 1), (&[6], 2)], None),
        ["SwitchInt 0 -> 1 2 3 1 3", "Ret 1", "Ret 2", "Bad"]
    );
    // What is far from a run of them is tested after it.
    assert_eq!(
        ints(&[(&[5, 700], 1), (&[6, 7], 2)], default()),
        [
            "SwitchInt 0 -> 4 5 5 1",
            "JumpUnlessInt 0 700 -> 3",
            "Jump -> 4",
            "Jump -> 6",
            "Ret 1",
            "Ret 2",
            "Ret 3",
        ]
    );
    // In the order they are written, each with its arm behind it.
    assert_eq!(
        ints(
            &[
                (&[200], 1),
                (&[404], 2),
                (&[500], 1),
                (&[201], 2),
                (&[204], 1),
                (&[301, 302], 2),
            ],
            default()
        ),
        [
            "SwitchInt 0 -> 10 11 1 1 12 1",
            "JumpUnlessInt 0 404 -> 3",
            "Ret 2",
            "JumpUnlessInt 0 500 -> 5",
            "Ret 1",
            "JumpUnlessInt 0 301 -> 7",
            "Jump -> 8",
            "JumpUnlessInt 0 302 -> 9",
            "Ret 2",
            "Jump -> 13",
            "Ret 1",
            "Ret 2",
            "Ret 1",
            "Ret 3",
        ]
    );
    // And what is written in front of the run is tested in front of it.
    assert_eq!(
        ints(
            &[
                (&[1], 1),
                (&[1000], 2),
                (&[2000], 1),
                (&[2001, 2002], 2),
                (&[7], 1)
            ],
            default()
        ),
        [
            "JumpUnlessInt 0 1 -> 2",
            "Ret 1",
            "JumpUnlessInt 0 1000 -> 4",
            "Ret 2",
            "SwitchInt 0 -> 8 9 9 5",
            "JumpUnlessInt 0 7 -> 7",
            "Ret 1",
            "Jump -> 10",
            "Ret 1",
            "Ret 2",
            "Ret 3",
        ]
    );
    // Many that are far from each other are found among them.
    let far: Vec<i64> = (0..9).map(|i| i * 1000).collect();
    let made = ints(&[(&far, 1)], default());
    assert_eq!(
        made,
        ["SwitchIntFar 0 -> 1 1 1 1 1 1 1 1 1 2", "Ret 1", "Ret 3"]
    );
    let made = ints(&[(&far[..8], 1)], default());
    assert_eq!(made[..2], ["JumpUnlessInt 0 0 -> 2", "Jump -> 15"]);
    let floats = vec![
        (vec![FloatKey::new(-0.0), FloatKey::new(1.5)], gives(1)),
        (vec![FloatKey::new(2.0)], gives(2)),
    ];
    assert_eq!(
        jumps(&function(4, switch(0, Cases::Float(floats), default()))),
        [
            "JumpUnlessFloat 0 0.0 -> 2",
            "Jump -> 3",
            "JumpUnlessFloat 0 1.5 -> 4",
            "Ret 1",
            "JumpUnlessFloat 0 2.0 -> 6",
            "Ret 2",
            "Ret 3",
        ]
    );
    // A text is told by its constant, which has to be one.
    let mut program = empty_program();
    program.consts = vec![Const::String("a".to_string()), Const::Int(1)];
    let source = Source::new(&program).expect("a whole program");
    let texts = |id| {
        let arms = vec![(vec![ConstId(id)], gives(1))];
        let core = function(4, switch(0, Cases::Text(arms), default()));
        let mut tables = Tables::default();
        let draft = translate(&source, &core, None, &mut tables);
        let ops = draft.map(|d| instructions(&d).iter().map(Ins::op).collect::<Vec<_>>());
        (ops, tables.texts)
    };
    let (ops, text) = texts(0);
    assert_eq!(ops, Ok(vec![Op::JumpUnlessStr, Op::Ret, Op::Ret]));
    assert_eq!(text, [b"a".to_vec().into_boxed_slice()]);
    // One that is another kind of constant, and one that is not there.
    let other = Unfit::Broken(Broken::TextCase(ConstId(1)));
    assert_eq!(texts(1).0, Err(other));
    let missing = Unfit::Missing(Missing::NoConst(2));
    assert_eq!(texts(2).0, Err(missing));
}

/// Whatever the numbers a switch on an Int has cases for, each goes to its own and any
/// other to the default, and the tables have few entries for nothing. Numbers that are
/// near each other are found in one look.
#[test]
fn every_int_goes_to_its_own_case() {
    let every = |n: i64, step: i64| (0..n).map(|i| i * step).collect::<Vec<_>>();
    let all: Vec<Vec<i64>> = vec![
        vec![],
        vec![0],
        every(4, 1),
        vec![-3, -2, 0, 1],
        vec![10, 12, 13, 1000, 1001, 1002, 1004],
        every(6, 3),
        every(5, 4),
        every(5, 5),
        vec![i64::MIN, -1, 0, 1, i64::MAX],
        vec![i64::MIN, i64::MIN + 1, i64::MIN + 2],
        vec![i64::MAX - 2, i64::MAX - 1, i64::MAX],
        every(9, 100_000),
        every(8, 100_000),
        vec![0, 1 << 50, (1 << 50) + 1, (1 << 50) + 2, 1 << 62],
        [every(5, 1), every(20, 1000)].concat()[..]
            .iter()
            .skip(1)
            .copied()
            .collect(),
        vec![
            i64::from(i32::MAX) - 1,
            i64::from(i32::MAX),
            i64::from(i32::MAX) + 1,
        ],
        every(50_000, 1),
    ];
    let orders = all.into_iter().flat_map(|mut keys| {
        keys.sort_unstable();
        keys.dedup();
        let reversed = keys.iter().rev().copied().collect();
        let mut turned = keys.clone();
        turned.rotate_left(keys.len() / 2);
        [keys, reversed, turned]
    });
    for keys in orders {
        let arms = keys
            .iter()
            .zip(0u32..)
            .map(|(n, i)| (vec![*n], gives(1 + i)));
        let cases = Cases::Int(arms.collect());
        let count = u32::try_from(keys.len()).expect("a few");
        let core = function(1, switch(0, cases, Some(gives(0))));
        let program = empty_program();
        let source = Source::new(&program).expect("a whole program");
        let draft = translate(&source, &core, None, &mut Tables::default());
        let draft = draft.expect("it translates");
        let ins = instructions(&draft);
        let place = |l: Label| draft.labels[l.0 as usize];
        // Where a run goes on from the start for `n`, and how many steps that takes.
        let goes = |n: i64| {
            let (mut at, mut steps) = (0, 0);
            loop {
                steps += 1;
                at = match &ins[at] {
                    Ins::Ret { src, .. } => return (src.0.0, steps),
                    Ins::Jump { to } => place(*to),
                    Ins::JumpUnlessInt { n: k, to, .. } if *k != n => place(*to),
                    Ins::JumpUnlessInt { .. } => at + 1,
                    Ins::SwitchInt { table, .. } => {
                        let last = i128::from(table.table.last);
                        let past = i128::from(n) - i128::from(table.first);
                        let i = if past < 0 { last } else { past.min(last) };
                        let first = usize::try_from(table.table.first).expect("a few");
                        place(draft.lists.tables[first + usize::try_from(i).expect("a few")])
                    }
                    Ins::SwitchIntFar { places, to, .. } => {
                        // As the VM looks for it.
                        let all = places.places(&draft.lists);
                        let found = all.binary_search_by_key(&n, |(k, _)| *k);
                        place(found.map_or(*to, |at| all[at].1))
                    }
                    other => panic!("{other:?}"),
                };
            }
        };
        let step = (keys.len() / 50).max(1);
        let near = |n: &i64| [n.saturating_sub(1), *n, n.saturating_add(1)];
        let others = [i64::MIN, -5, 0, 4, 500, 5_000_000_000, i64::MAX];
        for n in keys.iter().step_by(step).flat_map(near).chain(others) {
            let want = keys.iter().zip(1u32..).find(|(k, _)| **k == n);
            let (got, steps) = goes(n);
            assert_eq!(got, want.map_or(0, |(_, i)| i), "{n} of {count}");
            // A test of each would take one step for each written up to it, and one more.
            let tests = want.map_or(count, |(_, i)| i) + 1;
            assert!(steps <= tests.clamp(3, 14), "{n} of {count} takes {steps}");
        }
        assert!(draft.lists.tables.len() <= keys.len() * 4 + 1);
    }
    // Numbers in a row are found in one look, whatever is far from them.
    let row: Vec<i64> = (100..160).chain([1 << 60]).collect();
    assert_eq!(near(&row), 0..60);
    assert_eq!(near(&[0, 100, 200, 201, 202]), 2..5);
    assert_eq!(near(&every(4, 4)), 0..4);
    assert_eq!(near(&every(4, 6)), 0..2);
    assert_eq!(near(&[]), 0..0);
    // A run is no longer than an instruction can say.
    assert_eq!(near(&every(70_000, 1)), 0..60_000);
}

/// A switch that names a value twice, or has a case for none, is refused whatever it is
/// on.
#[test]
fn a_switch_names_each_value_once() {
    let made = |cases: &dyn Fn(Vec<Vec<u8>>) -> Cases| {
        [
            vec![vec![0, 1], vec![2]],
            vec![vec![0], vec![1, 0]],
            vec![vec![1, 1]],
            vec![vec![2], vec![]],
        ]
        .map(|keys| {
            let core = function(4, switch(0, cases(keys), Some(gives(3))));
            let mut program = empty_program();
            let text = |c: char| Const::String(c.to_string());
            program.consts = "abc".chars().map(text).collect();
            let variants = vec![
                VariantNames {
                    name: String::new(),
                    fields: Vec::new(),
                };
                8
            ];
            let name = String::new();
            program.types.insert(EIGHT, TypeNames { name, variants });
            let source = Source::new(&program).expect("a whole program");
            let draft = translate(&source, &core, None, &mut Tables::default());
            draft.map(|_| ()).err()
        })
    };
    fn arms<K>(keys: Vec<Vec<u8>>, of: impl Fn(u8) -> K) -> Arms<K> {
        let arm = |keys: Vec<u8>| (keys.into_iter().map(&of).collect(), gives(1));
        keys.into_iter().map(arm).collect()
    }
    let bad = || Some(Unfit::Broken(Broken::Cases));
    let want = [None, bad(), bad(), bad()];
    assert_eq!(made(&|k| Cases::Int(arms(k, i64::from))), want);
    let float = |x| FloatKey::new(f64::from(x));
    assert_eq!(made(&|k| Cases::Float(arms(k, float))), want);
    let text = |c| ConstId(u32::from(c));
    assert_eq!(made(&|k| Cases::Text(arms(k, text))), want);
    let variants = |k| Cases::Variant {
        ty: EIGHT,
        arms: arms(k, u16::from),
    };
    assert_eq!(made(&variants), want);
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

/// Every number of fields is taken apart, those that do not fit in the
/// instruction through a list. A field that is not read has no register.
#[test]
fn a_constructor_of_any_number_of_fields_is_taken_apart() {
    for n in 1..=10u32 {
        for skip in 0..=n {
            let read = |i: &u32| *i != skip;
            let fields: Vec<_> = (1..=n)
                .filter(read)
                .map(|i| (u16::try_from(i - 1).unwrap(), bind(i)))
                .collect();
            if fields.is_empty() {
                continue;
            }
            let kept = fields.iter().map(|(_, b)| b.id).collect();
            let open = CoreExpr::Open {
                cell: LocalId(0),
                variant: with_fields(n as usize),
                fields,
                body: Box::new(returning(Atom::prim(PrimOp::MakeTuple, kept))),
            };
            let draft = drafted(&function(1, open));
            let got = shown(&instructions(&draft)[0], &draft.lists);
            let dst = |i: u32| {
                if read(&i) {
                    i.to_string()
                } else {
                    "_".to_string()
                }
            };
            let dsts: Vec<_> = (1..=n).map(dst).collect();
            let name = if n <= 8 {
                format!("Open{n}")
            } else {
                "OpenWide".to_string()
            };
            assert_eq!(got, format!("{} {name} 0", dsts.join(" ")));
        }
    }
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
    let update = |n: usize| Atom::Update {
        base: LocalId(4),
        variant: with_fields(10),
        sets: (0..).zip(list(n)).collect(),
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
        (prim(PrimOp::ArrayPrepend, 2), Op::ArrayPush),
        (prim(PrimOp::ArrayPrepend, 3), Op::ArrayExtend),
        (prim(PrimOp::ArrayAppend, 2), Op::ArrayPush),
        (prim(PrimOp::ArrayAppend, 3), Op::ArrayExtend),
        (prim(PrimOp::ArrayAppend, 40), Op::ArrayExtend),
        (ctor(8), Op::Ctor),
        (ctor(9), Op::CtorWide),
        (update(1), Op::Update1),
        (update(2), Op::Update2),
        (update(3), Op::Update3),
        (update(4), Op::UpdateWide),
        (update(10), Op::UpdateWide),
        (closure(8), Op::Closure),
        (closure(9), Op::ClosureWide),
        (call(Callee::Local(LocalId(0)), 6), Op::CallValue),
        (call(Callee::Local(LocalId(0)), 7), Op::CallValueWide),
    ];
    for (atom, want) in cases {
        let mut reads = Vec::new();
        atom.for_each_operand(|l| reads.push(of(l)));
        reads.sort();
        let core = function(5, returning(atom));
        let program = program_with_fields();
        let source = Source::new(&program).expect("a whole program");
        let draft = translate(&source, &core, Some(FuncIdx(0)), &mut Tables::default())
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
    let tail = |n| {
        let callee = Callee::Local(LocalId(0));
        let args = list(n);
        function(5, CoreExpr::TailCall { callee, args })
    };
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
        let draft = translate(&source, core, this, &mut tables).expect("it translates");
        assert_eq!(instructions(&draft).first().map(Ins::op), Some(Op::Bad));
        tables.whys.remove(0)
    };
    let call = |callee, n: u32| {
        let args = (0..n).map(LocalId).collect();
        function(n, CoreExpr::TailCall { callee, args })
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
    let load = |load| function(0, returning(Atom::Load(load)));
    let capture = |i| load(Load::Capture(scarlet_ir::core_ir::CaptureIdx(i)));
    let toplevel = Why::Broken(Broken::ToplevelCapture);
    assert_eq!(why(&capture(0), None), toplevel);
    let negative = Why::Broken(Broken::NegativeCapture);
    assert_eq!(why(&capture(-1), Some(main)), negative);
    let itself = Why::Broken(Broken::ToplevelSelf);
    assert_eq!(why(&load(Load::SelfClosure), None), itself);
    let undeclared = || Why::Broken(Broken::UndeclaredJoin);
    let wild = function(0, jump(3, &[]));
    assert_eq!(why(&wild, Some(main)), undeclared());
    // A join is gone once its own code has been reached, for its continuation
    // too.
    let goes_back = function(0, join(3, &[], jump(3, &[]), jump(3, &[])));
    let mut tables = Tables::default();
    translate(&source, &goes_back, None, &mut tables).expect("it translates");
    assert_eq!(tables.whys, [undeclared()]);
    // A jump passes a value for each parameter and no more.
    for (got, want) in [(0, 1), (2, 1), (1, 0), (1, 3)] {
        let (args, params) = (vec![0; got], (1..=want).collect::<Vec<_>>());
        let body = join(3, &params, jump(3, &args), gives(0));
        let want = want as usize;
        let wrong = Why::Broken(Broken::JumpArity { got, want });
        assert_eq!(why(&function(1, body), Some(main)), wrong);
    }
}

/// A built-in the VM does not run, or is called with another number of
/// arguments, leaves the function waiting for it.
#[test]
fn a_built_in_with_the_wrong_arguments_is_missing() {
    let call = |intrinsic, n: u32| {
        let args = (0..n).map(LocalId).collect();
        translated(&function(n, returning(Atom::Intrinsic { intrinsic, args })))
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

/// An update of a constructor that the program does not list, at a field that it
/// does not have, at none, or at fields that are not in order, is in a function
/// that does not load.
#[test]
fn an_update_that_names_no_field_does_not_load() {
    let program = program_with_fields();
    let source = Source::new(&program).expect("a whole program");
    let load = |variant, sets: &[u16]| {
        let sets = sets.iter().map(|field| (*field, LocalId(1))).collect();
        let core = function(
            2,
            returning(Atom::Update {
                base: LocalId(0),
                variant,
                sets,
            }),
        );
        let this = Some(FuncIdx(0));
        translate(&source, &core, this, &mut Tables::default()).err()
    };
    let three = with_fields(3);
    for sets in [&[0][..], &[2], &[0, 1], &[0, 2], &[0, 1, 2]] {
        assert_eq!(load(three, sets), None, "{sets:?}");
    }
    let wrong = |variant| Some(Unfit::Broken(Broken::UpdateFields(variant)));
    for sets in [
        &[][..],
        &[3],
        &[0, 3],
        &[1, 0],
        &[1, 1],
        &[0, 2, 1],
        &[u16::MAX],
    ] {
        assert_eq!(load(three, sets), wrong(three), "{sets:?}");
    }
    let plain = with_fields(0);
    assert_eq!(load(plain, &[0]), wrong(plain));
    let past = with_fields(11);
    assert_eq!(load(past, &[0]), Some(Unfit::Broken(Broken::NoCtor(past))));
}

/// An update takes its base and its values where nothing reads them again, and
/// its result has the register of the base. A base that is set in one of its own
/// fields is not taken, and of two fields set to one value the last takes it.
#[test]
fn an_update_takes_what_is_not_read_again() {
    let code = code(
        "type P {\n\tP(a Int, b Int, c Int)\n}\n\
         type W {\n\tW(next Option(W), n Int)\n}\n\
         fn taken(p P, x Int) P { P(..p, b: x) }\n\
         fn kept(p P, x Int) (P, P) { (P(..p, b: x), p) }\n\
         fn shared(p P, x Int) (P, Int) { (P(..p, b: x), x) }\n\
         fn twice(p P, x Int) P { P(..p, a: x, c: x) }\n\
         fn own(p P) P { P(..p, b: p.b) }\n\
         fn chain(p P) P {\n\
         \tq = P(..p, a: 1)\n\
         \tP(..q, b: 2)\n\
         }\n\
         fn branch(p P, x Int) P {\n\
         \tif x > 1 { P(..p, b: x) } else { p }\n\
         }\n\
         fn loops(p P, n Int) P {\n\
         \tif n == 0 { p } else { loops(P(..p, b: p.a + 1), n - 1) }\n\
         }\n\
         pub fn main() {\n\
         \tp = P(1, 2, 3)\n\
         \tprintln(taken(p, 1))\n\
         \tprintln(kept(p, 1))\n\
         \tprintln(shared(p, 1))\n\
         \tprintln(twice(p, 1))\n\
         \tprintln(own(p))\n\
         \tprintln(chain(p))\n\
         \tprintln(branch(p, 1))\n\
         \tprintln(loops(p, 1))\n\
         \tprintln(W(None, 1))\n\
         }\n",
    );
    // For each update: whether it takes its base and each value, and whether it
    // writes the register of the base.
    let updates = |name: &str| -> Vec<(Vec<bool>, bool)> {
        let ins = body(&code, named(&code, name)).iter();
        let updates = ins.filter(|i| i.op().name().starts_with("Update"));
        updates
            .map(|i| {
                let mut all = Vec::new();
                i.operands(&code.lists, &mut |reg, role| all.push((reg, role)));
                let passed = all[1..].iter();
                let moved = passed.map(|(_, role)| *role == Role::Pass { moved: true });
                (moved.collect(), all[0].0 == all[1].0)
            })
            .collect()
    };
    let all = |n: usize| (vec![true; n], true);
    assert_eq!(updates(".taken"), [all(2)]);
    assert_eq!(updates(".kept"), [(vec![false, true], false)]);
    assert_eq!(updates(".shared"), [(vec![true, false], true)]);
    assert_eq!(updates(".twice"), [(vec![true, false, true], true)]);
    assert_eq!(updates(".own"), [all(2)]);
    assert_eq!(updates(".chain"), [all(2), all(2)]);
    assert_eq!(updates(".branch"), [all(2)]);
    assert_eq!(updates(".loops"), [all(2)]);
}

/// A base that is one of its own new fields is not taken, so it is copied and
/// never made to hold itself.
#[test]
fn a_base_that_is_set_in_itself_is_not_taken() {
    let program = program_with_fields();
    let source = Source::new(&program).expect("a whole program");
    let mut core = function(
        2,
        returning(Atom::Update {
            base: LocalId(0),
            variant: with_fields(3),
            sets: vec![(0, LocalId(0)), (2, LocalId(0))],
        }),
    );
    core.ret_ty = RTy(0);
    let this = Some(FuncIdx(0));
    let mut draft = translate(&source, &core, this, &mut Tables::default()).expect("it translates");
    super::super::live::settle(&mut draft).expect("a function that loads");
    let mut passed = Vec::new();
    instructions(&draft)[0].operands(&draft.lists, &mut |v, role| passed.push((v.0, role)));
    let pass = |moved| Role::Pass { moved };
    assert_eq!(
        passed[1..],
        [(0, pass(false)), (0, pass(false)), (0, pass(true))]
    );
}

/// A long chain of joins, whose continuations are each in the last one's, takes no
/// stack for its length, and what is found out about it grows with its length. One
/// value is held over them all, and each branch gives a value of its own.
#[test]
fn a_long_chain_of_joins_is_gone_over_in_a_loop() {
    let chain = |n: u32| -> (usize, usize, usize) {
        let mut rest = gives(0);
        for i in (0..n).rev() {
            let (then, els, param) = (3 * i + 2, 3 * i + 3, 3 * i + 4);
            let branch = |at: u32| CoreExpr::Let {
                bind: bind(at),
                rhs: Atom::Nil,
                body: Box::new(jump(i, &[at])),
            };
            let body = CoreExpr::If {
                cond: LocalId(1),
                then: Box::new(branch(then)),
                els: Box::new(branch(els)),
            };
            rest = join(i, &[param], body, rest);
        }
        let mut core = function(2, rest);
        let program = empty_program();
        let source = Source::new(&program).expect("a whole program");
        let mut tables = Tables::default();
        let mut d = translate(&source, &core, None, &mut tables).expect("it translates");
        let steps = d.steps.len();
        let held = crate::code::live::settle(&mut d).expect("it loads");
        let clashes = held.clashes.len();
        let f = crate::code::alloc::allocate(d, &held).expect("it fits");
        // Freed from the outside in, because as a tree it is deeper than the stack.
        let mut e = std::mem::replace(&mut core.body, gives(0));
        while let CoreExpr::Join { cont, .. } = e {
            e = *cont;
        }
        assert_eq!(
            f.own.len(),
            5,
            "the value, the condition and what a join gives"
        );
        (steps, clashes, f.steps.len())
    };
    let small = std::thread::Builder::new().stack_size(512 * 1024);
    let run = small.spawn(move || (chain(100), chain(10_000)));
    let (short, long) = run.expect("a thread").join().expect("it ends");
    assert!(
        long.0 <= 100 * short.0 && long.1 <= 101 * short.1,
        "{short:?} {long:?}"
    );
    // Every jump's `Move` is of one register, and goes.
    assert!(long.2 <= 40_001, "{long:?}");
}

/// A jump is a move for each parameter, in order, and a jump, and its arguments
/// take the parameters' registers, so that nothing is left of it but the jump. One
/// that the continuation reads too, or that two parameters are given, is shared.
#[test]
fn a_jump_gives_each_parameter_its_value() {
    let shown_of = |core: &CoreFn| -> (Vec<String>, Vec<Op>) {
        let program = program_with_fields();
        let source = Source::new(&program).expect("a whole program");
        let draft = translate(&source, core, None, &mut Tables::default());
        let mut draft = draft.expect("it translates");
        let made = instructions(&draft)
            .iter()
            .map(|i| shown(i, &draft.lists))
            .collect();
        let held = crate::code::live::settle(&mut draft).expect("it loads");
        let f = crate::code::alloc::allocate(draft, &held).expect("it fits");
        let left = f.steps.iter().map(|step| match step {
            Step::Ins(ins) => ins.op(),
            Step::TailCall { .. } => panic!("{step:?}"),
        });
        (made, left.collect())
    };
    let tuple = |of: &[u32]| {
        returning(Atom::prim(
            PrimOp::MakeTuple,
            of.iter().copied().map(LocalId).collect(),
        ))
    };
    let branch = |then: CoreExpr, els: CoreExpr| CoreExpr::If {
        cond: LocalId(2),
        then: Box::new(then),
        els: Box::new(els),
    };
    let make = |dst: u32, rest: CoreExpr| CoreExpr::Let {
        bind: bind(dst),
        rhs: Atom::prim(PrimOp::ToString, vec![LocalId(0)]),
        body: Box::new(rest),
    };
    // Each branch makes the two values.
    let one = make(3, make(4, jump(1, &[3, 4])));
    let two = make(5, make(6, jump(1, &[6, 5])));
    let core = function(3, join(1, &[7, 8], branch(one, two), tuple(&[7, 8])));
    let (made, left) = shown_of(&core);
    assert_eq!(made[3..6], ["7 Move 3", "8 Move 4", "Jump"], "{made:?}");
    assert_eq!(made[8..11], ["7 Move 6", "8 Move 5", "Jump"], "{made:?}");
    assert!(!left.contains(&Op::Move), "{left:?}");
    // The continuation reads the argument too, so one move shares it.
    let core = function(
        3,
        join(
            1,
            &[7],
            branch(jump(1, &[0]), jump(1, &[1])),
            tuple(&[7, 0]),
        ),
    );
    let (_, left) = shown_of(&core);
    assert_eq!(
        left.iter().filter(|op| **op == Op::Move).count(),
        1,
        "{left:?}"
    );
    // One value for two parameters is in two registers.
    let core = function(3, join(1, &[7, 8], jump(1, &[0, 0]), tuple(&[7, 8])));
    let (_, left) = shown_of(&core);
    assert_eq!(
        left.iter().filter(|op| **op == Op::Move).count(),
        1,
        "{left:?}"
    );
    // Two of the function's parameters, each on one way: one of them is moved.
    let core = function(
        3,
        join(1, &[7], branch(jump(1, &[0]), jump(1, &[1])), tuple(&[7])),
    );
    let (_, left) = shown_of(&core);
    assert_eq!(
        left.iter().filter(|op| **op == Op::Move).count(),
        1,
        "{left:?}"
    );
}

fn probe(
    op: ProbeOp,
    args: &[u32],
    binds: &[Option<u32>],
    hit: CoreExpr,
    miss: CoreExpr,
) -> CoreExpr {
    CoreExpr::Probe {
        op,
        args: args.iter().copied().map(LocalId).collect(),
        binds: binds.iter().map(|found| found.map(bind)).collect(),
        hit: Box::new(hit),
        miss: Box::new(miss),
    }
}

/// How many operands each probe has.
const PROBES: [(ProbeOp, usize); 4] = [
    (ProbeOp::ArrayGet, 2),
    (ProbeOp::ArraySlice, 3),
    (ProbeOp::ArraySet, 3),
    (ProbeOp::MapGet, 2),
];

/// A probe is one instruction, with what a hit goes on with behind it and then what a
/// miss jumps to. One whose value nothing reads writes none, and a write that nothing
/// reads only asks whether there is the index.
#[test]
fn a_probe_is_one_instruction() {
    let made = |op, args: &[u32], found: Option<u32>| -> Vec<String> {
        let core = function(4, probe(op, args, &[found], gives(0), gives(1)));
        let program = program_with_fields();
        let source = Source::new(&program).expect("a whole program");
        let draft = translate(&source, &core, None, &mut Tables::default());
        let draft = draft.expect("it translates");
        let [get, ..] = instructions(&draft)[..] else {
            panic!("no instruction")
        };
        let mut to = Vec::new();
        get.targets(&draft.lists, &mut |l| to.push(draft.labels[l.0 as usize]));
        assert_eq!(to, [2], "{op:?}");
        assert!(get.falls_through());
        crate::code::live::settle(&mut copy_of(&draft)).expect("it loads");
        let all = instructions(&draft);
        all.iter().map(|i| shown(i, &draft.lists)).collect()
    };
    let ends = |first: &str| vec![first.to_string(), "Ret 0".into(), "Ret 1".into()];
    for (op, args, found, want) in [
        (ProbeOp::ArrayGet, &[0, 1][..], Some(9), "9 ArrayGet 0 1"),
        (
            ProbeOp::ArraySlice,
            &[0, 1, 2],
            Some(9),
            "9 ArraySlice 0 1 2",
        ),
        (ProbeOp::ArraySet, &[0, 1, 2], Some(9), "9 ArraySet 0 1 2"),
        (ProbeOp::MapGet, &[0, 1], Some(9), "9 MapGet 0 1"),
        (ProbeOp::ArrayGet, &[2, 2], Some(9), "9 ArrayGet 2 2"),
        (ProbeOp::ArrayGet, &[0, 1], None, "ArrayGet 0 1"),
        (ProbeOp::ArraySlice, &[0, 1, 2], None, "ArraySlice 0 1 2"),
        (ProbeOp::ArraySet, &[0, 1, 2], None, "ArrayGet 0 1"),
        (ProbeOp::MapGet, &[0, 1], None, "MapGet 0 1"),
    ] {
        assert_eq!(made(op, args, found), ends(want));
    }
}

/// A probe with other than its operands, or with other than a place for the one value it
/// finds, does not load, and says what it was.
#[test]
fn a_probe_of_other_things_does_not_load() {
    let program = program_with_fields();
    let source = Source::new(&program).expect("a whole program");
    for (op, want) in PROBES {
        for args in 0..5 {
            for binds in 0..3 {
                for read in [false, true] {
                    let all: Vec<u32> = (0..u32::try_from(args).expect("few")).collect();
                    let found = vec![read.then_some(9); binds];
                    let body = probe(op, &all, &found, gives(0), gives(1));
                    let core = function(5, body);
                    let got = translate(&source, &core, None, &mut Tables::default());
                    let right = args == want && binds == 1;
                    let why = Broken::Probe { op, args, binds };
                    assert_eq!(got.is_ok(), right, "{op:?} {args} {binds}");
                    if let Err(got) = got {
                        assert_eq!(got, Unfit::Broken(why.clone()));
                        let crate::Stop::BadProgram(why) = crate::Stop::broken(why) else {
                            panic!("a bad program");
                        };
                        let want = format!("{op:?} of {args} values, which finds {binds}");
                        assert_eq!(why.to_string(), want);
                    }
                }
            }
        }
    }
    // A function with one is a stop where it is called, and no other is.
    let mut program = crate::code::tests::program(
        "fn f(xs Array(Int)) Int { xs[0] or 1 }\npub fn main() { println(f([1])) }\n",
    );
    program.fns = program
        .fns
        .into_iter()
        .map(|mut f| {
            if f.name == "f" {
                f.core.body = probe(ProbeOp::MapGet, &[0], &[None], gives(0), gives(0));
            }
            f
        })
        .collect();
    let code = load(&program, Audit::Never).expect("the program loads");
    let [Ins::Bad { why }] = body(&code, named(&code, ".f")) else {
        panic!("one instruction");
    };
    let (op, args, binds) = (ProbeOp::MapGet, 1, 1);
    let want = crate::Stop::broken(Broken::Probe { op, args, binds });
    assert_eq!(code.why(*why), want);
    assert!(body(&code, named(&code, ".main")).len() > 1);
}

/// The built-ins that are probes are nothing else to the loader.
#[test]
fn a_built_in_that_is_a_probe_is_no_call() {
    let program = program_with_fields();
    let source = Source::new(&program).expect("a whole program");
    for (intrinsic, n) in [
        (Intrinsic::MapGet, 2),
        (Intrinsic::ArraySet, 3),
        (Intrinsic::ArrayPut, 3),
    ] {
        let args = (0..n).map(LocalId).collect();
        let core = function(3, returning(Atom::Intrinsic { intrinsic, args }));
        let got = translate(&source, &core, None, &mut Tables::default()).map(|_| ());
        assert_eq!(got, Err(Unfit::Missing(Missing::Intrinsic(intrinsic))));
    }
}

/// What a probe finds takes the register of what dies there: its array's, when a hit
/// does not read that again, whatever a miss does with it. A drop behind the hit is then
/// none, because the write gives up what was there. The one of a miss stays.
#[test]
fn what_a_probe_finds_has_the_register_of_what_dies() {
    let text = |src: &str| {
        let src =
            format!("import scarlet/array\nimport scarlet/map\n{src}pub fn main() {{ Nil }}\n");
        let program = crate::code::tests::program(&src);
        let at = (&program.fns).into_iter().position(|f| f.name == "f");
        let listing = crate::listing(&program).expect("it loads");
        let f = listing.function(at.expect("f")).expect("f");
        let lines = f.lines().skip(1).map(|l| l[5..].trim().to_string());
        lines.collect::<Vec<_>>()
    };
    // One write, and nothing for the miss but to go on.
    let strike = "fn f(flags Array(Bool), k Int, step Int, size Int) Array(Bool) {\n\
                  \tif k <= size {\n\
                  \t\tf(array::put(flags, k - 1, False), k + step, step, size)\n\
                  \t} else {\n\
                  \t\tflags\n\
                  \t}\n\
                  }\n";
    assert_eq!(
        text(strike),
        [
            "JumpUnlessIntLe(r3, r5) -> 6",
            "r6 = IntSubK(r3, 1)",
            "r7 = Const(false)",
            "r2 = ArraySet(move r2, r6, move r7) -> 4",
            "r3 = IntAdd(r3, r4)",
            "Jump -> 0",
            "Ret(move r2)",
        ]
    );
    // The same, when the miss has something to do. That is out of the loop.
    let checked = strike.replace(
        "array::put(flags, k - 1, False)",
        "array::set(flags, k - 1, False) or [True]",
    );
    assert_eq!(
        text(&checked),
        [
            "JumpUnlessIntLe(r3, r5) -> 6",
            "r6 = IntSubK(r3, 1)",
            "r7 = Const(false)",
            "r2 = ArraySet(move r2, r6, move r7) -> 7",
            "r3 = IntAdd(r3, r4)",
            "Jump -> 0",
            "Ret(move r2)",
            "Drop(r2)",
            "r2 = Const(true)",
            "r2 = Array(r2)",
            "Jump -> 4",
        ]
    );
    // Reads in one another's hits, each with a miss that is out of the way, and a miss
    // that has a read of its own.
    let nested = "fn f(xs Array(Int), i Int) Int {\n\
                  \ta = match xs[i] {\n\
                  \t\tSome(x) -> match xs[x] {\n\
                  \t\t\tSome(y) -> y\n\
                  \t\t\tNone -> 2\n\
                  \t\t}\n\
                  \t\tNone -> { xs[i + 1] or 4 } * 5\n\
                  \t}\n\
                  \ta + 1\n\
                  }\n";
    let hits = text(nested);
    let hits = hits.iter().take_while(|l| !l.starts_with("Ret"));
    assert_eq!(hits.filter(|l| l.contains("Jump")).count(), 0);
    let jumps = text(nested).iter().filter(|l| l.contains("Jump")).count();
    assert_eq!(jumps, 3, "{:?}", text(nested));
    // The array is dropped on both ways, and the read's register is the array's.
    let read = "fn f(xs Array(Array(Int)), i Int) Array(Int) {\n\
                \tys = xs[i] or array::reverse([i])\n\
                \t[..ys, i]\n\
                }\n";
    let read = text(read);
    assert_eq!(read[0], "r2 = ArrayGet(r2, r3) -> 3", "{read:?}");
    assert_eq!(
        read.iter().filter(|l| l.starts_with("Drop")).count(),
        1,
        "{read:?}"
    );
    // A lookup in a map that dies.
    let looked = "fn f(m map::Map(Int, Array(Int)), k Int) Int {\n\
                  \tmatch map::get(m, k) {\n\
                  \t\tSome(xs) -> array::length(xs)\n\
                  \t\tNone -> 0\n\
                  \t}\n\
                  }\n";
    assert_eq!(text(looked)[0], "r2 = MapGet(r2, r3) -> 3");
    // A hit that reads the array again keeps it, and a miss that does takes nothing.
    let both = "fn f(xs Array(Int), i Int) Array(Int) {\n\
                \tmatch xs[i] {\n\
                \t\tSome(x) -> [..xs, x]\n\
                \t\tNone -> [..xs, 0]\n\
                \t}\n\
                }\n";
    assert_eq!(text(both)[0], "r3 = ArrayGet(r2, r3) -> 3");
    let kept = "fn f(xs Array(Int), i Int) Array(Int) {\n\
                \tys = array::put(xs, i, 1)\n\
                \t[..ys, ..xs]\n\
                }\n";
    let kept = text(kept);
    assert!(
        kept.iter().any(|l| l.contains("ArraySet(r2, r3, move")),
        "{kept:?}"
    );
}

/// The probes that take an array apart, each with how many places it has for what it
/// finds, which is more than one or none.
fn pieces() -> Vec<(ProbeOp, usize)> {
    let mut all = vec![(ProbeOp::ArrayUncons, 2)];
    for len in [0, 1, 2, 5, 300] {
        for more in [false, true] {
            for from in [0, 1, 3, 260] {
                for n in 0..4u32 {
                    let op = ProbeOp::ArraySplit { len, more, from };
                    let cons = (len, more, from) == (1, true, 0) && n > 0;
                    if from + n <= len && !cons {
                        all.push((op, n as usize + usize::from(more)));
                    }
                }
            }
        }
    }
    all
}

/// An array is taken apart by the form that has what is asked for, and one of whose
/// pieces nothing reads any is a test of its length alone. What nothing reads goes to no
/// register, and those behind the last that is read are not asked for at all.
#[test]
fn an_array_is_taken_apart_by_one_instruction() {
    let mut forms = std::collections::BTreeMap::new();
    for (op, places) in pieces() {
        for read in 0..1u32 << places.min(4) {
            let binds: Vec<Option<u32>> = (0..u32::try_from(places).expect("few"))
                .map(|i| (read >> i & 1 == 1).then_some(10 + i))
                .collect();
            let core = function(4, probe(op, &[2], &binds, gives(0), gives(1)));
            let program = program_with_fields();
            let source = Source::new(&program).expect("a whole program");
            let draft = translate(&source, &core, None, &mut Tables::default());
            let draft = draft.expect("it translates");
            crate::code::live::settle(&mut copy_of(&draft)).expect("it loads");
            let all = instructions(&draft);
            let shown: Vec<String> = all.iter().map(|i| shown(i, &draft.lists)).collect();
            assert_eq!(shown[1..], ["Ret 0", "Ret 1"], "{op:?}");
            let mut to = Vec::new();
            all[0].targets(&draft.lists, &mut |l| to.push(draft.labels[l.0 as usize]));
            assert_eq!(to, [2], "{op:?}");
            let mut want: Vec<String> = binds.iter().flatten().map(|b| b.to_string()).collect();
            let name = all[0].op().name();
            want.extend([name.to_string(), "2".to_string()]);
            assert_eq!(shown[0], want.join(" "), "{op:?} {binds:?}");
            let (elements, tail) = match op {
                ProbeOp::ArrayUncons => (1, binds[1].is_some()),
                ProbeOp::ArraySplit { more: true, .. } => (places - 1, binds[places - 1].is_some()),
                _ => (places, false),
            };
            let used = binds[..elements].iter().rposition(Option::is_some);
            let used = used.map_or(0, |last| last + 1);
            let far =
                matches!(op, ProbeOp::ArraySplit { len, from, .. } if len > 255 || from > 255);
            let form = match (op, used, tail) {
                (_, 0, false) => Op::JumpUnlessLength,
                (ProbeOp::ArrayUncons, _, true) => Op::ArrayUncons,
                _ if far || used > 4 => Op::ArraySplitWide,
                _ => Op::ArraySplit,
            };
            assert_eq!(name, form.name(), "{op:?} {binds:?}");
            *forms.entry(name).or_insert(0) += 1;
            // Each says the length and where it reads from.
            let (len, more, from) = match op {
                ProbeOp::ArraySplit { len, more, from } => (len, more, from),
                _ => (1, true, 0),
            };
            let length = Length { len, more };
            match all[0] {
                Ins::JumpUnlessLength { length: got, .. } => assert_eq!(got, length),
                Ins::ArraySplit { split, .. } => {
                    assert_eq!(split.split(), Split { length, from });
                }
                Ins::ArraySplitWide { elements, .. } => {
                    let wide = elements.of(&draft.lists).expect("a list");
                    assert_eq!(wide.split, Split { length, from });
                    assert_eq!(wide.elements.len(), used);
                }
                _ => {}
            }
        }
    }
    assert!(
        forms.len() == 4 && forms.values().all(|n| *n > 1),
        "{forms:?}"
    );
}

/// One that asks for elements an array of its length does not have, or that has no place
/// for its tail, or another number of operands, does not load. Neither does a split that
/// is a cons.
#[test]
fn an_array_taken_apart_into_other_things_does_not_load() {
    let program = program_with_fields();
    let source = Source::new(&program).expect("a whole program");
    let split = |len, more, from| ProbeOp::ArraySplit { len, more, from };
    let wrong = [
        (ProbeOp::ArrayUncons, 1, 0),
        (ProbeOp::ArrayUncons, 1, 1),
        (ProbeOp::ArrayUncons, 1, 3),
        (ProbeOp::ArrayUncons, 0, 2),
        (ProbeOp::ArrayUncons, 2, 2),
        (split(2, false, 0), 0, 2),
        (split(2, false, 0), 2, 2),
        (split(2, false, 0), 1, 3),
        (split(2, false, 1), 1, 2),
        (split(2, false, 3), 1, 0),
        (split(2, true, 0), 1, 0),
        (split(2, true, 0), 1, 4),
        (split(2, true, 2), 1, 2),
        (split(0, true, 0), 1, 2),
        (split(1, true, 0), 1, 2),
        (split(3, true, u32::MAX), 1, 2),
    ];
    for (op, args, binds) in wrong {
        let all: Vec<u32> = (0..u32::try_from(args).expect("few")).collect();
        let found: Vec<Option<u32>> = (0..binds).map(|i| Some(9 + i)).collect();
        let binds = found.len();
        let core = function(5, probe(op, &all, &found, gives(0), gives(1)));
        let got = translate(&source, &core, None, &mut Tables::default());
        let got = got.err().expect("it does not translate");
        assert_eq!(got, Unfit::Broken(Broken::Probe { op, args, binds }));
    }
}

/// Every probe that cannot miss goes to one place when it does, behind everything else,
/// and none has anything to jump over.
#[test]
fn what_cannot_miss_has_one_place_to_go() {
    let one = |found: u32, rest: CoreExpr| {
        probe(
            ProbeOp::ArrayUncons,
            &[0],
            &[Some(found), Some(found + 1)],
            rest,
            CoreExpr::NoArm,
        )
    };
    let core = function(1, one(5, one(7, returning(Atom::Local(LocalId(5))))));
    let draft = drafted(&core);
    let all = instructions(&draft);
    let kinds: Vec<&str> = all.iter().map(|i| i.op().name()).collect();
    assert_eq!(kinds, ["ArrayUncons", "ArrayUncons", "Move", "Ret", "Bad"]);
    for ins in &all[..2] {
        let mut to = Vec::new();
        ins.targets(&draft.lists, &mut |l| to.push(draft.labels[l.0 as usize]));
        assert_eq!(to, [4]);
    }
    assert!(draft.misses.is_empty());
    // One that can goes where its miss is, behind its hit.
    let both = probe(
        ProbeOp::ArrayUncons,
        &[0],
        &[Some(5), Some(6)],
        one(7, gives(5)),
        gives(0),
    );
    let draft = drafted(&function(1, both));
    let all = instructions(&draft);
    let kinds: Vec<&str> = all.iter().map(|i| i.op().name()).collect();
    assert_eq!(kinds, ["ArrayUncons", "ArrayUncons", "Ret", "Ret", "Bad"]);
}

/// The instructions of `f` in `src`, as they are listed.
fn listed(src: &str) -> Vec<String> {
    let src = format!("import scarlet/array\n{src}\npub fn main() {{ Nil }}\n");
    let program = crate::code::tests::program(&src);
    let at = (&program.fns).into_iter().position(|f| f.name == "f");
    let listing = crate::listing(&program).expect("it loads");
    let f = listing.function(at.expect("f")).expect("f");
    let lines = f.lines().skip(1).map(|l| l[5..].trim().to_string());
    lines.collect()
}

/// A walk is one instruction for the array at each turn, which takes it, and nothing is
/// asked at its end.
#[test]
fn a_walk_takes_its_array_apart_in_one_instruction() {
    let walk = |pattern: &str, next: &str| {
        listed(&format!(
            "fn f(xs Array(Int), acc Int) Int {{\n\tmatch xs {{\n\t\t[] -> acc\n\
             \t\t{pattern} -> f(t, {next})\n\t}}\n}}\n"
        ))
    };
    assert_eq!(
        walk("[h, ..t]", "acc + h"),
        [
            "r4, r2 = ArrayUncons(move r2) -> 3",
            "r3 = IntAdd(r3, r4)",
            "Jump -> 0",
            "Ret(move r3)",
        ]
    );
    // A head that nothing reads goes nowhere.
    assert_eq!(
        walk("[_, ..t]", "acc + 1")[0],
        "_, r2 = ArrayUncons(move r2) -> 3"
    );
    let twos = listed(
        "fn f(xs Array(Int), acc Int) Int {\n\tmatch xs {\n\
         \t\t[a, _, c, ..t] -> f(t, acc + a * c)\n\t\t_ -> acc\n\t}\n}\n",
    );
    assert_eq!(
        twos[0], "r4, _, r5, r2 = ArraySplit(move r2, 3..) -> 4",
        "{twos:?}"
    );
    // The elements in front of the first that is read are not in it.
    let third =
        listed("fn f(xs Array(Int)) Int {\n\tmatch xs {\n\t\t[_, _, c] -> c\n\t\t_ -> 0\n\t}\n}\n");
    assert!(
        third[0].contains("ArraySplit(move r2, 3, from 2)"),
        "{third:?}"
    );
}

/// The head is looked at and the array is left alone until a guard has held, when it is
/// taken. What a guard that fails goes on with finds it where it was.
#[test]
fn a_guard_looks_at_the_head_and_the_tail_is_taken_behind_it() {
    let f = listed(
        "fn f(xs Array(Int), p Int) Array(Int) {\n\tmatch xs {\n\
         \t\t[h, ..t] if h > p -> t\n\t\t_ -> xs\n\t}\n}\n",
    );
    assert_eq!(
        f,
        [
            "r4, _ = ArraySplit(r2, 1..) -> 4",
            "JumpUnlessIntLt(r3, r4) -> 4",
            "_, r2 = ArrayUncons(move r2) -> 5",
            "Ret(move r2)",
            "Ret(move r2)",
            "Bad(NoArm)",
        ],
        "{f:#?}"
    );
}

/// What one instruction finds has a register for each, and none that something held
/// beside it has.
#[test]
fn what_is_found_together_is_kept_apart() {
    let f = listed(
        "fn f(xs Array(Int), k Int, g fn(Int) Int) Int {\n\tmatch xs {\n\
         \t\t[a, b, c, ..t] -> g(a) + g(b) * g(c) + array::length(t) + k\n\
         \t\t_ -> k\n\t}\n}\n",
    );
    let (dsts, rest) = f[0].split_once(" = ").expect("it writes");
    assert!(rest.starts_with("ArraySplit(move r2, 3..)"), "{f:?}");
    let mut dsts: Vec<&str> = dsts.split(", ").collect();
    assert_eq!(dsts.len(), 4);
    dsts.sort_unstable();
    dsts.dedup();
    assert_eq!(dsts.len(), 4, "{f:?}");
    assert!(!dsts.contains(&"r3") && !dsts.contains(&"r4"), "{f:?}");
}

/// One item is pushed by an instruction that takes it, and several are put there by one.
#[test]
fn a_spread_pushes_what_is_beside_it() {
    let f = |e: &str| {
        let name = |n: &str| format!("{}{n}", if e.contains(n) { "" } else { "_" });
        let (a, b) = (name("a"), name("b"));
        listed(&format!(
            "fn f(xs Array(Int), {a} Int, {b} Int) Array(Int) {{ {e} }}\n"
        ))
    };
    assert_eq!(f("[a, ..xs]")[0], "r2 = ArrayPush(move r2, move r3, Front)");
    assert_eq!(f("[..xs, b]")[0], "r2 = ArrayPush(move r2, move r4, Back)");
    assert_eq!(
        f("[b, a, ..xs]")[0],
        "r2 = ArrayExtend(move r2, r4, r3, Front)"
    );
    let both = f("[a, ..xs, b, a]");
    assert_eq!(
        both[..2],
        [
            "r2 = ArrayPush(move r2, r3, Front)",
            "r2 = ArrayExtend(move r2, r4, r3, Back)",
        ],
        "{both:?}"
    );
}
