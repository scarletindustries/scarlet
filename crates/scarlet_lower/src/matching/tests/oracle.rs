//! What a match means, written down by itself, and a way to run what it was lowered
//! to. A guard, a size and a body are each a call to a function of their own, so both
//! say what was called with what, and the two have to say the same.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};

use scarlet_ir::TypeId;
use scarlet_ir::core_ir::{
    Atom, Callee, Cases, Const, ConstId, CoreExpr, CoreFn, FuncIdx, JoinId, LocalId, PrimOp,
    ProbeOp,
};

use crate::typed_ir::{
    PatRest, Shapes, TypedArm, TypedBinPatSeg, TypedCallee, TypedExpr, TypedPat, ValueRef,
    pat_binds,
};

/// A value.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Val {
    Int(i128),
    Float(f64),
    Text(String),
    Bits(Vec<bool>),
    Ctor(TypeId, u16, Vec<Val>),
    Tuple(Vec<Val>),
    Array(Vec<Val>),
    /// One that nothing looks into.
    Any(u64),
}

/// One call of a guard, a size or a body, with what it was given.
pub(super) type Call = (u32, Vec<Val>);

/// What a match did.
#[derive(Debug, PartialEq)]
pub(super) struct Run {
    /// The last one is the body's, when an arm fits.
    pub(super) calls: Vec<Call>,
    pub(super) found: bool,
}

/// What the calls before a body answer. It depends on nothing but the call.
pub(super) struct Answers {
    pub(super) seed: u64,
}

impl Answers {
    fn number(&self, call: &Call) -> u64 {
        // FNV over what the call is shown as.
        let text = format!("{} {call:?}", self.seed);
        let mix = |h: u64, b: u8| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
        text.bytes().fold(0xcbf2_9ce4_8422_2325, mix) >> 7
    }

    fn holds(&self, call: &Call) -> bool {
        self.number(call).is_multiple_of(2)
    }

    /// How many bits a size asks for, one of which no binary has.
    fn size(&self, call: &Call) -> i128 {
        [8, 8, 16, 0, 4, 24, -8, 8][usize::try_from(self.number(call) % 8).unwrap()]
    }
}

/// The functions that stand for the parts of a match: a size is a call to one from
/// [`SIZES`] on and a body to one from [`BODIES`] on. A guard is below both.
pub(super) const SIZES: u32 = 1000;
pub(super) const BODIES: u32 = 2000;

/// What there is to a match beside its arms.
pub(super) struct World<'w> {
    pub(super) consts: &'w [Const],
    pub(super) true_: crate::VariantRef,
    pub(super) nil: crate::VariantRef,
}

impl World<'_> {
    fn bool(&self, b: bool) -> Val {
        let (t, f) = (self.true_.variant_idx, 1 - self.true_.variant_idx);
        Val::Ctor(self.true_.type_id, if b { t } else { f }, Vec::new())
    }

    pub(super) fn nil(&self) -> Val {
        Val::Ctor(self.nil.type_id, self.nil.variant_idx, Vec::new())
    }

    fn is_true(&self, v: &Val) -> bool {
        *v == self.bool(true)
    }

    fn constant(&self, c: ConstId) -> Val {
        match &self.consts[c.0 as usize] {
            Const::Int(n) => Val::Int(i128::from(*n)),
            Const::Float(x) => Val::Float(*x),
            Const::String(s) => Val::Text(s.clone()),
            Const::Binary { bytes, bit_len } => {
                let bits = bytes
                    .iter()
                    .flat_map(|b| (0..8).map(move |i| b >> (7 - i) & 1 == 1));
                Val::Bits(bits.take(usize::try_from(*bit_len).unwrap()).collect())
            }
        }
    }

    fn int(&self, c: ConstId) -> i128 {
        match self.constant(c) {
            Val::Int(n) => n,
            other => panic!("{other:?} is no Int"),
        }
    }
}

/// `==`, of what a literal pattern is for.
fn same(a: &Val, b: &Val) -> bool {
    match (a, b) {
        (Val::Float(x), Val::Float(y)) => x == y,
        _ => a == b,
    }
}

// ---- what a match means

/// The names bound so far.
type Env = BTreeMap<usize, Val>;

