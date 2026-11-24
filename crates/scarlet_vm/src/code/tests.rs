use super::*;

/// A program with calls, a loop by tail call, a `match` with several
/// arms, an `if` and a map lookup that jumps, so its functions have
/// blocks of every shape.
const SHAPES: &str = "import scarlet/map\n\
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

fn bodies(code: &Code) -> Vec<&Body> {
    let mut out = Vec::new();
    for f in (&code.fns).into_iter().chain(&code.toplevels) {
        match f {
            Func::Ready(body) => out.push(body),
            Func::NotBuiltYet(_) => {}
        }
    }
    out
}

fn is_count(instr: &Instr) -> bool {
    matches!(instr, Instr::Count { .. })
}

/// A counted load is the ordinary load with a `Count` put in at the head
/// of each block and nothing else changed: the same instructions in the
/// same order, and every jump going where it went before.
#[test]
fn a_counted_load_is_the_same_program_with_a_count_at_each_block_head() {
    let result = scarlet_core::compile::compile_text(SHAPES, None);
    assert!(result.success(), "{:?}", result.diagnostics());
    let program = result.into_runnable().expect("a clean compile is runnable");
    let plain = load(&program, Audit::Never).expect("the program loads");
    let counted = load_counted(&program, Audit::Never).expect("the program loads");
    assert!(
        bodies(&plain)
            .iter()
            .all(|b| !b.instrs.iter().any(is_count))
    );

    let mut numbered = 0;
    let mut jumps = 0;
    for (was, now) in bodies(&plain).into_iter().zip(bodies(&counted.code)) {
        assert!(now.instrs.first().is_some_and(is_count), "{}", now.name);
        let kept: Vec<&Instr> = now.instrs.iter().filter(|i| !is_count(i)).collect();
        let kinds = |instrs: &[&Instr]| instrs.iter().map(|i| i.op()).collect::<Vec<_>>();
        assert_eq!(kinds(&kept), kinds(&was.instrs.iter().collect::<Vec<_>>()));
        // Where each instruction of the ordinary load sits in this one.
        let sits: Vec<usize> = (0..now.instrs.len())
            .filter(|i| !is_count(&now.instrs[*i]))
            .collect();
        for (i, instr) in now.instrs.iter().enumerate() {
            match instr {
                Instr::Count { block } => {
                    assert_eq!(*block, numbered, "blocks are numbered in order");
                    let kinds: Vec<Op> = now.instrs[i + 1..]
                        .iter()
                        .take_while(|i| !is_count(i))
                        .map(Instr::op)
                        .collect();
                    assert_eq!(*counted.blocks[numbered as usize], kinds);
                    numbered += 1;
                }
                _ => {
                    // Anything that may not fall through ends its block.
                    let next = i + 1;
                    let falls_only = instr.successors(i).eq([next]);
                    if !falls_only && next < now.instrs.len() {
                        assert!(is_count(&now.instrs[next]), "{} at {i}", now.name);
                    }
                }
            }
        }
        for (old, new) in was.instrs.iter().zip(kept) {
            let (mut old, mut new) = (old.clone(), new.clone());
            if let (Some(old), Some(new)) = (old.target(), new.target()) {
                let lands = *new as usize;
                assert!(is_count(&now.instrs[lands]), "a jump lands on a count");
                assert_eq!(sits[*old as usize], lands + 1, "and on the same block");
                jumps += 1;
            }
        }
    }
    assert_eq!(numbered as usize, counted.blocks.len());
    assert!(jumps > 5, "the program has jumps to check: {jumps}");
}

