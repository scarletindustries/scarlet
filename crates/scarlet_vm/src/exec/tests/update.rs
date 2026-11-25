//! The instructions that update a constructor.

use super::*;
use crate::code::{Assign, AssignList, Sets};

/// Where the tests put the constructor, and where they put the result when it
/// goes to a register of its own.
const BASE: Reg = Reg(FIRST.0 + 250);
const AWAY: Reg = Reg(FIRST.0 + 251);

/// An instruction that updates the constructor in [`BASE`].
struct Updater {
    ins: Ins<Reg>,
    dst: Reg,
    /// How many fields the constructor has.
    n: usize,
    base: Arg<Reg>,
    sets: Vec<(usize, Arg<Reg>)>,
    code: &'static Code,
}

impl Updater {
    /// The register of each value.
    fn regs(&self) -> Vec<Reg> {
        self.sets.iter().map(|(_, a)| a.reg).collect()
    }

    /// The registers it takes from.
    fn taken(&self) -> impl Iterator<Item = Reg> {
        let args = self.sets.iter().map(|(_, a)| *a).chain([self.base]);
        args.filter(|a| a.moved).map(|a| a.reg)
    }
}

/// Every instruction that sets the fields in `at` of a constructor of `n`, from
/// `regs`, with the base and the values taken and shared in several ways. Of the
/// operands that name one register only the last may take it, as the loader has
/// it.
fn updaters(n: usize, at: &[usize], regs: &[Reg], dst: Reg) -> Vec<Updater> {
    let layout = Layout::new(as_wide_as_you_like().1[n], n);
    let last = |i: usize| regs.iter().rposition(|r| *r == regs[i]) == Some(i);
    let mut made = Vec::new();
    for (base, takes) in [(true, 2), (true, 0), (true, 1), (false, 2), (false, 0)] {
        let base = Arg {
            reg: BASE,
            moved: base && !regs.contains(&BASE),
        };
        let value = |i: usize| Arg {
            reg: regs[i],
            moved: last(i) && (takes == 2 || i % 2 == 1 && takes == 1),
        };
        let sets: Vec<_> = at.iter().copied().zip((0..).map(value)).collect();
        let listed: Box<[Assign<Reg>]> = sets
            .iter()
            .map(|(at, value)| Assign {
                field: u16::try_from(*at).expect("a few"),
                value: *value,
            })
            .collect();
        let mut add = |ins: Ins<Reg>| {
            let lists = Lists {
                sets: vec![listed.clone()],
                ..Lists::default()
            };
            let body = vec![ins, Ins::Bad { why: WhyId(0) }];
            let ctors = as_wide_as_you_like().0;
            made.push(Updater {
                ins,
                dst,
                n,
                base,
                sets: sets.clone(),
                code: function_of(ReadsClosure::Nothing, 0, 254, body, lists, ctors),
            });
        };
        add(Ins::UpdateWide {
            dst: Dst(dst),
            base,
            layout,
            sets: AssignList(0),
        });
        macro_rules! form {
            ($($name:ident)*) => {
                $(if let Some(sets) = Sets::new(BASE, layout, &listed) {
                    let mut ins = Ins::$name { dst: Dst(dst), sets };
                    let all = sets.each().map(|(_, a)| a.reg).count() + 1;
                    let mut moved = [base.moved].into_iter().chain(listed.iter().map(|s| s.value.moved));
                    let mut asked = 0;
                    ins.decide(&mut Lists::default(), &mut |_| {
                        asked += 1;
                        moved.next().expect("as many as operands")
                    });
                    assert_eq!(asked, all);
                    add(ins);
                })*
            };
        }
        form!(Update1 Update2 Update3);
    }
    made
}

