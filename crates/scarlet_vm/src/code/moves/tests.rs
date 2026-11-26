use super::super::Op;
use super::super::alloc::allocate;
use super::super::ins::{Src, Table};
use super::super::live::settle;
use super::super::tests::programs::every_draft;
use super::super::tests::random::{Random, random_function, variant};
use super::super::tests::{copy_of, draft, of, same_step, targets};
use super::super::translate::Draft;
use super::*;
use crate::stop::Limit;
use crate::value::{Tag, Value};
use scarlet_ir::TypeId;
use scarlet_ir::core_ir::VariantRef;

fn expand_it(mut d: Draft) -> Result<Body, Unfit> {
    let held = settle(&mut d)?;
    expand(allocate(d, &held)?)
}

/// `d` as far as instructions, or `None` when it holds too much.
fn expanded(d: Draft) -> Option<Body> {
    match expand_it(d) {
        Ok(body) => Some(body),
        Err(Unfit::TooMany(Limit::Values)) => None,
        Err(why) => panic!("{why:?}"),
    }
}

/// The instructions of `body`, each as its kind and its registers, those it
/// writes first. A register it takes has a `!`, and where it jumps is last.
fn shown(body: &Body) -> Vec<String> {
    let show = |ins: &Ins<VReg>| {
        let mut regs: Vec<String> = Vec::new();
        ins.operands(&body.lists, &mut |v, role| {
            let r = body.regs[v.0 as usize].expect("a register").0;
            let taken = matches!(role, Role::Take | Role::Pass { moved: true });
            regs.push(format!("{r}{}", if taken { "!" } else { "" }));
        });
        let to = targets(ins, &body.lists);
        let to = to
            .iter()
            .map(|l| format!("-> {}", body.labels[l.0 as usize]));
        let kind = ins.op().name().to_string();
        let parts: Vec<String> = [kind].into_iter().chain(regs).chain(to).collect();
        parts.join(" ")
    };
    body.ins.iter().map(show).collect()
}

/// What a tail call of a function of `params` parameters becomes, where value
/// `v` is in register `v + 2`. Every value may hold a cell.
fn tail_call(params: u32, values: u32, step: Step) -> Body {
    let made = (params..values).map(|v| of::push(v, 0, 0).into());
    let steps: Vec<Step> = made.chain([step]).collect();
    let f = Assigned {
        steps,
        lists: Lists::default(),
        labels: Vec::new(),
        regs: (0..values).map(|v| param(v as usize)).collect(),
        kinds: vec![Some(Makes::Any); values as usize],
        params: (0..params).map(VReg).collect(),
        words: Vec::new(),
        own: room(values as usize + 2),
        closure: ReadsClosure::Nothing,
    };
    let mut body = expand(f).expect("it expands");
    body.ins.drain(..(values - params) as usize);
    body
}

/// Arguments that are in their parameters' registers take only the jump, one
/// that is elsewhere takes a move, and two or three at once one move.
#[test]
fn a_tail_call_moves_what_is_not_in_place() {
    let moves = |params, values, args: &[u32]| shown(&tail_call(params, values, of::again(args)));
    assert_eq!(moves(2, 2, &[0, 1]), ["Jump -> 0"]);
    assert_eq!(moves(0, 0, &[]), ["Jump -> 0"]);
    assert_eq!(moves(2, 3, &[0, 2]), ["Move 3 4!", "Jump -> 0"]);
    assert_eq!(moves(2, 4, &[2, 3]), ["Move2 2 3 4! 5!", "Jump -> 0"]);
    let three = moves(3, 6, &[3, 4, 5]);
    assert_eq!(three, ["Move3 2 3 4 5! 6! 7!", "Jump -> 0"]);
    let five = moves(5, 10, &[5, 6, 7, 8, 9]);
    assert_eq!(
        five,
        ["Move3 2 3 4 7! 8! 9!", "Move2 5 6 10! 11!", "Jump -> 0"]
    );
    // A chain goes from its end: nothing is written while it is still read.
    let chain = moves(3, 4, &[1, 2, 3]);
    assert_eq!(chain, ["Move3 2 3 4 3! 4! 5!", "Jump -> 0"]);
    let chain = moves(3, 4, &[3, 0, 1]);
    assert_eq!(chain, ["Move3 4 3 2 3! 2! 5!", "Jump -> 0"]);
}

/// Registers that trade places, or go round, are read before one is written.
#[test]
fn a_cycle_is_turned_with_no_register_to_spare() {
    let moves = |args: &[u32]| {
        let n = u32::try_from(args.len()).expect("few");
        shown(&tail_call(n, n, of::again(args)))
    };
    assert_eq!(moves(&[1, 0]), ["Move2 2 3 3! 2!", "Jump -> 0"]);
    assert_eq!(moves(&[1, 2, 0]), ["Move3 2 3 4 3! 4! 2!", "Jump -> 0"]);
    assert_eq!(
        moves(&[1, 2, 3, 0]),
        ["Move3 2 3 4 3! 4! 2!", "Move2 4 5 5! 4!", "Jump -> 0"]
    );
    assert_eq!(
        moves(&[1, 0, 3, 2]),
        ["Move2 2 3 3! 2!", "Move2 4 5 5! 4!", "Jump -> 0"]
    );
}

/// A value passed twice is moved once and shared the other times, and one that
/// stays where it is is only shared.
#[test]
fn a_value_passed_twice_is_shared_once() {
    let moves = |params, values, args: &[u32]| shown(&tail_call(params, values, of::again(args)));
    assert_eq!(moves(2, 3, &[2, 2]), ["Move 2 4", "Move 3 4!", "Jump -> 0"]);
    assert_eq!(moves(2, 2, &[0, 0]), ["Move 3 2", "Jump -> 0"]);
    assert_eq!(moves(2, 2, &[1, 1]), ["Move 2 3", "Jump -> 0"]);
    // One of two that trade places goes to a third as well.
    assert_eq!(
        moves(3, 3, &[1, 0, 0]),
        ["Move 4 2", "Move2 2 3 3! 2!", "Jump -> 0"]
    );
}

