//! Core IR ↔ Compiler bridges.
//!
//! The Core IR passes (`lower`, `emit`) are compiler-agnostic and speak to
//! the enclosing compilation through the [`crate::core_ir::emit::EmitCtx`]
//! and [`ElabCtx`] traits; `Compiler` provides the concrete impls here
//! rather than exposing its fields to `core_ir`.

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
        let Some(vs) = self
            .env
            .lookup_type_info_by_id(tid)
            .and_then(|ti| ti.variants())
        else {
            labels_of_variantless_type(tid);
        };
        let variant = self.engine.variants_of(vs)[variant_idx as usize];
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
    fn switch_variant_count(&self, tid: TypeId) -> Option<u8> {
        let n = self.env.lookup_type_info_by_id(tid)?.variants()?.len;
        // `Bool` is unboxed at the VM level so its scrutinee has no tag word;
        // >255 variants overflow the `SwitchTag.a` byte.
        if self.prelude.bool.is(tid) || n > 255 {
            None
        } else {
            Some(n as u8)
        }
    }
    fn bool_variant(&self, tid: TypeId, variant_idx: u16) -> Option<bool> {
        if !self.prelude.bool.is(tid) {
            return None;
        }
        Some(self.prelude.true_.is(tid, variant_idx))
    }
}

/// `emit` asked for the variant labels of a type that has no variants — a
/// broken emit invariant. Aborts, in release as well as debug: any constant
/// returned instead (the old fallback was pool index 0) silently ships the
/// wrong labels. Mirrors `emit`'s `unbound_local`.
#[allow(clippy::unreachable)]
#[cold]
#[inline(never)]
fn labels_of_variantless_type(tid: TypeId) -> ! {
    unreachable!(
        "internal compiler error: emit asked for labels of a type with no variants: {tid:?}. \
         Please report this as a compiler bug."
    )
}

impl PreludeTys for Compiler {
    /// The one bridge from a live inference `Ty` into the program's `RTy` pool.
    /// Total: `zonk_or_opaque` encodes a variable inference never solved as a
    /// fresh `Bound` rather than failing, because "undetermined" and "rigidly
    /// polymorphic" are the same operational fact downstream.
    ///
    /// A `Zonker` memoises only within one call, and the elaborator asks for a
    /// type per *node*: without a cache across calls the five `Int`s of `1 + 2
    /// * 3` become five structurally identical pool nodes, and every consumer
    /// that keys off an `RTy` (a `Drop` shape, a golden snapshot's `:tN`) stops
    /// seeing that they are one type. Only a *fully solved* type is cached —
    /// `zonk_or_opaque` reports when it invented an opaque `Bound`, and a
    /// variable that is unsolved now may be solved by a later body, so such a
    /// node must not be remembered. `rty_cache` is keyed by union-find root
    /// and belongs to the pool `elaborate` opened, which clears it.
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
    // `program.constants` is `ConstId`-addressed, so pooling during an
    // elaboration moves no address, and the elaborator writes no code at all.
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
        // Always consulted, even for a constructor or a builtin: it is what
        // marks the name used for the unused-binding diagnostic, and what
        // records a capture when a nested frame reads an enclosing local.
        // `Denotation::from_kind` then discards the place for the two kinds
        // that have no runtime binding.
        let place = self.resolve_variable(id);
        let den = match (Denotation::from_kind(kind, id), place) {
            (Some(fixed), _) => fixed,
            (None, Some(place)) => place,
            // `analyse_module` unwinds `self.locals` before the `__main__`
            // elaboration runs, so a decl's own name is only findable on its
            // `ToplevelDecl`. Reached for an intra-SCC forward ref (a dependee
            // is already bound in the elaborator's own scope and never gets
            // here).
            (None, None) if self.outer_scopes.is_empty() => self
                .decl_denotation(id)
                .unwrap_or_else(Denotation::self_closure),
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
        let sid = self.engine.intern(member);
        // `@vm` builtins and re-exported constructors have no `local_slot`;
        // their `ValueKind` alone denotes them. Anything else the exporter
        // must have given a slot — a member without one is a broken exporter
        // invariant, and answering with a placeholder here would emit a load
        // of the wrong value.
        let den = match Denotation::from_kind(scheme.kind, sid) {
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
    /// Read the labels off the *type*, by `VariantRef`, rather than off the
    /// constructor's scheme by name. `mod.Ctor(..)` resolves through
    /// `resolve_qualified` and its bare name is not in `env`, so a name lookup
    /// answered `None` for a call the check walk had accepted.
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
    fn closure(&mut self, span: Span) -> Option<(crate::core_ir::FuncIdx, Vec<StrId>)> {
        // The frame's own lambdas, a handful at most: a linear scan over the
        // list the frame owns, not a probe into a table keyed by a span that
        // any module could have minted.
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
        // A fn body's outermost block is elaborated by the same code path as a
        // module toplevel, and it is not one: its node indices address its own
        // body, not the module's. Only a module toplevel runs with no
        // enclosing frame — the same guard `take_global_slot` uses.
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
        let (tref, ok, fail, err_has_payload) = if self.prelude.option.is(id) {
            (
                self.prelude.option,
                self.prelude.some,
                self.prelude.none,
                false,
            )
        } else if self.prelude.result.is(id) {
            (self.prelude.result, self.prelude.ok, self.prelude.err, true)
        } else {
            return None;
        };
        let tn = self.engine.intern(tref.name);
        use crate::bytecode::prelude_bindings::names as pn;
        let (okn, failn) = if err_has_payload {
            (pn::OK, pn::ERR)
        } else {
            (pn::SOME, pn::NONE)
        };
        Some(OrShape {
            fail: VariantRef {
                type_id: fail.type_id,
                variant_idx: fail.variant_idx,
                type_name: tn,
                variant_name: self.engine.intern(failn),
            },
            ok: VariantRef {
                type_id: ok.type_id,
                variant_idx: ok.variant_idx,
                type_name: tn,
                variant_name: self.engine.intern(okn),
            },
            err_has_payload,
        })
    }
    fn ty_nil(&mut self) -> Ty {
        Compiler::ty_nil(self)
    }
}
