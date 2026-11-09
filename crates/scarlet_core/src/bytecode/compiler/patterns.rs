//! Checks patterns. This is [`ModuleChecker::type_pattern`], plus the helpers
//! it uses to look up constructors and put their arguments in slots.

use super::*;

impl ModuleChecker<'_> {
    /// Type-check a pattern against `expected` and resolve it. Constructors
    /// resolve to their variants, and each name to a new binding, which comes
    /// into scope as the pattern reaches it. A segment's size expression is
    /// checked before the segment's own value, so it sees the names that
    /// earlier segments bind and nothing later.
    ///
    /// `binders` holds the binder of each name `b` has recorded, by its index
    /// there. The `bool` is false when the pattern doesn't typecheck. The
    /// caller must pass that on, so the exhaustiveness checks, which assume
    /// well-typed patterns, are skipped.
    pub(super) fn type_pattern(
        &mut self,
        pat: &ast::Pattern,
        expected: Ty,
        b: &mut PatternSink<'_>,
        binders: &mut Vec<BinderId>,
    ) -> (CPat, bool) {
        let span = pat.span();
        let at = |kind| CPat {
            ty: expected,
            span,
            kind,
        };
        match pat {
            ast::Pattern::Binary { segments, rest, .. } => {
                let bin_ty = self.c.ty_binary();
                let mut ok = self.c.engine.unify_at(expected, bin_ty, span);
                let mut segs = Vec::with_capacity(segments.len());
                for seg in segments {
                    if let Some(s) = seg.utf8_literal() {
                        segs.push(CBinPatSeg::Literal(s.to_string()));
                        continue;
                    }
                    let (width, val_ty) = match &seg.spec {
                        ast::BinSpec::Int { bits } => (
                            CWidth::Int(self.type_seg_size(bits.as_ref())),
                            self.c.ty_int(),
                        ),
                        ast::BinSpec::Binary { bytes } => (
                            CWidth::Bytes(self.type_seg_size(bytes.as_ref())),
                            self.c.ty_binary(),
                        ),
                        ast::BinSpec::Utf8 => (CWidth::Utf8, self.c.ty_int()),
                    };
                    let (value, v_ok) = self.type_pattern(&seg.value, val_ty, b, binders);
                    ok &= v_ok;
                    segs.push(CBinPatSeg::Value {
                        width,
                        value: Box::new(value),
                    });
                }
                let rest = match rest {
                    None => CRest::None,
                    Some(r) => match &r.binding {
                        None => CRest::Discard,
                        Some(id) => {
                            let rest_ty = self.c.ty_binary();
                            let (rest, r_ok) = self.bind_rest(id, rest_ty, b, binders);
                            ok &= r_ok;
                            rest
                        }
                    },
                };
                (at(CPatKind::Binary { segs, rest }), ok)
            }
            ast::Pattern::Var { name } => {
                let (bound, ok) = self.bind_pattern_name(name, expected, b, binders);
                let kind = match bound {
                    Some((name, binder)) => CPatKind::Bind { name, binder },
                    None if ok => CPatKind::Wild,
                    None => CPatKind::Error,
                };
                (at(kind), ok)
            }
            ast::Pattern::Literal(lit) => {
                let (lit_ty, sp, kind) = match lit {
                    // `const_number` raises the out-of-range/malformed error.
                    ast::PatternLiteral::Number(n) => {
                        let v = self.c.const_number(n);
                        let ty = if matches!(v, crate::core_ir::Const::Float(_)) {
                            self.c.engine.icon_float()
                        } else {
                            self.c.ty_int()
                        };
                        (ty, n.span, CPatKind::Number(n.clone()))
                    }
                    ast::PatternLiteral::String(s) => (
                        self.c.ty_string(),
                        s.span,
                        CPatKind::String(s.value.clone()),
                    ),
                };
                let ok = self.c.engine.unify_at(expected, lit_ty, sp);
                (at(kind), ok)
            }
            ast::Pattern::Range { start, end, .. } => {
                let int_t = self.c.ty_int();
                let mut ok = self.c.engine.unify_at(expected, int_t, span);
                for bound in [start, end] {
                    // `const_number` raises the out-of-range/malformed error.
                    let v = self.c.const_number(bound);
                    if matches!(v, crate::core_ir::Const::Float(_)) {
                        self.c.error(
                            format!(
                                "Range pattern bound must be an integer, got '{}'",
                                bound.value
                            ),
                            bound.span,
                        );
                        ok = false;
                    }
                }
                let kind = CPatKind::Range {
                    start: start.clone(),
                    end: end.clone(),
                };
                (at(kind), ok)
            }
            ast::Pattern::Tuple { elements, .. } => {
                let fresh: Vec<Ty> = (0..elements.len())
                    .map(|_| self.c.engine.fresh_var())
                    .collect();
                let tup = self.c.engine.mk_tuple(&fresh);
                let mut ok = self.c.engine.unify_at(expected, tup, span);
                let mut elems = Vec::with_capacity(elements.len());
                for (elem, &ety) in elements.iter().zip(fresh.iter()) {
                    let (e, e_ok) = self.type_pattern(elem, ety, b, binders);
                    ok &= e_ok;
                    elems.push(e);
                }
                (at(CPatKind::Tuple(elems)), ok)
            }
            ast::Pattern::Array { elements, .. } => {
                let elem_var = self.c.engine.fresh_var();
                let arr_ty = self.c.ty_array(elem_var);
                let mut ok = self.c.engine.unify_at(expected, arr_ty, span);

                let mut prefix = Vec::with_capacity(elements.len());
                let mut rest = CRest::None;
                let mut seen_spread = false;
                for (i, e) in elements.iter().enumerate() {
                    match e {
                        ast::ArrayPatternElement::Pattern(p) => {
                            let (el, e_ok) = self.type_pattern(p, elem_var, b, binders);
                            ok &= e_ok;
                            prefix.push(el);
                        }
                        ast::ArrayPatternElement::Spread { binding, span: ssp } => {
                            if seen_spread {
                                self.c.error(
                                    "An array pattern can only have one spread".to_string(),
                                    *ssp,
                                );
                                ok = false;
                            } else if i != elements.len() - 1 {
                                self.c.error(
                                    "The spread in an array pattern must come last".to_string(),
                                    *ssp,
                                );
                                ok = false;
                            }
                            seen_spread = true;
                            rest = match binding {
                                None => CRest::Discard,
                                Some(id) => {
                                    let rest_ty = self.c.ty_array(elem_var);
                                    let (r, r_ok) = self.bind_rest(id, rest_ty, b, binders);
                                    ok &= r_ok;
                                    r
                                }
                            };
                        }
                    }
                }
                let kind = CPatKind::Array {
                    elem: elem_var,
                    prefix,
                    rest,
                };
                (at(kind), ok)
            }
            ast::Pattern::Constructor {
                qualifier,
                name,
                args,
                rest,
                ..
            } => self.type_ctor_pattern(
                CtorPattern {
                    qualifier: qualifier.as_ref(),
                    name,
                    args,
                    rest: *rest,
                    span,
                },
                expected,
                b,
                binders,
            ),
            ast::Pattern::Or { first, rest, .. } => {
                // The first alternative sets the canonical set of bindings.
                // `finish` folds the names into the enclosing frame, so a later
                // binding next to it still sees them when checking for
                // duplicates.
                let mut or = b.enter_or();
                let (head, mut ok) = self.type_pattern(first, expected, &mut or.sink(), binders);
                let mut alts = Vec::with_capacity(1 + rest.len());
                alts.push(head);
                for alt in rest {
                    let mut a = or.enter_alternative();
                    let (p, a_ok) = self.type_pattern(alt, expected, &mut a.sink(), binders);
                    ok &= a_ok;
                    alts.push(p);
                    let (scope, complete) = a.finish(alt.span(), &mut self.c.engine);
                    ok &= complete;
                    or = scope;
                }
                or.finish();
                (at(CPatKind::Or(alts)), ok)
            }
        }
    }

    /// Bind a name a pattern reaches. It is `None` when it binds nothing you
    /// can read, which is a `_name` or one rejected with a diagnostic
    /// (`false`).
    fn bind_pattern_name(
        &mut self,
        id: &ast::Identifier,
        ty: Ty,
        b: &mut PatternSink<'_>,
        binders: &mut Vec<BinderId>,
    ) -> (Option<(StrId, BinderId)>, bool) {
        match b.bind(&id.name, ty, id.span, &mut self.c.engine) {
            Bound::Fresh(index) => {
                let sid = self.c.engine.intern(&id.name);
                let binder = self.bind_mono(&id.name, ty, id.span);
                debug_assert_eq!(index, binders.len(), "pattern binders out of step");
                binders.push(binder);
                (Some((sid, binder)), true)
            }
            Bound::Again(index) => {
                let sid = self.c.engine.intern(&id.name);
                (binders.get(index).map(|&binder| (sid, binder)), true)
            }
            // Bind it like any name, so hover and go-to-definition see it and
            // a read of it says it is write-only. Nothing may read it, so the
            // pattern keeps no binder for it, and its discarded type is checked again
            // once inference is final.
            Bound::WriteOnly => {
                self.bind_mono(&id.name, ty, id.span);
                self.nil_discards.push((id.name.clone(), ty, id.span));
                (None, true)
            }
            Bound::Rejected => (None, false),
        }
    }

    fn bind_rest(
        &mut self,
        id: &ast::Identifier,
        ty: Ty,
        b: &mut PatternSink<'_>,
        binders: &mut Vec<BinderId>,
    ) -> (CRest, bool) {
        let (bound, ok) = self.bind_pattern_name(id, ty, b, binders);
        let rest = match bound {
            Some((name, binder)) => CRest::Bind { name, binder },
            None => CRest::Discard,
        };
        (rest, ok)
    }

    /// Resolve a constructor name in scope to a [`CtorLookup`].
    fn lookup_ctor(&self, name: &str) -> Option<CtorLookup> {
        CtorLookup::from_scheme(self.scheme_of(self.c.engine.lookup_str(name)?)?)
    }

    /// Resolve `io::NotFound` through the module qualifier. Every path that
    /// gives `None` has already reported its own diagnostic, so the caller must
    /// only stop, and never report it again.
    fn lookup_ctor_qualified(&mut self, qual: &str, name: &str, span: Span) -> Option<CtorLookup> {
        // A silent `None` would leave the module with no errors, mint a
        // `CleanModule`, and abort the elaborator on a program that
        // `scarlet check` accepted.
        let Some(key) = self.imported_qualifiers.get(qual).cloned() else {
            self.c.error(
                format!("Unknown module qualifier '{qual}'. Import the module first"),
                span,
            );
            return None;
        };
        let Some(iface) = self.c.module_table.get(&key) else {
            let module = self.c.module_name(&key);
            self.c
                .error(format!("Module '{module}' is not loaded"), span);
            return None;
        };
        let Some(ev) = iface.values.get(name) else {
            let private = iface.private_names.contains(name);
            let module = self.c.module_name(&key);
            let msg = if private {
                format!("Constructor '{name}' is private in module '{module}'")
            } else {
                format!("Module '{module}' has no constructor '{name}'")
            };
            self.c.error(msg, span);
            return None;
        };
        match CtorLookup::from_scheme(ev.scheme) {
            Some(ctor) => Some(ctor),
            None => {
                let module = self.c.module_name(&key);
                self.c.error(
                    format!(
                        "'{module}::{name}' is not a constructor and cannot be used in a pattern"
                    ),
                    span,
                );
                None
            }
        }
    }

    fn type_ctor_pattern(
        &mut self,
        pat: CtorPattern<'_>,
        expected: Ty,
        b: &mut PatternSink<'_>,
        binders: &mut Vec<BinderId>,
    ) -> (CPat, bool) {
        let CtorPattern {
            qualifier,
            name,
            args,
            rest,
            span,
        } = pat;
        let error = || CPat {
            ty: expected,
            span,
            kind: CPatKind::Error,
        };
        let ctor = match qualifier {
            // `lookup_ctor_qualified` reports its own diagnostic.
            Some(q) => match self.lookup_ctor_qualified(&q.name, &name.name, name.span) {
                Some(f) => f,
                None => return (error(), false),
            },
            None => match self.lookup_ctor(&name.name) {
                Some(f) => f,
                None => {
                    let qualified = self.qualified_spellings(&name.name);
                    let is_value = self
                        .c
                        .engine
                        .lookup_str(&name.name)
                        .and_then(|id| self.scheme_of(id))
                        .is_some();
                    let msg = if is_value {
                        format!(
                            "'{}' is not a constructor and cannot be used in a pattern",
                            name.name
                        )
                    } else if self.c.env.lookup_type_info(&name.name).is_some() {
                        format!(
                            "'{0}' is a type, not a constructor. To check a value's type and \
                             drop it, write `_ {0} = value`",
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
                    self.c.error(msg, name.span);
                    return (error(), false);
                }
            },
        };
        let CtorLookup {
            variant,
            type_name,
            arity,
            field_labels,
            scheme,
        } = ctor;

        let inst = self
            .c
            .engine
            .instantiate(&scheme, &self.frames.current.rigid);
        if self.c.collect_hover_facts {
            let qualified = format!("{}.{}", self.c.engine.str(type_name), name.name);
            self.record(&qualified, inst, name.span);
        }
        // A qualified pattern records the member use AND a use of the module
        // alias, so the unused-import check and rename see modules that are
        // only referred to from patterns. This must match the expression path.
        match qualifier {
            Some(q) => {
                self.record_value_use(scheme.def, name.span, ReferenceKind::Qualified);
                self.record_qualifier_use(q);
            }
            None => self.record_value_use(scheme.def, name.span, ReferenceKind::Unqualified),
        }

        let r = self.c.engine.find(inst);
        match self.c.engine.node(r) {
            TypeNode::Fun { params, ret } => {
                let params: Vec<Ty> = self.c.engine.children_of(params).to_vec();
                // Unify the constructor's return type with the subject's
                // expected type FIRST, so the parameter types share
                // type-variable cells with the subject and recursing into the
                // arguments refines it.
                let mut ok = self.c.engine.unify_at(expected, ret, span);

                let (by_pos, args_ok) = self.slot_ctor_args(
                    &name.name,
                    arity,
                    field_labels,
                    args.iter().map(|a| match a {
                        ast::PatternArg::Positional(p) => (None, p, p.span()),
                        ast::PatternArg::Labeled { label, pattern, .. } => {
                            (Some(label), pattern, label.span)
                        }
                    }),
                    if rest {
                        None
                    } else {
                        Some((span, ". Use '..' to ignore them"))
                    },
                );
                ok &= args_ok;
                // One sub-pattern per declared field. A field the source left
                // out matches anything.
                let mut fields = Vec::with_capacity(arity);
                for (i, sub) in by_pos.iter().enumerate() {
                    let field_ty = params
                        .get(i)
                        .cloned()
                        .unwrap_or_else(|| self.c.engine.fresh_var());
                    let field = match sub {
                        Some(p) => {
                            let (f, f_ok) = self.type_pattern(p, field_ty, b, binders);
                            ok &= f_ok;
                            f
                        }
                        None => CPat {
                            ty: field_ty,
                            span,
                            kind: CPatKind::Wild,
                        },
                    };
                    fields.push(field);
                }
                let kind = CPatKind::Ctor { variant, fields };
                (
                    CPat {
                        ty: expected,
                        span,
                        kind,
                    },
                    ok,
                )
            }
            _ => {
                let mut ok = self.c.engine.unify_at(expected, r, span);
                if !args.is_empty() {
                    self.c.error(
                        format!(
                            "Constructor '{}' takes no arguments but {} were given",
                            name.name,
                            args.len()
                        ),
                        span,
                    );
                    ok = false;
                }
                let kind = CPatKind::Ctor {
                    variant,
                    fields: Vec::new(),
                };
                (
                    CPat {
                        ty: expected,
                        span,
                        kind,
                    },
                    ok,
                )
            }
        }
    }

    /// Put positional and labelled constructor arguments in the order the
    /// fields are declared, with [`slot_labeled`]. Its errors become
    /// diagnostics, plus one for missing fields when `missing` is
    /// `Some((span, hint))`. Constructor calls and patterns both use this.
    pub(super) fn slot_ctor_args<'a, T>(
        &mut self,
        name: &str,
        arity: usize,
        field_labels: ArenaSlice<pool::StrSlices>,
        args: impl Iterator<Item = (Option<&'a ast::Identifier>, &'a T, Span)>,
        missing: Option<(Span, &str)>,
    ) -> (SmallVec<[Option<&'a T>; 4]>, bool) {
        // Interning first releases the `&mut engine` borrow before we take the
        // `str_ids_of` slice.
        type Item<'a, T> = (Option<StrId>, (&'a T, Span));
        let items: SmallVec<[Item<'a, T>; 4]> = args
            .map(|(l, v, sp)| (l.map(|l| self.c.engine.intern(&l.name)), (v, sp)))
            .collect();
        // This is exactly `arity` long. `slot_labeled` sizes its slots and
        // resolves labels from this one slice, so the two can't disagree.
        let field_ids: SmallVec<[StrId; 4]> =
            SmallVec::from_slice(self.c.engine.str_ids_of(field_labels));
        let fields: SmallVec<[Option<StrId>; 4]> =
            (0..arity).map(|i| field_ids.get(i).copied()).collect();
        let (by_pos, errors) = slot_labeled(&fields, items);
        let mut ok = errors.is_empty();
        for e in errors {
            match e {
                SlotError::ExtraPositional((_, span)) => self.c.error(
                    format!(
                        "Constructor '{}' has {} field(s) but more were supplied",
                        name, arity
                    ),
                    span,
                ),
                SlotError::UnknownLabel(label, (_, span)) => self.c.error(
                    format!(
                        "Constructor '{}' has no field '{}'. Available: {}",
                        name,
                        self.c.engine.str(label),
                        self.c.engine.strs_of(field_labels).join(", ")
                    ),
                    span,
                ),
                SlotError::Duplicate((_, span), field) => {
                    let dup = field_ids
                        .get(field)
                        .map_or("_", |&id| self.c.engine.str(id))
                        .to_string();
                    self.c
                        .error(format!("Field '{}' is specified more than once", dup), span);
                }
            }
        }
        if let Some((span, hint)) = missing
            && by_pos.iter().any(Option::is_none)
        {
            let labels = self.c.engine.strs_of(field_labels);
            let absent: Vec<&str> = (0..arity)
                .filter(|i| by_pos[*i].is_none())
                .map(|i| labels.get(i).map(String::as_str).unwrap_or("_"))
                .collect();
            self.c.error(
                format!(
                    "Constructor '{}' is missing field(s): {}{}",
                    name,
                    absent.join(", "),
                    hint
                ),
                span,
            );
            ok = false;
        }
        (by_pos.into_iter().map(|s| s.map(|(v, _)| v)).collect(), ok)
    }
}
