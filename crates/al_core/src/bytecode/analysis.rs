//! Module top level: the multi-pass declaration analysis that fronts the
//! compiler's fused infer+emit pass.
//!
//! Top-level declarations are mutually recursive and order-free, so bodies
//! cannot be compiled in source order. `analyse_module` makes the order
//! explicit in passes, each establishing one fact before the next needs it:
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
//! One SCC's members are inferred at a single engine level against their
//! pre-registered signatures and generalized together, so mutual recursion
//! typechecks monomorphically inside the group and polymorphically outside.
//!
//! Pass 6 is a phase boundary, not a step of pass 5: all of pass 5 sits inside
//! one `begin_deferred_elaboration`/`end_deferred_elaboration` pair, so no
//! body is lowered while the module is still being typechecked. That is what
//! `lower(p: &TypedProgram) -> CoreProgram` needs — the whole module at once,
//! with every var already resolved.
//!
//! # Invariant: per-decl data is positional, never name-keyed
//!
//! Data computed in an early pass and read in a later one (`Prepared`,
//! `PreparedType`) rides in `Vec`s built one-to-one with the decl lists.
//! Keying by name desyncs the moment two decls share a name: the decl list
//! keeps both, a map keeps one.

use std::collections::{HashMap, HashSet};

use petgraph::Directed;
use petgraph::algo::tarjan_scc;
use petgraph::stable_graph::{NodeIndex, StableGraph};

use super::Op;
use super::compiler::{Compiler, ToplevelDecl};
use crate::ast;
use crate::module::{self, ExportedValue, ModuleInterface};
use crate::reference::{DefId, DefinitionKind};
use crate::span::Span;
use crate::type_def::TypeId;
use crate::typed_ir::GlobalSlot;
use crate::types::{
    AddedTypeVar, AnnotationContext, ArenaSlice, DefinitionLocation, EntityKind, Hydrator, Scheme,
    StrId, Ty, TypeBody, TypeInfo, TypeParam, ValueKind, Variant, VariantField, pool,
};

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

    /// The declaration's position in the module block's `body`. This, not the
    /// name, is what the toplevel elaboration schedules on.
    fn node(&self) -> usize {
        match *self {
            Decl::Fn { node, .. } | Decl::Const { node, .. } => node,
        }
    }
}

/// Per-declaration data computed in Pass 3 and read in Pass 5, positionally
/// (see the module invariant).
enum Prepared<'a> {
    Fn {
        fd: &'a ast::FunctionDeclaration,
        is_pub: bool,
        slot: GlobalSlot,
        /// The declaration's identity, built once and reused by Pass 5's
        /// `emit_def`/`defid_of` and by the post-generalisation `scheme.def`,
        /// so definition and scheme cannot land on different locations.
        dl: DefinitionLocation,
        body: &'a ast::Expression,
        param_tys: Vec<Ty>,
        ret_ty: Ty,
        hydrator: Hydrator,
    },
    Const {
        cb: &'a ast::ConstBinding,
        is_pub: bool,
        slot: GlobalSlot,
        /// See `Prepared::Fn::dl`.
        dl: DefinitionLocation,
    },
}

impl<'a> Prepared<'a> {
    /// A function's parameter names in declaration order; empty otherwise.
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
    fn is_pub(&self) -> bool {
        match self {
            Prepared::Fn { is_pub, .. } | Prepared::Const { is_pub, .. } => *is_pub,
        }
    }
    fn slot(&self) -> GlobalSlot {
        match self {
            Prepared::Fn { slot, .. } | Prepared::Const { slot, .. } => *slot,
        }
    }
    fn dl(&self) -> DefinitionLocation {
        match self {
            Prepared::Fn { dl, .. } | Prepared::Const { dl, .. } => *dl,
        }
    }
    fn is_const(&self) -> bool {
        matches!(self, Prepared::Const { .. })
    }
}

