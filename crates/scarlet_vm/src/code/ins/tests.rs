use super::*;
use crate::code::tests::{a_ctor, body, named, program, targets};
use crate::code::{Code, load};
use crate::ops::{Arith, Compare, Order};

/// The registers `ins` names and what it does with each, in order.
fn operands<R: Names>(ins: &Ins<R>, lists: &Lists<R>) -> Vec<(R, Role)> {
    let mut out = Vec::new();
    ins.operands(lists, &mut |r, role| out.push((r, role)));
    out
}

/// Every operator instruction names its registers, in the order its operator
/// reads them, and each comparison becomes the jump beside it in the list with
/// the same operands.
#[test]
fn every_operator_instruction_names_its_operands() {
    let (dst, a, b) = (Dst(VReg(5)), Src(VReg(6)), Src(VReg(7)));
    let k = SmallInt::new(-3).expect("small");
    let x = Float::new(0.5);
    let to = Label(9);
    let mut two = Vec::new();
    let mut one = Vec::new();
    let mut jumps = Vec::new();
    macro_rules! pairs {
        ([] $($name:ident $op:path),*) => {
            two.extend([$((Ins::$name { dst, a, b }, format!("{:?}", $op))),*])
        };
    }
    macro_rules! with {
        ([$k:ident] $($name:ident $op:path),*) => {
            one.extend([$((Ins::$name { dst, a, k: $k }, format!("{:?}", $op))),*])
        };
    }
    macro_rules! compare_pairs {
        ([] $($name:ident $jump:ident $op:path),*) => {
            $(
                two.push((Ins::$name { dst, a, b }, format!("{:?}", $op)));
                jumps.push((Ins::$name { dst, a, b }, Ins::$jump { a, b, to }, 2));
            )*
        };
    }
    macro_rules! compare_with {
        ([$k:ident] $($name:ident $jump:ident $op:path),*) => {
            $(
                one.push((Ins::$name { dst, a, k: $k }, format!("{:?}", $op)));
                jumps.push((
                    Ins::$name { dst, a, k: $k },
                    Ins::$jump { a, k: $k, to },
                    1,
                ));
            )*
        };
    }
    int_arith!(pairs);
    float_arith!(pairs);
    int_arith_k!(with k);
    int_arith_k_first!(with k);
    float_arith_k!(with x);
    float_arith_k_first!(with x);
    int_compare!(compare_pairs);
    float_compare!(compare_pairs);
    int_compare_k!(compare_with k);
    float_compare_k!(compare_with x);
    assert_eq!((two.len(), one.len(), jumps.len()), (16, 22, 16));

    let lists = Lists::default();
    let all = two.iter().map(|i| (i, 2)).chain(one.iter().map(|i| (i, 1)));
    for ((ins, op), n) in all {
        let want = [
            (VReg(5), Role::Write),
            (VReg(6), Role::Look),
            (VReg(7), Role::Look),
        ];
        assert_eq!(operands(ins, &lists), want[..=n], "{ins:?}");
        assert!(
            ins.falls_through() && targets(ins, &lists).is_empty(),
            "{ins:?}"
        );
        // An instruction is named after its operator.
        let op = op.replace("Order(", "").replace(')', "");
        assert!(ins.op().name().contains(&op), "{op} {}", ins.op().name());
    }
    for (stored, jump, n) in &jumps {
        let want = [(VReg(6), Role::Look), (VReg(7), Role::Look)];
        assert_eq!(operands(jump, &lists), want[..*n], "{jump:?}");
        assert!(jump.falls_through());
        assert_eq!(targets(jump, &lists), [to]);
        let name = format!("JumpUnless{}", stored.op().name());
        assert_eq!(jump.op().name(), name);
    }
}