/// An audited load is the ordinary load with an `Audit` in front of every
/// instruction, a counted load's `Count`s among them, and every jump going to
/// the audit in front of where it went. The blocks counted hold no audits.
#[test]
fn an_audited_load_is_the_same_program_with_an_audit_before_each_instruction() {
    let result = scarlet_core::compile::compile_text(SHAPES, None);
    assert!(result.success(), "{:?}", result.diagnostics());
    let program = result.into_runnable().expect("a clean compile is runnable");
    let plain = load(&program, Audit::Never).expect("the program loads");
    let audited = load(&program, Audit::EachInstruction).expect("the program loads");
    let counted = load_counted(&program, Audit::Never).expect("the program loads");
    let both = load_counted(&program, Audit::EachInstruction).expect("the program loads");
    assert_eq!(counted.blocks, both.blocks);
    // Only a run that audits before each instruction has audits to run.
    let at_end = load(&program, Audit::AtEnd).expect("the program loads");
    let counted_at_end = load_counted(&program, Audit::AtEnd).expect("the program loads");
    for code in [&plain, &at_end, &counted.code, &counted_at_end.code] {
        let mut instrs = bodies(code).into_iter().flat_map(|body| &body.instrs);
        assert!(!instrs.any(|instr| matches!(instr, Instr::Audit)));
    }
    let mut jumps = 0;
    for (was, now) in [(&plain, &audited), (&counted.code, &both.code)] {
        assert!(!bodies(was).is_empty());
        for (was, now) in bodies(was).into_iter().zip(bodies(now)) {
            assert_eq!(now.instrs.len(), 2 * was.instrs.len(), "{}", now.name);
            for (old, new) in was.instrs.iter().zip(now.instrs.chunks(2)) {
                let [Instr::Audit, new] = new else {
                    panic!("{}: an audit and then the instruction", now.name);
                };
                assert_eq!(old.op(), new.op());
                let (mut old, mut new) = (old.clone(), new.clone());
                if let (Some(old), Some(new)) = (old.target(), new.target()) {
                    assert_eq!(*new, 2 * *old);
                    jumps += 1;
                }
            }
        }
    }
    assert!(jumps > 5, "the program has jumps to check: {jumps}");
}

fn mv(dst: u32, src: u32) -> Instr {
    Instr::Move {
        dst: Reg(dst),
        src: Reg(src),
        moved: false,
    }
}

fn ret(src: u32) -> Instr {
    Instr::Ret { src: Reg(src) }
}

/// Whether each `Move` in `instrs` takes its source, in order.
fn marks(instrs: &[Instr]) -> Vec<bool> {
    let moved = |i: &Instr| {
        if let Instr::Move { moved, .. } = i {
            Some(*moved)
        } else {
            None
        }
    };
    instrs.iter().filter_map(moved).collect()
}

/// [`marks`] of `instrs` once `moves_on_last_use` has been over them.
fn takes(mut instrs: Vec<Instr>, regs: u32) -> Vec<bool> {
    moves_on_last_use(&mut instrs, regs);
    marks(&instrs)
}

#[test]
fn a_move_takes_a_source_nothing_reads_later() {
    assert_eq!(takes(vec![mv(1, 0), ret(1)], 2), [true]);
}

/// The `Drop` Perceus puts after a copy does not read the register: it
/// finds it empty and does nothing.
#[test]
fn a_move_takes_a_source_that_is_only_dropped_later() {
    let drop = Instr::Drop { reg: Reg(0) };
    assert_eq!(takes(vec![mv(1, 0), drop, ret(1)], 2), [true]);
}

#[test]
fn a_move_shares_a_source_something_reads_later() {
    let len = Instr::ArrayLen {
        dst: Reg(2),
        src: Reg(0),
    };
    assert_eq!(takes(vec![mv(1, 0), len, ret(1)], 3), [false]);
    // The first of two copies shares, and the second takes.
    assert_eq!(takes(vec![mv(1, 0), mv(2, 0), ret(1)], 3), [false, true]);
}

/// A cell this frame parks for a constructor of its own has to still be
/// in its register when the `DropReuse` runs, so a copy before it shares.
#[test]
fn a_move_shares_a_source_whose_cell_is_kept_for_reuse() {
    let park = Instr::DropReuse { reg: Reg(0) };
    assert_eq!(takes(vec![mv(1, 0), park, ret(1)], 2), [false]);
}

