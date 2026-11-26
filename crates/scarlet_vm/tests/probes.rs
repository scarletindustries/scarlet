//! Programs made at random of reads, slices, writes and lookups, in every use that their
//! values have, against a model of what each gives. Each is run with its heap checked,
//! and some with the heap checked before every instruction.

use std::collections::BTreeMap;

use scarlet_vm::{Audit, OsHost};

/// A source of numbers that look random.
struct Dice(u64);

impl Dice {
    fn below(&mut self, n: usize) -> usize {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        usize::try_from((z ^ (z >> 31)) % n as u64).expect("small")
    }

    fn of<T: Clone>(&mut self, all: &[T]) -> T {
        all[self.below(all.len())].clone()
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Ty {
    Int,
    Array,
    Map,
    /// `Option(Int)`, kept under a name.
    Option,
    /// `Result(Array(Int), Nil)`.
    Result,
    /// A record that holds an array, a map and an Int.
    Record,
}

#[derive(Debug, Clone, PartialEq)]
enum Val {
    Int(i128),
    Array(Vec<i128>),
    Map(BTreeMap<i128, i128>),
    Option(Option<i128>),
    Result(Option<Vec<i128>>),
    Record(Vec<i128>, BTreeMap<i128, i128>, i128),
}

impl Val {
    fn int(&self) -> i128 {
        match self {
            Val::Int(n) => *n,
            other => panic!("{other:?}"),
        }
    }

    fn array(self) -> Vec<i128> {
        match self {
            Val::Array(xs) => xs,
            other => panic!("{other:?}"),
        }
    }

    fn map(self) -> BTreeMap<i128, i128> {
        match self {
            Val::Map(m) => m,
            other => panic!("{other:?}"),
        }
    }
}

type B = Box<E>;

/// An expression. A name is its number in the scope.
#[derive(Debug, Clone)]
enum E {
    Name(usize),
    Num(i128),
    Add(B, B),
    /// `a % 97`, which keeps numbers small.
    Small(B),
    Length(B),
    If(B, B, B, B),
    Literal(Vec<E>),
    Range(B, B),
    Append(B, B),
    Prepend(B, B),
    /// `xs[i] or e`.
    GetOr(B, B, B),
    /// `match xs[i]`, whose `Some` arm has a name.
    GetMatch(B, B, usize, B, B),
    /// The same with a literal, a guard and an or-pattern in front.
    GetCases(B, B, usize, [B; 4]),
    /// `Some(_)`, for whether it is there.
    GetHas(B, B, B, B),
    Get(B, B),
    /// `option::unwrap(o, e)`.
    Unwrap(B, B),
    /// A match on an `Option` that has a name.
    OptionMatch(B, usize, B, B),
    Put(B, B, B),
    Set(B, B, B),
    SetOr(B, B, B, B),
    SetMatch(B, B, B, usize, B, B),
    /// `Ok(_)`, for whether it would be.
    SetHas(B, B, B, B),
    /// `result::then(r, fn(ys) array::set(ys, i, x))`.
    Then(B, B, B),
    ResultOr(B, B),
    Slice(B, B, B),
    SliceOr(B, B, B, B),
    SliceMatch(B, B, B, usize, B, B),
    Swap(B, B, B),
    MapSet(B, B, B),
    MapDelete(B, B),
    LookOr(B, B, B),
    LookMatch(B, B, usize, B, B),
    Look(B, B),
    Record(B, B, B),
    /// The array, the map or the Int of a record.
    Of(B, usize),
    /// `R(..r, xs: e)`, where `e` has the name of the record.
    WithArray(usize, B),
    WithMap(usize, B),
    /// A call of a function that is given back what it is given.
    Same(B),
    /// A call of a closure that captures what its body names.
    Closure(B),
}

struct Maker<'d> {
    dice: &'d mut Dice,
    /// The type of every name there is or was.
    types: Vec<Ty>,
}

impl Maker<'_> {
    fn name(&mut self, ty: Ty) -> usize {
        self.types.push(ty);
        self.types.len() - 1
    }

    fn boxed(&mut self, ty: Ty, deep: usize, scope: &[usize]) -> B {
        Box::new(self.expr(ty, deep, scope))
    }