/// Per-type data computed in Pass 1 and read in Pass 3.5, positionally (see
/// the module invariant).
///
/// Aliases have no head registered in Pass 1 (Pass 2 owns them), so they get
/// the `Alias` variant rather than a sentinel-filled `Head` a consumer could
/// mistake for real data.
enum PreparedType {
    Head {
        type_id: TypeId,
        name_id: StrId,
        hydrator: Hydrator,
        param_tys: Vec<Ty>,
        /// The type head's `TypeParam` slice; Pass 3.5 needs it to
        /// `close_body` each stored field template.
        type_params: ArenaSlice<pool::TypeParams>,
    },
    /// `type X = ...`, registered in Pass 2. `None` means registration was
    /// skipped (the alias is in a cycle) and there is nothing to export.
    Alias { type_id: Option<TypeId> },
}

/// Hydrated shape of one `fn` declaration's signature.
struct FnSig {
    hydrator: Hydrator,
    param_tys: Vec<Ty>,
    ret_ty: Ty,
    fn_ty: Ty,
}

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
    /// schemes into whatever scope is current on entry and does not pop it, so
    /// the entry-file caller keeps them visible across the later `lower_body`
    /// on `__main__`. Import callers bracket the call with their own
    /// `push_scope`/`pop_scope`.
    pub(super) fn analyse_module(
        &mut self,
        block: &ast::BlockExpression,
        mut iface: Option<&mut ModuleInterface>,
    ) {
        self.push_local_scope();

        // Pass 0 — partition + reserved/duplicate name checks.
        let mut type_decls: Vec<(&ast::TypeDeclaration, bool)> = Vec::new();
        // Positional with `type_decls`: `true` marks a duplicate-named ctor,
        // which Pass 3.5 drops as duplicate fns/consts are dropped here.
        let mut dup_ctors: Vec<Vec<bool>> = Vec::new();
        let mut decls: Vec<Decl<'_>> = Vec::new();
        let mut vm_fns: Vec<(&ast::FunctionDeclaration, bool, Op)> = Vec::new();
        let mut other_nodes: Vec<&ast::Node> = Vec::new();

        let in_prelude = self.current_module == module::al_prelude();
        let mut seen_types: HashMap<&str, Span> = HashMap::new();
        let mut seen_values: HashMap<&str, Span> = HashMap::new();

        let in_stdlib = module::is_stdlib(&self.current_module);

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
                                let mut dups: Vec<bool> = Vec::new();
                                if let ast::TypeBody::Variants { ctors, .. } = &td.body {
                                    for ctor in ctors {
                                        check_reserved(self, &ctor.identifier, in_prelude);
                                        dups.push(check_duplicate(
                                            self,
                                            &mut seen_values,
                                            &ctor.identifier,
                                        ));
                                    }
                                }
                                if !dup {
                                    type_decls.push((td, is_public));
                                    dup_ctors.push(dups);
                                }
                            }
                            ast::Declaration::Function(fd) => {
                                self.validate_attributes(&fd.attributes, in_stdlib, AttrTarget::Fn);
                                if !check_duplicate(self, &mut seen_values, &fd.identifier) {
                                    // `@vm` fns carry no AL body: siphon them
                                    // off so every surviving `Decl::Fn` holds
                                    // a real expression body.
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

        // Pass 1 — register custom-type heads and opaque types. Aliases wait
        // for Pass 2 because their RHS can reference these heads.
        let mut prepared_types: Vec<PreparedType> = Vec::with_capacity(type_decls.len());
        for (td, is_public) in &type_decls {
            if matches!(td.body, ast::TypeBody::Alias(_)) {
                prepared_types.push(PreparedType::Alias { type_id: None });
                continue;
            }
            let (h, type_params, param_tys) = self.hydrate_type_params(&td.type_params);
            let name_id = self.engine.intern(&td.identifier.name);
            let module = self.current_module_slice();
            let type_id =
                self.env
                    .register_type_head(&td.identifier.name, name_id, module, type_params);
            self.store_and_emit_def(&td.identifier, &td.doc, DefinitionKind::Type, *is_public);
            if matches!(td.body, ast::TypeBody::External) {
                self.env.set_type_body(type_id, TypeBody::External);
            }
            prepared_types.push(PreparedType::Head {
                type_id,
                name_id,
                hydrator: h,
                param_tys,
                type_params,
            });
        }

        // Pass 2 — toposort + register aliases.
        self.register_aliases(&type_decls, &mut prepared_types);

        // Pass 3 — pre-allocate one slot per decl, then register fn
        // signatures, positionally in `prepared`.
        //
        // `@vm` fns get a `Builtin{op}` scheme, no slot, no body codegen, and
        // export here rather than after generalisation: their type is the
        // annotated signature verbatim, there is no body to infer from.
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
            let params: Vec<String> = fd
                .params
                .iter()
                .map(|p| p.identifier.name.clone())
                .collect();
            self.emit_def(
                dl,
                name,
                fd.doc.clone(),
                is_pub,
                DefinitionKind::Function {
                    param_names: params.clone(),
                },
            );
            self.record(name, scheme.ty, fd.identifier.span, fd.doc.clone());
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

        // Not `get_or_create_local`: a decl's slot reaches the toplevel
        // elaboration through `toplevel_decls`, not through the module-scope
        // bind queue. Only `let`/destructuring binds, which have no `Decl` to
        // hang a slot on, go through that queue.
        let mut prepared: Vec<Prepared> = Vec::with_capacity(decls.len());
        for d in &decls {
            let slot = self.alloc_decl_slot(d.name());
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
                        dl,
                        body,
                        param_tys,
                        ret_ty,
                        hydrator,
                    }
                }
                Decl::Const { cb, is_pub, .. } => {
                    let m = self.current_module_slice();
                    let dl = DefinitionLocation::new(cb.identifier.span, m, EntityKind::Constant);
                    Prepared::Const {
                        cb,
                        is_pub,
                        slot,
                        dl,
                    }
                }
            };
            prepared.push(p);
        }

        // Pass 3.5 — hydrate constructor bodies; define ctor values.
        for (((td, is_public), pt), dups) in type_decls.iter().zip(prepared_types).zip(&dup_ctors) {
            self.register_constructors(td, *is_public, pt, dups, &mut iface);
        }

        // Pass 4 — build call graph over fns+consts, Tarjan SCC.
        let sccs = build_call_graph_sccs(&decls);

        // Pass 5 — infer each SCC, then generalize. SCC members index into
        // `prepared`, so per-decl data is reached positionally.
        //
        // THE PHASE BOUNDARY. Everything up to `end_deferred_elaboration` is
        // the typecheck walk: it infers types, reserves `Function` slots and
        // records capture shapes, and emits no body bytecode. Every body it
        // walks is parked in `deferred_bodies`; the Core pipeline drains them
        // in one loop afterwards, when every var any SCC would solve is solved.
        //
        // A parked body's scope snapshot is not enough on its own:
        // `resolve_name` re-reads `self.env` at drain time, and a toplevel
        // `let` overwrites a same-scope binding in place. `pin_deferred_env`
        // below freezes the env the decl walk saw.
        self.begin_deferred_elaboration();
        // Filled positionally as each SCC generalizes. Tarjan covers every
        // node, so every entry is `Some` by the export loop below.
        let mut final_schemes: Vec<Option<Scheme>> = vec![None; prepared.len()];
        for scc in &sccs {
            self.engine.enter_level();
            let mut inferred: Vec<(usize, Ty)> = Vec::with_capacity(scc.len());

            for &idx in scc {
                // Publish this decl to the toplevel elaboration. Pushed in
                // SCC-visit order, so the elaborated spine initialises
                // dependencies first and a forward-referenced `const` is
                // stored before it is read.
                let name_id = self.engine.intern(prepared[idx].name());
                self.toplevel_decls.push(ToplevelDecl {
                    node: decls[idx].node(),
                    name: name_id,
                    slot: prepared[idx].slot(),
                });
                match &prepared[idx] {
                    Prepared::Fn {
                        fd,
                        is_pub,
                        slot,
                        dl,
                        body,
                        param_tys,
                        ret_ty,
                        hydrator,
                    } => {
                        let name = &fd.identifier.name;
                        let dl = *dl;
                        let names = fd
                            .params
                            .iter()
                            .map(|p| p.identifier.name.clone())
                            .collect();
                        self.emit_def(
                            dl,
                            name,
                            fd.doc.clone(),
                            *is_pub,
                            DefinitionKind::Function { param_names: names },
                        );
                        // Attribute references emitted inside this body to the
                        // fn's own `DefId` so the dead-code walk follows
                        // def→def edges. Spans nested lambdas too: a lambda has
                        // no `DefId` of its own.
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
                        // Nothing to record for the toplevel spine:
                        // `compile_declared_function` keyed this body under the
                        // slot the spine stores it into, and a top-level fn
                        // captures nothing (sibling refs are `PushGlobal`).
                        inferred.push((idx, fn_ty));
                    }
                    Prepared::Const { cb, is_pub, dl, .. } => {
                        let name = &cb.identifier.name;
                        self.emit_def(*dl, name, cb.doc.clone(), *is_pub, DefinitionKind::Constant);
                        let owner = self.owner_defid(cb.identifier.span, EntityKind::Constant);
                        let final_ty = self.with_owner(owner, |c| {
                            let mut h = Hydrator::new(AnnotationContext::Signature);
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

            for (idx, ty) in inferred {
                let mut scheme = self.engine.generalize_top(ty);
                let p = &prepared[idx];
                scheme.def = Some(p.dl());
                if p.is_const() {
                    scheme.kind = ValueKind::Local;
                }
                self.env.define(p.name(), scheme);
                final_schemes[idx] = Some(scheme);
            }
        }

        // Export now that schemes are final. Schemes come from
        // `final_schemes`, never back out of the env by name (which desyncs
        // under shadowing). Iterated in source order, not SCC order:
        // `ModuleInterface.values` is an `IndexMap`, and its insertion order
        // is what importers observe.
        for (p, s) in prepared.iter().zip(final_schemes) {
            #[allow(clippy::panic)] // the loop above generalizes every prepared decl
            let s = s.unwrap_or_else(|| panic!("decl '{}' was never generalized", p.name()));
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

        // Other nodes (let bindings, bare expressions) — linear walk.
        //
        // The parked decl bodies have not lowered yet, and a `let` here may
        // shadow a name one of them resolved (`println = 5` after a decl that
        // calls `println`). Freeze the env they were walked against.
        if !other_nodes.is_empty() {
            self.pin_deferred_env();
        }
        // The only walk allowed to fill the positional `toplevel_binds` queue;
        // the toplevel elaboration drains it against exactly these nodes, in
        // this order. Nested walks re-enter `compile_node` from here but push
        // `outer_scopes`/`scope_marks`, which `bind_local` also checks.
        let outer_walk = std::mem::replace(&mut self.walking_module_statements, true);
        for node in &other_nodes {
            let _ty = self.compile_node(node);
        }
        self.walking_module_statements = outer_walk;

        // End of the walk; start of the Core pipeline. Types are final: what
        // no SCC solved is now `Generic`, which only `zonk` cares about
        // (`Generic` is quantifiable, `Unbound` is an error).
        self.end_deferred_elaboration();

        self.pop_local_scope();
    }

    fn hydrate_type_params(
        &mut self,
        params: &[ast::Identifier],
    ) -> (Hydrator, ArenaSlice<pool::TypeParams>, Vec<Ty>) {
        let mut h = Hydrator::new(AnnotationContext::TypeDefinition);
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

    /// Intern the current module path as a `str_slices` slice. Memoised so a
    /// module's path occupies one pool entry however many types it declares.
    pub(super) fn current_module_slice(&mut self) -> ArenaSlice<pool::StrSlices> {
        if let Some(sl) = self.module_path_slice {
            return sl;
        }
        let segs = self.current_module.clone();
        let sl = self.engine.intern_slice(&segs);
        self.module_path_slice = Some(sl);
        sl
    }

    /// Record a named declaration: definition location and doc into the env,
    /// plus the matching graph definition. The entity comes from `kind`, so
    /// record and payload cannot disagree.
    fn store_and_emit_def(
        &mut self,
        id: &ast::Identifier,
        doc: &Option<String>,
        kind: DefinitionKind,
        is_pub: bool,
    ) {
        let dl = DefinitionLocation::new(id.span, self.current_module_slice(), kind.entity());
        self.env.store_definition(&id.name, dl);
        self.env.store_doc_opt(&id.name, doc);
        self.emit_def(dl, &id.name, doc.clone(), is_pub, kind);
    }

    /// Run `f` with `current_owner` set to `owner` so references emitted
    /// inside `f` are attributed to it.
    fn with_owner<R>(&mut self, owner: DefId, f: impl FnOnce(&mut Self) -> R) -> R {
        let prev = self.current_owner;
        self.current_owner = Some(owner);
        let r = f(self);
        self.current_owner = prev;
        r
    }

    fn hydrate_fn_signature(&mut self, fd: &ast::FunctionDeclaration) -> FnSig {
        let mut hydrator = Hydrator::new(AnnotationContext::Signature);
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

    /// Registers each acyclic alias and records its `TypeId` back into the
    /// matching `prepared_types` slot, so Pass 3.5's export reads the registry
    /// by id instead of by name.
    fn register_aliases(
        &mut self,
        type_decls: &[(&ast::TypeDeclaration, bool)],
        prepared_types: &mut [PreparedType],
    ) {
        type AliasEntry<'a> = (
            usize,
            &'a ast::TypeDeclaration,
            &'a ast::TypeIdentifier,
            bool,
        );
        let aliases: Vec<AliasEntry<'_>> = type_decls
            .iter()
            .enumerate()
            .filter_map(|(decl_idx, (td, p))| match &td.body {
                ast::TypeBody::Alias(rhs) => Some((decl_idx, *td, rhs, *p)),
                _ => None,
            })
            .collect();
        if aliases.is_empty() {
            return;
        }

        let mut graph: StableGraph<(), (), Directed> = StableGraph::new();
        let mut idx_of: HashMap<&str, NodeIndex> = HashMap::new();
        for &(_, a, _, _) in &aliases {
            let n = graph.add_node(());
            idx_of.insert(a.identifier.name.as_str(), n);
        }
        for &(_, a, rhs, _) in &aliases {
            let from = idx_of[a.identifier.name.as_str()];
            let mut deps: Vec<&str> = Vec::new();
            collect_type_ast_deps(rhs, &mut deps);
            for dep in &deps {
                if let Some(&to) = idx_of.get(dep) {
                    graph.add_edge(from, to, ());
                }
            }
        }

        let by_idx: HashMap<NodeIndex, AliasEntry<'_>> = aliases
            .iter()
            .map(|a| (idx_of[a.1.identifier.name.as_str()], *a))
            .collect();

        // tarjan_scc yields dependees before dependers. A component with more
        // than one node, or one node with a self-edge, is a cycle: report each
        // member and skip it so one bad alias does not block the rest.
        for component in tarjan_scc(&graph) {
            let cyclic =
                component.len() > 1 || graph.find_edge(component[0], component[0]).is_some();
            if cyclic {
                for node in &component {
                    if let Some(&(_, td, _, _)) = by_idx.get(node) {
                        self.error(
                            format!("Recursive type alias '{}'", td.identifier.name),
                            td.identifier.span,
                        );
                    }
                }
                continue;
            }
            let Some(&(decl_idx, td, rhs, is_pub)) = by_idx.get(&component[0]) else {
                continue;
            };
            let (mut h, type_params, _) = self.hydrate_type_params(&td.type_params);
            let name_id = self.engine.intern(&td.identifier.name);
            let module = self.current_module_slice();
            let type_id =
                self.env
                    .register_type_head(&td.identifier.name, name_id, module, type_params);
            let owner = self.owner_defid(td.identifier.span, EntityKind::Type);
            let target = self.with_owner(owner, |c| c.hydrate(&mut h, rhs));
            // Store the target CLOSED (`Var(param_id)` → `Bound(idx)`): the
            // body outlives incremental rewinds, which clear the engine's
            // `vars` table, and the exported interface copies this `TypeInfo`
            // by value before Pass 3.5's `export_type`.
            let target = self.engine.close_body(target, type_params);
            self.env.set_type_body(type_id, TypeBody::Alias { target });
            prepared_types[decl_idx] = PreparedType::Alias {
                type_id: Some(type_id),
            };
            self.store_and_emit_def(&td.identifier, &td.doc, DefinitionKind::Type, is_pub);
        }
    }

    /// `dup_ctors` is positional with `ctors`: a `true` entry marks a
    /// duplicate-named constructor, dropped here rather than added to
    /// `variants` or defined as a value.
    fn register_constructors(
        &mut self,
        td: &ast::TypeDeclaration,
        is_public: bool,
        pt: PreparedType,
        dup_ctors: &[bool],
        iface: &mut Option<&mut ModuleInterface>,
    ) {
        // The id the export below reads the registry with. Positional, never
        // the name: a same-named registration analysed later shadows the
        // by-name map but cannot hijack the by-id entry.
        let export_id = match &pt {
            PreparedType::Head { type_id, .. } => Some(*type_id),
            PreparedType::Alias { type_id } => *type_id,
        };
        let (
            ast::TypeBody::Variants { ctors, opaque },
            PreparedType::Head {
                type_id,
                name_id: type_name_id,
                hydrator: mut h,
                param_tys: param_generics,
                type_params,
            },
        ) = (&td.body, pt)
        else {
            // Aliases and externals have no constructors; just export the type info.
            let ti = export_id.and_then(|id| self.env.lookup_type_info_by_id(id));
            export_type(iface.as_deref_mut(), &td.identifier.name, is_public, ti);
            return;
        };
        let ctors_public = is_public && !*opaque;

        let type_name = &td.identifier.name;
        let m = self.current_module_slice();
        let mut variants: Vec<Variant> = Vec::with_capacity(ctors.len());

        // Constructor/field hydration is the type's body: types it names are
        // references owned by this type, giving the dead-code walk type→type
        // edges.
        let owner = self.owner_defid(td.identifier.span, EntityKind::Type);
        self.with_owner(owner, |c| {
            for (ctor, _) in ctors.iter().zip(dup_ctors).filter(|&(_, dup)| !dup) {
                // Index into the surviving variants, not the source ctor list:
                // dropped duplicates must not leave holes in the
                // `variant_idx` ↔ `variants` correspondence the VM tags by.
                let variant_idx = variants.len() as u16;
                let mut field_itys: Vec<Ty> = Vec::with_capacity(ctor.fields.len());
                let mut field_defs: Vec<VariantField> = Vec::with_capacity(ctor.fields.len());
                let mut label_ids: Vec<StrId> = Vec::with_capacity(ctor.fields.len());

                for f in &ctor.fields {
                    let ity = c.hydrate(&mut h, &f.typ);
                    let label = c.engine.intern(&f.label.name);
                    field_itys.push(ity);
                    // Stored CLOSED (`Var(param_id)` → `Bound(idx)`) so it
                    // never references the engine's transient `vars` table,
                    // which incremental rewinds clear. The ctor scheme below
                    // still generalizes over the open `ity`.
                    field_defs.push(VariantField {
                        label,
                        ty: c.engine.close_body(ity, type_params),
                    });
                    label_ids.push(label);

                    let qualified = format!("{}.{}", ctor.identifier.name, f.label.name);
                    let field_dl = DefinitionLocation::new(f.label.span, m, EntityKind::Field);
                    c.env.store_definition(&qualified, field_dl);
                    c.emit_def(
                        field_dl,
                        &f.label.name,
                        None,
                        ctors_public,
                        DefinitionKind::Field,
                    );
                }

                let fields = c.engine.push_variant_fields(&field_defs);
                variants.push(Variant {
                    name: c.engine.intern(&ctor.identifier.name),
                    fields,
                });
                let field_labels = c.engine.push_str_ids(&label_ids);

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
                    variant_idx,
                    arity: ctor.fields.len() as u16,
                    field_labels,
                };
                // One identity for the ctor: `scheme.def` and the graph
                // `DefId` below both read it, so goto-def and find-references
                // cannot disagree on where it lives.
                let ctor_dl =
                    DefinitionLocation::new(ctor.identifier.span, m, EntityKind::Constructor);
                scheme.def = Some(ctor_dl);

                let name = &ctor.identifier.name;
                c.env.define(name, scheme);
                // Mirror the field labels into the graph and the export so
                // hover can render `NotFound fn(path String) IoError` without
                // reaching back into the engine's string pool.
                let labels = c.engine.strs_of(field_labels);
                // `Config(name: 'x')` names the constructor, never the type,
                // so without the `ctor_of` edge every single-constructor type
                // reads as unused.
                let type_dl = DefinitionLocation::new(td.identifier.span, m, EntityKind::Type);
                let type_defid = c.defid_of(type_dl);
                c.store_and_emit_def(
                    &ctor.identifier,
                    &ctor.doc,
                    DefinitionKind::Constructor {
                        ctor_of: Some(type_defid),
                        param_names: labels.clone(),
                    },
                    ctors_public,
                );
                // Unconditional: `export_value` routes a non-`pub` name into
                // `iface.private_names`, so a private ctor gives importers
                // "'X' is private" rather than "no member 'X'".
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
        });

        let variants = self.engine.push_variants(&variants);
        self.env
            .set_type_body(type_id, TypeBody::Custom { variants });

        let ti = self.env.lookup_type_info_by_id(type_id);
        export_type(iface.as_deref_mut(), type_name, is_public, ti);
    }
}

#[derive(Clone, Copy)]
enum AttrTarget {
    Fn,
    Type,
}

impl Compiler {
    /// Reject unknown attributes, attributes on the wrong target, and any
    /// attribute outside the embedded stdlib. Arity is not checked here: the
    /// parser already rejects a `@vm` fn with anything but one arg.
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
                }
                other => {
                    self.error(format!("Unknown attribute '@{other}'"), a.span);
                }
            }
        }
    }
}

/// Record a name in the module interface: into `values`/`types` when public,
/// into `private_names` otherwise so importers get a "this is private" error.
fn export_value(
    iface: Option<&mut ModuleInterface>,
    name: &str,
    is_pub: bool,
    scheme: Scheme,
    slot: Option<GlobalSlot>,
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

/// Records a top-level name, erroring if it was already defined. Returns
/// `true` for a duplicate so the caller drops the redundant declaration.
/// Dropping is load-bearing: the call graph and the alias toposort key their
/// nodes by name, so a second decl would alias the first's node and desync
/// SCC scheduling.
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

/// Returns SCCs in dependency order (leaves first). Each inner Vec holds
/// indices into `decls`.
fn build_call_graph_sccs(decls: &[Decl<'_>]) -> Vec<Vec<usize>> {
    let mut graph: StableGraph<(), (), Directed> = StableGraph::new();
    let mut node_of: HashMap<&str, NodeIndex> = HashMap::new();

    for d in decls {
        let n = graph.add_node(());
        // Pass 0 dropped same-named decls, so names are unique here; a
        // collision would alias two decls onto one node.
        let prev = node_of.insert(d.name(), n);
        debug_assert!(
            prev.is_none(),
            "duplicate decl '{}' reached the call graph",
            d.name()
        );
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

    // Reverse topological order: callees come first, which is the order
    // inference needs.
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
    /// Undo log of names newly inserted into `shadowed`. `scoped` truncates it
    /// on exit instead of deep-cloning the whole set per block/arm/lambda.
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
        // Nested `scoped` calls already drained their own names, so this holds
        // exactly what was shadowed at this level.
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
                    if let Some(sz) = seg.spec.size_expr() {
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
                // The RHS of `.` is a member, not a free identifier.
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
