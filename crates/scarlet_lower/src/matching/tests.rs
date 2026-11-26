use super::*;
use scarlet_ir::rty::{PrimIds, Repr, ResolvedPool};

use crate::immediates::{self, Immediates};
use crate::lower::{Around, Lowered, lower_fn};
use crate::perceus::perceus;
use crate::program::LinkError;
use crate::testkit::{ownership, vref};
use crate::typed_ir::{BindingId, TypedBinPatSeg, TypedCallee, TypedExpr, TypedFn, ValueRef};
use crate::{CoreExpr, CoreFn, tail_args};
use oracle::{Answers, Dice, Machine, Meaning, Val, World, value};
use random::{BOOL, Kind, Match, NIL, make};

mod oracle;
mod programs;
mod random;

fn im() -> Immediates {
    Immediates {
        true_: vref(BOOL.0, 0),
        false_: vref(BOOL.0, 1),
        nil: vref(NIL.0, 0),
    }
}

/// One match, as the function `f` that is nothing else.
struct Case<'a> {
    f: &'a TypedFn,
    pool: &'a ResolvedPool,
    shapes: &'a Shapes,
    consts: &'a [Const],
    temps: TempTys,
    im: Immediates,
}

impl Case<'_> {
    fn of(m: &Match) -> Case<'_> {
        Case {
            f: &m.f,
            pool: &m.pool,
            shapes: &m.shapes,
            consts: &m.consts,
            temps: m.temps,
            im: im(),
        }
    }

    fn arms(&self) -> &[TypedArm] {
        match &self.f.body {
            TypedExpr::Match { arms, .. } => arms,
            other => panic!("{other:?}"),
        }
    }

    /// What it is lowered to, and what the passes behind that make of it, which has to
    /// pass the ownership checker.
    fn lowered(&self, allowance: Allowance) -> [CoreFn; 2] {
        let around = Around {
            temps: self.temps,
            shapes: self.shapes,
            consts: self.consts,
            allowance,
        };
        let lowered = lower_fn(around, self.f);
        let Lowered { core, locals } = lowered.unwrap_or_else(|why| panic!("{why}: {:#?}", self.f));
        let first = core.clone();
        let mut core = core;
        immediates::rewrite(&mut core, &self.im);
        let reached = core.body.always_reached();
        let core = perceus(self.pool, self.shapes, &locals, &reached, core);
        let core = tail_args::schedule(self.pool, &reached, core.expect("it has drops"));
        let core = core.expect("it is in order");
        let fields = |ty| match self.shapes.of(ty) {
            Some(shape) => {
                let all = 0..u16::try_from(shape.variants()).unwrap();
                all.map(|v| shape.fields(v).unwrap()).collect()
            }
            None => Vec::new(),
        };
        let checked = ownership::check(self.pool, &fields, &core, ownership::Body::Function);
        assert_eq!(checked, Ok(()), "{core}");
        [first, core]
    }
}

/// The allowance there is, and none, so that every match is also a tree where nothing
/// may be copied.
fn usual_and_none() -> [Allowance; 2] {
    [Allowance::USUAL, Allowance { times: 0, any: 0 }]
}

/// Run `m` on `values` values, and say which arms were found.
fn agrees(
    dice: &mut Dice,
    m: &Case<'_>,
    allowance: Allowance,
    values: usize,
    what: &str,
) -> BTreeSet<u32> {
    let mut found = BTreeSet::new();
    let world = World {
        consts: m.consts,
        true_: m.im.true_,
    };
    let [first, last] = m.lowered(allowance);
    let mut pats: Vec<&TypedPat> = m.arms().iter().map(|a| &a.pat).collect();
    for _ in 0..values {
        // The first says most about what the value is like.
        let like = dice.below(pats.len());
        pats.swap(0, like);
        let v = value(dice, &world, m.shapes, &pats);
        let answers = Answers {
            seed: u64::try_from(dice.below(1 << 30)).unwrap(),
        };
        let meaning = Meaning {
            world: &world,
            answers: &answers,
            calls: Default::default(),
        };
        let want = meaning.run(&v, m.arms());
        let machine = Machine {
            world: &world,
            answers: &answers,
        };
        for f in [&first, &last] {
            let got = machine.run(f, &v);
            assert_eq!(got, want, "{what}: {v:?}\n{:#?}\n{first}{last}", m.arms());
        }
        if want.found {
            found.extend(want.calls.last().map(|(f, _)| *f));
        }
    }
    found
}

