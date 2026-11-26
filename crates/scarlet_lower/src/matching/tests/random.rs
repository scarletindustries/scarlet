//! Matches made at random, of random types.

use std::collections::{BTreeMap, HashMap};

use scarlet_ir::TypeId;
use scarlet_ir::core_ir::{Const, ConstId, FuncIdx, ProbeOp, VariantRef};
use scarlet_ir::rty::{PrimIds, RTy, Repr, ResolvedPool};

use super::oracle::{BODIES, Dice, SIZES};
use crate::typed_ir::{
    BindingId, Gives, PatRest, Shapes, TempTys, TypedArm, TypedBinPatSeg, TypedBind, TypedCallee,
    TypedExpr, TypedFn, TypedPat, ValueRef, pat_binds,
};

/// The prelude's Bool and Nil, which `immediates` makes plain values of.
pub(super) const BOOL: TypeId = TypeId(901);
pub(super) const NIL: TypeId = TypeId(902);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum Ty {
    Int,
    Float,
    Text,
    Bits,
    Named(TypeId),
    Tuple(Vec<Ty>),
    Array(Box<Ty>),
}

/// How large and of what kind a match is.
#[derive(Clone, Copy)]
pub(super) struct Kind {
    pub(super) arms: usize,
    pub(super) depth: usize,
    /// One in this many patterns is an or-pattern.
    pub(super) ors: usize,
    /// One in this many arms has a guard.
    pub(super) guards: usize,
    /// One in this many patterns of a binary asks for a size by a call.
    pub(super) sizes: usize,
    /// One in this many matches is on a probe.
    pub(super) probes: usize,
}

/// Types such as a probe's value is of: the first constructor holds something, and the
/// second nothing or a Nil.
pub(super) const OPTION: TypeId = TypeId(20);
pub(super) const RESULT: TypeId = TypeId(21);

/// One match, as the function that is nothing else.
pub(super) struct Match {
    pub(super) f: TypedFn,
    pub(super) consts: Vec<Const>,
    pub(super) shapes: Shapes,
    pub(super) pool: ResolvedPool,
    pub(super) temps: TempTys,
}

struct Make<'d> {
    dice: &'d mut Dice,
    kind: Kind,
    /// The fields of each constructor of each type.
    types: BTreeMap<TypeId, Vec<Vec<Ty>>>,
    consts: Vec<Const>,
    const_ids: HashMap<Const, ConstId>,
    pool: ResolvedPool,
    rtys: HashMap<Ty, RTy>,
    binds: usize,
    /// How many sizes are asked for by a call.
    sizes: u32,
    int: RTy,
    bool: RTy,
    string: RTy,
    binary: RTy,
    int_pair: RTy,
}

/// How a pattern names things. The alternatives of an or-pattern after the first name
/// what the first does.
struct Names {
    /// What the first alternative named, by type, and is not named in this one yet.
    again: Option<Vec<(RTy, TypedBind)>>,
    /// What has been named, which a size may read.
    scope: Vec<TypedBind>,
}

