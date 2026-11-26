use super::super::ins::{Arg, ArgList, Args, LastReg, MaybeDst, OwnRoom, Shared, Taken};
use super::super::tests::{draft, of};
use super::super::translate::Draft;
use super::super::{DstList, tests::a_ctor};
use super::*;

/// Whether each operand `d`'s instructions hand on is taken, in order, once
/// [`settle`] has been over it.
fn takes(mut d: Draft) -> Vec<bool> {
    settle(&mut d).expect("a function that loads");
    let mut out = Vec::new();
    for step in &d.steps {
        step.operands(&d.lists, &mut |_, role| {
            if let Role::Pass { moved } = role {
                out.push(moved);
            }
        });
    }
    out
}

/// What `d` fails with.
fn fails(mut d: Draft) -> Option<Unfit> {
    settle(&mut d).err()
}

#[test]
fn a_move_takes_a_source_nothing_reads_later() {
    let d = draft(2, 1, vec![of::mv(1, 0), of::ret(1)], &[]);
    assert_eq!(takes(d), [true]);
}

/// The `Drop` Perceus puts after a copy does not read the register: it
/// finds it empty and does nothing.
#[test]
fn a_move_takes_a_source_that_is_only_dropped_later() {
    let d = draft(2, 1, vec![of::mv(1, 0), of::drop(0), of::ret(1)], &[]);
    assert_eq!(takes(d), [true]);
}

#[test]
fn a_move_shares_a_source_something_reads_later() {
    let d = draft(3, 1, vec![of::mv(1, 0), of::len(2, 0), of::ret(1)], &[]);
    assert_eq!(takes(d), [false]);
    // The first of two copies shares, and the second takes.
    let d = draft(3, 1, vec![of::mv(1, 0), of::mv(2, 0), of::ret(1)], &[]);
    assert_eq!(takes(d), [false, true]);
}

/// Taking a cell apart reads its register, where a `Drop` would not, so a copy
/// before it shares.
#[test]
fn a_move_shares_a_source_that_is_taken_apart_later() {
    let open = of::open([Some(2), None], 0);
    let d = draft(3, 1, vec![of::mv(1, 0), open, of::ret(1)], &[]);
    assert_eq!(takes(d), [false]);
}

/// Liveness follows the jumps. A read on the path a `Move` is on keeps
/// its source, and a read on some other path does not.
#[test]
fn a_move_looks_only_at_what_runs_after_it() {
    let branch = |after_join: u32| {
        let ins = vec![
            of::unless(1, 0),
            of::mv(2, 0),
            of::jump(1),
            of::constant(2),
            of::ret(after_join),
        ];
        draft(3, 2, ins, &[3, 4])
    };
    assert_eq!(takes(branch(0)), [false]);
    assert_eq!(takes(branch(2)), [true]);
    let ins = vec![of::unless(1, 0), of::mv(2, 0), of::ret(2), of::ret(0)];
    assert_eq!(takes(draft(3, 2, ins, &[3])), [true]);
}

/// A write that misses takes nothing, so its array and its value are
/// still in their registers for whatever runs after the miss. Reading
/// them there does not stop the write taking them when it does not miss.
#[test]
fn a_write_takes_what_nothing_reads_after_it_does_not_miss() {
    let after_a_miss = |r: u32| {
        let write = of::write(3, [0, 1, 2], 0);
        draft(4, 3, vec![write, of::ret(3), of::ret(r)], &[2])
    };
    assert_eq!(takes(after_a_miss(0)), [true, true]);
    assert_eq!(takes(after_a_miss(2)), [true, true]);
}

#[test]
fn a_write_shares_what_is_read_after_it_does_not_miss() {
    let then = |read: u32| {
        let write = of::write(3, [0, 1, 2], 0);
        let ins = vec![write, of::len(4, read), of::ret(3), of::ret(0)];
        draft(5, 3, ins, &[3])
    };
    assert_eq!(takes(then(0)), [false, true]);
    assert_eq!(takes(then(2)), [true, false]);
}

