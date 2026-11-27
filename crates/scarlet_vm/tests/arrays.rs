//! Programs made at random of array patterns and spreads, against a model of what each
//! does. A guard says what it was given as it runs, so what a match asks and in what
//! order is a part of what is printed. Each program is run with its heap checked, and
//! some with the heap checked before every instruction.

use std::fmt::Write as _;

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

    fn one_in(&mut self, n: usize) -> bool {
        self.below(n) == 0
    }
}

/// What the elements of an array are.
#[derive(Debug, Clone, PartialEq)]
enum Ty {
    Int,
    Text,
    Pair(Box<Ty>),
    Array(Box<Ty>),
}

#[derive(Debug, Clone, PartialEq)]
enum Val {
    Int(i128),
    Text(String),
    Pair(i128, Box<Val>),
    Array(Vec<Val>),
}

impl Ty {
    fn source(&self) -> String {
        match self {
            Ty::Int => "Int".to_string(),
            Ty::Text => "String".to_string(),
            Ty::Pair(of) => format!("(Int, {})", of.source()),
            Ty::Array(of) => format!("Array({})", of.source()),
        }
    }

    /// The name of the function that makes a number of a value of this type.
    fn number(&self) -> String {
        match self {
            Ty::Int => "ni".to_string(),
            Ty::Text => "nt".to_string(),
            Ty::Pair(of) => format!("p{}", of.number()),
            Ty::Array(of) => format!("a{}", of.number()),
        }
    }

    /// That function, and those it calls.
    fn numbers(&self, out: &mut String) {
        let (name, ty) = (self.number(), self.source());
        if out.contains(&format!("fn {name}(")) {
            return;
        }
        let body = match self {
            Ty::Int => "v".to_string(),
            Ty::Text => "string::length(v)".to_string(),
            Ty::Pair(of) => {
                of.numbers(out);
                format!("{{ v.0 * 7 + {}(v.1) }} % 9973", of.number())
            }
            Ty::Array(of) => {
                of.numbers(out);
                let f = of.number();
                format!("array::fold(v, 3, fn(acc, x) {{ acc * 31 + {f}(x) }} % 9973)")
            }
        };
        writeln!(out, "fn {name}(v {ty}) Int {{ {body} }}").expect("a string");
    }
}

impl Val {
    fn number(&self) -> i128 {
        match self {
            Val::Int(n) => *n,
            Val::Text(s) => s.len() as i128,
            Val::Pair(n, of) => (n * 7 + of.number()) % 9973,
            Val::Array(all) => all.iter().fold(3, |acc, x| (acc * 31 + x.number()) % 9973),
        }
    }

    /// What `beside` in [`START`] makes of two arrays: whether they are equal, whether
    /// either is empty, how they are as a map's keys, and what a read and a write of the
    /// first give.
    fn beside(&self, other: &Val) -> i128 {
        let (Val::Array(a), Val::Array(b)) = (self, other) else {
            panic!("no arrays")
        };
        let same = a == b;
        let is = |yes: bool| i128::from(yes);
        let keys = if same { 1 } else { 2 };
        let read = if a.is_empty() { 7 } else { 5 };
        let all = [
            is(same),
            is(a.is_empty()),
            is(b.is_empty()),
            keys,
            read,
            (a.len() + b.len()) as i128,
        ];
        all.into_iter().fold(0, |acc, n| acc * 11 + n) % 9973
    }

    fn source(&self) -> String {
        match self {
            Val::Int(n) => n.to_string(),
            Val::Text(s) => format!("'{s}'"),
            Val::Pair(n, of) => format!("({n}, {})", of.source()),
            Val::Array(all) => {
                let all: Vec<String> = all.iter().map(Val::source).collect();
                format!("[{}]", all.join(", "))
            }
        }
    }
}

#[derive(Debug, Clone)]
enum Pat {
    Wild,
    Name(usize),
    /// A name that nothing reads.
    Unread,
    Int(i128),
    Text(String),
    Pair(Box<Pat>, Box<Pat>),
    Array(Vec<Pat>, Rest),
    Or(Vec<Pat>),
}

#[derive(Debug, Clone)]
enum Rest {
    None,
    Discard,
    Unread,
    Name(usize),
}

