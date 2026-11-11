//! The names a pattern binds and the constructors it names. A name comes
//! into scope as soon as the pattern reaches it, so a binary segment's size
//! can read a name an earlier segment bound.

use std::collections::HashSet;

use indexmap::IndexMap;
use scarlet_syntax::ast;
use scarlet_syntax::span::Span;
use scarlet_types::interface::ValueKind;
use scarlet_types::name::Name;

use crate::env::Env;
use crate::error::{CheckError, FieldSite};
use crate::index::{EntityKind, UseKind};
use crate::resolve::{CtorInfo, Number, Resolver, Walk, number};
use crate::resolved::{RBinSegPat, RPatKind, RPattern};
use crate::typed::{BinderId, Rest};

/// The names one pattern has bound so far, and the or-patterns it is inside.
#[derive(Default)]
struct PatBinds {
    bound: IndexMap<Name, BinderId>,
    /// The innermost last.
    ors: Vec<OrFrame>,
}

/// Which alternative of an `|` pattern is being resolved.
enum OrFrame {
    /// The first, which sets the names every later one must bind.
    First { seen: HashSet<Name> },
    /// A later one.
    Later {
        canonical: HashSet<Name>,
        seen: HashSet<Name>,
    },
}

impl OrFrame {
    fn seen(&mut self) -> &mut HashSet<Name> {
        match self {
            OrFrame::First { seen } | OrFrame::Later { seen, .. } => seen,
        }
    }
}

impl Resolver<'_, '_> {
    pub fn pattern(&mut self, pat: &ast::Pattern, w: &Walk) -> RPattern {
        let mut pb = PatBinds::default();
        self.pat(pat, &mut pb, w)
    }

    fn pat(&mut self, pat: &ast::Pattern, pb: &mut PatBinds, w: &Walk) -> RPattern {
        let span = pat.span();
        let kind = match pat {
            ast::Pattern::Var { name } => match self.bind_name(name, pb) {
                Some(b) => RPatKind::Bind(b),
                None => RPatKind::Wild,
            },
            ast::Pattern::Literal(ast::PatternLiteral::Number(n)) => {
                match number(n, &mut self.report) {
                    Number::Int(i) => RPatKind::Int(i),
                    Number::Float(f) => RPatKind::Float(f),
                }
            }
            ast::Pattern::Literal(ast::PatternLiteral::String(s)) => {
                RPatKind::String(s.value.clone())
            }
            ast::Pattern::Range { start, end, .. } => {
                let bound = |r: &mut Self, n: &ast::NumberLiteral| match number(n, &mut r.report) {
                    Number::Int(i) => Some(i),
                    Number::Float(_) => {
                        r.report
                            .add(n.span, CheckError::RangeBoundNotInt(n.value.clone()));
                        None
                    }
                };
                match (bound(self, start), bound(self, end)) {
                    (Some(lo), Some(hi)) => RPatKind::Range(lo, hi),
                    _ => RPatKind::Error,
                }
            }
            ast::Pattern::Tuple { elements, .. } => {
                RPatKind::Tuple(elements.iter().map(|p| self.pat(p, pb, w)).collect())
            }
            ast::Pattern::Array { elements, .. } => {
                let mut prefix = Vec::with_capacity(elements.len());
                let mut rest = Rest::None;
                let mut seen_spread = false;
                let mut ok = true;
                for (i, el) in elements.iter().enumerate() {
                    match el {
                        ast::ArrayPatternElement::Pattern(p) => prefix.push(self.pat(p, pb, w)),
                        ast::ArrayPatternElement::Spread { binding, span: ssp } => {
                            if seen_spread {
                                self.report.add(*ssp, CheckError::TwoSpreads);
                                ok = false;
                            } else if i != elements.len() - 1 {
                                self.report.add(*ssp, CheckError::SpreadNotLast);
                                ok = false;
                            }
                            seen_spread = true;
                            rest = self.rest(binding.as_ref(), pb);
                        }
                    }
                }
                if ok {
                    RPatKind::Array { prefix, rest }
                } else {
                    RPatKind::Error
                }
            }
            ast::Pattern::Binary { segments, rest, .. } => {
                let mut segs = Vec::with_capacity(segments.len());
                for seg in segments {
                    if let Some(s) = seg.utf8_literal() {
                        segs.push(RBinSegPat::Text(s.to_string()));
                        continue;
                    }
                    segs.push(match &seg.spec {
                        ast::BinSpec::Int { bits } => RBinSegPat::Bits {
                            bits: bits.as_ref().map(|b| self.expr(b, w)),
                            value: self.pat(&seg.value, pb, w),
                        },
                        ast::BinSpec::Binary { bytes } => RBinSegPat::Bytes {
                            bytes: bytes.as_ref().map(|b| self.expr(b, w)),
                            value: self.pat(&seg.value, pb, w),
                        },
                        ast::BinSpec::Utf8 => RBinSegPat::Utf8(self.pat(&seg.value, pb, w)),
                    });
                }
                let rest = match rest {
                    None => Rest::None,
                    Some(r) => self.rest(r.binding.as_ref(), pb),
                };
                RPatKind::Binary { segs, rest }
            }
            ast::Pattern::Or { first, rest, .. } => self.or(first, rest, pb, w),
            ast::Pattern::Constructor {
                qualifier,
                name,
                args,
                rest,
                ..
            } => self.ctor_pattern(qualifier.as_ref(), name, args, *rest, span, pb, w),
        };
        RPattern { span, kind }
    }