/// Which fields of `n` a test sets: one at each end and in the middle, two and
/// three of them, all but one and all.
fn fields_set(n: usize) -> Vec<Vec<usize>> {
    let last = n - 1;
    let mut all = vec![
        vec![0],
        vec![last],
        vec![n / 2],
        vec![0, last],
        vec![n / 3, n / 2],
        vec![0, n / 2, last],
        (0..n.min(60)).collect(),
        (0..n).filter(|i| i % 5 == 0 || i + 1 == n).collect(),
        (1..n).rev().take(40).rev().collect(),
    ];
    for at in &mut all {
        at.dedup();
    }
    all.retain(|at| !at.is_empty());
    all
}

/// Where `k` values are: each in a register of its own, as far as there are any,
/// or all in one.
fn value_registers(k: usize) -> Vec<Vec<Reg>> {
    let mut all = vec![(0..k).map(|i| r((i % 240) as u8)).collect::<Vec<_>>()];
    if k > 1 {
        all.push(vec![r(0); k]);
    }
    all
}

/// Who else holds the constructor.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Held {
    Nothing,
    Global,
    /// The register that is written.
    Dst,
    /// The first value, so that it is to hold itself.
    Value,
    /// So many that it is never counted again.
    Everyone,
    /// One fewer than that.
    Almost,
}

/// A frame to run `up` in: a constructor in [`BASE`] with cells and numbers for
/// fields, some of the cells held by a global too, values that are cells and
/// numbers by turns, and a cell in the register that is written. Gives the
/// constructor, its fields, and what each register holds.
fn to_update(m: &mut Machine<'_, '_>, up: &Updater, held: Held) -> (Cell, Vec<Value>, Vec<Value>) {
    m.rest.globals = Vec::new();
    let heap = &mut m.rest.heap;
    let mut fields = Vec::new();
    for i in 0..up.n {
        fields.push(if i % 3 == 2 {
            Value::int(5000 + i as i64).expect("small")
        } else {
            let field = Value::cell(heap.make(Kind::Binary, &[i as u64; 12]).expect("room"));
            if i % 3 == 1 {
                m.rest.globals.push(heap.own(field).expect("live"));
            }
            field
        });
    }
    let ctor = as_wide_as_you_like().1[up.n];
    let cell = heap.ctor(ctor, &fields).expect("room");
    let mut registers = vec![Value::NIL; WINDOW];
    registers[usize::from(BASE.0)] = Value::cell(cell);
    for (i, reg) in up.regs().into_iter().chain([up.dst]).enumerate() {
        let slot = &mut registers[usize::from(reg.0)];
        if slot.bits() == Value::NIL.bits() {
            *slot = if i % 2 == 0 {
                Value::cell(
                    heap.make(Kind::Binary, &[100 + i as u64; 12])
                        .expect("room"),
                )
            } else {
                Value::int(i as i64).expect("small")
            };
        }
    }
    let itself = Value::cell(cell);
    let mut put = |heap: &mut Heap, reg: Reg| {
        if reg != BASE {
            heap.share(cell).expect("live");
            let old = std::mem::replace(&mut registers[usize::from(reg.0)], itself);
            if let Some(old) = old.as_cell() {
                heap.release(old).expect("live");
            }
        }
    };
    match held {
        Held::Nothing => {}
        Held::Global => m.rest.globals.push(heap.own(itself).expect("live")),
        Held::Dst => put(heap, up.dst),
        Held::Value => put(heap, up.regs()[0]),
        Held::Everyone => heap.say_held(cell, i32::MIN),
        Held::Almost => heap.say_held(cell, i32::MAX),
    }
    (cell, fields, registers)
}

/// What each field of the result is to be.
fn updated(up: &Updater, fields: &[Value], registers: &[Value]) -> Vec<u64> {
    let mut want: Vec<u64> = fields.iter().map(|v| v.bits()).collect();
    for (at, value) in &up.sets {
        want[*at] = registers[usize::from(value.reg.0)].bits();
    }
    want
}