/// Every other register that may hold a cell is given up before anything moves,
/// from the top down, and nothing else is.
#[test]
fn a_tail_call_gives_up_what_it_does_not_pass() {
    let moves = |params, values, step| shown(&tail_call(params, values, step));
    assert_eq!(
        moves(1, 4, of::again(&[2])),
        ["Drop 5", "Drop 3", "Move 2 4!", "Jump -> 0"]
    );
    // A parameter past those the callee has, and one it writes.
    assert_eq!(
        moves(3, 3, of::enter(5, &[1])),
        ["Drop 4", "Move 2 3!", "Enter"]
    );
    assert_eq!(
        moves(2, 2, of::enter(5, &[])),
        ["Drop 3", "Drop 2", "Enter"]
    );
    // More parameters than this function has registers.
    assert_eq!(
        moves(1, 1, of::enter(5, &[0, 0, 0])),
        ["Move 3 2", "Move 4 2", "Enter"]
    );
}

/// A tail call to another function leaves the closure the frame ran as, after
/// everything else, and one to self keeps it.
#[test]
fn a_tail_call_to_another_function_gives_up_the_closure() {
    let all = [
        (ReadsClosure::Nothing, false),
        (ReadsClosure::Itself, true),
        (ReadsClosure::Captures, true),
    ];
    for (closure, keeps) in all {
        let of = |step| {
            let mut d = draft(2, 2, vec![step], &[]);
            d.closure = closure;
            shown(&expanded(d).expect("it fits"))
        };
        assert_eq!(of(of::again(&[1, 0])), ["Move2 2 3 3! 2!", "Jump -> 0"]);
        let left: &[&str] = if keeps { &["Drop 1"] } else { &[] };
        let want = [&["Move2 2 3 3! 2!"], left, &["Enter"]].concat();
        assert_eq!(of(of::enter(0, &[1, 0])), want);
    }
}

/// What an instruction that gives no cell, or a number, left in a register is
/// known, along every way there.
#[test]
fn only_a_register_that_may_hold_a_cell_is_given_up() {
    let shown = |steps: Vec<Step>, labels: &[usize]| {
        shown(&expanded(draft(9, 2, steps, labels)).expect("it fits"))
    };
    // A constant has no count, and a sum may be a big Int, which stays in its
    // register while the function goes round.
    let unused = |ins: Ins<VReg>, end: Step| {
        let steps = vec![ins.into(), of::unless(2, 0).into(), of::ret(0).into(), end];
        shown(steps, &[3]).split_off(3)
    };
    let (again, enter) = (|| of::again(&[0, 1]), || of::enter(3, &[0, 1]));
    assert_eq!(unused(of::constant(2), again()), ["Jump -> 0"]);
    assert_eq!(unused(of::constant(2), enter()), ["Enter"]);
    assert_eq!(unused(of::add(2, 0, 1), again()), ["Jump -> 0"]);
    assert_eq!(unused(of::add(2, 0, 1), enter()), ["Drop 4", "Enter"]);
    assert_eq!(unused(of::push(2, 0, 1), again()), ["Drop 4", "Jump -> 0"]);
    // It stays there too when the function can leave for another, which gives
    // it up then: what a turn left would be where that one does not look.
    let both = |first: Step, second: Step| {
        let steps = vec![
            of::add(2, 0, 1).into(),
            of::unless(2, 0).into(),
            first,
            second,
        ];
        shown(steps, &[3])
    };
    let want = ["IntAdd 4 2 3", "JumpIfFalse 4 -> 3"];
    let left = both(again(), again());
    assert_eq!(left, [&want[..], &["Jump -> 0", "Jump -> 0"]].concat());
    let left = both(again(), enter());
    assert_eq!(left[2..], ["Jump -> 0", "Drop 4", "Enter"]);
    let left = both(enter(), again());
    assert_eq!(left[2..], ["Drop 4", "Enter", "Jump -> 0"]);
    // A cell on one way there is a cell where they meet.
    let meet = |one: Ins<VReg>, other: Ins<VReg>| {
        let steps = vec![
            of::unless(1, 0).into(),
            one.into(),
            of::jump(1).into(),
            other.into(),
            of::unless(2, 2).into(),
            of::ret(0).into(),
            of::again(&[0, 1]),
        ];
        shown(steps, &[3, 4, 6]).split_off(6)
    };
    let (cell, none) = (of::push(2, 0, 1), of::constant(2));
    assert_eq!(meet(none, none), ["Jump -> 0"]);
    assert_eq!(meet(cell, none), ["Drop 4", "Jump -> 0"]);
    assert_eq!(meet(none, cell), ["Drop 4", "Jump -> 0"]);
    // What one turn gives up says nothing of a place it does not reach.
    let steps = vec![
        of::unless(1, 0).into(),
        cell.into(),
        of::again(&[0, 1]),
        of::again(&[0, 1]),
    ];
    let left = shown(steps, &[3]);
    assert_eq!(left[2..], ["Drop 4", "Jump -> 0", "Jump -> 0"]);
    // A copy of what is no cell is no cell.
    let steps = vec![
        none.into(),
        of::mv(3, 2).into(),
        of::unless(2, 0).into(),
        of::unless(3, 0).into(),
        of::enter(3, &[0, 1]),
    ];
    let left = shown(steps, &[4]);
    assert_eq!(left[left.len() - 2..], ["JumpIfFalse 5 -> 4", "Enter"]);
}

