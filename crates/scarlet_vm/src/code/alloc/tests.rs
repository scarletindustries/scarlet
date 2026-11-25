use super::super::Source;
use super::super::ins::{Arg, Role};
use super::super::live::settle;
use super::super::tests::{SHAPES, draft, of, program};
use super::super::translate::translate;
use super::super::{Tables, literals};
use super::*;
use scarlet_ir::core_ir::{FuncIdx, LoweredFn, Program};
use std::collections::BTreeMap;

fn fitted(mut d: Draft) -> Result<Body, Unfit> {
    let held = settle(&mut d)?;
    allocate(d, &held)
}

/// The register each value of `d` gets.
fn regs(d: Draft) -> Vec<Option<u8>> {
    let body = fitted(d).expect("a function that fits");
    body.regs.iter().map(|r| r.map(|r| r.0)).collect()
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
    let ins = vec![of::constant(0), of::len(1, 0), of::drop(0), of::ret(1)];
    assert_eq!(regs(draft(2, 0, ins, &[])), [Some(2), Some(3)]);
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
    let opened = d.ins.iter().filter(|i| i.op().name() == "Open2");
    assert!(opened.count() >= 300);
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

impl PartialEq for Body {
    fn eq(&self, other: &Body) -> bool {
        self.own == other.own && format!("{:?}", self.ins) == format!("{:?}", other.ins)
    }
}

impl std::fmt::Debug for Body {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.ins)
    }
}

/// The kinds of what is left of `d`, and where its labels stand.
fn left(d: Draft) -> (Vec<String>, Vec<usize>) {
    let body = fitted(d).expect("a function that fits");
    let kinds = body.ins.iter().map(|i| i.op().name().to_string());
    (kinds.collect(), body.labels)
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
    // A write's result does not have its array's register, because the copy on
    // a miss reads the array there.
    let ins = vec![
        of::write(3, [0, 1, 2], 0),
        of::jump(1),
        of::mv(3, 0),
        of::ret(3),
    ];
    let (kinds, labels) = left(draft(4, 3, ins, &[2, 3]));
    assert_eq!(kinds, ["ArraySet", "Jump", "Move", "Ret"]);
    assert_eq!(labels, [2, 3]);
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
}

/// What every register holds. One that is not here holds nothing a path may
/// rely on.
type State = BTreeMap<u8, Holds>;

fn meet(a: &State, b: &State) -> State {
    let mut out = State::new();
    for (r, x) in a {
        let held = match (x, b.get(r)) {
            (x, Some(y)) if x == y => Some(*x),
            (
                Holds::Value(v) | Holds::MaybeTaken(v),
                Some(Holds::Value(w) | Holds::MaybeTaken(w)),
            ) if v == w => Some(Holds::MaybeTaken(*v)),
            _ => None,
        };
        out.extend(held.map(|h| (*r, h)));
    }
    out
}