/// What the registers are to hold, `dst` left out.
fn left(up: &Updater, registers: &[Value]) -> Vec<u64> {
    let mut left: Vec<u64> = registers.iter().map(|v| v.bits()).collect();
    for reg in up.taken() {
        left[usize::from(reg.0)] = Value::NIL.bits();
    }
    left
}

fn is_left(m: &Machine<'_, '_>, up: &Updater, registers: &[Value], what: &str) {
    let left = left(up, registers);
    for (i, (got, want)) in m.stack.iter().zip(&left).enumerate().skip(2) {
        if i != usize::from(up.dst.0) {
            assert_eq!(got.bits(), *want, "{what}: register {i}");
        }
    }
}

/// Run `check` with every instruction that updates a constructor, of widths on
/// both sides of every line there is.
fn each_updater(widths: impl Iterator<Item = usize>, mut check: impl FnMut(&Updater)) {
    for n in widths {
        for at in fields_set(n) {
            for regs in value_registers(at.len()) {
                for dst in [BASE, regs[0], AWAY] {
                    updaters(n, &at, &regs, dst).iter().for_each(&mut check);
                }
            }
            // The base is to hold itself.
            let mut regs = value_registers(at.len()).swap_remove(0);
            regs[0] = BASE;
            updaters(n, &at, &regs, AWAY).iter().for_each(&mut check);
        }
    }
}

fn widths() -> impl Iterator<Item = usize> {
    (1..=9).chain([16, 17, 20, 252, 253, 254, 300])
}

/// Every instruction that updates a constructor leaves one with the fields it
/// sets and the rest as they were, in the register it writes. One that it takes,
/// and that nothing else holds, is written where it is and no cell is made. Any
/// other is as it was, with one holder fewer when it was taken, and the new one
/// holds each field that was kept as well. What is taken leaves its register and
/// what is shared stays, and each field that is written over is given up once.
#[test]
fn every_instruction_that_updates_a_constructor_sets_its_fields() {
    let mut ran: Vec<&str> = Vec::new();
    let (mut in_place, mut copied) = (0, 0);
    let all = [
        Held::Nothing,
        Held::Global,
        Held::Dst,
        Held::Value,
        Held::Everyone,
        Held::Almost,
    ];
    on_a_machine(|m| {
        each_updater(widths(), |up| {
            ran.push(up.ins.op().name());
            for (held, freed) in all.iter().flat_map(|h| [(*h, false), (*h, true)]) {
                let what = format!("{:?} {:?} {held:?} {freed}", up.ins, up.sets);
                m.rest.heap = Heap::new(up.code.ctors.clone());
                let (cell, fields, registers) = to_update(m, up, held);
                let space = freed.then(|| a_freed_cell(&mut m.rest.heap, up.n));
                let before = m.rest.heap.held_by(cell);
                let made = m.rest.heap.made();
                let shares_itself = up.regs().contains(&BASE) && !up.taken().any(|r| r == BASE);
                let alone = before == Some(1) && up.base.moved && !shares_itself;
                let stop = run_with(m, up.code, &registers);
                assert_eq!(stop, Stop::broken(Broken::NoArm), "{what}");
                let heap = &m.rest.heap;
                let got = m.stack[usize::from(up.dst.0)].as_cell().expect("a cell");
                let want = updated(up, &fields, &registers);
                let have: Vec<_> = heap.values(got).map(Value::bits).collect();
                assert_eq!(have, want, "{what}");
                let ctor = as_wide_as_you_like().1[up.n];
                assert_eq!(heap.ctor_of(got), Some(ctor), "{what}");
                is_left(m, up, &registers, &what);
                assert_eq!(heap.made(), made + usize::from(!alone), "{what}");
                assert_eq!(got == cell, alone, "{what}");
                if alone {
                    in_place += 1;
                    assert_eq!(heap.held_by(got), Some(1), "{what}");
                } else {
                    copied += 1;
                    if let Some(space) = space {
                        assert_eq!(got.place(), space.place(), "{what}");
                    }
                    let gone = heap.held_by(cell) == Some(0);
                    if !gone {
                        let still: Vec<_> = heap.values(cell).map(Value::bits).collect();
                        let want: Vec<_> = fields.iter().map(|v| v.bits()).collect();
                        assert_eq!(still, want, "{what}");
                    }
                }
                counted_by(m, cell, held);
                assert_eq!(check(&m.stack, &m.rest, Value::NIL), Ok(()), "{what}");
                all_given_up(m);
            }
        });
    });
    for form in ["Update1", "Update2", "Update3", "UpdateWide"] {
        assert!(ran.contains(&form), "{form}");
    }
    assert!(in_place > 500 && copied > 5000, "{in_place} {copied}");
}