/// Liveness follows the jumps. A read on the path a `Move` is on keeps
/// its source, and a read on some other path does not.
#[test]
fn a_move_looks_only_at_what_runs_after_it() {
    let branch = |after_join: u32| {
        vec![
            Instr::JumpIfFalse {
                cond: Reg(2),
                to: 3,
            },
            mv(1, 0),
            Instr::Jump { to: 4 },
            Instr::Const {
                dst: Reg(1),
                value: Value::NIL,
            },
            ret(after_join),
        ]
    };
    assert_eq!(takes(branch(0), 3), [false]);
    assert_eq!(takes(branch(1), 3), [true]);
    let other_path = vec![
        Instr::JumpIfFalse {
            cond: Reg(2),
            to: 3,
        },
        mv(1, 0),
        ret(1),
        ret(0),
    ];
    assert_eq!(takes(other_path, 3), [true]);
}

/// `r2` gets the array in `r0` with the value in `value` at the index
/// in `r1`, or the run continues at `miss`.
fn write(value: u32, miss: u32) -> Instr {
    Instr::ArraySet {
        dst: Reg(2),
        src: Reg(0),
        index: Reg(1),
        value: Arg::shared(Reg(value)),
        moved: false,
        miss,
    }
}

/// Whether each write in `instrs` takes its array and its value, once
/// `moves_on_last_use` has been over them.
fn write_takes(mut instrs: Vec<Instr>, regs: u32) -> Vec<(bool, bool)> {
    moves_on_last_use(&mut instrs, regs);
    let taken = |i: &Instr| {
        if let Instr::ArraySet { value, moved, .. } = i {
            Some((*moved, value.moved))
        } else {
            None
        }
    };
    instrs.iter().filter_map(taken).collect()
}

/// A write that misses takes nothing, so its array and its value are
/// still in their registers for whatever runs after the miss. Reading
/// them there does not stop the write taking them when it does not miss.
#[test]
fn a_write_takes_what_nothing_reads_after_it_does_not_miss() {
    let after_a_miss = |r: u32| vec![write(3, 2), ret(2), mv(2, r), ret(2)];
    assert_eq!(write_takes(after_a_miss(0), 4), [(true, true)]);
    assert_eq!(write_takes(after_a_miss(3), 4), [(true, true)]);
}

#[test]
fn a_write_shares_what_is_read_after_it_does_not_miss() {
    let len = |src: u32| Instr::ArrayLen {
        dst: Reg(1),
        src: Reg(src),
    };
    let then = |read: Instr| vec![write(3, 3), read, ret(2), ret(0)];
    assert_eq!(write_takes(then(len(0)), 4), [(false, true)]);
    assert_eq!(write_takes(then(len(3)), 4), [(true, false)]);
}

/// The array is read before the value is taken. When one register is
/// both, only the value takes it, or two holders would share one
/// reference.
#[test]
fn a_write_of_its_own_array_takes_the_register_once() {
    let instrs = vec![write(0, 2), ret(2), ret(1)];
    assert_eq!(write_takes(instrs, 3), [(false, true)]);
}

/// A write that misses has written nothing. So a register it would
/// have written is still live before it, when what runs after a miss
/// reads the value the register held.
#[test]
fn a_register_read_after_a_miss_is_live_before_the_write() {
    let live_before = |instrs: Vec<Instr>| liveness(&instrs, 4, false).0[0].has(Reg(2));
    assert!(live_before(vec![write(3, 2), ret(2), ret(2)]));
    assert!(!live_before(vec![write(3, 2), ret(2), ret(0)]));
    // The lookup that jumps on a missing key is the same.
    let find = Instr::MapFind {
        dst: Reg(2),
        map: Reg(0),
        key: Reg(1),
        to: 2,
    };
    assert!(live_before(vec![find.clone(), ret(2), ret(2)]));
    assert!(!live_before(vec![find, ret(2), ret(0)]));
}