/// The array is read before the value is taken. When one register is
/// both, only the value takes it, or two holders would share one
/// reference. An index that is the array's register keeps the array.
#[test]
fn of_the_operands_that_name_one_register_only_the_last_takes_it() {
    let write = |operands| {
        let ins = vec![of::write(3, operands, 0), of::ret(3), of::ret(1)];
        draft(4, 3, ins, &[2])
    };
    assert_eq!(takes(write([0, 1, 0])), [false, true]);
    assert_eq!(takes(write([1, 1, 2])), [false, true]);
    let d = draft(2, 1, vec![of::append(1, 0, 0), of::ret(1)], &[]);
    assert_eq!(takes(d), [false]);
}

/// A call of a function value reads the value before it takes its arguments,
/// so an argument may take the register the value is in.
#[test]
fn an_argument_may_take_the_register_of_the_function_called() {
    let call = Ins::TailCallValue {
        callee: Shared(VReg(0)),
        args: Args::new(&[VReg(0), VReg(1)]).expect("room for two"),
        own: OwnRoom,
    };
    assert_eq!(takes(draft(2, 2, vec![call], &[])), [true, true]);
}

/// A write that misses has written nothing. So a register it would
/// have written is still live before it, when what runs after a miss
/// reads the value the register held.
#[test]
fn a_register_read_after_a_miss_is_live_before_the_write() {
    let before = |write: Ins<VReg>, after_a_miss: u32| {
        let ins = vec![of::constant(3), write, of::ret(3), of::ret(after_a_miss)];
        let mut d = draft(4, 3, ins, &[3]);
        let held = settle(&mut d).expect("a function that loads");
        // Value 3 was written while the operands of the write were held, and
        // clashes with them only if it is not written again before it is read.
        !held.idle[0] && held.clashes.contains(&(VReg(3), VReg(0)))
    };
    let write = || of::write(3, [0, 1, 2], 0);
    let find = || of::find(3, [0, 1], 0);
    assert!(before(write(), 3));
    assert!(before(find(), 3));
    // A value that nothing reads after a miss is not held before the write.
    let d = |miss: u32| {
        let ins = vec![of::mv(4, 0), write(), of::ret(3), of::ret(miss)];
        draft(5, 3, ins, &[3])
    };
    assert_eq!(fails(d(4)), None);
    assert_eq!(fails(d(3)), Some(Unfit::Broken(Broken::ReadBeforeWritten)));
}

/// A copy of a register that a write leaves alone when it misses shares what
/// it holds, because the code after the miss reads it. When nothing there does,
/// the copy takes it.
#[test]
fn a_copy_shares_what_a_miss_leaves_to_be_read() {
    let copied = |after_a_miss: u32| {
        let ins = vec![
            of::constant(3),
            of::mv(4, 3),
            of::write(3, [0, 1, 2], 0),
            of::ret(3),
            of::len(5, 4),
            of::ret(after_a_miss),
        ];
        takes(draft(6, 3, ins, &[4]))
    };
    assert_eq!(copied(3), [false, true, true]);
    assert_eq!(copied(5), [true, true, true]);
}

/// A value read on some path that did not write it is found, wherever the
/// path goes, and so is a drop of one.
#[test]
fn a_value_read_before_it_is_written_is_refused() {
    let unwritten = Some(Unfit::Broken(Broken::ReadBeforeWritten));
    assert_eq!(fails(draft(1, 0, vec![of::ret(0)], &[])), unwritten);
    let one_side = |dropped: bool| {
        let last = if dropped { of::drop(2) } else { of::len(1, 2) };
        let ins = vec![
            of::unless(0, 0),
            of::constant(2),
            of::len(1, 2),
            last,
            of::halt(0),
        ];
        draft(3, 1, ins, &[3])
    };
    assert_eq!(fails(one_side(false)), unwritten);
    assert_eq!(fails(one_side(true)), unwritten);
    // Written on both, it is not.
    let ins = vec![
        of::unless(0, 0),
        of::constant(2),
        of::jump(1),
        of::constant(2),
        of::ret(2),
    ];
    assert_eq!(fails(draft(3, 1, ins, &[3, 4])), None);
}