impl Pat {
    fn source(&self) -> String {
        match self {
            Pat::Wild => "_".to_string(),
            Pat::Name(n) => format!("v{n}"),
            Pat::Unread => "_v".to_string(),
            Pat::Int(n) => n.to_string(),
            Pat::Text(s) => format!("'{s}'"),
            Pat::Pair(a, b) => format!("({}, {})", a.source(), b.source()),
            Pat::Array(all, rest) => {
                let mut all: Vec<String> = all.iter().map(Pat::source).collect();
                match rest {
                    Rest::None => {}
                    Rest::Discard => all.push("..".to_string()),
                    Rest::Unread => all.push(".._t".to_string()),
                    Rest::Name(n) => all.push(format!("..v{n}")),
                }
                format!("[{}]", all.join(", "))
            }
            Pat::Or(alts) => {
                let alts: Vec<String> = alts.iter().map(Pat::source).collect();
                alts.join(" | ")
            }
        }
    }

    /// Whether `v` fits, with what each name is then. Or-patterns bind nothing.
    fn fits(&self, v: &Val, env: &mut Vec<(usize, Val)>) -> bool {
        match (self, v) {
            (Pat::Wild | Pat::Unread, _) => true,
            (Pat::Name(n), v) => {
                env.push((*n, v.clone()));
                true
            }
            (Pat::Int(a), Val::Int(b)) => a == b,
            (Pat::Text(a), Val::Text(b)) => a == b,
            (Pat::Pair(a, b), Val::Pair(n, of)) => a.fits(&Val::Int(*n), env) && b.fits(of, env),
            (Pat::Array(pats, rest), Val::Array(all)) => {
                let exact = matches!(rest, Rest::None);
                if all.len() < pats.len() || (exact && all.len() > pats.len()) {
                    return false;
                }
                if let Rest::Name(n) = rest {
                    env.push((*n, Val::Array(all[pats.len()..].to_vec())));
                }
                pats.iter().zip(all).all(|(p, v)| p.fits(v, env))
            }
            (Pat::Or(alts), v) => alts.iter().any(|p| p.fits(v, &mut Vec::new())),
            (p, v) => panic!("{v:?} against {p:?}"),
        }
    }
}

/// What an arm's body gives, beside the numbers of its names.
#[derive(Debug, Clone)]
enum Body {
    Plain,
    /// The number of the whole value, which is read under its own name.
    Whole,
    /// The number of an array that a spread of these makes.
    Spread(Vec<Piece>),
    /// The function again, on a tail, and a constant.
    Again(usize),
    /// A closure that is called and reads the names.
    Closure,
    /// What two arrays of one type say of each other ([`Val::beside`]).
    Beside(usize, usize),
}

#[derive(Debug, Clone)]
enum Piece {
    Element(usize),
    Literal(Val),
    /// The number of an array, as an element of an array of Ints.
    Number(usize),
    Spread(usize),
}

#[derive(Debug, Clone)]
struct Arm {
    pat: Pat,
    names: Vec<(usize, Ty)>,
    /// The names that the guard is given, when there is one.
    guard: Option<Vec<usize>>,
    body: Body,
}

struct Maker<'d> {
    dice: &'d mut Dice,
    names: usize,
}