/// `array::put` and `array::set` load as one write each. `put` gives
/// the array back on a miss, and `set` builds `Ok` or `Err(Nil)`. The
/// write takes an array that is not read again, and shares one that is.
#[test]
fn the_loader_makes_one_write_of_a_put_and_of_a_set() {
    let src = "import scarlet/array\n\
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
                   }\n";
    let result = scarlet_core::compile::compile_text(src, None);
    assert!(result.success(), "{:?}", result.diagnostics());
    let program = result.into_runnable().expect("a clean compile is runnable");
    let code = load(&program, Audit::Never).expect("the program loads");
    let body = |name: &str| -> &Body {
        bodies(&code)
            .into_iter()
            .find(|b| b.name.ends_with(name))
            .unwrap_or_else(|| panic!("no function called {name}"))
    };
    let ops = |name: &str| -> Vec<Op> { body(name).instrs.iter().map(Instr::op).collect() };
    let taken = |name: &str| -> Vec<bool> {
        let taken = |i: &Instr| {
            if let Instr::ArraySet { moved, .. } = i {
                Some(*moved)
            } else {
                None
            }
        };
        body(name).instrs.iter().filter_map(taken).collect()
    };
    assert_eq!(
        ops(".last_use"),
        [Op::Const, Op::ArraySet, Op::Jump, Op::Move, Op::Ret]
    );
    assert_eq!(taken(".last_use"), [true]);
    // The copy a miss makes takes the array too.
    assert_eq!(marks(&body(".last_use").instrs), [true]);
    assert_eq!(taken(".read_again"), [false]);
    assert_eq!(
        ops(".checked"),
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
/// binds the whole value. One that is read again is shared.
#[test]
fn the_loader_marks_the_moves_of_a_compiled_program() {
    let src = "fn fresh(n Int) Array(Int) { [n, n + 1] }\n\
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
                   }\n";
    let result = scarlet_core::compile::compile_text(src, None);
    assert!(result.success(), "{:?}", result.diagnostics());
    let program = result.into_runnable().expect("a clean compile is runnable");
    let code = load(&program, Audit::Never).expect("the program loads");
    let moves = |name: &str| -> Vec<bool> {
        let body = bodies(&code)
            .into_iter()
            .find(|b| b.name.ends_with(name))
            .unwrap_or_else(|| panic!("no function called {name}"));
        marks(&body.instrs)
    };
    assert_eq!(moves(".joined"), [true, true]);
    assert_eq!(moves(".bound"), [true]);
    assert_eq!(moves(".kept"), [false]);
}