#[test]
fn a_jump_back_is_refused() {
    let ins = vec![of::constant(1), of::unless(0, 0), of::ret(1)];
    let back = Some(Unfit::Broken(Broken::JumpsBack));
    assert_eq!(fails(draft(2, 1, ins.clone(), &[0])), back);
    assert_eq!(fails(draft(2, 1, ins.clone(), &[1])), back);
    assert_eq!(fails(draft(2, 1, ins, &[2])), None);
}

/// Another function's code comes right after this one's, so one that may go on
/// past its end is refused, whether it falls off it or jumps there.
#[test]
fn a_function_that_goes_past_its_end_is_refused() {
    let runs_off = Some(Unfit::Broken(Broken::RunsOff));
    assert_eq!(fails(draft(1, 1, Vec::<Step>::new(), &[])), runs_off);
    assert_eq!(fails(draft(2, 1, vec![of::constant(1)], &[])), runs_off);
    let falls = vec![of::unless(0, 0), of::ret(0), of::len(1, 0)];
    assert_eq!(fails(draft(2, 1, falls, &[2])), runs_off);
    assert_eq!(fails(draft(1, 1, vec![of::jump(0)], &[1])), runs_off);
    let jumps = vec![of::unless(0, 0), of::ret(0)];
    assert_eq!(fails(draft(1, 1, jumps.clone(), &[2])), runs_off);
    assert_eq!(fails(draft(1, 1, jumps, &[1])), None);
}

/// A constant that each instruction reading it holds is named by none, and is
/// left out. One that something only drops is not.
#[test]
fn a_constant_nothing_names_is_unread() {
    let ins = vec![
        of::constant(1),
        of::constant(2),
        of::constant(3),
        of::drop(3),
        of::halt(2),
    ];
    let mut d = draft(4, 1, ins, &[]);
    let held = settle(&mut d).expect("a function that loads");
    assert_eq!(held.idle, [true, false, false, false, false]);
    assert_eq!(held.named, [false, false, true, true]);
}

/// As many values as there are registers may be held at once, and one more may
/// not.
#[test]
fn a_function_may_hold_as_many_values_as_there_are_registers() {
    let holding = |n: u32| {
        let mut ins: Vec<_> = (0..n).map(of::constant).collect();
        ins.extend((1..n).map(|v| of::add(0, 0, v)));
        ins.push(of::ret(0));
        draft(n, 0, ins, &[])
    };
    assert_eq!(fails(holding(254)), None);
    assert_eq!(fails(holding(255)), Some(Unfit::TooMany(Limit::Values)));
}

/// The lists beside the code are gone over as the operands in an instruction
/// are.
#[test]
fn a_list_of_arguments_is_decided_as_the_others_are() {
    let mut d = draft(3, 3, Vec::<Step>::new(), &[]);
    let arg = |v| Arg::shared(VReg(v));
    d.lists.args.push(Box::new([arg(0), arg(1), arg(0)]));
    d.steps.push(Step::Ins(Ins::TailCallValueWide {
        callee: Shared(VReg(1)),
        args: ArgList(0),
        own: OwnRoom,
    }));
    let held = settle(&mut d).expect("a function that loads");
    let moved: Vec<bool> = d.lists.args[0].iter().map(|a| a.moved).collect();
    assert_eq!(moved, [false, true, true]);
    assert_eq!(held.named, [true, true, false]);
}

/// The fields of a cell taken apart are written at once, whether they are in the
/// instruction or in a list, so each clashes with the others and with what is
/// held after, and with the cell only if that is read again. One with no
/// register is no value.
#[test]
fn the_fields_of_a_cell_clash_with_each_other() {
    let listed = || Ins::OpenWide {
        dsts: DstList(0),
        src: Taken(VReg(0)),
        ctor: a_ctor(),
    };
    for (ins, again) in [
        (of::open([Some(2), Some(3)], 0), false),
        (of::open([Some(2), Some(3)], 0), true),
        (listed(), false),
        (listed(), true),
    ] {
        let end = if again { of::len(4, 0) } else { of::len(4, 1) };
        let mut d = draft(5, 2, vec![ins, end, of::ret(1)], &[]);
        let dsts = [Some(2), None, Some(3)].map(|v| MaybeDst::new(v.map(VReg)));
        d.lists.dsts.push(Box::new(dsts));
        let held = settle(&mut d).expect("a function that loads");
        let pairs = held
            .clashes
            .iter()
            .map(|(a, b)| (a.0.min(b.0), a.0.max(b.0)));
        let mut pairs: Vec<_> = pairs.filter(|(_, b)| *b < 4).collect();
        pairs.sort_unstable();
        let want = [(0, 2), (0, 3), (1, 2), (1, 3), (2, 3)];
        assert_eq!(pairs, want[if again { 0 } else { 2 }..], "{ins:?} {again}");
        assert_eq!(held.named, [true; 5]);
    }
}