/// A way out to another function gives up the big Ints that any turn may have
/// left, whichever way that turn went, and no other. A loop's exit pays for them
/// and its turns do not.
#[test]
fn the_way_out_of_a_loop_gives_up_what_its_turns_left() {
    let ints = |of: std::ops::Range<u32>| of.map(|v| of::add(v, 0, 1).into());
    let keep = |of: std::ops::Range<u32>| of.map(|v| of::unless(v, 0).into());
    // The exit comes first, as it does in most loops.
    let body = |made: std::ops::Range<u32>, args: &[u32]| {
        let mut steps: Vec<Step> = vec![of::unless(0, 0).into(), of::enter(1, &[0, 1])];
        steps.extend(ints(made.clone()).chain(keep(made.clone())));
        steps.extend([of::again(args), of::ret(0).into()]);
        let end = steps.len() - 1;
        let mut d = draft(made.end, 2, steps, &[2, end]);
        // Those that are read again go on to the function's end.
        for step in &mut d.steps {
            if let Step::Ins(Ins::JumpIfFalse { cond, to }) = step
                && cond.0.0 >= 2
            {
                *to = Label(1);
            }
        }
        shown(&expanded(d).expect("it fits"))
    };
    let out = body(2..4, &[0, 1]);
    let turn = ["IntAdd 4 2 3", "IntAdd 5 2 3"];
    assert_eq!(
        out[1..6],
        ["Drop 5", "Drop 4", "Enter", turn[0], turn[1]].map(String::from)
    );
    assert_eq!(out[8..], ["Jump -> 0", "Ret 2!"]);
    // One that the turn passes is not left, and neither is what it is put over.
    let out = body(2..4, &[2, 1]);
    assert_eq!(out[1..3], ["Drop 5", "Enter"]);
    let out = body(2..4, &[2, 3]);
    assert_eq!(out[1], "Enter");
    // Many at once are one instruction.
    let out = body(2..5, &[0, 1]);
    assert_eq!(out[1..3], ["DropWide 6 5 4", "Enter"]);
    // Each of two turns leaves its own, and gives up the array it made.
    let two = |out_to: Step| {
        let steps = vec![
            of::unless(0, 0).into(),
            out_to,
            of::unless(1, 1).into(),
            of::add(2, 0, 1).into(),
            of::again(&[0, 1]),
            of::push(3, 0, 1).into(),
            of::add(4, 0, 1).into(),
            of::unless(3, 2).into(),
            of::again(&[0, 1]),
            of::ret(0).into(),
        ];
        let body = expanded(draft(5, 2, steps, &[2, 5, 9])).expect("it fits");
        let lines = shown(&body).into_iter();
        lines
            .filter(|line| line.starts_with("Drop"))
            .collect::<Vec<_>>()
    };
    assert_eq!(two(of::enter(1, &[0, 1])), ["Drop 5", "Drop 4", "Drop 4"]);
    assert_eq!(two(of::ret(0).into()), ["Drop 4"]);
    // What a way out gave up, no turn left.
    let steps = vec![
        of::unless(0, 0).into(),
        of::add(2, 0, 1).into(),
        of::enter(1, &[0, 1]),
        of::unless(1, 1).into(),
        of::enter(2, &[0, 1]),
        of::again(&[0, 1]),
    ];
    let out = shown(&expanded(draft(3, 2, steps, &[3, 5])).expect("it fits"));
    assert_eq!(out[2..4], ["Drop 4", "Enter"]);
    assert_eq!(out[5..], ["Enter", "Jump -> 0"]);
}

/// More than a few registers are given up by one instruction, from the top
/// down, and tail calls that give up the same ones name the same list.
#[test]
fn many_registers_are_given_up_at_once() {
    let body = tail_call(6, 6, of::enter(1, &[2]));
    assert_eq!(shown(&body), ["DropWide 7 6 5 3", "Move 2 4!", "Enter"]);
    let body = tail_call(5, 5, of::enter(1, &[2]));
    assert_eq!(shown(&body), ["DropWide 6 5 3", "Move 2 4!", "Enter"]);
    let body = tail_call(4, 4, of::enter(1, &[2]));
    assert_eq!(shown(&body), ["Drop 5", "Drop 3", "Move 2 4!", "Enter"]);
    let steps = vec![
        of::unless(0, 0).into(),
        of::enter(1, &[0]),
        of::unless(1, 1).into(),
        of::enter(2, &[0]),
        of::enter(3, &[1]),
    ];
    let body = expanded(draft(6, 6, steps, &[2, 4])).expect("it fits");
    let lists: Vec<u32> = body
        .ins
        .iter()
        .filter_map(|ins| match ins {
            Ins::DropWide { regs } => Some(regs.0),
            _ => None,
        })
        .collect();
    assert_eq!(lists, [0, 0, 1]);
    assert_eq!(body.lists.regs.len(), 2);
}

/// A tail call whose argument has no register, or that has more arguments than
/// there are registers, is refused.
#[test]
fn a_tail_call_with_an_argument_that_has_no_place_is_refused() {
    let call = |regs: Vec<Option<Reg>>, step: Step| {
        let f = Assigned {
            steps: vec![step],
            lists: Lists::default(),
            labels: Vec::new(),
            regs,
            kinds: vec![Some(Makes::Any); 2],
            params: vec![VReg(0), VReg(1)],
            words: Vec::new(),
            own: room(4),
            closure: ReadsClosure::Nothing,
        };
        expand(f).map(|body| shown(&body)).err()
    };
    let regs = |n: usize| (0..n).map(param).collect::<Vec<_>>();
    let lost = Some(Unfit::Broken(Broken::NoPlace));
    for step in [of::again, |args: &[u32]| of::enter(3, args)] {
        assert_eq!(call(regs(2), step(&[0, 1])), None);
        assert_eq!(call(vec![param(0), None], step(&[0, 1])), lost);
        assert_eq!(call(vec![param(0), None], step(&[1, 0])), lost);
        assert_eq!(call(regs(2), step(&[0, 2])), lost);
        // Register 255 is the last, and is the 254th argument's.
        assert_eq!(call(regs(1), step(&[0; 254])), None);
        assert_eq!(call(regs(1), step(&[0; 255])), lost);
    }
}

