//! The checks that need the module's types to be final: whether matches and
//! bindings cover every value, `_` over a `Nil`, an `or` that drops an error,
//! a statement whose value nothing uses, and bindings nothing reads. Each one
//! is a walk over the finished tree.

use scarlet_types::exhaustiveness::{BinPart, BinSegShape, BinUnit, PatShape, PatternShape, Space, UsefulnessMatrix, unroll_depth};
use scarlet_types::ty::Type;

use crate::check::Checker;
use crate::typed::{BinSegPat, Block, Const, Expr, ExprKind, FnBody, Function, OrShape, Pattern, PatternKind, Rest, Stmt};

impl Checker<'_> {
    pub fn finalise(&mut self, functions: &[Function], consts: &[Const], statements: &[Stmt]) {
        let mut visit = |c: &mut Self, e: &Expr| c.visit(e);
        for f in functions {
            if let FnBody::Expr(body) = &f.body {
                body.walk(&mut |e| visit(self, e));
            }
        }
        for c in consts {
            c.value.walk(&mut |e| visit(self, e));
        }
        for s in statements {
            s.for_each_expr(&mut |e| e.walk(&mut |e| visit(self, e)));
            if let Stmt::Let { pattern, value } = s {
                self.check_irrefutable(pattern, value);
            }
        }
        self.check_discards();
        self.report_unused();
    }

    fn visit(&mut self, e: &Expr) {
        match &e.kind {
            ExprKind::Match { subject, arms } => self.check_match_arms(subject, arms, e),
            ExprKind::Block(b) => self.check_block_stmts(b),
            ExprKind::OrElse {
                shape: OrShape::Result,
                value,
                error,
                ..
            } => self.check_or_receiver(value, *error),
            _ => {}
        }
    }

    fn check_block_stmts(&mut self, b: &Block) {
        for s in &b.stmts {
            match s {
                Stmt::Let { pattern, value } => self.check_irrefutable(pattern, value),
                Stmt::Expr(x) => {
                    let ty = self.e.zonk(x.ty);
                    let ignorable = match &ty {
                        Type::Var(_) => true,
                        Type::Named { name, .. } => *name == self.e.known().nil,
                        Type::Fn { .. } | Type::Tuple(_) => false,
                    };
                    if !ignorable {
                        self.error(
                            x.span,
                            format!(
                                "This expression's value of type '{ty}' isn't used. Assign it to a name, or write '_ = ...' to throw it away"
                            ),
                        );
                    }
                }
            }
        }
    }

    /// The space of values of `ty`, unrolled `depth` levels deep.
    fn space(&self, ty: &Type, depth: usize) -> Space {
        match ty {
            Type::Var(_) | Type::Fn { .. } => Space::Infinite,
            Type::Tuple(elems) => Space::Tuple(elems.iter().map(|e| self.space(e, depth.saturating_sub(1))).collect()),
            Type::Named { name, args } => {
                if *name == self.e.known().array {
                    let elem = args.first().map_or(Space::Infinite, |a| self.space(a, depth.saturating_sub(1)));
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

    fn check_match_arms(&mut self, subject: &Expr, arms: &[crate::typed::Arm], m: &Expr) {
        if arms.iter().any(|a| has_error(&a.pattern)) {
            return;
        }
        let depth = arms.iter().map(|a| unroll_depth(&a.pattern)).max().unwrap_or(0) + 1;
        let ty = self.e.zonk(subject.ty);
        let space = self.space(&ty, depth);
        let mut um = UsefulnessMatrix::new(&space);
        let pats: Vec<_> = arms.iter().map(|a| um.lower(&a.pattern)).collect();
        // A guarded arm may not match, so it neither covers anything nor makes
        // a later arm unreachable.
        let unguarded = arms.iter().zip(&pats).filter(|(a, _)| a.guard.is_none()).map(|(_, p)| p);
        if let Some(missing) = um.find_missing(unguarded) {
            self.error(subject.span, format!("Match is not exhaustive. Add arms for: {missing}"));
        }
        let _ = m;
        for (i, arm) in arms.iter().enumerate() {
            if !um.is_useful(&pats[i]) {
                self.error(
                    arm.pattern.span,
                    "This pattern is unreachable, because an earlier pattern matches the same values",
                );
            } else if let Some(missing) = um.catch_all(&pats[..i], &pats[i]) {
                // A catch-all over named variants would quietly take any
                // variant added later, so the arms have to name them.
                self.error(
                    arm.pattern.span,
                    format!(
                        "This pattern stands for {}. Name {} instead, like `{} ->`",
                        missing.join(", "),
                        if missing.len() == 1 { "it" } else { "them" },
                        missing.join(" | ")
                    ),
                );
            }
            if arm.guard.is_none() {
                um.push(&pats[i]);
            }
        }
    }

    /// A binding's pattern has to match every value.
    fn check_irrefutable(&mut self, pattern: &Pattern, value: &Expr) {
        if matches!(pattern.kind, PatternKind::Bind(_) | PatternKind::Wild) || has_error(pattern) {
            return;
        }
        let depth = unroll_depth(pattern) + 1;
        let ty = self.e.zonk(value.ty);
        let space = self.space(&ty, depth);
        let mut um = UsefulnessMatrix::new(&space);
        let p = um.lower(pattern);
        if let Some(missing) = um.find_missing(&[p]) {
            self.error(
                pattern.span,
                format!(
                    "A binding's pattern must match every value, and this one misses {missing}. Use `match` to handle it"
                ),
            );
        }
    }

    /// An `or` on a `Result` must handle the error. A `Nil` error carries
    /// nothing, so like `Option` it takes no receiver. Any other error must be
    /// bound to a name, and one starting with `_` drops it just as quietly.
    fn check_or_receiver(&mut self, value: &Expr, error: Option<crate::typed::BinderId>) {
        let err = match self.e.zonk(value.ty) {
            Type::Named { args, .. } => args.get(1).cloned(),
            Type::Var(_) | Type::Fn { .. } | Type::Tuple(_) => None,
        };
        let Some(err) = err else { return };
        let is_nil = matches!(&err, Type::Named { name, .. } if *name == self.e.known().nil);
        let receiver = error.map(|b| {
            let s = &self.binders[b.0 as usize];
            (s.name.clone(), s.span)
        });
        match (is_nil, receiver) {
            (true, None) => {}
            (true, Some((_, sp))) => self.error(
                sp,
                "'or' on a Result(_, Nil) can't bind a value, because the error carries nothing",
            ),
            (false, None) => self.error(
                value.span,
                format!(
                    "This 'or' throws away the Result's error, of type '{err}'. Bind it with 'e -> ...' and handle it"
                ),
            ),
            (false, Some((name, sp))) if name.starts_with('_') => self.error(
                sp,
                format!("'{name}' throws away the error. Give the 'or' receiver a name and use it"),
            ),
            (false, Some(_)) => {}
        }
    }

    /// A `_` that throws away a value the types know is `Nil`. There is
    /// nothing to throw away, and the explicit `Nil` pattern stops compiling
    /// the day the type gains data.
    fn check_discards(&mut self) {
        let nil = self.e.known().nil.clone();
        let mut found = Vec::new();
        for b in &self.binders {
            if b.discards {
                found.push((b.name.clone(), b.scheme.ty, b.span));
            }
        }
        for (name, ty, span) in found {
            if matches!(self.e.zonk(ty), Type::Named { name: n, .. } if n == nil) {
                self.error(
                    span,
                    format!("'{name}' always matches Nil here, so write the explicit 'Nil' pattern instead"),
                );
            }
        }
    }

    fn report_unused(&mut self) {
        let mut unused: Vec<(String, scarlet_syntax::span::Span)> = self
            .binders
            .iter()
            .filter(|b| !b.used && !b.name.starts_with('_'))
            .map(|b| (b.name.to_string(), b.span))
            .collect();
        unused.sort_by_key(|(_, sp)| (sp.start_line, sp.start_column));
        for (name, span) in unused {
            self.unused_binding(&name, span);
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
        PatternKind::Ctor { fields: ps, .. } | PatternKind::Tuple(ps) | PatternKind::Array { prefix: ps, .. } | PatternKind::Or(ps) => {
            ps.iter().for_each(|q| walk_patterns(q, f))
        }
        PatternKind::Binary { segs, .. } => {
            for s in segs {
                match s {
                    BinSegPat::Text(_) => {}
                    BinSegPat::Utf8(q) | BinSegPat::Bits { value: q, .. } | BinSegPat::Bytes { value: q, .. } => {
                        walk_patterns(q, f)
                    }
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
            PatternKind::Binary { segs, rest } if segs.is_empty() && !matches!(rest, Rest::None) => PatShape::Wild,
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
    let (part, size) = (bin_part, size_part);
    match seg {
        BinSegPat::Text(s) => BinSegShape {
            value: BinPart::Str(s),
            unit: BinUnit::Utf8,
            size: None,
        },
        BinSegPat::Utf8(p) => BinSegShape {
            value: part(p),
            unit: BinUnit::Utf8,
            size: None,
        },
        BinSegPat::Bits { bits, value } => BinSegShape {
            value: part(value),
            unit: BinUnit::Bits,
            size: size(bits),
        },
        BinSegPat::Bytes { bytes, value } => BinSegShape {
            value: part(value),
            unit: BinUnit::Bytes,
            size: size(bytes),
        },
    }
}