    fn index(&mut self, deep: usize, scope: &[usize]) -> B {
        Box::new(match self.dice.below(12) {
            0 => E::Num(-1),
            1 => E::Num(self.dice.of(&[4000, 1 << 50, 1 << 62])),
            2 => E::Add(Box::new(E::Num(1 << 62)), Box::new(E::Num(1 << 62))),
            3 => E::Num(0),
            4 if deep > 0 => E::Length(self.boxed(Ty::Array, deep - 1, scope)),
            5 if deep > 0 => self.expr(Ty::Int, deep - 1, scope),
            _ => E::Num(self.dice.below(6) as i128),
        })
    }

    /// `inner` in scope beside `scope`.
    fn with(scope: &[usize], inner: usize) -> Vec<usize> {
        [scope, &[inner]].concat()
    }

    fn expr(&mut self, ty: Ty, deep: usize, scope: &[usize]) -> E {
        let mine: Vec<usize> = scope
            .iter()
            .copied()
            .filter(|n| self.types[*n] == ty)
            .collect();
        if !mine.is_empty() && (deep == 0 || self.dice.below(4) == 0) {
            return E::Name(self.dice.of(&mine));
        }
        let d = deep.saturating_sub(1);
        if deep > 0 {
            match self.dice.below(14) {
                0 => {
                    let (a, b) = (self.boxed(Ty::Int, d, scope), self.boxed(Ty::Int, d, scope));
                    return E::If(a, b, self.boxed(ty, d, scope), self.boxed(ty, d, scope));
                }
                1 => return E::Same(self.boxed(ty, d, scope)),
                2 => return E::Closure(self.boxed(ty, d, scope)),
                _ => {}
            }
        }
        macro_rules! b {
            ($ty:ident) => {
                self.boxed(Ty::$ty, d, scope)
            };
        }
        match ty {
            Ty::Int if deep == 0 => E::Num(self.dice.below(9) as i128 - 1),
            Ty::Array if deep == 0 => match self.dice.below(4) {
                0 => E::Range(
                    Box::new(E::Num(1)),
                    Box::new(E::Num(self.dice.below(60) as i128)),
                ),
                _ => {
                    let n = self.dice.of(&[0, 1, 2, 3, 5, 40]);
                    let one = |i| E::Num(i as i128 * 7 % 11);
                    E::Literal((0..n).map(one).collect())
                }
            },
            Ty::Int => match self.dice.below(14) {
                0 => E::Num(self.dice.below(9) as i128),
                1 => E::Small(Box::new(E::Add(b!(Int), b!(Int)))),
                2 => E::Length(b!(Array)),
                3 | 4 => E::GetOr(b!(Array), self.index(d, scope), b!(Int)),
                5 => {
                    let x = self.name(Ty::Int);
                    let some = self.boxed(Ty::Int, d, &Self::with(scope, x));
                    E::GetMatch(b!(Array), self.index(d, scope), x, some, b!(Int))
                }
                6 => {
                    let x = self.name(Ty::Int);
                    let inner = Self::with(scope, x);
                    let arms = [
                        b!(Int),
                        self.boxed(Ty::Int, d, &inner),
                        self.boxed(Ty::Int, d, &inner),
                        b!(Int),
                    ];
                    E::GetCases(b!(Array), self.index(d, scope), x, arms)
                }
                7 => E::GetHas(b!(Array), self.index(d, scope), b!(Int), b!(Int)),
                8 => E::Unwrap(b!(Option), b!(Int)),
                9 => {
                    let x = self.name(Ty::Int);
                    let some = self.boxed(Ty::Int, d, &Self::with(scope, x));
                    E::OptionMatch(b!(Option), x, some, b!(Int))
                }
                10 => E::SetHas(b!(Array), self.index(d, scope), b!(Int), b!(Int)),
                11 => E::LookOr(b!(Map), b!(Int), b!(Int)),
                12 => {
                    let x = self.name(Ty::Int);
                    let some = self.boxed(Ty::Int, d, &Self::with(scope, x));
                    E::LookMatch(b!(Map), b!(Int), x, some, b!(Int))
                }
                _ => E::Of(b!(Record), 2),
            },
            Ty::Array => match self.dice.below(14) {
                0 => E::Range(Box::new(E::Num(0)), b!(Int)),
                1 => E::Append(b!(Array), b!(Int)),
                2 => E::Prepend(b!(Int), b!(Array)),
                3 | 4 => E::Put(b!(Array), self.index(d, scope), b!(Int)),
                5 | 6 => E::SetOr(b!(Array), self.index(d, scope), b!(Int), b!(Array)),
                7 => {
                    let ys = self.name(Ty::Array);
                    let ok = self.boxed(Ty::Array, d, &Self::with(scope, ys));
                    E::SetMatch(b!(Array), self.index(d, scope), b!(Int), ys, ok, b!(Array))
                }
                8 => E::ResultOr(b!(Result), b!(Array)),
                9 => {
                    let (a, b) = (self.index(d, scope), self.index(d, scope));
                    E::SliceOr(b!(Array), a, b, b!(Array))
                }
                10 => {
                    let ys = self.name(Ty::Array);
                    let ok = self.boxed(Ty::Array, d, &Self::with(scope, ys));
                    let (a, b) = (self.index(d, scope), self.index(d, scope));
                    E::SliceMatch(b!(Array), a, b, ys, ok, b!(Array))
                }
                11 => E::Swap(b!(Array), self.index(d, scope), self.index(d, scope)),
                12 => E::Of(b!(Record), 0),
                _ => E::Literal((0..3).map(|_| self.expr(Ty::Int, d, scope)).collect()),
            },
            Ty::Map => match self.dice.below(6) {
                _ if deep == 0 => E::MapSet(
                    Box::new(E::MapSet(
                        Box::new(E::Literal(Vec::new())),
                        Box::new(E::Num(1)),
                        Box::new(E::Num(5)),
                    )),
                    Box::new(E::Num(3)),
                    Box::new(E::Num(6)),
                ),
                0..=2 => E::MapSet(b!(Map), b!(Int), b!(Int)),
                3 => E::MapDelete(b!(Map), b!(Int)),
                _ => E::Of(b!(Record), 1),
            },
            Ty::Option => match self.dice.below(3) {
                0 => E::Look(b!(Map), b!(Int)),
                _ => E::Get(b!(Array), self.index(d, scope)),
            },
            Ty::Result => match self.dice.below(5) {
                0 | 1 => E::Set(b!(Array), self.index(d, scope), b!(Int)),
                2 if deep > 0 => E::Then(b!(Result), self.index(d, scope), b!(Int)),
                _ => E::Slice(b!(Array), self.index(d, scope), self.index(d, scope)),
            },
            Ty::Record => {
                let records = mine.len();
                match self.dice.below(3) {
                    0 if records > 0 && deep > 0 => {
                        let r = self.dice.of(&mine);
                        E::WithArray(r, self.boxed(Ty::Array, d, scope))
                    }
                    1 if records > 0 && deep > 0 => {
                        let r = self.dice.of(&mine);
                        E::WithMap(r, self.boxed(Ty::Map, d, scope))
                    }
                    _ => E::Record(b!(Array), b!(Map), b!(Int)),
                }
            }
        }
    }
}