/// Whatever the match and whatever the value, the tree finds the first arm that fits,
/// with the same names, and runs the same guards and sizes on the way, each with the
/// same values and in the same order.
#[test]
fn a_tree_finds_what_the_rows_say() {
    let cases = std::env::var("MATCHES").ok().and_then(|n| n.parse().ok());
    let first = std::env::var("MATCHES_FROM")
        .ok()
        .and_then(|n| n.parse().ok());
    let (cases, first) = (cases.unwrap_or(12_000), first.unwrap_or(0u64));
    let mut found = 0;
    for seed in first..first + cases {
        let mut dice = Dice(seed << 20);
        let kind = Kind {
            arms: [3, 6, 6, 12][dice.below(4)],
            depth: 1 + dice.below(4),
            ors: [2, 4, 8, 1000][dice.below(4)],
            guards: [1, 2, 4, 1000][dice.below(4)],
            sizes: [2, 4, 1000][dice.below(3)],
        };
        let m = make(&mut dice, kind);
        let allowance = match dice.below(4) {
            0 => Allowance { times: 0, any: 0 },
            1 => Allowance {
                times: 0,
                any: dice.below(40),
            },
            _ => Allowance::USUAL,
        };
        found += agrees(&mut dice, &Case::of(&m), allowance, 12, &format!("{seed}")).len();
    }
    let cases = usize::try_from(cases).unwrap();
    assert!(found > cases * 2, "{found} arms found");
}

/// Writes patterns out, of a type whose constructors are `A` and `B`, with an Int each,
/// and `C` and `D`, with nothing.
#[derive(Default)]
struct Write {
    consts: Vec<Const>,
    binds: usize,
    /// The names that no guard and no body reads.
    unread: BTreeSet<usize>,
    /// Whether each body reads the matched value too.
    again: bool,
    /// Whether nothing may be copied.
    no_copies: bool,
}

const T: scarlet_ir::TypeId = scarlet_ir::TypeId(10);
/// A type with one constructor of two fields.
const PAIR: scarlet_ir::TypeId = scarlet_ir::TypeId(11);
const A: u16 = 0;
const B: u16 = 1;
const C: u16 = 2;
const D: u16 = 3;

/// The types that a [`Write`]'s patterns are of, which nothing here looks into.
const INT: RTy = RTy(0);
const OF_T: RTy = RTy(1);
const BOOL_TY: RTy = RTy(2);
const TUPLE: RTy = RTy(3);

impl Write {
    fn constant(&mut self, c: Const) -> ConstId {
        self.consts.push(c);
        ConstId(u32::try_from(self.consts.len() - 1).unwrap())
    }

    fn int(&mut self, n: i64) -> TypedPat {
        let value = self.constant(Const::Int(n));
        TypedPat::Lit { ty: INT, value }
    }

    fn name(&mut self) -> TypedBind {
        self.binds += 1;
        TypedBind {
            id: BindingId(self.binds),
            named: true,
            ty: INT,
            global: None,
        }
    }

    /// Constructor `v`, with `field` in it when it has one.
    fn ctor(&self, v: u16, field: TypedPat) -> TypedPat {
        let variant = VariantRef {
            type_id: T,
            variant_idx: v,
        };
        TypedPat::Ctor {
            ty: OF_T,
            variant,
            fields: if v < C { vec![field] } else { Vec::new() },
        }
    }

    fn plain(&self, v: u16) -> TypedPat {
        self.ctor(v, TypedPat::Wild { ty: INT })
    }

    /// A pair with these fields.
    fn pair(&self, fields: [TypedPat; 2]) -> TypedPat {
        TypedPat::Ctor {
            ty: OF_T,
            variant: VariantRef {
                type_id: PAIR,
                variant_idx: 0,
            },
            fields: fields.into(),
        }
    }

    /// A binary that starts with an Int of 8 bits each for `values`.
    fn bytes(&mut self, values: Vec<TypedPat>, rest: PatRest) -> TypedPat {
        let (zero, eight) = (self.constant(Const::Int(0)), self.constant(Const::Int(8)));
        let segs = values.into_iter().map(|value| TypedBinPatSeg::Int {
            bits: TypedExpr::Const {
                ty: INT,
                value: eight,
            },
            value,
        });
        TypedPat::Bin {
            ty: TUPLE,
            zero,
            segs: segs.collect(),
            rest,
        }
    }

    /// The function that is a match with these arms. A guard is a call of its own.
    fn typed(&self, arms: Vec<(TypedPat, bool)>) -> TypedFn {
        let scrut = TypedBind {
            id: BindingId(0),
            named: true,
            ty: TUPLE,
            global: None,
        };
        let whole = || TypedExpr::Var {
            ty: TUPLE,
            place: ValueRef::Local(scrut.id),
        };
        let call = |f: u32, ty: RTy, pat: &TypedPat| TypedExpr::Call {
            ty,
            callee: TypedCallee::Known(crate::FuncIdx(f)),
            args: pat_binds(pat)
                .iter()
                .filter(|b| !self.unread.contains(&b.id.0))
                .map(|b| TypedExpr::Var {
                    ty: b.ty,
                    place: ValueRef::Local(b.id),
                })
                .collect(),
        };
        let arms = (0u32..).zip(arms).map(|(i, (pat, guard))| TypedArm {
            guard: guard.then(|| call(i, BOOL_TY, &pat)),
            body: match (call(oracle::BODIES + i, INT, &pat), self.again) {
                (
                    TypedExpr::Call {
                        ty,
                        callee,
                        mut args,
                    },
                    true,
                ) => {
                    args.push(whole());
                    TypedExpr::Call { ty, callee, args }
                }
                (body, _) => body,
            },
            pat,
        });
        TypedFn {
            params: vec![scrut],
            ret: INT,
            body: TypedExpr::Match {
                ty: INT,
                scrut: Box::new(whole()),
                arms: arms.collect(),
            },
            binds: self.binds + 1,
        }
    }

