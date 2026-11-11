//! The checks that need the module's types to be final: whether matches and
//! bindings cover every value, `_` over a `Nil`, an `or` that drops an error,
//! and a statement whose value nothing uses. Each reads the frozen types and
//! the finished tree, and reports what it finds.

use scarlet_types::def::TypeDef;
use scarlet_types::engine::Types;
use scarlet_types::exhaustiveness::{
    BinPart, BinSegShape, BinUnit, PatShape, PatternShape, Space, analyse, missed_by, unroll_depth,
};
use scarlet_types::name::TypeName;
use scarlet_types::ty::Type;

use crate::UnusedBindings;
use crate::env::Env;
use crate::error::CheckError;
use crate::report::Report;
use crate::resolved::BinderInfo;
use crate::typed::{
    Arm, BinSegPat, BinderId, Block, Expr, ExprKind, FnBody, Function, OrShape, Pattern,
    PatternKind, Rest, Stmt,
};
use crate::types::TypeDefs;

/// What the checks read.
pub(crate) struct Finished<'f, 'a> {
    pub env: &'f Env<'a>,
    pub defs: &'f TypeDefs,
    pub types: &'f Types,
    pub binders: &'f [BinderInfo],
    pub binder_types: &'f [scarlet_types::engine::Ty],
    pub unused: &'f [BinderId],
    pub refused: &'f [BinderId],
}

pub(crate) fn finalise(
    f: &Finished<'_, '_>,
    functions: &[Function],
    consts: &[crate::typed::Const],
    statements: &[Stmt],
) -> Report {
    let mut report = Report::new(f.env.input.source);
    let mut visit = |e: &Expr| f.visit(e, &mut report);
    for func in functions {
        if let FnBody::Expr(body) = &func.body {
            body.walk(&mut visit);
        }
    }
    for c in consts {
        c.value.walk(&mut visit);
    }
    for s in statements {
        s.for_each_expr(&mut |e| e.walk(&mut visit));
    }
    for s in statements {
        if let Stmt::Let { pattern, value } = s {
            f.irrefutable(pattern, value, &mut report);
        }
    }
    f.discards(&mut report);
    f.unused(&mut report);
    report
}

impl Finished<'_, '_> {
    fn visit(&self, e: &Expr, report: &mut Report) {
        match &e.kind {
            ExprKind::Match { subject, arms } => self.match_arms(subject, arms, report),
            ExprKind::Block(b) => self.block(b, report),
            ExprKind::OrElse {
                shape: OrShape::Result,
                value,
                error,
                ..
            } => self.or_receiver(value, *error, report),
            ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::String(_)
            | ExprKind::Interp(_)
            | ExprKind::Binary(_)
            | ExprKind::Var(_)
            | ExprKind::Lambda(_)
            | ExprKind::Call { .. }
            | ExprKind::Construct { .. }
            | ExprKind::Tuple(_)
            | ExprKind::TupleIndex { .. }
            | ExprKind::Field { .. }
            | ExprKind::Array(_)
            | ExprKind::Index { .. }
            | ExprKind::Slice { .. }
            | ExprKind::Range { .. }
            | ExprKind::BinOp { .. }
            | ExprKind::And(..)
            | ExprKind::Or(..)
            | ExprKind::Unary { .. }
            | ExprKind::If { .. }
            | ExprKind::OrElse { .. }
            | ExprKind::Error => {}
        }
    }

    fn is_nil(&self, t: &Type) -> bool {
        matches!(t, Type::Named { name, .. } if *name == self.types.known().nil)
    }

    fn block(&self, b: &Block, report: &mut Report) {
        for s in &b.stmts {
            match s {
                Stmt::Let { pattern, value } => self.irrefutable(pattern, value, report),
                Stmt::Expr(x) => {
                    let ty = self.types.get(x.ty);
                    if !matches!(ty, Type::Var(_)) && !self.is_nil(&ty) {
                        report.add(x.span, CheckError::UnusedValue(ty));
                    }
                }
            }
        }
    }

    fn type_def(&self, name: &TypeName) -> Option<&TypeDef> {
        if *name.module() == self.env.key {
            return self.defs.types.get(name.name());
        }
        self.env.interface_of(name.module())?.types.get(name.name())
    }

