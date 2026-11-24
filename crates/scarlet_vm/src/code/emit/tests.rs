use super::super::ins::ArgList;
use super::super::tests::{SHAPES, body, entries, of, program};
use super::super::{Arg, Code, Entry, load, load_counted};
use super::*;

fn is_count(ins: &Ins<Reg>) -> bool {
    matches!(ins, Ins::Count { .. })
}

/// Where instruction `pc` of the whole code is in `entry`.
fn within(entry: &Entry, pc: Pc) -> usize {
    pc.index() - entry.pc.index()
}

/// A counted load is the ordinary load with a `Count` put in at the head
/// of each block and nothing else changed: the same instructions in the
/// same order, and every jump going where it went before.
#[test]
fn a_counted_load_is_the_same_program_with_a_count_at_each_block_head() {
    let program = program(SHAPES);
    let plain = load(&program, Audit::Never).expect("the program loads");
    let counted = load_counted(&program, Audit::Never).expect("the program loads");
    assert!(!plain.ins.iter().any(is_count));

    let mut numbered = 0;
    let mut jumps = 0;
    for (was, now) in entries(&plain).into_iter().zip(entries(&counted.code)) {
        let (old, new) = (body(&plain, was), body(&counted.code, now));
        assert!(new.first().is_some_and(is_count), "{}", now.name);
        let kept: Vec<Ins<Reg>> = new.iter().copied().filter(|i| !is_count(i)).collect();
        let kinds = |ins: &[Ins<Reg>]| ins.iter().map(Ins::op).collect::<Vec<_>>();
        assert_eq!(kinds(&kept), kinds(old));
        // Where each instruction of the ordinary load sits in this one.
        let sits: Vec<usize> = (0..new.len()).filter(|i| !is_count(&new[*i])).collect();
        for (i, ins) in new.iter().enumerate() {
            if let Ins::Count { block } = ins {
                assert_eq!(*block, BlockId(numbered), "blocks are numbered in order");
                let rest = new[i + 1..].iter().take_while(|i| !is_count(i));
                let kinds: Vec<Op> = rest.map(Ins::op).collect();
                assert_eq!(*counted.blocks[numbered as usize], kinds);
                numbered += 1;
            } else if (!ins.falls_through() || ins.target().is_some())
                && let Some(next) = new.get(i + 1)
            {
                // Anything that may not fall through ends its block.
                assert!(is_count(next), "{} at {i}", now.name);
            }
        }
        for (a, b) in old.iter().zip(&kept) {
            if let (Some(a), Some(b)) = (a.target(), b.target()) {
                let lands = within(now, b);
                assert!(is_count(&new[lands]), "a jump lands on a count");
                assert_eq!(sits[within(was, a)], lands + 1, "and on the same block");
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
    let program = program(SHAPES);
    let plain = load(&program, Audit::Never).expect("the program loads");
    let audited = load(&program, Audit::EachInstruction).expect("the program loads");
    let counted = load_counted(&program, Audit::Never).expect("the program loads");
    let both = load_counted(&program, Audit::EachInstruction).expect("the program loads");
    assert_eq!(counted.blocks, both.blocks);
    // Only a run that audits before each instruction has audits to run.
    let at_end = load(&program, Audit::AtEnd).expect("the program loads");
    let counted_at_end = load_counted(&program, Audit::AtEnd).expect("the program loads");
    for code in [&plain, &at_end, &counted.code, &counted_at_end.code] {
        assert!(!code.ins.iter().any(|ins| matches!(ins, Ins::Audit)));
    }
    let mut jumps = 0;
    for (a, b) in [(&plain, &audited), (&counted.code, &both.code)] {
        for (was, now) in entries(a).into_iter().zip(entries(b)) {
            let (old, new) = (body(a, was), body(b, now));
            assert_eq!(new.len(), 2 * old.len(), "{}", now.name);
            for (old, new) in old.iter().zip(new.chunks(2)) {
                let [Ins::Audit, new] = new else {
                    panic!("{}: an audit and then the instruction", now.name);
                };
                assert_eq!(old.op(), new.op());
                if let (Some(old), Some(new)) = (old.target(), new.target()) {
                    assert_eq!(within(now, new), 2 * within(was, old));
                    jumps += 1;
                }
            }
        }
    }
    assert!(jumps > 5, "the program has jumps to check: {jumps}");
}

/// A call by name holds where its callee starts and how many registers it
/// needs, whether the callee is before it, after it or itself. Every frame
/// size an instruction holds is its own function's.
#[test]
fn a_call_holds_where_its_callee_starts() {
    let src = "fn early(n Int) Int { if n == 0 { 0 } else { late(n - 1, 2, 3) } }\n\
               fn late(a Int, b Int, c Int) Int { early(a) + b * c + late(a, b, c) }\n\
               fn round(n Int) Int { if n == 0 { 0 } else { round(n - 1) } }\n\
               pub fn main() { println(early(3) + round(2)) }\n";
    let code = load(&program(src), Audit::Never).expect("the program loads");
    let mut seen = std::collections::BTreeSet::new();
    let sizes = |entry: &Entry, own: Room| assert_eq!(own, entry.needs, "{}", entry.name);
    for entry in entries(&code) {
        for ins in body(&code, entry) {
            let called = |to: EntryPc| {
                let mut all = entries(&code).into_iter();
                all.find(|e| e.pc.index() == to.index())
                    .expect("a function's start")
            };
            match *ins {
                Ins::Call { to, needs, own, .. } | Ins::TailCall { to, needs, own, .. } => {
                    assert_eq!(called(to).needs, needs);
                    sizes(entry, own);
                    seen.insert((entry.name.clone(), called(to).name.clone()));
                }
                Ins::CallSelf { to, own, .. } | Ins::TailCallSelf { to, own, .. } => {
                    assert_eq!(called(to).name, entry.name);
                    sizes(entry, own);
                    seen.insert((entry.name.clone(), called(to).name.clone()));
                }
                Ins::Ret { own, .. } => sizes(entry, own),
                _ => {}
            }
        }
    }
    let calls = |from: &str, to: &str| {
        let pair = (format!("main.{from}"), format!("main.{to}"));
        assert!(seen.contains(&pair), "{from} calls {to}: {seen:?}");
    };
    calls("early", "late");
    calls("late", "early");
    calls("late", "late");
    calls("round", "round");
    calls("main", "round");
}

fn fitted(ins: Vec<Ins<VReg>>, regs: Vec<Option<Reg>>) -> Fitted {
    let body = Body {
        ins,
        lists: Lists::default(),
        labels: Vec::new(),
        regs,
        needs: Room(3),
    };
    Fitted::new("f".to_string(), 1, body)
}

fn ran(funcs: Vec<Fitted>) -> Result<Placed, Stop> {
    let extra = Extra {
        count: None,
        audit: Audit::Never,
    };
    emit(&funcs.into_iter().collect(), &[], extra)
}

/// Each function's lists go after those of the functions before it, and its
/// instructions name them there.
#[test]
fn each_functions_lists_are_its_own() {
    let wide = |v: u32| {
        let mut f = fitted(
            vec![Ins::ConcatWide {
                dst: super::super::Dst(VReg(0)),
                parts: super::super::LookList(0),
            }],
            vec![Some(Reg(0)), Some(Reg(1)), Some(Reg(2))],
        );
        f.body.lists.regs.push(Box::new([VReg(v), VReg(0)]));
        f.body.lists.args.push(Box::new([Arg::shared(VReg(v))]));
        f.body.ins.push(Ins::TailCallSelfWide {
            to: super::super::ins::Callee(FuncIdx(0)),
            args: ArgList(0),
            own: super::super::ins::OwnRoom,
        });
        f
    };
    let placed = ran(vec![wide(1), wide(2)]).expect("it fits");
    let mut parts = Vec::new();
    let mut args = Vec::new();
    for ins in &placed.ins {
        match ins {
            Ins::ConcatWide { parts: list, .. } => parts.push(list.regs(&placed.lists)),
            Ins::TailCallSelfWide { args: list, .. } => {
                args.push(list.args(&placed.lists)[0].reg);
            }
            _ => {}
        }
    }
    assert_eq!(parts, [[Reg(1), Reg(0)], [Reg(2), Reg(0)]]);
    assert_eq!(args, [Reg(1), Reg(2)]);
}

/// An instruction that names what was given no place is not written.
#[test]
fn what_has_no_place_is_refused() {
    let lost = Some(Stop::broken(Broken::NoPlace));
    let fails = |f: Fitted| ran(vec![f]).err();
    assert_eq!(fails(fitted(vec![of::ret(0)], vec![None])), lost);
    assert_eq!(fails(fitted(vec![of::ret(1)], vec![Some(Reg(0))])), lost);
    assert_eq!(fails(fitted(vec![of::jump(0)], Vec::new())), lost);
    let mut f = fitted(vec![of::jump(0)], Vec::new());
    f.body.labels.push(0);
    assert_eq!(fails(f), None);
    let mut f = fitted(vec![of::jump(0)], Vec::new());
    f.body.labels.push(1);
    assert_eq!(fails(f), lost, "a label past the last instruction");
    let call = |to: u32| Ins::TailCallSelf {
        to: super::super::ins::Callee(FuncIdx(to)),
        args: super::super::Args::new(&[]).expect("none"),
        own: super::super::ins::OwnRoom,
    };
    assert_eq!(fails(fitted(vec![call(0)], Vec::new())), None);
    assert_eq!(fails(fitted(vec![call(1)], Vec::new())), lost);
    let mut f = fitted(vec![of::ret(0)], vec![Some(Reg(0))]);
    f.body.lists.args.push(Box::new([Arg::shared(VReg(9))]));
    assert_eq!(fails(f), lost);
    // A list the function does not have, though one before it has that many.
    let listed = |regs: u32, args: u32| {
        let mut f = fitted(
            vec![
                Ins::ConcatWide {
                    dst: super::super::Dst(VReg(0)),
                    parts: super::super::LookList(regs),
                },
                Ins::TailCallSelfWide {
                    to: super::super::ins::Callee(FuncIdx(0)),
                    args: ArgList(args),
                    own: super::super::ins::OwnRoom,
                },
            ],
            vec![Some(Reg(0))],
        );
        f.body.lists.regs.push(Box::new([VReg(0)]));
        f.body.lists.args.push(Box::new([Arg::shared(VReg(0))]));
        ran(vec![f]).err()
    };
    assert_eq!(listed(0, 0), None);
    assert_eq!(listed(1, 0), lost);
    assert_eq!(listed(0, 1), lost);
}

/// The whole code is one array: a `Halt`, then each function after the last.
#[test]
fn the_code_starts_with_a_halt_and_has_no_gaps() {
    let code: Code = load(&program(SHAPES), Audit::EachInstruction).expect("the program loads");
    assert!(matches!(code.ins[0], Ins::Halt { .. }));
    let halts = code.ins.iter().filter(|i| i.op() == Op::Halt);
    assert_eq!(halts.count(), 1);
    let total: usize = entries(&code).iter().map(|e| body(&code, e).len()).sum();
    assert_eq!(total + 1, code.ins.len());
}