    /// What that is lowered to.
    fn tried(&self, arms: Vec<(TypedPat, bool)>) -> Result<Lowered, LinkError> {
        let around = Around {
            temps: TempTys {
                int: INT,
                bool: BOOL_TY,
                string: INT,
                binary: INT,
                int_pair: TUPLE,
            },
            shapes: &shapes(),
            consts: &self.consts,
            allowance: usual_and_none()[usize::from(self.no_copies)],
        };
        lower_fn(around, &self.typed(arms))
    }

    fn lowered(&self, arms: Vec<(TypedPat, bool)>) -> CoreFn {
        self.tried(arms).expect("it is lowered").core
    }

    /// What it is once it has drops.
    fn dropped(&self, arms: Vec<(TypedPat, bool)>) -> CoreFn {
        let mut pool = ResolvedPool::new(PrimIds::default());
        let int = pool.prims().int;
        let mut con = |id, repr| pool.mk_con(id, repr, &[]);
        let all = [
            con(int, Repr::Counted),
            con(T, Repr::Counted),
            con(BOOL, Repr::Word),
            pool.mk_tuple(&[INT, INT]),
        ];
        assert_eq!(all, [INT, OF_T, BOOL_TY, TUPLE].map(Ok));
        let Lowered { core, locals } = self.tried(arms).expect("it is lowered");
        let reached = core.body.always_reached();
        perceus(&pool, &shapes(), &locals, &reached, core).expect("it has drops")
    }
}

fn shapes() -> Shapes {
    Shapes::new([(T, vec![1, 1, 0, 0]), (PAIR, vec![2])])
}

fn wild() -> TypedPat {
    TypedPat::Wild { ty: OF_T }
}

fn tuple(elems: Vec<TypedPat>) -> TypedPat {
    TypedPat::Tuple { ty: TUPLE, elems }
}

fn or(alts: Vec<TypedPat>) -> TypedPat {
    TypedPat::Or { ty: OF_T, alts }
}

/// How many nodes `e` is.
fn nodes(e: &CoreExpr) -> usize {
    let (mut n, mut work) = (0, vec![e]);
    while let Some(e) = work.pop() {
        n += 1;
        match e {
            CoreExpr::Let { body, .. }
            | CoreExpr::SetGlobal { body, .. }
            | CoreExpr::Drop { body, .. }
            | CoreExpr::Open { body, .. } => work.push(body),
            CoreExpr::Join { cont, body, .. } => work.extend([&**cont, &**body]),
            CoreExpr::If { then, els, .. } => work.extend([&**then, &**els]),
            CoreExpr::Switch { cases, default, .. } => {
                work.extend(cases.arms());
                work.extend(default.as_deref());
            }
            CoreExpr::Ret(_)
            | CoreExpr::TailCall { .. }
            | CoreExpr::Jump { .. }
            | CoreExpr::NoArm => {}
        }
    }
    n
}

/// Arms that grow with a number, each with whether it has a guard.
type Arms = fn(&mut Write, usize) -> Vec<(TypedPat, bool)>;

