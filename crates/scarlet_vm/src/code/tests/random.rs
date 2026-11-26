//! Functions made at random, for the tests of the passes.

use super::super::ins::{Ins, IntTable, Label, Lists, PlaceList, Src, Table, VReg};
use super::super::translate::{Draft, Step};
use super::{draft, of};

/// Numbers from a generator that gives the same ones each time.
pub(in super::super) struct Random(pub(in super::super) u64);

impl Random {
    pub(in super::super) fn below(&mut self, n: usize) -> usize {
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
    steps: Vec<Step>,
    /// How many parameters it has.
    params: u32,
    labels: Vec<usize>,
    lists: Lists<VReg>,
    /// [`Draft::words`].
    words: Vec<(Label, VReg)>,
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
        self.labels[l as usize] = self.steps.len();
    }

    fn push(&mut self, ins: Ins<VReg>) {
        self.steps.push(Step::Ins(ins));
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
                    self.push(of::constant(v));
                    scope.push(v);
                }
                2 => {
                    let v = self.value();
                    self.push(of::mv(v, a));
                    scope.push(v);
                }
                3 | 4 => {
                    let v = self.value();
                    self.push(of::add(v, a, b));
                    scope.push(v);
                }
                5 => {
                    let v = self.value();
                    self.push(of::append(v, a, b));
                    scope.push(v);
                }
                6 => {
                    let v = self.value();
                    self.push(of::call(v, &[a, b, c]));
                    scope.push(v);
                }
                7 | 8 if scope.len() > theirs.max(1) => {
                    let i = theirs + self.random.below(scope.len() - theirs);
                    let v = scope.remove(i);
                    self.push(of::drop(v));
                }
                // A cell taken apart, with a register for some of its fields.
                9 if scope.len() > theirs => {
                    let i = theirs + self.random.below(scope.len() - theirs);
                    let v = scope.remove(i);
                    let wanted = 1 + self.random.below(3);
                    let dsts = [wanted & 1, wanted & 2].map(|w| (w > 0).then(|| self.value()));
                    self.push(of::open(dsts, v));
                    scope.extend(dsts.into_iter().flatten());
                }
                10 if depth < 4 && self.random.below(2) == 0 => self.switch(depth, scope),
                10 | 11 if depth < 4 => self.branch(depth, scope),
                12 if depth < 4 => self.find(depth, scope),
                13 => self.write(scope),
                14 if depth < 4 => self.tail(depth, scope),
                _ => {
                    let v = self.value();
                    self.push(of::len(v, a));
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
        self.push(of::unless(cond, other));
        for side in 0..2 {
            let mut inner = scope.clone();
            let n = self.random.below(6);
            self.steps(n, depth + 1, &mut inner, scope.len());
            let from = self.random.of(&inner);
            match self.random.below(3) {
                0 => self.push(of::constant(joined)),
                _ => self.push(of::mv(joined, from)),
            }
            self.push(of::jump(after));
            if side == 0 {
                self.place(other);
            }
        }
        self.place(after);
        scope.push(joined);
        self.push(of::len(joined, joined));
    }

    /// A switch on a variant or on an Int, whose sides give one value.
    fn switch(&mut self, depth: usize, scope: &mut Vec<u32>) {
        let on = self.random.of(scope);
        let joined = self.value();
        let after = self.label();
        let sides: Vec<Label> = (0..1 + self.random.below(4))
            .map(|_| Label(self.label()))
            .collect();
        let src = Src(VReg(on));
        if self.random.below(4) == 0 {
            let many = self.random.below(6);
            let keys = (0..10).map(|i| i * 3).take(many);
            let places: Vec<_> = keys.map(|n| (n, self.random.of(&sides))).collect();
            let to = self.random.of(&sides);
            self.lists.places.push(places.into());
            let last = u32::try_from(self.lists.places.len() - 1).expect("few");
            let places = PlaceList(last);
            self.push(Ins::SwitchIntFar { src, places, to });
        } else {
            let len = [4, 8][self.random.below(2)];
            let to: Vec<Label> = (0..len).map(|_| self.random.of(&sides)).collect();
            let first = u32::try_from(self.lists.tables.len()).expect("few");
            let last = u16::try_from(len - 1).expect("few");
            let table = Table { first, last };
            self.lists.tables.extend(&to);
            if self.random.below(3) == 0 {
                let table = IntTable { table, first: 2 };
                self.push(Ins::SwitchInt { src, table });
            } else {
                // A value that is the second of any four has no fields (`variant`).
                for side in &sides {
                    let mut of = (0..len).filter(|v| to[*v] == *side).peekable();
                    if of.peek().is_some() && of.all(|v| v % 4 == 1) {
                        self.words.push((*side, VReg(on)));
                    }
                }
                self.push(Ins::Switch { src, table });
            }
        }
        let last = sides.len() - 1;
        for (i, side) in sides.into_iter().enumerate() {
            self.place(side.0);
            // Two of them may be at one place.
            if i < last && self.random.below(3) == 0 {
                continue;
            }
            let mut inner = scope.clone();
            let n = self.random.below(5);
            self.steps(n, depth + 1, &mut inner, scope.len());
            let from = self.random.of(&inner);
            self.push(of::mv(joined, from));
            self.push(of::jump(after));
        }
        self.place(after);
        scope.push(joined);
        self.push(of::len(joined, joined));
    }