/// The kinds of the instructions each function of `src` is loaded as, by the
/// function's name.
fn loaded(src: &str) -> HashMap<String, Vec<Op>> {
    let result = scarlet_core::compile::compile_text(src, None);
    assert!(result.success(), "{:?}", result.diagnostics());
    let program = result.into_runnable().expect("a clean compile is runnable");
    let code = load(&program, Audit::Never).expect("the program loads");
    bodies(&code)
        .into_iter()
        .map(|b| {
            let name = b.name.rsplit('.').next().unwrap_or_default().to_string();
            (name, b.instrs.iter().map(Instr::op).collect())
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

/// A constant that a module keeps in a global is stored there, even when only
/// an operator that could hold it reads it. The compiler reads a global back
/// with a load of its own, so this makes the read by hand.
#[test]
fn a_constant_kept_in_a_global_is_stored() {
    let result =
        scarlet_core::compile::compile_text("const kept = 7\npub fn main() { Nil }\n", None);
    assert!(result.success(), "{:?}", result.diagnostics());
    let program = result.into_runnable().expect("a clean compile is runnable");
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
        let f = CoreFn {
            params: vec![x.clone()],
            body: CoreExpr::Let {
                bind,
                rhs: rhs.clone(),
                body: Box::new(CoreExpr::Tail(sum)),
            },
            ret_ty: program.toplevel.core.ret_ty,
        };
        let loaded = Loader::new(&program.consts, &program.abi, &f, None).and_then(|l| l.body(&f));
        let (_, instrs) = loaded.unwrap_or_else(|_| panic!("the function loads"));
        instrs.iter().map(Instr::op).collect()
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
}

/// Every operator instruction names its registers everywhere the loader looks
/// for them. Each comparison becomes the jump beside it in the list, with the
/// same operands, when the branch is on what it writes.
#[test]
fn every_operator_instruction_names_its_operands() {
    let (dst, a, b) = (Reg(5), Reg(6), Reg(7));
    let k = SmallInt::new(-3).expect("small");
    let x = Float::new(0.5);
    let mut two = Vec::new();
    let mut one = Vec::new();
    let mut jumps = Vec::new();
    macro_rules! pairs {
        ([] $($name:ident $op:path),*) => { two.extend([$(Instr::$name(Pair { dst, a, b })),*]) };
    }
    macro_rules! ints {
        ([] $($name:ident $op:path),*) => { one.extend([$(Instr::$name(WithK { dst, a, k })),*]) };
    }
    macro_rules! floats {
        ([] $($name:ident $op:path),*) => {
            one.extend([$(Instr::$name(WithK { dst, a, k: x })),*])
        };
    }
    macro_rules! compare_pairs {
        ([] $($name:ident $jump:ident $op:path),*) => {
            $(
                two.push(Instr::$name(Pair { dst, a, b }));
                jumps.push((
                    Instr::$name(Pair { dst, a, b }),
                    Instr::$jump(JumpPair { a, b, to: 0 }),
                    vec![a, b],
                ));
            )*
        };
    }
    macro_rules! compare_with {
        ([$k:ident] $($name:ident $jump:ident $op:path),*) => {
            $(
                one.push(Instr::$name(WithK { dst, a, k: $k }));
                jumps.push((
                    Instr::$name(WithK { dst, a, k: $k }),
                    Instr::$jump(JumpWithK { a, k: $k, to: 0 }),
                    vec![a],
                ));
            )*
        };
    }
    int_arith!(rows pairs);
    float_arith!(rows pairs);
    int_arith_k!(rows ints);
    int_arith_k_first!(rows ints);
    float_arith_k!(rows floats);
    float_arith_k_first!(rows floats);
    int_compare!(rows compare_pairs);
    float_compare!(rows compare_pairs);
    int_compare_k!(rows compare_with k);
    float_compare_k!(rows compare_with x);
    assert_eq!((two.len(), one.len(), jumps.len()), (16, 22, 16));

    let reads = |i: &Instr| {
        let mut out = Vec::new();
        i.reads(|r| out.push(r));
        out
    };
    let named = |i: &Instr| {
        let mut out = Vec::new();
        i.clone().regs_mut(&mut |r| out.push(*r));
        out
    };
    let stored = two.iter().map(|i| (i, vec![a, b]));
    for (i, operands) in stored.chain(one.iter().map(|i| (i, vec![a]))) {
        assert_eq!(reads(i), operands, "{i}");
        assert_eq!(named(i), [vec![dst], operands].concat(), "{i}");
        assert_eq!(i.writes().map(Write::reg), Some(dst), "{i}");
        assert!(i.successors(4).eq([5]), "{i}");
        assert!(i.clone().target().is_none(), "{i}");
        let text = i.to_string();
        let name = i.op().name();
        assert!(text.starts_with(&format!("r5 = {name}(")), "{text}");
    }
    for (stored, want, operands) in &jumps {
        let jump = stored.as_test(dst).expect("a comparison").jump(9);
        assert_eq!(jump.op(), want.op());
        assert!(stored.as_test(a).is_none(), "{stored}");
        assert_eq!(&reads(&jump), operands, "{jump}");
        assert_eq!(&named(&jump), operands, "{jump}");
        assert!(jump.writes().is_none(), "{jump}");
        assert_eq!(jump.clone().target(), Some(&mut 9), "{jump}");
        assert!(jump.successors(4).eq([5, 9]), "{jump}");
        let shown = stored.to_string();
        let args = &shown[shown.find('(').expect("operands")..];
        let name = stored.op().name();
        assert_eq!(jump.to_string(), format!("JumpUnless{name}{args} -> 9"));
    }
    for i in two.iter().chain(&one) {
        let jumps = jumps.iter().any(|(stored, _, _)| stored.op() == i.op());
        assert_eq!(i.as_test(dst).is_some(), jumps, "{i}");
    }
}
