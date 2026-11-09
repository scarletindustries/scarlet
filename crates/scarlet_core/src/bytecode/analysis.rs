//! A module's top level. This is the declaration analysis, in several passes,
//! that runs before the compiler's fused infer-and-emit pass.
//!
//! Top-level declarations can refer to each other in any order, so their
//! bodies can't be compiled in source order. `analyse_module` sets the order
//! out in passes, and each one settles one fact before the next needs it:
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
//! | 5    | infer each SCC, callees first; generalise per SCC; then walk  |
//! |      | the non-declaration nodes                                     |
//! | 6    | elaborate every parked body and lower it to Core IR           |
//!
//! The members of one SCC are inferred at a single engine level against the
//! signatures registered for them earlier, and generalised together. So mutual
//! recursion typechecks monomorphically inside the group and polymorphically
//! outside it.
//!
//! Pass 6 is a phase boundary, not a step of pass 5. All of pass 5 sits inside
//! one `begin_deferred_elaboration` and `end_deferred_elaboration` pair, so no
//! body is lowered while the module is still being typechecked. That is what
//! `lower(p: &TypedProgram) -> CoreProgram` needs: the whole module at once,
//! with every variable already resolved.
//!
//! # Invariant: data for each declaration goes by position, never by name
//!
//! Data worked out in an early pass and read in a later one (`Prepared` and
//! `PreparedType`) is kept in `Vec`s built one-to-one with the declaration
//! lists. Keying by name breaks as soon as two declarations share a name,
//! because the declaration list keeps both and a map keeps one.

use std::collections::{HashMap, HashSet};

use petgraph::Directed;
use petgraph::algo::tarjan_scc;
use petgraph::stable_graph::{NodeIndex, StableGraph};

use super::compiler::{Compiler, DeclSite, ModuleChecker, ToplevelDecl};
use crate::ast;
use crate::module::{self, ExportedType, ExportedValue, ModuleInterface};
use crate::reference::{DefId, DefinitionKind};
use crate::span::Span;
use crate::type_def::TypeId;
use crate::typed_ir::GlobalSlot;
use crate::typed_ir::checked::{BinderId, CNode};
use crate::types::{
    AddedTypeVar, AnnotationContext, ArenaSlice, DefinitionLocation, EntityKind, Hydrator, Scheme,
    StrId, Ty, TypeBody, TypeInfo, TypeParam, ValueKind, Variant, VariantField, pool,
};
use scarlet_types::intrinsic::Intrinsic;

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

    /// The declaration's position in the module block's `body`. The toplevel
    /// elaboration schedules on this, not on the name.
    fn node(&self) -> usize {
        match *self {
            Decl::Fn { node, .. } | Decl::Const { node, .. } => node,
        }
    }
}

/// Data for each declaration, worked out in Pass 3 and read in Pass 5 by
/// position (see the module invariant).
enum Prepared<'a> {
    Fn {
        fd: &'a ast::FunctionDeclaration,
        is_pub: bool,
        slot: GlobalSlot,
        binder: BinderId,
        /// The declaration's identity. It is built once and reused by
        /// `emit_def` and `defid_of` in Pass 5 and by `scheme.def` after
        /// generalisation, so the definition and the scheme can't end up at
        /// different locations.
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
        binder: BinderId,
        /// See `Prepared::Fn::dl`.
        dl: DefinitionLocation,
    },
}

impl<'a> Prepared<'a> {
    /// Where the declaration lives.
    fn site(&self) -> DeclSite {
        match *self {
            Prepared::Fn { slot, binder, .. } | Prepared::Const { slot, binder, .. } => {
                DeclSite { slot, binder }
            }
        }
    }