    /// A lookup with a side of its own for a key that is not there.
    fn find(&mut self, depth: usize, scope: &mut Vec<u32>) {
        let (m, k) = (self.random.of(scope), self.random.of(scope));
        let found = self.value();
        let joined = self.value();
        let (miss, after) = (self.label(), self.label());
        self.push(of::find(found, [m, k], miss));
        let mut inner = scope.clone();
        inner.push(found);
        let n = self.random.below(5);
        self.steps(n, depth + 1, &mut inner, scope.len());
        let from = self.random.of(&inner);
        self.push(of::mv(joined, from));
        self.push(of::jump(after));
        self.place(miss);
        let mut inner = scope.clone();
        let n = self.random.below(5);
        self.steps(n, depth + 1, &mut inner, scope.len());
        let from = self.random.of(&inner);
        self.push(of::mv(joined, from));
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
        self.push(of::write(v, operands, miss));
        self.push(of::jump(after));
        self.place(miss);
        self.push(of::mv(v, operands[0]));
        self.place(after);
        scope.push(v);
    }

    /// A path that leaves the function.
    fn tail(&mut self, depth: usize, scope: &[u32]) {
        let cond = self.random.of(scope);
        let on = self.label();
        self.push(of::unless(cond, on));
        let mut inner = scope.to_vec();
        let n = self.random.below(5);
        self.steps(n, depth + 1, &mut inner, 0);
        self.end(&inner);
        self.place(on);
    }

    /// The end of a path: a return, or a tail call of the function to itself or
    /// to another. Its arguments are often its parameters, in another order.
    fn end(&mut self, scope: &[u32]) {
        let own = self.some_of(scope, self.params as usize);
        match self.random.below(5) {
            0 | 1 => self.steps.push(of::again(&own)),
            2 => {
                let n = self.random.below(9);
                let args = self.some_of(scope, n);
                self.steps.push(of::enter(7, &args));
            }
            _ => self.push(of::ret(own[0])),
        }
    }

    /// `n` of `scope`, and often parameters among them.
    fn some_of(&mut self, scope: &[u32], n: usize) -> Vec<u32> {
        let params: Vec<u32> = (0..self.params).filter(|p| scope.contains(p)).collect();
        let mut one = || match self.random.below(3) {
            0 if !params.is_empty() => self.random.of(&params),
            _ => self.random.of(scope),
        };
        (0..n).map(|_| one()).collect()
    }
}

/// A function of `params` parameters and about so many steps.
pub(in super::super) fn random_function(seed: u64, params: u32, steps: usize) -> Draft {
    let mut random = Random(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let values = u32::try_from(20_000.min(params as usize + steps * 100)).expect("few");
    let mut numbers: Vec<u32> = (params..values).collect();
    for i in (1..numbers.len()).rev() {
        numbers.swap(i, random.below(i + 1));
    }
    let mut m = Maker {
        random,
        steps: Vec::new(),
        params,
        labels: Vec::new(),
        lists: Lists::default(),
        words: Vec::new(),
        numbers,
    };
    let mut scope: Vec<u32> = (0..params).collect();
    m.steps(steps, 0, &mut scope, 0);
    m.end(&scope);
    let mut d = draft(values, params, m.steps, &m.labels);
    (d.lists, d.words) = (m.lists, m.words);
    d
}

/// Which variant of its type the value `by` stands for is.
pub(in super::super) fn variant(by: u64, table: Table) -> u64 {
    by % (u64::from(table.last) + 1)
}
