//! `Compiler`'s impls of the traits the elaborator speaks to the enclosing
//! compilation through ([`PreludeTys`], [`ElabCtx`]), kept here so the
//! elaborator never sees the compiler's fields.

use super::*;
use crate::typed_ir::{FieldAt, PreludeTys, VariantLayout};

impl PreludeTys for ModuleChecker<'_> {
    /// The only way a live inference `Ty` gets into the program's `RTy` pool.
    /// It never fails: an unsolved variable becomes a fresh `Bound`.
    ///
    /// The cache makes structurally identical types share one pool node, so
    /// code keyed on an `RTy` can tell they are the same type. Only fully
    /// solved types go in the cache, because a variable that is unsolved now
    /// can be solved by a later body. The cache is keyed by union-find root
    /// and cleared along with the pool.
    fn resolve_rty(&mut self, pool: &mut ResolvedPool, t: Ty) -> RTy {
        let root = self.c.engine.find(t);
        if let Some(&r) = self.c.rty_cache.get(&root) {
            return r;
        }
        let (r, invented) = Zonker::new(&self.c.engine).zonk_or_opaque(pool, root);
        if !invented {
            self.c.rty_cache.insert(root, r);
        }
        r
    }
    fn ty_bool(&mut self) -> Ty {
        self.c.ty_bool()
    }
    fn ty_int(&mut self) -> Ty {
        self.c.ty_int()
    }
    fn ty_string(&mut self) -> Ty {
        self.c.ty_string()
    }
    fn ty_binary(&mut self) -> Ty {
        self.c.ty_binary()
    }
}

impl ElabCtx for ModuleChecker<'_> {
    fn intern(&mut self, s: &str) -> StrId {
        self.c.engine.intern(s)
    }
    // This is safe in the middle of elaboration. The pool is addressed by
    // `ConstId`, so adding a constant never moves one that is already there.
    fn number_const(&mut self, lit: &ast::NumberLiteral) -> crate::core_ir::ConstId {
        let c = self.c.const_number(lit);
        self.c.add_constant(c)
    }
    fn string_const(&mut self, s: &str) -> crate::core_ir::ConstId {
        self.c.const_str(s)
    }
    fn int_const(&mut self, i: i64) -> crate::core_ir::ConstId {
        self.c.const_int(i)
    }
    fn binary_const(&mut self, bytes: Vec<u8>, bit_len: u64) -> crate::core_ir::ConstId {
        self.c.const_binary(bytes, bit_len)
    }
    fn toplevel_fn_ty(&mut self, name: StrId) -> Option<Ty> {
        let name = self.c.engine.str(name).to_string();
        let scheme = self.c.env.lookup(&name)?;
        Some(
            self.c
                .engine
                .instantiate(scheme, &self.frames.current.rigid),
        )
    }
    fn decl_denotation(&self, binder: BinderId) -> Option<Denotation> {
        ModuleChecker::decl_denotation(self, binder)
    }
    fn ctor_field(&mut self, receiver: Ty, field: StrId) -> Option<FieldAt> {
        let resolved = self.c.engine.find(receiver);
        let (type_id, type_args) = match self.c.engine.node(resolved) {
            TypeNode::Con { id, args, .. } => (id, self.c.engine.children_of(args).to_vec()),
            _ => return None,
        };
        let info = self.c.env.lookup_type_info_by_id(type_id)?;
        let shared = self.field_in_variants(info, &type_args, field, None).ok()?;
        if let Some(&first) = shared.slots.first()
            && shared.slots.iter().all(|&s| s == first)
        {
            return Some(FieldAt::Same(first));
        }
        let variants = info.variants()?;
        let mut layouts = Vec::with_capacity(shared.slots.len());
        for (vi, &slot) in shared.slots.iter().enumerate() {
            let v = *self.c.engine.variants_of(variants).get(vi)?;
            let declared: SmallVec<[Ty; 4]> = self
                .c
                .engine
                .variant_fields_of(v.fields)
                .iter()
                .map(|f| f.ty)
                .collect();
            let fields = declared
                .into_iter()
                .map(|t| {
                    self.c
                        .engine
                        .substitute_type_vars(t, info.type_params, &type_args)
                })
                .collect();
            layouts.push(VariantLayout {
                variant: crate::core_ir::VariantRef {
                    type_id,
                    variant_idx: u16::try_from(vi).ok()?,
                },
                fields,
                slot,
            });
        }
        Some(FieldAt::PerVariant(layouts))
    }
    /// Read the labels off the type by `VariantRef`, not off the constructor's
    /// scheme by name, because the bare name in `mod.Ctor(..)` is not in `env`.
    fn ctor_labels(&mut self, v: crate::core_ir::VariantRef) -> Option<Vec<StrId>> {
        let info = self.c.env.lookup_type_info_by_id(v.type_id)?;
        let variants = info.variants()?;
        let variant = *self
            .c
            .engine
            .variants_of(variants)
            .get(v.variant_idx as usize)?;
        Some(
            self.c
                .engine
                .variant_fields_of(variant.fields)
                .iter()
                .map(|f| f.label)
                .collect(),
        )
    }
    fn fn_of_global(&self, slot: GlobalSlot) -> Option<crate::core_ir::FuncIdx> {
        self.c.global_to_func.get(&slot).copied()
    }
    fn toplevel_decls(&self) -> Vec<(usize, GlobalSlot)> {
        self.toplevel_decls
            .iter()
            .map(|d| (d.node, d.slot))
            .collect()
    }
    fn or_shape(&mut self, lhs_ty: Ty) -> Option<OrShape> {
        use crate::core_ir::VariantRef;
        let resolved = self.c.engine.find(lhs_ty);
        let TypeNode::Con { id, .. } = self.c.engine.node(resolved) else {
            return None;
        };
        let (ok, fail, err_has_payload) = if self.c.prelude.option().is(id) {
            (self.c.prelude.some(), self.c.prelude.none(), false)
        } else if self.c.prelude.result().is(id) {
            (self.c.prelude.ok(), self.c.prelude.err(), true)
        } else {
            return None;
        };
        Some(OrShape {
            fail: VariantRef {
                type_id: fail.type_id,
                variant_idx: fail.variant_idx,
            },
            ok: VariantRef {
                type_id: ok.type_id,
                variant_idx: ok.variant_idx,
            },
            err_has_payload,
        })
    }
    fn ty_nil(&mut self) -> Ty {
        self.c.ty_nil()
    }
}