/// The instructions that are not followed by the next one, and those that may
/// jump. Nothing else is either.
#[test]
fn only_these_instructions_end_or_jump() {
    let ends = [
        Op::Halt,
        Op::TailCallValue,
        Op::TailCallValueWide,
        Op::Ret,
        Op::RetClosure,
        Op::Jump,
        Op::Enter,
        Op::Bad,
        Op::Switch,
        Op::SwitchInt,
        Op::SwitchIntFar,
    ];
    let listed = |op: &Op| ends.contains(op);
    let names = Op::ALL.iter().map(|op| (op.name(), listed(op)));
    for (name, ends) in names {
        let looks = name.starts_with("TailCall")
            || name.starts_with("Switch")
            || ["Halt", "Ret", "RetClosure", "Jump", "Enter", "Bad"].contains(&name);
        assert_eq!(looks, ends, "{name}");
    }
    let (own, last) = (OwnRoom, LastReg);
    let (to, why) = (Callee(FuncIdx(0)), WhyId(0));
    let args = Args::new(&[]).expect("none");
    let callee = Shared(VReg(0));
    let mut lists = Lists::default();
    lists.tables.extend([Label(5), Label(3), Label(5)]);
    let places = [(1, Label(6)), (900, Label(7))];
    lists.places.push(Box::new(places));
    let src = Src(VReg(0));
    let ending: [Ins<VReg>; 11] = [
        Ins::Halt {
            src: Taken(VReg(0)),
        },
        Ins::TailCallValue { callee, args, own },
        Ins::TailCallValueWide {
            callee,
            args: ArgList(0),
            own,
        },
        Ins::Ret {
            src: Taken(VReg(0)),
            last,
        },
        Ins::RetClosure {
            src: Taken(VReg(0)),
            last,
        },
        Ins::Jump { to: Label(0) },
        Ins::Enter { to },
        Ins::Bad { why },
        Ins::Switch {
            src,
            table: Table { first: 1, last: 1 },
        },
        Ins::SwitchInt {
            src,
            table: IntTable {
                table: Table { first: 0, last: 2 },
                first: 7,
            },
        },
        Ins::SwitchIntFar {
            src,
            places: PlaceList(0),
            to: Label(8),
        },
    ];
    for (ins, op) in ending.iter().zip(ends) {
        assert_eq!(ins.op(), op);
        assert!(!ins.falls_through(), "{op:?}");
        let want = match op {
            Op::Jump => vec![0],
            Op::Switch => vec![3, 5],
            Op::SwitchInt => vec![5, 3, 5],
            Op::SwitchIntFar => vec![6, 7, 8],
            _ => Vec::new(),
        };
        let to: Vec<u32> = targets(ins, &lists).iter().map(|l| l.0).collect();
        assert_eq!(to, want, "{op:?}");
    }
    // A table that is not there has nowhere to go.
    let lost = Ins::Switch {
        src,
        table: Table { first: 2, last: 1 },
    };
    assert!(targets::<VReg>(&lost, &lists).is_empty());
    let hit: Ins<VReg> = Ins::MapGet {
        dst: MaybeHit::new(Some(VReg(0))),
        map: Src(VReg(1)),
        key: Src(VReg(2)),
        miss: Label(4),
    };
    assert!(hit.falls_through());
    assert_eq!(targets(&hit, &lists), [Label(4)]);
}

/// What an instruction says it makes: a number or a Bool is no cell but a big
/// Int, and one that says nothing may make anything.
#[test]
fn an_instruction_says_when_it_makes_no_cell() {
    let (dst, a, b) = (Dst(VReg(0)), Src(VReg(1)), Src(VReg(2)));
    let (k, x) = (SmallInt::new(1).expect("small"), Float::new(0.5));
    let mut made = Vec::new();
    macro_rules! makes {
        ([$makes:ident $fields:tt] $($name:ident $op:path),*) => {
            $(made.push((Ins::$name $fields, Makes::$makes));)*
        };
        ([$makes:ident $fields:tt] $($name:ident $jump:ident $op:path),*) => {
            $(made.push((Ins::$name $fields, Makes::$makes));)*
        };
    }
    int_arith!(makes Int { dst, a, b });
    int_arith_k!(makes Int { dst, a, k });
    int_arith_k_first!(makes Int { dst, a, k });
    float_arith!(makes NoCell { dst, a, b });
    float_arith_k!(makes NoCell { dst, a, k: x });
    float_arith_k_first!(makes NoCell { dst, a, k: x });
    int_compare!(makes NoCell { dst, a, b });
    int_compare_k!(makes NoCell { dst, a, k });
    float_compare!(makes NoCell { dst, a, b });
    float_compare_k!(makes NoCell { dst, a, k: x });
    assert_eq!(made.len(), 38);
    let (src, newline) = (Src(VReg(1)), Newline(true));
    let one: Ins<VReg> = Ins::Const {
        dst,
        value: Value::NIL,
    };
    let big = Ins::BigInt {
        dst,
        n: i64::MAX,
        id: ConstId(0),
    };
    made.extend([
        (one, Makes::NoCell),
        (Ins::FloatNeg { dst, a }, Makes::NoCell),
        (Ins::FloatFromInt { dst, a }, Makes::NoCell),
        (Ins::Print { dst, a, newline }, Makes::NoCell),
        (Ins::Equal { dst, a, b }, Makes::NoCell),
        (Ins::NotEqual { dst, a, b }, Makes::NoCell),
        (Ins::Not { dst, a }, Makes::NoCell),
        (Ins::IntNeg { dst, a }, Makes::Int),
        (big, Makes::Int),
        (Ins::ArrayLen { dst, src }, Makes::Int),
        (Ins::StackDepth { dst }, Makes::Int),
        (Ins::Neg { dst, a }, Makes::Any),
        (Ins::Field { dst, src, index: 0 }, Makes::Any),
        (
            Ins::Concat {
                dst,
                parts: Looks::new(&[]).expect("none"),
            },
            Makes::Any,
        ),
        (Ins::GetCapture { dst, index: 0 }, Makes::Any),
        (
            Ins::Num {
                dst,
                op: NumOp::Arith(Arith::Add),
                a,
                b,
            },
            Makes::Any,
        ),
        (
            Ins::ToString {
                dst,
                a: Shared(VReg(1)),
            },
            Makes::Any,
        ),
        (
            Ins::Range {
                dst,
                start: a,
                end: b,
            },
            Makes::Any,
        ),
    ]);
    for (ins, makes) in made {
        assert_eq!(ins.makes(), makes, "{ins:?}");
    }
    assert!(Makes::NoCell < Makes::Int && Makes::Int < Makes::Any);
}