/// The name `n` in a pattern, when `body` reads it.
fn named(n: usize, body: &str) -> String {
    let name = format!("v{n}");
    let ends = |rest: &str| !rest.starts_with(|c: char| c.is_ascii_digit());
    let mut after = body
        .match_indices(&name)
        .map(|(at, _)| &body[at + name.len()..]);
    match after.any(ends) {
        true => name,
        false => "_".to_string(),
    }
}

/// `e` as source. An empty literal that a map is set in is an empty map.
fn source(e: &E) -> String {
    let s = |e: &E| source(e);
    match e {
        E::Name(n) => format!("v{n}"),
        E::Num(n) if *n < 0 => format!("{{ 0 - {} }}", -n),
        E::Num(n) => n.to_string(),
        E::Add(a, b) => format!("{{ {{ {} }} + {{ {} }} }}", s(a), s(b)),
        E::Small(a) => format!("{{ {{ {} }} % 97 }}", s(a)),
        E::Length(a) => format!("array::length({})", s(a)),
        E::If(a, b, x, y) => format!(
            "if {{ {} }} < {{ {} }} {{ {} }} else {{ {} }}",
            s(a),
            s(b),
            s(x),
            s(y)
        ),
        E::Literal(all) => {
            let all: Vec<String> = all.iter().map(source).collect();
            format!("[{}]", all.join(", "))
        }
        E::Range(a, b) => format!("{{ {{ {} }}..{{ {} }} }}", s(a), s(b)),
        E::Append(a, x) => format!("[..{}, {}]", s(a), s(x)),
        E::Prepend(x, a) => format!("[{}, ..{}]", s(x), s(a)),
        E::GetOr(a, i, x) => format!("{{ {{ {} }}[{}] or {{ {} }} }}", s(a), s(i), s(x)),
        E::GetMatch(a, i, n, x, y) => format!(
            "match {{ {} }}[{}] {{\n Some({}) -> {}\n None -> {}\n}}",
            s(a),
            s(i),
            named(*n, &s(x)),
            s(x),
            s(y)
        ),
        E::GetCases(a, i, n, [one, two, three, four]) => format!(
            "match {{ {} }}[{}] {{\n Some(3) | Some(5) -> {}\n Some(v{n}) if v{n} > 4 -> {}\n \
             Some({}) -> {}\n None -> {}\n}}",
            s(a),
            s(i),
            s(one),
            s(two),
            named(*n, &s(three)),
            s(three),
            s(four)
        ),
        E::GetHas(a, i, x, y) => format!(
            "match {{ {} }}[{}] {{\n None -> {}\n Some(_) -> {}\n}}",
            s(a),
            s(i),
            s(y),
            s(x)
        ),
        E::Get(a, i) => format!("{{ {} }}[{}]", s(a), s(i)),
        E::Unwrap(o, x) => format!("option::unwrap({}, {})", s(o), s(x)),
        E::OptionMatch(o, n, x, y) => format!(
            "match {} {{\n Some({}) -> {}\n None -> {}\n}}",
            s(o),
            named(*n, &s(x)),
            s(x),
            s(y)
        ),
        E::Put(a, i, x) => format!("array::put({}, {}, {})", s(a), s(i), s(x)),
        E::Set(a, i, x) => format!("array::set({}, {}, {})", s(a), s(i), s(x)),
        E::SetOr(a, i, x, y) => {
            format!(
                "{{ array::set({}, {}, {}) or {{ {} }} }}",
                s(a),
                s(i),
                s(x),
                s(y)
            )
        }
        E::SetMatch(a, i, x, n, y, z) => format!(
            "match array::set({}, {}, {}) {{\n Ok({}) -> {}\n Err(Nil) -> {}\n}}",
            s(a),
            s(i),
            s(x),
            named(*n, &s(y)),
            s(y),
            s(z)
        ),
        E::SetHas(a, i, x, y) => format!(
            "match array::set({}, {}, 1) {{\n Ok(_) -> {}\n Err(_) -> {}\n}}",
            s(a),
            s(i),
            s(x),
            s(y)
        ),
        E::Then(r, i, x) => format!(
            "result::then({}, fn(ys) array::set(ys, {}, {}))",
            s(r),
            s(i),
            s(x)
        ),
        E::ResultOr(r, a) => format!("{{ {{ {} }} or {{ {} }} }}", s(r), s(a)),
        E::Slice(a, i, j) => format!("{{ {} }}[{{ {} }}..{{ {} }}]", s(a), s(i), s(j)),
        E::SliceOr(a, i, j, b) => {
            format!(
                "{{ {{ {} }}[{{ {} }}..{{ {} }}] or {{ {} }} }}",
                s(a),
                s(i),
                s(j),
                s(b)
            )
        }
        E::SliceMatch(a, i, j, n, x, y) => format!(
            "match {{ {} }}[{{ {} }}..{{ {} }}] {{\n Err(_) -> {}\n Ok({}) -> {}\n}}",
            s(a),
            s(i),
            s(j),
            s(y),
            named(*n, &s(x)),
            s(x)
        ),
        E::Swap(a, i, j) => format!("array::swap({}, {}, {})", s(a), s(i), s(j)),
        E::MapSet(m, k, v) => {
            let m = match &**m {
                E::Literal(_) => "map::new()".to_string(),
                m => s(m),
            };
            format!("map::set({m}, {}, {})", s(k), s(v))
        }
        E::MapDelete(m, k) => format!("map::delete({}, {})", s(m), s(k)),
        E::LookOr(m, k, x) => format!("{{ map::get({}, {}) or {{ {} }} }}", s(m), s(k), s(x)),
        E::LookMatch(m, k, n, x, y) => format!(
            "match map::get({}, {}) {{\n Some({}) -> {}\n None -> {}\n}}",
            s(m),
            s(k),
            named(*n, &s(x)),
            s(x),
            s(y)
        ),
        E::Look(m, k) => format!("map::get({}, {})", s(m), s(k)),
        E::Record(a, m, n) => format!("R({}, {}, {})", s(a), s(m), s(n)),
        E::Of(r, i) => format!("{{ {} }}.{}", s(r), ["xs", "m", "n"][*i]),
        E::WithArray(r, e) => format!("R(..v{r}, xs: {})", s(e)),
        E::WithMap(r, e) => format!("R(..v{r}, m: {})", s(e)),
        E::Same(e) => format!("same({})", s(e)),
        E::Closure(e) => format!("call(fn() {{ {} }})", s(e)),
    }
}