/// Give `cell`, whose count a test made up, as many holders as hold it.
fn counted_by(m: &mut Machine<'_, '_>, cell: Cell, held: Held) {
    if !matches!(held, Held::Everyone | Held::Almost) {
        return;
    }
    let holders = m.stack.iter().filter(|v| v.as_cell() == Some(cell)).count();
    let within = m.stack[2..]
        .iter()
        .filter_map(|v| v.as_cell())
        .filter(|c| *c != cell && m.rest.heap.is(*c, Kind::Ctor))
        .flat_map(|c| m.rest.heap.values(c))
        .filter(|v| v.as_cell() == Some(cell))
        .count();
    m.rest.globals.push(Value::cell(cell));
    let all = 1 + holders + within;
    m.rest
        .heap
        .say_held(cell, i32::try_from(all).expect("a few"));
}

/// A copy with no room stops with a full heap and has done nothing: nothing is
/// taken, each register holds what it held, and every count is right.
#[test]
fn a_copy_with_no_room_leaves_everything_as_it_was() {
    on_a_machine(|m| {
        each_updater((1..=9).chain([20, 253, 254, 300]), |up| {
            let held = if up.base.moved {
                Held::Global
            } else {
                Held::Nothing
            };
            m.rest.heap = Heap::new(up.code.ctors.clone());
            to_update(m, up, held);
            let used = m.rest.heap.words_used();
            let needs = words_of(up.n);
            for room in [0, 1, needs - 1] {
                let what = format!("{:?} {:?} in {room}", up.ins, up.sets);
                m.rest.heap = Heap::limited_for(used + room, up.code.ctors.clone());
                let (_, _, registers) = to_update(m, up, held);
                let heap = &m.rest.heap;
                let before = (heap.made(), heap.live(), heap.words_used());
                let stop = run_with(m, up.code, &registers);
                assert_eq!(stop, Stop::HeapFull, "{what}");
                let heap = &m.rest.heap;
                let after = (heap.made(), heap.live(), heap.words_used());
                assert_eq!(after, before, "{what}");
                is_untouched(m, &registers, &what);
                assert_eq!(check(&m.stack, &m.rest, Value::NIL), Ok(()), "{what}");
                all_given_up(m);
            }
        });
    });
}

