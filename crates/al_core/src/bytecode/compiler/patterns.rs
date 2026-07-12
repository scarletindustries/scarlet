//! Pattern type-checking (no codegen): [`Compiler::type_pattern`] and the
//! constructor lookup/argument-slotting helpers it leans on. Split out of
//! `mod.rs` along its section banner; every method here is part of the same
//! `impl Compiler` pass and shares its state.

use super::*;

impl Compiler {
    /// Type-check a pattern against `expected`, recording bound names in `b`.
    /// Emits no bytecode. Returns `false` if any unification or binding step
    /// failed; the caller propagates this so that exhaustiveness/usefulness
    /// checks (which assume well-typed patterns) can be skipped.
    #[must_use = "a false result means the pattern is ill-typed and must be propagated"]
    pub(super) fn type_pattern(
        &mut self,
        pat: &ast::Pattern,
        expected: Ty,
        b: &mut PatternSink<'_>,
    ) -> bool {
        match pat {
            ast::Pattern::Binary {
                segments,
                rest,
                span,
            } => {
                let bin_ty = self.ty_binary();
                let mut ok = self.engine.unify_at(expected, bin_ty, *span);
                for seg in segments {
                    // A string-literal Utf8 segment (`<<'GET ', ..>>`) matches
                    // its encoded bytes as a prefix; it binds nothing and has
                    // no value type to check. Other Int / Utf8 segments bind an
                    // integer (a value or a codepoint); Binary segments bind a
                    // sub-binary. Size expressions are operands, not bindings,
                    // and are checked by `type_pattern_sizes` once every name
                    // this pattern binds is in scope.
                    if seg.utf8_literal().is_some() {
                        continue;
                    }
                    let seg_val_ty = match seg.spec {
                        ast::BinSpec::Int { .. } | ast::BinSpec::Utf8 => self.ty_int(),
                        ast::BinSpec::Binary { .. } => self.ty_binary(),
                    };
                    ok &= self.type_pattern(&seg.value, seg_val_ty, b);
                }
                if let Some(r) = rest
                    && let Some(id) = &r.binding
                {
                    let rest_ty = self.ty_binary();
                    ok &= b.bind(&id.name, rest_ty, id.span, &mut self.engine);
                }
                ok
            }
            ast::Pattern::Wildcard { .. } => true,
            ast::Pattern::Var { name } => b.bind(&name.name, expected, name.span, &mut self.engine),
            ast::Pattern::Literal(lit) => {
                let (lit_ty, sp) = match lit {
                    // `const_number` is what raises the out-of-range / malformed
                    // diagnostic; the pooled `Value` itself is Core's business.
                    ast::PatternLiteral::Number(n) => {
                        let v = self.const_number(n);
                        let ty = if v.is_float() {
                            self.engine.icon_float()
                        } else {
                            self.ty_int()
                        };
                        (ty, n.span)
                    }
                    ast::PatternLiteral::String(s) => (self.ty_string(), s.span),
                };
                self.engine.unify_at(expected, lit_ty, sp)
            }
            ast::Pattern::Range { start, end, span } => {
                let int_t = self.ty_int();
                let mut ok = self.engine.unify_at(expected, int_t, *span);
                for bound in [start, end] {
                    // `const_number` raises the out-of-range / malformed
                    // diagnostic itself; float classification comes from the
                    // parsed value, same as the `Literal` arm above.
                    let v = self.const_number(bound);
                    if v.is_float() {
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
                ok
            }
            ast::Pattern::Tuple { elements, span } => {
                let fresh: Vec<Ty> = (0..elements.len())
                    .map(|_| self.engine.fresh_var())
                    .collect();
                let tup = self.engine.mk_tuple(&fresh);
                let mut ok = self.engine.unify_at(expected, tup, *span);
                for (elem, &ety) in elements.iter().zip(fresh.iter()) {
                    ok &= self.type_pattern(elem, ety, b);
                }
                ok
            }
            ast::Pattern::Array { elements, span } => {
                let elem_var = self.engine.fresh_var();
                let arr_ty = self.ty_array(elem_var);
                let mut ok = self.engine.unify_at(expected, arr_ty, *span);

                let mut seen_spread = false;
                for (i, e) in elements.iter().enumerate() {
                    match e {
                        ast::ArrayPatternElement::Pattern(p) => {
                            ok &= self.type_pattern(p, elem_var, b);
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
                            if let Some(id) = binding {
                                let rest_ty = self.ty_array(elem_var);
                                ok &= b.bind(&id.name, rest_ty, id.span, &mut self.engine);
                            }
                        }
                    }
                }
                ok
            }
            ast::Pattern::Constructor {
                qualifier,
                name,
                args,
                rest,
                span,
            } => self.type_ctor_pattern(
                CtorHead {
                    qualifier: qualifier.as_ref(),
                    name,
                },
                args,
                *rest,
                *span,
                expected,
                b,
            ),
            ast::Pattern::Or { first, rest, .. } => {
                // Scope the canonical binding set to this or-pattern: `enter_or`
                // pushes a frame whose first alternative establishes the
                // canonical set; `finish` pops it and folds the bound names
                // into the enclosing frame so a sibling binding after the
                // or-pattern still sees them for duplicate detection. Each
                // non-first alternative takes ownership of the scope and only
                // its completeness check hands the scope back.
                let mut or = b.enter_or();
                let mut ok = self.type_pattern(first, expected, &mut or.sink());
                for alt in rest {
                    let mut a = or.enter_alternative();
                    ok &= self.type_pattern(alt, expected, &mut a.sink());
                    let (scope, complete) = a.finish(alt.span(), &mut self.engine);
                    ok &= complete;
                    or = scope;
                }
                or.finish();
                ok
            }
        }
    }

    /// Resolve a constructor name in scope to a [`CtorLookup`].
    fn lookup_ctor(&self, name: &str) -> Option<CtorLookup> {
        CtorLookup::from_scheme(*self.env.lookup(name)?)
    }

    /// `io.NotFound` — the same constructor `import al/io.{NotFound}` would
    /// bring into scope, reached through the module qualifier instead. Returns
    /// `None` when the qualifier is unknown, the member is missing, or it is
    /// not a constructor; the caller renders the diagnostic.
    ///
    /// A private (or `opaque`-hidden) constructor is reported here, so
    /// `match e { id.Id(n) -> n }` gives the same error `id.Id(1)` already
    /// gives as an expression.
    fn lookup_ctor_qualified(&mut self, qual: &str, name: &str, span: Span) -> Option<CtorLookup> {
        // Every failure below must report. A silent `None` leaves the module
        // error-free, so `CleanModule` is minted and the elaborator — which has
        // no diagnostics to fall back on — aborts on a program `al check`
        // accepted.
        let Some(key) = self.imported_qualifiers.get(qual).cloned() else {
            self.error(
                format!("Unknown module qualifier '{qual}' — did you `import` it?"),
                span,
            );
            return None;
        };
        let Some(iface) = self.module_table.get_or_hydrate(&key) else {
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
                        "'{module}.{name}' is not a constructor and cannot be used in a pattern"
                    ),
                    span,
                );
                None
            }
        }
    }

    fn type_ctor_pattern(
        &mut self,
        head: CtorHead<'_>,
        args: &[ast::PatternArg],
        rest: bool,
        span: Span,
        expected: Ty,
        b: &mut PatternSink<'_>,
    ) -> bool {
        let CtorHead { qualifier, name } = head;
        let ctor = match qualifier {
            // `lookup_ctor_qualified` reports its own diagnostic: it knows
            // whether the member is missing, private, or not a constructor.
            Some(q) => match self.lookup_ctor_qualified(&q.name, &name.name, name.span) {
                Some(f) => f,
                None => return false,
            },
            None => match self.lookup_ctor(&name.name) {
                Some(f) => f,
                None => {
                    let msg = if self.env.lookup(&name.name).is_some() {
                        format!(
                            "'{}' is not a constructor and cannot be used in a pattern",
                            name.name
                        )
                    } else {
                        format!("Unknown constructor '{}' in pattern", name.name)
                    };
                    self.error(msg, name.span);
                    return false;
                }
            },
        };
        let CtorLookup {
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
        // Mirror the expression path: a qualified pattern (`io.NotFound(x)`)
        // records a `Qualified` member occurrence plus a `Qualifier` occurrence
        // on the module alias, so unused-import liveness and rename see modules
        // referenced only from patterns.
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
                // Bidirectional pivot: unify the constructor's return type with
                // the expected subject type FIRST so that the param types share
                // type-variable cells with the subject and recursing into args
                // refines the subject's type.
                let mut ok = self.engine.unify_at(expected, ret, span);

                let (by_pos, args_ok) = self.slot_ctor_args(
                    &name.name,
                    arity,
                    field_labels,
                    args.iter().map(|a| match a {
                        ast::PatternArg::Positional(p) => (None, p, p.span()),
                        ast::PatternArg::Labeled { label, pattern } => {
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
                for (i, sub) in by_pos.iter().enumerate() {
                    if let Some(p) = sub {
                        let field_ty = params
                            .get(i)
                            .cloned()
                            .unwrap_or_else(|| self.engine.fresh_var());
                        ok &= self.type_pattern(p, field_ty, b);
                    }
                }
                ok
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
                ok
            }
        }
    }

    /// Slot positional + labelled constructor args into field-declaration
    /// order with [`slot_labeled`], rendering its too-many-positional /
    /// unknown-label / duplicate-field errors as diagnostics, plus a
    /// missing-fields diagnostic when `missing` is `Some((span, hint))`.
    /// Shared by ctor calls and patterns.
    pub(super) fn slot_ctor_args<'a, T>(
        &mut self,
        name: &str,
        arity: usize,
        field_labels: ArenaSlice<pool::StrSlices>,
        args: impl Iterator<Item = (Option<&'a ast::Identifier>, &'a T, Span)>,
        missing: Option<(Span, &str)>,
    ) -> (SmallVec<[Option<&'a T>; 4]>, bool) {
        // Interning up front releases the `&mut engine` borrow before the
        // `str_ids_of` slice is taken. Each item carries its own span, so the
        // errors `slot_labeled` hands back point at the offending argument
        // without re-indexing this sequence.
        type Item<'a, T> = (Option<StrId>, (&'a T, Span));
        let items: SmallVec<[Item<'a, T>; 4]> = args
            .map(|(l, v, sp)| (l.map(|l| self.engine.intern(&l.name)), (v, sp)))
            .collect();
        // One declared-fields description, exactly `arity` long: `slot_labeled`
        // sizes its slots and resolves labels from this same slice, so the
        // label table cannot disagree with the arity inside it.
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
