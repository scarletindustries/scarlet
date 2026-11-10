//! Patterns: their types, the constructors they name, and the names they
//! bind. A name comes into scope as soon as the pattern reaches it, so a
//! binary segment's size can read a name an earlier segment bound.

use std::collections::HashSet;

use indexmap::IndexMap;
use scarlet_syntax::ast;
use scarlet_syntax::span::Span;
use scarlet_types::engine::{LocalScheme, Shape, Ty};
use scarlet_types::interface::ValueKind;
use scarlet_types::name::{CtorName, Name};
use scarlet_types::slots::{SlotError, slot_labeled};

use crate::check::Checker;
use crate::index::UseKind;
use crate::typed::{BinSegPat, BinderId, Pattern, PatternKind, Rest};

/// The names one pattern has bound so far, and the or-patterns it is inside.
#[derive(Default)]
pub(crate) struct PatBinds {
    bound: IndexMap<Name, (BinderId, Ty)>,
    ors: Vec<OrFrame>,
}

struct OrFrame {
    /// The names the first alternative bound, once it is done. Every later
    /// alternative must bind exactly these.
    canonical: Option<HashSet<Name>>,
    /// The names the current alternative has bound.
    seen: HashSet<Name>,
}

enum Bound {
    Binder(BinderId),
    /// A `_name`: bound so hover sees it, but nothing may read it.
    WriteOnly,
    Rejected,
}

/// A constructor a pattern or call names.
pub(crate) struct CtorRef {
    pub ctor: CtorName,
    pub name: Name,
    pub labels: Vec<Name>,
    pub ty: Ty,
}

