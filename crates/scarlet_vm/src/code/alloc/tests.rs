use super::super::ins::{Arg, Role};
use super::super::live::settle;
use super::super::tests::programs::every_draft;
use super::super::tests::random::random_function;
use super::super::tests::{body, copy_of, draft, named, of, program};
use super::super::translate::translate;
use super::super::{Source, Tables, load};
use super::*;
use crate::Audit;
use scarlet_ir::core_ir::FuncIdx;
use std::collections::BTreeMap;

fn fitted(mut d: Draft) -> Result<Assigned, Unfit> {
    let held = settle(&mut d)?;
    allocate(d, &held)
}

/// The register each value of `d` gets.
fn regs(d: Draft) -> Vec<Option<u8>> {
    let f = fitted(d).expect("a function that fits");
    f.regs.iter().map(|r| r.map(|r| r.0)).collect()
}

/// The arguments arrive in order in the registers from the first that is not
/// the frame's own, whatever else the function does with its values.
#[test]
fn parameters_have_the_first_registers() {
    let d = draft(4, 3, vec![of::add(3, 2, 1), of::ret(3)], &[]);
    let regs = regs(d);
    assert_eq!(regs[..3], [Some(2), Some(3), Some(4)]);
    // The sum may go where nothing is read again.
    assert_eq!(regs[3], Some(2));
}

/// Two values held at once have two registers, and one that is held no more
/// gives its register to the next. A `Drop` holds its value until it runs.
#[test]
fn values_share_a_register_when_they_are_not_held_at_once() {
    let ins = vec![
        of::constant(0),
        of::constant(1),
        of::add(2, 0, 1),
        of::len(3, 2),
        of::ret(3),
    ];
    assert_eq!(
        regs(draft(4, 0, ins, &[])),
        [Some(2), Some(3), Some(2), Some(2)]
    );
    let ins = vec![
        of::constant(0),
        of::len(1, 0),
        of::len(2, 0),
        of::drop(0),
        of::add(3, 1, 2),
        of::ret(3),
    ];
    let held = [Some(2), Some(3), Some(2), Some(2)];
    assert_eq!(regs(draft(4, 0, ins, &[])), held);
}

/// A value written while one with a higher number is held does not take its
/// register, though it is given one first.
#[test]
fn a_value_keeps_clear_of_one_with_a_higher_number() {
    let ins = vec![
        of::constant(1),
        of::constant(0),
        of::add(2, 0, 1),
        of::ret(2),
    ];
    assert_eq!(regs(draft(3, 0, ins, &[])), [Some(2), Some(3), Some(2)]);
}

/// The registers an instruction writes at once are different ones, whether or
/// not anything reads them, and one may be the register of the value it takes.
/// A field with no register has none.
#[test]
fn fields_of_one_cell_have_registers_of_their_own() {
    let ins = vec![of::open([Some(1), Some(2)], 0), of::len(3, 2), of::ret(3)];
    assert_eq!(
        regs(draft(4, 1, ins, &[])),
        [Some(2), Some(2), Some(3), Some(2)]
    );
    let ins = vec![of::open([None, Some(2)], 0), of::ret(2)];
    assert_eq!(regs(draft(3, 1, ins, &[])), [Some(2), None, Some(2)]);
    // A value that is read again has another.
    let ins = vec![
        of::open([Some(1), Some(2)], 0),
        of::add(3, 1, 2),
        of::add(4, 3, 5),
        of::ret(4),
    ];
    let regs = regs(draft(6, 1, [vec![of::constant(5)], ins].concat(), &[]));
    assert_eq!(regs[..3], [Some(2), Some(2), Some(3)]);
    assert_eq!(regs[5], Some(4));
}

