//! Pattern checking: [`Compiler::type_pattern`] and the constructor lookup
//! and argument-slotting helpers it leans on.

use super::*;

impl Compiler {
    /// Type-check a pattern against `expected` and resolve it: constructors to
    /// their variants, and each name to a new binding, brought into scope as
    /// the pattern reaches it. A segment's size expression is checked before
    /// the segment's own value, so it sees the names earlier segments bind and
    /// nothing later.
    ///
    /// `binders` holds the binder of each name `b` has recorded, by its index
    /// there. The `bool` is false when the pattern is ill-typed: the caller
    /// must propagate it, so exhaustiveness checks, which assume well-typed
    /// patterns, are skipped.
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
                let bin_ty = self.ty_binary();
                let mut ok = self.engine.unify_at(expected, bin_ty, span);
                let mut segs = Vec::with_capacity(segments.len());
                for seg in segments {
                    if let Some(s) = seg.utf8_literal() {
                        segs.push(CBinPatSeg::Literal(s.to_string()));
                        continue;
                    }
                    let (width, val_ty) = match &seg.spec {
                        ast::BinSpec::Int { bits } => (
                            CWidth::Int(self.type_seg_size(bits.as_ref())),
                            self.ty_int(),
                        ),
                        ast::BinSpec::Binary { bytes } => (
                            CWidth::Bytes(self.type_seg_size(bytes.as_ref())),
                            self.ty_binary(),
                        ),
                        ast::BinSpec::Utf8 => (CWidth::Utf8, self.ty_int()),
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
                            let rest_ty = self.ty_binary();
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
                        let v = self.const_number(n);
                        let ty = if matches!(v, crate::core_ir::Const::Float(_)) {
                            self.engine.icon_float()
                        } else {
                            self.ty_int()
                        };
                        (ty, n.span, CPatKind::Number(n.clone()))
                    }
                    ast::PatternLiteral::String(s) => {
                        (self.ty_string(), s.span, CPatKind::String(s.value.clone()))
                    }
                };
                let ok = self.engine.unify_at(expected, lit_ty, sp);
                (at(kind), ok)
            }
            ast::Pattern::Range { start, end, .. } => {
                let int_t = self.ty_int();
                let mut ok = self.engine.unify_at(expected, int_t, span);
                for bound in [start, end] {
                    // `const_number` raises the out-of-range/malformed error.
                    let v = self.const_number(bound);
                    if matches!(v, crate::core_ir::Const::Float(_)) {
                        self.error(
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
                    .map(|_| self.engine.fresh_var())
                    .collect();
                let tup = self.engine.mk_tuple(&fresh);
                let mut ok = self.engine.unify_at(expected, tup, span);
                let mut elems = Vec::with_capacity(elements.len());
                for (elem, &ety) in elements.iter().zip(fresh.iter()) {
                    let (e, e_ok) = self.type_pattern(elem, ety, b, binders);
                    ok &= e_ok;
                    elems.push(e);
                }
                (at(CPatKind::Tuple(elems)), ok)
            }
            ast::Pattern::Array { elements, .. } => {
                let elem_var = self.engine.fresh_var();
                let arr_ty = self.ty_array(elem_var);
                let mut ok = self.engine.unify_at(expected, arr_ty, span);

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
                                self.error(
                                    "Array pattern may contain at most one spread".to_string(),
                                    *ssp,
                                );
                                ok = false;
                            } else if i != elements.len() - 1 {
                                self.error(
                                    "Spread in array pattern must be the last element".to_string(),
                                    *ssp,
                                );
                                ok = false;
                            }
                            seen_spread = true;
                            rest = match binding {
                                None => CRest::Discard,
                                Some(id) => {
                                    let rest_ty = self.ty_array(elem_var);
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
                // The first alternative establishes the canonical binding set;
                // `finish` folds the names into the enclosing frame so a later
                // sibling binding still sees them for duplicate detection.
                let mut or = b.enter_or();
                let (head, mut ok) = self.type_pattern(first, expected, &mut or.sink(), binders);
                let mut alts = Vec::with_capacity(1 + rest.len());
                alts.push(head);
                for alt in rest {
                    let mut a = or.enter_alternative();
                    let (p, a_ok) = self.type_pattern(alt, expected, &mut a.sink(), binders);
                    ok &= a_ok;
                    alts.push(p);
                    let (scope, complete) = a.finish(alt.span(), &mut self.engine);
                    ok &= complete;
                    or = scope;
                }
                or.finish();
                (at(CPatKind::Or(alts)), ok)
            }
        }
    }

    /// Bind a name a pattern reaches. `None` when it binds nothing readable:
    /// a write-only `_name`, or one rejected with a diagnostic (`false`).
    fn bind_pattern_name(
        &mut self,
        id: &ast::Identifier,
        ty: Ty,
        b: &mut PatternSink<'_>,
        binders: &mut Vec<BinderId>,
    ) -> (Option<(StrId, BinderId)>, bool) {
        match b.bind(&id.name, ty, id.span, &mut self.engine) {
            Bound::Fresh(index) => {
                self.get_or_create_local(&id.name);
                self.register_local_binding(&id.name, ty, id.span);
                let sid = self.engine.intern(&id.name);
                let binder = self.publish_local(sid);
                debug_assert_eq!(index, binders.len(), "pattern binders out of step");
                binders.push(binder);
                (Some((sid, binder)), true)
            }
            Bound::Again(index) => {
                let sid = self.engine.intern(&id.name);
                (binders.get(index).map(|&binder| (sid, binder)), true)
            }
            // Registered so hover and go-to-definition see it. Nothing reads
            // it, so it has no slot, and its discarded type is re-checked once
            // inference is final.
            Bound::WriteOnly => {
                self.register_local_binding(&id.name, ty, id.span);
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
        CtorLookup::from_scheme(*self.env.lookup(name)?)
    }

    /// Resolve `io::NotFound` through the module qualifier. Every `None` path
    /// has already reported its own diagnostic, so the caller must only
    /// short-circuit, never report again.
    fn lookup_ctor_qualified(&mut self, qual: &str, name: &str, span: Span) -> Option<CtorLookup> {
        // A silent `None` would leave the module error-free, mint a
        // `CleanModule`, and abort the elaborator on a program `al check` took.
        let Some(key) = self.imported_qualifiers.get(qual).cloned() else {
            self.error(
                format!("Unknown module qualifier '{qual}'. Import the module before use."),
                span,
            );
            return None;
        };
        let Some(iface) = self.module_table.get(&key) else {
            let module = self.module_name(&key);
            self.error(format!("Module '{module}' is not loaded"), span);
            return None;
        };
        let Some(ev) = iface.values.get(name) else {
            let private = iface.private_names.contains(name);
            let module = self.module_name(&key);
            let msg = if private {
                format!("Constructor '{name}' is private in module '{module}'")
            } else {
                format!("Module '{module}' has no constructor '{name}'")
            };
            self.error(msg, span);
            return None;
        };
        match CtorLookup::from_scheme(ev.scheme) {
            Some(ctor) => Some(ctor),
            None => {
                let module = self.module_name(&key);
                self.error(
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
                    let msg = if self.env.lookup(&name.name).is_some() {
                        format!(
                            "'{}' is not a constructor and cannot be used in a pattern",
                            name.name
                        )
                    } else if self.env.lookup_type_info(&name.name).is_some() {
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
                    self.error(msg, name.span);
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

        let inst = self.engine.instantiate(&scheme, &self.rigid_ids);
        if self.collect_hover_facts {
            let qualified = format!("{}.{}", self.engine.str(type_name), name.name);
            let doc = self.doc_if_collecting(&qualified);
            self.record(&qualified, inst, name.span, doc);
        }
        // A qualified pattern records the member use AND a use of the module
        // alias, so unused-import liveness and rename see modules referenced
        // only from patterns. Must mirror the expression path.
        match qualifier {
            Some(q) => {
                self.record_value_use(scheme.def, name.span, ReferenceKind::Qualified);
                self.record_qualifier_use(q);
            }
            None => self.record_value_use(scheme.def, name.span, ReferenceKind::Unqualified),
        }

        let r = self.engine.find(inst);
        match self.engine.node(r) {
            TypeNode::Fun { params, ret } => {
                let params: Vec<Ty> = self.engine.children_of(params).to_vec();
                // Unify the constructor's return type with the expected subject
                // type FIRST, so the param types share type-variable cells with
                // the subject and recursing into args refines it.
                let mut ok = self.engine.unify_at(expected, ret, span);

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
                // One sub-pattern per declared field; a field the source left
                // out matches anything.
                let mut fields = Vec::with_capacity(arity);
                for (i, sub) in by_pos.iter().enumerate() {
                    let field_ty = params
                        .get(i)
                        .cloned()
                        .unwrap_or_else(|| self.engine.fresh_var());
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
                let mut ok = self.engine.unify_at(expected, r, span);
                if !args.is_empty() {
                    self.error(
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

    /// Slot positional and labelled constructor args into field-declaration
    /// order with [`slot_labeled`], rendering its errors as diagnostics plus a
    /// missing-fields one when `missing` is `Some((span, hint))`. Shared by
    /// ctor calls and patterns.
    pub(super) fn slot_ctor_args<'a, T>(
        &mut self,
        name: &str,
        arity: usize,
        field_labels: ArenaSlice<pool::StrSlices>,
        args: impl Iterator<Item = (Option<&'a ast::Identifier>, &'a T, Span)>,
        missing: Option<(Span, &str)>,
    ) -> (SmallVec<[Option<&'a T>; 4]>, bool) {
        // Interning up front releases the `&mut engine` borrow before the
        // `str_ids_of` slice is taken.
        type Item<'a, T> = (Option<StrId>, (&'a T, Span));
        let items: SmallVec<[Item<'a, T>; 4]> = args
            .map(|(l, v, sp)| (l.map(|l| self.engine.intern(&l.name)), (v, sp)))
            .collect();
        // Exactly `arity` long: `slot_labeled` sizes its slots and resolves
        // labels from this one slice, so the two cannot disagree.
        let field_ids: SmallVec<[StrId; 4]> =
            SmallVec::from_slice(self.engine.str_ids_of(field_labels));
        let fields: SmallVec<[Option<StrId>; 4]> =
            (0..arity).map(|i| field_ids.get(i).copied()).collect();
        let (by_pos, errors) = slot_labeled(&fields, items);
        let mut ok = errors.is_empty();
        for e in errors {
            match e {
                SlotError::ExtraPositional((_, span)) => self.error(
                    format!(
                        "Constructor '{}' has {} field(s) but more were supplied",
                        name, arity
                    ),
                    span,
                ),
                SlotError::UnknownLabel(label, (_, span)) => self.error(
                    format!(
                        "Constructor '{}' has no field '{}'. Available: {}",
                        name,
                        self.engine.str(label),
                        self.engine.strs_of(field_labels).join(", ")
                    ),
                    span,
                ),
                SlotError::Duplicate((_, span), field) => {
                    let dup = field_ids
                        .get(field)
                        .map_or("_", |&id| self.engine.str(id))
                        .to_string();
                    self.error(format!("Field '{}' is specified more than once", dup), span);
                }
            }
        }
        if let Some((span, hint)) = missing
            && by_pos.iter().any(Option::is_none)
        {
            let labels = self.engine.strs_of(field_labels);
            let absent: Vec<&str> = (0..arity)
                .filter(|i| by_pos[*i].is_none())
                .map(|i| labels.get(i).map(String::as_str).unwrap_or("_"))
                .collect();
            self.error(
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