/// Operands held in an instruction are as many as were put there, and more
/// than fit are refused.
#[test]
fn an_instruction_holds_as_many_operands_as_fit() {
    let regs: Vec<VReg> = (0..9).map(VReg).collect();
    for n in 0..=8 {
        let few = Looks::<VReg, 8>::new(&regs[..n]).expect("it fits");
        assert_eq!(few.regs(), &regs[..n]);
    }
    assert!(Looks::<VReg, 8>::new(&regs).is_none());
    assert!(Shares::<VReg, 8>::new(&regs).is_none());
    assert!(Args::<VReg, 6>::new(&regs[..7]).is_none());
    let args = Args::<VReg, 6>::new(&regs[..6]).expect("it fits");
    let want: Vec<_> = regs[..6].iter().copied().map(Arg::shared).collect();
    assert_eq!(args.iter().collect::<Vec<_>>(), want);
    assert_eq!(args.len(), 6);
}

/// What an argument is decided to be is what it says it is, for each of the
/// arguments an instruction holds.
#[test]
fn each_argument_is_decided_on_its_own() {
    let regs: Vec<VReg> = (0..6).map(VReg).collect();
    for bits in 0..64u32 {
        let mut ins: Ins<VReg> = Ins::CallSelf {
            dst: Dst(VReg(9)),
            to: OwnEntry,
            args: Args::new(&regs).expect("it fits"),
            own: OwnRoom,
        };
        let mut lists = Lists::default();
        ins.decide(&mut lists, &mut |v| bits >> v.0 & 1 == 1);
        let moved = |v: u32| Role::Pass {
            moved: bits >> v & 1 == 1,
        };
        let passed = (0..6).map(|v| (VReg(v), moved(v)));
        let want: Vec<_> = std::iter::once((VReg(9), Role::Write))
            .chain(passed)
            .collect();
        assert_eq!(operands(&ins, &lists), want);
    }
    // An instruction's own operands that it only looks at are not asked about.
    let mut asked = Vec::new();
    let mut ins: Ins<VReg> = Ins::ArraySet {
        dst: Hit(VReg(0)),
        src: Arg::shared(VReg(1)),
        index: Src(VReg(2)),
        value: Arg::shared(VReg(3)),
        miss: Label(0),
    };
    ins.decide(&mut Lists::default(), &mut |v| {
        asked.push(v);
        true
    });
    assert_eq!(asked, [VReg(1), VReg(3)]);
    let roles = [
        (VReg(0), Role::WriteOnHit),
        (VReg(1), Role::Pass { moved: true }),
        (VReg(2), Role::Look),
        (VReg(3), Role::Pass { moved: true }),
    ];
    assert_eq!(operands(&ins, &Lists::default()), roles);
}

/// A constructor that takes each of its fields, of one to eight, becomes the
/// one with that number at its end, which names the same registers and leaves
/// each empty. One that shares any of them stays as it is.
#[test]
fn a_constructor_that_takes_its_fields_has_a_form_of_its_own() {
    let forms = [
        Op::Ctor1,
        Op::Ctor2,
        Op::Ctor3,
        Op::Ctor4,
        Op::Ctor5,
        Op::Ctor6,
        Op::Ctor7,
        Op::Ctor8,
    ];
    let lists = &mut Lists::default();
    let regs: Vec<VReg> = (10..18).map(VReg).collect();
    for (n, form) in (1..).zip(forms) {
        assert_eq!(form.name(), format!("Ctor{n}"));
        let all = (1u32 << n) - 1;
        for bits in 0..=all {
            let mut ins: Ins<VReg> = Ins::Ctor {
                dst: Dst(VReg(3)),
                ctor: a_ctor(),
                fields: Args::new(&regs[..n]).expect("it fits"),
            };
            ins.decide(lists, &mut |v| bits >> (v.0 - 10) & 1 == 1);
            assert_eq!(ins.op(), Op::Ctor);
            let taking = ins.taking();
            assert_eq!(taking.is_some(), bits == all, "{n} {bits:b}");
            let Some(taking) = taking else { continue };
            assert_eq!(taking.op(), form);
            assert!(taking.falls_through() && targets(&taking, lists).is_empty());
            let mut want = vec![(VReg(3), Role::Write)];
            want.extend(regs[..n].iter().map(|r| (*r, Role::Take)));
            assert_eq!(operands(&taking, lists), want);
            // Nothing in it is left to decide, and it has no other form.
            let mut again = taking;
            again.decide(lists, &mut |v| panic!("{v:?} is decided"));
            assert!(again.taking().is_none());
        }
    }
    // A wide one takes what its list says, and stays in its list.
    lists.args.push(Box::new([Arg {
        reg: VReg(1),
        moved: true,
    }]));
    let wide: Ins<VReg> = Ins::CtorWide {
        dst: Dst(VReg(0)),
        ctor: a_ctor(),
        fields: ArgList(0),
    };
    assert!(wide.taking().is_none());
}