/// What a value may be by its type counts whatever made it, and what made it
/// counts whatever its type.
#[test]
fn a_value_is_what_its_type_says() {
    let left = |made: Ins<VReg>, kind: Makes, end: Step| {
        let steps = vec![made.into(), of::unless(2, 0).into(), of::ret(0).into(), end];
        let mut d = draft(3, 2, steps, &[3]);
        d.kinds[2] = Some(kind);
        shown(&expanded(d).expect("it fits")).split_off(3)
    };
    let given_up = |made, kind| {
        let turn = left(made, kind, of::again(&[0, 1]));
        let leaves = left(made, kind, of::enter(1, &[0, 1]));
        assert_eq!(turn.last().map(String::as_str), Some("Jump -> 0"));
        assert_eq!(leaves.last().map(String::as_str), Some("Enter"));
        (turn.len() > 1, leaves.len() > 1)
    };
    let call = || of::call(2, &[0]);
    assert_eq!(given_up(call(), Makes::NoCell), (false, false));
    assert_eq!(given_up(call(), Makes::Int), (false, true));
    assert_eq!(given_up(call(), Makes::Any), (true, true));
    let drop = ["Drop 4", "Jump -> 0"];
    assert_eq!(left(call(), Makes::Any, of::again(&[0, 1])), drop);
    for kind in [Makes::NoCell, Makes::Int, Makes::Any] {
        assert_eq!(given_up(of::constant(2), kind), (false, false));
    }
    assert_eq!(given_up(of::add(2, 0, 0), Makes::Any), (false, true));
    // A copy is what it is of, when its type says no more.
    let drops = |from: Makes, to: Makes| {
        let steps = vec![
            of::call(2, &[0]).into(),
            of::mv(3, 2).into(),
            of::unless(2, 0).into(),
            of::unless(3, 0).into(),
            of::enter(1, &[0, 1]),
        ];
        let mut d = draft(4, 2, steps, &[4]);
        (d.kinds[2], d.kinds[3]) = (Some(from), Some(to));
        let ins = shown(&expanded(d).expect("it fits"));
        ins.iter().filter(|i| i.starts_with("Drop")).count()
    };
    assert_eq!(drops(Makes::Any, Makes::Any), 2);
    assert_eq!(drops(Makes::Any, Makes::NoCell), 1);
    assert_eq!(drops(Makes::NoCell, Makes::Any), 0);
}

/// A parameter that the next function has no register for is given up when by
/// its type it may be a cell.
#[test]
fn a_parameter_is_what_its_type_says() {
    let out = |kind: Makes, end: Step| {
        let mut d = draft(2, 2, [end], &[]);
        d.kinds[1] = Some(kind);
        shown(&expanded(d).expect("it fits"))
    };
    let leaves = || of::enter(1, &[0]);
    assert_eq!(out(Makes::NoCell, leaves()), ["Enter"]);
    assert_eq!(out(Makes::Int, leaves()), ["Drop 3", "Enter"]);
    assert_eq!(out(Makes::Any, leaves()), ["Drop 3", "Enter"]);
    // An argument is passed whatever it is.
    for kind in [Makes::NoCell, Makes::Int, Makes::Any] {
        let turned = ["Move2 2 3 3! 2!", "Jump -> 0"];
        assert_eq!(out(kind, of::again(&[1, 0])), turned);
    }
}

/// Where a test has found a constructor with no fields, its register holds no
/// cell. Where it has found one with fields it may, and so it may where the test
/// has jumped, whichever it was for.
#[test]
fn a_constructor_with_no_fields_is_no_cell() {
    let plain = Value::nullary(VariantRef {
        type_id: TypeId(7),
        variant_idx: 1,
    });
    let found = |tag: Tag| {
        let test = Ins::JumpUnlessTag {
            src: Src(VReg(1)),
            tag,
            to: Label(0),
        };
        let steps = vec![
            test.into(),
            of::drop(1).into(),
            of::halt(0).into(),
            of::enter(1, &[0]),
        ];
        let body = expanded(draft(2, 2, steps, &[3])).expect("it fits");
        let ins = shown(&body);
        let (here, there) = ins.split_at(body.labels[0]);
        let first = |ins: &[String]| ins[0].clone();
        [first(&here[1..]), first(there)]
    };
    assert_eq!(found(plain.tag()), ["Halt 2!", "Drop 3"]);
    assert_eq!(found(Tag::of_cell(1)), ["Drop 3", "Drop 3"]);
}

/// Where only a switch goes, and only for constructors with no fields, its register
/// holds no cell. A place it goes to for others too may.
#[test]
fn a_case_for_no_fields_is_no_cell() {
    let lists = || Lists {
        tables: vec![Label(0), Label(1), Label(0)],
        ..Lists::default()
    };
    let switch = Ins::Switch {
        src: Src(VReg(1)),
        table: Table { first: 0, last: 2 },
    };
    let found = |words: &[u32]| {
        let steps = vec![switch.into(), of::enter(1, &[0]), of::enter(2, &[0])];
        let mut d = draft(2, 2, steps, &[1, 2]);
        d.lists = lists();
        d.words = words.iter().map(|l| (Label(*l), VReg(1))).collect();
        let body = expanded(d).expect("it fits");
        let ins = shown(&body);
        assert_eq!(
            ins[0],
            "Switch 3 -> 1 -> 2 -> 1".replace("2", &body.labels[1].to_string())
        );
        let (first, second) = ins[1..].split_at(body.labels[1] - 1);
        [first.len(), second.len()]
    };
    assert_eq!(found(&[]), [2, 2]);
    assert_eq!(found(&[0]), [1, 2]);
    assert_eq!(found(&[1]), [2, 1]);
    assert_eq!(found(&[0, 1]), [1, 1]);
    // Two cases at one place, which it may be in for either.
    let here = |words: &[u32]| {
        let steps = vec![switch.into(), of::enter(1, &[0])];
        let mut d = draft(2, 2, steps, &[1, 1]);
        d.lists = lists();
        d.words = words.iter().map(|l| (Label(*l), VReg(1))).collect();
        shown(&expanded(d).expect("it fits")).len()
    };
    assert_eq!(here(&[]), 3);
    assert_eq!(here(&[0]), 3);
    assert_eq!(here(&[1]), 3);
    assert_eq!(here(&[0, 1]), 2);
    // A place that the code in front of it goes on to as well.
    let into = |words: &[u32]| {
        let test = of::unless(0, 0);
        let more = of::drop(0);
        let steps = vec![test.into(), switch.into(), more.into(), of::enter(1, &[0])];
        let mut d = draft(2, 2, steps, &[2, 3]);
        d.lists = lists();
        d.words = words.iter().map(|l| (Label(*l), VReg(1))).collect();
        let ins = shown(&expanded(d).expect("it fits"));
        ins.iter().filter(|i| *i == "Drop 3").count()
    };
    assert_eq!(into(&[1]), 1);
}