/// An instruction that writes several registers needs one for each, even for
/// one that nothing reads, and for one that it writes only when it does not
/// jump. No instruction writes some of each, so this makes the operands of one
/// by hand.
#[test]
fn values_written_together_clash() {
    let mut walk = Walk::new(6, 0, 0);
    for v in [VReg(4), VReg(1)] {
        walk.held.add(v);
        walk.read.add(v);
    }
    walk.operands = vec![
        (VReg(1), Role::Write),
        (VReg(2), Role::WriteOnHit),
        (VReg(3), Role::Write),
        (VReg(5), Role::Look),
    ];
    walk.clash(Role::WriteOnHit);
    walk.clash(Role::Write);
    let mut clashes: Vec<_> = walk.out.clashes.iter().map(|(a, b)| (a.0, b.0)).collect();
    clashes.sort_unstable();
    let want = [
        (1, 2),
        (1, 3),
        (1, 4),
        (2, 1),
        (2, 3),
        (2, 4),
        (3, 1),
        (3, 4),
    ];
    assert_eq!(clashes, want);
}

/// The steps of `d` that do nothing, by their number.
fn idle(mut d: Draft) -> Vec<usize> {
    let held = settle(&mut d).expect("a function that loads");
    let idle = held.idle.iter().enumerate();
    idle.filter(|(_, idle)| **idle).map(|(i, _)| i).collect()
}

/// A drop of what every way to it took does nothing, and its value gives its
/// register up where it was taken. One that some way finds its value is done.
#[test]
fn a_drop_of_what_was_taken_does_nothing() {
    // Taken, then dropped: by a copy, an array, a call, or a cell taken apart.
    let open = of::open([Some(1), None], 0);
    for taking in [of::mv(1, 0), of::append(1, 0, 2), of::call(1, &[0]), open] {
        let d = draft(3, 3, vec![taking, of::drop(0), of::halt(1)], &[]);
        assert_eq!(idle(d), [1], "{taking:?}");
    }
    // Only looked at, shared, or read again later on the way.
    let d = draft(2, 1, vec![of::len(1, 0), of::drop(0), of::halt(1)], &[]);
    assert_eq!(idle(d), [0; 0]);
    let ins = vec![of::mv(1, 0), of::len(2, 0), of::drop(0), of::halt(1)];
    assert_eq!(idle(draft(3, 1, ins, &[])), [0; 0]);
    let ins = vec![of::append(1, 2, 0), of::drop(0), of::halt(1)];
    assert_eq!(idle(draft(3, 3, ins, &[])), [0; 0]);
    // Never read at all, whether it came in or was made.
    assert_eq!(
        idle(draft(1, 1, vec![of::drop(0), of::halt(0)], &[])),
        [0; 0]
    );
    let ins = vec![of::len(1, 0), of::drop(1), of::halt(0)];
    assert_eq!(idle(draft(2, 1, ins, &[])), [0; 0]);
}

/// A value that is written on a hit and dropped after a miss is held for that
/// drop before it, so what is written between keeps clear of it. A drop that
/// the hit leads to says nothing of it.
#[test]
fn what_a_miss_drops_is_held_until_then() {
    // Value 2 is made, and then written again by the write unless it misses. On
    // the hit it is passed on, and its drop there does nothing. On the miss the
    // first is dropped.
    let ins = vec![
        of::append(2, 0, 1),
        of::constant(3),
        of::write(2, [0, 3, 1], 0),
        of::mv(5, 2),
        of::drop(2),
        of::halt(5),
        of::drop(2),
        of::halt(1),
    ];
    let mut d = draft(6, 2, ins, &[6]);
    let held = settle(&mut d).expect("a function that loads");
    let mut clashes = held.clashes.iter().map(|(x, y)| (x.0, y.0));
    assert!(clashes.any(|pair| [(2, 3), (3, 2)].contains(&pair)));
    assert!(held.idle[4] && !held.idle[6]);
}