impl Maker<'_> {
    fn value(&mut self, ty: &Ty, long: bool) -> Val {
        match ty {
            Ty::Int => Val::Int(self.dice.below(4) as i128),
            Ty::Text => Val::Text(["", "a", "bb", "a long one"][self.dice.below(4)].to_string()),
            Ty::Pair(of) => {
                let n = self.dice.below(3) as i128;
                Val::Pair(n, Box::new(self.value(of, false)))
            }
            Ty::Array(of) => {
                let n = match self.dice.below(8) {
                    0 if long => [31, 32, 33, 64, 65, 70][self.dice.below(6)],
                    1 if long => self.dice.below(12),
                    2 => 0,
                    _ => self.dice.below(5),
                };
                Val::Array((0..n).map(|_| self.value(of, false)).collect())
            }
        }
    }

    fn name(&mut self, ty: &Ty, names: &mut Vec<(usize, Ty)>) -> usize {
        self.names += 1;
        names.push((self.names, ty.clone()));
        self.names
    }

    fn pat(&mut self, ty: &Ty, deep: usize, names: &mut Vec<(usize, Ty)>) -> Pat {
        match self.dice.below(8) {
            0 => return Pat::Wild,
            1 => return Pat::Unread,
            2 | 3 => return Pat::Name(self.name(ty, names)),
            4 if deep > 0 && self.dice.one_in(2) => {
                let alts = 2 + self.dice.below(2);
                return Pat::Or((0..alts).map(|_| self.closed(ty, deep - 1)).collect());
            }
            _ => {}
        }
        match ty {
            Ty::Int => Pat::Int(self.dice.below(4) as i128),
            Ty::Text => match self.value(ty, false) {
                Val::Text(s) => Pat::Text(s),
                other => panic!("{other:?}"),
            },
            Ty::Pair(of) if deep > 0 => {
                let a = self.pat(&Ty::Int, deep - 1, names);
                Pat::Pair(Box::new(a), Box::new(self.pat(of, deep - 1, names)))
            }
            Ty::Array(of) if deep > 0 => {
                let n = match self.dice.below(10) {
                    0 => 5 + self.dice.below(8),
                    _ => self.dice.below(4),
                };
                let all = (0..n).map(|_| self.pat(of, deep - 1, names)).collect();
                let rest = match self.dice.below(5) {
                    0 | 1 => Rest::None,
                    2 => Rest::Discard,
                    3 if self.dice.one_in(3) => Rest::Unread,
                    _ => Rest::Name(self.name(ty, names)),
                };
                Pat::Array(all, rest)
            }
            Ty::Pair(_) | Ty::Array(_) => Pat::Wild,
        }
    }

    /// A pattern that binds nothing.
    fn closed(&mut self, ty: &Ty, deep: usize) -> Pat {
        fn close(p: Pat) -> Pat {
            match p {
                Pat::Name(_) | Pat::Unread => Pat::Wild,
                Pat::Pair(a, b) => Pat::Pair(Box::new(close(*a)), Box::new(close(*b))),
                Pat::Array(all, rest) => {
                    let rest = match rest {
                        Rest::None => Rest::None,
                        Rest::Discard | Rest::Unread | Rest::Name(_) => Rest::Discard,
                    };
                    Pat::Array(all.into_iter().map(close).collect(), rest)
                }
                // One in another is one.
                Pat::Or(mut alts) => alts.swap_remove(0),
                p @ (Pat::Wild | Pat::Int(_) | Pat::Text(_)) => p,
            }
        }
        close(close(self.pat(ty, deep, &mut Vec::new())))
    }

    fn arm(&mut self, pat: Pat, names: Vec<(usize, Ty)>) -> Arm {
        let guard = (self.dice.below(5) < 2).then(|| {
            let all = names.iter().map(|(n, _)| *n);
            all.filter(|_| self.dice.one_in(2)).collect()
        });
        let arrays: Vec<(usize, Ty)> = names
            .iter()
            .filter(|(_, t)| matches!(t, Ty::Array(_)))
            .cloned()
            .collect();
        // A tail that is shorter than the whole.
        let own = match &pat {
            Pat::Array(items, Rest::Name(n)) if !items.is_empty() => Some(*n),
            _ => None,
        };
        let body = match self.dice.below(10) {
            0 => Body::Whole,
            1 => Body::Closure,
            8 | 9 if !arrays.is_empty() => {
                let (n, ty) = arrays[self.dice.below(arrays.len())].clone();
                let all: Vec<_> = arrays.iter().filter(|(_, t)| *t == ty).collect();
                Body::Beside(n, all[self.dice.below(all.len())].0)
            }
            2 | 3 => match own {
                Some(n) => Body::Again(n),
                None => Body::Plain,
            },
            4..=6 => match arrays.is_empty() {
                true => Body::Plain,
                false => {
                    let (n, t) = arrays[self.dice.below(arrays.len())].clone();
                    Body::Spread(self.spread(n, &t, &names))
                }
            },
            _ => Body::Plain,
        };
        Arm {
            pat,
            names,
            guard,
            body,
        }
    }

    /// A spread of the array `xs` and other things beside it.
    fn spread(&mut self, xs: usize, ty: &Ty, names: &[(usize, Ty)]) -> Vec<Piece> {
        let Ty::Array(of) = ty else {
            panic!("no array")
        };
        let mut pieces = vec![Piece::Spread(xs)];
        for _ in 0..1 + self.dice.below(4) {
            let same: Vec<usize> = names
                .iter()
                .filter(|(_, t)| *t == **of)
                .map(|(n, _)| *n)
                .collect();
            let arrays: Vec<(usize, Ty)> = names
                .iter()
                .filter(|(_, t)| matches!(t, Ty::Array(_)))
                .cloned()
                .collect();
            let piece = match self.dice.below(6) {
                0 | 1 if !same.is_empty() => Piece::Element(same[self.dice.below(same.len())]),
                2 if **of == Ty::Int => Piece::Number(arrays[self.dice.below(arrays.len())].0),
                3 => {
                    let all: Vec<_> = arrays.iter().filter(|(_, t)| t == ty).collect();
                    Piece::Spread(all[self.dice.below(all.len())].0)
                }
                _ => Piece::Literal(self.value(of, false)),
            };
            let at = self.dice.below(pieces.len() + 1);
            pieces.insert(at, piece);
        }
        pieces
    }
}