/// A value that a tail call passes has the register of the parameter it is for,
/// when nothing that clashes with it has, whatever other register is free.
#[test]
fn a_value_a_tail_call_passes_has_its_parameters_register() {
    // Value 3 is for the third parameter, and reads the second.
    let steps = vec![of::add(3, 1, 1).into(), of::again(&[0, 1, 3])];
    assert_eq!(regs(draft(4, 3, steps, &[]))[3], Some(4));
    // The parameter is read again after value 3 is made, so each has the
    // other's: to have its own, one would need a register more.
    let steps = vec![
        of::add(3, 1, 1).into(),
        of::len(4, 2).into(),
        of::again(&[0, 4, 3]),
    ];
    assert_eq!(regs(draft(5, 3, steps, &[]))[3..], [Some(3), Some(4)]);
    // A parameter that some callee has and this function does not, when its
    // frame is that large anyway.
    let to = || of::enter(1, &[0, 0, 0, 1]);
    let steps = vec![of::add(1, 0, 0).into(), to()];
    assert_eq!(regs(draft(2, 1, steps, &[]))[1], Some(3));
    let mut steps: Vec<Step> = (2..6).map(|v| of::constant(v).into()).collect();
    steps.extend((2..6).map(|v| of::len(0, v).into()));
    steps.extend([of::add(1, 0, 0).into(), to()]);
    assert_eq!(regs(draft(6, 1, steps, &[]))[1], Some(5));
    // That one value's register is past the frame does not keep another from
    // its own.
    let steps = vec![
        of::add(2, 0, 0).into(),
        of::add(3, 0, 0).into(),
        of::add(4, 0, 0).into(),
        of::enter(1, &[0, 3, 4, 0, 0, 0, 2]),
    ];
    let given = regs(draft(5, 2, steps, &[]));
    assert_eq!(given[2..], [Some(5), Some(3), Some(4)]);
    // A value passed for two has the first it can.
    let steps = vec![of::add(2, 0, 0).into(), of::again(&[0, 2])];
    assert_eq!(regs(draft(3, 2, steps, &[]))[2], Some(3));
    let steps = vec![of::add(3, 0, 0).into(), of::again(&[0, 3, 3])];
    assert_eq!(regs(draft(4, 3, steps, &[]))[3], Some(3));
    let steps = vec![
        of::add(3, 0, 0).into(),
        of::again(&[0, 1, 3]),
        of::again(&[3, 1, 3]),
    ];
    assert_eq!(regs(draft(4, 3, steps, &[]))[3], Some(4));
    // The first parameter is read after value 4 is made, so it has the fourth's.
    let steps = vec![
        of::add(4, 1, 1).into(),
        of::len(5, 0).into(),
        of::again(&[4, 5, 5, 4]),
    ];
    assert_eq!(regs(draft(6, 4, steps, &[]))[4], Some(5));
    // Two values held at once want one register. The one with the lower number
    // has it, though the other's call is first.
    let steps = vec![
        of::constant(1).into(),
        of::constant(2).into(),
        of::unless(1, 0).into(),
        of::again(&[2]),
        of::again(&[1]),
    ];
    assert_eq!(regs(draft(3, 1, steps, &[4]))[1..], [Some(2), Some(3)]);
}

/// What a tail call passes has the register of its parameter before it has that of
/// what it is made from, so a loop that keeps the best so far moves only what is
/// better, whichever branch comes first.
#[test]
fn what_a_tail_call_passes_goes_before_a_jumps_moves() {
    // The parameters are 0 and 1, and 2 is what may be better.
    for (one, other) in [(2, 1), (1, 2)] {
        let steps = vec![
            of::add(2, 0, 0).into(),
            of::unless(0, 0).into(),
            of::mv(3, one).into(),
            of::jump(1).into(),
            of::mv(3, other).into(),
            of::again(&[0, 3]),
        ];
        let regs = regs(draft(4, 2, steps, &[4, 5]));
        assert_eq!(regs[2..], [Some(4), Some(3)]);
    }
    // That holds when the parameter is given by neither, too.
    let steps = vec![
        of::add(2, 0, 1).into(),
        of::add(3, 0, 1).into(),
        of::unless(0, 0).into(),
        of::mv(4, 2).into(),
        of::jump(1).into(),
        of::mv(4, 3).into(),
        of::add(0, 2, 3).into(),
        of::again(&[0, 4]),
    ];
    assert_eq!(regs(draft(5, 2, steps, &[5, 6]))[4], Some(3));
    // A call to the function itself goes before one to another.
    let steps = vec![
        of::add(2, 0, 1).into(),
        of::unless(2, 0).into(),
        of::constant(3).into(),
        of::enter(9, &[2, 3]),
        of::constant(4).into(),
        of::again(&[4, 2]),
    ];
    assert_eq!(regs(draft(5, 2, steps, &[4]))[2], Some(3));
}

/// The instructions of a loop that keeps a best, a least and a most, or a count,
/// with the branches either way round.
#[test]
fn a_loop_that_keeps_a_value_in_one_branch_moves_nothing_in_the_other() {
    let ops = |kept: &str| {
        let src = format!(
            "fn go(n Int, a Int, b Int) Int {{\n\
             \tif n == 0 {{ a + b }} else {{\n\
             \t\tx = n * 7 % 1000\n{kept}\
             \t}}\n\
             }}\n\
             pub fn main() {{ println(go(9, 0, 0)) }}\n"
        );
        let code = load(&program(&src), Audit::Never).expect("it loads");
        let ins = body(&code, named(&code, ".go"));
        let after = |op: &str| !matches!(op, "IntMulK" | "IntRemK");
        let ops: Vec<&str> = ins.iter().map(|i| i.op().name()).collect();
        // What comes before the branch is no part of it.
        let first = ops.iter().rposition(|op| !after(op)).expect("it is there");
        ops[first + 1..].join(" ")
    };
    let best = |c: &str| format!("\t\tc = {c}\n\t\tgo(n - 1, c, b)\n");
    assert_eq!(
        ops(&best("if x > a { x } else { a }")),
        "JumpUnlessIntLt Move IntSubK Jump"
    );
    assert_eq!(
        ops(&best("if a >= x { a } else { x }")),
        "JumpUnlessIntLe Jump Move IntSubK Jump"
    );
    assert_eq!(
        ops(&best("if x > a { a + 1 } else { a }")),
        "JumpUnlessIntLt IntAddK IntSubK Jump"
    );
    assert_eq!(
        ops(&best("if x > a { a } else { a + 1 }")),
        "JumpUnlessIntLt Jump IntAddK IntSubK Jump"
    );
    let both = "\t\tc = if x < a { x } else { a }\n\
                \t\td = if x > b { x } else { b }\n\
                \t\tgo(n - 1, c, d)\n";
    assert_eq!(
        ops(both),
        "JumpUnlessIntLt Move JumpUnlessIntLt Move IntSubK Jump"
    );
}