/// A call that takes its one, two or three arguments becomes the call with that
/// number at its end, and one that shares any of them stays as it is.
#[test]
fn a_call_that_takes_its_arguments_has_a_form_of_its_own() {
    let (dst, own, to) = (Dst(VReg(3)), OwnRoom, Callee(FuncIdx(1)));
    let arg = |reg: u32, bits: u32| Arg {
        reg: VReg(reg),
        moved: bits >> (reg - 10) & 1 == 1,
    };
    for bits in 0..8 {
        let (a, b, c) = (arg(10, bits), arg(11, bits), arg(12, bits));
        let calls: [(Ins<VReg>, u32, Op); 3] = [
            (Ins::Call1Share { dst, own, to, a }, 1, Op::Call1),
            (Ins::Call2Share { dst, own, to, a, b }, 3, Op::Call2),
            (
                Ins::Call3Share {
                    dst,
                    own,
                    to,
                    a,
                    b,
                    c,
                },
                7,
                Op::Call3,
            ),
        ];
        for (call, all, form) in calls {
            let taking = call.taking();
            assert_eq!(taking.is_some(), bits & all == all, "{form:?} {bits:b}");
            let Some(taking) = taking else { continue };
            assert_eq!(taking.op(), form);
            let lists = &Lists::default();
            let (was, is) = (operands(&call, lists), operands(&taking, lists));
            let registers = |of: &[(VReg, Role)]| of.iter().map(|(r, _)| *r).collect::<Vec<_>>();
            assert_eq!(registers(&was), registers(&is));
            assert!(is[1..].iter().all(|(_, role)| *role == Role::Take));
        }
    }
}

/// What a new cell is given comes in the order of the operands, each as the
/// instruction says.
#[test]
fn a_new_cell_is_given_its_operands_in_order() {
    let regs = [Reg(7), Reg(3), Reg(7), Reg(250)];
    let given = |hands: &[Arg<Reg>]| -> Vec<(u8, bool)> {
        hands.iter().map(|a| (a.reg.0, a.moved)).collect()
    };
    let each = |moved: bool| regs.map(|r| (r.0, moved)).to_vec();
    let taken: Vec<_> = regs.map(Taken).hands().collect();
    assert_eq!(given(&taken), each(true));
    let shared: Vec<_> = regs.map(Shared).hands().collect();
    assert_eq!(given(&shared), each(false));
    let listed: Vec<_> = regs[..].hands().collect();
    assert_eq!(given(&listed), each(false));
    let mut args = Args::<Reg, 8>::new(&regs).expect("it fits");
    Part::<Reg>::decide(&mut args, &mut Lists::default(), &mut |r| r == Reg(250));
    let want = vec![(7, false), (3, false), (7, false), (250, true)];
    let some: Vec<_> = args.hands().collect();
    assert_eq!(given(&some), want);
    let listed: Vec<_> = some[..].hands().collect();
    assert_eq!(given(&listed), want);
    assert_eq!(some[..].hands().len(), 4);
    assert_eq!(args.hands().len(), 4);
}

/// Places that give each value the register of its number plus one, and
/// nothing past `fits`.
struct Shifted {
    fits: u32,
}

impl Places for Shifted {
    fn reg(&self, r: VReg) -> Result<Reg, Unplaced> {
        let next = u8::try_from(r.0).ok().and_then(|r| r.checked_add(1));
        let fits = next.filter(|_| r.0 < self.fits);
        fits.map(Reg).ok_or(Unplaced::NoPlace)
    }

    fn label(&self, l: Label) -> Result<Pc, Unplaced> {
        let here = (l.0 < 4).then_some(Pc(l.0 + 100));
        here.ok_or(Unplaced::NoPlace)
    }

    fn entry(&self, f: FuncIdx) -> Result<EntryPc, Unplaced> {
        let here = (f.0 < 4).then_some(EntryPc(f.0 + 200));
        here.ok_or(Unplaced::NeedsClosure(f))
    }

    fn own_entry(&self) -> EntryPc {
        EntryPc(300)
    }

    fn own(&self) -> Room {
        Room(9)
    }

    fn reg_list(&self, i: u32) -> Result<u32, Unplaced> {
        Ok(i + 10)
    }

    fn arg_list(&self, i: u32) -> Result<u32, Unplaced> {
        Ok(i + 20)
    }

    fn dst_list(&self, i: u32) -> Result<u32, Unplaced> {
        Ok(i + 30)
    }

    fn set_list(&self, i: u32) -> Result<u32, Unplaced> {
        Ok(i + 40)
    }

    fn table(&self, table: Table) -> Result<Table, Unplaced> {
        let first = table.first + 50;
        Ok(Table { first, ..table })
    }

    fn place_list(&self, i: u32) -> Result<u32, Unplaced> {
        Ok(i + 60)
    }
}