/// Each drop is decided by itself, and two that one way leads to are decided
/// together.
#[test]
fn a_drop_on_each_way_is_decided_by_the_ways_to_it() {
    // One side takes and the other only looks.
    let sides = |other: Ins<VReg>| {
        let ins = vec![
            of::unless(1, 0),
            of::mv(2, 0),
            of::drop(0),
            of::halt(2),
            other,
            of::drop(0),
            of::halt(2),
        ];
        draft(3, 2, ins, &[4])
    };
    assert_eq!(idle(sides(of::len(2, 0))), [2]);
    assert_eq!(idle(sides(of::mv(2, 0))), [2, 5]);
    // Both sides take it, or one, and it is dropped after they meet.
    let meet = |other: Ins<VReg>| {
        let ins = vec![
            of::unless(1, 0),
            of::mv(2, 0),
            of::jump(1),
            other,
            of::drop(0),
            of::halt(2),
        ];
        draft(3, 2, ins, &[3, 4])
    };
    assert_eq!(idle(meet(of::mv(2, 0))), [4]);
    assert_eq!(idle(meet(of::len(2, 0))), [0; 0]);
    assert_eq!(idle(meet(of::constant(2))), [0; 0]);
    // Taken before the ways part, and dropped on both.
    let ins = vec![
        of::mv(2, 0),
        of::unless(1, 0),
        of::drop(0),
        of::halt(2),
        of::drop(0),
        of::halt(1),
    ];
    assert_eq!(idle(draft(3, 2, ins, &[4])), [2, 4]);
    // Read on one way and dropped on the other, so not taken before.
    let ins = vec![
        of::mv(2, 0),
        of::unless(1, 0),
        of::drop(0),
        of::halt(2),
        of::halt(0),
    ];
    assert_eq!(idle(draft(3, 2, ins.clone(), &[4])), [0; 0]);
    // The other way round: read by what follows, dropped where it jumps.
    let ins = vec![
        of::mv(2, 0),
        of::unless(1, 0),
        of::halt(0),
        of::drop(0),
        of::halt(2),
    ];
    assert_eq!(idle(draft(3, 2, ins, &[3])), [0; 0]);
}

/// A write that misses takes nothing, so a drop where it goes then finds the
/// array, and one where it goes on does not.
#[test]
fn a_drop_after_a_miss_finds_what_the_write_left() {
    let after = |after_a_miss: Vec<Ins<VReg>>| {
        let mut ins = vec![of::write(3, [0, 1, 2], 0), of::drop(0), of::halt(3)];
        ins.extend(after_a_miss);
        draft(5, 3, ins, &[3])
    };
    assert_eq!(idle(after(vec![of::drop(0), of::halt(1)])), [1]);
    assert_eq!(idle(after(vec![of::halt(0)])), [1]);
    // Both go on to one drop, and only the one that missed leaves it the array.
    let meet = |on_a_miss: Ins<VReg>| {
        let ins = vec![
            of::write(3, [0, 1, 2], 0),
            of::jump(1),
            on_a_miss,
            of::drop(0),
            of::halt(3),
        ];
        draft(4, 3, ins, &[2, 3])
    };
    assert_eq!(idle(meet(of::mv(3, 0))), [3]);
    assert_eq!(idle(meet(of::len(3, 0))), [0; 0]);
    assert_eq!(idle(meet(of::constant(3))), [0; 0]);
}