/// How many clashes are gone over to put the values of `d`'s moves together, and
/// how many it has.
fn gone_over(mut d: Draft) -> (usize, usize) {
    let held = settle(&mut d).expect("a function that loads");
    let clashes = Clashes::new(d.values as usize, &held.clashes);
    let seen = std::cell::Cell::new(0);
    let of = |v: VReg| {
        let of = clashes.of(v);
        seen.set(seen.get() + of.len());
        of
    };
    Groups::joined(&d, &of, moves(&d));
    (seen.get(), clashes.all.len())
}

/// A value that many branches give is moved into many parameters and clashes with
/// each, and a group of many values has many clashes. Neither is gone over for each
/// value that may be put with it, so a function with a great many joins is loaded
/// in time.
#[test]
fn what_clashes_with_much_is_not_gone_over_for_each_move() {
    // In each join one branch gives value 1 and the other a value of its own.
    let (mut ins, mut labels) = (Vec::new(), Vec::new());
    for i in 0..5_000 {
        let (param, made) = (2 + 2 * i, 3 + 2 * i);
        let (els, after) = (2 * i, 2 * i + 1);
        ins.extend([of::unless(0, els), of::mv(param, 1), of::jump(after)]);
        labels.push(ins.len());
        ins.extend([of::constant(made), of::mv(param, made)]);
        labels.push(ins.len());
        ins.push(of::len(made, param));
    }
    ins.push(of::ret(1));
    let (seen, all) = gone_over(draft(2 + 10_000, 2, ins, &labels));
    assert!(all > 20_000 && seen < 4 * all, "{seen} {all}");
    // One join, with as many jumps to it, all with the same value.
    let (mut ins, mut labels) = (Vec::new(), vec![0]);
    for i in 1..5_000 {
        ins.extend([of::unless(0, i), of::mv(2, 1), of::jump(0)]);
        labels.push(ins.len());
    }
    ins.push(of::constant(2));
    labels[0] = ins.len();
    ins.extend([of::len(3, 2), of::len(3, 1), of::ret(3)]);
    let (seen, all) = gone_over(draft(4, 2, ins, &labels));
    assert!(all > 5_000 && seen < 4 * all, "{seen} {all}");
    // Each value of a chain of moves is held with one value more than the last.
    let chain = 200;
    let held = |k: u32| 2 + chain + k;
    let mut ins = vec![of::constant(2)];
    for k in 1..chain {
        ins.extend([of::mv(2 + k, 1 + k), of::constant(held(k))]);
    }
    ins.extend((1..chain).map(|k| of::len(1, held(k))));
    ins.extend([of::len(1, 1 + chain), of::ret(1)]);
    let (seen, all) = gone_over(draft(2 + 2 * chain, 2, ins, &[]));
    assert!(all > 20_000 && seen < 4 * all, "{seen} {all}");
}

/// The values of a pair go to the group that has more, so a group that grows by one
/// at a time is not gone over each time.
#[test]
fn the_smaller_group_goes_to_the_larger() {
    let ins = vec![
        of::mv(1, 0),
        of::mv(2, 1),
        of::mv(3, 2),
        of::len(4, 3),
        of::ret(4),
    ];
    let mut d = draft(5, 1, ins, &[]);
    let held = settle(&mut d).expect("a function that loads");
    let clashes = Clashes::new(5, &held.clashes);
    let pairs = [(1, 0), (2, 1), (3, 2)].map(|(a, b)| (VReg(a), VReg(b)));
    for order in [[0, 1, 2], [2, 1, 0], [2, 0, 1]] {
        let pairs = order.map(|i| pairs[i]);
        let groups = Groups::joined(&d, &|v| clashes.of(v), pairs.into_iter());
        let of = |v: u32| groups.group(VReg(v));
        assert!((0..4).all(|v| of(v) == of(0)) && of(4) == 4);
        // It is the number that the first pair's group has.
        assert_eq!(of(0), pairs[0].0.0);
    }
}

/// A move that no tail call's argument is in goes with its values all the same.
#[test]
fn a_move_beside_a_tail_call_is_left_out() {
    // The register of value 2 is free where value 4 is written, and lower.
    let steps = vec![
        of::constant(2).into(),
        of::len(3, 1).into(),
        of::unless(2, 0).into(),
        of::mv(4, 3).into(),
        of::add(5, 4, 4).into(),
        of::again(&[5, 1]),
    ];
    let regs = regs(draft(6, 2, steps, &[3]));
    assert_eq!(regs[2..], [Some(2), Some(4), Some(4), Some(2)]);
}