/// A value that has no type is refused, whether it is written or a parameter.
#[test]
fn a_value_with_no_type_is_refused() {
    let no_type = Some(Unfit::Broken(Broken::NoType));
    for end in [of::again(&[0, 1]), of::enter(1, &[0, 1])] {
        for v in 0..3 {
            let steps = vec![of::call(2, &[0]).into(), same_step(&end)];
            let mut d = draft(3, 2, steps, &[]);
            d.kinds[v] = None;
            assert_eq!(expand_it(d).err(), no_type);
        }
        let mut short = draft(3, 2, vec![of::call(2, &[0]).into(), end], &[]);
        short.kinds.truncate(2);
        assert_eq!(expand_it(short).err(), no_type);
    }
}

/// A drop of a register that can hold no cell is left out, and the labels go on
/// to lead where they did.
#[test]
fn a_drop_of_what_is_no_cell_is_left_out() {
    let dropped = |ins: Ins<VReg>| {
        let ins = vec![of::unless(1, 0), ins, of::drop(2), of::drop(0), of::halt(1)];
        shown(&expanded(draft(3, 2, ins, &[3])).expect("it fits"))
    };
    let kept = dropped(of::push(2, 1, 1));
    assert_eq!(kept[0], "JumpIfFalse 3 -> 3");
    assert!(kept[2].starts_with("Drop 4"), "{kept:?}");
    assert_eq!(dropped(of::add(2, 1, 1))[2], "Drop 4");
    let out = dropped(of::constant(2));
    assert_eq!(out[0], "JumpIfFalse 3 -> 2");
    assert_eq!(out[2..], ["Drop 2", "Halt 3!"]);
}

/// The labels of a function whose tail calls are longer than they were lead to
/// the same instructions.
#[test]
fn labels_stand_where_they_did() {
    let steps = vec![
        of::unless(1, 0).into(),
        of::again(&[1, 0]),
        of::unless(0, 1).into(),
        of::enter(4, &[]),
        of::ret(1).into(),
    ];
    let body = expanded(draft(2, 2, steps, &[2, 4])).expect("it fits");
    assert_eq!(
        shown(&body),
        [
            "JumpIfFalse 3 -> 3",
            "Move2 2 3 3! 2!",
            "Jump -> 0",
            "JumpIfFalse 2 -> 7",
            "Drop 3",
            "Drop 2",
            "Enter",
            "Ret 3!",
        ]
    );
}

/// A register has one name, however many moves name it.
#[test]
fn a_register_is_named_once() {
    let steps = vec![
        of::unless(1, 0).into(),
        of::again(&[1, 0]),
        of::again(&[1, 0]),
    ];
    let body = expanded(draft(2, 2, steps, &[2])).expect("it fits");
    assert_eq!(body.regs.len(), 4);
}

/// What a register holds, as the tests below run moves: a value, and whether it
/// is a cell. `None` is nothing.
type Holds = Option<(u64, bool)>;

/// Run the moves in `ins` over `regs`, counting the references to each cell in
/// `counts`.
fn run_moves(body: &Body, regs: &mut [Holds], counts: &mut std::collections::BTreeMap<u64, i64>) {
    let at = |v: VReg| usize::from(body.regs[v.0 as usize].expect("a register").0);
    let gone = |old: Holds, counts: &mut std::collections::BTreeMap<u64, i64>| {
        if let Some((v, true)) = old {
            *counts.get_mut(&v).expect("a cell that is held") -= 1;
        }
    };
    for ins in &body.ins {
        match *ins {
            Ins::Move { dst, src } => {
                let v = regs[at(src.reg)];
                if src.moved {
                    regs[at(src.reg)] = None;
                } else if let Some((v, true)) = v {
                    *counts.get_mut(&v).expect("a cell that is held") += 1;
                }
                gone(std::mem::replace(&mut regs[at(dst.0)], v), counts);
            }
            Ins::Move2 { dsts, srcs } => {
                let taken = srcs.map(|s| regs[at(s.0)].take());
                for (dst, v) in dsts.iter().zip(taken) {
                    gone(std::mem::replace(&mut regs[at(dst.0)], v), counts);
                }
            }
            Ins::Move3 { dsts, srcs } => {
                let taken = srcs.map(|s| regs[at(s.0)].take());
                for (dst, v) in dsts.iter().zip(taken) {
                    gone(std::mem::replace(&mut regs[at(dst.0)], v), counts);
                }
            }
            Ins::Drop { reg } => gone(regs[at(reg.0)].take(), counts),
            Ins::DropWide { regs: all } => {
                let all = all.regs(&body.lists);
                all.iter().for_each(|r| gone(regs[at(*r)].take(), counts));
            }
            Ins::Jump { .. } | Ins::Enter { .. } => {}
            ref ins => panic!("{ins:?}"),
        }
    }
}