/// What is still to match, the first first.
#[derive(Clone, Copy)]
enum Work<'p> {
    Pat(&'p TypedPat),
    Bits(Left<'p>),
}

/// What is left to match of a binary, from bit `at`.
#[derive(Clone, Copy)]
struct Left<'p> {
    segs: &'p [TypedBinPatSeg],
    rest: PatRest,
    at: usize,
}

pub(super) struct Meaning<'m> {
    pub(super) world: &'m World<'m>,
    pub(super) answers: &'m Answers,
    pub(super) calls: RefCell<Vec<Call>>,
}

impl Meaning<'_> {
    fn call(&self, e: &TypedExpr, env: &Env) -> Call {
        let TypedExpr::Call {
            callee: TypedCallee::Known(f),
            args,
            ..
        } = e
        else {
            panic!("{e:?} is no call")
        };
        let args = args.iter().map(|a| match a {
            TypedExpr::Var {
                place: ValueRef::Local(b),
                ..
            } => env.get(&b.0).expect("it is in scope").clone(),
            other => panic!("{other:?} is no name"),
        });
        (f.0, args.collect())
    }

    /// The first arm whose pattern fits and whose guard holds.
    pub(super) fn run(self, v: &Val, arms: &[TypedArm]) -> Run {
        let found = arms.iter().any(|arm| {
            let mut work = vec![(v.clone(), Work::Pat(&arm.pat))];
            self.fits(&mut work, &Env::new(), &mut |env| {
                if let Some(guard) = &arm.guard {
                    let call = self.call(guard, env);
                    let holds = self.answers.holds(&call);
                    self.calls.borrow_mut().push(call);
                    if !holds {
                        return false;
                    }
                }
                let call = self.call(&arm.body, env);
                self.calls.borrow_mut().push(call);
                true
            })
        });
        Run {
            calls: self.calls.into_inner(),
            found,
        }
    }

    /// Whether all of `work` fits, the last of it first, and then `then` does. The
    /// alternatives of an or-pattern that binds names are each tried with all that
    /// comes after it. Of one that binds none the first that fits is the only one.
    fn fits(
        &self,
        work: &mut Vec<(Val, Work<'_>)>,
        env: &Env,
        then: &mut dyn FnMut(&Env) -> bool,
    ) -> bool {
        let Some((v, p)) = work.pop() else {
            return then(env);
        };
        let p = match p {
            Work::Pat(p) => p,
            Work::Bits(left) => return self.bits((&v, left), work, env, then),
        };
        match (p, &v) {
            (TypedPat::Wild { .. }, _) => self.fits(work, env, then),
            (TypedPat::Bind(b), _) => {
                let mut env = env.clone();
                env.insert(b.id.0, v);
                self.fits(work, &env, then)
            }
            (TypedPat::Lit { value, .. }, _) => {
                same(&self.world.constant(*value), &v) && self.fits(work, env, then)
            }
            (TypedPat::Range { lo, hi, .. }, Val::Int(n)) => {
                let (lo, hi) = (self.world.int(*lo), self.world.int(*hi));
                (lo..hi).contains(n) && self.fits(work, env, then)
            }
            (
                TypedPat::Ctor {
                    variant, fields, ..
                },
                Val::Ctor(ty, idx, parts),
            ) => {
                assert_eq!(*ty, variant.type_id);
                if *idx != variant.variant_idx {
                    return false;
                }
                let parts = parts.iter().cloned().zip(fields.iter().map(Work::Pat));
                work.extend(parts.rev());
                self.fits(work, env, then)
            }
            (TypedPat::Tuple { elems, .. }, Val::Tuple(parts)) => {
                let parts = parts.iter().cloned().zip(elems.iter().map(Work::Pat));
                work.extend(parts.rev());
                self.fits(work, env, then)
            }
            (TypedPat::Array { prefix, rest, .. }, Val::Array(items)) => {
                let long = items.len() >= prefix.len();
                if !long || (*rest == PatRest::None && items.len() > prefix.len()) {
                    return false;
                }
                let mut env = env.clone();
                if let PatRest::Bind(b) = rest {
                    env.insert(b.id.0, Val::Array(items[prefix.len()..].to_vec()));
                }
                let parts = items.iter().cloned().zip(prefix.iter().map(Work::Pat));
                work.extend(parts.rev());
                self.fits(work, &env, then)
            }
            (TypedPat::Bin { segs, rest, .. }, Val::Bits(_)) => {
                let left = Left {
                    segs,
                    rest: *rest,
                    at: 0,
                };
                self.bits((&v, left), work, env, then)
            }
            (TypedPat::Or { alts, .. }, _) if !pat_binds(p).is_empty() => alts.iter().any(|alt| {
                let mut work = work.clone();
                work.push((v.clone(), Work::Pat(alt)));
                self.fits(&mut work, env, then)
            }),
            (TypedPat::Or { alts, .. }, _) => {
                let one = alts.iter().any(|alt| {
                    let mut only = vec![(v.clone(), Work::Pat(alt))];
                    self.fits(&mut only, env, &mut |_| true)
                });
                one && self.fits(work, env, then)
            }
            (p, v) => panic!("{p:?} of {v:?}"),
        }
    }

    /// [`Self::fits`], with what is left of a binary in front of `work`.
    fn bits<'p>(
        &self,
        (v, left): (&Val, Left<'p>),
        work: &mut Vec<(Val, Work<'p>)>,
        env: &Env,
        then: &mut dyn FnMut(&Env) -> bool,
    ) -> bool {
        let Left { segs, rest, at } = left;
        let Val::Bits(bits) = v else {
            panic!("{v:?} is no binary")
        };
        let left = |segs, at| Work::Bits(Left { segs, rest, at });
        let Some((seg, segs)) = segs.split_first() else {
            return match rest {
                PatRest::None => at == bits.len() && self.fits(work, env, then),
                PatRest::Discard => self.fits(work, env, then),
                PatRest::Bind(b) => {
                    let mut env = env.clone();
                    env.insert(b.id.0, Val::Bits(bits[at..].to_vec()));
                    self.fits(work, &env, then)
                }
            };
        };
        let (value, read, end) = match seg {
            TypedBinPatSeg::Utf8Literal { bytes, .. } => {
                let Val::Bits(prefix) = self.world.constant(*bytes) else {
                    panic!("a literal of bits")
                };
                if !bits[at..].starts_with(&prefix) {
                    return false;
                }
                work.push((v.clone(), left(segs, at + prefix.len())));
                return self.fits(work, env, then);
            }
            TypedBinPatSeg::Utf8 { value } => {
                let Some((code, width)) = utf8(&bits[at..]) else {
                    return false;
                };
                (value, Val::Int(code), at + width)
            }
            TypedBinPatSeg::Int { bits: size, value } => {
                let Some(end) = self.end(size, env, at, bits.len()) else {
                    return false;
                };
                (value, Val::Int(number(&bits[at..end])), end)
            }
            TypedBinPatSeg::Binary { bits: size, value } => {
                let end = match size {
                    None => bits.len(),
                    Some(size) => match self.end(size, env, at, bits.len()) {
                        Some(end) => end,
                        None => return false,
                    },
                };
                (value, Val::Bits(bits[at..end].to_vec()), end)
            }
        };
        work.push((v.clone(), left(segs, end)));
        work.push((read, Work::Pat(value)));
        self.fits(work, env, then)
    }

    /// Where a segment of this size ends that starts at bit `at` of `len`.
    fn end(&self, size: &TypedExpr, env: &Env, at: usize, len: usize) -> Option<usize> {
        let size = match size {
            TypedExpr::Const { value, .. } => self.world.int(*value),
            call => {
                let call = self.call(call, env);
                let size = self.answers.size(&call);
                self.calls.borrow_mut().push(call);
                size
            }
        };
        let size = usize::try_from(size).ok()?;
        (at + size <= len).then_some(at + size)
    }
}

fn number(bits: &[bool]) -> i128 {
    let digit = |n: i128, b: &bool| (n << 1) | i128::from(*b);
    bits.iter().fold(0, digit)
}

/// The code point that `bits` start with, and how many bits it takes.
fn utf8(bits: &[bool]) -> Option<(i128, usize)> {
    let bytes: Vec<u8> = bits
        .chunks_exact(8)
        .take(4)
        .map(|b| u8::try_from(number(b)).unwrap())
        .collect();
    (1..=bytes.len()).find_map(|n| {
        let text = std::str::from_utf8(&bytes[..n]).ok()?;
        let c = text.chars().next()?;
        Some((i128::from(u32::from(c)), n * 8))
    })
}

// ---- what it was lowered to

/// Runs lowered functions of one parameter.
pub(super) struct Machine<'m> {
    pub(super) world: &'m World<'m>,
    pub(super) answers: &'m Answers,
}