/// What a tail call would like makes no frame larger: every call above it would
/// start that much higher, and every return give up that much more.
#[test]
fn what_a_tail_call_wants_makes_no_frame_larger() {
    // Counts down by a call that is no tail call, and has at its end a tail call
    // with many arguments.
    for (params, want) in [(4, 4), (8, 4), (201, 4)] {
        let args = [vec!["n"; params - 1], vec!["n + 1"]].concat().join(", ");
        let names = (0..params).map(|i| format!("_a{i} Int"));
        let src = format!(
            "fn wide({}) Int {{ 1 }}\n\
             fn depth(n Int) Int {{ if n == 0 {{ wide({args}) }} else {{ 1 + depth(n - 1) }} }}\n\
             pub fn main() {{ println(depth(3)) }}\n",
            names.collect::<Vec<_>>().join(", ")
        );
        let program = program(&src);
        let f = (&program.fns).into_iter().find(|f| f.name == "depth");
        let f = f.expect("depth");
        let source = Source::new(&program).expect("a whole program");
        let d = translate(&source, &f.core, None, &mut Tables::default());
        let f = fitted(d.expect("it loads")).expect("it fits");
        assert_eq!(f.own, Room(want), "{params}");
    }
    // No function of the repository's, and none made at random, has more
    // registers than the lowest one for every value comes to.
    let mut smaller = |mut d: Draft, what: &str| {
        let Ok(held) = settle(&mut d) else {
            return false;
        };
        let clashes = Clashes::new(d.values as usize, &held.clashes);
        let groups = Groups::joined(&d, &|v| clashes.of(v), moves(&d));
        let (_, end) = colour(&d, &held, &clashes, &groups, &[]).expect("it fits");
        let f = allocate(d, &held).expect("it fits");
        assert!(f.own.len() <= end, "{what}");
        let highest = f.regs.iter().flatten().map(|r| usize::from(r.0) + 1);
        assert_eq!(f.own, room(highest.max().unwrap_or(2)), "{what}");
        true
    };
    every_draft(&mut smaller);
    for seed in 0..3000u64 {
        let steps = 3 + usize::try_from(seed % 50).expect("small");
        let params = 1 + u32::try_from(seed % 9).expect("small");
        let d = random_function(seed + 900_000, params, steps);
        smaller(d, &format!("seed {seed}"));
    }
}

/// The fields of a cell taken apart go to the registers of what they are passed
/// for, also when one is the cell's own.
#[test]
fn fields_go_to_the_registers_they_are_passed_in() {
    let steps = vec![of::open([Some(2), Some(3)], 0).into(), of::again(&[3, 2])];
    assert_eq!(
        regs(draft(4, 2, steps, &[])),
        [Some(2), Some(3), Some(3), Some(2)]
    );
    // A `Drop` that does nothing does not hold the array past the push.
    let steps = vec![
        of::push(2, 0, 1).into(),
        of::drop(0).into(),
        of::again(&[2, 1]),
    ];
    assert_eq!(regs(draft(3, 2, steps, &[]))[2], Some(2));
    // One that does holds it, but not from what is written straight before it.
    let steps = vec![
        of::len(2, 0).into(),
        of::len(3, 1).into(),
        of::drop(0).into(),
        of::again(&[2, 3]),
    ];
    assert_eq!(regs(draft(4, 2, steps, &[]))[2..], [Some(4), Some(3)]);
    let steps = vec![of::len(2, 0).into(), of::drop(0).into(), of::again(&[2, 1])];
    assert_eq!(regs(draft(3, 2, steps, &[]))[2], Some(2));
}

/// A function that fits when each value takes the lowest register still fits: the
/// registers its tail calls would like are then not looked at.
#[test]
fn a_function_that_fits_with_the_lowest_registers_fits() {
    // Two values that are held together, and a third with each of them in turn,
    // where the registers the tail calls like leave no room for the rest.
    let holding = |n: u32| {
        let mut steps: Vec<Step> = (2..n).map(|v| of::constant(v).into()).collect();
        steps.extend((2..n).map(|v| of::add(0, 0, v).into()));
        steps.push(of::add(n, 1, 0).into());
        steps.push(of::enter(0, &vec![n; 250]));
        draft(n + 1, 2, steps, &[])
    };
    for n in [3, 100, 252, 253, 254] {
        let f = fitted(holding(n)).unwrap_or_else(|why| panic!("{n}: {why:?}"));
        assert!(f.own.len() <= 256, "{n}");
    }
    assert_eq!(
        fitted(holding(255)).err(),
        Some(Unfit::TooMany(Limit::Values))
    );
    // The last value is for the register of a 250th parameter, and all but five
    // of the 254 held before it are held with it. With that register set aside
    // there is one too few for them.
    let last = 254;
    let mut steps: Vec<Step> = (0..last).map(|v| of::constant(v).into()).collect();
    steps.extend((1..5).map(|v| of::add(0, 0, v).into()));
    steps.push(of::add(last, 0, 0).into());
    steps.extend((5..last).map(|v| of::add(last, last, v).into()));
    steps.push(of::enter(0, &[vec![0; 249], vec![last]].concat()));
    let d = draft(last + 1, 0, steps, &[]);
    assert_eq!(check(d, "one set aside"), Some(256));
}