/// A match of `kind`. With `on`, it is on a value of that type.
pub(super) fn make(dice: &mut Dice, kind: Kind) -> Match {
    let mut pool = ResolvedPool::new(PrimIds::default());
    let prims = pool.prims();
    let mut con = |id, repr| pool.mk_con(id, repr, &[]).unwrap();
    let int = con(prims.int, Repr::Counted);
    let bool = con(BOOL, Repr::Word);
    let float = con(prims.float, Repr::Word);
    let string = con(prims.string, Repr::Counted);
    let binary = con(TypeId(900), Repr::Counted);
    let nil = con(NIL, Repr::Word);
    let int_pair = pool.mk_tuple(&[int, int]).unwrap();
    let rtys = HashMap::from([
        (Ty::Int, int),
        (Ty::Float, float),
        (Ty::Text, string),
        (Ty::Bits, binary),
        (Ty::Named(BOOL), bool),
        (Ty::Named(NIL), nil),
    ]);
    let types = BTreeMap::from([(BOOL, vec![Vec::new(); 2]), (NIL, vec![Vec::new()])]);
    let mut make = Make {
        dice,
        kind,
        types,
        consts: Vec::new(),
        const_ids: HashMap::new(),
        pool,
        rtys,
        binds: 0,
        sizes: 0,
        int,
        bool,
        string,
        binary,
        int_pair,
    };
    make.name_types();
    let asked = make.dice.one_in(kind.probes).then(|| make.asked());
    let ty = match &asked {
        Some((ty, _)) => ty.clone(),
        None => make.ty(2),
    };
    let scrut = make.fresh(&ty);
    let mut arms: Vec<TypedArm> = Vec::new();
    for i in 0..u32::try_from(make.dice.below(kind.arms) + 1).unwrap() {
        // Sometimes an arm whose pattern has been there.
        let mut pat = make.top(&ty);
        if !arms.is_empty() && make.dice.one_in(12) {
            let at = make.dice.below(arms.len());
            pat = make.renamed(&arms[at].pat);
        }
        arms.push(make.arm(i, pat));
    }
    // An arm for everything, most of the time.
    if !make.dice.one_in(6) {
        let pat = match make.dice.one_in(2) {
            true => TypedPat::Wild { ty: scrut.ty },
            false => TypedPat::Bind(make.fresh(&ty)),
        };
        let i = u32::try_from(arms.len()).unwrap();
        arms.push(make.arm(i, pat));
        if make.dice.one_in(4) {
            let pat = make.top(&ty);
            arms.push(make.arm(i + 1, pat));
        }
    }
    let var = |b: &TypedBind| TypedExpr::Var {
        ty: b.ty,
        place: ValueRef::Local(b.id),
    };
    let on = match &asked {
        Some((_, found)) => probe(&make, var(&scrut), *found),
        None => var(&scrut),
    };
    let int = make.int;
    let on_it = |scrut| TypedExpr::Match {
        ty: int,
        scrut: Box::new(scrut),
        arms,
    };
    let body = match asked.is_some() && make.dice.one_in(3) {
        true => {
            let bind = make.fresh(&ty);
            TypedExpr::Let {
                ty: make.int,
                bind,
                init: Box::new(on),
                body: Box::new(on_it(var(&bind))),
            }
        }
        false => on_it(on),
    };
    let shapes = make.types.iter();
    let shapes = shapes.map(|(ty, all)| (*ty, all.iter().map(Vec::len).collect()));
    Match {
        f: TypedFn {
            params: vec![scrut],
            ret: make.int,
            body,
            binds: make.binds,
        },
        shapes: Shapes::new(shapes),
        consts: make.consts,
        temps: TempTys {
            int: make.int,
            bool: make.bool,
            string: make.string,
            binary: make.binary,
            int_pair: make.int_pair,
        },
        pool: make.pool,
    }
}

/// A probe of `of`, whose value is of the same type, that finds a `found`.
fn probe(make: &Make<'_>, of: TypedExpr, found: RTy) -> TypedExpr {
    let gives = match make.types.contains_key(&OPTION) {
        true => Gives::Option,
        false => Gives::Result {
            nil: make.rtys[&Ty::Named(NIL)],
        },
    };
    TypedExpr::Probe {
        ty: of.ty(),
        op: ProbeOp::MapGet,
        args: vec![of.clone(), of],
        found,
        gives,
    }
}