/// Only the constructor that the instruction names is updated. Any other value,
/// and any other cell, whether something else holds it or not, stops it with
/// every register and every count as they were. So does a freed cell.
#[test]
fn only_the_constructor_named_is_updated() {
    on_a_machine(|m| {
        for n in [1, 2, 5, 8, 9, 20, 253, 254, 300] {
            for up in fields_set(n)
                .iter()
                .flat_map(|at| updaters(n, at, &value_registers(at.len())[0], BASE))
            {
                m.rest.heap = Heap::new(up.code.ctors.clone());
                let (cell, _, registers) = to_update(m, &up, Held::Nothing);
                let base = usize::from(BASE.0);
                m.rest.heap.release(cell).expect("live");
                let heap = &mut m.rest.heap;
                let ctors = as_wide_as_you_like().1;
                let nils = |n: usize| vec![Value::NIL; n];
                let mut cells = vec![
                    heap.tuple(&nils(n)).expect("room"),
                    heap.ctor(ctors[n + 1], &nils(n)).expect("room"),
                    heap.ctor(ctors[n], &nils(n - 1)).expect("room"),
                    heap.ctor(ctors[n], &nils(if n == 300 { 20 } else { 350 }))
                        .expect("room"),
                ];
                cells.extend(a_cell_of_each_kind(heap).into_iter().map(|(cell, _)| cell));
                let plain = [
                    Value::int(3).expect("small"),
                    Value::float(0.5),
                    Value::NIL,
                    Value::bool(true),
                    Value::func(FuncIdx(0)),
                    Value::nullary(SOME_VARIANT),
                ];
                let wrong: Vec<Value> = cells.into_iter().map(Value::cell).collect();
                let wrong = wrong.iter().flat_map(|v| [(*v, 1), (*v, 2)]);
                for (v, holders) in wrong.chain(plain.map(|v| (v, 0))) {
                    let what = format!("{:?} of {v:?} held {holders}", up.ins);
                    if let Some(cell) = v.as_cell() {
                        m.rest.heap.say_held(cell, holders);
                    }
                    let mut registers = registers.clone();
                    registers[base] = v;
                    let before = (m.rest.heap.live(), m.rest.heap.made());
                    let stop = run_with(m, up.code, &registers);
                    let want = Stop::broken(Broken::UpdatedOther(v.view()));
                    assert_eq!(stop, want, "{what}");
                    is_untouched(m, &registers, &what);
                    let after = (m.rest.heap.live(), m.rest.heap.made());
                    assert_eq!(after, before, "{what}");
                    if let Some(cell) = v.as_cell() {
                        assert_eq!(m.rest.heap.held_by(cell), Some(holders), "{what}");
                    }
                }
                let freed = m.rest.heap.ctor(ctors[n], &nils(n)).expect("room");
                m.rest.heap.release(freed).expect("live");
                let mut registers = registers.clone();
                registers[base] = Value::cell(freed);
                let before = (m.rest.heap.live(), m.rest.heap.made());
                let stop = run_with(m, up.code, &registers);
                assert_eq!(stop, Stop::broken(Broken::DeadCell), "{:?}", up.ins);
                is_untouched(m, &registers, "freed");
                assert_eq!((m.rest.heap.live(), m.rest.heap.made()), before);
                // Every value has the holders it had.
                registers[base] = Value::NIL;
                for v in registers.iter().filter_map(|v| v.as_cell()) {
                    assert_eq!(m.rest.heap.held_by(v), Some(1), "{:?}", up.ins);
                }
            }
        }
    });
}

/// A field that the constructor does not have is not set. Nothing is written past
/// the cell, and nothing is changed.
#[test]
fn a_field_that_is_not_there_is_not_set() {
    on_a_machine(|m| {
        for n in [1, 2, 8, 9, 20, 253, 254, 300] {
            for at in [vec![n], vec![0, n], vec![0, n, n + 1], vec![n + 7]] {
                for up in updaters(n, &at, &value_registers(at.len())[0], AWAY) {
                    for held in [Held::Nothing, Held::Global] {
                        let what = format!("{:?} {held:?}", up.ins);
                        m.rest.heap = Heap::new(up.code.ctors.clone());
                        let (cell, _, registers) = to_update(m, &up, held);
                        let stop = run_with(m, up.code, &registers);
                        let want = Stop::broken(Broken::UpdatedOther(Value::cell(cell).view()));
                        assert_eq!(stop, want, "{what}");
                        is_untouched(m, &registers, &what);
                        assert_eq!(check(&m.stack, &m.rest, Value::NIL), Ok(()), "{what}");
                        all_given_up(m);
                    }
                }
            }
        }
    });
}