/// The shapes that make a tree grow: what a tree of `n` is made of.
fn growing() -> Vec<(&'static str, Arms)> {
    fn row(n: usize, at: impl Fn(usize) -> Option<TypedPat>) -> TypedPat {
        tuple((0..n).map(|j| at(j).unwrap_or_else(wild)).collect())
    }
    vec![
        ("each row asks another place", |w, n| {
            let one = |i| (row(n, |j| (i == j).then(|| w.plain(A))), false);
            (0..n).map(one).chain([(wild(), false)]).collect()
        }),
        ("the same, with guards", |w, n| {
            let one = |i| (row(n, |j| (i == j).then(|| w.plain(A))), true);
            (0..n).map(one).chain([(wild(), false)]).collect()
        }),
        ("each row asks two places, and two rows ask each", |w, n| {
            let one = |i: usize| {
                let of = |v| row(n, |j| (j == i || j == i + 1).then(|| w.plain(v)));
                [(of(A), false), (of(B), false)]
            };
            let rows = (0..n - 1).flat_map(one);
            rows.chain([(wild(), false)]).collect()
        }),
        ("each row asks one place more", |w, n| {
            let one = |i| {
                let at = |j| match i32::try_from(i).unwrap() - i32::try_from(j).unwrap() {
                    0 => Some(w.plain(B)),
                    1.. => Some(w.plain(A)),
                    _ => None,
                };
                (row(n, at), false)
            };
            (0..n).map(one).chain([(wild(), false)]).collect()
        }),
        ("every row asks every third place", |w, n| {
            let one = |i: usize| {
                let v = [A, B, C, D][i / 3 % 4];
                (
                    row(n, |j| (j % 3 == i % 3).then(|| w.plain(v))),
                    i.is_multiple_of(2),
                )
            };
            (0..n).map(one).chain([(wild(), false)]).collect()
        }),
        ("or-patterns that bind nothing beside each other", |w, n| {
            let pat = |w: &mut Write, _| {
                let (x, y) = (w.int(1), w.int(2));
                or(vec![w.ctor(A, x), w.ctor(B, y), w.plain(D)])
            };
            let elems = (0..n).map(|j| pat(w, j)).collect();
            vec![(tuple(elems), true), (wild(), false)]
        }),
        ("or-patterns that bind beside each other", |w, n| {
            let pat = |w: &mut Write| {
                let b = w.name();
                or(vec![
                    w.ctor(A, TypedPat::Bind(b)),
                    w.ctor(B, TypedPat::Bind(b)),
                ])
            };
            let elems = (0..n).map(|_| pat(w)).collect();
            vec![(tuple(elems), false), (wild(), false)]
        }),
        (
            "the same, with a guard, told apart by their constructors",
            |w, n| or_patterns(w, n, |w, b| [w.ctor(A, b.clone()), w.ctor(B, b)]),
        ),
        ("by literals in tuples", |w, n| {
            or_patterns(w, n, |w, b| {
                [1, 2].map(|k| tuple(vec![b.clone(), wild(), w.int(k)]))
            })
        }),
        ("by literals in one constructor", |w, n| {
            or_patterns(w, n, |w, b| {
                [1, 2].map(|k| {
                    let field = tuple(vec![w.int(k), b.clone()]);
                    w.ctor(A, field)
                })
            })
        }),
        ("by how long arrays are", |w, n| {
            or_patterns(w, n, |w, b| {
                let long = vec![b.clone(), wild()];
                [
                    array(w, vec![b], PatRest::None),
                    array(w, long, PatRest::Discard),
                ]
            })
        }),
        ("by how long arrays are, the longer first", |w, n| {
            or_patterns(w, n, |w, b| {
                let long = vec![b.clone(), wild()];
                [
                    array(w, long, PatRest::None),
                    array(w, vec![b], PatRest::None),
                ]
            })
        }),
        ("by what is in arrays", |w, n| {
            or_patterns(w, n, |w, b| {
                [1, 2].map(|k| {
                    let items = vec![w.int(k), b.clone()];
                    array(w, items, PatRest::Discard)
                })
            })
        }),
        ("by an or-pattern in one of them", |w, n| {
            or_patterns(w, n, |w, b| {
                let ab = [A, B].map(|v| w.ctor(v, wild()));
                let cd = [C, D].map(|v| w.plain(v));
                [ab, cd].map(|alts| tuple(vec![or(alts.into()), b.clone()]))
            })
        }),
        ("binaries that bind beside each other", |w, n| {
            let pat = |w: &mut Write| {
                let b = TypedPat::Bind(w.name());
                let (one, two) = (vec![b.clone()], vec![wild(), b]);
                or(vec![
                    w.bytes(one, PatRest::Discard),
                    w.bytes(two, PatRest::Discard),
                ])
            };
            let elems = (0..n).map(|_| pat(w)).collect();
            vec![(tuple(elems), false), (wild(), false)]
        }),
        ("one long or-pattern", |w, n| {
            let all = (0..n)
                .map(|i| w.int(i64::try_from(i * 7).unwrap()))
                .collect();
            vec![(or(all), false), (wild(), false)]
        }),
        ("long or-patterns beside each other", |w, n| {
            let mut pat = || or((0..n).map(|i| w.int(i64::try_from(i).unwrap())).collect());
            let rows = vec![
                tuple(vec![pat(), pat(), pat()]),
                tuple(vec![wild(), pat(), pat()]),
            ];
            let rows = rows.into_iter().map(|p| (p, true));
            rows.chain([(wild(), false)]).collect()
        }),
        ("many literals", |w, n| {
            let one = |i| (w.int(i64::try_from(i).unwrap()), false);
            let rows: Vec<_> = (0..n).map(one).collect();
            rows.into_iter().chain([(wild(), false)]).collect()
        }),
        ("many literals, with a guard on each", |w, n| {
            let one = |i| (w.int(i64::try_from(i / 2).unwrap()), true);
            let rows: Vec<_> = (0..n).map(one).collect();
            rows.into_iter().chain([(wild(), false)]).collect()
        }),
        ("literals and guards by turns", |w, n| {
            let one = |i: usize| match i % 2 {
                0 => (w.int(i64::try_from(i).unwrap()), false),
                _ => (wild(), true),
            };
            let rows: Vec<_> = (0..n).map(one).collect();
            rows.into_iter().chain([(wild(), false)]).collect()
        }),
        ("literals and ranges by turns", |w, n| {
            let one = |i: usize| {
                let k = i64::try_from(i * 10).unwrap();
                let (lo, hi) = (w.constant(Const::Int(k)), w.constant(Const::Int(k + 5)));
                match i % 2 {
                    0 => (w.int(k), false),
                    _ => (TypedPat::Range { ty: INT, lo, hi }, i.is_multiple_of(3)),
                }
            };
            let rows: Vec<_> = (0..n).map(one).collect();
            rows.into_iter().chain([(wild(), false)]).collect()
        }),
        ("literals behind rows that ask something else", |w, n| {
            let first = tuple(vec![w.int(0), wild()]);
            let other = |w: &mut Write, i: usize| {
                let k = w.int(i64::try_from(i).unwrap());
                tuple(vec![wild(), k])
            };
            let own = |w: &mut Write, i: usize| {
                let k = w.int(i64::try_from(i + 1).unwrap());
                tuple(vec![k, wild()])
            };
            let mut rows = vec![first];
            rows.extend((0..n).map(|i| other(w, i)));
            rows.extend((0..n).map(|i| own(w, i)));
            let rows = rows.into_iter().map(|p| (p, false));
            rows.chain([(wild(), false)]).collect()
        }),
        ("pairs of literals", |w, n| {
            let one = |i: usize| {
                let (a, b) = (i64::try_from(i % 7).unwrap(), i64::try_from(i / 7).unwrap());
                match i % 5 {
                    4 => (tuple(vec![wild(), w.int(b)]), i.is_multiple_of(2)),
                    _ => (tuple(vec![w.int(a), w.int(b)]), false),
                }
            };
            let rows: Vec<_> = (0..n).map(one).collect();
            rows.into_iter().chain([(wild(), false)]).collect()
        }),
        ("arrays of every length", |w, n| {
            let one = |i: usize| {
                let len = w.constant(Const::Int(i64::try_from(i % 9).unwrap()));
                let pat = TypedPat::Array {
                    ty: TUPLE,
                    elem_ty: OF_T,
                    len,
                    prefix: (0..i % 9)
                        .map(|j| if j == i / 9 { w.plain(A) } else { wild() })
                        .collect(),
                    rest: if i.is_multiple_of(2) {
                        PatRest::None
                    } else {
                        PatRest::Discard
                    },
                };
                (pat, i.is_multiple_of(4))
            };
            let rows: Vec<_> = (0..n).map(one).collect();
            rows.into_iter().chain([(wild(), false)]).collect()
        }),
        ("each row asks how long another is", |w, n| {
            let one = |i| {
                let at = |w: &mut Write, j| match i == j {
                    true => array(w, vec![wild(); 1 + i % 3], PatRest::Discard),
                    false => wild(),
                };
                (tuple((0..n).map(|j| at(w, j)).collect()), true)
            };
            let rows: Vec<_> = (0..n).map(one).collect();
            rows.into_iter().chain([(wild(), false)]).collect()
        }),
        ("names in names", |w, n| {
            let deep = n.min(40);
            let pat = |w: &Write, last: u16| {
                let end = w.plain(last);
                (0..deep).fold(end, |p, _| tuple(vec![w.plain(A), p]))
            };
            let rows = [A, B, C, D].map(|v| (pat(w, v), v == B));
            rows.into_iter().chain([(wild(), false)]).collect()
        }),
    ]
}