impl Make<'_> {
    /// The type of a probe's value, and of what it finds.
    fn asked(&mut self) -> (Ty, RTy) {
        let found = self.ty(2);
        let (id, miss) = match self.dice.one_in(2) {
            true => (OPTION, Vec::new()),
            false => (RESULT, vec![Ty::Named(NIL)]),
        };
        let of = self.rty(&found);
        let rty = self.pool.mk_con(id, Repr::Counted, &[of]).unwrap();
        self.rtys.insert(Ty::Named(id), rty);
        self.types.insert(id, vec![vec![found], miss]);
        (Ty::Named(id), of)
    }

    /// A few types, whose constructors hold values of each other.
    fn name_types(&mut self) {
        let ids: Vec<TypeId> = (10..14).map(TypeId).collect();
        for id in &ids {
            let variants = match self.dice.below(12) {
                0 => 1,
                1 => 2 + self.dice.below(30),
                2 | 3 => 2,
                _ => 1 + self.dice.below(6),
            };
            let mut all = Vec::with_capacity(variants);
            let bare = self.dice.one_in(6);
            for _ in 0..variants {
                let n = if bare { 0 } else { self.dice.below(4) };
                let fields = (0..n).map(|_| match self.dice.below(9) {
                    0 => Ty::Int,
                    1 => Ty::Named(BOOL),
                    2 => Ty::Text,
                    3 => Ty::Tuple(vec![Ty::Int, Ty::Named(*self.dice.pick(&ids))]),
                    4 => Ty::Array(Box::new(Ty::Named(*self.dice.pick(&ids)))),
                    5 => Ty::Named(NIL),
                    _ => Ty::Named(*self.dice.pick(&ids)),
                });
                all.push(fields.collect());
            }
            let word = all.iter().all(Vec::is_empty);
            let repr = if word { Repr::Word } else { Repr::Counted };
            let rty = self.pool.mk_con(*id, repr, &[]).unwrap();
            self.rtys.insert(Ty::Named(*id), rty);
            self.types.insert(*id, all);
        }
    }

    fn ty(&mut self, deep: usize) -> Ty {
        match self.dice.below(if deep == 0 { 7 } else { 14 }) {
            0 => Ty::Int,
            1 => Ty::Float,
            2 => Ty::Text,
            3 => Ty::Bits,
            4..=8 => {
                let id = 10 + i32::try_from(self.dice.below(4)).unwrap();
                Ty::Named(TypeId(id))
            }
            9 => Ty::Array(Box::new(self.ty(deep - 1))),
            _ => {
                let n = 1 + self.dice.below(4);
                Ty::Tuple((0..n).map(|_| self.ty(deep - 1)).collect())
            }
        }
    }

    fn rty(&mut self, ty: &Ty) -> RTy {
        if let Some(rty) = self.rtys.get(ty) {
            return *rty;
        }
        let rty = match ty {
            Ty::Tuple(elems) => {
                let elems: Vec<RTy> = elems.iter().map(|t| self.rty(t)).collect();
                self.pool.mk_tuple(&elems).unwrap()
            }
            Ty::Array(elem) => {
                let elem = self.rty(elem);
                let array = self.pool.prims().array;
                self.pool.mk_con(array, Repr::Counted, &[elem]).unwrap()
            }
            other => panic!("{other:?} has none"),
        };
        self.rtys.insert(ty.clone(), rty);
        rty
    }

    fn fresh(&mut self, ty: &Ty) -> TypedBind {
        let ty = self.rty(ty);
        self.binds += 1;
        TypedBind {
            id: BindingId(self.binds - 1),
            named: true,
            ty,
            global: None,
        }
    }

    fn constant(&mut self, c: Const) -> ConstId {
        if let Some(id) = self.const_ids.get(&c) {
            return *id;
        }
        let id = ConstId(u32::try_from(self.consts.len()).unwrap());
        self.consts.push(c.clone());
        self.const_ids.insert(c, id);
        id
    }

    fn call(&mut self, f: u32, args: &[TypedBind], ty: RTy) -> TypedExpr {
        let args = args.iter().map(|b| TypedExpr::Var {
            ty: b.ty,
            place: ValueRef::Local(b.id),
        });
        TypedExpr::Call {
            ty,
            callee: TypedCallee::Known(FuncIdx(f)),
            args: args.collect(),
        }
    }

    /// Some of the names that `pat` binds, each once.
    fn some(&mut self, pat: &TypedPat) -> Vec<TypedBind> {
        let all = pat_binds(pat);
        all.into_iter().filter(|_| !self.dice.one_in(3)).collect()
    }

    fn arm(&mut self, i: u32, pat: TypedPat) -> TypedArm {
        let guard = self.dice.one_in(self.kind.guards).then(|| {
            let args = self.some(&pat);
            let bool = self.bool;
            self.call(i, &args, bool)
        });
        let args = self.some(&pat);
        let int = self.int;
        let body = self.call(BODIES + i, &args, int);
        TypedArm { pat, guard, body }
    }

    fn top(&mut self, ty: &Ty) -> TypedPat {
        let mut names = Names {
            again: None,
            scope: Vec::new(),
        };
        self.pat(ty, self.kind.depth, &mut names)
    }

    /// `pat` with names and calls of its own.
    fn renamed(&mut self, pat: &TypedPat) -> TypedPat {
        let mut new: HashMap<BindingId, BindingId> = HashMap::new();
        for b in pat_binds(pat) {
            self.binds += 1;
            new.insert(b.id, BindingId(self.binds - 1));
        }
        let mut pat = pat.clone();
        rename(&mut pat, &new);
        pat
    }

    /// A name for a value of `ty`, or nothing.
    fn name(&mut self, ty: &Ty, names: &mut Names) -> TypedPat {
        let rty = self.rty(ty);
        let wild = TypedPat::Wild { ty: rty };
        if let Some(again) = &mut names.again {
            let Some(at) = again.iter().position(|(t, _)| *t == rty) else {
                return wild;
            };
            let (_, b) = again.swap_remove(at);
            names.scope.push(b);
            return TypedPat::Bind(b);
        }
        if self.dice.one_in(2) {
            return wild;
        }
        let b = self.fresh(ty);
        names.scope.push(b);
        TypedPat::Bind(b)
    }

    fn pat(&mut self, ty: &Ty, deep: usize, names: &mut Names) -> TypedPat {
        if deep == 0 || self.dice.one_in(5) {
            return self.name(ty, names);
        }
        if self.dice.one_in(self.kind.ors) {
            return self.or(ty, deep, names);
        }
        let rty = self.rty(ty);
        match ty {
            Ty::Int => match self.dice.below(6) {
                0 => {
                    let lo = self.small();
                    let hi = lo.saturating_add(i64::try_from(self.dice.below(6)).unwrap());
                    let hi = if self.dice.one_in(8) {
                        lo.saturating_sub(1)
                    } else {
                        hi
                    };
                    TypedPat::Range {
                        ty: rty,
                        lo: self.constant(Const::Int(lo)),
                        hi: self.constant(Const::Int(hi)),
                    }
                }
                _ => {
                    let n = self.small();
                    TypedPat::Lit {
                        ty: rty,
                        value: self.constant(Const::Int(n)),
                    }
                }
            },
            Ty::Float => {
                let all = [0.0, -0.0, 1.0, -1.0, 0.5, f64::MAX, 5e-324];
                let x = *self.dice.pick(&all);
                TypedPat::Lit {
                    ty: rty,
                    value: self.constant(Const::Float(x)),
                }
            }
            Ty::Text => {
                let all = ["", "a", "ab", "b", "\u{e9}"];
                let s = (*self.dice.pick(&all)).to_string();
                TypedPat::Lit {
                    ty: rty,
                    value: self.constant(Const::String(s)),
                }
            }
            Ty::Bits => self.bits(rty, deep, names),
            Ty::Named(id) => {
                let variants = self.types[id].clone();
                let v = self.dice.below(variants.len());
                let fields = variants[v].iter();
                let fields = fields.map(|t| self.pat(t, deep - 1, names)).collect();
                TypedPat::Ctor {
                    ty: rty,
                    variant: VariantRef {
                        type_id: *id,
                        variant_idx: u16::try_from(v).unwrap(),
                    },
                    fields,
                }
            }
            Ty::Tuple(elems) => TypedPat::Tuple {
                ty: rty,
                elems: elems.iter().map(|t| self.pat(t, deep - 1, names)).collect(),
            },
            Ty::Array(elem) => {
                let n = self.dice.below(4);
                let prefix = (0..n).map(|_| self.pat(elem, deep - 1, names)).collect();
                let rest = self.rest(ty, names);
                TypedPat::Array {
                    ty: rty,
                    elem_ty: self.rty(elem),
                    len: self.constant(Const::Int(i64::try_from(n).unwrap())),
                    prefix,
                    rest,
                }
            }
        }
    }

    fn rest(&mut self, ty: &Ty, names: &mut Names) -> PatRest {
        if self.dice.one_in(2) {
            return PatRest::None;
        }
        match self.name(ty, names) {
            TypedPat::Bind(b) => PatRest::Bind(b),
            _ => PatRest::Discard,
        }
    }

    fn small(&mut self) -> i64 {
        match self.dice.below(12) {
            0 => i64::MAX,
            1 => i64::MIN,
            2 => 1 << 50,
            3 => -(1 << 50) - 1,
            4 => 1_000_000_000_000_000,
            _ => i64::try_from(self.dice.below(6)).unwrap() - 1,
        }
    }

    fn bits(&mut self, rty: RTy, deep: usize, names: &mut Names) -> TypedPat {
        let mut segs = Vec::new();
        for _ in 0..self.dice.below(4) {
            segs.push(match self.dice.below(6) {
                0 => {
                    let text = *self.dice.pick(&["a", "ab", "\u{e9}", ""]);
                    let width = i64::try_from(text.len() * 8).unwrap();
                    TypedBinPatSeg::Utf8Literal {
                        bytes: self.constant(Const::Binary {
                            bytes: text.as_bytes().to_vec(),
                            bit_len: width.unsigned_abs(),
                        }),
                        bits: self.constant(Const::Int(width)),
                    }
                }
                1 => TypedBinPatSeg::Utf8 {
                    value: self.pat(&Ty::Int, deep - 1, names),
                },
                2 => {
                    let bits = match self.dice.one_in(3) {
                        true => None,
                        false => Some(self.size(names)),
                    };
                    TypedBinPatSeg::Binary {
                        bits,
                        value: self.pat(&Ty::Bits, deep - 1, names),
                    }
                }
                _ => TypedBinPatSeg::Int {
                    bits: self.size(names),
                    value: self.pat(&Ty::Int, deep - 1, names),
                },
            });
        }
        let rest = self.rest(&Ty::Bits, names);
        TypedPat::Bin {
            ty: rty,
            zero: self.constant(Const::Int(0)),
            segs,
            rest,
        }
    }

    /// A size, which is a constant or a call that may be given what is named so far.
    fn size(&mut self, names: &Names) -> TypedExpr {
        if !self.dice.one_in(self.kind.sizes) {
            let n = *self.dice.pick(&[8, 8, 8, 4, 16, 0]);
            return TypedExpr::Const {
                ty: self.int,
                value: self.constant(Const::Int(n)),
            };
        }
        let args: Vec<TypedBind> = names
            .scope
            .iter()
            .filter(|_| self.dice.one_in(2))
            .copied()
            .collect();
        self.sizes += 1;
        let (f, int) = (SIZES + self.sizes, self.int);
        self.call(f, &args, int)
    }

    /// Alternatives that all bind the same names, or none.
    fn or(&mut self, ty: &Ty, deep: usize, names: &mut Names) -> TypedPat {
        let rty = self.rty(ty);
        let mut first = Names {
            again: match self.dice.one_in(2) {
                // Nothing is bound.
                true => Some(Vec::new()),
                false => names.again.as_ref().map(|_| Vec::new()),
            },
            scope: names.scope.clone(),
        };
        let outer = names.scope.len();
        let one = self.pat(ty, deep - 1, &mut first);
        let bound: Vec<TypedBind> = first.scope[outer..].to_vec();
        let mut alts = vec![one];
        for _ in 0..1 + self.dice.below(3) {
            // It may take a few tries to have all of the names.
            for _ in 0..6 {
                let mut again = Names {
                    again: Some(bound.iter().map(|b| (b.ty, *b)).collect()),
                    scope: names.scope.clone(),
                };
                let alt = self.pat(ty, deep - 1, &mut again);
                if again.again.is_some_and(|left| left.is_empty()) {
                    alts.push(alt);
                    break;
                }
            }
        }
        names.scope.extend(&bound);
        match alts.len() {
            1 => alts.remove(0),
            _ => TypedPat::Or { ty: rty, alts },
        }
    }
}