    /// The space of values of `ty`, unrolled `depth` levels deep.
    fn space(&self, ty: &Type, depth: usize) -> Space {
        match ty {
            Type::Var(_) | Type::Fn { .. } => Space::Infinite,
            Type::Tuple(elems) => Space::Tuple(
                elems
                    .iter()
                    .map(|e| self.space(e, depth.saturating_sub(1)))
                    .collect(),
            ),
            Type::Named { name, args } => {
                if *name == self.types.known().array {
                    let elem = args
                        .first()
                        .map_or(Space::Infinite, |a| self.space(a, depth.saturating_sub(1)));
                    return Space::Array(std::rc::Rc::new(elem));
                }
                if depth == 0 {
                    return Space::Infinite;
                }
                let Some(def) = self.type_def(name) else {
                    return Space::Infinite;
                };
                let Some(variants) = def.variants() else {
                    return Space::Infinite;
                };
                Space::Variants(
                    variants
                        .iter()
                        .enumerate()
                        .map(|(i, v)| {
                            let fields = def
                                .field_types(i, args)
                                .unwrap_or_default()
                                .iter()
                                .map(|f| self.space(f, depth - 1))
                                .collect();
                            (v.name.to_string(), fields)
                        })
                        .collect(),
                )
            }
        }
    }

    fn match_arms(&self, subject: &Expr, arms: &[Arm], report: &mut Report) {
        if arms.iter().any(|a| has_error(&a.pattern)) {
            return;
        }
        let depth = arms
            .iter()
            .map(|a| unroll_depth(&a.pattern))
            .max()
            .unwrap_or(0)
            + 1;
        let space = self.space(&self.types.get(subject.ty), depth);
        let checked: Vec<(&Pattern, bool)> = arms
            .iter()
            .map(|a| (&a.pattern, a.guard.is_some()))
            .collect();
        let found = analyse(&space, &checked);
        if let Some(missing) = found.missing {
            report.add(subject.span, CheckError::NotExhaustive { missing });
        }
        for i in found.unreachable {
            report.add(arms[i].pattern.span, CheckError::Unreachable);
        }
        // A catch-all over named variants would quietly take any variant
        // added later, so the arms have to name them.
        for (i, missing) in found.catch_alls {
            report.add(arms[i].pattern.span, CheckError::CatchAll(missing));
        }
    }

    /// A binding's pattern has to match every value.
    fn irrefutable(&self, pattern: &Pattern, value: &Expr, report: &mut Report) {
        if matches!(pattern.kind, PatternKind::Bind(_) | PatternKind::Wild) || has_error(pattern) {
            return;
        }
        let depth = unroll_depth(pattern) + 1;
        let space = self.space(&self.types.get(value.ty), depth);
        if let Some(missing) = missed_by(&space, pattern) {
            report.add(pattern.span, CheckError::BindingMisses { missing });
        }
    }

    /// An `or` on a `Result` must handle the error. A `Nil` error carries
    /// nothing, so like `Option` it takes no receiver. Any other error must be
    /// bound to a name, and one starting with `_` drops it just as quietly.
    fn or_receiver(
        &self,
        value: &Expr,
        error: Option<crate::typed::BinderId>,
        report: &mut Report,
    ) {
        let err = match self.types.get(value.ty) {
            Type::Named { args, .. } => args.get(1).cloned(),
            Type::Var(_) | Type::Fn { .. } | Type::Tuple(_) => None,
        };
        let Some(err) = err else { return };
        let is_nil = self.is_nil(&err);
        let receiver = error.map(|b| {
            let s = &self.binders[b.0 as usize];
            (s.name.clone(), s.span)
        });
        match (is_nil, receiver) {
            (true, None) => {}
            (true, Some((_, sp))) => report.add(sp, CheckError::OrNilReceiver),
            (false, None) => report.add(value.span, CheckError::OrDropsError(err)),
            (false, Some((name, sp))) if name.starts_with('_') => {
                report.add(sp, CheckError::OrDiscardsReceiver(name))
            }
            (false, Some(_)) => {}
        }
    }

    /// A `_` that throws away a value the types know is `Nil`. There is
    /// nothing to throw away, and the `Nil` pattern stops compiling the day
    /// the type gains data.
    fn discards(&self, report: &mut Report) {
        for (b, &ty) in self.binders.iter().zip(self.binder_types) {
            if b.discards && self.is_nil(&self.types.get(ty)) {
                report.add(b.span, CheckError::NilDiscard(b.name.clone()));
            }
        }
    }
}