/// What `e` gives, where each name has the value `env` has for it.
fn value(e: &E, env: &mut BTreeMap<usize, Val>) -> Val {
    macro_rules! int {
        ($e:expr) => {
            value($e, env).int()
        };
    }
    macro_rules! array {
        ($e:expr) => {
            value($e, env).array()
        };
    }
    macro_rules! map {
        ($e:expr) => {
            value($e, env).map()
        };
    }
    macro_rules! named {
        ($n:expr, $v:expr, $e:expr) => {{
            env.insert(*$n, $v);
            value($e, env)
        }};
    }
    let get = |xs: &[i128], i: i128| usize::try_from(i).ok().and_then(|i| xs.get(i)).copied();
    let set = |mut xs: Vec<i128>, i: i128, x: i128| {
        let slot = usize::try_from(i).ok().and_then(|i| xs.get_mut(i));
        slot.map(|slot| *slot = x).map(|()| xs)
    };
    let slice = |xs: &[i128], a: i128, b: i128| {
        let ends = usize::try_from(a).ok().zip(usize::try_from(b).ok());
        ends.filter(|(a, b)| a <= b)
            .and_then(|(a, b)| xs.get(a..b))
            .map(<[_]>::to_vec)
    };
    match e {
        E::Name(n) => env[n].clone(),
        E::Num(n) => Val::Int(*n),
        E::Add(a, b) => Val::Int(int!(a) + int!(b)),
        E::Small(a) => Val::Int(int!(a) % 97),
        E::Length(a) => Val::Int(array!(a).len() as i128),
        E::If(a, b, x, y) => match int!(a) < int!(b) {
            true => value(x, env),
            false => value(y, env),
        },
        E::Literal(all) => Val::Array(all.iter().map(|e| value(e, env).int()).collect()),
        E::Range(a, b) => Val::Array((int!(a)..int!(b)).collect()),
        E::Append(a, x) => {
            let mut xs = array!(a);
            xs.push(int!(x));
            Val::Array(xs)
        }
        E::Prepend(x, a) => {
            let x = int!(x);
            let mut xs = array!(a);
            xs.insert(0, x);
            Val::Array(xs)
        }
        E::GetOr(a, i, x) => {
            let (xs, i) = (array!(a), int!(i));
            get(&xs, i).map_or_else(|| value(x, env), Val::Int)
        }
        E::GetMatch(a, i, n, x, y) => {
            let (xs, i) = (array!(a), int!(i));
            match get(&xs, i) {
                Some(v) => named!(n, Val::Int(v), x),
                None => value(y, env),
            }
        }
        E::GetCases(a, i, n, [one, two, three, four]) => {
            let (xs, i) = (array!(a), int!(i));
            match get(&xs, i) {
                Some(3 | 5) => value(one, env),
                Some(v) if v > 4 => named!(n, Val::Int(v), two),
                Some(v) => named!(n, Val::Int(v), three),
                None => value(four, env),
            }
        }
        E::GetHas(a, i, x, y) => {
            let (xs, i) = (array!(a), int!(i));
            match get(&xs, i) {
                Some(_) => value(x, env),
                None => value(y, env),
            }
        }
        E::Get(a, i) => {
            let (xs, i) = (array!(a), int!(i));
            Val::Option(get(&xs, i))
        }
        E::Unwrap(o, x) => match (value(o, env), int!(x)) {
            (Val::Option(o), x) => Val::Int(o.unwrap_or(x)),
            (other, _) => panic!("{other:?}"),
        },
        E::OptionMatch(o, n, x, y) => match value(o, env) {
            Val::Option(Some(v)) => named!(n, Val::Int(v), x),
            Val::Option(None) => value(y, env),
            other => panic!("{other:?}"),
        },
        E::Put(a, i, x) => {
            let (xs, i, x) = (array!(a), int!(i), int!(x));
            Val::Array(set(xs.clone(), i, x).unwrap_or(xs))
        }
        E::Set(a, i, x) => {
            let (xs, i, x) = (array!(a), int!(i), int!(x));
            Val::Result(set(xs, i, x))
        }
        E::SetOr(a, i, x, y) => {
            let (xs, i, x) = (array!(a), int!(i), int!(x));
            set(xs, i, x).map_or_else(|| value(y, env), Val::Array)
        }
        E::SetMatch(a, i, x, n, y, z) => {
            let (xs, i, x) = (array!(a), int!(i), int!(x));
            match set(xs, i, x) {
                Some(ys) => named!(n, Val::Array(ys), y),
                None => value(z, env),
            }
        }
        E::SetHas(a, i, x, y) => {
            let (xs, i) = (array!(a), int!(i));
            match get(&xs, i) {
                Some(_) => value(x, env),
                None => value(y, env),
            }
        }
        E::Then(r, i, x) => match value(r, env) {
            Val::Result(Some(xs)) => {
                let (i, x) = (int!(i), int!(x));
                Val::Result(set(xs, i, x))
            }
            none => none,
        },
        E::ResultOr(r, a) => match value(r, env) {
            Val::Result(Some(xs)) => Val::Array(xs),
            _ => value(a, env),
        },
        E::Slice(a, i, j) => {
            let (xs, i, j) = (array!(a), int!(i), int!(j));
            Val::Result(slice(&xs, i, j))
        }
        E::SliceOr(a, i, j, b) => {
            let (xs, i, j) = (array!(a), int!(i), int!(j));
            slice(&xs, i, j).map_or_else(|| value(b, env), Val::Array)
        }
        E::SliceMatch(a, i, j, n, x, y) => {
            let (xs, i, j) = (array!(a), int!(i), int!(j));
            match slice(&xs, i, j) {
                Some(ys) => named!(n, Val::Array(ys), x),
                None => value(y, env),
            }
        }
        E::Swap(a, i, j) => {
            let (xs, i, j) = (array!(a), int!(i), int!(j));
            Val::Array(match (get(&xs, i), get(&xs, j)) {
                (Some(x), Some(y)) => {
                    let ys = set(xs, i, y).expect("there");
                    set(ys, j, x).expect("there")
                }
                _ => xs,
            })
        }
        E::MapSet(m, k, v) => {
            let mut m = match &**m {
                E::Literal(_) => BTreeMap::new(),
                m => map!(m),
            };
            let (k, v) = (int!(k), int!(v));
            m.insert(k, v);
            Val::Map(m)
        }
        E::MapDelete(m, k) => {
            let (mut m, k) = (map!(m), int!(k));
            m.remove(&k);
            Val::Map(m)
        }
        E::LookOr(m, k, x) => {
            let (m, k) = (map!(m), int!(k));
            m.get(&k).copied().map_or_else(|| value(x, env), Val::Int)
        }
        E::LookMatch(m, k, n, x, y) => {
            let (m, k) = (map!(m), int!(k));
            match m.get(&k).copied() {
                Some(v) => named!(n, Val::Int(v), x),
                None => value(y, env),
            }
        }
        E::Look(m, k) => {
            let (m, k) = (map!(m), int!(k));
            Val::Option(m.get(&k).copied())
        }
        E::Record(a, m, n) => Val::Record(array!(a), map!(m), int!(n)),
        E::Of(r, i) => match (value(r, env), i) {
            (Val::Record(xs, ..), 0) => Val::Array(xs),
            (Val::Record(_, m, _), 1) => Val::Map(m),
            (Val::Record(_, _, n), _) => Val::Int(n),
            (other, _) => panic!("{other:?}"),
        },
        E::WithArray(r, e) => match (env[r].clone(), array!(e)) {
            (Val::Record(_, m, n), xs) => Val::Record(xs, m, n),
            (other, _) => panic!("{other:?}"),
        },
        E::WithMap(r, e) => match (env[r].clone(), map!(e)) {
            (Val::Record(xs, _, n), m) => Val::Record(xs, m, n),
            (other, _) => panic!("{other:?}"),
        },
        E::Same(e) | E::Closure(e) => value(e, env),
    }
}