/// A value held only for drops that do nothing clashes with nothing, and one
/// held for a drop that finds it clashes with what is written before it.
#[test]
fn a_value_held_for_a_drop_that_does_nothing_clashes_with_nothing() {
    let clashes = |first: Ins<VReg>| {
        let ins = vec![
            first,
            of::constant(2),
            of::drop(0),
            of::add(3, 1, 2),
            of::ret(3),
        ];
        let mut d = draft(4, 1, ins, &[]);
        let held = settle(&mut d).expect("a function that loads");
        assert_eq!(held.idle[2], first.op().name() == "Move");
        held.clashes
    };
    let (zero, one, two) = (VReg(0), VReg(1), VReg(2));
    assert_eq!(clashes(of::mv(1, 0)), [(two, one)]);
    // The constant is straight in front of the drop, so it may take the place of
    // what is dropped.
    assert_eq!(clashes(of::len(1, 0)), [(two, one), (one, zero)]);
}

/// What an instruction writes may take the place of a value that is dropped
/// straight after it. Then the drop goes, because the write gives the value up.
#[test]
fn a_write_gives_up_what_is_dropped_straight_after() {
    let settled = |ins: Vec<Ins<VReg>>, labels: &[usize]| {
        let mut d = draft(8, 3, ins, labels);
        settle(&mut d).expect("a function that loads")
    };
    let over = |step, dropped, written| Overwritten {
        step,
        dropped: VReg(dropped),
        written: VReg(written),
    };
    let after = |drops: &[u32]| {
        let drops = drops.iter().map(|v| of::drop(*v));
        let ends = [of::add(5, 4, 4), of::ret(5)];
        let ins = [of::len(4, 0)].into_iter().chain(drops).chain(ends);
        settled(ins.collect(), &[])
    };
    let held = after(&[0]);
    assert_eq!(held.overwritten, [over(1, 0, 4)]);
    assert!(held.clashes.is_empty());
    // Any of several, and whatever else they are of.
    let held = after(&[0, 1, 2]);
    let mut found = held.overwritten.clone();
    found.sort_by_key(|o| o.step);
    assert_eq!(found, [over(1, 0, 4), over(2, 1, 4), over(3, 2, 4)]);
    assert!(held.clashes.is_empty());
    // Not what it wrote itself.
    let held = settled(
        vec![of::len(4, 0), of::drop(4), of::len(5, 1), of::ret(5)],
        &[],
    );
    assert_eq!(held.overwritten, []);
    // Nor one behind another instruction, or that something jumps to.
    let ins = vec![
        of::len(4, 0),
        of::constant(6),
        of::drop(0),
        of::add(5, 4, 6),
        of::ret(5),
    ];
    let held = settled(ins, &[]);
    assert_eq!(held.overwritten, [over(2, 0, 6)]);
    assert!(held.clashes.contains(&(VReg(4), VReg(0))));
    let ins = vec![
        of::constant(4),
        of::unless(1, 0),
        of::len(4, 2),
        of::drop(0),
        of::add(5, 4, 4),
        of::ret(5),
    ];
    let held = settled(ins.clone(), &[3]);
    assert_eq!(held.overwritten, []);
    assert!(held.clashes.contains(&(VReg(4), VReg(0))));
    assert_eq!(settled(ins, &[4]).overwritten, [over(3, 0, 4)]);
    // A write that may miss gives up what is dropped straight after a hit, and where it
    // jumps the register is as it was.
    let ins = vec![
        of::write(4, [0, 1, 2], 0),
        of::drop(1),
        of::len(5, 4),
        of::ret(5),
        of::drop(0),
        of::ret(1),
    ];
    let held = settled(ins, &[4]);
    assert_eq!(held.overwritten, [over(1, 1, 4)]);
    // So what it writes may be in the register of any value that only the miss needs.
    assert_eq!(held.clashes, []);
    // Each of two written at once may.
    let ins = vec![
        of::open([Some(4), Some(6)], 0),
        of::drop(1),
        of::add(5, 4, 6),
        of::ret(5),
    ];
    assert_eq!(
        settled(ins, &[]).overwritten,
        [over(1, 1, 4), over(1, 1, 6)]
    );
}

/// The steps of `ins` that do nothing, and what clashes.
fn idle_steps(ins: Vec<Ins<VReg>>, labels: &[usize]) -> (Vec<usize>, Vec<(VReg, VReg)>) {
    let mut d = draft(6, 3, ins, labels);
    let held = settle(&mut d).expect("a function that loads");
    let idle = (0..held.idle.len()).filter(|i| held.idle[*i]);
    (idle.collect(), held.clashes)
}