/// Whatever is passed, in whatever order: each parameter holds its argument,
/// no other register holds a cell, and each cell is held as often as it was
/// passed. No move is made that is not needed, and none writes what it reads.
fn is_moved_right(params: u32, values: u32, args: &[u32], own: bool) {
    let step = if own {
        of::again(args)
    } else {
        of::enter(9, args)
    };
    let body = tail_call(params, values, step);
    let mut regs: Vec<Holds> = vec![None; WINDOW];
    let mut counts = std::collections::BTreeMap::new();
    for v in 0..values {
        regs[v as usize + 2] = Some((u64::from(v), true));
        counts.insert(u64::from(v), 1);
    }
    run_moves(&body, &mut regs, &mut counts);
    let what = format!("{params} {values} {args:?}");
    for (i, v) in args.iter().enumerate() {
        assert_eq!(regs[i + 2], Some((u64::from(*v), true)), "{what}");
    }
    assert!(regs[args.len() + 2..].iter().all(Option::is_none), "{what}");
    for (v, count) in counts {
        let passed = args.iter().filter(|a| u64::from(**a) == v).count();
        assert_eq!(count, i64::try_from(passed).expect("few"), "{what}");
    }
    let moved: usize = body
        .ins
        .iter()
        .map(|ins| match ins {
            Ins::Move { .. } => 1,
            Ins::Move2 { .. } => 2,
            Ins::Move3 { .. } => 3,
            _ => 0,
        })
        .sum();
    let out_of_place = args.iter().zip(0..).filter(|(a, i)| **a != *i).count();
    // A cycle that takes more than one instruction writes a register twice for
    // each after the first.
    assert!(
        (out_of_place..=out_of_place + out_of_place / 2).contains(&moved),
        "{what}: {moved} moves for {out_of_place}"
    );
}

/// Every way to pass up to five arguments from up to seven values, to self and
/// to another function.
#[test]
fn every_small_tail_call_passes_its_arguments() {
    let mut calls = 0;
    for n in 0..=5u32 {
        let most = if n == 5 { 6 } else { 7 };
        for values in n.max(1)..=most {
            let mut args = vec![0u32; n as usize];
            loop {
                is_moved_right(n, values, &args, true);
                for params in 0..=n.min(values) {
                    is_moved_right(params, values, &args, false);
                }
                calls += 1;
                let Some(i) = args.iter().rposition(|a| *a + 1 < values) else {
                    break;
                };
                args[i] += 1;
                args[i + 1..].fill(0);
            }
        }
    }
    assert!(calls > 15_000, "{calls}");
}

/// Calls of up to 250 arguments: every one in place, the next one's, turned
/// round, swapped in pairs, all from others, one value, and any at random.
#[test]
fn every_large_tail_call_passes_its_arguments() {
    let mut random = Random(77);
    for n in [6u32, 7, 12, 50, 125, 249, 250] {
        let values = (n + 4).min(254);
        let calls: [Vec<u32>; 7] = [
            (0..n).collect(),
            (0..n).map(|i| (i + 1) % n).collect(),
            (0..n).rev().collect(),
            (0..n).map(|i| (i ^ 1).min(n - 1)).collect(),
            (0..n).map(|i| (i + 2) % values).collect(),
            vec![n / 2; n as usize],
            (0..n)
                .map(|i| if i % 3 == 0 { (i + 3) % n } else { i })
                .collect(),
        ];
        for args in calls {
            is_moved_right(n, values, &args, true);
            is_moved_right(n / 2, values, &args, false);
        }
        for _ in 0..40 {
            let one = || u32::try_from(random.below(values as usize)).expect("few");
            let args: Vec<u32> = std::iter::repeat_with(one).take(n as usize).collect();
            is_moved_right(n, values, &args, true);
            is_moved_right(n - 3, values, &args, false);
        }
    }
}

/// A callee of more parameters than this function has registers takes the
/// registers above the frame, and one of fewer leaves none of them.
#[test]
fn a_frame_may_grow_and_shrink_at_a_tail_call() {
    let args: Vec<u32> = (0..250).map(|i| i % 3).collect();
    is_moved_right(3, 3, &args, false);
    is_moved_right(254, 254, &[253, 7], false);
    is_moved_right(250, 254, &[], false);
}

/// Each tail call's moves are worked out alone: one leaves nothing behind that
/// another sees.
#[test]
fn one_tail_call_does_not_change_the_next() {
    let end = |args: &[u32]| of::again(args);
    let steps = vec![
        of::unless(0, 0).into(),
        end(&[1, 2, 0]),
        of::unless(1, 1).into(),
        end(&[0, 0, 0]),
        end(&[1, 2, 0]),
    ];
    let body = expanded(draft(3, 3, steps, &[2, 4])).expect("it fits");
    let turns = shown(&body);
    let turn = |line: &&String| line.starts_with("Move3 2 3 4 3! 4! 2!");
    assert_eq!(turns.iter().filter(turn).count(), 2, "{turns:?}");
}

/// A value in what runs below: what made it, by a number that says so, and which
/// cell it is when it is one.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Made {
    by: u64,
    cell: Option<u64>,
}

fn mix(of: impl IntoIterator<Item = u64>) -> u64 {
    of.into_iter().fold(0x9E37_79B9_7F4A_7C15, |h, x| {
        (h ^ x).wrapping_mul(0x2545_F491_4F6C_DD1D).rotate_left(29)
    })
}

/// What an instruction gives is what it gives for the values it reads, so two
/// runs that read the same give the same. This says what for `dst`th of those
/// it writes.
fn gives(ins: &Ins<VReg>, read: &[u64], dst: u64) -> u64 {
    let kind = u64::from(ins.op() as u8);
    mix(read.iter().copied().chain([kind, dst]))
}