/// How a program prints the value `name` of type `ty`.
fn shown(ty: Ty, name: &str) -> String {
    match ty {
        Ty::Int => format!("println({name})"),
        Ty::Array => format!("say({name})"),
        Ty::Map => format!("println(look({name}))"),
        Ty::Option => format!("println({name} or 777)"),
        Ty::Result => format!("say({name} or [888])"),
        Ty::Record => {
            format!("say({name}.xs)\nprintln(look({name}.m))\nprintln({name}.n)")
        }
    }
}

fn say(xs: &[i128]) -> String {
    xs.iter().map(|x| format!("{x},")).collect::<String>() + "\n"
}

/// The sum of what a map has for a few keys, each with a weight of its own.
fn look(m: &BTreeMap<i128, i128>) -> i128 {
    (-1..12).fold(0, |acc, k| acc * 3 + m.get(&k).copied().unwrap_or(-1) % 50)
}

fn printed(v: &Val) -> String {
    match v {
        Val::Int(n) => format!("{n}\n"),
        Val::Array(xs) => say(xs),
        Val::Map(m) => format!("{}\n", look(m)),
        Val::Option(o) => format!("{}\n", o.unwrap_or(777)),
        Val::Result(r) => say(r.as_deref().unwrap_or(&[888])),
        Val::Record(xs, m, n) => format!("{}{}\n{n}\n", say(xs), look(m)),
    }
}