/// A drop straight in front of a return does nothing, and holds nothing, and
/// neither does one in front of it.
#[test]
fn a_drop_in_front_of_a_return_does_nothing() {
    let idle = idle_steps;
    let two = vec![of::len(4, 0), of::drop(0), of::drop(1), of::ret(4)];
    assert_eq!(idle(two, &[]), (vec![1, 2], Vec::new()));
    let closure = Ins::RetClosure {
        src: Taken(VReg(4)),
        last: LastReg,
    };
    let ins = vec![of::len(4, 0), of::drop(0), closure];
    assert_eq!(idle(ins, &[]).0, [1]);
    // A constant that nothing names goes too, so it does not stand between.
    let ins = vec![of::drop(0), of::constant(5), of::ret(1)];
    assert_eq!(idle(ins, &[]).0, [0, 1]);
    let ins = vec![of::len(4, 0), of::drop(0), of::len(5, 4), of::ret(5)];
    assert_eq!(idle(ins, &[]).0, [0; 0]);
    // A tail call of a function value gives up every register too.
    let ins = vec![of::len(4, 0), of::drop(0), of::drop(4), of::leave(1)];
    assert_eq!(idle(ins, &[]), (vec![1, 2], vec![(VReg(4), VReg(1))]));
    // One that a jump goes to from elsewhere is in front of a return all the
    // same.
    let ins = vec![of::unless(1, 0), of::len(4, 2), of::drop(0), of::ret(1)];
    assert_eq!(idle(ins, &[2]).0, [2]);
}

/// What makes no cell and calls nothing may come between a drop and the return,
/// a jump among it. Anything else may not.
#[test]
fn a_drop_with_little_between_it_and_a_return_does_nothing() {
    let idle = |ins: Vec<Ins<VReg>>, labels: &[usize]| idle_steps(ins, labels).0;
    let none: [usize; 0] = [];
    let field = |dst: u32, src: u32| Ins::Field {
        dst: Dst(VReg(dst)),
        src: Src(VReg(src)),
        index: 0,
    };
    let lt = |dst: u32, a: u32, b: u32| Ins::IntLt {
        dst: Dst(VReg(dst)),
        a: Src(VReg(a)),
        b: Src(VReg(b)),
    };
    let ins = vec![
        of::drop(0),
        of::mv(3, 1),
        of::constant(4),
        lt(5, 3, 4),
        of::ret(5),
    ];
    assert_eq!(idle(ins, &[]), [0]);
    let ins = vec![of::drop(0), field(3, 1), of::leave(3)];
    assert_eq!(idle(ins, &[]), [0]);
    // It holds nothing then, so what is written may have its register.
    let ins = vec![of::drop(0), field(3, 1), of::ret(3)];
    assert_eq!(idle_steps(ins, &[]), (vec![0], Vec::new()));
    for other in [
        of::add(3, 1, 2),
        of::len(3, 1),
        of::append(3, 1, 2),
        of::call(3, &[1]),
    ] {
        let ins = vec![of::drop(0), of::mv(4, 1), other, of::ret(3)];
        assert_eq!(idle(ins, &[]), none);
    }
    // Past a jump, when what it goes to is so too.
    let ins = vec![of::drop(0), of::jump(0), of::len(4, 1), of::ret(1)];
    assert_eq!(idle(ins, &[3]), [0]);
    let ins = vec![
        of::drop(0),
        of::jump(0),
        of::ret(1),
        of::len(4, 1),
        of::ret(1),
    ];
    assert_eq!(idle(ins, &[3]), none);
    let branch = |one: Ins<VReg>, other: Ins<VReg>| {
        let ins = vec![
            of::drop(0),
            of::unless(1, 0),
            one,
            of::ret(3),
            other,
            of::ret(3),
        ];
        idle(ins, &[4])
    };
    assert_eq!(branch(of::mv(3, 1), of::mv(3, 2)), [0]);
    assert_eq!(branch(of::len(3, 1), of::mv(3, 2)), none);
    assert_eq!(branch(of::mv(3, 1), of::len(3, 2)), none);
    // A tail call to a function that is known gives up what it does not pass, but
    // by then it may have been written over.
    let steps = vec![of::drop(0).into(), of::enter(1, &[1])];
    let mut d = draft(2, 2, steps, &[]);
    let held = settle(&mut d).expect("a function that loads");
    assert_eq!(held.idle, [false, false]);
}