/// What an expression goes on to.
enum Next {
    Body,
    Jump(JoinId, Vec<Val>),
    Nothing,
}

impl Machine<'_> {
    pub(super) fn run(&self, f: &CoreFn, v: &Val) -> Run {
        let mut env = HashMap::from([(f.params[0].id, v.clone())]);
        let mut calls = Vec::new();
        let found = match self.expr(&f.body, &mut env, &mut calls) {
            Next::Body => true,
            Next::Nothing => false,
            Next::Jump(to, ..) => panic!("a jump out to {to}"),
        };
        Run { calls, found }
    }

    fn expr(
        &self,
        mut e: &CoreExpr,
        env: &mut HashMap<LocalId, Val>,
        calls: &mut Vec<Call>,
    ) -> Next {
        loop {
            match e {
                CoreExpr::Let { bind, rhs, body } => {
                    let v = self.atom(rhs, env, calls);
                    assert!(env.insert(bind.id, v).is_none(), "{} again", bind.id);
                    e = body;
                }
                CoreExpr::Drop { local, body } => {
                    assert!(env.contains_key(local), "{local} is dropped and not there");
                    e = body;
                }
                CoreExpr::Open {
                    cell,
                    variant,
                    fields,
                    body,
                } => {
                    let Some(Val::Ctor(ty, idx, parts)) = env.remove(cell) else {
                        panic!("{cell} is taken apart and is no constructor")
                    };
                    assert_eq!((ty, idx), (variant.type_id, variant.variant_idx), "{cell}");
                    for (i, bind) in fields {
                        env.insert(bind.id, parts[usize::from(*i)].clone());
                    }
                    e = body;
                }
                CoreExpr::Join {
                    id,
                    params,
                    cont,
                    body,
                } => match self.expr(body, &mut env.clone(), calls) {
                    Next::Jump(to, args) if to == *id => {
                        assert_eq!(args.len(), params.len());
                        env.extend(params.iter().map(|p| p.id).zip(args));
                        e = cont;
                    }
                    out @ (Next::Jump(..) | Next::Body | Next::Nothing) => return out,
                },
                CoreExpr::If { cond, then, els } => {
                    e = match self.world.is_true(&env[cond]) {
                        true => then,
                        false => els,
                    };
                }
                CoreExpr::Probe {
                    op: op @ (ProbeOp::ArrayUncons | ProbeOp::ArraySplit { .. }),
                    args,
                    binds,
                    hit,
                    miss,
                } => {
                    let [Val::Array(items)] = [&env[&args[0]]] else {
                        panic!("{op:?} of {:?}", env[&args[0]])
                    };
                    assert_eq!(args.len(), 1);
                    let Some(found) = pieces(*op, items, binds.len()) else {
                        e = miss;
                        continue;
                    };
                    for (bind, v) in binds.iter().zip(found) {
                        if let Some(bind) = bind {
                            assert!(env.insert(bind.id, v).is_none(), "{} again", bind.id);
                        }
                    }
                    e = hit;
                }
                // What it asks for is there when its first operand is the first
                // constructor of its type, and is the field of that.
                CoreExpr::Probe {
                    args,
                    binds,
                    hit,
                    miss,
                    ..
                } => {
                    let Val::Ctor(_, idx, parts) = &env[&args[0]] else {
                        panic!("a probe of {:?}", env[&args[0]])
                    };
                    if *idx != 0 {
                        e = miss;
                        continue;
                    }
                    let found = parts[0].clone();
                    if let [Some(bind)] = binds.as_slice() {
                        assert!(env.insert(bind.id, found).is_none(), "{} again", bind.id);
                    }
                    e = hit;
                }
                CoreExpr::Switch {
                    scrut,
                    cases,
                    default,
                } => match self.case(&env[scrut], cases).or(default.as_deref()) {
                    Some(case) => e = case,
                    None => panic!("no case for {:?} in\n{e}", env[scrut]),
                },
                CoreExpr::TailCall { callee, args } => {
                    let Callee::Known(FuncIdx(f)) = callee else {
                        panic!("a call to another")
                    };
                    let args = args.iter().map(|l| env[l].clone()).collect();
                    calls.push((*f, args));
                    return Next::Body;
                }
                CoreExpr::Jump { to, args } => {
                    let args = args.iter().map(|l| env[l].clone()).collect();
                    return Next::Jump(*to, args);
                }
                CoreExpr::NoArm => return Next::Nothing,
                CoreExpr::Ret(_) | CoreExpr::SetGlobal { .. } => panic!("{e}"),
            }
        }
    }

    fn case<'e>(&self, v: &Val, cases: &'e Cases) -> Option<&'e CoreExpr> {
        match (cases, v) {
            (Cases::Variant { ty, arms }, Val::Ctor(of, idx, _)) => {
                assert_eq!(ty, of);
                let arm = arms.iter().find(|(keys, _)| keys.contains(idx));
                arm.map(|(_, e)| e)
            }
            (Cases::Int(arms), Val::Int(n)) => {
                let is = |k: &i64| i128::from(*k) == *n;
                let arm = arms.iter().find(|(keys, _)| keys.iter().any(is));
                arm.map(|(_, e)| e)
            }
            (Cases::Float(arms), Val::Float(x)) => {
                let is = |k: &crate::FloatKey| k.get() == *x;
                let arm = arms.iter().find(|(keys, _)| keys.iter().any(is));
                arm.map(|(_, e)| e)
            }
            (Cases::Text(arms), Val::Text(_)) => {
                let is = |k: &ConstId| self.world.constant(*k) == *v;
                let arm = arms.iter().find(|(keys, _)| keys.iter().any(is));
                arm.map(|(_, e)| e)
            }
            (cases, v) => panic!("{v:?} in {cases:?}"),
        }
    }

    fn atom(&self, atom: &Atom, env: &HashMap<LocalId, Val>, calls: &mut Vec<Call>) -> Val {
        let get = |l: &LocalId| env.get(l).unwrap_or_else(|| panic!("{l} is not there"));
        match atom {
            Atom::Local(l) => get(l).clone(),
            Atom::Const(c) => self.world.constant(*c),
            Atom::Bool(b) => self.world.bool(*b),
            Atom::Nil => self.world.nil(),
            Atom::Ctor { variant, fields } => {
                let fields = fields.iter().map(|l| get(l).clone()).collect();
                Val::Ctor(variant.type_id, variant.variant_idx, fields)
            }
            Atom::Call {
                callee: Callee::Known(FuncIdx(f)),
                args,
            } => {
                let call = (*f, args.iter().map(|l| get(l).clone()).collect());
                let answer = match *f {
                    0..SIZES => self.world.bool(self.answers.holds(&call)),
                    _ => Val::Int(self.answers.size(&call)),
                };
                calls.push(call);
                answer
            }
            Atom::PrimOp { op, args } => {
                let args: Vec<&Val> = args.iter().map(get).collect();
                self.prim(*op, &args)
            }
            other => panic!("{other}"),
        }
    }

    fn prim(&self, op: PrimOp, args: &[&Val]) -> Val {
        let at = |i: u16| usize::from(i);
        let small = |n: &i128| usize::try_from(*n).unwrap();
        match (op, args) {
            (PrimOp::Field(i), [Val::Ctor(_, _, parts)]) => parts[at(i)].clone(),
            (PrimOp::TupleField(i), [Val::Tuple(parts)]) => parts[at(i)].clone(),
            (PrimOp::ArrayLen, [Val::Array(items)]) => {
                Val::Int(i128::try_from(items.len()).unwrap())
            }
            (PrimOp::IntEq, [Val::Int(a), Val::Int(b)]) => self.world.bool(a == b),
            (PrimOp::IntGe, [Val::Int(a), Val::Int(b)]) => self.world.bool(a >= b),
            (PrimOp::IntGt, [Val::Int(a), Val::Int(b)]) => self.world.bool(a > b),
            (PrimOp::IntLe, [Val::Int(a), Val::Int(b)]) => self.world.bool(a <= b),
            (PrimOp::IntLt, [Val::Int(a), Val::Int(b)]) => self.world.bool(a < b),
            (PrimOp::IntAdd, [Val::Int(a), Val::Int(b)]) => Val::Int(a + b),
            (PrimOp::IntSub, [Val::Int(a), Val::Int(b)]) => Val::Int(a - b),
            (PrimOp::BinaryBitSize, [Val::Bits(bits)]) => {
                Val::Int(i128::try_from(bits.len()).unwrap())
            }
            (PrimOp::BinaryMatchPrefix, [Val::Bits(bits), Val::Int(from), Val::Bits(prefix)]) => {
                self.world.bool(bits[small(from)..].starts_with(prefix))
            }
            (PrimOp::BinaryReadUtf8, [Val::Bits(bits), Val::Int(from)]) => {
                let (code, width) = utf8(&bits[small(from)..]).unwrap_or((0, 0));
                let width = i128::try_from(width).unwrap();
                Val::Tuple(vec![Val::Int(code), Val::Int(width)])
            }
            (PrimOp::BinaryReadInt, [Val::Bits(bits), Val::Int(from), Val::Int(width)]) => {
                Val::Int(number(&bits[small(from)..][..small(width)]))
            }
            (PrimOp::BinaryView, [Val::Bits(bits), Val::Int(from), Val::Int(width)]) => {
                Val::Bits(bits[small(from)..][..small(width)].to_vec())
            }
            (op, args) => panic!("{op:?} of {args:?}"),
        }
    }
}