/// Giving an instruction its registers keeps everything else about it.
#[test]
fn placing_an_instruction_keeps_what_it_does() {
    let at = Shifted { fits: 50 };
    let mut args = Args::<VReg, 6>::new(&[VReg(1), VReg(2)]).expect("it fits");
    Part::<VReg>::decide(&mut args, &mut Lists::default(), &mut |v| v == VReg(2));
    let call: Ins<VReg> = Ins::Call {
        dst: Dst(VReg(3)),
        to: Callee(FuncIdx(1)),
        args,
        own: OwnRoom,
    };
    let placed = call.place(&at).expect("a place for everything");
    let roles = [
        (Reg(4), Role::Write),
        (Reg(2), Role::Pass { moved: false }),
        (Reg(3), Role::Pass { moved: true }),
    ];
    assert_eq!(operands(&placed, &Lists::default()), roles);
    let Ins::Call { to, own, .. } = placed else {
        panic!("a call");
    };
    assert_eq!((to.index(), own), (201, Room(9)));
    let call: Ins<VReg> = Ins::CallSelf {
        dst: Dst(VReg(3)),
        to: OwnEntry,
        args,
        own: OwnRoom,
    };
    let Ok(Ins::CallSelf { to, own, .. }) = call.place(&at) else {
        panic!("a call to itself");
    };
    assert_eq!((to.index(), own), (300, Room(9)));

    let wide: Ins<VReg> = Ins::CtorWide {
        dst: Dst(VReg(0)),
        ctor: a_ctor(),
        fields: ArgList(1),
    };
    let Ok(Ins::CtorWide { ctor, fields, .. }) = wide.place(&at) else {
        panic!("a constructor");
    };
    assert_eq!((ctor, fields.0), (a_ctor(), 21));

    // A field with nowhere to go has nowhere still, also where the value it
    // is read as by a mistake has a register.
    let dsts = [Some(VReg(7)), None, Some(VReg(0))].map(MaybeDst::new);
    let src = Taken(VReg(4));
    let ctor = a_ctor();
    let open: Ins<VReg> = Ins::Open3 { dsts, src, ctor };
    let placed = open.place(&at).expect("a place for everything");
    let roles = [
        (Reg(8), Role::Write),
        (Reg(1), Role::Write),
        (Reg(5), Role::Take),
    ];
    assert_eq!(operands(&placed, &Lists::default()), roles);
    let Ins::Open3 { dsts, ctor, .. } = placed else {
        panic!("{placed:?}");
    };
    let regs = dsts.map(|dst| dst.dst().map(|dst| dst.0));
    assert_eq!(regs, [Some(Reg(8)), None, Some(Reg(1))]);
    assert_eq!(ctor, a_ctor());
    let dsts = DstList(1);
    let open: Ins<VReg> = Ins::OpenWide { dsts, src, ctor };
    let Ok(Ins::OpenWide { dsts, .. }) = open.place(&at) else {
        panic!("a cell taken apart");
    };
    assert_eq!(dsts.0, 31);

    // The registers an instruction names one by one keep their order.
    let three: Ins<VReg> = Ins::Ctor3 {
        dst: Dst(VReg(9)),
        ctor: a_ctor(),
        fields: [VReg(6), VReg(4), VReg(5)].map(Taken),
    };
    let placed = three.place(&at).expect("a place for everything");
    let roles = [
        (Reg(10), Role::Write),
        (Reg(7), Role::Take),
        (Reg(5), Role::Take),
        (Reg(6), Role::Take),
    ];
    assert_eq!(operands(&placed, &Lists::default()), roles);
    let pair: Ins<VReg> = Ins::Tuple2 {
        dst: Dst(VReg(9)),
        elements: [VReg(6), VReg(4)].map(Shared),
    };
    let placed = pair.place(&at).expect("a place for everything");
    let roles = [
        (Reg(10), Role::Write),
        (Reg(7), Role::Share),
        (Reg(5), Role::Share),
    ];
    assert_eq!(operands(&placed, &Lists::default()), roles);

    // What is moved comes after where to, and each keeps its order.
    let moves: Ins<VReg> = Ins::Move3 {
        dsts: [VReg(9), VReg(7), VReg(8)].map(Dst),
        srcs: [VReg(7), VReg(8), VReg(9)].map(Taken),
    };
    let placed = moves.place(&at).expect("a place for everything");
    let roles = [
        (Reg(10), Role::Write),
        (Reg(8), Role::Write),
        (Reg(9), Role::Write),
        (Reg(8), Role::Take),
        (Reg(9), Role::Take),
        (Reg(10), Role::Take),
    ];
    assert_eq!(operands(&placed, &Lists::default()), roles);

    // Nothing is placed when one thing it names has no place, and what that
    // was comes back.
    let lost: [Ins<VReg>; 9] = [
        Ins::Open2 {
            dsts: [None, Some(VReg(50))].map(MaybeDst::new),
            src,
            ctor,
        },
        Ins::Ctor2 {
            dst: Dst(VReg(0)),
            ctor: a_ctor(),
            fields: [VReg(1), VReg(50)].map(Taken),
        },
        Ins::Tuple3 {
            dst: Dst(VReg(0)),
            elements: [VReg(50), VReg(1), VReg(2)].map(Shared),
        },
        Ins::Jump { to: Label(4) },
        Ins::Drop {
            reg: Dead(VReg(50)),
        },
        Ins::Concat {
            dst: Dst(VReg(0)),
            parts: Looks::new(&[VReg(1), VReg(50)]).expect("it fits"),
        },
        Ins::Move3 {
            dsts: [VReg(1), VReg(50), VReg(2)].map(Dst),
            srcs: [VReg(1), VReg(2), VReg(3)].map(Taken),
        },
        Ins::Move2 {
            dsts: [VReg(1), VReg(2)].map(Dst),
            srcs: [VReg(1), VReg(50)].map(Taken),
        },
        Ins::CallSelf {
            dst: Dst(VReg(1)),
            to: OwnEntry,
            args: Args::new(&[VReg(50)]).expect("it fits"),
            own: OwnRoom,
        },
    ];
    for ins in lost {
        let why = ins.place(&at).map(|ins| ins.op()).err();
        assert_eq!(why, Some(Unplaced::NoPlace), "{ins:?}");
    }
    let enter: Ins<VReg> = Ins::Enter {
        to: Callee(FuncIdx(4)),
    };
    let why = enter.place(&at).map(|ins| ins.op()).err();
    assert_eq!(why, Some(Unplaced::NeedsClosure(FuncIdx(4))));
}