/// One arm with a guard and `n` or-patterns of two alternatives beside each other, which
/// `alts` makes with the name they both bind, and an arm for everything.
fn or_patterns(
    w: &mut Write,
    n: usize,
    alts: impl Fn(&mut Write, TypedPat) -> [TypedPat; 2],
) -> Vec<(TypedPat, bool)> {
    let one = |w: &mut Write| {
        let b = TypedPat::Bind(w.name());
        or(alts(w, b).into())
    };
    let elems = (0..n).map(|_| one(w)).collect();
    vec![(tuple(elems), true), (wild(), false)]
}

/// What a tree is made of grows no faster than the match does, whatever its shape. Here
/// each is made twice as large again and again.
#[test]
fn a_tree_grows_with_its_match() {
    // One way through a tree of 128 places asks of each in turn, and each of those is
    // in the one before.
    let large = std::thread::Builder::new().stack_size(64 * 1024 * 1024);
    let run = large.spawn(|| {
        for (what, make) in growing() {
            // Each segment of a binary is read in a dozen nodes.
            let most = if what.starts_with("binaries") {
                100
            } else {
                24
            };
            let mut each = Vec::new();
            for n in [8, 16, 32, 64, 128] {
                let mut w = Write::default();
                let arms = make(&mut w, n);
                // A guard and a body are a few nodes each.
                let size: usize = arms.iter().map(|(p, _)| rows::size_of(p) + 4).sum();
                let got = nodes(&w.lowered(arms).body);
                // Past the first few, no faster than by half more than the match does.
                if let Some((smaller, few)) = each.last()
                    && *few > 600
                {
                    let grows = got * smaller * 2 <= few * size * 3;
                    assert!(grows, "{what}: {each:?} {size} {got}");
                }
                each.push((size, got));
                assert!(got <= size * most + 600, "{what}: {each:?}");
            }
        }
    });
    run.expect("a thread").join().expect("each is small");
}

