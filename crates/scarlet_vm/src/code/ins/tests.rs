use super::*;
use crate::code::tests::{body, named, program};
use crate::code::{Code, load};
use crate::ops::{Arith, Compare, Order};
use scarlet_ir::TypeId;
use scarlet_ir::core_ir::VariantRef;

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
        assert!(ins.falls_through() && ins.target().is_none(), "{ins:?}");
        // An instruction is named after its operator.
        let op = op.replace("Order(", "").replace(')', "");
        assert!(ins.op().name().contains(&op), "{op} {}", ins.op().name());
    }
    for (stored, jump, n) in &jumps {
        let want = [(VReg(6), Role::Look), (VReg(7), Role::Look)];
        assert_eq!(operands(jump, &lists), want[..*n], "{jump:?}");
        assert!(jump.falls_through());
        assert_eq!(jump.target(), Some(to));
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
        Op::TailCall,
        Op::TailCallWide,
        Op::TailCallSelf,
        Op::TailCallSelfWide,
        Op::TailCallSelfKeep,
        Op::TailCallValue,
        Op::TailCallValueWide,
        Op::Ret,
        Op::Jump,
        Op::Bad,
    ];
    let listed = |op: &Op| ends.contains(op);
    let names = Op::ALL.iter().map(|op| (op.name(), listed(op)));
    for (name, ends) in names {
        let looks = name.starts_with("TailCall") || ["Halt", "Ret", "Jump", "Bad"].contains(&name);
        assert_eq!(looks, ends, "{name}");
    }
    let own = OwnRoom;
    let (to, why) = (Callee(FuncIdx(0)), WhyId(0));
    let args = Args::new(&[]).expect("none");
    let callee = Shared(VReg(0));
    let ending: [Ins<VReg>; 11] = [
        Ins::Halt {
            src: Taken(VReg(0)),
        },
        Ins::TailCall {
            to,
            args,
            needs: CalleeRoom(FuncIdx(0)),
            own,
        },
        Ins::TailCallWide {
            to,
            args: ArgList(0),
            needs: CalleeRoom(FuncIdx(0)),
            own,
        },
        Ins::TailCallSelf { to, args, own },
        Ins::TailCallSelfWide {
            to,
            args: ArgList(0),
            own,
        },
        Ins::TailCallSelfKeep {
            to,
            args: ArgList(0),
            keep: KeepList(0),
            own,
        },
        Ins::TailCallValue { callee, args, own },
        Ins::TailCallValueWide {
            callee,
            args: ArgList(0),
            own,
        },
        Ins::Ret {
            src: Taken(VReg(0)),
            own,
        },
        Ins::Jump { to: Label(0) },
        Ins::Bad { why },
    ];
    for (ins, op) in ending.iter().zip(ends) {
        assert_eq!(ins.op(), op);
        assert!(!ins.falls_through(), "{op:?}");
        assert_eq!(ins.target().is_some(), op == Op::Jump, "{op:?}");
    }
    let hit: Ins<VReg> = Ins::MapFind {
        dst: Hit(VReg(0)),
        map: Src(VReg(1)),
        key: Src(VReg(2)),
        to: Label(4),
    };
    assert!(hit.falls_through() && hit.target() == Some(Label(4)));
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
        let mut ins: Ins<VReg> = Ins::TailCallSelf {
            to: Callee(FuncIdx(0)),
            args: Args::new(&regs).expect("it fits"),
            own: OwnRoom,
        };
        let mut lists = Lists::default();
        ins.decide(&mut lists, &mut |v| bits >> v.0 & 1 == 1);
        let moved = |v: u32| Role::Pass {
            moved: bits >> v & 1 == 1,
        };
        let want: Vec<_> = (0..6).map(|v| (VReg(v), moved(v))).collect();
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

/// Places that give each value the register of its number plus one, and
/// nothing past `fits`.
struct Shifted {
    fits: u32,
}

impl Places for Shifted {
    fn reg(&self, r: VReg) -> Option<Reg> {
        let next = u8::try_from(r.0).ok()?.checked_add(1)?;
        (r.0 < self.fits).then_some(Reg(next))
    }

    fn label(&self, l: Label) -> Option<Pc> {
        (l.0 < 4).then_some(Pc(l.0 + 100))
    }

    fn entry(&self, f: FuncIdx) -> Option<EntryPc> {
        (f.0 < 4).then_some(EntryPc(f.0 + 200))
    }

    fn needs(&self, f: FuncIdx) -> Option<Room> {
        (f.0 < 4).then_some(Room(7))
    }

