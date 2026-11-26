use super::super::ins::{
    ArgList, Assign, AssignList, Callee, HitList, Length, MaybeHit, OwnEntry, OwnRoom, Shared,
    Split, SplitWide,
};
use super::super::tests::{SHAPES, a_ctor, body, entries, named, of, program, targets};
use super::super::{Arg, Args, Code, Dst, Entry, ReadsClosure, load, load_counted};
use super::*;
use crate::heap::Layout;

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
            } else if (!ins.falls_through() || !targets(ins, &counted.code.lists).is_empty())
                && let Some(next) = new.get(i + 1)
            {
                // Anything that may not fall through ends its block.
                assert!(is_count(next), "{} at {i}", now.name);
            }
        }
        for (a, b) in old.iter().zip(&kept) {
            let (a, b) = (targets(a, &plain.lists), targets(b, &counted.code.lists));
            assert_eq!(a.len(), b.len());
            for (a, b) in a.into_iter().zip(b) {
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
/// the audit in front of where it went. Beside those it has only the checks of
/// what was written. The blocks counted hold no audits.
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
    let is_audit = |op| matches!(op, Op::Audit | Op::AuditMade);
    for code in [&plain, &at_end, &counted.code, &counted_at_end.code] {
        assert!(!code.ins.iter().any(|ins| is_audit(ins.op())));
    }
    for block in &both.blocks {
        assert!(!block.iter().any(|op| is_audit(*op)));
    }
    let mut jumps = 0;
    for (a, b) in [(&plain, &audited), (&counted.code, &both.code)] {
        for (was, now) in entries(a).into_iter().zip(entries(b)) {
            let (old, new) = (body(a, was), body(b, now));
            // Where each of the old is, and where a jump to it goes.
            let at: Vec<usize> = (0..new.len()).filter(|i| !is_audit(new[*i].op())).collect();
            assert_eq!(at.len(), old.len(), "{}", now.name);
            let before = |i: usize| new[i - 1].op();
            for (old, at_old) in old.iter().zip(&at) {
                let new = &new[*at_old];
                assert_eq!(before(*at_old), Op::Audit, "{}", now.name);
                assert_eq!(old.op(), new.op());
                let (old, new) = (targets(old, &a.lists), targets(new, &b.lists));
                assert_eq!(old.len(), new.len());
                for (old, new) in old.into_iter().zip(new) {
                    // The audit is right in front of the instruction, and in
                    // front of that, at the start, are the parameters' checks.
                    let (from, to) = (within(was, old), within(now, new));
                    let start = if from == 0 { 0 } else { at[from] - 1 };
                    assert_eq!(to, start, "{}", now.name);
                    jumps += 1;
                }
            }
        }
    }
    assert!(jumps > 5, "the program has jumps to check: {jumps}");
}

/// In an audited load each value whose type says what it may be is checked right
/// after the instruction that writes it, where a jump to the next one does not
/// go, and each parameter where the function starts, where a jump back does.
#[test]
fn an_audited_load_checks_what_is_written() {
    let src = "import scarlet/map\n\
        type Shade {\n\tDark\n\tLight\n}\n\
        type Dot {\n\tDot(shade Shade, at Float, n Int, next Option(Int))\n}\n\
        fn shade(d Dot) Shade { d.shade }\n\
        fn at(d Dot) Float { d.at }\n\
        fn n(d Dot) Int { d.n }\n\
        fn next(d Dot) Option(Int) { d.next }\n\
        fn same(x a) a { x }\n\
        fn more(d Dot) Int { d.n + 1 }\n\
        fn looked(m map::Map(Int, Shade), k Int) Shade {\n\
        \tfound = match map::get(m, k) {\n\t\tSome(s) -> flip(s)\n\t\tNone -> Dark\n\t}\n\
        \tflip(found)\n}\n\
        fn flip(s Shade) Shade { if s == Dark { Light } else { Dark } }\n\
        fn go(n Int, s Shade, x Float, name String) Float {\n\
        \tif n == 0 { x } else { go(n - 1, s, x, name) }\n}\n\
        pub fn main() {\n\
        \td = Dot(Light, 1.5, 3, None)\n\
        \ts = same(shade(d))\n\
        \tkept = [s, s]\n\
        \tprintln(go(n(d), s, at(d), ''))\n\
        \tprintln(kept)\n\
        \tprintln((next(d), more(d), looked(map::set(map::new(), 1, s), 1)))\n\
        }\n";
    let code = load(&program(src), Audit::EachInstruction).expect("the program loads");
    let checks = |name: &str| -> Vec<(Reg, Makes)> {
        let check = |ins: &Ins<Reg>| match ins {
            Ins::AuditMade { src, may } => Some((src.0, *may)),
            _ => None,
        };
        body(&code, named(&code, name))
            .iter()
            .filter_map(check)
            .collect()
    };
    let params = |name: &str| {
        let ins = body(&code, named(&code, name)).iter();
        ins.take_while(|i| i.op() == Op::AuditMade).count()
    };
    let (r2, r3, r4) = (Reg(2), Reg(3), Reg(4));
    let (word, int) = (Makes::NoCell, Makes::Int);
    // A parameter that may be any cell is not checked.
    assert_eq!([params("shade"), params("same"), params("go")], [0, 0, 3]);
    assert_eq!(checks("same"), []);
    assert_eq!(checks("go")[..3], [(r2, int), (r3, word), (r4, word)]);
    // So is what a tail expression gives, when its type says anything.
    for (name, makes) in [(".shade", word), (".at", word), (".n", int)] {
        assert_eq!(checks(name), [(r2, makes)], "{name}");
    }
    assert_eq!(checks(".next"), []);
    assert_eq!(checks(".more").len(), 2);
    assert!(checks(".more").iter().all(|(_, may)| *may == int));
    // What a lookup finds is checked too.
    let looked = body(&code, named(&code, ".looked"));
    let at = looked.iter().position(|i| i.op() == Op::MapGet);
    let after = &looked[at.expect("a lookup") + 1];
    assert!(matches!(after, Ins::AuditMade { may, .. } if *may == word));
    let main = checks("main");
    assert!(main.iter().filter(|(_, may)| *may == word).count() >= 4);
    assert!(main.iter().any(|(_, may)| *may == int), "{main:?}");
    // A check is of what the instruction before it writes.
    let main = body(&code, named(&code, "main"));
    for (i, ins) in main.iter().enumerate() {
        let Ins::AuditMade { src, .. } = ins else {
            continue;
        };
        let mut written = Vec::new();
        let wrote = main[..i].iter().rfind(|i| i.op() != Op::AuditMade);
        wrote
            .expect("one that writes")
            .operands(&code.lists, &mut |r, role| {
                if matches!(role, Role::Write | Role::WriteOnHit) {
                    written.push(r);
                }
            });
        assert!(written.contains(&src.0), "{ins:?} {written:?}");
    }
    // A loop goes back to its parameters' checks.
    let go = named(&code, "go");
    let mut back = body(&code, go).iter().flat_map(|i| targets(i, &code.lists));
    assert!(back.any(|to| within(go, to) == 0));
}

/// A call by name holds where its callee starts, whether the callee is before
/// it, after it or itself, and a call to itself where its own function does.
/// Every frame size an instruction holds is its own function's. The program has
/// an instruction of each kind that holds one.
#[test]
fn a_call_holds_where_its_callee_starts() {
    let src = "fn none() Int { 1 }\n\
        fn early(n Int) Int { if n == 0 { none() + 0 } else { late(n - 1, 2, 3) } }\n\
        fn late(a Int, b Int, c Int) Int { early(a) + late(b, c, b) + two(a, b) + two(a, a) }\n\
        fn two(a Int, b Int) Int { early(a) + three(b, 1, 2) + a }\n\
        fn three(a Int, b Int, c Int) Int { three(a, b, c) + a + four(a, b, c, 1) }\n\
        fn four(a Int, b Int, c Int, d Int) Int { four(a, b, c, d) + a }\n\
        fn many(a Int, b Int, c Int, d Int, e Int, f Int, g Int) Int {\n\
        \tif a == 0 { many(b, a, c, d, e, f, g) } else {\n\
        \t\tif a == 1 { over(g, f, e, d, c, b, a) } else { many(a, b, c, d, e, f, g) + a }\n\t}\n}\n\
        fn over(a Int, b Int, c Int, d Int, e Int, f Int, g Int) Int {\n\
        \tmany(b, a, c, d, e, f, g) + 1\n}\n\
        fn round(n Int) Int { if n == 0 { 0 } else { round(n - 1) } }\n\
        fn value(f fn(Int) Int, n Int) Int { if n == 0 { f(n) } else { f(n) + 1 } }\n\
        fn wide(f fn(Int, Int, Int, Int, Int, Int, Int) Int, n Int) Int {\n\
        \tif n == 0 { f(n, 1, 2, 3, 4, 5, 6) } else { f(1, 2, 3, 4, 5, 6, n) + 1 }\n}\n\
        pub fn main() {\n\
        \tk = none()\n\
        \tsmall = fn(n) { if n == 0 { k } else { small(n - 1) + 1 } }\n\
        \tlong = fn(a, b, c, d, e, f, g) {\n\
        \t\tif a == 0 { k + b + c + d + e + f + g } else { long(a - 1, b, c, d, e, f, g) + 1 }\n\t}\n\
        \tprintln(early(3) + round(2) + value(small, 1) + wide(long, 2))\n\
        }\n";
    let code = load(&program(src), Audit::Never).expect("the program loads");
    let mut seen = std::collections::BTreeSet::new();
    let mut kinds = std::collections::BTreeSet::new();
    for entry in entries(&code) {
        for ins in body(&code, entry) {
            // Where it goes when it says, and whether that is by name.
            let goes = match *ins {
                Ins::Call0 { to, own, .. }
                | Ins::Call1 { to, own, .. }
                | Ins::Call2 { to, own, .. }
                | Ins::Call3 { to, own, .. }
                | Ins::Call1Share { to, own, .. }
                | Ins::Call2Share { to, own, .. }
                | Ins::Call3Share { to, own, .. }
                | Ins::Call { to, own, .. }
                | Ins::CallWide { to, own, .. } => Some((own, Some((to, true)))),
                Ins::CallSelf { to, own, .. } | Ins::CallSelfWide { to, own, .. } => {
                    Some((own, Some((to, false))))
                }
                Ins::Enter { to } => Some((entry.own, Some((to, true)))),
                Ins::CallValue { own, .. }
                | Ins::CallValueWide { own, .. }
                | Ins::TailCallValue { own, .. }
                | Ins::TailCallValueWide { own, .. } => Some((own, None)),
                Ins::Ret { last, .. } | Ins::RetClosure { last, .. } => {
                    assert_eq!(last.0, entry.own.last().0, "{}", entry.name);
                    kinds.insert(ins.op().name());
                    None
                }
                _ => None,
            };
            let Some((own, to)) = goes else {
                continue;
            };
            if !matches!(ins, Ins::Enter { .. }) {
                kinds.insert(ins.op().name());
            }
            assert_eq!(own, entry.own, "{}", entry.name);
            if let Some((to, by_name)) = to {
                let mut all = entries(&code).into_iter();
                let called = all.find(|e| e.pc.index() == to.index());
                let called = &called.expect("a function's start").name;
                if !by_name {
                    assert_eq!(*called, entry.name);
                }
                seen.insert((entry.name.clone(), called.clone()));
            }
        }
    }
    let sized = |name: &&str| name.contains("Call") || name.starts_with("Ret");
    let all: Vec<_> = Op::ALL.iter().map(|op| op.name()).filter(sized).collect();
    let missing: Vec<_> = all.iter().filter(|op| !kinds.contains(**op)).collect();
    assert_eq!(missing, Vec::<&&str>::new());
    assert_eq!(all.len(), kinds.len());
    let calls = |from: &str, to: &str| {
        let pair = (format!("main.{from}"), format!("main.{to}"));
        assert!(seen.contains(&pair), "{from} calls {to}: {seen:?}");
    };
    calls("early", "late");
    calls("late", "early");
    calls("late", "late");
    // One that goes round does it with a jump to its own start.
    let round = named(&code, ".round");
    let back = |ins: &&Ins<Reg>| matches!(ins, Ins::Jump { to } if to.index() == round.pc.index());
    assert_eq!(body(&code, round).iter().filter(back).count(), 1);
    calls("main", "round");
}

fn fitted(ins: Vec<Ins<VReg>>, regs: Vec<Option<Reg>>) -> Fitted {
    let body = Body {
        ins,
        lists: Lists::default(),
        labels: Vec::new(),
        kinds: vec![Some(Makes::Any); regs.len()],
        params: Vec::new(),
        regs,
        own: Room(5),
        closure: ReadsClosure::Nothing,
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
        f.body.ins.push(Ins::TailCallValueWide {
            callee: Shared(VReg(0)),
            args: ArgList(0),
            own: OwnRoom,
        });
        f
    };
    let placed = ran(vec![wide(1), wide(2)]).expect("it fits");
    let mut parts = Vec::new();
    let mut args = Vec::new();
    for ins in &placed.ins {
        match ins {
            Ins::ConcatWide { parts: list, .. } => parts.push(list.regs(&placed.lists)),
            Ins::TailCallValueWide { args: list, .. } => {
                args.push(list.args(&placed.lists)[0].reg);
            }
            _ => {}
        }
    }
    assert_eq!(parts, [[Reg(1), Reg(0)], [Reg(2), Reg(0)]]);
    assert_eq!(args, [Reg(1), Reg(2)]);
}

/// The same of the lists of fields that are set.
#[test]
fn each_functions_updates_are_its_own() {
    let layout = Layout::new(a_ctor(), 300);
    let update = |list: u32, values: &[u32]| {
        let mut f = fitted(
            vec![Ins::UpdateWide {
                dst: super::super::Dst(VReg(0)),
                base: Arg::moved(VReg(0)),
                layout,
                sets: AssignList(list),
            }],
            vec![Some(Reg(5)), Some(Reg(1)), Some(Reg(2))],
        );
        for v in values {
            let value = Arg::moved(VReg(*v));
            let field = u16::try_from(100 + v).expect("a few");
            f.body.lists.sets.push(Box::new([Assign { field, value }]));
        }
        f.body.ins.push(of::jump(0));
        f.body.labels.push(0);
        f
    };
    let placed = ran(vec![update(1, &[1, 2]), update(0, &[2, 0])]).expect("it fits");
    let mut sets = Vec::new();
    for ins in &placed.ins {
        if let Ins::UpdateWide { sets: list, .. } = ins {
            let each = list.sets(&placed.lists).iter();
            sets.extend(each.map(|s| (s.field, s.value.reg, s.value.moved)));
        }
    }
    assert_eq!(sets, [(102, Reg(2), true), (102, Reg(2), true)]);
    let lost = Some(Stop::broken(Broken::NoPlace));
    assert_eq!(ran(vec![update(2, &[1, 2])]).err(), lost);
    assert_eq!(ran(vec![update(0, &[3])]).err(), lost);
}

/// The same of the lists of elements that a pattern finds.
#[test]
fn each_functions_splits_are_its_own() {
    let wide = |list: u32, to: &[u32]| {
        let mut f = fitted(
            vec![Ins::ArraySplitWide {
                elements: HitList(list),
                tail: MaybeHit::new(None),
                src: Arg::moved(VReg(0)),
                miss: Label(0),
            }],
            vec![Some(Reg(5)), Some(Reg(1)), Some(Reg(2))],
        );
        for v in to {
            let length = Length {
                len: 9,
                more: false,
            };
            f.body.lists.splits.push(SplitWide {
                split: Split { length, from: *v },
                elements: Box::new([MaybeHit::new(None), MaybeHit::new(Some(VReg(*v)))]),
            });
        }
        f.body.ins.push(of::jump(0));
        f.body.labels.push(0);
        f
    };
    let placed = ran(vec![wide(1, &[1, 2]), wide(0, &[2, 0])]).expect("it fits");
    let mut found = Vec::new();
    for ins in &placed.ins {
        if let Ins::ArraySplitWide { elements, .. } = ins {
            let wide = elements.of(&placed.lists).expect("a list");
            let to = wide.elements.iter().map(|e| e.hit().is_some());
            found.push((wide.split.from, to.collect::<Vec<_>>()));
        }
    }
    let to = vec![false, true];
    assert_eq!(found, [(2, to.clone()), (2, to)]);
    let lost = Some(Stop::broken(Broken::NoPlace));
    assert_eq!(ran(vec![wide(2, &[1, 2])]).err(), lost);
    assert_eq!(ran(vec![wide(0, &[3])]).err(), lost);
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
    let call = |to: u32| Ins::Enter {
        to: Callee(FuncIdx(to)),
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
                Ins::TailCallValueWide {
                    callee: Shared(VReg(0)),
                    args: ArgList(args),
                    own: OwnRoom,
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

/// A function that reads captures finds them in the closure it runs as, so a
/// call by name to it is refused, whichever instruction makes it. Its calls to
/// itself pass the closure on, and one that reads only itself needs none.
#[test]
fn a_call_by_name_to_a_function_that_reads_captures_is_refused() {
    let (dst, own, to) = (Dst(VReg(0)), OwnRoom, Callee(FuncIdx(1)));
    let (a, b, c) = (Taken(VReg(0)), Taken(VReg(0)), Taken(VReg(0)));
    let shared = Arg::shared(VReg(0));
    let (few, wide) = (Args::new(&[VReg(0)]).expect("room for one"), ArgList(0));
    let by_name: [Ins<VReg>; 10] = [
        Ins::Call0 { dst, own, to },
        Ins::Call1 { dst, own, to, a },
        Ins::Call2 { dst, own, to, a, b },
        Ins::Call3 {
            dst,
            own,
            to,
            a,
            b,
            c,
        },
        Ins::Call1Share {
            dst,
            own,
            to,
            a: shared,
        },
        Ins::Call2Share {
            dst,
            own,
            to,
            a: shared,
            b: shared,
        },
        Ins::Call3Share {
            dst,
            own,
            to,
            a: shared,
            b: shared,
            c: shared,
        },
        Ins::Call {
            dst,
            own,
            to,
            args: few,
        },
        Ins::CallWide {
            dst,
            own,
            to,
            args: wide,
        },
        Ins::Enter { to },
    ];
    let to = OwnEntry;
    let to_itself: [Ins<VReg>; 2] = [
        Ins::CallSelf {
            dst,
            own,
            to,
            args: few,
        },
        Ins::CallSelfWide {
            dst,
            own,
            to,
            args: wide,
        },
    ];
    let of = |ins: Ins<VReg>, closure| {
        let mut f = fitted(vec![ins], vec![Some(Reg(2))]);
        f.body.lists.args.push(Box::new([shared]));
        f.body.closure = closure;
        f
    };
    let all = [
        ReadsClosure::Nothing,
        ReadsClosure::Itself,
        ReadsClosure::Captures,
    ];
    for reads in all {
        for call in by_name {
            let stop = ran(vec![of(call, all[0]), of(of::ret(0), reads)]).err();
            let func = "f".to_string();
            let refused = Stop::broken(Broken::CalledByName { func });
            let needs_one = reads == ReadsClosure::Captures;
            assert_eq!(stop, needs_one.then_some(refused), "{call:?} {reads:?}");
        }
        for call in to_itself {
            assert_eq!(ran(vec![of(call, reads)]).err(), None, "{call:?} {reads:?}");
        }
    }
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