/// Hundreds of rows that each ask something else take a little stack, and so do thousands
/// with a guard on every second.
#[test]
fn many_rows_take_a_little_stack() {
    let small = std::thread::Builder::new().stack_size(1024 * 1024);
    let run = small.spawn(|| {
        let mut w = Write::default();
        let one = |i: usize| match i % 2 {
            0 => (w.int(i64::try_from(i).unwrap()), false),
            _ => (wild(), true),
        };
        let mut rows: Vec<_> = (0..6000).map(one).collect();
        rows.push((wild(), false));
        let guards = nodes(&w.lowered(rows).body);
        let n = 600;
        let one = |i| {
            let at = |j| if i == j { w.plain(A) } else { wild() };
            (tuple((0..n).map(at).collect()), false)
        };
        let rows = (0..n).map(one).chain([(wild(), false)]).collect();
        [guards, nodes(&w.lowered(rows).body)]
    });
    let [guards, questions] = run.expect("a thread").join().expect("they fit");
    assert!(guards > 6000 && questions > 600);
}

/// What the tree of `arms` finds for each of `values` is what the rows say, whatever the
/// guards say.
fn finds(w: &Write, arms: Vec<(TypedPat, bool)>, values: &[Val]) {
    let f = w.typed(arms.clone());
    let TypedExpr::Match { arms: typed, .. } = &f.body else {
        panic!("a match")
    };
    let core = w.lowered(arms);
    let world = World {
        consts: &w.consts,
        true_: im().true_,
    };
    for v in values {
        for seed in 0..20 {
            let answers = Answers { seed };
            let meaning = Meaning {
                world: &world,
                answers: &answers,
                calls: Default::default(),
            };
            let machine = Machine {
                world: &world,
                answers: &answers,
            };
            assert_eq!(
                machine.run(&core, v),
                meaning.run(v, typed),
                "{v:?}\n{core}"
            );
        }
    }
}

/// Where nothing may be copied, what is left of a row that is several stays in order, in
/// front of the rows behind it.
#[test]
fn what_is_cut_stays_in_order() {
    let mut w = Write {
        no_copies: true,
        ..Write::default()
    };
    let x = TypedPat::Bind(w.name());
    let both = [vec![x.clone(), wild()], vec![wild(), x]].map(tuple);
    let either = or(both.into());
    let pair = Val::Tuple(vec![Val::Int(1), Val::Int(2)]);
    let empty = array(&mut w, Vec::new(), PatRest::None);
    let one = array(&mut w, vec![wild()], PatRest::None);
    let a = w.plain(A);
    let asks = [
        tuple(vec![one, wild(), wild()]),
        tuple(vec![wild(), a, wild()]),
    ];
    let no = Val::Array(Vec::new());
    for first in asks {
        let arms = vec![
            (first, true),
            (tuple(vec![wild(), wild(), either.clone()]), true),
            (tuple(vec![empty.clone(), wild(), wild()]), true),
            (wild(), false),
        ];
        let values = [A, B].map(|v| Val::Ctor(T, v, vec![Val::Int(7)]));
        let values = values.map(|v| Val::Tuple(vec![no.clone(), v, pair.clone()]));
        finds(&w, arms, &values);
    }
}

/// Alternatives that are all the constructors of a type are for every value only when they
/// ask nothing of any field.
#[test]
fn alternatives_that_ask_of_a_field_are_asked() {
    let mut w = Write {
        no_copies: true,
        ..Write::default()
    };
    let (one, two, five) = (w.int(1), w.int(2), w.int(5));
    let alts = [one, two].map(|n| w.pair([n, wild()]));
    let first = tuple(vec![or(alts.into()), five.clone()]);
    let second = tuple(vec![wild(), five]);
    let arms = vec![(first, true), (second, false), (wild(), false)];
    let pairs = (0..4).flat_map(|n| [(n, 5), (n, 6)]);
    let values = pairs.map(|(n, k)| {
        let pair = Val::Ctor(PAIR, 0, vec![Val::Int(n), Val::Int(0)]);
        Val::Tuple(vec![pair, Val::Int(k)])
    });
    finds(&w, arms, &values.collect::<Vec<_>>());
}

/// A range has no more of its ends for an Int that is already known than for another.
#[test]
fn a_known_int_is_in_a_range_up_to_its_end() {
    let mut w = Write::default();
    let (lo, hi) = (w.constant(Const::Int(2)), w.constant(Const::Int(5)));
    let arms = [1, 2, 3, 5, 6].map(|n| (w.int(n), true));
    let range = TypedPat::Range { ty: INT, lo, hi };
    let either = or(vec![range, w.int(8)]);
    let arms = arms.into_iter().chain([(either, true), (wild(), false)]);
    let values: Vec<Val> = (0..10).map(Val::Int).collect();
    finds(&w, arms.collect(), &values);
}

/// What `arms` are lowered to, as it is printed.
fn printed(make: impl Fn(&mut Write) -> Vec<(TypedPat, bool)>) -> String {
    let mut w = Write::default();
    let arms = make(&mut w);
    w.lowered(arms).to_string()
}

fn array(w: &mut Write, prefix: Vec<TypedPat>, rest: PatRest) -> TypedPat {
    let len = w.constant(Const::Int(i64::try_from(prefix.len()).unwrap()));
    TypedPat::Array {
        ty: TUPLE,
        elem_ty: OF_T,
        len,
        prefix,
        rest,
    }
}