/// A match of `arms` on an array, as a function, and the model of it.
struct Func {
    ty: Ty,
    arms: Vec<Arm>,
}

impl Func {
    fn source(&self) -> String {
        let mut out = format!(
            "fn f(xs {}, k Int) Int {{\n\tmatch xs {{\n",
            self.ty.source()
        );
        for (i, arm) in self.arms.iter().enumerate() {
            write!(out, "\t\t{}", arm.pat.source()).expect("a string");
            let number = |n: &usize| {
                let ty = arm.names.iter().find(|(m, _)| m == n);
                format!("{}(v{n})", ty.expect("a name").1.number())
            };
            if let Some(guard) = &arm.guard {
                let all: Vec<String> = guard.iter().map(number).collect();
                let all = match all.is_empty() {
                    true => "k".to_string(),
                    false => all.join(" * 3 + "),
                };
                write!(out, " if ok({i}, {all})").expect("a string");
            }
            let mut all: Vec<String> = arm.names.iter().map(|(n, _)| number(n)).collect();
            all.insert(0, format!("{i}"));
            let names = all.join(" * 5 + ");
            let body = match &arm.body {
                Body::Plain => names,
                Body::Whole => format!("{names} * 5 + {}(xs)", self.ty.number()),
                Body::Again(n) => format!("again({names}, f(v{n}, k + 1))"),
                Body::Closure => format!("call(fn() {{ {names} }})"),
                Body::Beside(a, b) => format!("{names} * 5 + beside(v{a}, v{b})"),
                Body::Spread(pieces) => {
                    let pieces: Vec<String> = pieces
                        .iter()
                        .map(|p| match p {
                            Piece::Element(n) => format!("v{n}"),
                            Piece::Literal(v) => v.source(),
                            Piece::Number(n) => number(n),
                            Piece::Spread(n) => format!("..v{n}"),
                        })
                        .collect();
                    let first = pieces_type(arm);
                    let pieces = pieces.join(", ");
                    format!("{names} * 5 + {}([{pieces}])", first.number())
                }
            };
            writeln!(out, " -> small({body})").expect("a string");
        }
        out + "\t\t_ -> k * 0 - 1\n\t}\n}\n"
    }

    fn run(&self, v: &Val, k: i128, out: &mut String) -> i128 {
        for (i, arm) in self.arms.iter().enumerate() {
            let mut env = Vec::new();
            if !arm.pat.fits(v, &mut env) {
                continue;
            }
            let get = |n: &usize| &env.iter().find(|(m, _)| m == n).expect("bound").1;
            let joined = |all: Vec<i128>, times: i128| {
                // `a * t + b * t + c`, as it is written.
                let last = all.len() - 1;
                let each = all.iter().enumerate();
                each.map(|(i, n)| if i < last { n * times } else { *n })
                    .sum::<i128>()
            };
            if let Some(guard) = &arm.guard {
                let given = match guard.is_empty() {
                    true => k,
                    false => joined(guard.iter().map(|n| get(n).number()).collect(), 3),
                };
                writeln!(out, "{i}:{given}").expect("a string");
                if (given + i as i128) % 2 != 0 {
                    continue;
                }
            }
            let mut all = vec![i as i128];
            all.extend(arm.names.iter().map(|(n, _)| get(n).number()));
            let got = match &arm.body {
                Body::Plain | Body::Closure => joined(all, 5),
                Body::Whole => {
                    all.push(v.number());
                    joined(all, 5)
                }
                Body::Again(n) => {
                    let names = joined(all, 5);
                    let then = self.run(get(n), k + 1, out);
                    (names * 13 + then) % 9973
                }
                Body::Beside(a, b) => {
                    all.push(get(a).beside(get(b)));
                    joined(all, 5)
                }
                Body::Spread(pieces) => {
                    let mut made = Vec::new();
                    for piece in pieces {
                        match piece {
                            Piece::Element(n) => made.push(get(n).clone()),
                            Piece::Literal(v) => made.push(v.clone()),
                            Piece::Number(n) => made.push(Val::Int(get(n).number())),
                            Piece::Spread(n) => match get(n) {
                                Val::Array(more) => made.extend(more.clone()),
                                other => panic!("{other:?}"),
                            },
                        }
                    }
                    all.push(Val::Array(made).number());
                    joined(all, 5)
                }
            };
            return got % 9973;
        }
        -1
    }
}