/// Every instruction of a loaded program shows as its kind and its operands.
#[test]
fn the_listing_shows_every_operand() {
    let src = "import scarlet/map\n\
               fn pick(m map::Map(Int, Int), k Int) Int {\n\
               \thit = match map::get(m, k) {\n\
               \t\tSome(v) -> v\n\
               \t\tNone -> 0 - k\n\
               \t}\n\
               \thit + map::size(m)\n\
               }\n\
               pub fn main() {\n\
               \tprintln(pick(map::set(map::new(), 1, 2), 1))\n\
               }\n";
    let listing = crate::listing(&program(src)).expect("the program loads");
    let code: Code = load(&program(src), crate::Audit::Never).expect("the program loads");
    let ins = body(&code, named(&code, ".pick"));
    let index = (&code.funcs)
        .into_iter()
        .position(|e| e.name.ends_with(".pick"));
    let text = listing.function(index.expect("pick")).expect("pick");
    for (i, (ins, line)) in ins.iter().zip(text.lines().skip(1)).enumerate() {
        assert!(line.starts_with(&format!("{i:>5}  ")), "{line}");
        assert!(line.contains(ins.op().name()), "{line}");
        for (r, _) in operands(ins, &code.lists) {
            assert!(line.contains(&r.to_string()), "{line} names {r}");
        }
        let jumps = !targets(ins, &code.lists).is_empty();
        assert_eq!(line.contains(" -> "), jumps, "{line}");
    }
    assert!(text.contains("= MapGet(r2, r3) -> "), "{text}");
}

/// A test of a variant says which, and whether it has fields. A field of a
/// constructor and an element of a tuple are read by one instruction.
#[test]
fn the_listing_shows_a_variant_by_its_number() {
    let src = "type P {\n\tA\n\tB(n Int, t (Int, Int))\n\tC\n}\n\
               type Q {\n\tQ(a Int, b Int)\n}\n\
               fn f(p P) Int {\n\
               \tmatch p {\n\
               \t\tA -> 1\n\
               \t\tB(_, (_, x)) -> x\n\
               \t\tC -> 2\n\
               \t}\n\
               }\n\
               fn g(q Q) Int { q.b }\n\
               fn h(p P) Int {\n\
               \tmatch p {\n\
               \t\tB(n, _) -> n\n\
               \t\tA | C -> 0\n\
               \t}\n\
               }\n\
               fn i(p P) Int {\n\
               \tmatch p {\n\
               \t\tA -> 1\n\
               \t\tB(..) | C -> h(p)\n\
               \t}\n\
               }\n\
               fn j(n Int) Int {\n\
               \tmatch n {\n\
               \t\t1 | 3 -> 5\n\
               \t\t2 -> 6\n\
               \t\t70 -> 7\n\
               \t\t_ -> 8\n\
               \t}\n\
               }\n\
               pub fn main() { println(f(B(1, (2, 3))) + g(Q(1, 2)) + i(C) + j(2)) }\n";
    let listing = crate::listing(&program(src)).expect("the program loads");
    let text = listing.function(0).expect("f");
    for part in [
        "Switch(r2, .0 -> 1, .1 -> 3, .2 -> 6)\n",
        "_, r2 = Open2(move r2, ",
        "= Field(r2, 1)\n",
    ] {
        assert!(text.contains(part), "{part}: {text}");
    }
    let text = listing.function(1).expect("g");
    assert!(text.contains("= Field(r2, 1)\n"), "{text}");
    for (f, part) in [
        (2, "JumpUnlessTag(r2, .1(..)) ->"),
        (3, "JumpUnlessTag(r2, .0) ->"),
        (4, "SwitchInt(r2, 1 3 -> 4, 2 -> 6, _ -> 1)\n"),
        (4, "JumpUnlessInt(r2, 70) ->"),
    ] {
        let text = listing.function(f).expect("a function");
        assert!(text.contains(part), "{part}: {text}");
    }
}