    fn own(&self) -> Room {
        Room(9)
    }

    fn reg_list(&self, i: u32) -> Option<u32> {
        i.checked_add(10)
    }

    fn arg_list(&self, i: u32) -> Option<u32> {
        i.checked_add(20)
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
        needs: CalleeRoom(FuncIdx(1)),
        own: OwnRoom,
    };
    let placed = call.place(&at).expect("a place for everything");
    let roles = [
        (Reg(4), Role::Write),
        (Reg(2), Role::Pass { moved: false }),
        (Reg(3), Role::Pass { moved: true }),
    ];
    assert_eq!(operands(&placed, &Lists::default()), roles);
    let Ins::Call { to, needs, own, .. } = placed else {
        panic!("a call");
    };
    assert_eq!((to.index(), needs, own), (201, Room(7), Room(9)));

    let wide: Ins<VReg> = Ins::CtorOverWide {
        dst: Dst(VReg(0)),
        variant: Variant::new(VariantRef {
            type_id: TypeId(-5),
            variant_idx: 65_535,
        }),
        fields: ArgList(1),
        reuse: Over(VReg(2)),
    };
    let Some(Ins::CtorOverWide {
        variant, fields, ..
    }) = wide.place(&at)
    else {
        panic!("a constructor");
    };
    let want = VariantRef {
        type_id: TypeId(-5),
        variant_idx: 65_535,
    };
    assert_eq!((variant.get(), fields.0), (want, 21));

    // Nothing is placed when one thing it names has no place.
    let mut lost: Vec<Ins<VReg>> = Vec::new();
    lost.push(Ins::Jump { to: Label(4) });
    lost.push(Ins::Drop {
        reg: Dead(VReg(50)),
    });
    lost.push(Ins::Concat {
        dst: Dst(VReg(0)),
        parts: Looks::new(&[VReg(1), VReg(50)]).expect("it fits"),
    });
    lost.push(Ins::TailCallSelf {
        to: Callee(FuncIdx(0)),
        args: Args::new(&[VReg(50)]).expect("it fits"),
        own: OwnRoom,
    });
    lost.push(Ins::TailCallSelf {
        to: Callee(FuncIdx(4)),
        args: Args::new(&[]).expect("it fits"),
        own: OwnRoom,
    });
    for ins in lost {
        assert!(ins.place(&at).is_none(), "{ins:?}");
    }
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
        assert_eq!(line.contains(" -> "), ins.target().is_some(), "{line}");
    }
    assert!(text.contains("= MapFind(r0, r1) -> "), "{text}");
}

/// The register whose cell a constructor may be written over is told apart
/// from its fields, and so are the registers whose cells a loop carries round
/// from what it passes.
#[test]
fn the_listing_tells_a_kept_cell_from_the_other_operands() {
    let src = "type P {\n\tP(a Int, b Int)\n}\n\
               fn turn(p P) P {\n\
               \tmatch p {\n\
               \t\tP(a, b) -> P(b, a)\n\
               \t}\n\
               }\n\
               fn round(n Int, acc Int) Int {\n\
               \tp = P(n, acc)\n\
               \tmatch p {\n\
               \t\tP(a, b) -> if a == 0 { b } else { round(a - 1, b + a) }\n\
               \t}\n\
               }\n\
               pub fn main() {\n\
               \tprintln(turn(P(1, 2)))\n\
               \tprintln(round(3, 0))\n\
               }\n";
    let listing = crate::listing(&program(src)).expect("the program loads");
    let code: Code = load(&program(src), crate::Audit::Never).expect("the program loads");
    let line = |name: &str, kind: &str| -> String {
        let mut all = (&code.funcs).into_iter();
        let index = all.position(|e| e.name.ends_with(name));
        let text = listing.function(index.expect("a function")).expect("text");
        let found: Vec<_> = text.lines().filter(|l| l.contains(kind)).collect();
        let [line] = found[..] else {
            panic!("one {kind}: {text}");
        };
        line.trim_start().to_string()
    };
    assert_eq!(
        line(".turn", "CtorOver"),
        "4  r0 = CtorOver(10.0, move r2, move r1, over r0)"
    );
    assert_eq!(
        line(".round", "CtorOver"),
        "0  r2 = CtorOver(10.0, move r0, move r1, over r2)"
    );
    assert_eq!(
        line(".round", "TailCallSelfKeep"),
        "9  TailCallSelfKeep(fn#1, move r3, move r0, keep r2)"
    );
}
