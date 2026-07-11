//! Module top level: the multi-pass declaration analysis that fronts the
//! compiler's fused infer+emit pass.
//!
//! Top-level declarations are mutually recursive and order-free — a fn may
//! call a fn defined below it, a type may mention a type defined below it —
//! so bodies cannot simply be compiled in source order. `analyse_module`
//! makes the order explicit in passes, each establishing one kind of fact
//! before the next pass needs it:
//!
//! | pass | does                                                          |
//! |------|---------------------------------------------------------------|
//! | 0    | partition nodes; duplicate/reserved checks; siphon `@vm` fns  |
//! | 1    | register custom-type heads (name + params) and opaque types   |
//! | 2    | toposort alias dependencies; register aliases                 |
//! | 3    | pre-allocate one global slot per fn/const; register fn        |
//! |      | signatures                                                    |
//! | 3.5  | hydrate constructor bodies; define ctor values                |
//! | 4    | build the fn/const call graph; Tarjan SCC                     |
//! | 5    | infer each SCC, callees first; generalize per SCC; then walk  |
//! |      | the non-declaration nodes                                     |
//! | 6    | elaborate every parked body to Core IR and bytecode           |
//!
//! Pass 5 hands each body to `compiler.rs` (`compile_declared_function` /
//! `compile_expr_with_hint`). Members of one SCC are inferred at a single
//! engine level against their pre-registered signatures and generalized
//! together afterwards, so mutual recursion typechecks monomorphically
//! inside the group and polymorphically outside it. Non-declaration nodes
//! (let bindings, bare expressions) run last, in source order.
//!
//! Pass 6 is a *phase boundary*, not a step of pass 5. The whole of pass 5 is
//! bracketed by one `begin_deferred_elaboration`/`end_deferred_elaboration`
//! pair, so no body — declared fn, nested lambda, or a lambda bound by a
//! toplevel `let` — is lowered or emitted while the module is being
//! typechecked. Pass 6 is the single loop that drains them.
//!
//! Two things make that boundary worth having. Types: the fused pipeline
//! lowered a body while the SCC around it was still being inferred, handing
//! `lower` a var that `generalize_top` or a later sibling was about to move.
//! Shape: with emit hoisted out of the walk, the walk's product is the module
//! as a whole, which is what `lower(p: &TypedProgram) -> CoreProgram` consumes
//! — a per-SCC drain could only ever have fed it one SCC at a time.
//!
//! # Invariant: per-decl data is positional, never name-keyed
//!
//! Data computed in an early pass and consumed in a later one (`Prepared`,
//! `PreparedType`) is carried in `Vec`s built one-to-one with the decl
//! lists and indexed positionally. Keying by name desyncs the moment two
//! decls share a name — the decl list keeps both while a map keeps one —
//! so that bug class is kept unrepresentable here.

use std::collections::{HashMap, HashSet};

use petgraph::Directed;
use petgraph::algo::tarjan_scc;
use petgraph::stable_graph::{NodeIndex, StableGraph};

use super::Op;
use super::compiler::{Compiler, ToplevelDecl};
use crate::ast;
use crate::module::{self, ExportedValue, ModuleInterface};
use crate::reference::DefId;
use crate::span::Span;
use crate::type_def::TypeId;
use crate::typed_ir::GlobalSlot;
use crate::types::{
    AddedTypeVar, ArenaSlice, DefinitionLocation, EntityKind, Hydrator, NO_STR, Scheme, StrId, Ty,
    TypeBody, TypeInfo, TypeParam, ValueKind, Variant, VariantField, pool,
};

// ---------------------------------------------------------------------------
// Top-level decl wrapper for the call-graph nodes.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Decl<'a> {
    Fn {
        fd: &'a ast::FunctionDeclaration,
        is_pub: bool,
        body: &'a ast::Expression,
        /// Index of this declaration in the module block's `body`.
        node: usize,
    },
    Const {
        cb: &'a ast::ConstBinding,
        is_pub: bool,
        /// Index of this declaration in the module block's `body`.
        node: usize,
    },
}

impl<'a> Decl<'a> {
    fn name(&self) -> &'a str {
        match self {
            Decl::Fn { fd, .. } => &fd.identifier.name,
            Decl::Const { cb, .. } => &cb.identifier.name,
        }
    }

    /// The declaration's position in the module block's `body`. This — not its
    /// name — is what the toplevel elaboration schedules on.
    fn node(&self) -> usize {
        match *self {
            Decl::Fn { node, .. } | Decl::Const { node, .. } => node,
        }
    }
}

/// Per-declaration data computed in Pass 3 and consumed in Pass 5.
///
/// The original design stashed this in three parallel `HashMap`s keyed by
/// declaration name (`prereg_fn_tys`, `hydrators`, `decl_slots`). Two decls
/// with the same name silently collapsed those maps to one entry while the
/// decl list still held both, so Pass 5 desynced and panicked. Carrying the
/// data in a `Vec` built one-to-one from `decls` makes that class of bug
/// unrepresentable: lookups are positional, never by name.
enum Prepared<'a> {
    Fn {
        fd: &'a ast::FunctionDeclaration,
        is_pub: bool,
        slot: i32,
        body: &'a ast::Expression,
        param_tys: Vec<Ty>,
        ret_ty: Ty,
        hydrator: Hydrator,
    },
    Const {
        cb: &'a ast::ConstBinding,
        is_pub: bool,
        slot: i32,
    },
}

impl<'a> Prepared<'a> {
    /// A function's parameter names, in declaration order; empty otherwise.
    /// Documentation only — see `ExportedValue::param_names`.
    fn param_names(&self) -> Vec<String> {
        match self {
            Prepared::Fn { fd, .. } => fd
                .params
                .iter()
                .map(|p| p.identifier.name.clone())
                .collect(),
            _ => Vec::new(),
        }
    }

    fn doc(&self) -> Option<String> {
        match self {
            Prepared::Fn { fd, .. } => fd.doc.clone(),
            Prepared::Const { cb, .. } => cb.doc.clone(),
        }
    }