fn rename(p: &mut TypedPat, new: &HashMap<BindingId, BindingId>) {
    let bind = |b: &mut TypedBind| b.id = new[&b.id];
    let rest = |r: &mut PatRest| {
        if let PatRest::Bind(b) = r {
            bind(b);
        }
    };
    match p {
        TypedPat::Wild { .. } | TypedPat::Lit { .. } | TypedPat::Range { .. } => {}
        TypedPat::Bind(b) => bind(b),
        TypedPat::Ctor { fields: ps, .. }
        | TypedPat::Tuple { elems: ps, .. }
        | TypedPat::Or { alts: ps, .. } => ps.iter_mut().for_each(|p| rename(p, new)),
        TypedPat::Array {
            prefix, rest: r, ..
        } => {
            prefix.iter_mut().for_each(|p| rename(p, new));
            rest(r);
        }
        TypedPat::Bin { segs, rest: r, .. } => {
            for seg in segs {
                let size = |e: &mut TypedExpr| {
                    if let TypedExpr::Call { args, .. } = e {
                        for a in args {
                            if let TypedExpr::Var {
                                place: ValueRef::Local(b),
                                ..
                            } = a
                            {
                                *b = new.get(b).copied().unwrap_or(*b);
                            }
                        }
                    }
                };
                match seg {
                    TypedBinPatSeg::Utf8Literal { .. } => {}
                    TypedBinPatSeg::Utf8 { value } => rename(value, new),
                    TypedBinPatSeg::Int { bits, value } => {
                        size(bits);
                        rename(value, new);
                    }
                    TypedBinPatSeg::Binary { bits, value } => {
                        bits.iter_mut().for_each(size);
                        rename(value, new);
                    }
                }
            }
            rest(r);
        }
    }
}