const TYPES: [(Ty, &str); 6] = [
    (Ty::Int, "Int"),
    (Ty::Array, "Array(Int)"),
    (Ty::Map, "map::Map(Int, Int)"),
    (Ty::Option, "Option(Int)"),
    (Ty::Result, "Result(Array(Int), Nil)"),
    (Ty::Record, "R"),
];

const START: &str = "import scarlet/array\nimport scarlet/map\nimport scarlet/option\nimport scarlet/result\n\
type R {\n\txs Array(Int)\n\tm map::Map(Int, Int)\n\tn Int\n}\n\
fn same(x a) a { x }\n\
fn call(f fn() a) a { f() }\n\
fn text(xs Array(Int), i Int, acc String) String {\n\
\tmatch xs[i] {\n\t\tSome(x) -> text(xs, i + 1, acc + '${x},')\n\t\tNone -> acc\n\t}\n}\n\
fn say(xs Array(Int)) Nil { println(text(xs, 0, '')) }\n\
fn looked(m map::Map(Int, Int), k Int, acc Int) Int {\n\
\tif k == 12 { acc } else { looked(m, k + 1, acc * 3 + { map::get(m, k) or { 0 - 1 } } % 50) }\n}\n\
fn look(m map::Map(Int, Int)) Int { looked(m, 0 - 1, 0) }\n";