/// Two values of a `Move` that could share a register still fit when no register is
/// free for both: one is held with all the rest but the first, and the other with
/// all but the last.
#[test]
fn a_function_fits_whose_moves_cannot_share() {
    let (one, two) = (507, 508);
    let mut ins: Vec<_> = (1..254).map(of::constant).collect();
    ins.push(of::len(one, 0));
    ins.extend((1..254).map(|v| of::add(one, one, v)));
    ins.push(of::mv(two, one));
    ins.extend((254..507).map(of::constant));
    ins.extend((254..507).map(|v| of::add(two, two, v)));
    ins.push(of::ret(two));
    let d = || draft(509, 1, ins.clone(), &[]);
    let regs = regs(d());
    assert_eq!(
        (regs[one as usize], regs[two as usize]),
        (Some(2), Some(255))
    );
    assert_eq!(check(d(), "two that cannot share"), Some(256));
}

/// Each arm here takes cells apart and holds a few values while it does, so any
/// number of them fit.
#[test]
fn fields_in_different_arms_share_registers() {
    let arm = |i: usize| {
        format!(
            "\t\t{i} -> match e {{\n\
             \t\t\tNum(x) -> Num(x + {i})\n\
             \t\t\tAdd(a, b) -> match a {{\n\
             \t\t\t\tNum(y) -> Add(Num(y + {i}), b)\n\
             \t\t\t\tAdd(c, d) -> Add(Add(d, c), b)\n\
             \t\t\t}}\n\
             \t\t}}\n"
        )
    };
    let src = format!(
        "type E {{\n\tNum(n Int)\n\tAdd(a E, b E)\n}}\n\
         fn f(k Int, e E) E {{\n\tmatch k {{\n{}\t\t_ -> e\n\t}}\n}}\n\
         pub fn main() {{ println(f(1, Num(1))) }}\n",
        (0..300).map(arm).collect::<String>()
    );
    let program = program(&src);
    let f = (&program.fns).into_iter().find(|f| f.name == "f");
    let f = &f.expect("f").core;
    let source = Source::new(&program).expect("a whole program");
    let d = translate(&source, f, Some(FuncIdx(0)), &mut Tables::default()).expect("it loads");
    let opened = |step: &&Step| matches!(step, Step::Ins(Ins::Open2 { .. }));
    assert!(d.steps.iter().filter(opened).count() >= 300);
    let regs = check(d, "f").expect("a function that fits");
    assert!(regs < 20, "{regs} registers");
}

#[test]
fn a_function_that_needs_too_many_registers_does_not_fit() {
    let holding = |n: u32| {
        let mut ins: Vec<_> = (1..n).map(of::constant).collect();
        ins.extend((1..n).map(|v| of::add(0, 0, v)));
        ins.push(of::ret(0));
        draft(n, 1, ins, &[])
    };
    assert_eq!(fitted(holding(254)).map(|b| b.own), Ok(Room(256)));
    assert_eq!(
        fitted(holding(255)).err(),
        Some(Unfit::TooMany(Limit::Values))
    );
    // Few values are held at once here, but the function has 300 parameters.
    let many = draft(300, 300, vec![of::ret(0)], &[]);
    assert_eq!(fitted(many).err(), Some(Unfit::TooMany(Limit::Values)));
}

impl std::fmt::Debug for Assigned {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.regs)
    }
}

fn kind(step: &Step) -> &'static str {
    match step {
        Step::Ins(ins) => ins.op().name(),
        Step::TailCall { .. } => "tail call",
    }
}

/// The kinds of what is left of `d`, and where its labels stand.
fn left(d: Draft) -> (Vec<&'static str>, Vec<usize>) {
    let f = fitted(d).expect("a function that fits");
    (f.steps.iter().map(kind).collect(), f.labels)
}

/// A `Move` from a register to itself does nothing and is left out, and so is
/// a `Jump` that then goes where the run was going anyway. A label on one
/// that went stands at the next that stayed.
#[test]
fn what_does_nothing_is_left_out() {
    let ins = vec![of::mv(1, 0), of::len(2, 1), of::ret(2)];
    assert_eq!(left(draft(3, 1, ins, &[])).0, ["ArrayLen", "Ret"]);
    let branch = |other: Ins<VReg>| {
        let ins = vec![
            of::unless(1, 0),
            of::mv(2, 0),
            of::jump(1),
            other,
            of::ret(2),
        ];
        left(draft(3, 2, ins, &[3, 4]))
    };
    let (kinds, labels) = branch(of::len(2, 0));
    assert_eq!(kinds, ["JumpIfFalse", "Jump", "ArrayLen", "Ret"]);
    assert_eq!(labels, [2, 3]);
    let (kinds, labels) = branch(of::mv(2, 0));
    assert_eq!(kinds, ["JumpIfFalse", "Ret"]);
    assert_eq!(labels, [1, 1]);
    // A write's result has its array's register, though the copy on a miss reads the
    // array there, so neither way has anything to move.
    let ins = vec![
        of::write(3, [0, 1, 2], 0),
        of::jump(1),
        of::mv(3, 0),
        of::ret(3),
    ];
    let (kinds, labels) = left(draft(4, 3, ins, &[2, 3]));
    assert_eq!(kinds, ["ArraySet", "Ret"]);
    assert_eq!(labels, [1, 1]);
    // Not when a hit reads the array too.
    let ins = vec![
        of::write(3, [0, 1, 2], 0),
        of::push(3, 3, 0),
        of::jump(1),
        of::mv(3, 0),
        of::ret(3),
    ];
    let (kinds, _) = left(draft(4, 3, ins, &[3, 4]));
    assert_eq!(kinds, ["ArraySet", "ArrayPush", "Jump", "Move", "Ret"]);
}