/// Where `ins` goes instead of on to the next, when it read `read`. No value in a cell
/// goes where `words` says that there is none.
fn goes(
    ins: &Ins<VReg>,
    read: &[u64],
    lists: &Lists<VReg>,
    words: &[(Label, VReg)],
) -> Option<Label> {
    let mut to = targets(ins, lists);
    let fits = |l: &Label| read[0] % 4 == 1 || !words.iter().any(|(word, _)| word == l);
    match ins {
        Ins::Jump { .. } => to.pop(),
        Ins::Switch { table, .. } => {
            let from = usize::try_from(variant(read[0], *table)).expect("a few");
            let all = to[from..].iter().chain(&to[..from]);
            Some(all.copied().find(fits).unwrap_or(to[from]))
        }
        Ins::SwitchInt { table, .. } => {
            let first = u64::try_from(table.first).expect("a few");
            let past = (read[0] % 16).checked_sub(first);
            let last = u64::from(table.table.last);
            let at = past.unwrap_or(last).min(last);
            Some(to[usize::try_from(at).expect("a few")])
        }
        Ins::SwitchIntFar { places, to, .. } => {
            let n = i64::try_from(read[0] % 16).expect("a few");
            let all = places.places(lists).iter();
            Some(
                all.clone()
                    .find(|(k, _)| *k == n)
                    .map_or(*to, |(_, to)| *to),
            )
        }
        Ins::JumpUnlessTag { tag, .. } if !tag.is_cell() => to.pop().filter(|_| read[0] % 4 != 1),
        Ins::JumpUnlessTag { .. } => {
            let jumps = gives(ins, read, 99).is_multiple_of(3);
            to.pop().filter(|l| jumps && fits(l))
        }
        ins => to.pop().filter(|_| gives(ins, read, 99).is_multiple_of(3)),
    }
}

/// Whether a value that may be what `may` says is in a cell. Where a test can
/// find that it has no fields it is not.
fn in_a_cell(may: Makes, by: u64) -> bool {
    match may {
        Makes::NoCell => false,
        Makes::Int => by % 4 == 3,
        Makes::Any => by % 4 != 1,
    }
}

/// A run's tail calls and how it ended, each with the values it passed.
type Trace = Vec<(char, Vec<u64>)>;

const TURNS: usize = 12;

/// Run `d` over values, as it was before anything had a register.
fn run_values(d: &Draft) -> Trace {
    let mut trace = Trace::new();
    let mut values: Vec<u64> = vec![0; d.values as usize];
    for (p, i) in d.params.iter().zip(1..) {
        values[p.0 as usize] = mix([i]);
    }
    let mut at = 0;
    while trace.len() < TURNS {
        let step = &d.steps[at];
        at += 1;
        let ins = match step {
            Step::Ins(ins) => ins,
            Step::TailCall { to, args } => {
                let args: Vec<u64> = args.iter().map(|a| values[a.0 as usize]).collect();
                trace.push(('t', args.clone()));
                if *to != TailTo::Own {
                    return trace;
                }
                for (p, v) in d.params.iter().zip(args) {
                    values[p.0 as usize] = v;
                }
                at = 0;
                continue;
            }
        };
        let mut read = Vec::new();
        let mut writes = Vec::new();
        step.operands(&d.lists, &mut |v, role| match role {
            Role::Write | Role::WriteOnHit => writes.push(v),
            Role::Drop => {}
            Role::Look | Role::Share | Role::Pass { .. } | Role::Take => {
                read.push(values[v.0 as usize]);
            }
        });
        if let Some(to) = goes(ins, &read, &d.lists, &d.words) {
            at = d.labels[to.0 as usize];
            continue;
        }
        if !ins.falls_through() {
            trace.push(('r', read));
            return trace;
        }
        if let Ins::Move { dst, src } = ins {
            values[dst.0.0 as usize] = values[src.reg.0 as usize];
            continue;
        }
        for (v, i) in writes.into_iter().zip(0..) {
            values[v.0 as usize] = gives(ins, &read, i);
        }
    }
    trace
}

/// A frame's registers, and how many references there are to each cell.
struct Regs {
    regs: Vec<Option<Made>>,
    made: u64,
}

impl Regs {
    fn cells(&self) -> impl Iterator<Item = usize> {
        let cell = |(i, m): (usize, &Option<Made>)| m.and_then(|m| m.cell).map(|_| i);
        self.regs.iter().enumerate().filter_map(cell)
    }
}