/// A program and what it prints: a few names one after another in `main`, and a loop
/// that takes one of each type and gives each a new value at every turn.
fn program(seed: u64) -> (String, String) {
    let mut dice = Dice(seed << 8);
    let deep = 1 + dice.below(3);
    let mut make = Maker {
        dice: &mut dice,
        types: Vec::new(),
    };
    let (mut src, mut out) = (START.to_string(), String::new());
    let params: Vec<usize> = TYPES.iter().map(|(ty, _)| make.name(*ty)).collect();
    // What each is at the next turn. Some are as they were.
    let next: Vec<E> = TYPES
        .iter()
        .zip(&params)
        .map(|((ty, _), p)| match make.dice.below(4) {
            0 => E::Name(*p),
            _ => make.expr(*ty, deep, &params),
        })
        .collect();
    let first = make.dice.below(2) == 0;
    let heads = TYPES.iter().zip(&params);
    let heads: Vec<String> = heads.map(|((_, ty), p)| format!("v{p} {ty}")).collect();
    src += &format!(
        "fn turn(n Int, {}) R {{\n\tif n == 0 {{\n",
        heads.join(", ")
    );
    src += "\t\tR(v1, v2, v0 + { v3 or 5 } + array::length(v4 or []) + v5.n)\n\t} else {\n";
    let all: Vec<String> = match first {
        true => next.iter().map(source).collect(),
        false => {
            for (i, e) in next.iter().enumerate() {
                src += &format!("\t\tn{i} = {}\n", source(e));
            }
            (0..next.len()).map(|i| format!("n{i}")).collect()
        }
    };
    let small = |s: &String| format!("{{ {s} }} % 97");
    let args = [vec![small(&all[0])], all[1..].to_vec()].concat();
    src += &format!("\t\tturn(n - 1, {})\n\t}}\n}}\n", args.join(", "));
    src += "pub fn main() {\n";
    let mut scope = Vec::new();
    let mut env = BTreeMap::new();
    for _ in 0..4 + make.dice.below(5) {
        let (ty, _) = make.dice.of(&TYPES);
        let e = make.expr(ty, deep, &scope);
        let n = make.name(ty);
        src += &format!("v{n} = {}\n{}\n", source(&e), shown(ty, &format!("v{n}")));
        let v = value(&e, &mut env);
        out += &printed(&v);
        env.insert(n, v);
        scope.push(n);
    }
    let starts: Vec<E> = TYPES
        .iter()
        .map(|(ty, _)| make.expr(*ty, 1, &scope))
        .collect();
    let turns = make.dice.below(5);
    let args: Vec<String> = starts.iter().map(source).collect();
    src += &format!(
        "end = turn({turns}, {})\n{}\n}}\n",
        args.join(", "),
        shown(Ty::Record, "end")
    );
    let mut now: Vec<Val> = starts.iter().map(|e| value(e, &mut env)).collect();
    for _ in 0..turns {
        let mut env: BTreeMap<usize, Val> = params.iter().copied().zip(now).collect();
        now = next.iter().map(|e| value(e, &mut env)).collect();
        now[0] = Val::Int(now[0].int() % 97);
    }
    let sum = match (&now[0], &now[3], &now[4], &now[5]) {
        (Val::Int(n), Val::Option(o), Val::Result(r), Val::Record(.., of)) => {
            n + o.unwrap_or(5) + r.as_ref().map_or(0, Vec::len) as i128 + of
        }
        other => panic!("{other:?}"),
    };
    let end = Val::Record(now[1].clone().array(), now[2].clone().map(), sum);
    out += &printed(&end);
    (src, out)
}