/// An array that is not empty has a first element, which is not asked again.
#[test]
fn the_length_of_an_array_is_asked_once() {
    let core = printed(|w| {
        let (first, tail) = (w.name(), w.name());
        let empty = array(w, Vec::new(), PatRest::None);
        let more = array(w, vec![TypedPat::Bind(first)], PatRest::Bind(tail));
        vec![(empty, false), (more, false)]
    });
    assert_eq!(core.matches("if ").count(), 1, "{core}");
    assert!(!core.contains("no arm"), "{core}");
    // The head is read in front of the rest, which then takes the array over.
    let (head, cut) = (core.find("ArrayElem"), core.find("ArrayDrop"));
    assert!(head.is_some() && head < cut, "{core}");
}

/// What two arms ask is asked once, and a field is read on the ways that use it.
#[test]
fn a_field_is_read_where_it_is_used() {
    let core = printed(|w| {
        let (one, two, x) = (w.int(1), w.int(2), w.name());
        vec![
            (w.ctor(A, one), false),
            (w.ctor(A, two), false),
            (w.ctor(B, TypedPat::Bind(x)), false),
            (or(vec![w.plain(A), w.plain(C), w.plain(D)]), false),
        ]
    });
    assert_eq!(core.matches("switch").count(), 2, "{core}");
    assert_eq!(core.matches("Field").count(), 2, "{core}");
}

/// A body that several alternatives lead to is there once, and so is a guard when they
/// bind nothing.
#[test]
fn a_body_is_there_once() {
    let core = printed(|w| {
        let x = w.name();
        let both = [A, B].map(|v| w.ctor(v, TypedPat::Bind(x)));
        vec![
            (or(both.into()), false),
            (or(vec![w.plain(C), w.plain(D)]), true),
            (or(vec![w.plain(C), w.plain(D)]), false),
        ]
    });
    for f in [oracle::BODIES, 1, oracle::BODIES + 1, oracle::BODIES + 2] {
        let call = format!("fn#{f}(");
        assert_eq!(core.matches(&call).count(), 1, "{call} {core}");
    }
}

/// A value with one constructor is taken apart and not asked which it is.
#[test]
fn the_only_constructor_is_not_asked_for() {
    let pair = VariantRef {
        type_id: PAIR,
        variant_idx: 0,
    };
    let core = printed(|w| {
        let fields = vec![w.int(3), TypedPat::Bind(w.name())];
        let first = TypedPat::Ctor {
            ty: OF_T,
            variant: pair,
            fields,
        };
        vec![(first, false), (wild(), false)]
    });
    assert!(!core.contains("no arm"), "{core}");
    assert_eq!(core.matches("switch").count(), 2, "{core}");
}

/// A guard is there for each way its pattern can fit, however many those are. Where it
/// is plain that one alternative can fit at the most, it is there more than once only as
/// far as the allowance goes.
#[test]
fn a_guard_is_there_for_each_way_its_pattern_can_fit() {
    let guards = |n, make: fn(&mut Write, TypedPat) -> [TypedPat; 2]| {
        let core = printed(|w| or_patterns(w, n, make));
        core.matches("fn#0(").count()
    };
    let apart = |w: &mut Write, b: TypedPat| [w.ctor(A, b.clone()), w.ctor(B, b)];
    assert!(guards(12, apart) < 16);
    let both =
        |_: &mut Write, b: TypedPat| [[b.clone(), wild()], [wild(), b]].map(|e| tuple(e.into()));
    assert_eq!(guards(8, both), 256);
}

/// What is wanted of a constructor is read in one place, so that it can be taken apart
/// there, though a test of a field stands between.
#[test]
fn a_constructor_is_taken_apart_in_front_of_what_is_asked_of_its_fields() {
    let rows = |w: &mut Write| {
        let (one, x, y) = (w.int(1), w.name(), w.name());
        let (x, y) = (TypedPat::Bind(x), TypedPat::Bind(y));
        [w.pair([one, x]), w.pair([wild(), y])]
    };
    // As the matched value itself, and in another.
    for inside in [false, true] {
        let mut w = Write::default();
        let rows = rows(&mut w).map(|p| match inside {
            true => (tuple(vec![p]), false),
            false => (p, false),
        });
        let core = w.dropped(rows.into()).to_string();
        assert_eq!(core.matches("open").count(), 1, "{core}");
        assert!(!core.contains(" Field("), "{core}");
    }
    // Nothing is read ahead of a test where something behind the match needs the pair.
    let mut w = Write {
        again: true,
        ..Write::default()
    };
    let all = rows(&mut w).map(|p| (p, false));
    let core = w.lowered(all.into()).to_string();
    assert!(reads_behind_test(&core), "{core}");
    // Nor where a row below does.
    let core = printed(|w| {
        let [first, _] = rows(w);
        vec![(first, false), (TypedPat::Bind(w.name()), false)]
    });
    assert!(reads_behind_test(&core), "{core}");
    // And nothing where a miss goes on at rows behind, which may ask the pair too.
    let core = printed(|w| {
        let [first, _] = rows(w);
        let (lo, hi) = (w.constant(Const::Int(0)), w.constant(Const::Int(5)));
        let range = TypedPat::Range { ty: INT, lo, hi };
        vec![(tuple(vec![range, first]), false), (wild(), false)]
    });
    assert!(reads_behind_test(&core), "{core}");
}

