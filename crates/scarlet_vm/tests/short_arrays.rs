//! Programs made at random of what is done to arrays of about 1, 4, 8, 16 and 32 elements,
//! the lengths at which an array's cell is full and those past them, against a plain list
//! that the same is done to. Some of the arrays are kept and read again at the end. Each
//! program is run with its heap checked, and some with the heap checked before every
//! instruction.

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

    /// A length at which a cell is full, or one beside it.
    fn length(&mut self) -> usize {
        [1, 2, 3, 4, 5, 7, 8, 9, 15, 16, 17, 30, 31, 32, 33, 34, 40][self.below(17)]
    }
}

/// What the elements are. Each is made out of a number.
#[derive(Clone, Copy, PartialEq)]
enum Elem {
    Int,
    Text,
    Pair,
}

impl Elem {
    fn ty(self) -> &'static str {
        match self {
            Elem::Int => "Int",
            Elem::Text => "String",
            Elem::Pair => "(Int, String)",
        }
    }

    /// The element that stands for `n`, as it is written.
    fn source(self, n: i64) -> String {
        match self {
            Elem::Int => n.to_string(),
            Elem::Text => format!("\"s{n}\""),
            Elem::Pair => format!("({n}, \"s{n}\")"),
        }
    }

    fn shown(self, n: i64) -> String {
        match self {
            Elem::Int => n.to_string(),
            Elem::Text => format!("s{n}"),
            Elem::Pair => format!("({n}, s{n})"),
        }
    }

    /// A function that makes the element of a number.
    fn maker(self) -> &'static str {
        match self {
            Elem::Int => "fn el(n Int) Int { n }",
            Elem::Text => "fn el(n Int) String { \"s${n}\" }",
            Elem::Pair => "fn el(n Int) (Int, String) { (n, \"s${n}\") }",
        }
    }
}

/// The functions that a program calls, with `T` for what the elements are. A walk that does
/// not come to an end goes no further than an array here is long.
const HELPERS: &str = "\
fn show(n Int, xs Array(T), out String) String {\n\tif n == 0 { out } else { match xs {\n\t\t[] -> out\n\t\t[x, ..t] -> show(n - 1, t, \"${out}${x};\")\n\t} }\n}\n\
fn at(xs Array(T), i Int, out String) String {\n\tif i == 300 { out } else { match xs[i] {\n\t\tNone -> out\n\t\tSome(x) -> at(xs, i + 1, \"${out}${x};\")\n\t} }\n}\n\
fn back(n Int, from Int, xs Array(T)) Array(T) {\n\tif n == 0 { xs } else { back(n - 1, from + 1, [..xs, el(from)]) }\n}\n\
fn front(n Int, from Int, xs Array(T)) Array(T) {\n\tif n == 0 { xs } else { front(n - 1, from + 1, [el(from), ..xs]) }\n}\n\
fn drop(n Int, xs Array(T)) Array(T) {\n\tif n == 0 { xs } else { match xs {\n\t\t[_, ..t] -> drop(n - 1, t)\n\t\t[] -> xs\n\t} }\n}\n\
fn two(n Int, xs Array(T)) Array(T) {\n\tif n == 0 { xs } else { match xs {\n\t\t[a, b, ..t] -> two(n - 1, [..t, a, b])\n\t\t_ -> xs\n\t} }\n}\n\
fn queue(n Int, xs Array(T)) Array(T) {\n\tif n == 0 { xs } else { match xs {\n\t\t[h, ..t] -> queue(n - 1, [..t, h])\n\t\t[] -> xs\n\t} }\n}\n\
fn stack(n Int, xs Array(T)) Array(T) {\n\tif n == 0 { xs } else { match xs {\n\t\t[h, ..t] -> stack(n - 1, [h, ..t])\n\t\t[] -> xs\n\t} }\n}\n\
fn puts(i Int, xs Array(T)) Array(T) {\n\tif i == 300 { xs } else { match array::set(xs, i, el(900 + i)) {\n\t\tOk(ys) -> puts(i + 1, ys)\n\t\tErr(_) -> xs\n\t} }\n}\n\
fn third(xs Array(T)) Array(T) {\n\tmatch xs {\n\t\t[a, _, b, ..t] -> [b, a, ..t]\n\t\t[a, b] -> [b, a]\n\t\t_ -> xs\n\t}\n}\n\
fn same(xs Array(T)) Array(T) { xs }\n";

/// One array as it is now, by its name, and what is in it.
#[derive(Clone)]
struct Var {
    name: String,
    items: Vec<i64>,
}

struct Maker {
    dice: Dice,
    elem: Elem,
    main: String,
    /// How many names there are, and the next element's number.
    names: usize,
    next: i64,
    vars: Vec<Var>,
}

impl Maker {
    fn fresh(&mut self, n: usize) -> Vec<i64> {
        let from = self.next;
        self.next += n as i64;
        (from..self.next).collect()
    }