    fn rest(&mut self, binding: Option<&ast::Identifier>, pb: &mut PatBinds) -> Rest {
        match binding.map(|id| self.bind_name(id, pb)) {
            Some(Some(b)) => Rest::Bind(b),
            Some(None) | None => Rest::Discard,
        }
    }

    /// `p | q | ..`: the first alternative sets the names, and every later
    /// one must bind exactly those.
    fn or(
        &mut self,
        first: &ast::Pattern,
        rest: &[ast::Pattern],
        pb: &mut PatBinds,
        w: &Walk,
    ) -> RPatKind {
        pb.ors.push(OrFrame::First {
            seen: HashSet::new(),
        });
        let mut alts = vec![self.pat(first, pb, w)];
        for alt in rest {
            let canonical = match pb.ors.pop() {
                Some(OrFrame::First { seen }) => seen,
                Some(OrFrame::Later { canonical, .. }) => canonical,
                None => HashSet::new(),
            };
            pb.ors.push(OrFrame::Later {
                canonical: canonical.clone(),
                seen: HashSet::new(),
            });
            alts.push(self.pat(alt, pb, w));
            if let Some(OrFrame::Later { seen, .. }) = pb.ors.last() {
                for name in pb.bound.keys() {
                    if canonical.contains(name) && !seen.contains(name) {
                        self.report.add(
                            alt.span(),
                            CheckError::UnboundInSomeAlternative(name.clone()),
                        );
                    }
                }
            }
        }
        let names = match pb.ors.pop() {
            Some(OrFrame::First { seen }) => seen,
            Some(OrFrame::Later { canonical, .. }) => canonical,
            None => HashSet::new(),
        };
        if let Some(parent) = pb.ors.last_mut() {
            parent.seen().extend(names);
        }
        RPatKind::Or(alts)
    }