/// Run the registers `body` gave `d`'s values along every path of `d`, and say
/// what is wrong when a register does not hold the value an instruction reads
/// there. `d` is the function before anything was left out of it.
fn wrong_register(d: &Draft, body: &Body) -> Option<String> {
    let reg = |v: VReg| body.regs[v.0 as usize].map(|r| r.0);
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
            && !(2..body.own.0).contains(&u16::from(r))
        {
            return Some(format!("{v:?} is in r{r} of {:?}", body.own));
        }
    }
    for (p, r) in d.params.iter().zip(2..=u8::MAX) {
        if reg(*p) != Some(r) {
            return Some(format!("{p:?} is in {:?} and arrives in r{r}", reg(*p)));
        }
        let first = state.as_mut().expect("the start");
        first.insert(r, Holds::Value(*p));
    }
    for (i, ins) in d.ins.iter().enumerate() {
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
        let jumps = ins.target().map(|l| l.0 as usize);
        let unread = matches!(ins, Ins::Const { dst, .. } if reg(dst.0).is_none());
        ins.operands(&d.lists, &mut |v, role| {
            let Some(r) = reg(v) else {
                if !unread {
                    wrong = Some(format!("{ins:?} names {v:?}, which has no register"));
                }
                return;
            };
            match role {
                Role::Write | Role::WriteOnHit => written.push((r, v)),
                Role::Drop => {
                    let ok = [Holds::Value(v), Holds::MaybeTaken(v)];
                    if !now.get(&r).is_some_and(|h| ok.contains(h)) {
                        wrong = Some(format!("{i}: {ins:?} drops {v:?} in r{r}: {now:?}"));
                    }
                    taken.push((r, v));
                }
                Role::Look | Role::Share | Role::Pass { .. } | Role::Take => {
                    if now.get(&r) != Some(&Holds::Value(v)) {
                        wrong = Some(format!("{i}: {ins:?} reads {v:?} in r{r}: {now:?}"));
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
            return Some(format!("{i}: {ins:?} writes a register twice: {written:?}"));
        }
        // A jump takes nothing and writes nothing.
        if let Some(l) = jumps {
            arrive(&mut at_label, l, now);
        }
        for (r, v) in taken {
            now.insert(r, Holds::MaybeTaken(v));
        }
        for (r, v) in written {
            now.insert(r, Holds::Value(v));
        }
        if !ins.falls_through() {
            state = None;
        }
    }
    None
}

/// Where the code left in `body` and its labels put each jump, check that it
/// goes to what ran there before anything was left out.
fn wrong_jump(d: &Draft, body: &Body) -> Option<String> {
    let reg = |v: VReg| body.regs[v.0 as usize];
    let goes = |ins: &Ins<VReg>| match ins {
        Ins::Move { dst, src } => reg(dst.0) == reg(src.reg),
        Ins::Const { dst, .. } => reg(dst.0).is_none(),
        _ => false,
    };
    // The kinds each label leads to, with what does nothing passed over.
    let follow = |ins: &[Ins<VReg>], labels: &[usize], from: usize| {
        let mut out = Vec::new();
        let mut at = from;
        while out.len() < 12 {
            let Some(i) = ins.get(at) else { break };
            match i {
                Ins::Jump { to } => {
                    at = labels[to.0 as usize];
                    continue;
                }
                _ if goes(i) => {}
                _ => out.push(i.op()),
            }
            at += 1;
            if !i.falls_through() {
                break;
            }
        }
        out
    };
    for l in 0..d.labels.len() {
        let before = follow(&d.ins, &d.labels, d.labels[l]);
        let now = follow(&body.ins, &body.labels, body.labels[l]);
        if before != now {
            return Some(format!("label {l} led to {before:?} and leads to {now:?}"));
        }
    }
    let before = follow(&d.ins, &d.labels, 0);
    let now = follow(&body.ins, &body.labels, 0);
    (before != now).then(|| format!("the start led to {before:?} and leads to {now:?}"))
}

fn copy_of(d: &Draft) -> Draft {
    Draft {
        ins: d.ins.clone(),
        lists: Lists {
            regs: d.lists.regs.clone(),
            args: d.lists.args.clone(),
            dsts: d.lists.dsts.clone(),
        },
        labels: d.labels.clone(),
        values: d.values,
        params: d.params.clone(),
        closure: d.closure,
    }
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
    let body = match allocate(d, &held) {
        Ok(body) => body,
        Err(Unfit::TooMany(Limit::Values)) => return None,
        Err(why) => panic!("{what}: {why:?}"),
    };
    if let Some(wrong) = wrong_register(&before, &body) {
        panic!("{what}: {wrong}");
    }
    if let Some(wrong) = wrong_jump(&before, &body) {
        panic!("{what}: {wrong}");
    }
    Some(body.own.len())
}

/// Check every function of `program`.
fn check_program(program: &Program, what: &str) -> usize {
    let source = &Source::new(program).expect("a whole program");
    let mut checked = 0;
    let mut one = |f: &LoweredFn, this: Option<FuncIdx>| {
        let what = format!("{what}: {}.{}", f.module, f.name);
        let mut tables = Tables::default();
        let Ok(d) = translate(source, &f.core, this, &mut tables) else {
            return;
        };
        if check(d, &what).is_none() {
            let pieces = literals::in_pieces(&f.core);
            let core = pieces.unwrap_or_else(|| panic!("{what} does not fit"));
            let d = translate(source, &core, this, &mut tables);
            check(d.expect("it loads"), &what).unwrap_or_else(|| panic!("{what} does not fit"));
        }
        checked += 1;
    };
    for (f, i) in (&program.fns).into_iter().zip(0..) {
        one(f, Some(FuncIdx(i)));
    }
    for f in program.inits.iter().chain([&program.toplevel]) {
        one(f, None);
    }
    checked
}

/// Every function of every program in the repository that is one file, and of
/// the stdlib modules they import: each register an instruction reads holds
/// the value it is to read, on every path that reaches it.
#[test]
fn every_register_read_holds_its_value_in_every_program() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    for dir in ["examples", "crates/scarlet/tests/programs"] {
        let all = std::fs::read_dir(root.join(dir)).expect("a directory");
        files.extend(all.map(|f| f.expect("a file").path()));
    }
    let benches = std::fs::read_dir(root.join("benches")).expect("a directory");
    for dir in benches.map(|f| f.expect("a file").path()) {
        if let Some(name) = dir.file_name() {
            files.push(dir.join(name).with_extension("sca"));
        }
    }
    let (mut programs, mut functions) = (0, 0);
    for path in files {
        if path.extension().is_none_or(|e| e != "sca") {
            continue;
        }
        let Ok(src) = std::fs::read_to_string(&path) else {
            continue;
        };
        let result = scarlet_core::compile::compile_text(&src, None);
        let Some(program) = result.into_runnable() else {
            continue;
        };
        programs += 1;
        functions += check_program(&program, &path.display().to_string());
    }
    assert!(programs > 60, "{programs} programs");
    assert!(functions > 3000, "{functions} functions");
    assert!(check_program(&program(SHAPES), "shapes") > 4);
}

/// Numbers from a generator that gives the same ones each time.
struct Random(u64);

impl Random {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        usize::try_from(self.0 >> 20).expect("64 bits") % n
    }

    fn of<T: Copy>(&mut self, from: &[T]) -> T {
        from[self.below(from.len())]
    }
}

/// Makes a function at random, as `translate` would: every value is written
/// before it is read on each path, every jump goes forward, and a value may be
/// dropped once and not read after.
struct Maker {
    random: Random,
    ins: Vec<Ins<VReg>>,
    labels: Vec<usize>,
    /// The numbers no value has yet, in no order, so that a value written late
    /// may have a lower number than one that is held then.
    numbers: Vec<u32>,
}

impl Maker {
    fn value(&mut self) -> u32 {
        self.numbers.pop().expect("enough numbers")
    }

    fn label(&mut self) -> u32 {
        self.labels.push(usize::MAX);
        u32::try_from(self.labels.len() - 1).expect("few")
    }

    fn place(&mut self, l: u32) {
        self.labels[l as usize] = self.ins.len();
    }

    /// `n` steps of straight code over the values in `scope`, which it adds
    /// to. It may drop those past the first `theirs`, which are the ones no
    /// code after it reads.
    fn steps(&mut self, n: usize, depth: usize, scope: &mut Vec<u32>, theirs: usize) {
        for _ in 0..n {
            let a = self.random.of(scope);
            let b = self.random.of(scope);
            let c = self.random.of(scope);
            match self.random.below(16) {
                0 | 1 => {
                    let v = self.value();
                    self.ins.push(of::constant(v));
                    scope.push(v);
                }
                2 => {
                    let v = self.value();
                    self.ins.push(of::mv(v, a));
                    scope.push(v);
                }
                3 | 4 => {
                    let v = self.value();
                    self.ins.push(of::add(v, a, b));
                    scope.push(v);
                }
                5 => {
                    let v = self.value();
                    self.ins.push(of::append(v, a, b));
                    scope.push(v);
                }
                6 => {
                    let v = self.value();
                    self.ins.push(of::call(v, &[a, b, c]));
                    scope.push(v);
                }
                7 | 8 if scope.len() > theirs.max(1) => {
                    let i = theirs + self.random.below(scope.len() - theirs);
                    let v = scope.remove(i);
                    self.ins.push(of::drop(v));
                }
                // A cell taken apart, with a register for some of its fields.
                9 if scope.len() > theirs => {
                    let i = theirs + self.random.below(scope.len() - theirs);
                    let v = scope.remove(i);
                    let wanted = 1 + self.random.below(3);
                    let dsts = [wanted & 1, wanted & 2].map(|w| (w > 0).then(|| self.value()));
                    self.ins.push(of::open(dsts, v));
                    scope.extend(dsts.into_iter().flatten());
                }
                10 | 11 if depth < 4 => self.branch(depth, scope),
                12 if depth < 4 => self.find(depth, scope),
                13 => self.write(scope),
                14 if depth < 4 => self.tail(depth, scope),
                _ => {
                    let v = self.value();
                    self.ins.push(of::len(v, a));
                    scope.push(v);
                }
            }
        }
    }

    /// An `if` whose two sides give one value, or leave the function.
    fn branch(&mut self, depth: usize, scope: &mut Vec<u32>) {
        let cond = self.random.of(scope);
        let joined = self.value();
        let (other, after) = (self.label(), self.label());
        self.ins.push(of::unless(cond, other));
        for side in 0..2 {
            let mut inner = scope.clone();
            let n = self.random.below(6);
            self.steps(n, depth + 1, &mut inner, scope.len());
            let from = self.random.of(&inner);
            match self.random.below(3) {
                0 => self.ins.push(of::constant(joined)),
                _ => self.ins.push(of::mv(joined, from)),
            }
            self.ins.push(of::jump(after));
            if side == 0 {
                self.place(other);
            }
        }
        self.place(after);
        scope.push(joined);
        self.ins.push(of::len(joined, joined));
    }

    /// A lookup with a side of its own for a key that is not there.
    fn find(&mut self, depth: usize, scope: &mut Vec<u32>) {
        let (m, k) = (self.random.of(scope), self.random.of(scope));
        let found = self.value();
        let joined = self.value();
        let (miss, after) = (self.label(), self.label());
        self.ins.push(of::find(found, [m, k], miss));
        let mut inner = scope.clone();
        inner.push(found);
        let n = self.random.below(5);
        self.steps(n, depth + 1, &mut inner, scope.len());
        let from = self.random.of(&inner);
        self.ins.push(of::mv(joined, from));
        self.ins.push(of::jump(after));
        self.place(miss);
        let mut inner = scope.clone();
        let n = self.random.below(5);
        self.steps(n, depth + 1, &mut inner, scope.len());
        let from = self.random.of(&inner);
        self.ins.push(of::mv(joined, from));
        self.place(after);
        scope.push(joined);
    }

    /// A write that gives the array back when it misses.
    fn write(&mut self, scope: &mut Vec<u32>) {
        let operands = [
            self.random.of(scope),
            self.random.of(scope),
            self.random.of(scope),
        ];
        let v = self.value();
        let (miss, after) = (self.label(), self.label());
        self.ins.push(of::write(v, operands, miss));
        self.ins.push(of::jump(after));
        self.place(miss);
        self.ins.push(of::mv(v, operands[0]));
        self.place(after);
        scope.push(v);
    }

    /// A path that leaves the function.
    fn tail(&mut self, depth: usize, scope: &[u32]) {
        let cond = self.random.of(scope);
        let on = self.label();
        self.ins.push(of::unless(cond, on));
        let mut inner = scope.to_vec();
        let n = self.random.below(5);
        self.steps(n, depth + 1, &mut inner, 0);
        self.end(&inner);
        self.place(on);
    }

    fn end(&mut self, scope: &[u32]) {
        let args = [
            self.random.of(scope),
            self.random.of(scope),
            self.random.of(scope),
        ];
        if self.random.below(2) == 0 {
            self.ins.push(of::again(&args));
        } else {
            self.ins.push(of::ret(args[0]));
        }
    }
}

fn random_function(seed: u64, steps: usize) -> Draft {
    let mut random = Random(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let values = u32::try_from(20_000.min(3 + steps * 100)).expect("few");
    let mut numbers: Vec<u32> = (3..values).collect();
    for i in (1..numbers.len()).rev() {
        numbers.swap(i, random.below(i + 1));
    }
    let mut m = Maker {
        random,
        ins: Vec::new(),
        labels: Vec::new(),
        numbers,
    };
    let mut scope = vec![0, 1, 2];
    m.steps(steps, 0, &mut scope, 0);
    m.end(&scope);
    draft(values, 3, m.ins, &m.labels)
}

/// The same for functions made at random: straight code, branches, lookups
/// and writes that miss, cells taken apart, and paths that leave early.
#[test]
fn every_register_read_holds_its_value_in_random_functions() {
    let mut fitted = 0;
    for seed in 0..3000u64 {
        let steps = 5 + usize::try_from(seed % 60).expect("small");
        let d = random_function(seed, steps);
        fitted += usize::from(check(d, &format!("seed {seed}")).is_some());
    }
    assert!(fitted > 2900, "{fitted} fit");
    // Long ones, most of which hold too much at once.
    for seed in 0..40u64 {
        check(random_function(seed, 600), &format!("long seed {seed}"));
    }
}

/// The check finds a register given to two values held at once, which is the
/// mistake it is for.
#[test]
fn a_register_shared_by_mistake_is_found() {
    let ins = vec![
        of::constant(0),
        of::constant(1),
        of::add(2, 0, 1),
        of::ret(2),
    ];
    let mut d = draft(3, 0, ins, &[]);
    let held = settle(&mut d).expect("a function that loads");
    let before = copy_of(&d);
    let mut body = allocate(d, &held).expect("a function that fits");
    assert_eq!(wrong_register(&before, &body), None);
    body.regs[1] = body.regs[0];
    assert!(wrong_register(&before, &body).is_some());
    // And one given to two values that are written at once.
    let ins = vec![of::open([Some(1), Some(2)], 0), of::ret(1)];
    let mut d = draft(3, 1, ins, &[]);
    let held = settle(&mut d).expect("a function that loads");
    let before = copy_of(&d);
    let mut body = allocate(d, &held).expect("a function that fits");
    assert_eq!(wrong_register(&before, &body), None);
    body.regs[2] = body.regs[1];
    assert!(wrong_register(&before, &body).is_some());
    // And a value read after it was taken.
    let mut d = draft(3, 1, vec![of::mv(1, 0), of::len(2, 0), of::ret(1)], &[]);
    let held = settle(&mut d).expect("a function that loads");
    if let Ins::Move { src, .. } = &mut d.ins[0] {
        *src = Arg {
            moved: true,
            ..*src
        };
    }
    let before = copy_of(&d);
    let body = allocate(d, &held).expect("a function that fits");
    assert!(wrong_register(&before, &body).is_some());
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