impl Checker<'_> {
    /// Check `pat` against `expected`. The `bool` is false when the pattern
    /// doesn't typecheck, in which case nothing may check it for
    /// exhaustiveness.
    pub fn check_pattern(&mut self, pat: &ast::Pattern, expected: Ty, pb: &mut PatBinds) -> (Pattern, bool) {
        let span = pat.span();
        let at = |kind| Pattern {
            ty: expected,
            span,
            kind,
        };
        match pat {
            ast::Pattern::Var { name } => match self.bind_pattern_name(name, expected, pb) {
                Bound::Binder(b) => (at(PatternKind::Bind(b)), true),
                Bound::WriteOnly => (at(PatternKind::Wild), true),
                Bound::Rejected => (at(PatternKind::Error), false),
            },
            ast::Pattern::Literal(ast::PatternLiteral::Number(n)) => {
                let (lit_ty, kind) = match self.number(n) {
                    Number::Int(i) => (self.e.int(), PatternKind::Int(i)),
                    Number::Float(f) => (self.e.float(), PatternKind::Float(f)),
                };
                let ok = self.unify(expected, lit_ty, n.span);
                (at(kind), ok)
            }
            ast::Pattern::Literal(ast::PatternLiteral::String(s)) => {
                let t = self.e.string();
                let ok = self.unify(expected, t, s.span);
                (at(PatternKind::String(s.value.clone())), ok)
            }
            ast::Pattern::Range { start, end, .. } => {
                let int = self.e.int();
                let mut ok = self.unify(expected, int, span);
                let mut bound = |c: &mut Self, n: &ast::NumberLiteral| match c.number(n) {
                    Number::Int(i) => i,
                    Number::Float(_) => {
                        c.error(
                            n.span,
                            format!("Range pattern bound must be an integer, got '{}'", n.value),
                        );
                        ok = false;
                        0
                    }
                };
                let lo = bound(self, start);
                let hi = bound(self, end);
                (at(PatternKind::Range(lo, hi)), ok)
            }
            ast::Pattern::Tuple { elements, .. } => {
                let fresh: Vec<Ty> = elements.iter().map(|_| self.e.fresh()).collect();
                let tup = self.e.tuple(fresh.clone());
                let mut ok = self.unify(expected, tup, span);
                let mut elems = Vec::with_capacity(elements.len());
                for (el, ty) in elements.iter().zip(fresh) {
                    let (p, p_ok) = self.check_pattern(el, ty, pb);
                    ok &= p_ok;
                    elems.push(p);
                }
                (at(PatternKind::Tuple(elems)), ok)
            }
            ast::Pattern::Array { elements, .. } => {
                let elem = self.e.fresh();
                let arr = self.e.array(elem);
                let mut ok = self.unify(expected, arr, span);
                let mut prefix = Vec::with_capacity(elements.len());
                let mut rest = Rest::None;
                let mut seen_spread = false;
                for (i, el) in elements.iter().enumerate() {
                    match el {
                        ast::ArrayPatternElement::Pattern(p) => {
                            let (p, p_ok) = self.check_pattern(p, elem, pb);
                            ok &= p_ok;
                            prefix.push(p);
                        }
                        ast::ArrayPatternElement::Spread { binding, span: ssp } => {
                            if seen_spread {
                                self.error(*ssp, "An array pattern can only have one spread");
                                ok = false;
                            } else if i != elements.len() - 1 {
                                self.error(*ssp, "The spread in an array pattern must come last");
                                ok = false;
                            }
                            seen_spread = true;
                            rest = match binding {
                                None => Rest::Discard,
                                Some(id) => {
                                    let ty = self.e.array(elem);
                                    match self.bind_pattern_name(id, ty, pb) {
                                        Bound::Binder(b) => Rest::Bind(b),
                                        Bound::WriteOnly => Rest::Discard,
                                        Bound::Rejected => {
                                            ok = false;
                                            Rest::Discard
                                        }
                                    }
                                }
                            };
                        }
                    }
                }
                (at(PatternKind::Array { prefix, rest }), ok)
            }
            ast::Pattern::Binary { segments, rest, .. } => {
                let bin = self.e.binary();
                let mut ok = self.unify(expected, bin, span);
                let mut segs = Vec::with_capacity(segments.len());
                for seg in segments {
                    if let Some(s) = seg.utf8_literal() {
                        segs.push(BinSegPat::Text(s.to_string()));
                        continue;
                    }
                    let s = match &seg.spec {
                        ast::BinSpec::Int { bits } => {
                            let bits = bits.as_ref().map(|b| self.check_size(b));
                            let int = self.e.int();
                            let (value, v_ok) = self.check_pattern(&seg.value, int, pb);
                            ok &= v_ok;
                            BinSegPat::Bits { bits, value }
                        }
                        ast::BinSpec::Binary { bytes } => {
                            let bytes = bytes.as_ref().map(|b| self.check_size(b));
                            let bin = self.e.binary();
                            let (value, v_ok) = self.check_pattern(&seg.value, bin, pb);
                            ok &= v_ok;
                            BinSegPat::Bytes { bytes, value }
                        }
                        ast::BinSpec::Utf8 => {
                            let int = self.e.int();
                            let (value, v_ok) = self.check_pattern(&seg.value, int, pb);
                            ok &= v_ok;
                            BinSegPat::Utf8(value)
                        }
                    };
                    segs.push(s);
                }
                let rest = match rest {
                    None => Rest::None,
                    Some(r) => match &r.binding {
                        None => Rest::Discard,
                        Some(id) => {
                            let bin = self.e.binary();
                            match self.bind_pattern_name(id, bin, pb) {
                                Bound::Binder(b) => Rest::Bind(b),
                                Bound::WriteOnly => Rest::Discard,
                                Bound::Rejected => {
                                    ok = false;
                                    Rest::Discard
                                }
                            }
                        }
                    },
                };
                (at(PatternKind::Binary { segs, rest }), ok)
            }
            ast::Pattern::Or { first, rest, .. } => {
                pb.ors.push(OrFrame {
                    canonical: None,
                    seen: HashSet::new(),
                });
                let (head, mut ok) = self.check_pattern(first, expected, pb);
                let mut alts = vec![head];
                if let Some(f) = pb.ors.last_mut() {
                    f.canonical = Some(std::mem::take(&mut f.seen));
                }
                for alt in rest {
                    let (p, p_ok) = self.check_pattern(alt, expected, pb);
                    ok &= p_ok;
                    alts.push(p);
                    let Some(frame) = pb.ors.last_mut() else { break };
                    let seen = std::mem::take(&mut frame.seen);
                    let canonical = frame.canonical.clone().unwrap_or_default();
                    for name in pb.bound.keys() {
                        if canonical.contains(name) && !seen.contains(name) {
                            self.error(
                                alt.span(),
                                format!("Variable '{name}' must be bound in every alternative of this pattern"),
                            );
                            ok = false;
                        }
                    }
                }
                if let Some(frame) = pb.ors.pop()
                    && let Some(parent) = pb.ors.last_mut()
                {
                    parent.seen.extend(frame.canonical.unwrap_or_default());
                }
                (at(PatternKind::Or(alts)), ok)
            }
            ast::Pattern::Constructor {
                qualifier,
                name,
                args,
                rest,
                ..
            } => self.check_ctor_pattern(qualifier.as_ref(), name, args, *rest, span, expected, pb),
        }
    }

    fn check_size(&mut self, e: &ast::Expression) -> crate::typed::Expr {
        let size = self.check_expr(e);
        let int = self.e.int();
        self.unify(int, size.ty, e.span());
        size
    }

    fn bind_pattern_name(&mut self, id: &ast::Identifier, ty: Ty, pb: &mut PatBinds) -> Bound {
        let name: Name = id.name.as_str().into();
        if id.name.starts_with('_') {
            let b = self.bind(&id.name, LocalScheme::mono(ty), id.span);
            self.binders[b.0 as usize].discards = true;
            return Bound::WriteOnly;
        }
        let checking = pb.ors.iter().rev().find_map(|f| f.canonical.as_ref());
        match checking {
            Some(canonical) => {
                let in_alt = pb.ors.last().is_some_and(|f| f.seen.contains(&name));
                if in_alt || !canonical.contains(&name) {
                    if in_alt {
                        self.error(id.span, format!("Variable '{name}' is bound more than once in this pattern"));
                    } else {
                        self.error(
                            id.span,
                            format!(
                                "Variable '{name}' is not bound in the first alternative of this pattern. Every alternative must bind the same names"
                            ),
                        );
                    }
                    return Bound::Rejected;
                }
                let Some(&(binder, first_ty)) = pb.bound.get(&name) else {
                    return Bound::Rejected;
                };
                if let Some(f) = pb.ors.last_mut() {
                    f.seen.insert(name);
                }
                if self.unify(first_ty, ty, id.span) {
                    Bound::Binder(binder)
                } else {
                    Bound::Rejected
                }
            }
            None => {
                if pb.bound.contains_key(&name) {
                    self.error(id.span, format!("Variable '{name}' is bound more than once in this pattern"));
                    return Bound::Rejected;
                }
                let b = self.bind_mono(&id.name, ty, id.span);
                pb.bound.insert(name.clone(), (b, ty));
                if let Some(f) = pb.ors.last_mut() {
                    f.seen.insert(name);
                }
                Bound::Binder(b)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn check_ctor_pattern(
        &mut self,
        qualifier: Option<&ast::Identifier>,
        name: &ast::Identifier,
        args: &[ast::PatternArg],
        rest: bool,
        span: Span,
        expected: Ty,
        pb: &mut PatBinds,
    ) -> (Pattern, bool) {
        let error = Pattern {
            ty: expected,
            span,
            kind: PatternKind::Error,
        };
        let found = match qualifier {
            Some(q) => self.qualified_ctor(q, name),
            None => self.bare_ctor_in_pattern(name),
        };
        let Some(c) = found else {
            return (error, false);
        };
        let params = match self.e.shape(c.ty) {
            Shape::Fn(params, ret) => {
                let mut ok = self.unify(expected, ret, span);
                let items = args.iter().map(|a| match a {
                    ast::PatternArg::Positional(p) => (None, (p, p.span())),
                    ast::PatternArg::Labeled { label, pattern, .. } => {
                        (Some(label.name.as_str()), (pattern, label.span))
                    }
                });
                let (by_pos, args_ok) = self.slot_ctor(&c, items, (!rest).then_some((span, ". Use '..' to ignore them")));
                ok &= args_ok;
                let mut fields = Vec::with_capacity(params.len());
                for (i, sub) in by_pos.into_iter().enumerate() {
                    let ty = params.get(i).copied().unwrap_or_else(|| self.e.fresh());
                    let p = match sub {
                        Some(p) => {
                            let (p, p_ok) = self.check_pattern(p, ty, pb);
                            ok &= p_ok;
                            p
                        }
                        None => Pattern {
                            ty,
                            span,
                            kind: PatternKind::Wild,
                        },
                    };
                    fields.push(p);
                }
                (fields, ok)
            }
            Shape::Var(_) | Shape::Con(..) | Shape::Tuple(_) => {
                let mut ok = self.unify(expected, c.ty, span);
                if !args.is_empty() {
                    self.error(
                        span,
                        format!(
                            "Constructor '{}' takes no arguments but {} were given",
                            name.name,
                            args.len()
                        ),
                    );
                    ok = false;
                }
                (Vec::new(), ok)
            }
        };
        let (fields, ok) = params;
        (
            Pattern {
                ty: expected,
                span,
                kind: PatternKind::Ctor { ctor: c.ctor, fields },
            },
            ok,
        )
    }

    /// The constructor a bare name in a pattern means.
    fn bare_ctor_in_pattern(&mut self, name: &ast::Identifier) -> Option<CtorRef> {
        if let Some(c) = self.bare_ctor(name) {
            return Some(c);
        }
        let qualified = self.qualified_spellings(&name.name);
        let msg = if self.is_value_name(&name.name) {
            format!("'{}' is not a constructor and cannot be used in a pattern", name.name)
        } else if self.types.contains_key(name.name.as_str())
            || self.prelude.is_some_and(|p| p.public_type(&name.name).is_some())
        {
            format!(
                "'{0}' is a type, not a constructor. To check a value's type and drop it, write `_ {0} = value`",
                name.name
            )
        } else if !qualified.is_empty() {
            format!(
                "Unknown constructor '{}' in pattern. It is {}",
                name.name,
                qualified.join(" or ")
            )
        } else {
            format!("Unknown constructor '{}' in pattern", name.name)
        };
        self.error(name.span, msg);
        None
    }

    /// A constructor named by a bare name, instantiated, with its use
    /// recorded. `None` when the name means no constructor, reported by the
    /// caller.
    pub fn bare_ctor(&mut self, name: &ast::Identifier) -> Option<CtorRef> {
        if let Some(v) = self.values.get(name.name.as_str()).cloned() {
            let ValueKind::Constructor { ctor, name: n, labels } = v.kind else {
                return None;
            };
            let ty = self.instantiate_value(&v.scheme);
            let target = self.site(v.span, crate::index::EntityKind::Constructor);
            self.use_site(name.span, UseKind::Unqualified, target);
            self.hovers.push((name.span, name.name.as_str().into(), ty));
            return Some(CtorRef { ctor, name: n, labels, ty });
        }
        let p = self.prelude?;
        let v = p.values.get(name.name.as_str())?;
        let ValueKind::Constructor { ctor, name: n, labels } = &v.kind else {
            return None;
        };
        let ty = self.e.instantiate_scheme(&v.scheme);
        let target = self.foreign_site(&p.key, v);
        self.use_site(name.span, UseKind::Unqualified, target);
        self.hovers.push((name.span, name.name.as_str().into(), ty));
        Some(CtorRef {
            ctor: ctor.clone(),
            name: n.clone(),
            labels: labels.clone(),
            ty,
        })
    }

    /// `io::NotFound` in a pattern. Every `None` has been reported.
    fn qualified_ctor(&mut self, q: &ast::Identifier, name: &ast::Identifier) -> Option<CtorRef> {
        let Some(key) = self.qualifiers.get(q.name.as_str()).cloned() else {
            self.error(
                name.span,
                format!("Unknown module qualifier '{}'. Import the module first", q.name),
            );
            return None;
        };
        let module = self.module_name(&key);
        let iface = self.interface_of(&key)?;
        let Some(v) = iface.values.get(name.name.as_str()) else {
            let msg = if iface.private_names.contains(name.name.as_str()) {
                format!("Constructor '{}' is private in module '{module}'", name.name)
            } else {
                format!("Module '{module}' has no constructor '{}'", name.name)
            };
            self.error(name.span, msg);
            return None;
        };
        let ValueKind::Constructor { ctor, name: n, labels } = &v.kind else {
            self.error(
                name.span,
                format!(
                    "'{module}::{}' is not a constructor and cannot be used in a pattern",
                    name.name
                ),
            );
            return None;
        };
        let ty = self.e.instantiate_scheme(&v.scheme);
        let target = self.foreign_site(&key, v);
        self.use_site(name.span, UseKind::Qualified, target);
        self.qualifier_use(q);
        self.hovers.push((name.span, name.name.as_str().into(), ty));
        Some(CtorRef {
            ctor: ctor.clone(),
            name: n.clone(),
            labels: labels.clone(),
            ty,
        })
    }

    /// How to reach a constructor `name` that isn't in scope: through each
    /// imported module that has one.
    pub fn qualified_spellings(&self, name: &str) -> Vec<String> {
        let mut out: Vec<String> = self
            .qualifiers
            .iter()
            .filter(|(_, key)| {
                self.interface_of(key)
                    .and_then(|i| i.values.get(name))
                    .is_some_and(|v| matches!(v.kind, ValueKind::Constructor { .. }))
            })
            .map(|(q, _)| format!("`{q}::{name}`"))
            .collect();
        out.sort();
        out
    }

    /// Put a constructor's arguments, positional and labelled, into the order
    /// its fields are declared in. With `missing`, a field nothing gives is
    /// an error too. Calls and patterns both come through here.
    pub fn slot_ctor<'x, T>(
        &mut self,
        c: &CtorRef,
        items: impl IntoIterator<Item = (Option<&'x str>, (T, Span))>,
        missing: Option<(Span, &str)>,
    ) -> (Vec<Option<T>>, bool) {
        let fields: Vec<Option<&str>> = c.labels.iter().map(|l| Some(&**l)).collect();
        let arity = fields.len();
        let items: Vec<(Option<&str>, (T, Span))> = items.into_iter().collect();
        let (by_pos, errors) = slot_labeled(&fields, items);
        let mut ok = errors.is_empty();
        for e in errors {
            match e {
                SlotError::ExtraPositional((_, span)) => self.error(
                    span,
                    format!("Constructor '{}' has {arity} field(s) but more were supplied", c.name),
                ),
                SlotError::UnknownLabel(label, (_, span)) => self.error(
                    span,
                    format!(
                        "Constructor '{}' has no field '{label}'. Available: {}",
                        c.name,
                        c.labels.join(", ")
                    ),
                ),
                SlotError::Duplicate((_, span), field) => {
                    let dup = c.labels.get(field).map_or("_", |l| &**l);
                    self.error(span, format!("Field '{dup}' is specified more than once"));
                }
            }
        }
        if let Some((span, hint)) = missing
            && by_pos.iter().any(Option::is_none)
        {
            let absent: Vec<&str> = (0..arity)
                .filter(|&i| by_pos[i].is_none())
                .map(|i| c.labels.get(i).map_or("_", |l| &**l))
                .collect();
            self.error(
                span,
                format!(
                    "Constructor '{}' is missing field(s): {}{hint}",
                    c.name,
                    absent.join(", ")
                ),
            );
            ok = false;
        }
        (by_pos.into_iter().map(|s| s.map(|(v, _)| v)).collect(), ok)
    }
}

/// A number literal's value.
pub(crate) enum Number {
    Int(i64),
    Float(f64),
}

impl Checker<'_> {
    /// The value of a number literal. Out of range is reported, and gives a
    /// zero of the same kind so checking carries on.
    pub fn number(&mut self, n: &ast::NumberLiteral) -> Number {
        let digits = n.digits();
        if digits.contains('.') {
            match digits.parse() {
                Ok(f) => Number::Float(f),
                Err(_) => {
                    self.error(n.span, format!("invalid number literal '{}'", n.value));
                    Number::Float(0.0)
                }
            }
        } else {
            match n.as_int() {
                Some(i) => Number::Int(i),
                None => {
                    self.error(
                        n.span,
                        format!(
                            "integer literal '{}' out of range for Int (must be between {} and {})",
                            n.value,
                            i64::MIN,
                            i64::MAX
                        ),
                    );
                    Number::Int(0)
                }
            }
        }
    }
}