/// A constructor and a tuple show what they are given in order, with what they
/// take told from what they share.
#[test]
fn the_listing_shows_what_a_new_cell_is_given() {
    let src = "type P {\n\tP(a Int, b Int, c Int)\n}\n\
               fn taken(a Int, b Int, c Int) P { P(c, a, b) }\n\
               fn shared(a Int, b Int) P { P(b, a, b) }\n\
               fn pair(a Int, b Int) (Int, Int) { (b, a) }\n\
               fn three(a Int, b Int) (Int, Int, Int) { (b, a, b) }\n\
               pub fn main() {\n\
               \tprintln(taken(1, 2, 3))\n\
               \tprintln(shared(1, 2))\n\
               \tprintln(pair(1, 2))\n\
               \tprintln(three(1, 2))\n\
               }\n";
    let listing = crate::listing(&program(src)).expect("the program loads");
    let code: Code = load(&program(src), crate::Audit::Never).expect("the program loads");
    for (name, want) in [
        (".taken", ["= Ctor3(", ".0, move r4, move r2, move r3)\n"]),
        (".shared", ["= Ctor(", ".0, r3, move r2, move r3)\n"]),
        (".pair", ["= Tuple2(", "(r3, r2)\n"]),
        (".three", ["= Tuple3(", "(r3, r2, r3)\n"]),
    ] {
        let mut all = (&code.funcs).into_iter();
        let index = all.position(|e| e.name.ends_with(name));
        let text = listing.function(index.expect("a function")).expect("text");
        for part in want {
            assert!(text.contains(part), "{part}: {text}");
        }
    }
}

/// A constructor taken apart shows the register of each field that has one, and
/// what it takes them out of.
#[test]
fn the_listing_shows_where_the_fields_of_a_cell_go() {
    let fields: Vec<String> = (0..9).map(|i| format!("f{i} Int")).collect();
    let fields = fields.join(", ");
    let src = format!(
        "type P {{\n\tP(a Int, b Int)\n\tW({fields})\n}}\n\
         fn turn(p P) P {{\n\tmatch p {{\n\
         \t\tP(a, b) -> P(b, a)\n\
         \t\tW(x, _, _, _, _, _, _, _, y) -> P(y, x)\n\t}}\n}}\n\
         fn second(p P) Int {{\n\tmatch p {{\n\
         \t\tP(_, b) -> b\n\
         \t\tW(_, _, _, _, _, _, _, _, _) -> 0\n\t}}\n}}\n\
         pub fn main() {{\n\
         \tprintln(turn(P(1, 2)))\n\
         \tprintln(second(P(1, 2)))\n\
         }}\n"
    );
    let listing = crate::listing(&program(&src)).expect("the program loads");
    let code: Code = load(&program(&src), crate::Audit::Never).expect("the program loads");
    let lines = |name: &str| -> Vec<String> {
        let mut all = (&code.funcs).into_iter();
        let index = all.position(|e| e.name.ends_with(name));
        let text = listing.function(index.expect("a function")).expect("text");
        let opens = text.lines().filter(|l| l.contains("Open"));
        opens.map(|l| l.trim_start().to_string()).collect()
    };
    assert_eq!(
        lines(".turn"),
        [
            "1  r2, r3 = Open2(move r2, 10.0)",
            "4  r2, _, _, _, _, _, _, _, r3 = OpenWide(move r2, 10.1)",
        ]
    );
    assert_eq!(lines(".second"), ["1  _, r2 = Open2(move r2, 10.0)"]);
}

/// The layout of a constructor of `fields` fields.
fn a_layout(fields: usize) -> Layout {
    Layout::new(a_ctor(), fields)
}

fn assign(field: u16, value: Arg<VReg>) -> Assign<VReg> {
    Assign { field, value }
}

