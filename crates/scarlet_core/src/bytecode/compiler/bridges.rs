//! `Compiler`'s impls of the traits `core_ir` speaks to the enclosing
//! compilation through ([`crate::core_ir::emit::EmitCtx`], [`ElabCtx`]), kept
//! here so `core_ir` never sees the compiler's fields.

use super::*;
use crate::typed_ir::PreludeTys;

impl crate::core_ir::emit::EmitCtx for Compiler {
    fn resolve_str(&self, id: StrId) -> &str {
        self.engine.str(id)
    }
    fn intern_int(&mut self, i: i64) -> i32 {
        self.const_int(i)
    }
    fn intern_str(&mut self, s: &str) -> i32 {
        self.const_str(s)
    }
    fn intern_labels(&mut self, tid: TypeId, variant_idx: u16) -> i32 {
        let variant = self.declared_variant(tid, variant_idx);
        let labels: Vec<String> = self
            .engine
            .variant_fields_of(variant.fields)
            .iter()
            .map(|f| self.engine.str(f.label).to_string())
            .collect();
        let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        let v = self.frozen.str_array(&refs).into_value();
        self.add_constant(v)
    }
    fn variant_name(&self, tid: TypeId, variant_idx: u16) -> &str {
        self.engine
            .str(self.declared_variant(tid, variant_idx).name)
    }
    fn switch_variant_count(&self, tid: TypeId) -> Option<u8> {
        Compiler::switch_variant_count(self, tid)
    }
    fn bool_variant(&self, tid: TypeId, variant_idx: u16) -> Option<bool> {
        if !self.prelude.bool().is(tid) {
            return None;
        }
        Some(self.prelude.true_().is(tid, variant_idx))
    }
}

/// `emit` asked for a constructor of a type with no variants. Aborts in
/// release too: any name or labels invented here ship the wrong cell.
#[allow(clippy::unreachable)]
#[cold]
#[inline(never)]
fn no_variants(tid: TypeId) -> ! {
    unreachable!(
        "internal compiler error: emit asked for a variant of a type with no variants: {tid:?}. \
         Report this as a compiler bug."
    )
}

impl Compiler {
    fn declared_variant(&self, tid: TypeId, variant_idx: u16) -> crate::types::Variant {
        let Some(vs) = self
            .env
            .lookup_type_info_by_id(tid)
            .and_then(|ti| ti.variants())
        else {
            no_variants(tid);
        };
        self.engine.variants_of(vs)[variant_idx as usize]
    }

    /// The variant count a `SwitchTag` over `tid` dispatches on, or `None`
    /// when the type never switches: the bytecode emitter's rule, and — via
    /// [`crate::core_ir::SwitchCounts`] — the native planner's, so the two
    /// backends ladder and switch the same matches. `Bool` is unboxed, so
    /// its scrutinee has no tag word; past 255 variants the `SwitchTag.a`
    /// byte overflows.
    pub(super) fn switch_variant_count(&self, tid: TypeId) -> Option<u8> {
        let n = self.env.lookup_type_info_by_id(tid)?.variants()?.len;
        if self.prelude.bool().is(tid) || n > 255 {
            None
        } else {
            Some(n as u8)
        }
    }
}

impl PreludeTys for Compiler {
    /// The one bridge from a live inference `Ty` into the program's `RTy`
    /// pool. Total: an unsolved variable becomes a fresh `Bound`.
    ///
    /// The cache is what makes structurally identical types share one pool
    /// node, so consumers keyed off an `RTy` can tell they are one type. Only
    /// fully solved types may be cached: a variable unsolved now can be solved
    /// by a later body. Keyed by union-find root; cleared with the pool.
    fn resolve_rty(&mut self, pool: &mut ResolvedPool, t: Ty) -> RTy {
        let root = self.engine.find(t);
        if let Some(&r) = self.rty_cache.get(&root) {
            return r;
        }
        let (r, invented) = Zonker::new(&self.engine).zonk_or_opaque(pool, root);
        if !invented {
            self.rty_cache.insert(root, r);
        }
        r
    }
    fn ty_bool(&mut self) -> Ty {
        Compiler::ty_bool(self)
    }
    fn ty_int(&mut self) -> Ty {
        Compiler::ty_int(self)
    }
    fn ty_string(&mut self) -> Ty {
        Compiler::ty_string(self)
    }
    fn ty_binary(&mut self) -> Ty {
        Compiler::ty_binary(self)
    }
}