/// What a probe of an array of `items` finds, in `places` places.
fn pieces(op: ProbeOp, items: &[Val], places: usize) -> Option<Vec<Val>> {
    let (len, more, from) = match op {
        ProbeOp::ArrayUncons => (1, true, 0),
        ProbeOp::ArraySplit { len, more, from } => {
            assert!((len, more) != (1, true), "a cons is no split");
            let small = |n: u32| usize::try_from(n).unwrap();
            (small(len), more, small(from))
        }
        other => panic!("{other:?} of an array"),
    };
    let elements = places - usize::from(more);
    assert!(from + elements <= len, "{op:?} finds {places}");
    if items.len() < len || (!more && items.len() > len) {
        return None;
    }
    let mut found = items[from..from + elements].to_vec();
    if more {
        found.push(Val::Array(items[len..].to_vec()));
    }
    Some(found)
}

// ---- values

/// A source of numbers that look random.
pub(super) struct Dice(pub(super) u64);

impl Dice {
    pub(super) fn below(&mut self, n: usize) -> usize {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        usize::try_from((z ^ (z >> 31)) % u64::try_from(n.max(1)).unwrap()).unwrap()
    }

    pub(super) fn one_in(&mut self, n: usize) -> bool {
        self.below(n) == 0
    }

    pub(super) fn pick<'t, T>(&mut self, of: &'t [T]) -> &'t T {
        &of[self.below(of.len())]
    }
}