    /// A function's parameter names in declaration order, or empty for anything
    /// else. They are only documentation: see `ExportedValue::param_names`.
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
    fn binder(&self) -> BinderId {
        match self {
            Prepared::Fn { binder, .. } | Prepared::Const { binder, .. } => *binder,
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

/// Data for each type, worked out in Pass 1 and read in Pass 3.5 by position
/// (see the module invariant).
///
/// Aliases have no head registered in Pass 1 (Pass 2 handles them), so they get
/// the `Alias` variant instead of a `Head` filled with placeholders that
/// something could mistake for real data.
enum PreparedType {
    Head {
        type_id: TypeId,
        name_id: StrId,
        hydrator: Hydrator,
        param_tys: Vec<Ty>,
        /// The type head's `TypeParam` slice, which Pass 3.5 needs to
        /// `close_body` each stored field template.
        type_params: ArenaSlice<pool::TypeParams>,
    },
    /// `type X = ...`, registered in Pass 2. `None` means it wasn't registered
    /// (the alias is in a cycle) and there's nothing to export.
    Alias { type_id: Option<TypeId> },
}

/// The hydrated shape of one `fn` declaration's signature.
struct FnSig {
    hydrator: Hydrator,
    param_tys: Vec<Ty>,
    ret_ty: Ty,
    fn_ty: Ty,
}

impl ModuleChecker<'_> {
    /// Keep a node that isn't a declaration, reporting an error first unless
    /// this is a script (the REPL's entry, see [`super::ModuleScope`]). The
    /// node is kept even when rejected, so the rest of the file is still
    /// analysed and reported.
    fn push_non_decl<'a>(
        &mut self,
        idx: usize,
        node: &'a ast::Node,
        in_module: bool,
        others: &mut Vec<(usize, &'a ast::Node)>,
    ) {
        if in_module {
            self.c.error(
                "Modules may only contain declarations at the top level".to_string(),
                node.span(),
            );
        } else if self.c.module_scope == super::ModuleScope::DeclarationsOnly {
            self.c.error(
                "Statements aren't allowed at module scope. Put a program's code inside `pub fn main()`"
                    .to_string(),
                node.span(),
            );
        }
        others.push((idx, node));
    }

    /// Analyse a module body. When `iface` is `Some`, the module is being
    /// compiled as an import and its `pub` items are recorded in the interface.
    /// When it is `None`, this is the entry module.
    ///
    /// Returns what the walk built for each of the block's nodes, at that
    /// node's index in the block.
    pub(super) fn analyse_module(
        &mut self,
        block: &ast::BlockExpression,
        mut iface: Option<&mut ModuleInterface>,
    ) -> Vec<CNode> {
        // The module toplevel is a body of its own, so its binders count from
        // zero.
        self.open_block();

        // Pass 0 sorts the nodes and checks for reserved and duplicate names.
        let mut type_decls: Vec<(&ast::TypeDeclaration, bool)> = Vec::new();
        // This lines up with `type_decls` by position. `true` marks a
        // constructor with a duplicate name, which Pass 3.5 drops, just as
        // duplicate functions and constants are dropped here.
        let mut dup_ctors: Vec<Vec<bool>> = Vec::new();
        let mut decls: Vec<Decl<'_>> = Vec::new();
        let mut vm_fns: Vec<(&ast::FunctionDeclaration, bool, Intrinsic)> = Vec::new();
        let mut other_nodes: Vec<(usize, &ast::Node)> = Vec::new();
        let mut nodes: Vec<CNode> = (0..block.body.len()).map(|_| CNode::Nothing).collect();

        let in_prelude = self.current_module == module::scarlet_prelude();
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
                                let dup = check_duplicate(self.c, &mut seen_types, &td.identifier);
                                check_reserved(self.c, &td.identifier, in_prelude);
                                let mut dups: Vec<bool> = Vec::new();
                                if let ast::TypeBody::Variants { ctors, .. } = &td.body {
                                    for ctor in ctors {
                                        check_reserved(self.c, &ctor.identifier, in_prelude);
                                        dups.push(check_duplicate(
                                            self.c,
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
                                if !check_duplicate(self.c, &mut seen_values, &fd.identifier) {
                                    // `@vm` functions have no Scarlet body, so
                                    // take them out here and every `Decl::Fn`
                                    // left has a real expression body.
                                    match &fd.body {
                                        ast::FnBody::Vm(key) => {
                                            match Intrinsic::from_key(&key.name) {
                                                Some(intrinsic) => {
                                                    vm_fns.push((fd, is_public, intrinsic))
                                                }
                                                None => self.c.error(
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
                                if !check_duplicate(self.c, &mut seen_values, &cb.identifier) {
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
                        // `load_imports` already compiled this and
                        // `bind_imports` bound it.
                    }
                    _ => self.push_non_decl(node_idx, node, iface.is_some(), &mut other_nodes),
                },
                ast::Node::Expression(_) => {
                    self.push_non_decl(node_idx, node, iface.is_some(), &mut other_nodes)
                }
            }
        }

        // Pass 1 registers the heads of custom types and opaque types. Aliases
        // wait for Pass 2, because their right-hand side can refer to these
        // heads.
        let mut prepared_types: Vec<PreparedType> = Vec::with_capacity(type_decls.len());
        for (td, is_public) in &type_decls {
            if matches!(td.body, ast::TypeBody::Alias(_)) {
                prepared_types.push(PreparedType::Alias { type_id: None });
                continue;
            }
            let (h, type_params, param_tys) = self.hydrate_type_params(&td.type_params);
            let name_id = self.c.engine.intern(&td.identifier.name);
            let module = self.current_module_slice();
            let type_id = self.declare_type(&td.identifier.name, name_id, module, type_params);
            self.store_and_emit_def(&td.identifier, &td.doc, DefinitionKind::Type, *is_public);
            if matches!(td.body, ast::TypeBody::External) {
                self.c.types.set_type_body(type_id, TypeBody::External);
            }
            prepared_types.push(PreparedType::Head {
                type_id,
                name_id,
                hydrator: h,
                param_tys,
                type_params,
            });
        }

        // Pass 2 sorts the aliases by dependency and registers them.
        self.register_aliases(&type_decls, &mut prepared_types);

        // Pass 3 allocates one slot per declaration up front, then registers
        // function signatures, by position in `prepared`.
        //
        // `@vm` functions get a `Builtin` scheme, no slot and no body codegen,
        // and are exported here instead of after generalisation. Their type is
        // exactly the annotated signature, because there's no body to infer it
        // from.
        for &(fd, is_pub, intrinsic) in &vm_fns {
            let name = &fd.identifier.name;
            let fn_ty = self.hydrate_fn_signature(fd).fn_ty;
            let params: Vec<String> = fd
                .params
                .iter()
                .map(|p| p.identifier.name.clone())
                .collect();
            let mut scheme = self.c.engine.generalize_top(fn_ty);
            scheme.kind = ValueKind::Builtin {
                intrinsic,
                param_labels: self.c.engine.intern_slice(&params),
            };
            let m = self.current_module_slice();
            let dl = DefinitionLocation::new(fd.identifier.span, m, EntityKind::Function);
            scheme.def = Some(dl);
            self.declare(name, scheme, None);
            self.emit_def(
                dl,
                name,
                fd.doc.clone(),
                is_pub,
                DefinitionKind::Function {
                    param_names: params.clone(),
                },
            );
            self.record(name, scheme.ty, fd.identifier.span);
            export_value(iface.as_deref_mut(), name, is_pub, scheme, None);
        }

        // Don't use `bind_local` here. A declaration's slot reaches the
        // toplevel elaboration through `toplevel_decls`, not through the
        // module-scope binding queue. Only plain and destructuring bindings,
        // which have no `Decl` to hang a slot on, go through that queue.
        let mut prepared: Vec<Prepared> = Vec::with_capacity(decls.len());
        for d in &decls {
            let site = self.alloc_decl();
            let (slot, binder) = (site.slot, site.binder);
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
                    // Put the labels on the early scheme too, not just the
                    // final one below, because a recursive call inside the body
                    // resolves against this scheme.
                    let names: Vec<String> = fd
                        .params
                        .iter()
                        .map(|p| p.identifier.name.clone())
                        .collect();
                    let param_labels = self.c.engine.intern_slice(&names);
                    self.declare(
                        &fd.identifier.name,
                        Scheme {
                            quantified: ArenaSlice::EMPTY,
                            ty: fn_ty,
                            kind: ValueKind::ModuleFn { param_labels },
                            def: Some(dl),
                        },
                        Some(site),
                    );
                    Prepared::Fn {
                        fd,
                        is_pub,
                        slot,
                        binder,
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
                        binder,
                        dl,
                    }
                }
            };
            prepared.push(p);
        }

        // Pass 3.5 hydrates constructor bodies and defines the constructor
        // values.
        for (((td, is_public), pt), dups) in type_decls.iter().zip(prepared_types).zip(&dup_ctors) {
            self.register_constructors(td, *is_public, pt, dups, &mut iface);
        }

        // Pass 4 builds the call graph over functions and constants, and finds
        // its SCCs with Tarjan's algorithm.
        let sccs = build_call_graph_sccs(&decls);

        // Pass 5 infers each SCC, then generalises it. SCC members are indexes
        // into `prepared`, so the data for each declaration is reached by
        // position.
        //
        // THE PHASE BOUNDARY. Everything up to `end_deferred_elaboration` is
        // the typecheck walk. It infers types, reserves `Function` slots and
        // records the shape of captures, and lowers no body. Every body it
        // walks is parked in `deferred_bodies`, and the Core pipeline drains
        // them in one loop afterwards, when every variable that any SCC would
        // solve is solved.
        //
        // A parked body reads nothing back from the env when it is drained. The
        // walk recorded what each name it met resolved to, so a toplevel
        // binding that binds a name again later can't change a body walked
        // earlier.
        self.begin_deferred_elaboration();
        // Filled in by position as each SCC is generalised. Tarjan covers every
        // node, so every entry is `Some` by the time of the export loop below.
        let mut final_schemes: Vec<Option<Scheme>> = vec![None; prepared.len()];
        for scc in &sccs {
            self.c.engine.enter_level();
            let mut inferred: Vec<(usize, Ty)> = Vec::with_capacity(scc.len());

            for &idx in scc {
                // Publish this declaration to the toplevel elaboration. These
                // are pushed in the order SCCs are visited, so the elaborated
                // spine initialises dependencies first, and a `const` referred
                // to before its declaration is stored before it is read.
                let name_id = self.c.engine.intern(prepared[idx].name());
                let binder = prepared[idx].binder();
                self.toplevel_decls.push(ToplevelDecl {
                    node: decls[idx].node(),
                    binder,
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
                        ..
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
                        // Give the references emitted inside this body to the
                        // function's own `DefId`, so the dead-code walk follows
                        // edges between definitions. This covers nested lambdas
                        // too, because a lambda has no `DefId` of its own.
                        let owner = self.owner_defid(fd.identifier.span, EntityKind::Function);
                        let fn_ty = self.with_owner(owner, |checker| {
                            checker.compile_declared_function(
                                name,
                                *slot,
                                &fd.params,
                                body,
                                param_tys.clone(),
                                *ret_ty,
                                hydrator,
                            )
                        });

                        self.record(name, fn_ty, fd.identifier.span);
                        nodes[decls[idx].node()] = CNode::Fn {
                            name: name_id,
                            binder,
                            span: fd.span,
                        };
                        // There's nothing to record for the toplevel spine.
                        // `compile_declared_function` keyed this body under the
                        // slot the spine stores it into, and a top-level
                        // function captures nothing (references to other
                        // top-level functions are `PushGlobal`).
                        inferred.push((idx, fn_ty));
                    }
                    Prepared::Const { cb, is_pub, dl, .. } => {
                        let name = &cb.identifier.name;
                        self.emit_def(*dl, name, cb.doc.clone(), *is_pub, DefinitionKind::Constant);
                        let owner = self.owner_defid(cb.identifier.span, EntityKind::Constant);
                        let (final_ty, init) = self.with_owner(owner, |checker| {
                            let mut h = Hydrator::new(AnnotationContext::Signature);
                            let annot_ty = cb.typ.as_ref().map(|t| checker.hydrate(&mut h, t));
                            let init = checker.compile_expr_with_hint(&cb.init, annot_ty);
                            let ty = if let Some(a) = annot_ty {
                                checker.c.engine.unify_at(a, init.ty, cb.identifier.span);
                                a
                            } else {
                                init.ty
                            };
                            (ty, init)
                        });
                        nodes[decls[idx].node()] = CNode::Const {
                            name: name_id,
                            binder,
                            init,
                        };
                        self.record(name, final_ty, cb.identifier.span);
                        inferred.push((idx, final_ty));
                    }
                }
            }

            self.c.engine.leave_level();

            for (idx, ty) in inferred {
                let p = &prepared[idx];
                // A const's initialiser has already run, so its type falls
                // under the relaxed value restriction like any value binding.
                // Function declarations are values too, and their variables all
                // sit under the arrow, where the guard quantifies as before.
                let mut scheme = if p.is_const() {
                    let restricted = self.c.restricted_generalization_cons();
                    self.c.engine.generalize_top_restricted(ty, &restricted)
                } else {
                    self.c.engine.generalize_top(ty)
                };
                scheme.def = Some(p.dl());
                if p.is_const() {
                    scheme.kind = ValueKind::Local;
                } else {
                    // `generalize_top` mints `ModuleFn` from a type alone, so
                    // it leaves the labels empty, and this is the first point
                    // where the declaration is in reach again. Without this,
                    // the final scheme (the one exported, and the one every
                    // call outside the body resolves against) would have no
                    // labels.
                    let names = p.param_names();
                    scheme.kind = ValueKind::ModuleFn {
                        param_labels: self.c.engine.intern_slice(&names),
                    };
                }
                self.declare(p.name(), scheme, Some(p.site()));
                final_schemes[idx] = Some(scheme);
            }
        }

        // Export now that the schemes are final. They come from
        // `final_schemes`, never back out of the env by name, which goes wrong
        // under shadowing. Go in source order, not SCC order, because
        // `ModuleInterface.values` is an `IndexMap` and importers see its
        // insertion order.
        for (p, s) in prepared.iter().zip(final_schemes) {
            #[allow(clippy::panic)] // the loop above generalizes every prepared decl
            let s = s.unwrap_or_else(|| panic!("decl '{}' was never generalised", p.name()));
            export_value(
                iface.as_deref_mut(),
                p.name(),
                p.is_pub(),
                s,
                Some(p.slot()),
            );
        }

        // Walk the other nodes (bindings and bare expressions) in order. This
        // is the only walk whose bindings take module-scope slots, each carried
        // on its own `CNode::Binding`. Nested walks come back into
        // `compile_node` from here, but they open frames or blocks, which
        // `bind_local` also checks.
        let outer_walk = std::mem::replace(&mut self.walking_module_statements, true);
        for &(idx, node) in &other_nodes {
            nodes[idx] = self.compile_node(node).1;
        }
        self.walking_module_statements = outer_walk;

        // This is the end of the walk and the start of the Core pipeline. The
        // types are final. Whatever no SCC solved is now `Generic`, which only
        // `zonk` cares about (`Generic` can be quantified, and `Unbound` is an
        // error).
        self.end_deferred_elaboration();
        self.check_nil_discards();

        self.close_block();
        nodes
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
                h.add_type_variable(&tp.name, &mut self.c.engine);
            if duplicate {
                self.c
                    .error(format!("Duplicate type parameter '{}'", tp.name), tp.span);
            }
            type_params.push(TypeParam {
                name: self.c.engine.intern(&tp.name),
                id,
            });
            param_tys.push(ty);
        }
        (h, self.c.engine.push_type_params(&type_params), param_tys)
    }

    /// Intern the current module path as a `str_slices` slice. It is memoised,
    /// so a module's path takes one pool entry however many types it declares.
    pub(super) fn current_module_slice(&mut self) -> ArenaSlice<pool::StrSlices> {
        if let Some(sl) = self.module_path_slice {
            return sl;
        }
        let segs = self.current_module.clone();
        let sl = self.c.engine.intern_slice(&segs);
        self.module_path_slice = Some(sl);
        sl
    }

    /// Record a named declaration as a graph definition. The entity comes from
    /// `kind`, so the record and its payload can't disagree.
    fn store_and_emit_def(
        &mut self,
        id: &ast::Identifier,
        doc: &Option<String>,
        kind: DefinitionKind,
        is_pub: bool,
    ) {
        let dl = DefinitionLocation::new(id.span, self.current_module_slice(), kind.entity());
        self.emit_def(dl, &id.name, doc.clone(), is_pub, kind);
    }

    /// Run `f` with `current_owner` set to `owner`, so references emitted
    /// inside `f` belong to it.
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
        let fn_ty = self.c.engine.mk_fun(&param_tys, ret_ty);
        FnSig {
            hydrator,
            param_tys,
            ret_ty,
            fn_ty,
        }
    }

    /// Registers each alias that isn't part of a cycle, and records its
    /// `TypeId` back in the matching `prepared_types` slot, so the export in
    /// Pass 3.5 reads the registry by id and not by name.
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

        // `tarjan_scc` gives what is depended on before what depends on it. A
        // component with more than one node, or one node with an edge to
        // itself, is a cycle. Report each member and skip it, so one bad alias
        // doesn't block the rest.
        for component in tarjan_scc(&graph) {
            let cyclic =
                component.len() > 1 || graph.find_edge(component[0], component[0]).is_some();
            if cyclic {
                for node in &component {
                    if let Some(&(_, td, _, _)) = by_idx.get(node) {
                        self.c.error(
                            format!(
                                "Recursive type alias '{}'. An alias can't refer back to itself",
                                td.identifier.name
                            ),
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
            let name_id = self.c.engine.intern(&td.identifier.name);
            let module = self.current_module_slice();
            let type_id = self.declare_type(&td.identifier.name, name_id, module, type_params);
            let owner = self.owner_defid(td.identifier.span, EntityKind::Type);
            let target = self.with_owner(owner, |checker| checker.hydrate(&mut h, rhs));
            // Store the target CLOSED (`Var(param_id)` → `Bound(idx)`). The
            // body outlives incremental rewinds, which clear the engine's
            // `vars` table, and the exported interface copies this `TypeInfo`
            // by value before `export_type` in Pass 3.5.
            let target = self.c.engine.close_body(target, type_params);
            self.c
                .types
                .set_type_body(type_id, TypeBody::Alias { target });
            prepared_types[decl_idx] = PreparedType::Alias {
                type_id: Some(type_id),
            };
            self.store_and_emit_def(&td.identifier, &td.doc, DefinitionKind::Type, is_pub);
        }
    }

    /// `dup_ctors` lines up with `ctors` by position. A `true` entry marks a
    /// constructor with a duplicate name, which is dropped here instead of
    /// being added to `variants` or defined as a value.
    fn register_constructors(
        &mut self,
        td: &ast::TypeDeclaration,
        is_public: bool,
        pt: PreparedType,
        dup_ctors: &[bool],
        iface: &mut Option<&mut ModuleInterface>,
    ) {
        // The id the export below reads the registry with. It goes by position,
        // never by name, because a registration with the same name analysed
        // later shadows the by-name map but can't take over the by-id entry.
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
            // Aliases and external types have no constructors, so just export
            // the type info.
            let ti = export_id.and_then(|id| self.c.types.lookup_type_info_by_id(id));
            export_type(iface.as_deref_mut(), &td.identifier.name, is_public, ti);
            return;
        };
        let ctors_public = is_public && !*opaque;

        let type_name = &td.identifier.name;
        let m = self.current_module_slice();
        let mut variants: Vec<Variant> = Vec::with_capacity(ctors.len());

        // Hydrating constructors and fields is the type's body. The types it
        // names are references owned by this type, which gives the dead-code
        // walk its edges between types.
        let owner = self.owner_defid(td.identifier.span, EntityKind::Type);
        self.with_owner(owner, |checker| {
            for (ctor, _) in ctors.iter().zip(dup_ctors).filter(|&(_, dup)| !dup) {
                // Index into the variants that survived, not the constructor
                // list in the source, because dropped duplicates must not leave
                // gaps in the `variant_idx` ↔ `variants` match that the VM tags
                // by.
                let variant_idx = variants.len() as u16;
                let mut field_itys: Vec<Ty> = Vec::with_capacity(ctor.fields.len());
                let mut field_defs: Vec<VariantField> = Vec::with_capacity(ctor.fields.len());
                let mut label_ids: Vec<StrId> = Vec::with_capacity(ctor.fields.len());

                for f in &ctor.fields {
                    let ity = checker.hydrate(&mut h, &f.typ);
                    let label = checker.c.engine.intern(&f.label.name);
                    field_itys.push(ity);
                    // Store it CLOSED (`Var(param_id)` → `Bound(idx)`) so it
                    // never refers to the engine's short-lived `vars` table,
                    // which incremental rewinds clear. The constructor scheme
                    // below still generalises over the open `ity`.
                    field_defs.push(VariantField {
                        label,
                        ty: checker.c.engine.close_body(ity, type_params),
                    });
                    label_ids.push(label);

                    let field_dl = DefinitionLocation::new(f.label.span, m, EntityKind::Field);
                    // Variants that share a label share the field: the first
                    // one declared is where it is defined.
                    checker
                        .field_defs
                        .entry((type_id, label))
                        .or_insert(field_dl);
                    checker.emit_def(
                        field_dl,
                        &f.label.name,
                        None,
                        ctors_public,
                        DefinitionKind::Field,
                    );
                }

                let fields = checker.c.engine.push_variant_fields(&field_defs);
                // One interned name for the variant, shared by the type's
                // variant table and the constructor's own scheme, so a use
                // never has to work it out again from the name it was bound
                // under, which an alias changes.
                let variant_name = checker.c.engine.intern(&ctor.identifier.name);
                variants.push(Variant {
                    name: variant_name,
                    fields,
                });
                let field_labels = checker.c.engine.push_str_ids(&label_ids);

                let result_ty = checker
                    .c
                    .engine
                    .mk_con_id(type_id, type_name_id, &param_generics);
                let ctor_ty = if field_itys.is_empty() {
                    result_ty
                } else {
                    checker.c.engine.mk_fun(&field_itys, result_ty)
                };
                let mut scheme = checker.c.engine.generalize_top(ctor_ty);
                scheme.kind = ValueKind::Constructor {
                    type_name: type_name_id,
                    type_id,
                    variant_idx,
                    variant_name,
                    arity: ctor.fields.len() as u16,
                    field_labels,
                };
                // One identity for the constructor. `scheme.def` and the graph
                // `DefId` below both read it, so goto-def and find-references
                // can't disagree about where it lives.
                let ctor_dl =
                    DefinitionLocation::new(ctor.identifier.span, m, EntityKind::Constructor);
                scheme.def = Some(ctor_dl);

                let name = &ctor.identifier.name;
                checker.declare(name, scheme, None);
                // Copy the field labels into the graph and the export, so hover
                // can show `NotFound fn(path String) IoError` without going
                // back to the engine's string pool.
                let labels = checker.c.engine.strs_of(field_labels);
                // `Config(name: 'x')` names the constructor, never the type, so
                // without the `ctor_of` edge every type with one constructor
                // would look unused.
                let type_dl = DefinitionLocation::new(td.identifier.span, m, EntityKind::Type);
                let type_defid = checker.defid_of(type_dl);
                checker.store_and_emit_def(
                    &ctor.identifier,
                    &ctor.doc,
                    DefinitionKind::Constructor {
                        ctor_of: Some(type_defid),
                        param_names: labels.clone(),
                    },
                    ctors_public,
                );
                // Always do this. `export_value` sends a name that isn't `pub`
                // to `iface.private_names`, so a private constructor gives
                // importers "'X' is private" and not "no member 'X'".
                export_value(iface.as_deref_mut(), name, ctors_public, scheme, None);
            }
        });

        let variants = self.c.engine.push_variants(&variants);
        self.c.types.set_type_body(
            type_id,
            TypeBody::Custom {
                variants,
                ctors_public,
            },
        );

        let ti = self.c.types.lookup_type_info_by_id(type_id);
        export_type(iface.as_deref_mut(), type_name, is_public, ti);
    }
}

#[derive(Clone, Copy)]
enum AttrTarget {
    Fn,
    Type,
}

impl ModuleChecker<'_> {
    /// Reject unknown attributes, attributes on the wrong kind of item, and any
    /// attribute outside the embedded stdlib. The number of arguments isn't
    /// checked here, because the parser already rejects a `@vm` function with
    /// anything but one.
    fn validate_attributes(&mut self, attrs: &[ast::Attribute], in_stdlib: bool, on: AttrTarget) {
        for a in attrs {
            match a.name.name.as_str() {
                "vm" => {
                    if !in_stdlib {
                        self.c.error(
                            "'@vm' is only allowed in the standard library".to_string(),
                            a.span,
                        );
                    }
                    match on {
                        AttrTarget::Fn => {}
                        // These are named, not negated, so a new kind of item
                        // has to decide whether '@vm' is allowed on it.
                        AttrTarget::Type => {
                            self.c
                                .error("'@vm' may only be used on functions".to_string(), a.span);
                        }
                    }
                }
                other => {
                    self.c
                        .error(format!("Unknown attribute '@{other}'"), a.span);
                }
            }
        }
    }
}

/// Record a name in the module interface. It goes in `values` or `types` when
/// it is public, and in `private_names` if not, so importers get a "this is
/// private" error.
fn export_value(
    iface: Option<&mut ModuleInterface>,
    name: &str,
    is_pub: bool,
    scheme: Scheme,
    slot: Option<GlobalSlot>,
) {
    let Some(iface) = iface else { return };
    if is_pub {
        let ev = ExportedValue {
            scheme,
            local_slot: slot,
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
            iface
                .types
                .insert(name.to_string(), ExportedType { info: ti });
        }
    } else {
        iface.private_names.insert(name.to_string());
    }
}

/// Records a top-level name, with an error if it was already defined. Returns
/// `true` for a duplicate so the caller drops the extra declaration. Dropping
/// it matters, because the call graph and the alias sort key their nodes by
/// name, so a second declaration would share the first one's node and throw SCC
/// scheduling out.
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

/// Emits a diagnostic if `id` clashes with a name the prelude reserves.
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

/// Walk a type annotation's AST and collect every reference to a named type.
fn collect_type_ast_deps<'a>(t: &'a ast::TypeIdentifier, out: &mut Vec<&'a str>) {
    match &t.kind {
        ast::TypeKind::NamedType(nt) => {
            // A qualified `module::Type` lives in another module, so it can
            // never be a dependency edge onto a local declaration. Pushing the
            // bare member name would mix it up with any local type of the same
            // name.
            if nt.qualifier.is_none() {
                out.push(nt.identifier.name.as_str());
            }
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

/// Returns SCCs in dependency order, leaves first. Each inner `Vec` holds
/// indexes into `decls`.
fn build_call_graph_sccs(decls: &[Decl<'_>]) -> Vec<Vec<usize>> {
    let mut graph: StableGraph<(), (), Directed> = StableGraph::new();
    let mut node_of: HashMap<&str, NodeIndex> = HashMap::new();

    for d in decls {
        let n = graph.add_node(());
        // Pass 0 dropped declarations with the same name, so names are unique
        // here. A clash would put two declarations on one node.
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

    // Reverse topological order puts callees first, which is the order
    // inference needs.
    tarjan_scc(&graph)
        .into_iter()
        .map(|component| component.into_iter().map(|n| n.index()).collect::<Vec<_>>())
        .collect()
}

/// Walks an expression body and collects edges to other top-level declarations.
/// It keeps a set of shadowed names, so names bound locally don't count as
/// references.
struct RefWalker<'a, 'g> {
    targets: &'a HashMap<&'a str, NodeIndex>,
    graph: &'g mut StableGraph<(), (), Directed>,
    from: NodeIndex,
    shadowed: HashSet<&'a str>,
    /// A log of names just added to `shadowed`, for undoing. `scoped` truncates
    /// it on the way out instead of deep-cloning the whole set for each block,
    /// arm or lambda.
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
        // exactly what was shadowed at this level. Nothing else takes names out
        // of the log, so it is at least as long as it was.
        let len = self.undo.len();
        debug_assert!(len >= mark, "the undo log shrank past this scope's mark");
        for name in self.undo.drain(mark.min(len)..) {
            self.shadowed.remove(name);
        }
    }

    fn expr(&mut self, e: &'a ast::Expression) {
        use ast::Expression as E;
        match e {
            E::Identifier(id) => self.referenced(&id.name),

            // A module member isn't a local name.
            E::NumberLiteral(_)
            | E::StringLiteral(_)
            | E::ErrorNode(_)
            | E::ModuleAccessExpression(_) => {}

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
                // The right side of `.` is a member, not a free identifier.
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

            // Pipes are removed before this walk, the same guarantee as for
            // `Statement::Backpass` below. One that survives is walked like the
            // call it stands for.
            E::PipeExpression(p) => {
                self.expr(&p.left);
                self.expr(&p.right);
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
                ast::Statement::Binding(b) => {
                    self.expr(&b.init);
                    self.pattern(&b.pattern);
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
                ast::Statement::ImportDeclaration(_) => {}
                // Backpasses are removed before this walk, and one that
                // survives was already rejected by the parser. Walk the call,
                // then the binders, like the lambda the sugar stands for.
                ast::Statement::Backpass(bp) => {
                    self.expr(&bp.call);
                    for b in &bp.binders {
                        self.define(&b.name);
                    }
                }
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
