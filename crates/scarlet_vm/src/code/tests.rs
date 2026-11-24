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
    let plain = load(&program).expect("the program loads");
    let counted = load_counted(&program).expect("the program loads");
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
    let code = load(&program).expect("the program loads");
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
    let code = load(&program).expect("the program loads");
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