/// A jump over instructions that were all left out goes too, however many
/// there are, and a jump over one that stayed does not.
#[test]
fn a_jump_over_nothing_is_left_out() {
    let ins = vec![
        of::jump(0),
        of::constant(1),
        of::jump(1),
        of::constant(2),
        of::ret(0),
    ];
    let (kinds, labels) = left(draft(3, 1, ins, &[3, 4]));
    assert_eq!(kinds, ["Ret"]);
    assert_eq!(labels, [0, 0]);
    let ins = vec![of::jump(0), of::len(1, 0), of::ret(0)];
    let (kinds, labels) = left(draft(2, 1, ins, &[2]));
    assert_eq!(kinds, ["Jump", "ArrayLen", "Ret"]);
    assert_eq!(labels, [2]);
}

/// A set of registers knows what is in it, and gives them from the highest down.
#[test]
fn a_set_of_registers_gives_the_highest_first() {
    let mut set = RegSet::default();
    let regs = [255u8, 192, 191, 128, 127, 64, 63, 1, 0];
    for r in regs {
        assert!(!set.has(Reg(r)));
        set.add(Reg(r));
        assert!(set.has(Reg(r)));
    }
    set.remove(Reg(64));
    set.remove(Reg(65));
    let mut more = RegSet::default();
    more.add(Reg(70));
    set.add_all(&more);
    let mut down = Vec::new();
    while let Some(Reg(r)) = set.take_highest() {
        down.push(r);
    }
    assert_eq!(down, [255, 192, 191, 128, 127, 70, 63, 1, 0]);
    assert!((0..=u8::MAX).all(|r| !set.has(Reg(r))));
}

#[test]
fn the_lowest_free_register_is_found_at_every_word() {
    for n in [0u8, 1, 63, 64, 127, 128, 191, 192, 254] {
        let mut taken = RegSet::default();
        (0..n).for_each(|r| taken.add(Reg(r)));
        assert_eq!(taken.lowest_free(), usize::from(n));
        taken.add(Reg(n));
        assert_eq!(taken.lowest_free(), usize::from(n) + 1);
    }
    let mut all = RegSet::default();
    (0..=u8::MAX).for_each(|r| all.add(Reg(r)));
    assert_eq!(all.lowest_free(), 256);
}

/// What a register holds, as far as the code before it says on every path.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Holds {
    /// This value.
    Value(VReg),
    /// This value, or nothing once an instruction took it.
    MaybeTaken(VReg),
    /// Nothing, since an instruction took this value.
    Taken(VReg),
}

impl Holds {
    fn value(self) -> VReg {
        match self {
            Holds::Value(v) | Holds::MaybeTaken(v) | Holds::Taken(v) => v,
        }
    }
}

/// What every register holds. One that is not here holds nothing a path may
/// rely on.
type State = BTreeMap<u8, Holds>;

fn meet(a: &State, b: &State) -> State {
    let mut out = State::new();
    for (r, x) in a {
        let held = match b.get(r) {
            Some(y) if x == y => Some(*x),
            Some(y) if x.value() == y.value() => Some(Holds::MaybeTaken(x.value())),
            _ => None,
        };
        out.extend(held.map(|h| (*r, h)));
    }
    out
}

/// Whether every way from step `at` of `d` comes to a return, or to a tail call of
/// a function value, past nothing but steps that make no cell and call nothing.
/// Those that `idle` says do nothing do not count.
fn returns_soon(d: &Draft, idle: &[bool], at: usize) -> bool {
    let brief = |name: &str| {
        let plain = ["Const", "Move", "Not", "Field", "Jump", "JumpIfFalse"];
        let families = ["Open", "Float", "JumpUnlessInt", "JumpUnlessFloat"];
        let int = ["IntEq", "IntNe", "IntLt", "IntLe", "IntGt", "IntGe"];
        let int = int.iter().any(|op| name == *op || name == format!("{op}K"));
        let one = plain.contains(&name) || ["JumpUnlessTag", "Switch"].contains(&name) || int;
        one || families.iter().any(|family| name.starts_with(family))
    };
    let mut work = vec![at];
    let mut seen = vec![false; d.steps.len()];
    while let Some(at) = work.pop() {
        if std::mem::replace(&mut seen[at], true) {
            continue;
        }
        let step = &d.steps[at];
        if !idle[at] {
            let name = kind(step);
            if name.starts_with("Ret") || name.starts_with("TailCallValue") {
                continue;
            }
            if !brief(name) {
                return false;
            }
        }
        step.targets(&d.lists, &mut |l| work.push(d.labels[l.0 as usize]));
        work.extend(step.falls_through().then_some(at + 1));
    }
    true
}