    fn literal(&self, items: &[i64]) -> String {
        let all: Vec<String> = items.iter().map(|n| self.elem.source(*n)).collect();
        all.join(", ")
    }

    /// A new array of a length near one at which a cell is full, made in some way.
    fn start(&mut self) -> (String, Vec<i64>) {
        let n = self.dice.length();
        match self.dice.below(6) {
            0 | 1 => {
                let items = self.fresh(n);
                (format!("[{}]", self.literal(&items)), items)
            }
            2 => {
                let items = self.fresh(n);
                (format!("back({n}, {}, [])", items[0]), items)
            }
            3 => {
                let items = self.fresh(n);
                let src = format!("front({n}, {}, [])", items[0]);
                (src, items.into_iter().rev().collect())
            }
            4 => {
                let items = self.fresh(n);
                let (from, to) = (items[0], items[0] + n as i64);
                (format!("array::map({from}..{to}, el)"), items)
            }
            _ => {
                // Cut from a longer one.
                let k = 1 + self.dice.below(34);
                let items = self.fresh(n + k);
                let src = format!("drop({k}, [{}])", self.literal(&items));
                (src, items[k..].to_vec())
            }
        }
    }

    /// What some operation makes of `v`.
    fn step(&mut self, v: &Var) -> (String, Vec<i64>) {
        let xs = match self.dice.one_in(6) {
            true => format!("same({})", v.name),
            false => v.name.clone(),
        };
        let len = v.items.len();
        let mut items = v.items.clone();
        let few = 1 + self.dice.below(3);
        match self.dice.below(20) {
            0 => {
                let new = self.fresh(few);
                let src = format!("[..{xs}, {}]", self.literal(&new));
                items.extend(new);
                (src, items)
            }
            1 => {
                let new = self.fresh(few);
                let src = format!("[{}, ..{xs}]", self.literal(&new));
                (src, [new, items].concat())
            }
            2 => {
                let n = self.dice.length();
                let new = self.fresh(n);
                let src = format!("back({n}, {}, {xs})", new[0]);
                items.extend(new);
                (src, items)
            }
            3 => {
                let n = self.dice.length();
                let new = self.fresh(n);
                let src = format!("front({n}, {}, {xs})", new[0]);
                let new: Vec<i64> = new.into_iter().rev().collect();
                (src, [new, items].concat())
            }
            4 | 5 => {
                let other = self.other();
                let first = self.dice.one_in(2);
                let (a, b) = match first {
                    true => (&other.name, &xs),
                    false => (&xs, &other.name),
                };
                let src = format!("[..{a}, ..{b}]");
                let all = match first {
                    true => [other.items, items].concat(),
                    false => [items, other.items].concat(),
                };
                (src, all)
            }
            6 => {
                let n = self.dice.length();
                let new = self.fresh(n);
                let (from, to) = (new[0], new[0] + n as i64);
                let range = match self.elem {
                    Elem::Int => format!("{from}..{to}"),
                    Elem::Text | Elem::Pair => format!("array::map({from}..{to}, el)"),
                };
                items.extend(new);
                (format!("[..{xs}, ..{range}]"), items)
            }
            7 => {
                let n = self.dice.below(len + 2);
                let src = format!("drop({n}, {xs})");
                (src, items.split_off(n.min(len)))
            }
            8 | 9 => {
                let from = self.dice.below(len + 1);
                let to = from + self.dice.below(len - from + 1);
                let src = format!("{{ {xs}[{from}..{to}] or [] }}");
                (src, items[from..to].to_vec())
            }
            10 => {
                let to = len + 1;
                (format!("{{ {xs}[1..{to}] or {xs} }}"), items)
            }
            11 | 12 => {
                let i = self.dice.below(len + 2);
                let new = self.fresh(1)[0];
                let src = format!("array::put({xs}, {i}, {})", self.elem.source(new));
                if let Some(slot) = items.get_mut(i) {
                    *slot = new;
                }
                (src, items)
            }
            13 => {
                for (i, slot) in items.iter_mut().enumerate() {
                    *slot = 900 + i as i64;
                }
                (format!("puts(0, {xs})"), items)
            }
            14 => {
                let n = 1 + self.dice.below(70);
                if len > 0 {
                    items.rotate_left(n % len);
                }
                (format!("queue({n}, {xs})"), items)
            }
            15 => {
                let n = 1 + self.dice.below(40);
                (format!("stack({n}, {xs})"), items)
            }
            16 => {
                let n = 1 + self.dice.below(40);
                if len > 1 {
                    items.rotate_left(2 * n % len);
                }
                (format!("two({n}, {xs})"), items)
            }
            17 => {
                items.reverse();
                (format!("array::reverse({xs})"), items)
            }
            18 => {
                if len > 2 {
                    let a = items.remove(0);
                    items[0] = items[1];
                    items[1] = a;
                } else if len == 2 {
                    items.swap(0, 1);
                }
                (format!("third({xs})"), items)
            }
            _ => {
                let (i, j) = (self.dice.below(len + 1), self.dice.below(len + 1));
                if i < len && j < len {
                    items.swap(i, j);
                }
                (format!("array::swap({xs}, {i}, {j})"), items)
            }
        }
    }