/// A freed cell among the values that are shared stops the instruction, wherever
/// it is among them, and nothing has changed.
#[test]
fn a_freed_value_stops_the_update() {
    on_a_machine(|m| {
        for n in [1, 2, 3, 5, 9, 20, 254] {
            for up in fields_set(n)
                .iter()
                .flat_map(|at| updaters(n, at, &value_registers(at.len())[0], AWAY))
            {
                let shared = up.sets.iter().filter(|(_, a)| !a.moved).map(|(_, a)| a.reg);
                let shared: Vec<_> = shared.collect();
                for dead in shared.iter().take(3).chain(shared.last()) {
                    for held in [Held::Nothing, Held::Global] {
                        let what = format!("{:?} {dead:?} {held:?}", up.ins);
                        m.rest.heap = Heap::new(up.code.ctors.clone());
                        let (_, _, mut registers) = to_update(m, &up, held);
                        let slot = &mut registers[usize::from(dead.0)];
                        let old = std::mem::replace(slot, Value::NIL);
                        m.rest.release(old).expect("live");
                        let gone = a_freed_cell(&mut m.rest.heap, 1);
                        registers[usize::from(dead.0)] = Value::cell(gone);
                        let before = (m.rest.heap.live(), m.rest.heap.made());
                        let stop = run_with(m, up.code, &registers);
                        assert_eq!(stop, Stop::broken(Broken::DeadCell), "{what}");
                        is_untouched(m, &registers, &what);
                        assert_eq!((m.rest.heap.live(), m.rest.heap.made()), before);
                        m.stack[usize::from(dead.0)] = Value::NIL;
                        assert_eq!(check(&m.stack, &m.rest, Value::NIL), Ok(()), "{what}");
                        all_given_up(m);
                    }
                }
            }
        }
    });
}

/// A freed cell among the fields that a copy keeps stops it, wherever it is among
/// them, and nothing has changed. A field that is set is not looked at there, and
/// neither is one that is kept in place.
#[test]
fn a_freed_field_that_a_copy_keeps_stops_it() {
    let (mut stopped, mut went_on) = (0, 0);
    on_a_machine(|m| {
        each_updater([1, 2, 3, 8, 9, 17, 20, 254].into_iter(), |up| {
            if up.regs().contains(&BASE) {
                return;
            }
            let each = (0..up.n).filter(|i| up.n < 10 || *i < 2 || *i > up.n - 3 || *i == 8);
            // The last is as large as the copy, which is then not to go there.
            let sizes = [40, up.n];
            for (at, size) in each.flat_map(|at| sizes.map(|size| (at, size))) {
                for held in [Held::Nothing, Held::Global, Held::Everyone] {
                    let what = format!("{:?} {:?} {at} {size} {held:?}", up.ins, up.sets);
                    m.rest.heap = Heap::new(up.code.ctors.clone());
                    let (cell, mut fields, registers) = to_update(m, up, held);
                    let dead = a_freed_cell(&mut m.rest.heap, size);
                    m.rest.heap.data_mut(cell)[at] = Value::cell(dead).bits();
                    let old = std::mem::replace(&mut fields[at], Value::cell(dead));
                    m.rest.release(old).expect("live");
                    let before = (m.rest.heap.live(), m.rest.heap.made());
                    let sets = up.sets.iter().any(|(i, _)| *i == at);
                    let alone = up.base.moved && held == Held::Nothing;
                    // It is given up as it is written over, with all it holds.
                    if sets && up.dst == BASE && !up.base.moved && held == Held::Nothing {
                        continue;
                    }
                    let stop = run_with(m, up.code, &registers);
                    let now = (m.rest.heap.live(), m.rest.heap.made());
                    if sets && alone {
                        assert_eq!(stop, Stop::broken(Broken::DeadCell), "{what}");
                        is_left(m, up, &registers, &what);
                    } else if sets || alone {
                        went_on += 1;
                        assert_eq!(stop, Stop::broken(Broken::NoArm), "{what}");
                    } else {
                        stopped += 1;
                        assert_eq!(stop, Stop::broken(Broken::DeadCell), "{what}");
                        assert_eq!(now, before, "{what}");
                        is_untouched(m, &registers, &what);
                        m.rest.heap.data_mut(cell)[at] = Value::NIL.bits();
                        counted_by(m, cell, held);
                        assert_eq!(check(&m.stack, &m.rest, Value::NIL), Ok(()), "{what}");
                        all_given_up(m);
                    }
                }
            }
        });
    });
    assert!(stopped > 1000 && went_on > 1000, "{stopped} {went_on}");
}