/// Run `body` over registers, as the machine would, and say what is wrong: a
/// register read that is empty, or at the end of a frame a cell where nothing
/// will look for it.
fn run_regs(body: &Body, words: &[(Label, VReg)]) -> Result<Trace, String> {
    let params = body.params.len();
    let mut trace = Trace::new();
    let mut f = Regs {
        regs: vec![None; WINDOW],
        made: 0,
    };
    let at_reg = |v: VReg| usize::from(body.regs[v.0 as usize].expect("a register").0);
    let may = |v: VReg| body.kinds.get(v.0 as usize).copied().flatten();
    for (i, p) in (1..).zip(&body.params) {
        f.made += 1;
        let by = mix([i]);
        let cell = in_a_cell(may(*p).expect("a type"), by).then_some(f.made);
        f.regs[at_reg(*p)] = Some(Made { by, cell });
    }
    let own = body.own.len();
    let mut at = 0;
    while trace.len() < TURNS {
        let ins = &body.ins[at];
        let here = at;
        at += 1;
        let mut read = Vec::new();
        let mut takes = Vec::new();
        let mut writes = Vec::new();
        let mut wrong = None;
        ins.operands(&body.lists, &mut |v, role| {
            let r = at_reg(v);
            match role {
                Role::Write | Role::WriteOnHit => writes.push((v, r)),
                Role::Drop => takes.push(r),
                Role::Look | Role::Share | Role::Pass { .. } | Role::Take => {
                    match f.regs[r] {
                        Some(m) => read.push(m),
                        None => wrong = Some(format!("{here}: r{r} is empty")),
                    }
                    if matches!(role, Role::Take | Role::Pass { moved: true }) {
                        takes.push(r);
                    }
                }
            }
        });
        // Each move reads its own.
        if let Some(wrong) = wrong {
            return Err(wrong);
        }
        let by: Vec<u64> = read.iter().map(|m| m.by).collect();
        if let Ins::Jump { to } = ins {
            let to = body.labels[to.0 as usize];
            if to < at {
                let passed = (0..params).map(|i| f.regs[i + 2].map_or(0, |m| m.by));
                trace.push(('t', passed.collect()));
            }
            at = to;
            continue;
        }
        if let Ins::Enter { .. } = ins {
            // Those that hold a cell are the callee's parameters, as far as
            // there are any.
            let cells: Vec<usize> = f.cells().collect();
            let passed: Vec<u64> = (2..)
                .map_while(|r| f.regs.get(r).copied().flatten())
                .map(|m| m.by)
                .collect();
            if let Some(r) = cells.iter().find(|r| **r >= passed.len() + 2) {
                return Err(format!("{here}: r{r} is left with a cell"));
            }
            trace.push(('t', passed));
            return Ok(trace);
        }
        if let Some(to) = goes(ins, &by, &body.lists, words) {
            at = body.labels[to.0 as usize];
            continue;
        }
        if !ins.falls_through() {
            if let Some(r) = f.cells().find(|r| *r >= own) {
                return Err(format!("{here}: r{r} has a cell above the frame"));
            }
            trace.push(('r', by));
            return Ok(trace);
        }
        let moved = |m: Made, taken: bool, made: &mut u64| {
            // A share is a reference more to the same cell. Here each reference
            // is told apart, so that one that is lost or had twice shows.
            if m.cell.is_none() || taken {
                return m;
            }
            *made += 1;
            Made {
                cell: Some(*made),
                ..m
            }
        };
        let written: Vec<Made> = match ins {
            Ins::Move { src, .. } => vec![moved(read[0], src.moved, &mut f.made)],
            Ins::Move2 { .. } | Ins::Move3 { .. } => read,
            ins => (0..)
                .zip(&writes)
                .map(|(i, (v, _))| {
                    let by = gives(ins, &by, i);
                    f.made += 1;
                    let may = ins.makes().min(may(*v).expect("a type"));
                    let cell = in_a_cell(may, by).then_some(f.made);
                    Made { by, cell }
                })
                .collect(),
        };
        for r in takes {
            f.regs[r] = None;
        }
        for ((_, r), m) in writes.into_iter().zip(written) {
            f.regs[r] = Some(m);
        }
        let mut cells: Vec<u64> = f
            .regs
            .iter()
            .filter_map(|m| m.and_then(|m| m.cell))
            .collect();
        let all = cells.len();
        cells.sort_unstable();
        cells.dedup();
        if cells.len() < all {
            return Err(format!("{here}: two registers have one reference"));
        }
    }
    Ok(trace)
}

/// Check that `d` does over registers what it does over values. Gives whether
/// it fits.
fn check(mut d: Draft, what: &str) -> bool {
    let Ok(held) = settle(&mut d) else {
        return false;
    };
    let before = copy_of(&d);
    let body = allocate(d, &held).and_then(expand);
    let body = body.unwrap_or_else(|why| panic!("{what}: {why:?}"));
    let mut want = run_values(&before);
    let got = run_regs(&body, &before.words).unwrap_or_else(|wrong| panic!("{what}: {wrong}"));
    // A callee's arguments are not told from the number that follow it.
    for (w, g) in want.iter_mut().zip(&got) {
        if w.0 == 't' && g.1.len() > w.1.len() && g.1.starts_with(&w.1) {
            w.1.clone_from(&g.1);
        }
    }
    assert_eq!(want, got, "{what}: {:?}", shown(&body));
    true
}

/// Every function of every program in the repository, and functions made at
/// random, pass and return over registers what they do over values, and leave no
/// cell where the frame that follows does not look.
#[test]
fn every_function_does_over_registers_what_it_does_over_values() {
    every_draft(&mut |d, what| check(d, what));
    let mut fit = 0;
    for seed in 0..6000u64 {
        let steps = 3 + usize::try_from(seed % 50).expect("small");
        let params = 1 + u32::try_from(seed % 11).expect("small");
        let d = random_function(seed + 100_000, params, steps);
        fit += usize::from(check(d, &format!("seed {seed}")));
    }
    assert!(fit > 5900, "{fit} fit");
}

/// The run over registers finds a move that is missing or wrong, a cell left
/// behind, and a reference that two have.
#[test]
fn the_run_over_registers_finds_what_is_wrong() {
    let wrong = |steps: Vec<Step>, kind: Op, change: fn(&mut Ins<VReg>)| {
        let d = || draft(3, 2, steps.iter().map(same_step), &[]);
        let want = run_values(&d());
        let mut body = expanded(d()).expect("it fits");
        assert_eq!(run_regs(&body, &[]).as_ref(), Ok(&want));
        let found = body.ins.iter_mut().find(|i| i.op() == kind);
        change(found.expect("one of these"));
        run_regs(&body, &[]).map_or(true, |got| got != want)
    };
    let swap = || vec![of::again(&[1, 0])];
    assert!(wrong(swap(), Op::Move2, |ins| *ins = of::mv(0, 0)));
    assert!(wrong(swap(), Op::Move2, |ins| {
        if let Ins::Move2 { srcs, .. } = ins {
            srcs[1] = srcs[0];
        }
    }));
    let twice = || vec![of::push(2, 0, 1).into(), of::enter(3, &[2, 2])];
    assert!(wrong(twice(), Op::Move, |ins| {
        if let Ins::Move { src, .. } = ins {
            src.moved = true;
        }
    }));
    let left = || vec![of::push(2, 0, 1).into(), of::enter(3, &[0])];
    assert!(wrong(left(), Op::Drop, |ins| *ins = of::mv(0, 0)));
}
