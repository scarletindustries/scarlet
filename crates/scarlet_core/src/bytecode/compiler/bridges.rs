//! `Compiler`'s impls of the traits the elaborator speaks to the enclosing
//! compilation through ([`PreludeTys`], [`ElabCtx`]), kept here so the
//! elaborator never sees the compiler's fields.

use super::*;
use crate::core_ir::WireOp;
use crate::typed_ir::{FieldAt, PreludeTys, VariantLayout, wire};
use scarlet_types::types::TypeBody;

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
    // Safe mid-elaboration: the pool is `ConstId`-addressed, so pooling moves
    // no address.
    fn number_const(&mut self, lit: &ast::NumberLiteral) -> crate::core_ir::ConstId {
        let c = self.const_number(lit);
        self.add_constant(c)
    }
    fn string_const(&mut self, s: &str) -> crate::core_ir::ConstId {
        self.const_str(s)
    }
    fn int_const(&mut self, i: i64) -> crate::core_ir::ConstId {
        self.const_int(i)
    }
    fn binary_const(&mut self, bytes: Vec<u8>, bit_len: u64) -> crate::core_ir::ConstId {
        self.const_binary(bytes, bit_len)
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
            (None, Some(Place::Outside(den))) => den,
            (None, Some(Place::ThisFunction)) => typed_ir::elaborator_bug(
                "a name this function binds has no binding in the elaborator",
                Span::DUMMY,
            ),
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
    fn ctor_field(&mut self, receiver: Ty, field: StrId) -> Option<FieldAt> {
        let resolved = self.engine.find(receiver);
        let (type_id, type_args) = match self.engine.node(resolved) {
            TypeNode::Con { id, args, .. } => (id, self.engine.children_of(args).to_vec()),
            _ => return None,
        };
        let info = self.env.lookup_type_info_by_id(type_id)?;
        let shared = self.field_in_variants(info, &type_args, field, None).ok()?;
        if let Some(&first) = shared.slots.first()
            && shared.slots.iter().all(|&s| s == first)
        {
            return Some(FieldAt::Same(first));
        }
        let variants = info.variants()?;
        let mut layouts = Vec::with_capacity(shared.slots.len());
        for (vi, &slot) in shared.slots.iter().enumerate() {
            let v = *self.engine.variants_of(variants).get(vi)?;
            let declared: SmallVec<[Ty; 4]> = self
                .engine
                .variant_fields_of(v.fields)
                .iter()
                .map(|f| f.ty)
                .collect();
            let fields = declared
                .into_iter()
                .map(|t| {
                    self.engine
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
        let (ok, fail, err_has_payload) = if self.prelude.option().is(id) {
            (self.prelude.some(), self.prelude.none(), false)
        } else if self.prelude.result().is(id) {
            (self.prelude.ok(), self.prelude.err(), true)
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
        Compiler::ty_nil(self)
    }

    /// **The type is read here, and here is after the whole module's check
    /// walk.** `analyse_module` finishes before any body is elaborated, so a
    /// `decode` whose payload is fixed only by a later `match` — which is the
    /// ordinary way one is written — is already solved by the time this runs.
    /// Asking any earlier describes an unresolved variable and refuses, and
    /// the refusal reads as "type is not known" on a program that is perfectly
    /// well typed.
    fn wire_descriptor(
        &mut self,
        pool: &mut ResolvedPool,
        ty: RTy,
        op: WireOp,
        at: Span,
    ) -> Option<WireDescIdx> {
        let desc = match wire::build_desc(pool, &mut *self, ty) {
            Ok(desc) => desc,
            Err(refusal) => {
                let msg = refusal.message(&*self, pool, op);
                self.error(msg, at);
                return None;
            }
        };
        let rt = desc.to_runtime();
        // Two call sites at one type describe it twice, and both must reach
        // the same entry. Descriptor equality is the right key and the
        // fingerprint is NOT: the fingerprint deliberately excludes the type's
        // identity, so two different types of one shape share it while naming
        // different constructors, and deduping on it would hand one type's
        // call site the other's.
        if let Some(i) = (&self.wire_descs).into_iter().position(|d| *d == rt) {
            return Some(WireDescIdx::from_usize(i));
        }
        Some(self.wire_descs.push(rt))
    }
}

/// The declaration half of the descriptor builder's seam. Everything it
/// answers comes off `env`/`engine`; nothing here decides encodability, which
/// is [`wire`]'s alone.
impl wire::WireCtx for Compiler {
    fn nominal(&mut self, pool: &mut ResolvedPool, id: TypeId) -> wire::Nominal {
        // The structural primitives answer first. `Int`, `Array`, `Map` and
        // the rest are declared with no body, so reading the body would report
        // every one of them as uninhabited and write nothing for an `Int`.
        if let Some(b) = self.wire_builtin(id) {
            return wire::Nominal::Builtin(b);
        }
        let Some(info) = self.env.lookup_type_info_by_id(id) else {
            wire::wire_bug("a type the checker admitted but never registered")
        };
        match info.body() {
            TypeBody::External => match self.wire_handle(id) {
                Some(kind) => wire::Nominal::Handle(kind),
                None => wire::Nominal::Uninhabited,
            },
            // A body is attached by the pass that declares it, and an imported
            // declaration is exported only after its own body is. Elaboration,
            // the only caller, starts once every pass has run, so a body still
            // in this state is the checker's bug; describing it as anything
            // would write nothing for a value that exists.
            TypeBody::Unresolved => wire::wire_bug("a type whose body is not yet hydrated"),
            TypeBody::Alias { target } => wire::Nominal::Alias(self.resolve_rty(pool, target)),
            TypeBody::Custom { variants, .. } => {
                // Copied out of the arena first: interning a field type
                // reborrows the engine.
                let vs = self.engine.variants_of(variants).to_vec();
                let ctors = vs
                    .iter()
                    .enumerate()
                    .map(|(i, v)| {
                        let declared: Vec<(StrId, Ty)> = self
                            .engine
                            .variant_fields_of(v.fields)
                            .iter()
                            .map(|f| (f.label, f.ty))
                            .collect();
                        let fields = declared
                            .into_iter()
                            .map(|(label, t)| (label, self.resolve_rty(pool, t)))
                            .collect();
                        wire::CtorDecl::new(
                            VariantRef {
                                type_id: id,
                                // The variant slice's own length is a `u16`,
                                // so an index into it cannot overflow one.
                                variant_idx: i as u16,
                            },
                            v.name,
                            fields,
                        )
                    })
                    .collect();
                wire::Nominal::Data { ctors }
            }
        }
    }

    fn name(&self, s: StrId) -> String {
        self.engine.str(s).to_string()
    }

    fn type_name(&self, id: TypeId) -> String {
        match self.env.lookup_type_info_by_id(id) {
            Some(info) => self.engine.str(info.name).to_string(),
            // Only reachable from a refusal's rendered type, where a name
            // nobody registered reads better than an abort on the error path.
            None => format!("<type {}>", id.0),
        }
    }
}

impl Compiler {
    /// The six types the wire format writes structurally rather than as a
    /// tagged constructor.
    ///
    /// Identity is the prelude's own binding, never a name: a user's
    /// `type Map(k, v)` is a different type from `scarlet/map`'s and must be
    /// described by its constructors. `Map` is late-bound, so this answers
    /// `None` for it until `scarlet/map` has loaded — which is also the first
    /// moment a value of it can exist.
    fn wire_builtin(&self, id: TypeId) -> Option<wire::Builtin> {
        let p = &self.prelude;
        if p.int().is(id) {
            Some(wire::Builtin::Int)
        } else if p.float().is(id) {
            Some(wire::Builtin::Float)
        } else if p.string().is(id) {
            Some(wire::Builtin::String)
        } else if p.binary().is(id) {
            Some(wire::Builtin::Binary)
        } else if p.array().is(id) {
            Some(wire::Builtin::Array)
        } else if p.map().is(id) {
            Some(wire::Builtin::Map)
        } else {
            None
        }
    }

    /// The five host-backed stdlib types the wire format writes as an
    /// identity, or `None` for a bodiless type that is not one of them.
    ///
    /// Identity is the declaring module's own binding, resolved through the
    /// module table and never a name: a user's `pub type Pid` is a different
    /// type and stays bodiless. `Port` is absent on purpose — it is a record
    /// over a `Connection`, and the runtime kind its stream carries is the
    /// value's to write (see [`scarlet_ir::wire::HandleKind`]).
    fn wire_handle(&mut self, id: TypeId) -> Option<wire::HandleKind> {
        const HANDLES: &[(&[&str], &str, wire::HandleKind)] = &[
            (&["scarlet", "process"], "Pid", wire::HandleKind::Pid),
            (
                &["scarlet", "process"],
                "Subject",
                wire::HandleKind::Subject,
            ),
            (
                &["scarlet", "net", "socket"],
                "Connection",
                wire::HandleKind::Connection,
            ),
            (&["scarlet", "net"], "Server", wire::HandleKind::Listener),
            (
                &["scarlet", "net", "tls"],
                "TlsConnection",
                wire::HandleKind::Tls,
            ),
        ];
        for &(module, name, kind) in HANDLES {
            let path: ModulePath = module.iter().map(|s| s.to_string()).collect();
            // A module this compile never loaded declares no type a value of
            // `id` could have, so a miss here is not a miss on `id`.
            let Some(iface) = self.module_table.get(&ModuleKey::of(&path)) else {
                continue;
            };
            if iface.types.get(name).is_some_and(|et| et.info.id == id) {
                return Some(kind);
            }
        }
        None
    }
}