/// A field that is never counted is kept by a copy like any other, and its count
/// stays. One whose count is at its end is never counted from then on.
#[test]
fn a_copy_keeps_a_field_whose_count_is_at_its_end() {
    let mut kept = 0;
    on_a_machine(|m| {
        each_updater([1, 2, 3, 8, 9, 17, 254].into_iter(), |up| {
            let own = up.regs().contains(&BASE);
            for at in (0..up.n).filter(|i| !own && (up.n < 18 || *i < 2 || *i > up.n - 3)) {
                if up.sets.iter().any(|(i, _)| *i == at) {
                    continue;
                }
                for (holders, after) in [(i32::MIN, None), (i32::MAX, None), (7, Some(8))] {
                    let what = format!("{:?} {:?} {at} {holders}", up.ins, up.sets);
                    m.rest.heap = Heap::new(up.code.ctors.clone());
                    let (cell, mut fields, registers) = to_update(m, up, Held::Global);
                    let heap = &mut m.rest.heap;
                    let field = heap.make(Kind::Binary, &[9; 12]).expect("room");
                    heap.data_mut(cell)[at] = Value::cell(field).bits();
                    let old = std::mem::replace(&mut fields[at], Value::cell(field));
                    m.rest.release(old).expect("live");
                    m.rest.heap.say_held(field, holders);
                    let stop = run_with(m, up.code, &registers);
                    assert_eq!(stop, Stop::broken(Broken::NoArm), "{what}");
                    assert_eq!(m.rest.heap.held_by(field), after, "{what}");
                    let got = m.stack[usize::from(up.dst.0)].as_cell().expect("a cell");
                    let have: Vec<_> = m.rest.heap.values(got).map(Value::bits).collect();
                    assert_eq!(have, updated(up, &fields, &registers), "{what}");
                    // The two constructors hold it.
                    m.rest.heap.say_held(field, 2);
                    assert_eq!(check(&m.stack, &m.rest, Value::NIL), Ok(()), "{what}");
                    all_given_up(m);
                    kept += 1;
                }
            }
        });
    });
    assert!(kept > 5000, "{kept}");
}

/// A freed cell that the instruction has only to give up, as a field it writes
/// over or as what the register it writes held, stops it once everything is in
/// its place.
#[test]
fn a_freed_cell_that_an_update_gives_up_stops_it_at_its_end() {
    on_a_machine(|m| {
        each_updater([1, 2, 3, 5, 9, 20, 254].into_iter(), |up| {
            if up.regs().contains(&BASE) {
                return;
            }
            let fields: Vec<_> = up.sets.iter().map(|(at, _)| Some(*at)).collect();
            let ends = [fields[0], *fields.last().expect("one"), None];
            for at in ends {
                let what = format!("{:?} {:?} {at:?}", up.ins, up.sets);
                m.rest.heap = Heap::new(up.code.ctors.clone());
                let (cell, fields, mut registers) = to_update(m, up, Held::Nothing);
                let dead = Value::cell(a_freed_cell(&mut m.rest.heap, 40));
                match at {
                    Some(at) if up.base.moved => {
                        m.rest.heap.data_mut(cell)[at] = dead.bits();
                        m.rest.release(fields[at]).expect("live");
                    }
                    None if up.dst == AWAY => {
                        let slot = &mut registers[usize::from(AWAY.0)];
                        let old = std::mem::replace(slot, dead);
                        m.rest.release(old).expect("live");
                    }
                    Some(_) | None => continue,
                }
                let stop = run_with(m, up.code, &registers);
                assert_eq!(stop, Stop::broken(Broken::DeadCell), "{what}");
                let got = m.stack[usize::from(up.dst.0)].as_cell().expect("a cell");
                let want = updated(up, &fields, &registers);
                let have: Vec<_> = m.rest.heap.values(got).map(Value::bits).collect();
                for (at, _) in &up.sets {
                    assert_eq!(have[*at], want[*at], "{what}");
                }
                is_left(m, up, &registers, &what);
                assert_eq!(check(&m.stack, &m.rest, Value::NIL), Ok(()), "{what}");
                all_given_up(m);
            }
        });
    });
}