impl ElabCtx for Compiler {
    fn intern(&mut self, s: &str) -> StrId {
        self.engine.intern(s)
    }
    fn str(&self, id: StrId) -> &str {
        self.engine.str(id)
    }
    // Safe mid-elaboration: `program.constants` is `ConstId`-addressed, so
    // pooling moves no address.
    fn add_const(&mut self, v: Value) -> crate::core_ir::ConstId {
        crate::core_ir::ConstId(self.add_constant(v) as u32)
    }
    fn number_const(&mut self, lit: &ast::NumberLiteral) -> crate::core_ir::ConstId {
        let v = self.const_number(lit);
        crate::core_ir::ConstId(self.add_constant(v) as u32)
    }
    fn string_const(&mut self, s: &str) -> crate::core_ir::ConstId {
        crate::core_ir::ConstId(self.const_str(s) as u32)
    }
    fn int_const(&mut self, i: i64) -> crate::core_ir::ConstId {
        crate::core_ir::ConstId(self.const_int(i) as u32)
    }
    fn binary_const(&mut self, bytes: Vec<u8>, bit_len: u64) -> crate::core_ir::ConstId {
        crate::core_ir::ConstId(self.const_binary(bytes, bit_len) as u32)
    }
    fn resolve_name(&mut self, name: &str) -> Option<(Ty, Denotation)> {
        let scheme = self.env.lookup(name)?;
        let kind = scheme.kind;
        let ty = self.engine.instantiate(scheme, &self.rigid_ids);
        let id = self.engine.intern(name);
        // Called even for constructors and builtins, for its side effects:
        // marking the name used, and recording a capture.
        let place = self.resolve_variable(id);
        let den = match (Denotation::from_kind(kind), place) {
            (Some(fixed), _) => fixed,
            (None, Some(place)) => place,
            // `analyse_module` unwinds `self.locals` before `__main__`
            // elaborates, so a toplevel decl's own name is only findable on
            // its `ToplevelDecl`. Reached for an intra-SCC forward ref.
            (None, None) if self.outer_scopes.is_empty() => match self.decl_denotation(id) {
                Some(den) => den,
                None => typed_ir::elaborator_bug(
                    "toplevel name resolves to no decl and no binding",
                    Span::DUMMY,
                ),
            },
            (None, None) => Denotation::self_closure(),
        };
        Some((ty, den))
    }
    fn resolve_qualified(
        &mut self,
        qual: &str,
        member: &str,
        span: Span,
    ) -> Option<(Ty, Denotation)> {
        let key = self.imported_qualifiers.get(qual)?.clone();
        let iface = self.module_table.get(&key)?;
        let ev = iface.values.get(member)?;
        let scheme = ev.scheme;
        let ty = self.engine.instantiate(&scheme, &self.rigid_ids);
        let local_slot = ev.local_slot;
        // `@vm` builtins and re-exported constructors carry no `local_slot`;
        // their `ValueKind` alone denotes them. Everything else must have one.
        let den = match Denotation::from_kind(scheme.kind) {
            Some(fixed) => fixed,
            None => match local_slot {
                Some(slot) => self.global_denotation(slot),
                None => {
                    typed_ir::elaborator_bug("qualified module member has no runtime binding", span)
                }
            },
        };
        Some((ty, den))
    }
    fn ctor_field(&mut self, receiver: Ty, field: &str) -> Option<(u32, Ty)> {
        let resolved = self.engine.find(receiver);
        let (type_id, type_args) = match self.engine.node(resolved) {
            TypeNode::Con { id, args, .. } => (id, self.engine.children_of(args).to_vec()),
            _ => return None,
        };
        let info = self.env.lookup_type_info_by_id(type_id)?;
        let field_id = self.engine.intern(field);
        let (idx, fty) = self
            .field_in_variants(info, &type_args, field_id, None)
            .ok()?;
        Some((idx as u32, fty))
    }
    /// Labels come off the type by `VariantRef`, not off the constructor's
    /// scheme by name: a `mod.Ctor(..)` bare name is not in `env`.
    fn ctor_labels(&mut self, v: crate::core_ir::VariantRef) -> Option<Vec<StrId>> {
        let info = self.env.lookup_type_info_by_id(v.type_id)?;
        let variants = info.variants()?;
        let variant = *self
            .engine
            .variants_of(variants)
            .get(v.variant_idx as usize)?;
        Some(
            self.engine
                .variant_fields_of(variant.fields)
                .iter()
                .map(|f| f.label)
                .collect(),
        )
    }
    /// Labels come off the callee's own scheme, found by the same lookup
    /// [`ElabCtx::resolve_name`] / [`ElabCtx::resolve_qualified`] used for its
    /// denotation, so the two cannot answer for different declarations.
    fn fn_param_labels(&mut self, qual: Option<&str>, name: &str) -> Option<Vec<StrId>> {
        let kind = match qual {
            None => self.env.lookup(name)?.kind,
            // Same lookup `resolve_qualified` uses for the callee's own
            // denotation: qualifier -> module key -> exported scheme. Reading
            // the labels off that scheme, not a second table, is what keeps
            // this answer for the same declaration `resolve_qualified` named.
            Some(q) => {
                let key = self.imported_qualifiers.get(q)?.clone();
                let iface = self.module_table.get(&key)?;
                iface.values.get(name)?.scheme.kind
            }
        };
        let ValueKind::ModuleFn { param_labels } = kind else {
            return None;
        };
        // Empty is "not recorded", never "takes no parameters", so it refuses
        // the label rather than slotting it against nothing.
        if param_labels.len == 0 {
            return None;
        }
        Some(self.engine.str_ids_of(param_labels).to_vec())
    }
    fn closure(&mut self, span: Span) -> Option<(crate::core_ir::FuncIdx, Vec<StrId>)> {
        // A scan over the frame's own lambdas, a handful at most. A table
        // keyed by span would collide across modules.
        self.frame_closures
            .iter()
            .find(|s| s.at == span)
            .map(|s| (s.func_idx, s.captures.clone()))
    }
    fn fn_of_global(&self, slot: GlobalSlot) -> Option<crate::core_ir::FuncIdx> {
        self.global_to_func.get(&slot).copied()
    }
    fn next_global_slot(&mut self) -> Option<GlobalSlot> {
        self.take_global_slot()
    }
    fn toplevel_decls(&self) -> Vec<(usize, GlobalSlot)> {
        // A fn body's outermost block takes the same code path but is not a
        // module toplevel: its node indices address its own body. Only a
        // module toplevel runs with no enclosing frame.
        if !self.outer_scopes.is_empty() {
            return Vec::new();
        }
        self.toplevel_decls
            .iter()
            .map(|d| (d.node, d.slot))
            .collect()
    }
    fn or_shape(&mut self, lhs_ty: Ty) -> Option<OrShape> {
        use crate::core_ir::VariantRef;
        let resolved = self.engine.find(lhs_ty);
        let TypeNode::Con { id, .. } = self.engine.node(resolved) else {
            return None;
        };
        let (tref, ok, fail, err_has_payload) = if self.prelude.option().is(id) {
            (
                self.prelude.option(),
                self.prelude.some(),
                self.prelude.none(),
                false,
            )
        } else if self.prelude.result().is(id) {
            (
                self.prelude.result(),
                self.prelude.ok(),
                self.prelude.err(),
                true,
            )
        } else {
            return None;
        };
        let tn = self.engine.intern(tref.name);
        Some(OrShape {
            fail: VariantRef {
                type_id: fail.type_id,
                variant_idx: fail.variant_idx,
                type_name: tn,
            },
            ok: VariantRef {
                type_id: ok.type_id,
                variant_idx: ok.variant_idx,
                type_name: tn,
            },
            err_has_payload,
        })
    }
    fn ty_nil(&mut self) -> Ty {
        Compiler::ty_nil(self)
    }
}