/// A value that `pats` ask something of. It is likely to fit the first of them, or
/// another, or to fail late.
pub(super) fn value(
    dice: &mut Dice,
    world: &World<'_>,
    shapes: &Shapes,
    pats: &[&TypedPat],
) -> Val {
    let mut flat = Vec::new();
    pats.iter().for_each(|p| flatten(p, &mut flat));
    flat.retain(|p| !matches!(p, TypedPat::Wild { .. } | TypedPat::Bind(_)));
    if flat.is_empty() {
        return Val::Any(u64::try_from(dice.below(1000)).unwrap());
    }
    // One of them says what it is.
    let model = match dice.one_in(3) {
        true => *dice.pick(&flat),
        false => flat[0],
    };
    match model {
        TypedPat::Ctor { variant, .. } => {
            let shape = shapes
                .of(variant.type_id)
                .expect("a type with constructors");
            let idx = match dice.one_in(3) {
                true => u16::try_from(dice.below(shape.variants())).unwrap(),
                false => variant.variant_idx,
            };
            let mut parts = Vec::new();
            for i in 0..shape.fields(idx).expect("a constructor") {
                let inner: Vec<&TypedPat> = flat
                    .iter()
                    .filter_map(|p| match p {
                        TypedPat::Ctor {
                            variant, fields, ..
                        } if variant.variant_idx == idx => fields.get(i),
                        _ => None,
                    })
                    .collect();
                parts.push(value(dice, world, shapes, &inner));
            }
            Val::Ctor(variant.type_id, idx, parts)
        }
        TypedPat::Tuple { elems, .. } => {
            let parts = (0..elems.len()).map(|i| {
                let inner = flat.iter().filter_map(|p| match p {
                    TypedPat::Tuple { elems, .. } => elems.get(i),
                    _ => None,
                });
                let inner: Vec<&TypedPat> = inner.collect();
                value(dice, world, shapes, &inner)
            });
            Val::Tuple(parts.collect())
        }
        TypedPat::Array { prefix, rest, .. } => {
            let len = match dice.below(4) {
                0 => dice.below(prefix.len() + 3),
                1 if *rest != PatRest::None => prefix.len() + dice.below(3),
                _ => prefix.len(),
            };
            let items = (0..len).map(|i| {
                let inner = flat.iter().filter_map(|p| match p {
                    TypedPat::Array { prefix, .. } => prefix.get(i),
                    _ => None,
                });
                let inner: Vec<&TypedPat> = inner.collect();
                value(dice, world, shapes, &inner)
            });
            Val::Array(items.collect())
        }
        TypedPat::Bin { segs, .. } => {
            let mut bits = Vec::new();
            for seg in segs {
                match seg {
                    TypedBinPatSeg::Utf8Literal { bytes, .. } => {
                        if let Val::Bits(b) = world.constant(*bytes) {
                            bits.extend(b);
                        }
                    }
                    TypedBinPatSeg::Utf8 { .. } => {
                        let c = *dice.pick(&['a', 'b', 'c', '\u{e9}', '\u{20ac}', '\u{1f600}']);
                        let mut room = [0; 4];
                        let bytes = c.encode_utf8(&mut room).bytes();
                        bits.extend(bytes.flat_map(|b| (0..8).map(move |i| b >> (7 - i) & 1 == 1)));
                    }
                    TypedBinPatSeg::Int { .. } | TypedBinPatSeg::Binary { .. } => {
                        let n = dice.below(4);
                        let n = u8::try_from(n).unwrap();
                        bits.extend((0..8).map(|i| n >> (7 - i) & 1 == 1));
                    }
                }
            }
            match dice.below(4) {
                0 => bits.truncate(dice.below(bits.len() + 1)),
                1 => bits.extend((0..dice.below(24)).map(|_| false)),
                _ => {}
            }
            Val::Bits(bits)
        }
        TypedPat::Lit { value, .. } => match world.constant(*value) {
            Val::Int(n) => Val::Int(near(dice, n)),
            Val::Float(x) => Val::Float(match dice.below(5) {
                0 => -x,
                1 => x + 1.0,
                _ => x,
            }),
            Val::Text(s) => Val::Text(match dice.below(4) {
                0 => format!("{s}x"),
                _ => s,
            }),
            other => panic!("{other:?}"),
        },
        TypedPat::Range { lo, hi, .. } => {
            let at = *dice.pick(&[lo, hi]);
            let Val::Int(n) = world.constant(*at) else {
                panic!("no Int")
            };
            Val::Int(near(dice, n))
        }
        TypedPat::Wild { .. } | TypedPat::Bind(_) | TypedPat::Or { .. } => Val::Any(0),
    }
}

/// `n`, or an Int near it, or one that is far from any.
fn near(dice: &mut Dice, n: i128) -> i128 {
    match dice.below(8) {
        0 => n - 1,
        1 => n + 1,
        2 => i128::from(i64::MAX) + 1 + n,
        3 => 1 << 70,
        _ => n,
    }
}

fn flatten<'p>(p: &'p TypedPat, out: &mut Vec<&'p TypedPat>) {
    match p {
        TypedPat::Or { alts, .. } => alts.iter().for_each(|p| flatten(p, out)),
        other => out.push(other),
    }
}
