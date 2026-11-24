use super::super::ins::{Arg, Args, KeepList, OwnEntry, OwnRoom, Shared};
use super::super::tests::{draft, of};
use super::super::translate::Draft;
use super::*;

/// Whether each operand `d`'s instructions hand on is taken, in order, once
/// [`settle`] has been over it.
fn takes(mut d: Draft) -> Vec<bool> {
    settle(&mut d).expect("a function that loads");
    let mut out = Vec::new();
    for ins in &d.ins {
        ins.operands(&d.lists, &mut |_, role| {
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

/// A cell this frame parks for a constructor of its own has to still be
/// in its register when the `DropReuse` runs, so a copy before it shares.
#[test]
fn a_move_shares_a_source_whose_cell_is_kept_for_reuse() {
    let d = draft(2, 1, vec![of::mv(1, 0), of::park(0), of::ret(1)], &[]);
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
    let d = draft(2, 2, vec![of::again(&[0, 1, 0])], &[]);
    assert_eq!(takes(d), [false, true, true]);
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
        !held.unread[0] && held.clashes.contains(&(VReg(3), VReg(0)))
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
            of::ret(0),
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

/// A cell a loop carries round may be one from its last turn, so its register
/// may be read before this turn writes it.
#[test]
fn a_carried_cell_may_be_from_the_turn_before() {
    let over = Ins::CtorOver {
        dst: Dst(VReg(1)),
        ctor: super::super::tests::a_ctor(),
        fields: Args::new(&[VReg(0)]).expect("room for one"),
        reuse: super::super::Over(VReg(1)),
    };
    let mut d = draft(2, 1, vec![over, of::ret(1)], &[]);
    assert_eq!(
        fails(draft(2, 1, vec![over, of::ret(1)], &[])),
        Some(Unfit::Broken(Broken::ReadBeforeWritten))
    );
    d.carried.insert(VReg(1));
    assert_eq!(fails(d), None);
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
    assert_eq!(fails(draft(1, 1, Vec::new(), &[])), runs_off);
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
        of::ret(2),
    ];
    let mut d = draft(4, 1, ins, &[]);
    let held = settle(&mut d).expect("a function that loads");
    assert_eq!(held.unread, [true, false, false, false, false]);
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
/// are, and what a loop keeps is not an operand.
#[test]
fn a_list_of_arguments_is_decided_as_the_others_are() {
    let mut d = draft(3, 3, Vec::new(), &[]);
    let arg = |v| Arg::shared(VReg(v));
    d.lists.args.push(Box::new([arg(0), arg(1), arg(0)]));
    d.lists.regs.push(Box::new([VReg(2)]));
    d.carried.insert(VReg(2));
    d.ins.push(Ins::TailCallSelfKeep {
        to: OwnEntry,
        args: super::super::ArgList(0),
        keep: KeepList(0),
        own: OwnRoom,
    });
    let held = settle(&mut d).expect("a function that loads");
    let moved: Vec<bool> = d.lists.args[0].iter().map(|a| a.moved).collect();
    assert_eq!(moved, [false, true, true]);
    assert_eq!(held.named, [true, true, false]);
    assert_eq!(d.lists.regs[0][0], VReg(2));
}

/// An instruction that writes several registers needs one for each, even for
/// one that nothing reads. No instruction does yet, so this makes the operands
/// of one by hand.
#[test]
fn values_written_together_clash() {
    let mut walk = Walk::new(6, 0, 0);
    walk.held.add(VReg(4));
    walk.held.add(VReg(1));
    walk.operands = vec![
        (VReg(1), Role::Write),
        (VReg(2), Role::WriteOnHit),
        (VReg(3), Role::Write),
        (VReg(5), Role::Look),
    ];
    walk.clash();
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