/// Run the registers `f` gave `d`'s values along every path of `d`, and say
/// what is wrong when a register does not hold the value a step reads there.
/// `d` is the function before anything was left out of it, and `idle` says which
/// of its steps are to be.
fn wrong_register(d: &Draft, f: &Assigned, idle: &[bool]) -> Option<String> {
    let reg = |v: VReg| f.regs[v.0 as usize].map(|r| r.0);
    let mut at_label: Vec<Option<State>> = vec![None; d.labels.len()];
    let arrive = |at_label: &mut Vec<Option<State>>, l: usize, s: &State| {
        let met = match &at_label[l] {
            Some(t) => meet(t, s),
            None => s.clone(),
        };
        at_label[l] = Some(met);
    };
    let mut state: Option<State> = Some(State::new());
    for v in (0..d.values).map(VReg) {
        // The first two are the frame's link and closure, and its callee's
        // are past its last.
        if let Some(r) = reg(v)
            && !(2..f.own.0).contains(&u16::from(r))
        {
            return Some(format!("{v:?} is in r{r} of {:?}", f.own));
        }
    }
    for (p, r) in d.params.iter().zip(2..=u8::MAX) {
        if reg(*p) != Some(r) {
            return Some(format!("{p:?} is in {:?} and arrives in r{r}", reg(*p)));
        }
        let first = state.as_mut().expect("the start");
        first.insert(r, Holds::Value(*p));
    }
    for (i, step) in d.steps.iter().enumerate() {
        for (l, at) in d.labels.iter().enumerate() {
            if *at == i
                && let Some(s) = &at_label[l]
            {
                state = Some(state.map_or(s.clone(), |t| meet(&t, s)));
            }
        }
        // Nothing reaches this.
        let Some(now) = &mut state else { continue };
        let mut wrong = None;
        let mut taken = Vec::new();
        let mut written = Vec::new();
        let mut jumps = Vec::new();
        step.targets(&d.lists, &mut |l| jumps.push(l.0 as usize));
        step.operands(&d.lists, &mut |v, role| {
            let Some(r) = reg(v) else {
                if !idle[i] {
                    wrong = Some(format!("{i} names {v:?}, which has no register"));
                }
                return;
            };
            // A drop that is left out does nothing, and the register may be
            // another value's by then. A return gives up every register.
            if role == Role::Drop && idle[i] {
                if returns_soon(d, idle, i) {
                    return;
                }
                for (r, held) in now.iter() {
                    if held.value() == v && *held != Holds::Taken(v) {
                        wrong = Some(format!("{i} drops {v:?}, which is in r{r}: {now:?}"));
                    }
                }
                return;
            }
            match role {
                Role::Write | Role::WriteOnHit => written.push((r, v)),
                Role::Drop => {
                    if now.get(&r).is_none_or(|h| h.value() != v) {
                        wrong = Some(format!("{i} drops {v:?} in r{r}: {now:?}"));
                    }
                    taken.push((r, v));
                }
                Role::Look | Role::Share | Role::Pass { .. } | Role::Take => {
                    if now.get(&r) != Some(&Holds::Value(v)) {
                        wrong = Some(format!("{i} reads {v:?} in r{r}: {now:?}"));
                    }
                    if matches!(role, Role::Take | Role::Pass { moved: true }) {
                        taken.push((r, v));
                    }
                }
            }
        });
        if wrong.is_some() {
            return wrong;
        }
        let mut regs: Vec<u8> = written.iter().map(|(r, _)| *r).collect();
        regs.sort_unstable();
        regs.dedup();
        if regs.len() < written.len() {
            return Some(format!("{i} writes a register twice: {written:?}"));
        }
        // A jump takes nothing and writes nothing.
        for l in jumps {
            arrive(&mut at_label, l, now);
        }
        for (r, v) in taken {
            now.insert(r, Holds::Taken(v));
        }
        for (r, v) in written {
            now.insert(r, Holds::Value(v));
        }
        if !step.falls_through() {
            state = None;
        }
    }
    None
}

/// Where the steps left in `f` and its labels put each jump, check that it goes
/// to what ran there before anything was left out.
fn wrong_jump(d: &Draft, f: &Assigned, idle: &[bool]) -> Option<String> {
    let reg = |v: VReg| f.regs[v.0 as usize];
    // The kinds each label leads to, with what does nothing passed over.
    let follow = |steps: &[Step], idle: &dyn Fn(usize) -> bool, labels: &[usize], from: usize| {
        let mut out = Vec::new();
        let mut at = from;
        while out.len() < 12 {
            let Some(step) = steps.get(at) else { break };
            match step {
                Step::Ins(Ins::Jump { to }) => {
                    at = labels[to.0 as usize];
                    continue;
                }
                Step::Ins(Ins::Move { dst, src }) if reg(dst.0) == reg(src.reg) => {}
                _ if idle(at) => {}
                step => out.push(kind(step)),
            }
            at += 1;
            if !step.falls_through() {
                break;
            }
        }
        out
    };
    for l in 0..d.labels.len() {
        let before = follow(&d.steps, &|i| idle[i], &d.labels, d.labels[l]);
        let now = follow(&f.steps, &|_| false, &f.labels, f.labels[l]);
        if before != now {
            return Some(format!("label {l} led to {before:?} and leads to {now:?}"));
        }
    }
    let before = follow(&d.steps, &|i| idle[i], &d.labels, 0);
    let now = follow(&f.steps, &|_| false, &f.labels, 0);
    (before != now).then(|| format!("the start led to {before:?} and leads to {now:?}"))
}