/// An update reads its base and then its values, each with a mark of its own,
/// and placing it changes nothing else. One in a list reads the same.
#[test]
fn an_update_names_its_base_and_then_its_values() {
    let at = Shifted { fits: 50 };
    let listed = [1, 4, 7].map(|field: u16| assign(field, Arg::shared(VReg(u32::from(field)))));
    for taken in 0..16u32 {
        let takes = |v: VReg| match v.0 {
            9 => taken & 1 != 0,
            1 => taken & 2 != 0,
            4 => taken & 4 != 0,
            _ => taken & 8 != 0,
        };
        let role = |v: u32| Role::Pass {
            moved: takes(VReg(v)),
        };
        let mut sets = Sets::<VReg, 3>::new(VReg(9), a_layout(8), &listed).expect("three");
        Part::<VReg>::decide(&mut sets, &mut Lists::default(), &mut |v| takes(v));
        let dst = Dst(VReg(2));
        let update: Ins<VReg> = Ins::Update3 { dst, sets };
        let want = [(VReg(2), Role::Write), (VReg(9), role(9))];
        let values = [1, 4, 7].map(|v| (VReg(v), role(v)));
        let want: Vec<_> = want.into_iter().chain(values).collect();
        assert_eq!(operands(&update, &Lists::default()), want);

        let placed = update.place(&at).expect("a place for everything");
        let moved: Vec<_> = want
            .iter()
            .map(|(v, role)| (Reg(v.0 as u8 + 1), *role))
            .collect();
        assert_eq!(operands(&placed, &Lists::default()), moved);
        let Ins::Update3 { sets, .. } = placed else {
            panic!("{placed:?}");
        };
        let fields: Vec<u8> = sets.each().map(|(field, _)| field).collect();
        assert_eq!((fields, sets.layout), (vec![1, 4, 7], a_layout(8)));
        let all = sets
            .taken()
            .map(|(base, values)| (base.0, values.map(|r| r.0)));
        let want = (taken == 15).then_some((Reg(10), [Reg(2), Reg(5), Reg(8)]));
        assert_eq!(all, want);

        let mut lists = Lists::default();
        lists.sets.push(Box::new([]));
        lists.sets.push(Box::new(listed));
        let (base, layout) = (Arg::shared(VReg(9)), a_layout(300));
        let sets = AssignList(1);
        let mut wide: Ins<VReg> = Ins::UpdateWide {
            dst,
            base,
            layout,
            sets,
        };
        wide.decide(&mut lists, &mut |v| takes(v));
        let names: Vec<_> = operands(&wide, &lists);
        assert_eq!(names, operands(&update, &Lists::default()));
        let Ok(Ins::UpdateWide { sets, layout, .. }) = wide.place(&at) else {
            panic!("an update");
        };
        assert_eq!((sets.0, layout), (41, a_layout(300)));
    }
    // It takes the fields of no long constructor, and no more than it has room
    // for, nor one past what it can name.
    let new = |layout, sets: &[Assign<VReg>]| Sets::<VReg, 3>::new(VReg(9), layout, sets);
    assert!(new(a_layout(300), &listed).is_none());
    assert!(new(a_layout(8), &listed[..2]).is_none());
    let past = [1, 2, 256].map(|field| assign(field, Arg::shared(VReg(0))));
    assert!(new(a_layout(8), &past).is_none());
    for bad in [VReg(50), VReg(3)] {
        let mut listed = listed;
        listed[2].value.reg = bad;
        let sets = new(a_layout(8), &listed).expect("three");
        let placed = Ins::Update3 {
            dst: Dst(VReg(0)),
            sets,
        }
        .place(&at);
        assert_eq!(placed.is_ok(), bad == VReg(3));
        let sets = Sets::<VReg, 3>::new(bad, a_layout(8), &listed).expect("three");
        let placed = Ins::Update3 {
            dst: Dst(VReg(0)),
            sets,
        }
        .place(&at);
        assert_eq!(placed.is_ok(), bad == VReg(3));
    }
}

/// An update shows its base, the constructor, and each field with what it is set
/// to.
#[test]
fn the_listing_shows_what_an_update_sets() {
    let fields: Vec<String> = (0..14).map(|i| format!("f{i} Int")).collect();
    let ones = vec!["1"; 14].join(", ");
    let src = format!(
        "type P {{\n\tP(a Int, b Int, c Int, d Int)\n}}\n\
         type W {{\n\tW({})\n}}\n\
         fn one(p P, n Int) P {{ P(..p, b: n) }}\n\
         fn two(p P, n Int) P {{ P(..p, d: n, a: n) }}\n\
         fn kept(p P, n Int) (P, P) {{ (P(..p, a: n, c: n + 1, d: 5), p) }}\n\
         fn many(w W, n Int) W {{ W(..w, f9: n, f1: 5, f2: 6, f3: 7) }}\n\
         pub fn main() {{\n\
         \tp = P(1, 2, 3, 4)\n\
         \tprintln(one(p, 1))\n\
         \tprintln(two(p, 1))\n\
         \tprintln(kept(p, 1))\n\
         \tprintln(many(W({ones}), 1))\n\
         }}\n",
        fields.join(", ")
    );
    let listing = crate::listing(&program(&src)).expect("the program loads");
    let code: Code = load(&program(&src), crate::Audit::Never).expect("the program loads");
    let line = |name: &str| -> String {
        let mut all = (&code.funcs).into_iter();
        let index = all.position(|e| e.name.ends_with(name));
        let text = listing.function(index.expect("a function")).expect("text");
        let update = text.lines().find(|l| l.contains("Update"));
        update.expect("an update")[7..].to_string()
    };
    assert_eq!(line(".one"), "r2 = Update1(move r2, 10.0, .1: move r3)");
    assert_eq!(
        line(".two"),
        "r2 = Update2(move r2, 10.0, .0: r3, .3: move r3)"
    );
    let kept = line(".kept");
    assert!(
        kept.contains("Update3(r2, 10.0, .0: move r") && kept.contains(", .2: move r"),
        "{kept}"
    );
    let many = line(".many");
    assert!(
        many.contains("UpdateWide(move r2, 11.0, .1: move r") && many.contains(", .9: move r3)"),
        "{many}"
    );
}