/// A drop in front of where a cell is taken apart is left out only when nothing
/// was read out of what it is of. That may hold the cell, and then its fields
/// would not move out.
#[test]
fn a_drop_of_what_may_hold_the_cell_stays_in_front_of_taking_it_apart() {
    let idle = |ins: Vec<Ins<VReg>>, labels: &[usize]| idle_steps(ins, labels).0;
    let none: [usize; 0] = [];
    let element = |dst: u32, src: u32| Ins::ArrayElem {
        dst: Dst(VReg(dst)),
        src: Src(VReg(src)),
        index: 0,
    };
    let field = |dst: u32, src: u32| Ins::Field {
        dst: Dst(VReg(dst)),
        src: Src(VReg(src)),
        index: 0,
    };
    let opened = || of::open([Some(4), None], 3);
    let ins = vec![of::mv(3, 1), of::drop(0), opened(), of::ret(4)];
    assert_eq!(idle(ins, &[]), [1]);
    for read in [field(3, 0), element(3, 0)] {
        let ins = vec![read, of::drop(0), opened(), of::ret(4)];
        assert_eq!(idle(ins, &[]), none);
        // What is dropped after it is in front of the return alone.
        let ins = vec![read, opened(), of::drop(0), of::ret(4)];
        assert_eq!(idle(ins, &[]), [2]);
        // It may be where a jump goes.
        let ins = vec![read, of::drop(0), of::jump(0), opened(), of::ret(4)];
        assert_eq!(idle(ins, &[3]), none);
    }
    for read in [of::find(3, [0, 1], 0), of::get(Some(3), [0, 1], 0)] {
        let ins = vec![read, of::drop(0), opened(), of::ret(4), of::ret(1)];
        assert_eq!(idle(ins, &[4]), none);
    }
}

/// Groups of drops that meet are found together, whichever of them is, and
/// before they meet or after.
#[test]
fn drops_that_meet_are_found_together() {
    let mut all = AllDrops::default();
    let drops: Vec<Drops> = (10..16).map(|step| all.add(step)).collect();
    all.join(drops[0], drops[1]);
    all.join(drops[2], drops[3]);
    all.find(drops[3]);
    assert!(all.find_it(drops[2]) && !all.find_it(drops[1]));
    all.join(drops[1], drops[2]);
    assert!((0..4).all(|i| all.find_it(drops[i])));
    all.join(drops[4], drops[5]);
    assert!(!all.find_it(drops[4]));
    all.find(drops[5]);
    assert!(all.find_it(drops[4]));
    all.join(drops[4], drops[0]);
    assert!((0..6).all(|i| all.find_it(drops[i])));
    let steps: Vec<(usize, bool)> = (10..16).map(|step| (step, false)).collect();
    assert_eq!(all.steps(), steps);
    let lone = all.add(16);
    assert_eq!(all.steps()[6], (16, true));
    all.find(lone);
    assert_eq!(all.steps()[6], (16, false));
}

/// Adding, removing and asking agree with a plain set, in any order.
#[test]
fn a_set_holds_what_was_added_and_not_removed() {
    let mut set = Set::new(50);
    let mut plain = std::collections::BTreeSet::new();
    let mut x = 7u32;
    for _ in 0..2000 {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let v = VReg((x >> 8) % 50);
        match (x >> 24) % 40 {
            0 => {
                set.clear();
                plain.clear();
            }
            n if n < 20 => {
                set.add(v);
                plain.insert(v);
            }
            _ => {
                set.remove(v);
                plain.remove(&v);
            }
        }
        let mut items = set.items.clone();
        items.sort();
        assert_eq!(items, plain.iter().copied().collect::<Vec<_>>());
        assert!((0..50).map(VReg).all(|v| set.has(v) == plain.contains(&v)));
    }
    set.add(VReg(50));
    assert!(!set.has(VReg(50)), "past the values there are");
}