    /// Bind a name the pattern reaches. `None` for a `_name`, which binds
    /// nothing anyone may read, or one refused with a diagnostic.
    fn bind_name(&mut self, id: &ast::Identifier, pb: &mut PatBinds) -> Option<BinderId> {
        let name: Name = id.name.as_str().into();
        if id.name.starts_with('_') {
            self.bind(id, true);
            return None;
        }
        let checking = pb.ors.iter().rev().find_map(|f| match f {
            OrFrame::Later { canonical, .. } => Some(canonical),
            OrFrame::First { .. } => None,
        });
        match checking {
            Some(canonical) => {
                let in_alt = pb
                    .ors
                    .last()
                    .is_some_and(|f| matches!(f, OrFrame::First { seen } | OrFrame::Later { seen, .. } if seen.contains(&name)));
                if in_alt {
                    self.report
                        .add(id.span, CheckError::BoundTwice(name.clone()));
                    return None;
                }
                if !canonical.contains(&name) {
                    self.report
                        .add(id.span, CheckError::NotInFirstAlternative(name.clone()));
                    return None;
                }
                let b = pb.bound.get(&name).copied()?;
                if let Some(f) = pb.ors.last_mut() {
                    f.seen().insert(name);
                }
                Some(b)
            }
            None => {
                if pb.bound.contains_key(&name) {
                    self.report
                        .add(id.span, CheckError::BoundTwice(name.clone()));
                    return None;
                }
                let b = self.bind(id, false);
                pb.bound.insert(name.clone(), b);
                if let Some(f) = pb.ors.last_mut() {
                    f.seen().insert(name);
                }
                Some(b)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn ctor_pattern(
        &mut self,
        qualifier: Option<&ast::Identifier>,
        name: &ast::Identifier,
        args: &[ast::PatternArg],
        rest: bool,
        span: Span,
        pb: &mut PatBinds,
        w: &Walk,
    ) -> RPatKind {
        let found = match qualifier {
            Some(q) => self.qualified_ctor(q, name, w),
            None => self.bare_ctor(name, w),
        };
        let Some(c) = found else {
            return RPatKind::Error;
        };
        if c.labels.is_empty() {
            if !args.is_empty() {
                self.report.add(
                    span,
                    CheckError::ConstructorTakesNoArguments {
                        ctor: name.name.as_str().into(),
                        given: args.len(),
                    },
                );
                return RPatKind::Error;
            }
            return RPatKind::Ctor {
                ctor: c.ctor,
                scheme: c.scheme,
                fields: Vec::new(),
                at: name.span,
            };
        }
        let items = args
            .iter()
            .map(|a| match a {
                ast::PatternArg::Positional(p) => (None, (p, p.span())),
                ast::PatternArg::Labeled { label, pattern, .. } => {
                    (Some(label.name.as_str()), (pattern, label.span))
                }
            })
            .collect();
        let by_pos = self.slot_ctor(&c, items, (!rest).then_some((span, FieldSite::Pattern)));
        let fields = by_pos
            .into_iter()
            .map(|sub| match sub {
                Some(p) => self.pat(p, pb, w),
                None => RPattern {
                    span,
                    kind: RPatKind::Wild,
                },
            })
            .collect();
        RPatKind::Ctor {
            ctor: c.ctor,
            scheme: c.scheme,
            fields,
            at: name.span,
        }
    }

    /// The constructor a bare name in a pattern means.
    fn bare_ctor(&mut self, name: &ast::Identifier, w: &Walk) -> Option<CtorInfo> {
        if let Some(c) = self.defs.ctors.get(name.name.as_str()) {
            let target = self.env.site(c.span, EntityKind::Constructor);
            self.use_of(name.span, UseKind::Unqualified, target, w);
            return Some(CtorInfo {
                ctor: c.ctor.clone(),
                name: name.name.as_str().into(),
                labels: c.labels.clone(),
                scheme: c.scheme.clone(),
            });
        }
        if let Some((key, v)) = self.env.prelude_value(&name.name)
            && let ValueKind::Constructor {
                ctor,
                name: n,
                labels,
            } = &v.kind
        {
            let target = Env::foreign_site(key, v);
            self.use_of(name.span, UseKind::Unqualified, target, w);
            return Some(CtorInfo {
                ctor: ctor.clone(),
                name: n.clone(),
                labels: labels.clone(),
                scheme: v.scheme.clone(),
            });
        }
        let n: Name = name.name.as_str().into();
        let e = if self.is_value_name(&name.name) {
            CheckError::PatternNotConstructor(n)
        } else if self.defs.types.contains_key(name.name.as_str())
            || self
                .env
                .prelude
                .is_some_and(|p| p.public_type(&name.name).is_some())
        {
            CheckError::PatternIsType(n)
        } else {
            let qualifiers = self.env.qualifiers_with_ctor(&name.name);
            if qualifiers.is_empty() {
                CheckError::UnknownConstructor(n)
            } else {
                CheckError::UnknownConstructorElsewhere {
                    name: n,
                    qualifiers,
                }
            }
        };
        self.report.add(name.span, e);
        None
    }

    /// `io::NotFound` in a pattern. Every `None` has been reported.
    fn qualified_ctor(
        &mut self,
        q: &ast::Identifier,
        name: &ast::Identifier,
        w: &Walk,
    ) -> Option<CtorInfo> {
        let Some(imp) = self.env.imports.get(&q.name) else {
            self.report.add(
                name.span,
                CheckError::UnknownQualifier(q.name.as_str().into()),
            );
            return None;
        };
        let key = imp.key.clone();
        let module = self.env.imports.module_name(&key);
        let Some(iface) = self.env.interface_of(&key) else {
            self.report.add(
                name.span,
                CheckError::NoConstructorInModule {
                    module,
                    name: name.name.as_str().into(),
                },
            );
            return None;
        };
        let Some(v) = iface.values.get(name.name.as_str()) else {
            let n: Name = name.name.as_str().into();
            let e = if iface.private_names.contains(name.name.as_str()) {
                CheckError::PrivateConstructor { module, name: n }
            } else {
                CheckError::NoConstructorInModule { module, name: n }
            };
            self.report.add(name.span, e);
            return None;
        };
        let ValueKind::Constructor {
            ctor,
            name: n,
            labels,
        } = &v.kind
        else {
            self.report.add(
                name.span,
                CheckError::QualifiedNotConstructor {
                    module,
                    name: name.name.as_str().into(),
                },
            );
            return None;
        };
        let target = Env::foreign_site(&key, v);
        self.use_of(name.span, UseKind::Qualified, target, w);
        self.qualifier_use(q, w);
        Some(CtorInfo {
            ctor: ctor.clone(),
            name: n.clone(),
            labels: labels.clone(),
            scheme: v.scheme.clone(),
        })
    }
}