/// The type of the array that the body of `arm` makes.
fn pieces_type(arm: &Arm) -> Ty {
    let Body::Spread(pieces) = &arm.body else {
        panic!("no spread")
    };
    let first = pieces.iter().find_map(|p| match p {
        Piece::Spread(n) => arm.names.iter().find(|(m, _)| m == n),
        Piece::Element(_) | Piece::Literal(_) | Piece::Number(_) => None,
    });
    first.expect("it has one").1.clone()
}

const START: &str = "import scarlet/array\nimport scarlet/map\nimport scarlet/string\n\
fn ok(i Int, v Int) Bool {\n\tprintln('${i}:${v}')\n\t{ v + i } % 2 == 0\n}\n\
fn call(f fn() Int) Int { f() }\n\
fn again(a Int, b Int) Int { { a * 13 + b } % 9973 }\n\
fn small(n Int) Int { n % 9973 }\n\
fn same(x a) a { x }\n\
fn is(b Bool) Int { if b { 1 } else { 0 } }\n\
fn rest(xs Array(a)) Array(a) {\n\tmatch xs {\n\t\t[_, ..t] -> t\n\t\t[] -> xs\n\t}\n}\n\
fn read(xs Array(a)) Int {\n\tmatch xs[0] {\n\t\tSome(x) -> if array::put(xs, 0, x) == xs { 5 } else { 6 }\n\t\tNone -> 7\n\t}\n}\n\
fn beside(a Array(a), b Array(a)) Int {\n\
\tkeys = map::size(map::set(map::set(map::new(), a, 1), b, 2))\n\
\tn = is(a == b) * 11 + is(a == [])\n\
\t{ { { { n * 11 + is([] == b) } * 11 + keys } * 11 + read(a) } * 11 + array::length([..a, ..b]) } % 9973\n\
}\n";

/// The array `v`, made in one of several ways.
fn made(dice: &mut Dice, v: &Val, ty: &Ty) -> String {
    let Val::Array(all) = v else {
        panic!("no array")
    };
    let of = |all: &[Val]| Val::Array(all.to_vec()).source();
    let cut = dice.below(all.len() + 1);
    let (a, b) = all.split_at(cut);
    let Ty::Array(elem) = ty else {
        panic!("no array")
    };
    let more = match &**elem {
        Ty::Int => Some("9".to_string()),
        Ty::Text => Some("'z'".to_string()),
        Ty::Pair(_) | Ty::Array(_) => None,
    };
    let none = || match (&more, **elem == Ty::Int) {
        (Some(more), true) if all.len().is_multiple_of(2) => format!("rest(rest([{more}]))"),
        (_, true) => "4..4".to_string(),
        (Some(more), false) => format!("rest([{more}])"),
        (None, false) => "array::filter([], fn(_) True)".to_string(),
    };
    match dice.below(12) {
        0 => of(all),
        7 => format!("[..{}, ..{}]", none(), of(all)),
        8 => format!("[..{}, ..{}]", of(all), none()),
        9 if all.is_empty() => none(),
        9 => {
            let items: Vec<String> = all.iter().map(Val::source).collect();
            format!("[{}, ..{}]", items.join(", "), none())
        }
        10 => format!("array::filter({}, fn(_) True)", of(all)),
        11 if more.is_some() => {
            let items: Vec<String> = all.iter().map(Val::source).collect();
            let inner = [more.into_iter().collect(), items].concat().join(", ");
            format!("rest([{inner}])")
        }
        1 => format!("[..{}, ..{}]", of(a), of(b)),
        2 if !b.is_empty() => {
            let items: Vec<String> = b.iter().map(Val::source).collect();
            format!("[..same({}), {}]", of(a), items.join(", "))
        }
        3 if !a.is_empty() => {
            let items: Vec<String> = a.iter().map(Val::source).collect();
            format!("[{}, ..same({})]", items.join(", "), of(b))
        }
        4 => {
            let Some(more) = more else {
                return of(all);
            };
            let items: Vec<String> = all.iter().map(Val::source).collect();
            let inner = [vec![more.clone()], items, vec![more]].concat().join(", ");
            format!("{{ [{inner}][1..{}] or [] }}", all.len() + 1)
        }
        5 => format!("({}, 1).0", of(all)),
        _ => format!("same({})", of(all)),
    }
}