    fn name(&self) -> &'a str {
        match self {
            Prepared::Fn { fd, .. } => &fd.identifier.name,
            Prepared::Const { cb, .. } => &cb.identifier.name,
        }
    }
    fn name_span(&self) -> Span {
        match self {
            Prepared::Fn { fd, .. } => fd.identifier.span,
            Prepared::Const { cb, .. } => cb.identifier.span,
        }
    }
    fn is_pub(&self) -> bool {
        match self {
            Prepared::Fn { is_pub, .. } | Prepared::Const { is_pub, .. } => *is_pub,
        }
    }
    fn slot(&self) -> i32 {
        match self {
            Prepared::Fn { slot, .. } | Prepared::Const { slot, .. } => *slot,
        }
    }
    fn is_const(&self) -> bool {
        matches!(self, Prepared::Const { .. })
    }
}

/// Per-type data computed in Pass 1 and consumed in Pass 3.5.
///
/// The original design stashed this in two parallel name-keyed maps
/// (`type_param_generics` and the shared `Compiler::hydrators` field), filled
/// in Pass 1 and looked up by type name in Pass 3.5. Two `type` decls with
/// the same name collapsed those maps to one entry while `type_decls` kept
/// both — the exact desync class as the fn/`Prepared` bug. Carrying this in a
/// `Vec` built one-to-one with `type_decls` makes the desync unrepresentable:
/// Pass 3.5 reads it positionally, never by name. Aliases push a placeholder
/// entry (`TypeId::NONE`, `NO_STR`, empty hydrator) that is dropped unread by
/// `register_constructors`' non-`Variants` early return.
struct PreparedType {
    type_id: TypeId,
    name_id: StrId,
    hydrator: Hydrator,
    param_tys: Vec<Ty>,
}

/// Hydrated shape of one `fn` declaration's signature. Returned by
/// [`Compiler::hydrate_fn_signature`] so callers destructure by field name
/// instead of guessing which `Ty` in a positional tuple is the return type.
struct FnSig {
    hydrator: Hydrator,
    param_tys: Vec<Ty>,
    ret_ty: Ty,
    fn_ty: Ty,
}

// ---------------------------------------------------------------------------
// Entry point.
// ---------------------------------------------------------------------------