fn run(src: &str, audit: Audit) -> String {
    let result = scarlet_core::compile::compile_text(src, None);
    assert!(result.success(), "{:?}\n{src}", result.diagnostics());
    let program = result.into_runnable().expect("a clean compile is runnable");
    let mut host = OsHost::new(Vec::new(), Vec::new(), Vec::new());
    let ran = scarlet_vm::run(&program, &mut host, audit);
    assert_eq!(ran, Ok(()), "{src}");
    String::from_utf8(std::mem::take(host.out())).expect("println writes UTF-8")
}

/// Whatever is read, cut, written and looked up, and whatever is done with what that
/// gives, a program prints what the model says, and every count of it is right.
#[test]
fn a_program_of_probes_does_what_a_model_says() {
    let from = std::env::var("PROBES_FROM")
        .ok()
        .and_then(|n| n.parse().ok());
    let cases = std::env::var("PROBES").ok().and_then(|n| n.parse().ok());
    let (from, cases) = (from.unwrap_or(0u64), cases.unwrap_or(600u64));
    for seed in from..from + cases {
        let (src, want) = program(seed);
        let audit = match seed % 8 {
            0 => Audit::EachInstruction,
            _ => Audit::AtEnd,
        };
        assert_eq!(run(&src, audit), want, "seed {seed}\n{src}");
    }
}