    fn other(&mut self) -> Var {
        let n = self.dice.below(self.vars.len());
        self.vars[n].clone()
    }

    /// A name for the array that `src` makes.
    fn name(&mut self, src: &str, items: Vec<i64>) {
        let name = format!("x{}", self.names);
        self.names += 1;
        writeln!(self.main, "\t{name} = {src}").expect("a string");
        self.vars.push(Var { name, items });
    }

    /// Print what the array is, in one of the ways that one is read, and what it is.
    fn read(&mut self, v: &Var, out: &mut String) {
        let shown: Vec<String> = v.items.iter().map(|n| self.elem.shown(*n)).collect();
        let name = &v.name;
        let each = shown.iter().map(|s| format!("{s};")).collect::<String>();
        let (src, want) = match self.dice.below(5) {
            0 => (format!("show(300, {name}, \"\")"), each),
            1 => (format!("at({name}, 0, \"\")"), each),
            2 => {
                let len = v.items.len();
                (format!("array::length({name})"), len.to_string())
            }
            3 => {
                let literal = self.literal(&v.items);
                let src = format!("{name} == [{literal}]");
                (src, "True".to_string())
            }
            _ => {
                // Found by one that is equal and made in another way.
                let literal = self.literal(&v.items);
                let keyed = format!("map::set(map::new(), {name}, 41)");
                let src = format!("map::get({keyed}, array::reverse(array::reverse([{literal}])))");
                (src, "Some(41)".to_string())
            }
        };
        writeln!(self.main, "\tprintln({src})").expect("a string");
        writeln!(out, "{want}").expect("a string");
    }
}

/// A program and what it prints.
fn program(seed: u64) -> (String, String) {
    let mut dice = Dice(seed << 8);
    let elem = [Elem::Int, Elem::Text, Elem::Pair][dice.below(3)];
    let mut make = Maker {
        dice,
        elem,
        main: String::new(),
        names: 0,
        next: 1,
        vars: Vec::new(),
    };
    let mut out = String::new();
    for _ in 0..2 {
        let (src, items) = make.start();
        make.name(&src, items);
    }
    for _ in 0..12 {
        // Mostly the last one made, which nothing else may hold.
        let v = match make.dice.one_in(3) {
            true => make.other(),
            false => make.vars[make.vars.len() - 1].clone(),
        };
        let (src, items) = make.step(&v);
        if items.len() > 140 {
            continue;
        }
        make.name(&src, items);
        if make.dice.one_in(3) {
            let v = make.vars[make.vars.len() - 1].clone();
            make.read(&v, &mut out);
        }
    }
    for v in make.vars.clone() {
        // A name that nothing reads is refused.
        let read = make.main.split(|c: char| !c.is_alphanumeric());
        let unread = read.filter(|word| *word == v.name).count() < 2;
        if unread || make.dice.one_in(2) {
            make.read(&v, &mut out);
        }
    }
    let helpers = HELPERS.replace("Array(T)", &format!("Array({})", elem.ty()));
    let main = &make.main;
    let src = format!(
        "import scarlet/array\nimport scarlet/map\n{}\n{helpers}pub fn main() {{\n{main}}}\n",
        elem.maker()
    );
    (src, out)
}

/// What `src` prints.
fn run(src: &str, audit: Audit) -> String {
    let result = scarlet_core::compile::compile_text(src, None);
    assert!(result.success(), "{:?}\n{src}", result.diagnostics());
    let program = result.into_runnable().expect("a clean compile is runnable");
    let mut host = OsHost::new(Vec::new(), Vec::new(), Vec::new());
    let ran = scarlet_vm::run(&program, &mut host, audit);
    assert_eq!(ran, Ok(()), "{src}");
    let out = std::mem::take(host.out());
    String::from_utf8(out).expect("println writes UTF-8")
}

/// Whatever is done to arrays whose cells are full or nearly, a program prints what a list
/// says, and every count of it is right.
#[test]
fn a_program_of_short_arrays_does_what_a_list_says() {
    let read = |name| std::env::var(name).ok().and_then(|n| n.parse().ok());
    let from = read("SHORT_FROM").unwrap_or(0u64);
    for seed in from..from + read("SHORT").unwrap_or(150) {
        let (src, want) = program(seed);
        let audit = match seed % 16 {
            0 => Audit::EachInstruction,
            _ => Audit::AtEnd,
        };
        assert_eq!(run(&src, audit), want, "seed {seed}\n{src}");
    }
}