/// Whether field 1 is read behind the one switch on an Int.
fn reads_behind_test(core: &str) -> bool {
    let (test, read) = (core.find("| 1 ->"), core.find(" Field(1)"));
    test.is_some() && test < read
}

/// Of the questions that a row has, the one is asked first that the most rows ask.
#[test]
fn what_most_rows_ask_is_asked_first() {
    let core = printed(|w| {
        let row = |w: &Write, a: u16, b: u16| match a {
            D => tuple(vec![wild(), w.plain(b)]),
            a => tuple(vec![w.plain(a), w.plain(b)]),
        };
        let rows = [row(w, A, A), row(w, D, B), row(w, D, C)];
        let rows = rows.into_iter().map(|p| (p, false));
        rows.chain([(wild(), false)]).collect()
    });
    assert_eq!(core.matches("switch").count(), 2, "{core}");
}

/// A name that no guard and no body reads is not read out of the value.
#[test]
fn a_name_that_nothing_reads_is_not_read() {
    let mut w = Write::default();
    let (x, y) = (w.name(), w.name());
    w.unread.insert(y.id.0);
    let rows = vec![
        (w.ctor(A, TypedPat::Bind(x)), false),
        (w.ctor(B, TypedPat::Bind(y)), false),
        (wild(), false),
    ];
    let core = w.lowered(rows).to_string();
    assert_eq!(core.matches(" Field(").count(), 1, "{core}");
}

/// Constructors that the same rows are left for are one case, whatever is between those.
#[test]
fn constructors_with_the_same_rows_are_one_case() {
    let core = printed(|w| {
        let of = |w: &Write, vs: [u16; 2]| or(vs.map(|v| w.plain(v)).into());
        let two = |a, b| tuple(vec![a, b]);
        vec![
            (two(of(w, [A, C]), wild()), true),
            (two(wild(), w.plain(B)), true),
            (two(of(w, [C, A]), wild()), true),
            (two(w.plain(D), wild()), false),
            (wild(), false),
        ]
    });
    assert!(core.contains("| 10.0, 10.2 ->"), "{core}");
}

/// A pattern that says what cannot be is an error that says so.
#[test]
fn a_pattern_that_cannot_be_is_refused() {
    let refused = |make: &dyn Fn(&mut Write) -> Vec<TypedPat>| {
        let mut w = Write::default();
        let arms = make(&mut w).into_iter().map(|p| (p, false)).collect();
        match w.tried(arms) {
            Err(LinkError::Internal(why)) => why,
            other => panic!("{:?}", other.map(|l| l.core.to_string())),
        }
    };
    let lit = |value| TypedPat::Lit { ty: INT, value };
    let bits = |w: &mut Write| {
        w.constant(Const::Binary {
            bytes: vec![1],
            bit_len: 8,
        })
    };
    let why = refused(&|w| vec![lit(bits(w))]);
    assert_eq!(why, InternalLinkError::OddLiteral(ConstId(0)));
    let why = refused(&|_| vec![lit(ConstId(7))]);
    assert_eq!(why, InternalLinkError::OddLiteral(ConstId(7)));
    let range = |lo, hi| TypedPat::Range { ty: INT, lo, hi };
    let text = |w: &mut Write| w.constant(Const::String("a".to_string()));
    let why = refused(&|w| {
        let (lo, hi) = (w.constant(Const::Int(1)), text(w));
        vec![range(lo, hi)]
    });
    assert_eq!(why, InternalLinkError::OddBound(ConstId(1)));
    let why = refused(&|w| {
        let (lo, hi) = (w.constant(Const::Float(1.0)), w.constant(Const::Int(2)));
        vec![range(lo, hi)]
    });
    assert_eq!(why, InternalLinkError::OddBound(ConstId(0)));
    let why = refused(&|w| {
        let hi = w.constant(Const::Int(2));
        vec![range(ConstId(9), hi)]
    });
    assert_eq!(why, InternalLinkError::OddBound(ConstId(9)));
    // Kinds of values that are not of one type.
    let why = refused(&|w| vec![w.plain(A), w.int(1)]);
    assert_eq!(why, InternalLinkError::OddPattern);
    let why = refused(&|w| vec![w.int(1), w.plain(A)]);
    assert_eq!(why, InternalLinkError::OddPattern);
    let why = refused(&|w| {
        let x = w.constant(Const::Float(1.0));
        vec![lit(x), w.int(1)]
    });
    assert_eq!(why, InternalLinkError::OddPattern);
    let why = refused(&|w| vec![lit(text(w)), w.int(1)]);
    assert_eq!(why, InternalLinkError::OddPattern);
    let why = refused(&|w| vec![w.plain(A), w.pair([wild(), wild()])]);
    assert_eq!(why, InternalLinkError::OddPattern);
    // An element that no instruction can name.
    let why = refused(&|w| {
        let mut elems = vec![wild(); 65_537];
        elems[65_536] = w.int(1);
        vec![tuple(elems)]
    });
    assert_eq!(why, InternalLinkError::IndexTooWide(65_536));
}