/// Check every register given to `d`. Gives how many registers that is, or
/// `None` when it does not fit.
fn check(mut d: Draft, what: &str) -> Option<usize> {
    let held = match settle(&mut d) {
        Ok(held) => held,
        Err(Unfit::TooMany(Limit::Values)) => return None,
        Err(why) => panic!("{what}: {why:?}"),
    };
    let before = copy_of(&d);
    let f = match allocate(d, &held) {
        Ok(f) => f,
        Err(Unfit::TooMany(Limit::Values)) => return None,
        Err(why) => panic!("{what}: {why:?}"),
    };
    let idle = idle(&held, &f.regs);
    if let Some(wrong) = wrong_register(&before, &f, &idle) {
        panic!("{what}: {wrong}");
    }
    if let Some(wrong) = wrong_jump(&before, &f, &idle) {
        panic!("{what}: {wrong}");
    }
    Some(f.own.len())
}

/// Every function of every program in the repository that is one file, and of
/// the stdlib modules they import: each register a step reads holds the value it
/// is to read, on every path that reaches it.
#[test]
fn every_register_read_holds_its_value_in_every_program() {
    every_draft(&mut |d, what| check(d, what).is_some());
}

/// The same for functions made at random: straight code, branches, lookups
/// and writes that miss, cells taken apart, and paths that leave early.
#[test]
fn every_register_read_holds_its_value_in_random_functions() {
    let mut fitted = 0;
    for seed in 0..3000u64 {
        let steps = 5 + usize::try_from(seed % 60).expect("small");
        let params = 1 + u32::try_from(seed % 7).expect("small");
        let d = random_function(seed, params, steps);
        fitted += usize::from(check(d, &format!("seed {seed}")).is_some());
    }
    assert!(fitted > 2900, "{fitted} fit");
    // Long ones, most of which hold too much at once.
    for seed in 0..40u64 {
        check(random_function(seed, 3, 600), &format!("long seed {seed}"));
    }
}

/// The check finds a register given to two values held at once, which is the
/// mistake it is for.
#[test]
fn a_register_shared_by_mistake_is_found() {
    type Wrong = fn(&mut Assigned, &mut Vec<bool>);
    let wrong = |mut d: Draft, wrong: Wrong| {
        let held = settle(&mut d).expect("a function that loads");
        let before = copy_of(&d);
        let mut f = allocate(d, &held).expect("a function that fits");
        let mut idle = idle(&held, &f.regs);
        assert_eq!(wrong_register(&before, &f, &idle), None);
        wrong(&mut f, &mut idle);
        assert!(wrong_register(&before, &f, &idle).is_some());
    };
    let sum = || {
        vec![
            of::constant(0),
            of::constant(1),
            of::add(2, 0, 1),
            of::ret(2),
        ]
    };
    wrong(draft(3, 0, sum(), &[]), |f, _| f.regs[1] = f.regs[0]);
    // And one given to two values that are written at once.
    let ins = vec![of::open([Some(1), Some(2)], 0), of::ret(1)];
    wrong(draft(3, 1, ins, &[]), |f, _| f.regs[2] = f.regs[1]);
    // And one of two arguments of a tail call.
    let steps = vec![of::len(1, 0).into(), of::again(&[0, 1])];
    wrong(draft(2, 1, steps, &[]), |f, _| f.regs[1] = f.regs[0]);
    // And a drop left out that still finds its value.
    let ins = vec![
        of::len(1, 0),
        of::constant(2),
        of::drop(0),
        of::add(3, 1, 2),
        of::ret(3),
    ];
    wrong(draft(4, 1, ins, &[]), |f, idle| {
        idle[2] = true;
        f.regs[2] = Some(Reg(9));
    });
    // And a value read after it was taken.
    let mut d = draft(3, 1, vec![of::mv(1, 0), of::len(2, 0), of::ret(1)], &[]);
    let held = settle(&mut d).expect("a function that loads");
    if let Step::Ins(Ins::Move { src, .. }) = &mut d.steps[0] {
        *src = Arg::moved(src.reg);
    }
    let before = copy_of(&d);
    let f = allocate(d, &held).expect("a function that fits");
    assert!(wrong_register(&before, &f, &held.idle).is_some());
}

/// A function's instructions, values and labels are each gone over a fixed
/// number of times, so twice as long a function makes twice as many clashes
/// and no more. The function here holds a few values at once, however long it
/// is.
#[test]
fn a_long_function_makes_no_more_clashes_than_its_length() {
    let chain = |n: u32| {
        let mut ins = vec![of::constant(0), of::constant(1)];
        ins.extend((2..n).map(|v| of::add(v, v - 1, v - 2)));
        ins.push(of::ret(n - 1));
        let mut d = draft(n, 0, ins, &[]);
        settle(&mut d).expect("a function that loads").clashes.len()
    };
    let (short, long) = (chain(1_000), chain(50_000));
    assert!(long < 51 * short, "{short} {long}");
    assert!(long < 2 * 50_000, "{long}");
}
