use super::*;
use scarlet_ir::rty::{PrimIds, Repr, ResolvedPool};

use crate::immediates;
use crate::lower::{Around, Lowered, lower_fn};
use crate::perceus::perceus;
use crate::program::{LinkError, PreludeCtors};
use crate::testkit::{ownership, vref};
use crate::typed_ir::{
    BindingId, Gives, TypedBinPatSeg, TypedCallee, TypedExpr, TypedFn, ValueRef,
};
use crate::{CoreExpr, CoreFn, tail_args};
use oracle::{Answers, Dice, Machine, Meaning, Val, World, value};
use random::{BOOL, Kind, Match, NIL, OPTION, RESULT, make};

mod held;
mod oracle;
mod programs;
mod random;

fn ctors() -> PreludeCtors {
    PreludeCtors {
        some: vref(OPTION.0, 0),
        none: vref(OPTION.0, 1),
        ok: vref(RESULT.0, 0),
        err: vref(RESULT.0, 1),
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
    ctors: PreludeCtors,
}

impl Case<'_> {
    fn of(m: &Match) -> Case<'_> {
        Case {
            f: &m.f,
            pool: &m.pool,
            shapes: &m.shapes,
            consts: &m.consts,
            temps: m.temps,
            ctors: ctors(),
        }
    }

    fn arms(&self) -> &[TypedArm] {
        match &self.f.body {
            TypedExpr::Match { arms, .. } => arms,
            TypedExpr::Let { body, .. } => match &**body {
                TypedExpr::Match { arms, .. } => arms,
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }

    /// The constructors of what the match is on, when that is a probe's value.
    fn outcomes(&self) -> Option<[crate::VariantRef; 2]> {
        let scrut = match &self.f.body {
            TypedExpr::Match { scrut, .. } => scrut,
            TypedExpr::Let { init, .. } => init,
            other => panic!("{other:?}"),
        };
        let TypedExpr::Probe { gives, .. } = &**scrut else {
            return None;
        };
        match gives {
            Gives::Option => Some([self.ctors.some, self.ctors.none]),
            Gives::Result { .. } => Some([self.ctors.ok, self.ctors.err]),
            Gives::OrFirst => None,
        }
    }

    /// What it is lowered to, and what the passes behind that make of it, which has to
    /// pass the ownership checker.
    fn lowered(&self, allowance: Allowance) -> [CoreFn; 2] {
        let around = Around {
            temps: self.temps,
            ctors: self.ctors,
            shapes: self.shapes,
            consts: self.consts,
            allowance,
        };
        let lowered = lower_fn(around, self.f);
        let Lowered { core, locals } = lowered.unwrap_or_else(|why| panic!("{why}: {:#?}", self.f));
        let first = core.clone();
        let mut core = core;
        immediates::rewrite(&mut core, &self.ctors.immediates());
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
        true_: m.ctors.true_,
        nil: m.ctors.nil,
    };
    let [first, last] = m.lowered(allowance);
    let mut pats: Vec<&TypedPat> = m.arms().iter().map(|a| &a.pat).collect();
    for _ in 0..values {
        // The first says most about what the value is like.
        let like = dice.below(pats.len());
        pats.swap(0, like);
        let mut v = value(dice, &world, m.shapes, &pats);
        if let Some([hit, miss]) = m.outcomes() {
            // What it is of has to be an outcome, whatever the patterns say of it.
            if let Val::Any(n) = v {
                let of = if n % 2 == 0 { hit } else { miss };
                v = Val::Ctor(of.type_id, of.variant_idx, vec![Val::Any(n)]);
            }
            if let Val::Ctor(_, idx, parts) = &mut v
                && *idx == miss.variant_idx
            {
                let held = usize::from(miss == m.ctors.err);
                *parts = vec![world.nil(); held];
            }
        }
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
            probes: 3,
            arrays: [2, 1000][dice.below(2)],
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

/// While a guard or a size runs, an array is held where it is read again behind it, and a
/// tail that it does not read waits for it only then.
#[test]
fn a_guard_holds_no_array_that_nothing_reads_again() {
    for sizes in [1000, 2] {
        holds_no_array_that_nothing_reads_again(sizes);
    }
}

fn holds_no_array_that_nothing_reads_again(sizes: usize) {
    let (mut early, mut late, mut length, mut guards) = (0, 0, 0, 0);
    let seeds = std::env::var("HELD").map_or(Ok(0..4000), |n| n.parse().map(|n| 4000..n));
    for seed in seeds.unwrap() {
        let mut dice = Dice(0x5EED << 32 | seed << 8);
        let kind = Kind {
            arms: [6, 6, 6, 40][dice.below(4)],
            depth: 1 + dice.below(3),
            ors: [4, 1000][dice.below(2)],
            guards: 2,
            sizes,
            probes: 1000,
            arrays: 2,
        };
        let m = make(&mut dice, kind);
        let [first, _] = Case::of(&m).lowered(Allowance::USUAL);
        // The checker refuses a match with an arm that nothing comes to.
        let core = first.to_string();
        let mut bodies = (oracle::BODIES..).take(Case::of(&m).arms().len());
        if !bodies.all(|f| core.contains(&format!("fn#{f}("))) {
            continue;
        }
        let wrong = held::wrong(&first);
        early += wrong.early;
        late += wrong.late;
        length += wrong.length;
        guards += wrong.guards;
    }
    assert!(guards > 1000, "{guards}");
    // A tuple is held by the parts that are read of it behind a guard, and those are not
    // looked at. A tail of an array in it is then taken early, which copies the root.
    assert!(early <= guards / 1000, "{early} of {guards}, sizes {sizes}");
    assert_eq!([late, length], [0; 2], "of {guards}, sizes {sizes}");
}

/// Writes patterns out, of a type whose constructors are `A` and `B`, with an Int each,
/// and `C` and `D`, with nothing.
#[derive(Default)]
struct Write {
    consts: Vec<Const>,
    binds: usize,
    /// The names that no guard and no body reads.
    unread: BTreeSet<usize>,
    /// Those that a body reads and no guard does.
    only_body: BTreeSet<usize>,
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
                .filter(|b| f >= oracle::BODIES || !self.only_body.contains(&b.id.0))
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
            ctors: ctors(),
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
            CoreExpr::If {
                then: a, els: b, ..
            }
            | CoreExpr::Probe {
                hit: a, miss: b, ..
            } => work.extend([&**a, &**b]),
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
            or_patterns(w, n, |_, b| {
                let long = vec![b.clone(), wild()];
                [array(vec![b], PatRest::None), array(long, PatRest::Discard)]
            })
        }),
        ("by how long arrays are, the longer first", |w, n| {
            or_patterns(w, n, |_, b| {
                let long = vec![b.clone(), wild()];
                [array(long, PatRest::None), array(vec![b], PatRest::None)]
            })
        }),
        ("by what is in arrays", |w, n| {
            or_patterns(w, n, |w, b| {
                [1, 2].map(|k| {
                    let items = vec![w.int(k), b.clone()];
                    array(items, PatRest::Discard)
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
                let pat = TypedPat::Array {
                    ty: TUPLE,
                    elem_ty: OF_T,
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
        ("each row asks how long another is", |_, n| {
            let one = |i| {
                let at = |j| match i == j {
                    true => array(vec![wild(); 1 + i % 3], PatRest::Discard),
                    false => wild(),
                };
                (tuple((0..n).map(at).collect()), true)
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
        true_: ctors().true_,
        nil: ctors().nil,
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
    let empty = array(Vec::new(), PatRest::None);
    let one = array(vec![wild()], PatRest::None);
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

fn array(prefix: Vec<TypedPat>, rest: PatRest) -> TypedPat {
    TypedPat::Array {
        ty: TUPLE,
        elem_ty: OF_T,
        prefix,
        rest,
    }
}

/// An array that is not empty has a first element, which is not asked again.
#[test]
fn the_length_of_an_array_is_asked_once() {
    let core = printed(|w| {
        let (first, tail) = (w.name(), w.name());
        let empty = array(Vec::new(), PatRest::None);
        let more = array(vec![TypedPat::Bind(first)], PatRest::Bind(tail));
        vec![(empty, false), (more, false)]
    });
    assert_eq!(core.matches("probe ").count(), 1, "{core}");
    assert!(core.contains("probe ArrayUncons"), "{core}");
    assert!(!core.contains("no arm"), "{core}");
}

/// The lines of `core` that ask for something, or take something, in order.
fn asks(core: &str) -> Vec<String> {
    let lines = core.lines().map(str::trim);
    let asks = lines.filter(|l| {
        let first = ["probe", "switch", "if", "| hit"];
        first.iter().any(|f| l.starts_with(f)) || l.contains("ArrayLen") || l.contains("call")
    });
    let short = |l: &str| {
        let l = l.replace("let ", "");
        match l.split_once(" = ") {
            Some((_, rhs)) => rhs.to_string(),
            None => l,
        }
    };
    asks.map(short).collect()
}

/// How `lines` start.
fn starts(lines: &[String]) -> Vec<&str> {
    let first = |l: &String| l.find(['(', '%']).map_or(l.len(), |at| at);
    lines.iter().map(|l| l[..first(l)].trim()).collect()
}

const HIT: &str = "| hit";
const CONS: &str = "probe ArrayUncons";

fn split(len: u32, more: bool, from: u32) -> String {
    format!("probe ArraySplit {{ len: {len}, more: {more}, from: {from} }}")
}

/// One probe finds each element that an arm names, and its tail, and nothing else.
#[test]
fn what_an_arm_names_is_found_at_once() {
    let core = printed(|w| {
        let (a, c, t) = (w.name(), w.name(), w.name());
        let (a, c) = (TypedPat::Bind(a), TypedPat::Bind(c));
        let items = vec![a, wild(), c];
        vec![(array(items, PatRest::Bind(t)), false), (wild(), false)]
    });
    let lines = asks(&core);
    assert_eq!(lines[0], format!("{}(%0)", split(3, true, 0)), "{core}");
    assert_eq!(lines[1], "| hit(%1:0, _, %2:0, %3:0) ->", "{core}");
    assert_eq!(core.matches("probe").count(), 1, "{core}");
    // One that nothing reads is not found, and neither is a tail of nothing.
    let core = printed(|w| {
        let (a, t) = (w.name(), w.name());
        w.unread.extend([a.id.0, t.id.0]);
        let items = vec![TypedPat::Bind(a)];
        vec![(array(items, PatRest::Bind(t)), false), (wild(), false)]
    });
    assert_eq!(asks(&core)[1], "| hit(_, _) ->", "{core}");
}

/// An arm that asks something of an element takes its tail only once that holds, and
/// nothing is asked of the array again.
#[test]
fn a_tail_is_taken_once_nothing_is_left_to_ask() {
    let core = printed(|w| {
        let (x, t) = (w.name(), w.name());
        let empty = array(Vec::new(), PatRest::None);
        let first = w.ctor(A, TypedPat::Bind(x));
        let cons = array(vec![first], PatRest::Bind(t));
        let other = array(vec![wild()], PatRest::Discard);
        vec![(empty, false), (cons, false), (other, false)]
    });
    let lines = asks(&core);
    let got = starts(&lines);
    let got: Vec<&str> = got.into_iter().filter(|l| *l != HIT).collect();
    let want = [
        CONS,
        "switch",
        CONS,
        "tail call fn#2001",
        "tail call fn#2002",
    ];
    assert_eq!(got[..5], want, "{core}");
    // The second finds the tail alone, and cannot fail.
    let found: Vec<String> = asks(&core)
        .into_iter()
        .filter(|l| l.contains("hit"))
        .collect();
    assert!(found[0].ends_with(", _) ->"), "{core}");
    assert!(found[1].starts_with("| hit(_, "), "{core}");
    assert!(core.contains("no arm"), "{core}");
}

/// Where the tail of an arm with a guard is taken, as what is asked and called in order,
/// with the arms that `make` writes. `late` says whether it is behind the guard.
fn behind_the_guard(late: bool, again: bool, make: impl Fn(&mut Write) -> Vec<(TypedPat, bool)>) {
    let mut w = Write {
        again,
        ..Write::default()
    };
    let arms = make(&mut w);
    let core = w.lowered(arms).to_string();
    let lines = asks(&core);
    let lines: Vec<&str> = starts(&lines).into_iter().filter(|l| *l != HIT).collect();
    let guard = lines.iter().position(|l| l.starts_with("call fn#"));
    let guard = guard.expect("a guard");
    let holds = lines.iter().position(|l| l.starts_with("tail call"));
    let cuts = |range: &[&str]| range.iter().filter(|l| l.starts_with("probe")).count();
    let behind = cuts(&lines[guard..holds.expect("a body")]);
    assert_eq!(behind, usize::from(late), "{core}");
}

/// A tail that a guard does not read is taken only where it has held, when the array is
/// read again where it fails. When nothing reads that again the tail is taken first, so
/// that the array is not held while the guard runs.
#[test]
fn a_tail_waits_for_a_guard_where_the_array_is_read_again() {
    let cons = |w: &mut Write, guarded: bool| {
        let (x, t) = (w.name(), w.name());
        if !guarded {
            w.only_body.insert(t.id.0);
        }
        array(vec![TypedPat::Bind(x)], PatRest::Bind(t))
    };
    let two = |w: &mut Write| {
        let (a, b) = (w.name(), w.name());
        array(vec![TypedPat::Bind(a), TypedPat::Bind(b)], PatRest::Discard)
    };
    // The next arm reads another element of it.
    behind_the_guard(true, false, |w| {
        vec![(cons(w, false), true), (two(w), false), (wild(), false)]
    });
    // Or the array itself, by a name of its own or by that of what is matched.
    behind_the_guard(true, false, |w| {
        let all = TypedPat::Bind(w.name());
        vec![(cons(w, false), true), (all, false)]
    });
    behind_the_guard(true, true, |w| {
        vec![(cons(w, false), true), (wild(), false)]
    });
    // The arm is behind others, whose tests are the rest of the tree.
    behind_the_guard(true, false, |w| {
        let first = array(Vec::new(), PatRest::None);
        vec![
            (first, false),
            (cons(w, false), true),
            (two(w), false),
            (wild(), false),
        ]
    });
    // The guard reads the tail.
    behind_the_guard(false, false, |w| {
        vec![(cons(w, true), true), (two(w), false), (wild(), false)]
    });
    // Nothing else reads anything of the array.
    behind_the_guard(false, false, |w| {
        vec![(cons(w, false), true), (wild(), false)]
    });
    // What the next arm reads is the same tail, or an element that is found already.
    behind_the_guard(false, false, |w| {
        vec![
            (cons(w, false), true),
            (cons(w, false), false),
            (wild(), false),
        ]
    });
    behind_the_guard(false, false, |w| {
        let x = TypedPat::Bind(w.name());
        let first = array(vec![x], PatRest::Discard);
        vec![(cons(w, false), true), (first, false), (wild(), false)]
    });
    // The next arm takes everything, and one behind it is not reached.
    behind_the_guard(false, false, |w| {
        vec![(cons(w, false), true), (wild(), false), (two(w), false)]
    });
}

/// Takes bits off the front of a binary, as many as a size says that reads `names`.
fn sized(w: &mut Write, names: &[TypedBind]) -> TypedPat {
    let args = names.iter().map(|b| TypedExpr::Var {
        ty: b.ty,
        place: ValueRef::Local(b.id),
    });
    let bits = TypedExpr::Call {
        ty: INT,
        callee: TypedCallee::Known(crate::FuncIdx(oracle::SIZES)),
        args: args.collect(),
    };
    let value = wild();
    TypedPat::Bin {
        ty: INT,
        zero: w.constant(Const::Int(0)),
        segs: vec![TypedBinPatSeg::Int { value, bits }],
        rest: PatRest::Discard,
    }
}

/// A size in an or-pattern may read the tail of an array that is read again.
#[test]
fn a_size_in_an_or_pattern_reads_a_tail() {
    let mut w = Write {
        no_copies: true,
        ..Write::default()
    };
    let (x, t, y) = (w.name(), w.name(), w.name());
    let cons = array(vec![TypedPat::Bind(x)], PatRest::Bind(t));
    let either = or(vec![sized(&mut w, &[t]), sized(&mut w, &[t, x])]);
    let two = array(vec![wild(), TypedPat::Bind(y)], PatRest::Discard);
    let arms = vec![
        (tuple(vec![cons, either]), false),
        (tuple(vec![two, wild()]), false),
        (wild(), false),
    ];
    let values = (0..4).map(|n| {
        let items = (0..n).map(Val::Int).collect();
        Val::Tuple(vec![Val::Array(items), Val::Bits(vec![true; 9])])
    });
    finds(&w, arms, &values.collect::<Vec<_>>());
}

/// A size is read in front of an element that is known not to fit, as it is in front of
/// one that nothing is known of.
#[test]
fn a_size_is_read_in_front_of_what_is_known_to_fail() {
    let mut w = Write::default();
    let (two, three) = (w.int(2), w.int(3));
    let bytes = sized(&mut w, &[]);
    let guarded = array(vec![wild(), two.clone()], PatRest::Discard);
    let dead = array(vec![bytes.clone(), three.clone(), wild()], PatRest::None);
    let arms = vec![(guarded, true), (dead, false), (wild(), false)];
    let bits = || Val::Bits(vec![true; 9]);
    let of = |n| Val::Array(vec![bits(), Val::Int(n), Val::Int(0)]);
    finds(&w, arms, &[of(2), of(3), of(4)]);

    let guarded = array(vec![tuple(vec![wild(), two])], PatRest::Discard);
    let dead = array(vec![tuple(vec![bytes, three]), wild()], PatRest::None);
    let arms = vec![(guarded, true), (dead, false), (wild(), false)];
    let of = |n| Val::Array(vec![Val::Tuple(vec![bits(), Val::Int(n)]), Val::Int(0)]);
    finds(&w, arms, &[of(2), of(3), of(4)]);
}

/// Which length is asked for first, and when the length itself is read.
#[test]
fn a_length_is_read_where_several_are_asked_for() {
    let none = |n| array(vec![wild(); n], PatRest::None);
    let more = |n| array(vec![wild(); n], PatRest::Discard);
    let plain = |pats: [TypedPat; 2]| {
        printed(|_| {
            let arms = pats.iter().map(|p| (p.clone(), false));
            arms.chain([(wild(), false)]).collect()
        })
    };
    let first = |core: &str| {
        let at = core.find("len: ");
        at.is_some_and(|at| core[at..].starts_with("len: 0"))
    };
    // Only one more element is asked for in front of fewer.
    let core = plain([none(0), more(1)]);
    assert!(!first(&core), "{core}");
    let core = plain([none(0), more(2)]);
    assert!(first(&core), "{core}");
    let core = plain([none(0), none(1)]);
    assert!(core.contains("ArrayLen"), "{core}");
    // One length that two arms ask for is asked once.
    let core = printed(|w| {
        let [one, two] = [1, 2].map(|n| w.int(n));
        let arms = [one, two].map(|n| (tuple(vec![none(2), n]), false));
        arms.into_iter().chain([(wild(), false)]).collect()
    });
    assert!(!core.contains("ArrayLen"), "{core}");
    // An array that nothing was asked of yet is held while a guard runs in any case.
    let core = printed(|w| {
        let guarded = tuple(vec![wild(), w.int(1)]);
        let empty = tuple(vec![none(0), wild()]);
        vec![(guarded, true), (empty, false), (wild(), false)]
    });
    assert!(!core.contains("ArrayLen"), "{core}");
}

/// A tail waits for a size too, as it does for a guard.
#[test]
fn a_tail_waits_for_a_size_where_the_array_is_read_again() {
    let order = |read_again: bool| {
        let core = printed(|w| {
            let (x, t) = (w.name(), w.name());
            let cons = array(vec![TypedPat::Bind(x)], PatRest::Bind(t));
            let bytes = sized(w, &[]);
            let (a, b) = (w.name(), w.name());
            let two = vec![TypedPat::Bind(a), TypedPat::Bind(b)];
            let two = tuple(vec![array(two, PatRest::Discard), wild()]);
            let mut arms = vec![(tuple(vec![cons, bytes]), false)];
            if read_again {
                arms.push((two, false));
            }
            arms.push((wild(), false));
            arms
        });
        let (size, body) = (core.find("call fn#1000"), core.find("call fn#2000"));
        let (size, body) = (size.expect("a size"), body.expect("a body"));
        let tail = core[size..body].contains("probe ArrayUncons");
        (tail, core)
    };
    let (late, core) = order(true);
    assert!(late, "{core}");
    let (late, core) = order(false);
    assert!(!late, "{core}");
}

/// Where only the length of an array is still asked for behind a guard, it is read in front
/// of that, and the array need not be held while the guard runs.
#[test]
fn a_length_is_read_in_front_of_a_guard_where_nothing_else_of_the_array_is() {
    let gives_up = |core: &str| {
        let drop = core.find("drop %0").expect("it is given up");
        let guard = core.find("call fn#0").expect("a guard");
        let length = core.find("ArrayLen").is_some_and(|at| at < guard);
        (length, drop < guard)
    };
    let unread = |n: usize, rest| array(vec![wild(); n], rest);
    let tried = |behind: &dyn Fn(&mut Write) -> Vec<TypedPat>| {
        let mut w = Write::default();
        let x = TypedPat::Bind(w.name());
        let first = array(vec![x], PatRest::Discard);
        let mut arms = vec![(first, true)];
        arms.extend(behind(&mut w).into_iter().map(|p| (p, false)));
        arms.push((wild(), false));
        gives_up(&w.dropped(arms).to_string())
    };
    assert_eq!(tried(&|_| vec![unread(3, PatRest::Discard)]), (true, true));
    assert_eq!(tried(&|_| vec![unread(3, PatRest::None)]), (true, true));
    let both = |_: &mut Write| vec![unread(2, PatRest::None), unread(4, PatRest::Discard)];
    assert_eq!(tried(&both), (true, true));
    let either = |w: &mut Write| {
        w.no_copies = true;
        vec![or(both(&mut Write::default()))]
    };
    assert_eq!(tried(&either), (true, true));
    // More arms are behind than are looked at.
    let many = |w: &mut Write| {
        let elements = (0..40).map(|_| array(vec![wild()], PatRest::Discard));
        let mut arms: Vec<(TypedPat, bool)> = elements.map(|p| (p, true)).collect();
        arms.push((unread(2, PatRest::None), false));
        arms.push((wild(), false));
        w.dropped(arms).to_string()
    };
    assert_eq!(gives_up(&many(&mut Write::default())), (true, true));
    // What has names that nothing reads is the same.
    let names = |w: &mut Write| {
        let (y, t) = (w.name(), w.name());
        w.unread.extend([y.id.0, t.id.0]);
        vec![array(vec![wild(), TypedPat::Bind(y)], PatRest::Bind(t))]
    };
    assert_eq!(tried(&names), (true, true));
    // Nothing is asked for, or something else is read too.
    assert_eq!(tried(&|_| Vec::new()), (false, true));
    let element = |w: &mut Write| {
        let items = vec![wild(), TypedPat::Bind(w.name())];
        vec![unread(3, PatRest::None), array(items, PatRest::None)]
    };
    assert_eq!(tried(&element), (false, false));
    let tail = |w: &mut Write| vec![array(vec![wild(), wild()], PatRest::Bind(w.name()))];
    assert_eq!(tried(&tail), (false, false));
    // It is read in front of a range that is asked for in front of the guard.
    let mut w = Write::default();
    let (lo, hi) = (w.constant(Const::Int(2)), w.constant(Const::Int(5)));
    let range = TypedPat::Range { ty: INT, lo, hi };
    let first = array(vec![range], PatRest::Discard);
    let third = unread(3, PatRest::None);
    let arms = vec![(first, true), (third, false), (wild(), false)];
    assert_eq!(gives_up(&w.dropped(arms).to_string()), (true, true));
    // What is found of it is known not to fit, so nothing is asked of it behind that.
    for either in [false, true] {
        let mut w = Write::default();
        let first = array(vec![w.int(1)], PatRest::Discard);
        let second = array(vec![wild()], PatRest::Discard);
        let one = w.int(1);
        let never = match either {
            true => or(vec![one.clone(), one]),
            false => one,
        };
        let items = vec![never, TypedPat::Bind(w.name()), wild(), wild()];
        let third = array(items, PatRest::None);
        let core = w.dropped(vec![
            (first, false),
            (second, true),
            (third, false),
            (wild(), false),
        ]);
        let core = core.to_string();
        let guard = core.find("call fn#1").expect("a guard");
        assert!(core[..guard].contains("drop %0"), "{core}");
        assert!(!core.contains("ArrayLen"), "{core}");
    }
    // The length is read before the tail is taken, which is then in place.
    for named in [false, true] {
        let mut w = Write::default();
        let (x, t) = (w.name(), w.name());
        let cons = array(vec![TypedPat::Bind(x)], PatRest::Bind(t));
        let mut second = vec![wild(); 3];
        if named {
            second[0] = TypedPat::Bind(w.name());
        }
        let second = array(second, PatRest::None);
        let core = w.dropped(vec![(cons, true), (second, false), (wild(), false)]);
        let core = core.to_string();
        let length = core.find("ArrayLen").expect("it is read");
        let tail = core.find("| hit(_, ").expect("the tail");
        assert!(length < tail, "{core}");
        assert_eq!(core.matches("ArrayLen").count(), 1, "{core}");
    }
}

/// A tail that was put off behind a guard is taken in front of a later one when nothing
/// else of its array is read any more, so that the array is held no longer.
#[test]
fn a_tail_that_was_put_off_is_taken_once_nothing_else_is_read() {
    let cons = |w: &mut Write| {
        let t = w.name();
        w.only_body.insert(t.id.0);
        array(vec![wild()], PatRest::Bind(t))
    };
    let gives_up = |guard: u32, arms: &dyn Fn(&mut Write) -> Vec<(TypedPat, bool)>| {
        let mut w = Write::default();
        let arms = arms(&mut w);
        let core = w.dropped(arms).to_string();
        let first = core.find("call fn#0").expect("the first guard");
        let guard = core.find(&format!("call fn#{guard}(")).expect("a guard");
        assert!(first < guard, "{core}");
        // It was put off, and is not any more.
        assert!(!core[..first].contains("drop %0"), "{core}");
        let (_, failed) = core[first..guard].split_once("else").expect("it fails");
        let taken = failed.rsplit_once("ArrayUncons(%0)");
        taken.map(|(_, taken)| taken.contains("drop %0"))
    };
    assert_eq!(
        Some(true),
        gives_up(1, &|w| {
            let second = array(vec![wild(), TypedPat::Bind(w.name())], PatRest::None);
            let (first, third) = (cons(w), cons(w));
            vec![
                (first, true),
                (second, true),
                (third, false),
                (wild(), false),
            ]
        })
    );
    // An element is still read.
    assert_eq!(
        None,
        gives_up(1, &|w| {
            let second = array(vec![wild(), TypedPat::Bind(w.name())], PatRest::None);
            let last = array(vec![TypedPat::Bind(w.name())], PatRest::Discard);
            let (first, third) = (cons(w), cons(w));
            let guarded = [first, second, third].map(|p| (p, true));
            let mut arms: Vec<(TypedPat, bool)> = guarded.into();
            arms.extend([(last, false), (wild(), false)]);
            arms
        })
    );
    // Nothing names the tail any more.
    assert_eq!(
        None,
        gives_up(1, &|w| {
            let second = array(vec![wild(), TypedPat::Bind(w.name())], PatRest::None);
            vec![(cons(w), true), (second, true), (wild(), false)]
        })
    );
    // Its length is asked for too.
    assert_eq!(
        Some(true),
        gives_up(2, &|w| {
            let second = array(vec![TypedPat::Bind(w.name()); 3], PatRest::Discard);
            let third = array(vec![wild()], PatRest::Discard);
            let fourth = array(vec![wild()], PatRest::None);
            let (first, fifth) = (cons(w), cons(w));
            vec![
                (first, true),
                (second, true),
                (third, true),
                (fourth, true),
                (fifth, false),
                (wild(), false),
            ]
        })
    );
}

/// A length and a tail that was put off are all that is read of the array behind the
/// second guard. Neither holds the array for the other: both are read in front of it.
#[test]
fn a_length_and_a_tail_that_was_put_off_hold_no_array() {
    let mut w = Write::default();
    let (t, u, b) = (w.name(), w.name(), w.name());
    w.only_body.extend([t.id.0, u.id.0]);
    let arms = vec![
        (array(vec![wild()], PatRest::Bind(t)), true),
        (
            array(vec![wild(), TypedPat::Bind(b)], PatRest::Discard),
            true,
        ),
        (array(vec![wild(); 3], PatRest::None), false),
        (array(vec![wild()], PatRest::Bind(u)), false),
        (wild(), false),
    ];
    let core = w.dropped(arms).to_string();
    let first = core.find("call fn#0").expect("the first guard");
    let second = core.find("call fn#1(").expect("the second");
    let between = &core[first..second];
    for read in ["ArrayLen(%0)", "ArrayUncons(%0)", "drop %0"] {
        assert!(between.contains(read), "{read}\n{core}");
    }
}

/// What is asked of an array in an or-pattern behind a guard is read behind it, whether
/// that pattern is one check or the tree still has to find which alternative to ask.
#[test]
fn an_or_pattern_behind_a_guard_reads_the_array() {
    let waits_behind = |item: TypedPat, other, guard, second: &dyn Fn(&mut Write) -> TypedPat| {
        let mut w = Write::default();
        let t = w.name();
        w.only_body.insert(t.id.0);
        let first = tuple(vec![array(vec![item], PatRest::Bind(t)), other]);
        let second = second(&mut w);
        let core = w
            .lowered(vec![(first, true), (second, guard), (wild(), false)])
            .to_string();
        let guard = core.find("call fn#0").expect("the guard");
        let tail = core.find("| hit(_, %").expect("the tail alone");
        guard < tail
    };
    let waits =
        |second: &dyn Fn(&mut Write) -> TypedPat| waits_behind(wild(), wild(), false, second);
    // Nothing reads the array again.
    assert!(!waits(&|w| tuple(vec![wild(), w.plain(A)])));
    // One of several constructors, or of several lengths.
    let of = |w: &mut Write, alts: Vec<TypedPat>| {
        let first = or(alts);
        let _ = w;
        tuple(vec![array(vec![first], PatRest::Discard), wild()])
    };
    assert!(waits(&|w| {
        let alts = vec![w.plain(A), w.plain(C)];
        of(w, alts)
    }));
    let lengths = |w: &mut Write, names: bool| {
        let item = match names {
            true => TypedPat::Bind(w.name()),
            false => wild(),
        };
        let long = array(vec![wild(), wild(), item.clone()], PatRest::None);
        let short = array(vec![item.clone(), item], PatRest::None);
        tuple(vec![or(vec![short, long]), wild()])
    };
    assert!(waits(&|w| lengths(w, true)));
    // Nothing behind an arm that takes every value is tried.
    let mut w = Write::default();
    let (t, all) = (w.name(), w.name());
    w.only_body.insert(t.id.0);
    let arms = vec![
        (array(vec![wild()], PatRest::Bind(t)), true),
        (array(vec![wild()], PatRest::Discard), false),
        (TypedPat::Bind(all), false),
    ];
    let core = w.lowered(arms).to_string();
    let guard = core.find("call fn#0").expect("the guard");
    let tail = core.find("| hit(_, %").expect("the tail alone");
    assert!(tail < guard, "{core}");
    // An alternative that is known not to fit reads nothing, and neither does anything
    // else of an arm that has no other.
    let w = Write::default();
    let alts = |first: TypedPat| {
        move |w: &mut Write| {
            let items = array(vec![first.clone(), w.plain(A)], PatRest::None);
            let item = tuple(vec![items, w.plain(C)]);
            or(vec![item, tuple(vec![wild(), w.plain(D)])])
        }
    };
    let known = w.plain(C);
    assert!(waits_behind(known.clone(), wild(), true, &alts(wild())));
    assert!(waits_behind(
        known.clone(),
        wild(),
        true,
        &alts(known.clone())
    ));
    assert!(!waits_behind(
        known.clone(),
        wild(),
        true,
        &alts(w.plain(D))
    ));
    let (empty, two) = (
        array(vec![], PatRest::None),
        array(vec![wild(); 2], PatRest::None),
    );
    let one = array(vec![wild()], PatRest::None);
    let second = |w: &mut Write, alt: &TypedPat| {
        let item = TypedPat::Bind(w.name());
        let items = array(vec![wild(), item], PatRest::Discard);
        tuple(vec![items, or(vec![empty.clone(), alt.clone()])])
    };
    assert!(!waits_behind(wild(), one.clone(), true, &|w| second(
        w, &two
    )));
    assert!(waits_behind(wild(), one.clone(), true, &|w| second(
        w, &one
    )));
}

/// An or-pattern of an element that is found, behind a guard, reads the array that the
/// element is, though the length of what holds it is not known.
#[test]
fn an_or_pattern_of_a_found_element_reads_it() {
    let waits = |inner: &dyn Fn(&mut Write) -> TypedPat| {
        let mut w = Write::default();
        let t = w.name();
        w.only_body.insert(t.id.0);
        let first = array(
            vec![array(vec![wild()], PatRest::Bind(t))],
            PatRest::Discard,
        );
        let inner = inner(&mut w);
        let second = array(vec![inner, wild(), wild()], PatRest::None);
        let arms = vec![(first, true), (second, false), (wild(), false)];
        let core = w.lowered(arms).to_string();
        let guard = core.find("call fn#0").expect("the guard");
        let tail = core.rfind("| hit(_, %").expect("the tail alone");
        guard < tail
    };
    assert!(!waits(&|_| wild()));
    assert!(waits(&|w| {
        let short = array(vec![wild(), w.plain(A)], PatRest::None);
        let long = array(vec![wild(), wild(), w.plain(C)], PatRest::None);
        or(vec![short, long])
    }));
}

/// A tail that was put off is taken in front of an or-pattern that asks for a size too.
#[test]
fn a_tail_that_was_put_off_is_taken_in_front_of_a_size() {
    let mut w = Write::default();
    let (t, b, u) = (w.name(), w.name(), w.name());
    w.only_body.extend([t.id.0, u.id.0]);
    let sizes = or(vec![sized(&mut w, &[]), sized(&mut w, &[])]);
    let second = array(vec![wild(), TypedPat::Bind(b)], PatRest::Discard);
    let arms = vec![
        (
            tuple(vec![array(vec![wild()], PatRest::Bind(t)), wild()]),
            true,
        ),
        (tuple(vec![second, sizes]), false),
        (
            tuple(vec![array(vec![wild()], PatRest::Bind(u)), wild()]),
            false,
        ),
        (wild(), false),
    ];
    let core = w.dropped(arms).to_string();
    let first = core.find("call fn#0").expect("the guard");
    let size = core.find("call fn#1000").expect("a size");
    assert!(first < size, "{core}");
    let (_, failed) = core[first..size].split_once("else").expect("it fails");
    assert!(failed.contains("| hit(_, %"), "{core}");
}

/// Everything else of a tuple that an arm needs is read in front of the tail of an array
/// in it, so that the tuple does not hold the array when its tail is taken.
#[test]
fn a_tail_is_taken_behind_what_else_is_read() {
    let mut w = Write::default();
    let names = [w.name(), w.name(), w.name(), w.name(), w.name()];
    let [a, at, b, bt, n] = names;
    let one = array(vec![TypedPat::Bind(a)], PatRest::Bind(at));
    let two = array(vec![TypedPat::Bind(b)], PatRest::Bind(bt));
    let all = tuple(vec![one, two, TypedPat::Bind(n)]);
    let core = w.dropped(vec![(all, false), (wild(), false)]).to_string();
    let fields = core.rfind("TupleField").expect("it reads them");
    let drop = core.find("drop %0").expect("the tuple is given up");
    let tails: Vec<usize> = core.match_indices("| hit(_, ").map(|(at, _)| at).collect();
    assert_eq!(tails.len(), 2, "{core}");
    assert!(fields < tails[0] && drop < tails[0], "{core}");
}

/// Where several lengths are asked for, the length is asked for once, and what it is then
/// known to be is not asked again.
#[test]
fn several_lengths_are_one_question() {
    let core = printed(|w| {
        let names: Vec<TypedPat> = (0..6).map(|_| TypedPat::Bind(w.name())).collect();
        let long = |n: usize| array(names[..n].to_vec(), PatRest::None);
        vec![
            (long(1), false),
            (long(2), false),
            (long(3), false),
            (wild(), false),
        ]
    });
    assert_eq!(core.matches("ArrayLen").count(), 1, "{core}");
    assert_eq!(core.matches("switch").count(), 3, "{core}");
    let lines = asks(&core);
    let probes = lines.iter().filter(|l| l.starts_with("probe")).count();
    assert_eq!(probes, 3, "{core}");
    // After `[]`, `[x]` and `[x, y]` an array has three or more.
    let core = printed(|w| {
        let (x, t) = (w.name(), w.name());
        let long = |n: usize| array(vec![wild(); n], PatRest::None);
        let cons = array(vec![wild(), wild(), TypedPat::Bind(x)], PatRest::Bind(t));
        vec![
            (long(0), false),
            (long(1), false),
            (long(2), false),
            (cons, false),
        ]
    });
    assert_eq!(core.matches("probe").count(), 1, "{core}");
    // Of two lengths one question tells.
    let core = printed(|w| {
        let (a, b) = (w.name(), w.name());
        let two = array(vec![wild(), TypedPat::Bind(a)], PatRest::Discard);
        let one = array(vec![TypedPat::Bind(b)], PatRest::None);
        let none = array(Vec::new(), PatRest::None);
        vec![(two, false), (one, false), (none, false)]
    });
    assert!(!core.contains("ArrayLen"), "{core}");
    assert_eq!(core.matches("probe").count(), 2, "{core}");
}

/// A long pattern is read a few elements at a time, so that it needs no more registers
/// than that, and each of them once. The first that is asked of something is read alone,
/// because the others are for nothing when it does not fit.
#[test]
fn a_long_pattern_is_read_a_few_elements_at_a_time() {
    let core = printed(|w| {
        let items = (0..30).map(|i| w.int(i)).collect();
        vec![(array(items, PatRest::None), false), (wild(), false)]
    });
    let lines = asks(&core);
    let hits: Vec<&String> = lines.iter().filter(|l| l.starts_with(HIT)).collect();
    let found: Vec<usize> = hits.iter().map(|l| l.matches('%').count()).collect();
    assert_eq!(found, [1, 8, 8, 8, 5], "{core}");
    let from = |i: u32| format!("{}(%0)", split(8 * i + 9, true, 8 * i + 1));
    assert_eq!(lines.iter().filter(|l| **l == from(1)).count(), 1, "{core}");
}

/// The element that is asked of next is read alone with the length, and what else the row
/// needs is read together behind that.
#[test]
fn the_element_that_is_asked_of_is_read_alone() {
    let hits = |core: &str| -> Vec<usize> {
        let lines = asks(core);
        let hits = lines.iter().filter(|l| l.starts_with(HIT));
        hits.map(|l| l.matches('%').count()).collect()
    };
    // The two that are named are read where the row fits.
    let core = printed(|w| {
        let mut items = vec![TypedPat::Bind(w.name()), w.int(7)];
        items.push(TypedPat::Bind(w.name()));
        vec![(array(items, PatRest::None), false), (wild(), false)]
    });
    assert_eq!(hits(&core), [1, 2], "{core}");
    // So it is where the length has been read by itself.
    let core = printed(|w| {
        let two: Vec<_> = (1..3).map(|n| w.int(n)).collect();
        let three: Vec<_> = (1..4).map(|n| w.int(n)).collect();
        let [two, three] = [two, three].map(|all| array(all, PatRest::None));
        vec![(two, false), (three, false), (wild(), false)]
    });
    assert!(core.contains("ArrayLen"), "{core}");
    assert_eq!(hits(&core), [1, 1, 1, 2], "{core}");
    // With something else to ask next, what is only named is read with the length.
    let core = printed(|w| {
        let items = (0..2).map(|_| TypedPat::Bind(w.name())).collect();
        let first = array(items, PatRest::Discard);
        let one = tuple(vec![first, w.plain(A)]);
        let two = tuple(vec![array(vec![wild(); 3], PatRest::None), wild()]);
        vec![(one, false), (two, false), (wild(), false)]
    });
    assert_eq!(hits(&core)[0], 2, "{core}");
    // With nothing asked of them, all are read at once.
    let core = printed(|w| {
        let items = (0..3).map(|_| TypedPat::Bind(w.name())).collect();
        vec![(array(items, PatRest::None), false), (wild(), false)]
    });
    assert_eq!(hits(&core), [3], "{core}");
}

/// An element that the first row needs is read with the test of the length, and one that
/// only another needs is read where that is tried.
#[test]
fn an_element_is_read_where_it_is_needed() {
    let core = printed(|w| {
        let (x, y) = (w.name(), w.name());
        let one = array(vec![w.plain(A), wild()], PatRest::Discard);
        let two = array(vec![wild(), TypedPat::Bind(y)], PatRest::Bind(x));
        vec![(one, false), (two, false), (wild(), false)]
    });
    let lines = asks(&core);
    let hits: Vec<&String> = lines.iter().filter(|l| l.starts_with(HIT)).collect();
    assert_eq!(hits[0], "| hit(%1:1, _) ->", "{core}");
    assert_eq!(hits[1], "| hit(%2:0, %3:0) ->", "{core}");
    assert!(
        lines.contains(&format!("{}(%0)", split(2, true, 1))),
        "{core}"
    );
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

/// Where every constructor is one case there is nothing to ask. The rows that there was
/// no allowance to put in the case come after it all the same.
#[test]
fn a_case_for_every_constructor_is_no_question() {
    let mut w = Write::default();
    let many = or((0..200).map(|n| w.int(n)).collect());
    let all = || or([A, B, C, D].map(|v| w.plain(v)).into());
    let core = w.lowered(vec![(all(), false)]).to_string();
    assert!(!core.contains("switch"), "{core}");
    let three = or([A, B, C].map(|v| w.plain(v)).into());
    let core = w.lowered(vec![(three, false), (wild(), false)]).to_string();
    assert!(core.contains("switch"), "{core}");
    let arms = vec![
        (tuple(vec![all(), wild()]), true),
        (tuple(vec![wild(), many]), true),
        (wild(), false),
    ];
    let value = |v: u16, n: i128| {
        let of = Val::Ctor(T, v, vec![Val::Int(1); usize::from(v < C)]);
        Val::Tuple(vec![of, Val::Int(n)])
    };
    let values = [A, D].map(|v| [0, 199, 200].map(|n| value(v, n)));
    finds(&w, arms, &values.concat());
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