/// A program and what it prints.
fn program(seed: u64) -> (String, String) {
    let mut dice = Dice(seed << 8);
    let elems = [
        Ty::Int,
        Ty::Int,
        Ty::Text,
        Ty::Pair(Box::new(Ty::Int)),
        Ty::Pair(Box::new(Ty::Array(Box::new(Ty::Int)))),
        Ty::Array(Box::new(Ty::Int)),
        Ty::Array(Box::new(Ty::Text)),
    ];
    let (mut numbers, mut out, mut main) = (String::new(), String::new(), String::new());
    let ty = Ty::Array(Box::new(elems[dice.below(elems.len())].clone()));
    ty.numbers(&mut numbers);
    let mut make = Maker {
        dice: &mut dice,
        names: 0,
    };
    let mut arms = Vec::new();
    for _ in 0..1 + make.dice.below(5) {
        let mut names = Vec::new();
        let mut pat = Pat::Wild;
        while !matches!(pat, Pat::Array(..) | Pat::Or(..)) {
            names.clear();
            let deep = 1 + make.dice.below(3);
            pat = make.pat(&ty, deep, &mut names);
        }
        arms.push(make.arm(pat, names));
    }
    let f = Func { ty, arms };
    for i in 0..8 {
        let mut make = Maker {
            dice: &mut dice,
            names: 0,
        };
        let v = make.value(&f.ty, true);
        let src = made(&mut dice, &v, &f.ty);
        let got = f.run(&v, 0, &mut out);
        writeln!(out, "= {got}").expect("a string");
        if dice.one_in(2) {
            // It is held while it is matched, and is what it was after that.
            let number = f.ty.number();
            writeln!(main, "x{i} = {src}\nprintln('= ${{f(x{i}, 0)}}')").expect("a string");
            writeln!(main, "println('${{{number}(x{i})}}')").expect("a string");
            writeln!(out, "{}", v.number()).expect("a string");
        } else {
            writeln!(main, "println('= ${{f({src}, 0)}}')").expect("a string");
        }
    }
    let f = f.source();
    let src = format!("{START}{numbers}{f}pub fn main() {{\n{main}}}\n");
    (src, out)
}

/// What `src` prints, or `None` when the checker refuses it, as it does when no value
/// comes to one of its arms.
fn run(src: &str, audit: Audit) -> Option<String> {
    let result = scarlet_core::compile::compile_text(src, None);
    if !result.success() {
        let why = format!("{:?}", result.diagnostics());
        let expected = ["never", "reach", "redundant"];
        assert!(expected.iter().any(|w| why.contains(w)), "{why}\n{src}");
        return None;
    }
    let program = result.into_runnable().expect("a clean compile is runnable");
    let mut host = OsHost::new(Vec::new(), Vec::new(), Vec::new());
    let ran = scarlet_vm::run(&program, &mut host, audit);
    assert_eq!(ran, Ok(()), "{src}");
    let out = std::mem::take(host.out());
    Some(String::from_utf8(out).expect("println writes UTF-8"))
}

/// Whatever array patterns there are, with whatever guards, and whatever is pushed and
/// spread, a program prints what the model says, and every count of it is right.
#[test]
fn a_program_of_array_patterns_does_what_a_model_says() {
    let from = std::env::var("ARRAYS_FROM")
        .ok()
        .and_then(|n| n.parse().ok());
    let cases = std::env::var("ARRAYS").ok().and_then(|n| n.parse().ok());
    let (from, cases) = (from.unwrap_or(0u64), cases.unwrap_or(300u64));
    let mut ran = 0;
    for seed in from..from + cases {
        let (src, want) = program(seed);
        let audit = match seed % 8 {
            0 => Audit::EachInstruction,
            _ => Audit::AtEnd,
        };
        if let Some(got) = run(&src, audit) {
            assert_eq!(got, want, "seed {seed}\n{src}");
            ran += 1;
        }
    }
    assert!(ran * 4 > cases, "{ran} of {cases}");
}