impl Compiler {
    /// Keep a non-declaration node, erroring first when inside an imported module.
    fn push_non_decl<'a>(
        &mut self,
        node: &'a ast::Node,
        in_module: bool,
        others: &mut Vec<&'a ast::Node>,
    ) {
        if in_module {
            self.error(
                "Modules may only contain declarations at the top level".to_string(),
                node.span(),
            );
        }
        others.push(node);
    }

    /// Analyse a module body. When `iface` is `Some` the module is being
    /// compiled as an import and its `pub` items are recorded into the
    /// interface; when `None` it is the entry module.
    ///
    /// The caller owns the enclosing `env` scope: this walk defines value
    /// schemes (constructors, fns, consts) into whatever scope is current on
    /// entry and does *not* pop it, so the entry-file caller can keep those
    /// schemes visible across the subsequent Core `lower_body` on `__main__`
    /// (which re-resolves ctor names via `LowerCtx::resolve_name`). Import
    /// callers bracket the call with their own `push_scope`/`pop_scope`.
    pub(super) fn analyse_module(
        &mut self,
        block: &ast::BlockExpression,
        mut iface: Option<&mut ModuleInterface>,
    ) {
        self.push_local_scope();

        // -------------------------------------------------------------------
        // Pass 0 — partition + reserved/duplicate name checks.
        // -------------------------------------------------------------------
        let mut type_decls: Vec<(&ast::TypeDeclaration, bool)> = Vec::new();
        let mut decls: Vec<Decl<'_>> = Vec::new();
        let mut vm_fns: Vec<(&ast::FunctionDeclaration, bool, Op)> = Vec::new();
        let mut other_nodes: Vec<&ast::Node> = Vec::new();

        let in_prelude = self.current_module == module::al_prelude();
        let mut seen_types: HashMap<&str, Span> = HashMap::new();
        let mut seen_values: HashMap<&str, Span> = HashMap::new();

        let in_stdlib = self.current_module.first().map(String::as_str) == Some("al");

        for (node_idx, node) in block.body.iter().enumerate() {
            match node {
                ast::Node::Statement(stmt) => match stmt.as_ref() {
                    ast::Statement::Declaration { decl, public } => {
                        let is_public = *public;
                        match decl.as_ref() {
                            ast::Declaration::Type(td) => {
                                self.validate_attributes(
                                    &td.attributes,
                                    in_stdlib,
                                    AttrTarget::Type,
                                );
                                let dup = check_duplicate(self, &mut seen_types, &td.identifier);
                                check_reserved(self, &td.identifier, in_prelude);
                                if let ast::TypeBody::Variants { ctors, .. } = &td.body {
                                    for ctor in ctors {
                                        check_reserved(self, &ctor.identifier, in_prelude);
                                        check_duplicate(self, &mut seen_values, &ctor.identifier);
                                    }
                                }
                                if !dup {
                                    type_decls.push((td, is_public));
                                }
                            }
                            ast::Declaration::Function(fd) => {
                                self.validate_attributes(&fd.attributes, in_stdlib, AttrTarget::Fn);
                                if !check_duplicate(self, &mut seen_values, &fd.identifier) {
                                    // `@vm` fns carry no AL body: siphon them off
                                    // here so every surviving `Decl::Fn` holds a
                                    // real expression body. They are registered
                                    // with their `Builtin` scheme in Pass 3.
                                    match &fd.body {
                                        ast::FnBody::Vm(key) => {
                                            match super::builtin_op(&key.name) {
                                                Some(op) => vm_fns.push((fd, is_public, op)),
                                                None => self.error(
                                                    format!("unknown @vm builtin '{}'", key.name),
                                                    key.span,
                                                ),
                                            }
                                        }
                                        ast::FnBody::Block(body) => decls.push(Decl::Fn {
                                            fd,
                                            is_pub: is_public,
                                            body,
                                            node: node_idx,
                                        }),
                                    }
                                }
                            }
                            ast::Declaration::Const(cb) => {
                                if !check_duplicate(self, &mut seen_values, &cb.identifier) {
                                    decls.push(Decl::Const {
                                        cb,
                                        is_pub: is_public,
                                        node: node_idx,
                                    });
                                }
                            }
                        }
                    }
                    ast::Statement::ImportDeclaration(_) => {
                        // Already handled by `process_imports`.
                    }
                    _ => self.push_non_decl(node, iface.is_some(), &mut other_nodes),
                },
                ast::Node::Expression(_) => {
                    self.push_non_decl(node, iface.is_some(), &mut other_nodes)
                }
            }
        }

        // -------------------------------------------------------------------
        // Pass 1 — register custom-type heads (name + params, empty variants)
        // and opaque types. Aliases are deferred to Pass 2 because their RHS
        // can reference these heads.
        // -------------------------------------------------------------------
        let mut prepared_types: Vec<PreparedType> = Vec::with_capacity(type_decls.len());
        for (td, is_public) in &type_decls {
            if matches!(td.body, ast::TypeBody::Alias(_)) {
                prepared_types.push(PreparedType {
                    type_id: TypeId::NONE,
                    name_id: NO_STR,
                    hydrator: Hydrator::new(),
                    param_tys: Vec::new(),
                });
                continue;
            }
            let (h, type_params, param_tys) = self.hydrate_type_params(&td.type_params);
            let name_id = self.engine.intern(&td.identifier.name);
            let module = self.current_module_slice();
            let type_id =
                self.env
                    .register_type_head(&td.identifier.name, name_id, module, type_params);
            self.store_and_emit_def(&td.identifier, &td.doc, EntityKind::Type, *is_public);
            if matches!(td.body, ast::TypeBody::External) {
                self.env
                    .set_type_body(&td.identifier.name, TypeBody::External);
            }
            prepared_types.push(PreparedType {
                type_id,
                name_id,
                hydrator: h,
                param_tys,
            });
        }

        // -------------------------------------------------------------------
        // Pass 2 — toposort + register aliases.
        // -------------------------------------------------------------------
        self.register_aliases(&type_decls);

        // -------------------------------------------------------------------
        // Pass 3 — pre-allocate one slot per decl, then register fn
        // signatures. The results are carried positionally in `prepared`
        // (one entry per `decls` entry, same order) so Pass 5 never has to
        // look anything up by name.
        //
        // `@vm` fns (siphoned off in Pass 0) get a `Builtin{op}` scheme, no
        // slot, no body codegen, and are exported here rather than after
        // generalisation (their type is the annotated signature verbatim —
        // there is no body to infer from).
        // -------------------------------------------------------------------
        for &(fd, is_pub, op) in &vm_fns {
            let name = &fd.identifier.name;
            let fn_ty = self.hydrate_fn_signature(fd).fn_ty;
            let mut scheme = self.engine.generalize_top(fn_ty);
            scheme.kind = ValueKind::Builtin { op };
            let m = self.current_module_slice();
            let dl = DefinitionLocation::new(fd.identifier.span, m, EntityKind::Function);
            scheme.def = Some(dl);
            self.env.define_at(name, scheme, dl);
            self.env.store_doc_opt(name, &fd.doc);
            self.emit_def(dl, name, fd.doc.clone(), is_pub);
            let defid = self.defid_of(dl);
            let names = fd
                .params
                .iter()
                .map(|p| p.identifier.name.clone())
                .collect();
            self.module_refs.set_param_names(defid, names);
            self.record(name, scheme.ty, fd.identifier.span, fd.doc.clone());
            let params = fd
                .params
                .iter()
                .map(|p| p.identifier.name.clone())
                .collect();
            export_value(
                iface.as_deref_mut(),
                name,
                is_pub,
                scheme,
                None,
                params,
                fd.doc.clone(),
            );
        }

        // Not `get_or_create_local`: a decl's slot is published to the toplevel
        // elaboration by `toplevel_decls` (recorded in SCC order in Pass 5,
        // below), not by the module-scope bind queue `bind_local` feeds. Only
        // `let`/destructuring binds — which have no `Decl` to hang a slot on —
        // go through that queue.
        let slots: Vec<i32> = decls
            .iter()
            .map(|d| self.alloc_decl_slot(d.name()))
            .collect();

        let mut prepared: Vec<Prepared> = Vec::with_capacity(decls.len());
        for (d, &slot) in decls.iter().zip(&slots) {
            let p = match *d {
                Decl::Fn {
                    fd, is_pub, body, ..
                } => {
                    let FnSig {
                        hydrator,
                        param_tys,
                        ret_ty,
                        fn_ty,
                    } = self.hydrate_fn_signature(fd);
                    let m = self.current_module_slice();
                    let dl = DefinitionLocation::new(fd.identifier.span, m, EntityKind::Function);
                    self.env.define_at(
                        &fd.identifier.name,
                        Scheme {
                            quantified: ArenaSlice::EMPTY,
                            ty: fn_ty,
                            kind: ValueKind::ModuleFn,
                            def: Some(dl),
                        },
                        dl,
                    );
                    Prepared::Fn {
                        fd,
                        is_pub,
                        slot,
                        body,
                        param_tys,
                        ret_ty,
                        hydrator,
                    }
                }
                Decl::Const { cb, is_pub, .. } => Prepared::Const { cb, is_pub, slot },
            };
            prepared.push(p);
        }

        // -------------------------------------------------------------------
        // Pass 3.5 — hydrate constructor bodies; define ctor values.
        // -------------------------------------------------------------------
        for ((td, is_public), pt) in type_decls.iter().zip(prepared_types) {
            self.register_constructors(td, *is_public, pt, &mut iface);
        }

        // -------------------------------------------------------------------
        // Pass 4 — build call graph over fns+consts, Tarjan SCC.
        // -------------------------------------------------------------------
        let sccs = build_call_graph_sccs(&decls);

        // -------------------------------------------------------------------
        // Pass 5 — infer each SCC, then generalize. Every index in an SCC is
        // an index into `prepared`, so the per-decl data is reached
        // positionally and the match is exhaustive — no name lookups, no
        // "this can't happen" fallbacks.
        //
        // THE PHASE BOUNDARY. Everything from here to `end_deferred_elaboration`
        // below is the typecheck/elaborate walk: it infers types, reserves
        // `Function` slots and records capture shapes, and emits *no* body
        // bytecode. Every body it walks — declared fn, nested lambda, a lambda
        // bound by a toplevel `let` — is parked in `deferred_bodies`. The Core
        // pipeline (`lower` → `perceus` → `emit`) then runs once, in one loop,
        // over the parked bodies after the whole module has been walked. That
        // is the shape `lower(p: &TypedProgram) -> CoreProgram` needs: a
        // whole-module IR handed to a lowering that runs strictly after
        // inference, never inside it.
        //
        // Draining per-SCC (as this used to) was already enough for *types* —
        // an SCC's types are final once it has been generalized — but it left
        // `emit` interleaved with the walk, so `lower` could never be handed
        // the module as a whole. Draining once, at the end, is strictly more
        // resolved: a later SCC never touches an earlier one's vars.
        //
        // A parked body's frame/scope snapshot (`DeferredBody::outer_scopes` /
        // `capture_env`) is *not* enough on its own, though: `resolve_name`
        // re-reads `self.env` at drain time for a free name's `Ty` and
        // `ValueKind`, and a toplevel `let` overwrites a same-scope binding in
        // place. `pin_deferred_env` below freezes the env the decl walk saw so
        // a later `let` cannot re-point a name a decl body already resolved.
        // -------------------------------------------------------------------
        self.begin_deferred_elaboration();
        for scc in &sccs {
            self.engine.enter_level();
            let mut inferred: Vec<(usize, Ty)> = Vec::with_capacity(scc.len());

            for &idx in scc {
                // Publish this decl to the toplevel elaboration: the body node
                // it occupies, and the entry-frame slot Pass 3 gave it. Pushed
                // in SCC-visit order, so the elaborated toplevel spine
                // initialises dependencies first (mirrors this loop's own
                // `StoreLocal` order) and a forward-referenced `const` is
                // stored before it is read. Overwritten per module, cleared to
                // the entry file's own decls at `code_mark`.
                let name_id = self.engine.intern(prepared[idx].name());
                self.toplevel_decls.push(ToplevelDecl {
                    node: decls[idx].node(),
                    name: name_id,
                    slot: GlobalSlot(slots[idx]),
                });
                match &prepared[idx] {
                    Prepared::Fn {
                        fd,
                        is_pub,
                        slot,
                        body,
                        param_tys,
                        ret_ty,
                        hydrator,
                    } => {
                        let name = &fd.identifier.name;
                        let m = self.current_module_slice();
                        let dl =
                            DefinitionLocation::new(fd.identifier.span, m, EntityKind::Function);
                        self.emit_def(dl, name, fd.doc.clone(), *is_pub);
                        let defid = self.defid_of(dl);
                        let names = fd
                            .params
                            .iter()
                            .map(|p| p.identifier.name.clone())
                            .collect();
                        self.module_refs.set_param_names(defid, names);
                        // Attribute every reference emitted while this body is
                        // compiled to the fn's own `DefId` so the dead-code
                        // reachability walk follows def→def edges. The owner
                        // spans nested lambdas (a lambda has no `DefId`, so its
                        // body's references belong to the enclosing definition).
                        let owner = self.owner_defid(fd.identifier.span, EntityKind::Function);
                        let fn_ty = self.with_owner(owner, |c| {
                            c.compile_declared_function(
                                name,
                                *slot,
                                &fd.params,
                                body,
                                param_tys.clone(),
                                *ret_ty,
                                hydrator,
                            )
                        });

                        self.env.store_doc_opt(name, &fd.doc);
                        self.record(name, fn_ty, fd.identifier.span, fd.doc.clone());
                        // Nothing recorded for the toplevel spine to read back:
                        // `compile_declared_function` keyed this body under the
                        // very slot the spine stores it into, and the elaborator
                        // asks `ElabCtx::fn_of_global` with the `GlobalSlot` it
                        // stamped on that decl's own `TypedBind`. A top-level fn
                        // captures nothing — sibling refs are `PushGlobal`, not
                        // by-value captures — so there is no capture list to
                        // carry either.
                        inferred.push((idx, fn_ty));
                    }
                    Prepared::Const { cb, is_pub, .. } => {
                        let name = &cb.identifier.name;
                        let m = self.current_module_slice();
                        self.emit_def(
                            DefinitionLocation::new(cb.identifier.span, m, EntityKind::Constant),
                            name,
                            cb.doc.clone(),
                            *is_pub,
                        );
                        let owner = self.owner_defid(cb.identifier.span, EntityKind::Constant);
                        let final_ty = self.with_owner(owner, |c| {
                            let mut h = Hydrator::new();
                            let annot_ty = cb.typ.as_ref().map(|t| c.hydrate(&mut h, t));
                            let init_ty = c.compile_expr_with_hint(&cb.init, annot_ty);
                            if let Some(a) = annot_ty {
                                c.engine.unify_at(a, init_ty, cb.identifier.span);
                                a
                            } else {
                                init_ty
                            }
                        });
                        self.env.store_doc_opt(name, &cb.doc);
                        self.record(name, final_ty, cb.identifier.span, cb.doc.clone());
                        inferred.push((idx, final_ty));
                    }
                }
            }

            self.engine.leave_level();

            let m = self.current_module_slice();
            for (idx, ty) in inferred {
                let mut scheme = self.engine.generalize_top(ty);
                let p = &prepared[idx];
                let entity = if p.is_const() {
                    EntityKind::Constant
                } else {
                    EntityKind::Function
                };
                scheme.def = Some(DefinitionLocation::new(p.name_span(), m, entity));
                if p.is_const() {
                    scheme.kind = ValueKind::Local;
                }
                self.env.define(p.name(), scheme);
            }
        }

        // Export fns/consts now that schemes are final.
        for p in &prepared {
            if let Some(&s) = self.env.lookup(p.name()) {
                export_value(
                    iface.as_deref_mut(),
                    p.name(),
                    p.is_pub(),
                    s,
                    Some(p.slot()),
                    p.param_names(),
                    p.doc(),
                );
            }
        }

        // -------------------------------------------------------------------
        // Other nodes (let bindings, bare expressions) — linear walk.
        //
        // The decl bodies parked above have not lowered yet, and a `let` here
        // may shadow a name one of them resolved (`println = 5` after a decl
        // that calls `println`). Freeze the env they were walked against; the
        // drain restores it around them.
        // -------------------------------------------------------------------
        if !other_nodes.is_empty() {
            self.pin_deferred_env();
        }
        // The only walk allowed to fill the positional `toplevel_binds` queue:
        // the toplevel elaboration drains it against exactly these nodes, in
        // this order. Nested walks (a lambda body, a deferred decl) re-enter
        // `compile_node` from here, but they push `outer_scopes`/`scope_marks`,
        // which `bind_local` also checks.
        let outer_walk = std::mem::replace(&mut self.walking_module_statements, true);
        for node in &other_nodes {
            let _ty = self.compile_node(node);
        }
        self.walking_module_statements = outer_walk;

        // End of the walk; start of the Core pipeline. Types are final —
        // `lower` reads `find()`-resolved types, and every var any SCC was ever
        // going to solve is solved. What none of them solved is now `Generic`,
        // which matters to `zonk` (a `Generic` root is quantifiable, an
        // `Unbound` one is an error) and to nothing else downstream: `lower`,
        // `perceus` and `emit` never read `root_var`. Do not read this as an
        // opcode win — the opcode mix is unchanged on the T0 corpus.
        self.end_deferred_elaboration();

        self.pop_local_scope();
    }

    fn hydrate_type_params(
        &mut self,
        params: &[ast::Identifier],
    ) -> (Hydrator, ArenaSlice<pool::TypeParams>, Vec<Ty>) {
        let mut h = Hydrator::new();
        let mut type_params: Vec<TypeParam> = Vec::with_capacity(params.len());
        let mut param_tys: Vec<Ty> = Vec::with_capacity(params.len());
        for tp in params {
            let AddedTypeVar { ty, id, duplicate } =
                h.add_type_variable(&tp.name, &mut self.engine);
            if duplicate {
                self.error(format!("Duplicate type parameter '{}'", tp.name), tp.span);
            }
            type_params.push(TypeParam {
                name: self.engine.intern(&tp.name),
                id,
            });
            param_tys.push(ty);
        }
        (h, self.engine.push_type_params(&type_params), param_tys)
    }

    /// Intern the current module path as a `str_slices` slice. Memoised so each
    /// module's path occupies one pool entry no matter how many types it
    /// declares.
    pub(super) fn current_module_slice(&mut self) -> ArenaSlice<pool::StrSlices> {
        if let Some(sl) = self.module_path_slice {
            return sl;
        }
        let segs = self.current_module.clone();
        let sl = self.engine.intern_slice(&segs);
        self.module_path_slice = Some(sl);
        sl
    }

    /// Record a named declaration: store its definition location and doc in
    /// the env, and emit the matching graph definition.
    fn store_and_emit_def(
        &mut self,
        id: &ast::Identifier,
        doc: &Option<String>,
        entity: EntityKind,
        is_pub: bool,
    ) {
        let dl = DefinitionLocation::new(id.span, self.current_module_slice(), entity);
        self.env.store_definition(&id.name, dl);
        self.env.store_doc_opt(&id.name, doc);
        self.emit_def(dl, &id.name, doc.clone(), is_pub);
    }

    /// Run `f` with `current_owner` set to `owner`, restoring the previous
    /// owner afterwards, so references emitted inside `f` are attributed to it.
    fn with_owner<R>(&mut self, owner: DefId, f: impl FnOnce(&mut Self) -> R) -> R {
        let prev = self.current_owner;
        self.current_owner = Some(owner);
        let r = f(self);
        self.current_owner = prev;
        r
    }

    fn hydrate_fn_signature(&mut self, fd: &ast::FunctionDeclaration) -> FnSig {
        let mut hydrator = Hydrator::new();
        let mut param_tys: Vec<Ty> = Vec::with_capacity(fd.params.len());
        for p in &fd.params {
            param_tys.push(self.hydrate_opt(&mut hydrator, p.typ.as_ref()));
        }
        let ret_ty = self.hydrate_opt(&mut hydrator, fd.return_type.as_ref());
        let fn_ty = self.engine.mk_fun(&param_tys, ret_ty);
        FnSig {
            hydrator,
            param_tys,
            ret_ty,
            fn_ty,
        }
    }

    // -----------------------------------------------------------------------
    // Pass 2 helper.
    // -----------------------------------------------------------------------

    fn register_aliases(&mut self, type_decls: &[(&ast::TypeDeclaration, bool)]) {
        let aliases: Vec<(&ast::TypeDeclaration, &ast::TypeIdentifier, bool)> = type_decls
            .iter()
            .filter_map(|(td, p)| match &td.body {
                ast::TypeBody::Alias(rhs) => Some((*td, rhs, *p)),
                _ => None,
            })
            .collect();
        if aliases.is_empty() {
            return;
        }

        let mut graph: StableGraph<(), (), Directed> = StableGraph::new();
        let mut idx_of: HashMap<&str, NodeIndex> = HashMap::new();
        for &(a, _, _) in &aliases {
            let n = graph.add_node(());
            idx_of.insert(a.identifier.name.as_str(), n);
        }
        for &(a, rhs, _) in &aliases {
            let from = idx_of[a.identifier.name.as_str()];
            let mut deps: Vec<&str> = Vec::new();
            collect_type_ast_deps(rhs, &mut deps);
            for dep in &deps {
                if let Some(&to) = idx_of.get(dep) {
                    graph.add_edge(from, to, ());
                }
            }
        }

        let by_idx: HashMap<NodeIndex, (&ast::TypeDeclaration, &ast::TypeIdentifier, bool)> =
            aliases
                .iter()
                .map(|a| (idx_of[a.0.identifier.name.as_str()], *a))
                .collect();

        // tarjan_scc yields components in reverse topological order (dependees
        // before dependers). A component with more than one node, or a single
        // node with a self-edge, is a cycle: report each member and skip it so
        // one bad alias does not prevent the rest from being registered.
        for component in tarjan_scc(&graph) {
            let cyclic =
                component.len() > 1 || graph.find_edge(component[0], component[0]).is_some();
            if cyclic {
                for node in &component {
                    if let Some(&(td, _, _)) = by_idx.get(node) {
                        self.error(
                            format!("Recursive type alias '{}'", td.identifier.name),
                            td.identifier.span,
                        );
                    }
                }
                continue;
            }
            let Some(&(td, rhs, is_pub)) = by_idx.get(&component[0]) else {
                continue;
            };
            let (mut h, type_params, _) = self.hydrate_type_params(&td.type_params);
            let name_id = self.engine.intern(&td.identifier.name);
            let module = self.current_module_slice();
            self.env
                .register_type_head(&td.identifier.name, name_id, module, type_params);
            h.disallow_new_type_variables();
            let owner = self.owner_defid(td.identifier.span, EntityKind::Type);
            let target = self.with_owner(owner, |c| c.hydrate(&mut h, rhs));
            self.env
                .set_type_body(&td.identifier.name, TypeBody::Alias { target });
            self.store_and_emit_def(&td.identifier, &td.doc, EntityKind::Type, is_pub);
        }
    }

    // -----------------------------------------------------------------------
    // Pass 3.5 helper.
    // -----------------------------------------------------------------------

    fn register_constructors(
        &mut self,
        td: &ast::TypeDeclaration,
        is_public: bool,
        pt: PreparedType,
        iface: &mut Option<&mut ModuleInterface>,
    ) {
        let ast::TypeBody::Variants { ctors, opaque } = &td.body else {
            // Aliases and externals have no constructors; just export the type info.
            let ti = self.env.lookup_type_info(&td.identifier.name);
            export_type(iface.as_deref_mut(), &td.identifier.name, is_public, ti);
            return;
        };
        let ctors_public = is_public && !*opaque;

        let PreparedType {
            type_id,
            name_id: type_name_id,
            hydrator: mut h,
            param_tys: param_generics,
        } = pt;
        h.disallow_new_type_variables();

        let type_name = &td.identifier.name;
        let m = self.current_module_slice();
        let mut variants: Vec<Variant> = Vec::with_capacity(ctors.len());

        // The constructor/field hydration below is the type's "body": any type
        // it names is a reference owned by this type, so the dead-code walk
        // sees type→type edges.
        let owner = self.owner_defid(td.identifier.span, EntityKind::Type);
        self.with_owner(owner, |c| {
            for (variant_idx, ctor) in ctors.iter().enumerate() {
                let mut field_itys: Vec<Ty> = Vec::with_capacity(ctor.fields.len());
                let mut field_defs: Vec<VariantField> = Vec::with_capacity(ctor.fields.len());
                let mut label_ids: Vec<StrId> = Vec::with_capacity(ctor.fields.len());

                for f in &ctor.fields {
                    let ity = c.hydrate(&mut h, &f.typ);
                    let label = c.engine.intern(&f.label.name);
                    field_itys.push(ity);
                    field_defs.push(VariantField { label, ty: ity });
                    label_ids.push(label);

                    let qualified = format!("{}.{}", ctor.identifier.name, f.label.name);
                    let field_dl = DefinitionLocation::new(f.label.span, m, EntityKind::Field);
                    c.env.definitions.insert(qualified, field_dl);
                    c.emit_def(field_dl, &f.label.name, None, ctors_public);
                }

                let fields = c.engine.push_variant_fields(&field_defs);
                variants.push(Variant {
                    name: c.engine.intern(&ctor.identifier.name),
                    fields,
                });
                let field_labels = c.engine.push_str_ids(&label_ids);

                // Build the constructor's type scheme.
                let result_ty = c.engine.mk_con_id(type_id, type_name_id, &param_generics);
                let ctor_ty = if field_itys.is_empty() {
                    result_ty
                } else {
                    c.engine.mk_fun(&field_itys, result_ty)
                };
                let mut scheme = c.engine.generalize_top(ctor_ty);
                scheme.kind = ValueKind::Constructor {
                    type_name: type_name_id,
                    type_id,
                    variant_idx: variant_idx as u16,
                    arity: ctor.fields.len() as u16,
                    field_labels,
                };
                scheme.def = Some(DefinitionLocation::new(
                    ctor.identifier.span,
                    m,
                    EntityKind::Constructor,
                ));

                let name = &ctor.identifier.name;
                c.env.define(name, scheme);
                c.store_and_emit_def(
                    &ctor.identifier,
                    &ctor.doc,
                    EntityKind::Constructor,
                    ctors_public,
                );
                let dl = DefinitionLocation::new(
                    ctor.identifier.span,
                    c.current_module_slice(),
                    EntityKind::Constructor,
                );
                let defid = c.defid_of(dl);
                let labels = c.engine.strs_of(field_labels);
                c.module_refs.set_param_names(defid, labels);
                // `Config(name: 'x')` names the constructor, never the type, so
                // reachability needs this edge or every single-constructor type
                // reads as unused.
                let type_dl = DefinitionLocation::new(
                    td.identifier.span,
                    c.current_module_slice(),
                    EntityKind::Type,
                );
                let type_defid = c.defid_of(type_dl);
                c.module_refs.set_ctor_of(defid, type_defid);
                if is_public {
                    // Opaque ctors are recorded as private so importers get a
                    // "constructor is private" hint instead of "unknown name".
                    // A constructor's field labels ARE semantic — they live in
                    // `ValueKind::Constructor.field_labels`. Mirror them here so
                    // hover can render `NotFound fn(path String) IoError`
                    // without reaching back into the engine's string pool.
                    let labels = c.engine.strs_of(field_labels);
                    export_value(
                        iface.as_deref_mut(),
                        name,
                        ctors_public,
                        scheme,
                        None,
                        labels,
                        ctor.doc.clone(),
                    );
                }
            }
        });

        // Write the now-complete variant slice back into the env.
        let variants = self.engine.push_variants(&variants);
        self.env
            .set_type_body(type_name, TypeBody::Custom { variants });

        let ti = self.env.lookup_type_info(type_name);
        export_type(iface.as_deref_mut(), type_name, is_public, ti);
    }
}