/// Something the host holds is let go of when the field it is in is written over
/// in place, once, and not when something else holds the constructor or when the
/// field is kept.
#[test]
fn a_handle_in_a_field_that_is_written_over_is_let_go_of() {
    on_a_machine(|m| {
        each_updater([1, 2, 5, 9, 20, 253, 254, 300].into_iter(), |up| {
            if up.regs().contains(&BASE) {
                return;
            }
            for at in [0, up.n - 1] {
                for held in [Held::Nothing, Held::Global] {
                    let what = format!("{:?} {:?} {at} {held:?}", up.ins, up.sets);
                    m.rest.heap = Heap::new(up.code.ctors.clone());
                    let (cell, fields, registers) = to_update(m, up, held);
                    let handle = m.rest.heap.make(Kind::Handle, &[41]).expect("room");
                    m.rest.heap.data_mut(cell)[at] = Value::cell(handle).bits();
                    m.rest.release(fields[at]).expect("live");
                    let sets = up.sets.iter().any(|(i, _)| *i == at);
                    let lone = up.base.moved && held == Held::Nothing;
                    let stop = run_with(m, up.code, &registers);
                    assert_eq!(stop, Stop::broken(Broken::NoArm), "{what}");
                    let want = if sets && lone { vec![41] } else { vec![] };
                    // A constructor that is not taken may be in the register that
                    // is written, and is then given up with all it holds.
                    let over = up.dst == BASE && held == Held::Nothing && !up.base.moved;
                    let want = if sets && over { vec![41] } else { want };
                    assert_eq!(m.rest.heap.take_released(), want, "{what}");
                    assert_eq!(check(&m.stack, &m.rest, Value::NIL), Ok(()), "{what}");
                    all_given_up(m);
                }
            }
        });
    });
}

/// Updates over and over free what they make and what they write over, both where
/// they write in place and where the base and the value are read again and so
/// each is a copy.
#[test]
fn updates_in_a_loop_leave_nothing() {
    let (out, left) = cells_left_after(
        "import scarlet/internal\n\
         type P {\n\tP(a String, b Int, c String)\n}\n\
         fn go(p P, q P, n Int) String {\n\
         \tif n == 0 { p.a + q.c } else { go(P(..q, a: 'x'), P(..p, c: p.a, b: n), n - 1) }\n\
         }\n\
         fn kept(p P, s String, n Int, acc Int) Int {\n\
         \tif n == 0 {\n\t\tacc\n\t} else {\n\
         \t\tq = P(..p, a: s)\n\
         \t\tkept(p, s, n - 1, if q.a == s && q.c == p.c { acc + 1 } else { acc })\n\t}\n\
         }\n\
         pub fn main() {\n\
         \tp = P('1', 0, '2')\n\
         \tprintln(go(p, p, 1000))\n\
         \tr = P('${p.b}', 0, '4')\n\
         \ts = 'a${p.b}'\n\
         \tcells = internal::cells_made()\n\
         \tprintln(kept(r, s, 1000, 0) + internal::cells_made() - cells)\n\
         }\n",
    );
    assert_eq!(out, "xx\n2000\n");
    assert_eq!(left, 0);
}