impl Finished<'_, '_> {
    fn unused(&self, report: &mut Report) {
        if self.env.input.unused == UnusedBindings::Ignore {
            return;
        }
        for b in self.unused.iter().filter(|b| !self.refused.contains(b)) {
            let info = &self.binders[b.0 as usize];
            report.add(info.span, CheckError::Unused(info.name.clone()));
        }
    }
}

fn has_error(p: &Pattern) -> bool {
    let mut found = false;
    walk_patterns(p, &mut |q| found |= matches!(q.kind, PatternKind::Error));
    found
}

fn walk_patterns(p: &Pattern, f: &mut impl FnMut(&Pattern)) {
    f(p);
    match &p.kind {
        PatternKind::Ctor { fields: ps, .. }
        | PatternKind::Tuple(ps)
        | PatternKind::Array { prefix: ps, .. }
        | PatternKind::Or(ps) => ps.iter().for_each(|q| walk_patterns(q, f)),
        PatternKind::Binary { segs, .. } => {
            for s in segs {
                match s {
                    BinSegPat::Text(_) => {}
                    BinSegPat::Utf8(q)
                    | BinSegPat::Bits { value: q, .. }
                    | BinSegPat::Bytes { value: q, .. } => walk_patterns(q, f),
                }
            }
        }
        PatternKind::Wild
        | PatternKind::Bind(_)
        | PatternKind::Int(_)
        | PatternKind::Float(_)
        | PatternKind::String(_)
        | PatternKind::Range(..)
        | PatternKind::Error => {}
    }
}

impl PatternShape for Pattern {
    fn shape(&self) -> PatShape<'_, Self> {
        match &self.kind {
            PatternKind::Wild | PatternKind::Bind(_) | PatternKind::Error => PatShape::Wild,
            PatternKind::Int(i) => PatShape::Int(*i),
            PatternKind::Float(f) => PatShape::Float(f.to_bits()),
            PatternKind::String(s) => PatShape::String(s),
            PatternKind::Range(lo, hi) => PatShape::Range(*lo, *hi),
            PatternKind::Ctor { ctor, fields } => PatShape::Variant {
                index: ctor.variant as usize,
                fields,
            },
            PatternKind::Tuple(elems) => PatShape::Tuple(elems),
            PatternKind::Array { prefix, rest } => PatShape::Array {
                prefix,
                open: !matches!(rest, Rest::None),
            },
            // `<<..rest>>` matches every binary.
            PatternKind::Binary { segs, rest }
                if segs.is_empty() && !matches!(rest, Rest::None) =>
            {
                PatShape::Wild
            }
            PatternKind::Binary { segs, rest } => PatShape::Binary {
                segs: segs.iter().map(bin_seg_shape).collect(),
                open: !matches!(rest, Rest::None),
            },
            PatternKind::Or(alts) => PatShape::Or(alts),
        }
    }
}

fn bin_part(p: &Pattern) -> BinPart<'_> {
    match &p.kind {
        PatternKind::Wild | PatternKind::Bind(_) => BinPart::Any,
        PatternKind::Int(i) => BinPart::Int(*i),
        PatternKind::String(s) => BinPart::Str(s),
        PatternKind::Float(_)
        | PatternKind::Range(..)
        | PatternKind::Ctor { .. }
        | PatternKind::Tuple(_)
        | PatternKind::Array { .. }
        | PatternKind::Binary { .. }
        | PatternKind::Or(_)
        | PatternKind::Error => BinPart::At(p.span),
    }
}

fn size_part(e: &Option<Expr>) -> Option<BinPart<'_>> {
    e.as_ref().map(|e| match &e.kind {
        ExprKind::Int(i) => BinPart::Int(*i),
        _ => BinPart::At(e.span),
    })
}

fn bin_seg_shape(seg: &BinSegPat) -> BinSegShape<'_> {
    match seg {
        BinSegPat::Text(s) => BinSegShape {
            value: BinPart::Str(s),
            unit: BinUnit::Utf8,
            size: None,
        },
        BinSegPat::Utf8(p) => BinSegShape {
            value: bin_part(p),
            unit: BinUnit::Utf8,
            size: None,
        },
        BinSegPat::Bits { bits, value } => BinSegShape {
            value: bin_part(value),
            unit: BinUnit::Bits,
            size: size_part(bits),
        },
        BinSegPat::Bytes { bytes, value } => BinSegShape {
            value: bin_part(value),
            unit: BinUnit::Bytes,
            size: size_part(bytes),
        },
    }
}