// ---------------------------------------------------------------------------
// Attributes.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum AttrTarget {
    Fn,
    Type,
}

impl Compiler {
    /// Reject unknown attributes, attributes on the wrong target, and any
    /// attribute outside the embedded stdlib. Arity is checked here so the
    /// later passes can assume `@vm` always carries exactly one arg.
    fn validate_attributes(&mut self, attrs: &[ast::Attribute], in_stdlib: bool, on: AttrTarget) {
        for a in attrs {
            match a.name.name.as_str() {
                "vm" => {
                    if !in_stdlib {
                        self.error(
                            "'@vm' is only allowed in the standard library".to_string(),
                            a.span,
                        );
                    }
                    if !matches!(on, AttrTarget::Fn) {
                        self.error("'@vm' may only be used on functions".to_string(), a.span);
                    }
                    if a.args.len() != 1 {
                        self.error(
                            "'@vm' takes exactly one argument: the VM op key".to_string(),
                            a.span,
                        );
                    }
                }
                other => {
                    self.error(format!("Unknown attribute '@{other}'"), a.span);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers (free functions).
// ---------------------------------------------------------------------------

/// Record a name in the module interface: into `values`/`types` when public,
/// into `private_names` otherwise so importers get a "this is private" error.
fn export_value(
    iface: Option<&mut ModuleInterface>,
    name: &str,
    is_pub: bool,
    scheme: Scheme,
    slot: Option<i32>,
    param_names: Vec<String>,
    doc: Option<String>,
) {
    let Some(iface) = iface else { return };
    if is_pub {
        let ev = ExportedValue {
            scheme,
            local_slot: slot,
            param_names,
            doc,
        };
        iface.values.insert(name.to_string(), ev);
    } else {
        iface.private_names.insert(name.to_string());
    }
}

fn export_type(
    iface: Option<&mut ModuleInterface>,
    name: &str,
    is_pub: bool,
    ti: Option<TypeInfo>,
) {
    let Some(iface) = iface else { return };
    if is_pub {
        if let Some(ti) = ti {
            iface.types.insert(name.to_string(), ti);
        }
    } else {
        iface.private_names.insert(name.to_string());
    }
}

/// Records a top-level name, emitting a diagnostic if it was already defined.
/// Returns `true` when the name is a duplicate so the caller can drop the
/// redundant declaration — keeping it would emit redundant codegen and a
/// second, confusing definition after the diagnostic. (Downstream passes carry
/// per-declaration state positionally and do not assume one decl per name.)
fn check_duplicate<'a>(
    c: &mut Compiler,
    seen: &mut HashMap<&'a str, Span>,
    id: &'a ast::Identifier,
) -> bool {
    if let Some(prev) = seen.insert(id.name.as_str(), id.span) {
        c.error(format!("'{}' is already defined", id.name), id.span);
        c.note("first definition was here".to_string(), prev);
        true
    } else {
        false
    }
}

/// Emits a diagnostic if `id` collides with a prelude-reserved name.
fn check_reserved(c: &mut Compiler, id: &ast::Identifier, in_prelude: bool) {
    if !in_prelude && c.is_reserved(&id.name) {
        c.error(
            format!(
                "'{}' is defined in the prelude and cannot be redefined",
                id.name
            ),
            id.span,
        );
    }
}

/// Walk a type-annotation AST collecting every named-type reference.
fn collect_type_ast_deps<'a>(t: &'a ast::TypeIdentifier, out: &mut Vec<&'a str>) {
    match &t.kind {
        ast::TypeKind::NamedType(nt) => {
            out.push(nt.identifier.name.as_str());
            for a in &nt.type_args {
                collect_type_ast_deps(a, out);
            }
        }
        ast::TypeKind::FunctionType(ft) => {
            for p in &ft.params {
                collect_type_ast_deps(p, out);
            }
            if let Some(r) = &ft.return_type {
                collect_type_ast_deps(r, out);
            }
        }
        ast::TypeKind::TupleType(tt) => {
            for e in &tt.elements {
                collect_type_ast_deps(e, out);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Pass 4 — call graph.
// ---------------------------------------------------------------------------

/// Returns SCCs in dependency order (leaves first). Each inner Vec holds
/// indices into `decls`.
fn build_call_graph_sccs(decls: &[Decl<'_>]) -> Vec<Vec<usize>> {
    let mut graph: StableGraph<(), (), Directed> = StableGraph::new();
    let mut node_of: HashMap<&str, NodeIndex> = HashMap::new();

    for d in decls {
        let n = graph.add_node(());
        node_of.insert(d.name(), n);
    }

    for d in decls {
        let from = node_of[d.name()];
        let mut walker = RefWalker {
            targets: &node_of,
            graph: &mut graph,
            from,
            shadowed: HashSet::new(),
            undo: Vec::new(),
        };
        match d {
            Decl::Fn { fd, body, .. } => {
                // Parameters shadow module names inside the body.
                for p in &fd.params {
                    walker.define(&p.identifier.name);
                }
                walker.expr(body);
            }
            Decl::Const { cb, .. } => {
                walker.expr(&cb.init);
            }
        }
    }

    // tarjan_scc returns components in reverse topological order: a component
    // appears after every component it has an edge to — i.e. callees come
    // first, which is exactly the order we want for inference.
    tarjan_scc(&graph)
        .into_iter()
        .map(|component| component.into_iter().map(|n| n.index()).collect::<Vec<_>>())
        .collect()
}

/// Walks an expression body collecting edges to other top-level declarations,
/// tracking a shadow set so locally-bound names don't count as references.
struct RefWalker<'a, 'g> {
    targets: &'a HashMap<&'a str, NodeIndex>,
    graph: &'g mut StableGraph<(), (), Directed>,
    from: NodeIndex,
    shadowed: HashSet<&'a str>,
    /// Undo log of names newly inserted into `shadowed`, in insertion order.
    /// `scoped` records the log length on entry and removes exactly the names
    /// pushed during the scope on exit, instead of deep-cloning the whole set
    /// per block / match-arm / lambda.
    undo: Vec<&'a str>,
}

impl<'a, 'g> RefWalker<'a, 'g> {
    fn define(&mut self, name: &'a str) {
        if self.shadowed.insert(name) {
            self.undo.push(name);
        }
    }

    fn referenced(&mut self, name: &str) {
        if self.shadowed.contains(name) {
            return;
        }
        if let Some(&to) = self.targets.get(name) {
            self.graph.add_edge(self.from, to, ());
        }
    }

    fn scoped<F: FnOnce(&mut Self)>(&mut self, f: F) {
        let mark = self.undo.len();
        f(self);
        // Names pushed by nested `scoped` calls are already drained and
        // removed by the time control returns here, so `drain(mark..)` holds
        // exactly the names newly shadowed at this scope level.
        for name in self.undo.drain(mark..) {
            self.shadowed.remove(name);
        }
    }

    fn expr(&mut self, e: &'a ast::Expression) {
        use ast::Expression as E;
        match e {
            E::Identifier(id) => self.referenced(&id.name),

            E::NumberLiteral(_) | E::StringLiteral(_) | E::ErrorNode(_) => {}

            E::BinaryLiteral(bl) => {
                for seg in &bl.segments {
                    self.expr(&seg.value);
                    if let Some(sz) = &seg.size {
                        self.expr(sz);
                    }
                }
            }

            E::InterpolatedString(is) => {
                for p in &is.parts {
                    if let ast::InterpPart::Expr(e) = p {
                        self.expr(e);
                    }
                }
            }

            E::ArrayExpression(a) => {
                for el in &a.elements {
                    match el {
                        ast::ArrayElement::Expression(e) => self.expr(e),
                        ast::ArrayElement::SpreadElement(s) => self.expr(&s.expression),
                    }
                }
            }

            E::TupleExpression(t) => {
                for el in &t.elements {
                    self.expr(el);
                }
            }

            E::ArrayIndexExpression(ai) => {
                self.expr(&ai.expression);
                self.expr(&ai.index);
            }

            E::RangeExpression(r) => {
                self.expr(&r.start);
                self.expr(&r.end);
            }

            E::BinaryExpression(b) => {
                self.expr(&b.left);
                self.expr(&b.right);
            }

            E::UnaryExpression(u) => self.expr(&u.expression),

            E::PropertyAccessExpression(p) => {
                // The RHS of `.` is a member, not a free identifier; only the
                // left side can reference a top-level name.
                self.expr(&p.left);
            }

            E::FunctionCallExpression(fc) => {
                self.expr(&fc.callee);
                for arg in &fc.arguments {
                    match arg {
                        ast::CallArg::Positional(e) => self.expr(e),
                        ast::CallArg::Labeled { value, .. } => self.expr(value),
                        ast::CallArg::Spread(e) => self.expr(e),
                    }
                }
            }

            E::IfExpression(ie) => {
                self.expr(&ie.condition);
                self.expr(&ie.body);
                self.expr(&ie.else_body);
            }

            E::OrExpression(oe) => {
                self.expr(&oe.expression);
                self.scoped(|w| {
                    if let Some(recv) = &oe.receiver {
                        w.define(&recv.name);
                    }
                    w.expr(&oe.body);
                });
            }

            E::MatchExpression(me) => {
                self.expr(&me.subject);
                for arm in &me.arms {
                    self.scoped(|w| {
                        w.pattern(&arm.pattern);
                        if let Some(g) = &arm.guard {
                            w.expr(g);
                        }
                        w.expr(&arm.body);
                    });
                }
            }

            E::FunctionExpression(fe) => {
                self.scoped(|w| {
                    for p in &fe.params {
                        w.define(&p.identifier.name);
                    }
                    w.expr(&fe.body);
                });
            }

            E::BlockExpression(be) => {
                self.scoped(|w| {
                    for node in &be.body {
                        w.node(node);
                    }
                });
            }
        }
    }

    fn node(&mut self, n: &'a ast::Node) {
        match n {
            ast::Node::Expression(e) => self.expr(e),
            ast::Node::Statement(s) => match s.as_ref() {
                ast::Statement::VariableBinding(vb) => {
                    self.expr(&vb.init);
                    self.define(&vb.identifier.name);
                }
                ast::Statement::TupleDestructuringBinding(tdb) => {
                    self.expr(&tdb.init);
                    for p in &tdb.patterns {
                        self.pattern(p);
                    }
                }
                ast::Statement::Declaration { decl, .. } => match decl.as_ref() {
                    ast::Declaration::Const(cb) => {
                        self.expr(&cb.init);
                        self.define(&cb.identifier.name);
                    }
                    ast::Declaration::Function(fd) => {
                        self.define(&fd.identifier.name);
                        self.scoped(|w| {
                            for p in &fd.params {
                                w.define(&p.identifier.name);
                            }
                            if let ast::FnBody::Block(body) = &fd.body {
                                w.expr(body);
                            }
                        });
                    }
                    ast::Declaration::Type(_) => {}
                },
                ast::Statement::TypedDiscard(td) => {
                    self.expr(&td.init);
                }
                ast::Statement::CtorDestructuringBinding(cdb) => {
                    self.expr(&cdb.init);
                    for arg in &cdb.args {
                        self.pattern(match arg {
                            ast::PatternArg::Positional(p) => p,
                            ast::PatternArg::Labeled { pattern, .. } => pattern,
                        });
                    }
                }
                ast::Statement::ImportDeclaration(_) => {}
            },
        }
    }

    fn pattern(&mut self, p: &'a ast::Pattern) {
        p.for_each_binder(ast::OrAlternatives::All, &mut |b| match b {
            ast::PatternBinder::Name(id) => self.define(&id.name),
            ast::PatternBinder::SizeExpr(sz) => self.expr(sz),
        });
    }
}
