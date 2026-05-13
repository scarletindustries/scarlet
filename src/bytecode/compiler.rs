use std::collections::HashMap;

use indexmap::IndexMap;

use super::{Function, Instruction, Op, Program, Value, op, op_arg};
use crate::ast;
use crate::diagnostic::{Diagnostic, Severity, has_errors};
use crate::flags::Flags;
use crate::span::Span;
use crate::token::Kind;
use crate::type_def::Type;
use crate::types::{
    Constraint, DefinitionLocation, EnumInfo, InferEngine, InferType, Pat, TypeEnv, TypeInfo,
    ast_pattern_to_pat, check_exhaustiveness, mono, new_engine, new_env,
};

// ============================================================================
// TypePosition — recorded during compilation for LSP hover/go-to-def.
// We record the live InferType; at the end of compile() we resolve once
// all unifications have happened.
// ============================================================================

#[derive(Debug, Clone)]
pub struct TypePosition {
    pub line: i32,
    pub column: i32,
    pub end_col: i32,
    pub name: String,
    pub type_info: Type,
    pub def_line: i32,
    pub def_col: i32,
    pub def_end: i32,
    pub doc: Option<String>,
}

#[derive(Debug, Clone)]
struct RecordedPosition {
    span: Span,
    name: String,
    ty: InferType,
    doc: Option<String>,
    def: Option<DefinitionLocation>,
}

// ============================================================================
// CompileResult
// ============================================================================

#[derive(Debug)]
pub struct CompileResult {
    pub program: Program,
    pub diagnostics: Vec<Diagnostic>,
    pub type_positions: Vec<TypePosition>,
    pub success: bool,
}

// ============================================================================
// Compiler — single-pass: HM type inference + bytecode emission
// ============================================================================

struct Scope {
    locals: HashMap<String, i32>,
}

pub(super) struct Compiler {
    #[allow(dead_code)]
    flags: Flags,
    // --- Codegen state ---
    program: Program,
    locals: HashMap<String, i32>,
    outer_scopes: Vec<Scope>,
    local_count: i32,
    captures: HashMap<String, i32>,
    capture_names: Vec<String>,
    current_binding: String,
    in_tail_position: bool,
    // --- Type inference state ---
    pub(super) engine: InferEngine,
    pub(super) env: TypeEnv,
    recorded: Vec<RecordedPosition>,
    current_fn_err: Option<InferType>,
    check_only: bool,
}

enum VarAccess {
    Local(i32),
    Capture(i32),
    Self_,
}

pub fn compile(expr: &ast::Expression, fl: Flags) -> CompileResult {
    compile_impl(expr, fl, false)
}

pub fn check(expr: &ast::Expression, fl: Flags) -> CompileResult {
    compile_impl(expr, fl, true)
}

fn compile_impl(expr: &ast::Expression, fl: Flags, check_only: bool) -> CompileResult {
    let mut c = Compiler {
        flags: fl,
        program: Program {
            constants: vec![],
            functions: vec![],
            code: vec![],
            entry: 0,
        },
        locals: HashMap::new(),
        outer_scopes: vec![],
        local_count: 0,
        captures: HashMap::new(),
        capture_names: vec![],
        current_binding: String::new(),
        in_tail_position: false,
        engine: new_engine(),
        env: new_env(),
        recorded: vec![],
        current_fn_err: None,
        check_only,
    };

    c.register_prelude();
    c.register_builtins();

    let main_start = c.program.code.len() as i32;
    c.compile_expr(expr);
    c.emit(Op::Halt);

    c.program.functions.push(Function {
        name: "__main__".to_string(),
        arity: 0,
        locals: c.local_count,
        capture_count: 0,
        code_start: main_start,
        code_len: c.program.code.len() as i32 - main_start,
    });
    c.program.entry = c.program.functions.len() as i32 - 1;

    let mut positions: Vec<TypePosition> = Vec::with_capacity(c.recorded.len());
    let recorded = std::mem::take(&mut c.recorded);
    for r in recorded {
        let (def_line, def_col, def_end) = match r.def {
            Some(loc) => (loc.line, loc.column, loc.end_col),
            None => (-1, -1, -1),
        };
        positions.push(TypePosition {
            line: r.span.start_line,
            column: r.span.start_column,
            end_col: r.span.end_column,
            name: r.name,
            type_info: c.engine.resolve(&r.ty, Some(&c.env)),
            def_line,
            def_col,
            def_end,
            doc: r.doc,
        });
    }

    let success = !has_errors(&c.engine.diagnostics);
    CompileResult {
        program: c.program,
        diagnostics: c.engine.diagnostics,
        type_positions: positions,
        success,
    }
}

impl Compiler {
    // ========================================================================
    // Codegen primitives (no-op in check_only mode)
    // ========================================================================

    #[inline]
    fn emit(&mut self, o: Op) {
        if self.check_only {
            return;
        }
        self.program.code.push(op(o));
    }

    #[inline]
    fn emit_arg(&mut self, o: Op, operand: i32) {
        if self.check_only {
            return;
        }
        self.program.code.push(op_arg(o, operand));
    }

    #[inline]
    fn patch(&mut self, addr: i32, inst: Instruction) {
        if self.check_only {
            return;
        }
        self.program.code[addr as usize] = inst;
    }

    fn current_addr(&self) -> i32 {
        self.program.code.len() as i32
    }

    fn add_constant(&mut self, v: Value) -> i32 {
        if self.check_only {
            return 0;
        }
        self.program.constants.push(v);
        self.program.constants.len() as i32 - 1
    }

    fn get_or_create_local(&mut self, name: &str) -> i32 {
        if let Some(idx) = self.locals.get(name) {
            return *idx;
        }
        let idx = self.local_count;
        self.locals.insert(name.to_string(), idx);
        self.local_count += 1;
        idx
    }

    fn emit_load(&mut self, access: &VarAccess) {
        match access {
            VarAccess::Local(idx) => self.emit_arg(Op::PushLocal, *idx),
            VarAccess::Capture(idx) => self.emit_arg(Op::PushCapture, *idx),
            VarAccess::Self_ => self.emit(Op::PushSelf),
        }
    }

    fn resolve_variable(&mut self, name: &str) -> Option<VarAccess> {
        if let Some(idx) = self.locals.get(name) {
            return Some(VarAccess::Local(*idx));
        }
        if let Some(idx) = self.captures.get(name) {
            return Some(VarAccess::Capture(*idx));
        }
        for scope in &self.outer_scopes {
            if scope.locals.contains_key(name) {
                if name == self.current_binding {
                    return Some(VarAccess::Self_);
                }
                let capture_idx = self.capture_names.len() as i32;
                self.captures.insert(name.to_string(), capture_idx);
                self.capture_names.push(name.to_string());
                return Some(VarAccess::Capture(capture_idx));
            }
        }
        None
    }

    // ========================================================================
    // Diagnostics & LSP recording
    // ========================================================================

    fn error(&mut self, msg: String, sp: Span) {
        self.engine.diagnostics.push(Diagnostic {
            span: sp,
            severity: Severity::Error,
            message: msg,
        });
    }

    fn at_top_level(&self) -> bool {
        self.env.scope_depth() <= 2
    }

    fn record(&mut self, name: &str, ty: InferType, sp: Span, doc: Option<String>) {
        // Capture the definition location NOW while the correct scope is active.
        // env.definitions is flat (last-write-wins) so looking it up at finalize
        // time gets the wrong answer when names are reused across scopes.
        self.recorded.push(RecordedPosition {
            span: sp,
            name: name.to_string(),
            ty,
            doc,
            def: self.env.lookup_definition(name),
        });
    }

    // ========================================================================
    // Type annotation resolution: ast::TypeIdentifier → InferType
    // ========================================================================

    fn resolve_type(
        &mut self,
        t: &ast::TypeIdentifier,
        param_vars: &HashMap<String, InferType>,
    ) -> InferType {
        match &t.kind {
            ast::TypeKind::OptionType(ot) => {
                let inner = self.resolve_type(&ot.inner, param_vars);
                self.engine.icon_option(inner)
            }
            ast::TypeKind::ArrayType(at) => {
                let elem = self.resolve_type(&at.element, param_vars);
                self.engine.icon_array(elem)
            }
            ast::TypeKind::FunctionType(ft) => {
                let mut params: Vec<InferType> = Vec::with_capacity(ft.params.len());
                for pt in &ft.params {
                    params.push(self.resolve_type(pt, param_vars));
                }
                let ret = if let Some(rt) = &ft.return_type {
                    self.resolve_type(rt, param_vars)
                } else {
                    self.engine.icon_none()
                };
                let err = ft
                    .error_type
                    .as_ref()
                    .map(|et| Box::new(self.resolve_type(et, param_vars)));
                InferType::IFun {
                    params,
                    ret: Box::new(ret),
                    err,
                }
            }
            ast::TypeKind::NamedType(nt) => {
                let name = &nt.identifier.name;

                if let Some(v) = param_vars.get(name) {
                    return v.clone();
                }

                match name.as_str() {
                    "Int" => return self.engine.icon_int(),
                    "Float" => return self.engine.icon_float(),
                    "String" => return self.engine.icon_string(),
                    "Bool" => return self.engine.icon_bool(),
                    "None" => return self.engine.icon_none(),
                    _ => {}
                }

                if let Some(info) = self.env.lookup_type_info(name) {
                    let mut args: Vec<InferType> = Vec::with_capacity(nt.type_args.len());
                    for ta in &nt.type_args {
                        args.push(self.resolve_type(ta, param_vars));
                    }
                    let tparams = type_info_params(&info);
                    if args.is_empty() && !tparams.is_empty() {
                        for _ in 0..tparams.len() {
                            args.push(self.engine.fresh_var());
                        }
                    }
                    return self.engine.icon(name.clone(), args);
                }

                if name.len() == 1 {
                    let b = name.as_bytes()[0];
                    if b.is_ascii_uppercase() {
                        return self.engine.fresh_var();
                    }
                }

                self.error(format!("Unknown type '{}'", name), nt.identifier.span);
                self.engine.fresh_var()
            }
        }
    }

    // ========================================================================
    // Node dispatch
    // ========================================================================

    fn compile_node(&mut self, node: &ast::Node) -> InferType {
        match node {
            ast::Node::Statement(s) => {
                self.compile_statement(s);
                self.engine.icon_none()
            }
            ast::Node::Expression(e) => self.compile_expr(e),
        }
    }

    // ========================================================================
    // Statements
    // ========================================================================

    fn compile_statement(&mut self, stmt: &ast::Statement) {
        match stmt {
            ast::Statement::VariableBinding(vb) => {
                let name = vb.identifier.name.clone();
                let idx = self.get_or_create_local(&name);

                let annot_ty = vb
                    .typ
                    .as_ref()
                    .map(|annot| self.resolve_type(annot, &HashMap::new()));

                self.engine.enter_level();
                let self_ty = self.engine.fresh_var();
                self.env.define(&name, mono(self_ty.clone()));

                let old_binding = std::mem::replace(&mut self.current_binding, name.clone());
                let init_ty = self.compile_expr_with_hint(&vb.init, annot_ty.clone());
                self.current_binding = old_binding;

                self.engine.unify(&self_ty, &init_ty, vb.init.span());
                self.engine.leave_level();

                let final_ty = if let Some(a) = &annot_ty {
                    self.engine.unify(a, &init_ty, vb.identifier.span);
                    a.clone()
                } else {
                    init_ty
                };

                let scheme = self.engine.generalize(&final_ty);
                self.env
                    .define_at(&name, scheme, def_loc(vb.identifier.span));
                if let Some(doc) = &vb.doc {
                    self.env.store_doc(&name, doc.clone());
                }
                self.record(&name, final_ty, vb.identifier.span, vb.doc.clone());

                self.emit_arg(Op::StoreLocal, idx);
            }
            ast::Statement::ConstBinding(cb) => {
                if !self.at_top_level() {
                    self.error(
                        "const declarations are only allowed at the top level".to_string(),
                        cb.span,
                    );
                }
                let name = cb.identifier.name.clone();

                let annot_ty = cb
                    .typ
                    .as_ref()
                    .map(|annot| self.resolve_type(annot, &HashMap::new()));

                self.engine.enter_level();
                let init_ty = self.compile_expr_with_hint(&cb.init, annot_ty.clone());
                self.engine.leave_level();

                let final_ty = if let Some(a) = &annot_ty {
                    self.engine.unify(a, &init_ty, cb.identifier.span);
                    a.clone()
                } else {
                    init_ty
                };

                let scheme = self.engine.generalize(&final_ty);
                self.env
                    .define_at(&name, scheme, def_loc(cb.identifier.span));
                if let Some(doc) = &cb.doc {
                    self.env.store_doc(&name, doc.clone());
                }
                self.record(&name, final_ty, cb.identifier.span, cb.doc.clone());

                let idx = self.get_or_create_local(&name);
                self.emit_arg(Op::StoreLocal, idx);
            }
            ast::Statement::TypePatternBinding(tpb) => {
                let init_ty = self.compile_expr(&tpb.init);
                let annot_ty = self.resolve_type(&tpb.typ, &HashMap::new());
                self.engine.unify(&annot_ty, &init_ty, tpb.typ.span);
                self.emit(Op::Pop);
            }
            ast::Statement::TupleDestructuringBinding(tdb) => {
                let init_ty = self.compile_expr(&tdb.init);

                let mut elem_vars: Vec<InferType> = Vec::with_capacity(tdb.patterns.len());
                for _ in 0..tdb.patterns.len() {
                    elem_vars.push(self.engine.fresh_var());
                }
                self.engine.unify(
                    &init_ty,
                    &InferType::ITuple {
                        elements: elem_vars.clone(),
                    },
                    tdb.init.span(),
                );

                for (i, pattern) in tdb.patterns.iter().enumerate() {
                    let elem_ty = elem_vars[i].clone();
                    match pattern {
                        ast::Expression::Identifier(id) => {
                            self.emit(Op::Dup);
                            self.emit_arg(Op::TupleIndex, i as i32);
                            let idx = self.get_or_create_local(&id.name);
                            self.emit_arg(Op::StoreLocal, idx);
                            self.env
                                .define_at(&id.name, mono(elem_ty.clone()), def_loc(id.span));
                            self.record(&id.name, elem_ty, id.span, None);
                        }
                        ast::Expression::TypeIdentifier(ti) => {
                            let annot_ty = self.resolve_type(ti, &HashMap::new());
                            self.engine.unify(&annot_ty, &elem_ty, ti.span);
                        }
                        _ => {
                            self.error(
                                "tuple destructuring only supports identifiers and type patterns"
                                    .to_string(),
                                tdb.span,
                            );
                        }
                    }
                }
                self.emit(Op::Pop);
            }
            ast::Statement::FunctionDeclaration(fd) => {
                if !self.at_top_level() {
                    self.error(
                        "named function declarations are only allowed at the top level".to_string(),
                        fd.span,
                    );
                }
                let name = fd.identifier.name.clone();

                self.engine.enter_level();
                let self_ty = self.engine.fresh_var();
                self.env.define(&name, mono(self_ty.clone()));

                let fn_ty = self.compile_function_common(
                    Some(name.clone()),
                    &fd.params,
                    &fd.body,
                    fd.return_type.as_ref(),
                    fd.error_type.as_ref(),
                );

                self.engine.unify(&self_ty, &fn_ty, fd.identifier.span);
                self.engine.leave_level();

                let scheme = self.engine.generalize(&fn_ty);
                self.env
                    .define_at(&name, scheme, def_loc(fd.identifier.span));
                if let Some(doc) = &fd.doc {
                    self.env.store_doc(&name, doc.clone());
                }
                self.record(&name, fn_ty, fd.identifier.span, fd.doc.clone());

                let idx = self.get_or_create_local(&name);
                self.emit_arg(Op::StoreLocal, idx);
            }
            ast::Statement::StructDeclaration(sd) => {
                if !self.at_top_level() {
                    self.error(
                        "struct declarations are only allowed at the top level".to_string(),
                        sd.span,
                    );
                }
                let name = sd.identifier.name.clone();
                let type_params: Vec<String> =
                    sd.type_params.iter().map(|tp| tp.name.clone()).collect();

                let mut param_vars: HashMap<String, InferType> = HashMap::new();
                for tp in &type_params {
                    let v = self.engine.fresh_var();
                    self.engine.name_var(&v, tp.clone());
                    param_vars.insert(tp.clone(), v);
                }
                let mut fields: IndexMap<String, Type> = IndexMap::new();
                for field in &sd.fields {
                    let field_ty = self.resolve_type(&field.typ, &param_vars);
                    fields.insert(
                        field.identifier.name.clone(),
                        self.engine.resolve(&field_ty, Some(&self.env)),
                    );
                    let qualified = format!("{}.{}", name, field.identifier.name);
                    self.env
                        .definitions
                        .insert(qualified.clone(), def_loc(field.identifier.span));
                    if let Some(doc) = &field.doc {
                        self.env.store_doc(&qualified, doc.clone());
                    }
                }

                self.env.register_struct(&name, type_params, fields);
                self.env
                    .definitions
                    .insert(name.clone(), def_loc(sd.identifier.span));
                if let Some(doc) = &sd.doc {
                    self.env.store_doc(&name, doc.clone());
                }
            }
            ast::Statement::EnumDeclaration(ed) => {
                if !self.at_top_level() {
                    self.error(
                        "enum declarations are only allowed at the top level".to_string(),
                        ed.span,
                    );
                }
                let name = ed.identifier.name.clone();
                let type_params: Vec<String> =
                    ed.type_params.iter().map(|tp| tp.name.clone()).collect();

                let mut param_vars: HashMap<String, InferType> = HashMap::new();
                for tp in &type_params {
                    let v = self.engine.fresh_var();
                    self.engine.name_var(&v, tp.clone());
                    param_vars.insert(tp.clone(), v);
                }

                let mut variants: IndexMap<String, Vec<Type>> = IndexMap::new();
                for variant in &ed.variants {
                    let mut payload: Vec<Type> = Vec::with_capacity(variant.payload.len());
                    for pt in &variant.payload {
                        let payload_ty = self.resolve_type(pt, &param_vars);
                        payload.push(self.engine.resolve(&payload_ty, Some(&self.env)));
                    }
                    variants.insert(variant.identifier.name.clone(), payload);
                    if let Some(doc) = &variant.doc {
                        self.env.store_doc(
                            &format!("{}.{}", name, variant.identifier.name),
                            doc.clone(),
                        );
                    }
                    self.env.definitions.insert(
                        format!("{}.{}", name, variant.identifier.name),
                        def_loc(variant.identifier.span),
                    );
                }

                self.env.register_enum(&name, type_params, variants);
                self.env
                    .definitions
                    .insert(name.clone(), def_loc(ed.identifier.span));
                if let Some(doc) = &ed.doc {
                    self.env.store_doc(&name, doc.clone());
                }
            }
            ast::Statement::ImportDeclaration(_) => {}
            ast::Statement::ExportDeclaration(exp) => {
                self.compile_statement(&exp.declaration);
            }
        }
    }

    // ========================================================================
    // Expressions
    // ========================================================================

    fn compile_expr(&mut self, expr: &ast::Expression) -> InferType {
        let is_tail = self.in_tail_position;
        self.in_tail_position = false;

        match expr {
            ast::Expression::BlockExpression(be) => {
                self.env.push_scope();
                let mut last_ty = self.engine.icon_none();

                for (i, node) in be.body.iter().enumerate() {
                    let is_last = i == be.body.len() - 1;
                    self.in_tail_position = is_tail && is_last;
                    let ty = self.compile_node(node);
                    self.in_tail_position = false;

                    if is_last {
                        last_ty = ty;
                    } else if matches!(node, ast::Node::Expression(_)) {
                        let resolved = self.engine.find(&ty);
                        match &resolved {
                            InferType::ICon { name, .. } => {
                                if name != "None" {
                                    let ty_str = self.engine.type_to_str(&ty);
                                    self.error(
                                        format!(
                                            "Expression of type '{ty_str}' must be consumed. Assign it to a variable or use '{ty_str} =' to discard"
                                        ),
                                        ast::node_span(node),
                                    );
                                }
                            }
                            InferType::IVar { .. } => {}
                            _ => {
                                let ty_str = self.engine.type_to_str(&ty);
                                self.error(
                                    format!(
                                        "Expression of type '{ty_str}' must be consumed. Assign it to a variable or use '{ty_str} =' to discard"
                                    ),
                                    ast::node_span(node),
                                );
                            }
                        }
                        self.emit(Op::Pop);
                    }
                }

                if !matches!(be.body.last(), Some(ast::Node::Expression(_))) {
                    self.emit(Op::PushNone);
                }

                self.env.pop_scope();
                last_ty
            }
            ast::Expression::NumberLiteral(nl) => {
                if nl.value.contains('.') {
                    let idx =
                        self.add_constant(Value::Float(nl.value.parse::<f64>().unwrap_or(0.0)));
                    self.emit_arg(Op::PushConst, idx);
                    self.engine.icon_float()
                } else {
                    let idx = self.add_constant(Value::Int(nl.value.parse::<i64>().unwrap_or(0)));
                    self.emit_arg(Op::PushConst, idx);
                    self.engine.icon_int()
                }
            }
            ast::Expression::StringLiteral(sl) => {
                let idx = self.add_constant(Value::Str(sl.value.clone()));
                self.emit_arg(Op::PushConst, idx);
                self.engine.icon_string()
            }
            ast::Expression::InterpolatedString(is) => {
                if is.parts.is_empty() {
                    let idx = self.add_constant(Value::Str(String::new()));
                    self.emit_arg(Op::PushConst, idx);
                } else {
                    self.compile_expr(&is.parts[0]);
                    self.emit(Op::ToString);
                    for i in 1..is.parts.len() {
                        self.compile_expr(&is.parts[i]);
                        self.emit(Op::ToString);
                        self.emit(Op::StrConcat);
                    }
                }
                self.engine.icon_string()
            }
            ast::Expression::BooleanLiteral(bl) => {
                if bl.value {
                    self.emit(Op::PushTrue);
                } else {
                    self.emit(Op::PushFalse);
                }
                self.engine.icon_bool()
            }
            ast::Expression::NoneExpression(_) => {
                self.emit(Op::PushNone);
                self.engine.icon_none()
            }
            ast::Expression::Identifier(id) => self.compile_identifier(id),
            ast::Expression::BinaryExpression(be) => self.compile_binary(be),
            ast::Expression::UnaryExpression(ue) => {
                let inner_ty = self.compile_expr(&ue.expression);
                match ue.op.kind {
                    Kind::PuncExclamationMark => {
                        let b = self.engine.icon_bool();
                        self.engine.unify(&inner_ty, &b, ue.expression.span());
                        self.emit(Op::Not);
                        self.engine.icon_bool()
                    }
                    Kind::PuncMinus => {
                        let result = self.engine.fresh_constrained_var(Constraint::Numeric);
                        self.engine.unify(&inner_ty, &result, ue.expression.span());
                        self.emit(Op::Neg);
                        result
                    }
                    _ => {
                        self.error(format!("Unknown unary operator: {}", ue.op.kind), ue.span);
                        self.engine.fresh_var()
                    }
                }
            }
            ast::Expression::IfExpression(ie) => {
                let cond_ty = self.compile_expr(&ie.condition);
                let b = self.engine.icon_bool();
                self.engine.unify(&cond_ty, &b, ie.condition.span());

                let else_jump = self.current_addr();
                self.emit_arg(Op::JumpIfFalse, 0);

                self.in_tail_position = is_tail;
                let then_ty = self.compile_expr(&ie.body);
                self.in_tail_position = false;

                let end_jump = self.current_addr();
                self.emit_arg(Op::Jump, 0);

                self.patch(else_jump, op_arg(Op::JumpIfFalse, self.current_addr()));

                if let Some(else_body) = &ie.else_body {
                    self.in_tail_position = is_tail;
                    let else_ty = self.compile_expr(else_body);
                    self.in_tail_position = false;
                    self.patch(end_jump, op_arg(Op::Jump, self.current_addr()));
                    return self.join_arms(vec![then_ty, else_ty], ie.span);
                }

                self.emit(Op::PushNone);
                self.patch(end_jump, op_arg(Op::Jump, self.current_addr()));
                self.engine.icon_none()
            }
            ast::Expression::MatchExpression(me) => self.compile_match(me, is_tail),
            ast::Expression::ArrayExpression(ae) => self.compile_array(ae),
            ast::Expression::TupleExpression(te) => {
                let mut elem_tys: Vec<InferType> = Vec::with_capacity(te.elements.len());
                for elem in &te.elements {
                    elem_tys.push(self.compile_expr(elem));
                }
                self.emit_arg(Op::MakeTuple, te.elements.len() as i32);
                InferType::ITuple { elements: elem_tys }
            }
            ast::Expression::ArrayIndexExpression(aie) => {
                let arr_ty = self.compile_expr(&aie.expression);
                let elem_var = self.engine.fresh_var();
                let arr_expected = self.engine.icon_array(elem_var.clone());
                self.engine
                    .unify(&arr_ty, &arr_expected, aie.expression.span());

                if let ast::Expression::RangeExpression(r) = aie.index.as_ref() {
                    let start_ty = self.compile_expr(&r.start);
                    let end_ty = self.compile_expr(&r.end);
                    let int_t = self.engine.icon_int();
                    self.engine.unify(&start_ty, &int_t, r.start.span());
                    self.engine.unify(&end_ty, &int_t, r.end.span());
                    self.emit(Op::ArraySlice);
                    self.engine.icon_array(elem_var)
                } else {
                    let idx_ty = self.compile_expr(&aie.index);
                    let int_t = self.engine.icon_int();
                    self.engine.unify(&idx_ty, &int_t, aie.index.span());
                    self.emit(Op::Index);
                    elem_var
                }
            }
            ast::Expression::RangeExpression(re) => {
                let start_ty = self.compile_expr(&re.start);
                let end_ty = self.compile_expr(&re.end);
                let int_t = self.engine.icon_int();
                self.engine.unify(&start_ty, &int_t, re.start.span());
                self.engine.unify(&end_ty, &int_t, re.end.span());
                self.emit(Op::MakeRange);
                self.engine.icon_array(self.engine.icon_int())
            }
            ast::Expression::FunctionExpression(fe) => {
                self.engine.enter_level();
                let ty = self.compile_function_common(
                    None,
                    &fe.params,
                    &fe.body,
                    fe.return_type.as_ref(),
                    fe.error_type.as_ref(),
                );
                self.engine.leave_level();
                ty
            }
            ast::Expression::FunctionCallExpression(fc) => self.compile_call(fc, is_tail),
            ast::Expression::PropertyAccessExpression(pa) => self.compile_property_access(pa),
            ast::Expression::StructInitExpression(si) => self.compile_struct_init(si),
            ast::Expression::ErrorExpression(ee) => {
                let inner_ty = self.compile_expr(&ee.expression);
                if let Some(fn_err) = self.current_fn_err.clone() {
                    self.engine.unify(&inner_ty, &fn_err, ee.expression.span());
                } else {
                    self.error(
                        "'error' expression outside of a failable function".to_string(),
                        ee.span,
                    );
                }
                self.emit(Op::MakeError);
                self.engine.fresh_var()
            }
            ast::Expression::OrExpression(oe) => self.compile_or(oe),
            ast::Expression::ErrorNode(_) => self.engine.fresh_var(),
            ast::Expression::OrPattern(_)
            | ast::Expression::WildcardPattern(_)
            | ast::Expression::TypeIdentifier(_) => {
                self.error(
                    "Pattern used in expression position".to_string(),
                    expr.span(),
                );
                self.engine.fresh_var()
            }
        }
    }

    // ========================================================================
    // Identifier
    // ========================================================================

    fn compile_identifier(&mut self, expr: &ast::Identifier) -> InferType {
        let name = &expr.name;

        if let Some(scheme) = self.env.lookup(name) {
            let ty = self.engine.instantiate(&scheme);
            let doc = self.env.lookup_doc(name);
            self.record(name, ty.clone(), expr.span, doc);

            if let Some(access) = self.resolve_variable(name) {
                self.emit_load(&access);
                return ty;
            }

            self.error(
                format!("'{}' is a builtin and cannot be used as a value", name),
                expr.span,
            );
            self.emit(Op::PushNone);
            return ty;
        }

        if let Some((enum_name, info)) = self.env.find_variant_owner(name) {
            let payload = info.variants.get(name).cloned().unwrap_or_default();
            if payload.is_empty() {
                let mut type_args: Vec<InferType> = Vec::with_capacity(info.type_params.len());
                for _ in 0..info.type_params.len() {
                    type_args.push(self.engine.fresh_var());
                }
                return self.emit_enum_construction(
                    &enum_name,
                    &info,
                    type_args,
                    name,
                    &[],
                    expr.span,
                );
            }
            self.error(
                format!(
                    "Variant '{}' requires {} argument(s); use {}(...)",
                    name,
                    payload.len(),
                    name
                ),
                expr.span,
            );
            self.emit(Op::PushNone);
            return self.engine.fresh_var();
        }

        if let Some(suggestion) = self.env.suggest_name(name) {
            self.error(
                format!(
                    "Unknown identifier '{}'. Did you mean '{}'?",
                    name, suggestion
                ),
                expr.span,
            );
        } else {
            self.error(format!("Unknown identifier '{}'", name), expr.span);
        }
        self.emit(Op::PushNone);
        self.engine.fresh_var()
    }

    // ========================================================================
    // Binary ops
    // ========================================================================

    fn compile_binary(&mut self, expr: &ast::BinaryExpression) -> InferType {
        if expr.op.kind == Kind::LogicalAnd {
            let l_ty = self.compile_expr(&expr.left);
            let b = self.engine.icon_bool();
            self.engine.unify(&l_ty, &b, expr.left.span());
            self.emit(Op::Dup);
            let end_jump = self.current_addr();
            self.emit_arg(Op::JumpIfFalse, 0);
            self.emit(Op::Pop);
            let r_ty = self.compile_expr(&expr.right);
            self.engine.unify(&r_ty, &b, expr.right.span());
            self.patch(end_jump, op_arg(Op::JumpIfFalse, self.current_addr()));
            return self.engine.icon_bool();
        }
        if expr.op.kind == Kind::LogicalOr {
            let l_ty = self.compile_expr(&expr.left);
            let b = self.engine.icon_bool();
            self.engine.unify(&l_ty, &b, expr.left.span());
            self.emit(Op::Dup);
            let end_jump = self.current_addr();
            self.emit_arg(Op::JumpIfTrue, 0);
            self.emit(Op::Pop);
            let r_ty = self.compile_expr(&expr.right);
            self.engine.unify(&r_ty, &b, expr.right.span());
            self.patch(end_jump, op_arg(Op::JumpIfTrue, self.current_addr()));
            return self.engine.icon_bool();
        }

        let l_ty = self.compile_expr(&expr.left);
        let r_ty = self.compile_expr(&expr.right);

        match expr.op.kind {
            Kind::PuncPlus => {
                let result = self.engine.fresh_constrained_var(Constraint::Addable);
                self.engine.unify(&l_ty, &result, expr.left.span());
                self.engine.unify(&r_ty, &result, expr.right.span());
                self.emit(Op::Add);
                result
            }
            Kind::PuncMinus | Kind::PuncMul | Kind::PuncDiv | Kind::PuncMod => {
                let result = self.engine.fresh_constrained_var(Constraint::Numeric);
                self.engine.unify(&l_ty, &result, expr.left.span());
                self.engine.unify(&r_ty, &result, expr.right.span());
                let opc = match expr.op.kind {
                    Kind::PuncMinus => Op::Sub,
                    Kind::PuncMul => Op::Mul,
                    Kind::PuncDiv => Op::Div,
                    Kind::PuncMod => Op::Mod,
                    _ => Op::Sub,
                };
                self.emit(opc);
                result
            }
            Kind::PuncLt | Kind::PuncGt | Kind::PuncLte | Kind::PuncGte => {
                let operand = self.engine.fresh_constrained_var(Constraint::Numeric);
                self.engine.unify(&l_ty, &operand, expr.left.span());
                self.engine.unify(&r_ty, &operand, expr.right.span());
                let opc = match expr.op.kind {
                    Kind::PuncLt => Op::Lt,
                    Kind::PuncGt => Op::Gt,
                    Kind::PuncLte => Op::Lte,
                    Kind::PuncGte => Op::Gte,
                    _ => Op::Lt,
                };
                self.emit(opc);
                self.engine.icon_bool()
            }
            Kind::PuncEqualsComparator | Kind::PuncNotEqual => {
                self.engine.unify(&l_ty, &r_ty, expr.span);
                if expr.op.kind == Kind::PuncEqualsComparator {
                    self.emit(Op::Eq);
                } else {
                    self.emit(Op::Neq);
                }
                self.engine.icon_bool()
            }
            _ => {
                self.error(
                    format!("Unknown binary operator: {}", expr.op.kind),
                    expr.span,
                );
                self.engine.fresh_var()
            }
        }
    }

    // ========================================================================
    // Array
    // ========================================================================

    fn compile_array(&mut self, expr: &ast::ArrayExpression) -> InferType {
        let elem_var = self.engine.fresh_var();
        let has_spread = expr
            .elements
            .iter()
            .any(|e| matches!(e, ast::ArrayElement::SpreadElement(_)));

        if !has_spread {
            for elem in &expr.elements {
                if let ast::ArrayElement::Expression(e) = elem {
                    let ty = self.compile_expr(e);
                    self.engine.unify(&ty, &elem_var, e.span());
                }
            }
            self.emit_arg(Op::MakeArray, expr.elements.len() as i32);
            return self.engine.icon_array(elem_var);
        }

        let mut have_result = false;
        let mut i = 0usize;
        while i < expr.elements.len() {
            let elem = &expr.elements[i];
            if let ast::ArrayElement::SpreadElement(spread) = elem {
                let inner = match &spread.expression {
                    Some(inner) => inner,
                    None => {
                        self.error(
                            "Spread in array literal missing expression".to_string(),
                            spread.span,
                        );
                        i += 1;
                        continue;
                    }
                };

                let spread_ty = self.compile_expr(inner);
                let expected = self.engine.icon_array(elem_var.clone());
                self.engine.unify(&spread_ty, &expected, inner.span());
                if have_result {
                    self.emit(Op::ArrayConcat);
                } else {
                    have_result = true;
                }
                i += 1;
            } else {
                let mut group_count = 0i32;
                let mut j = i;
                while j < expr.elements.len() {
                    if matches!(&expr.elements[j], ast::ArrayElement::SpreadElement(_)) {
                        break;
                    }
                    if let ast::ArrayElement::Expression(ae) = &expr.elements[j] {
                        let ty = self.compile_expr(ae);
                        self.engine.unify(&ty, &elem_var, ae.span());
                    }
                    group_count += 1;
                    j += 1;
                }
                self.emit_arg(Op::MakeArray, group_count);
                if have_result {
                    self.emit(Op::ArrayConcat);
                } else {
                    have_result = true;
                }
                i += group_count as usize;
            }
        }

        if !have_result {
            self.emit_arg(Op::MakeArray, 0);
        }
        self.engine.icon_array(elem_var)
    }

    // ========================================================================
    // Function call
    // ========================================================================

    fn compile_call(&mut self, expr: &ast::FunctionCallExpression, is_tail: bool) -> InferType {
        let name = expr.identifier.name.clone();

        if let Some(access) = self.resolve_variable(&name) {
            let scheme = match self.env.lookup(&name) {
                Some(s) => s,
                None => {
                    self.error(
                        format!(
                            "Internal: '{}' resolved as local but has no type scheme",
                            name
                        ),
                        expr.identifier.span,
                    );
                    return self.engine.fresh_var();
                }
            };
            let fn_ty = self.engine.instantiate(&scheme);
            let doc = self.env.lookup_doc(&name);
            self.record(&name, fn_ty.clone(), expr.identifier.span, doc);

            let ret_ty = self.call_against(&fn_ty, &expr.arguments, expr.span);

            self.emit_load(&access);
            if is_tail {
                self.emit_arg(Op::TailCall, expr.arguments.len() as i32);
            } else {
                self.emit_arg(Op::Call, expr.arguments.len() as i32);
            }

            return ret_ty;
        }

        if let Some((enum_name, info)) = self.env.find_variant_owner(&name) {
            let mut type_args: Vec<InferType> = Vec::with_capacity(info.type_params.len());
            for _ in 0..info.type_params.len() {
                type_args.push(self.engine.fresh_var());
            }
            return self.emit_enum_construction(
                &enum_name,
                &info,
                type_args,
                &name,
                &expr.arguments,
                expr.span,
            );
        }

        self.compile_builtin_call(expr)
    }

    fn call_against(
        &mut self,
        fn_ty: &InferType,
        args: &[ast::Expression],
        call_span: Span,
    ) -> InferType {
        let resolved = self.engine.find(fn_ty);

        if let InferType::IFun { params, ret, err } = &resolved {
            if params.len() != args.len() {
                self.error(
                    format!("Expected {} argument(s), got {}", params.len(), args.len()),
                    call_span,
                );
            }
            for (i, arg) in args.iter().enumerate() {
                let hint = if i < params.len() {
                    Some(params[i].clone())
                } else {
                    None
                };
                let arg_ty = self.compile_expr_with_hint(arg, hint);
                if i < params.len() {
                    self.engine.unify(&params[i], &arg_ty, arg.span());
                }
            }
            if let Some(e) = err {
                return self.engine.icon_result((**ret).clone(), (**e).clone());
            }
            return (**ret).clone();
        }

        let mut param_tys: Vec<InferType> = Vec::with_capacity(args.len());
        for arg in args {
            let arg_ty = self.compile_expr(arg);
            param_tys.push(arg_ty);
        }
        let ret_var = self.engine.fresh_var();
        self.engine.unify(
            fn_ty,
            &InferType::IFun {
                params: param_tys,
                ret: Box::new(ret_var.clone()),
                err: None,
            },
            call_span,
        );
        ret_var
    }

    fn compile_expr_with_hint(
        &mut self,
        expr: &ast::Expression,
        hint: Option<InferType>,
    ) -> InferType {
        let h = match hint {
            Some(h) => h,
            None => return self.compile_expr(expr),
        };
        let resolved = self.engine.find(&h);
        let (hint_name, hint_args) = match &resolved {
            InferType::ICon { name, args } => (name.clone(), args.clone()),
            _ => return self.compile_expr(expr),
        };
        let info = match self.env.lookup_type_info(&hint_name) {
            Some(info) => info,
            None => return self.compile_expr(expr),
        };
        let einfo = match info {
            TypeInfo::Enum(ei) => ei,
            _ => return self.compile_expr(expr),
        };

        let (variant_name, variant_args, variant_span): (String, Vec<ast::Expression>, Span) =
            match expr {
                ast::Expression::FunctionCallExpression(fc) => {
                    (fc.identifier.name.clone(), fc.arguments.clone(), fc.span)
                }
                ast::Expression::Identifier(id) => (id.name.clone(), vec![], id.span),
                _ => return self.compile_expr(expr),
            };

        if !einfo.variants.contains_key(&variant_name) {
            return self.compile_expr(expr);
        }

        self.emit_enum_construction(
            &hint_name,
            &einfo,
            hint_args,
            &variant_name,
            &variant_args,
            variant_span,
        )
    }

    // ========================================================================
    // Property access
    // ========================================================================

    fn compile_property_access(&mut self, expr: &ast::PropertyAccessExpression) -> InferType {
        if let ast::Expression::Identifier(left_id) = expr.left.as_ref()
            && let Some(info) = self.env.lookup_type_info(&left_id.name)
            && let TypeInfo::Enum(einfo) = info
        {
            let enum_name = left_id.name.clone();

            let (variant_name, args, variant_span): (String, Vec<ast::Expression>, Span) =
                match expr.right.as_ref() {
                    ast::Expression::FunctionCallExpression(call) => (
                        call.identifier.name.clone(),
                        call.arguments.clone(),
                        call.identifier.span,
                    ),
                    ast::Expression::Identifier(r) => (r.name.clone(), vec![], r.span),
                    _ => {
                        self.error("Invalid enum variant access".to_string(), expr.right.span());
                        return self.engine.fresh_var();
                    }
                };

            if !einfo.variants.contains_key(&variant_name) {
                self.error(
                    format!("Enum '{}' has no variant '{}'", enum_name, variant_name),
                    variant_span,
                );
                return self.engine.fresh_var();
            }

            let mut type_args: Vec<InferType> = Vec::with_capacity(einfo.type_params.len());
            for _ in 0..einfo.type_params.len() {
                type_args.push(self.engine.fresh_var());
            }

            let enum_ty = self.engine.icon(enum_name.clone(), type_args.clone());
            let doc = self.env.lookup_doc(&enum_name);
            self.record(&enum_name, enum_ty, left_id.span, doc);

            return self.emit_enum_construction(
                &enum_name,
                &einfo,
                type_args,
                &variant_name,
                &args,
                variant_span,
            );
        }

        let left_ty = self.compile_expr(&expr.left);

        if let ast::Expression::FunctionCallExpression(call) = expr.right.as_ref() {
            self.error(
                format!(
                    "Cannot call '{}' as a method. AL does not have methods — use '{}(...)' as a free function instead.",
                    call.identifier.name, call.identifier.name
                ),
                call.span,
            );
            for arg in &call.arguments {
                self.compile_expr(arg);
            }
            return self.engine.fresh_var();
        }

        if let ast::Expression::NumberLiteral(num) = expr.right.as_ref() {
            let index = num.value.parse::<i32>().unwrap_or(0);
            self.emit_arg(Op::TupleIndex, index);

            let resolved = self.engine.find(&left_ty);
            if let InferType::ITuple { elements } = &resolved {
                if (index as usize) < elements.len() {
                    return elements[index as usize].clone();
                }
                self.error(
                    format!(
                        "Tuple index {} out of bounds (tuple has {} elements)",
                        index,
                        elements.len()
                    ),
                    expr.right.span(),
                );
                return self.engine.fresh_var();
            }
            self.error(
                format!("Cannot index .{} on non-tuple type", index),
                expr.right.span(),
            );
            return self.engine.fresh_var();
        }

        if let ast::Expression::Identifier(field) = expr.right.as_ref() {
            let idx = self.add_constant(Value::Str(field.name.clone()));
            self.emit_arg(Op::GetField, idx);

            let resolved = self.engine.find(&left_ty);
            if let InferType::ICon { name, args } = &resolved
                && let Some(info) = self.env.lookup_type_info(name)
                && let TypeInfo::Struct(sinfo) = info
            {
                if let Some(field_tmpl) = sinfo.fields.get(&field.name) {
                    let mut vars: HashMap<String, InferType> = HashMap::new();
                    for (i, p) in sinfo.type_params.iter().enumerate() {
                        if i < args.len() {
                            vars.insert(p.clone(), args[i].clone());
                        }
                    }
                    let field_ty = self.engine.type_to_infer_with_vars(field_tmpl, &vars);
                    let qualified = format!("{}.{}", name, field.name);
                    let doc = self.env.lookup_doc(&qualified);
                    self.record(&qualified, field_ty.clone(), field.span, doc);
                    return field_ty;
                }
                let available: Vec<String> = sinfo.fields.keys().cloned().collect();
                self.error(
                    format!(
                        "Struct '{}' has no field '{}'. Available: {}",
                        name,
                        field.name,
                        available.join(", ")
                    ),
                    field.span,
                );
                return self.engine.fresh_var();
            }
            let ty_str = self.engine.type_to_str(&left_ty);
            self.error(
                format!("Cannot access field '{}' on type '{}'", field.name, ty_str),
                field.span,
            );
            return self.engine.fresh_var();
        }

        self.error("Invalid property access".to_string(), expr.right.span());
        self.engine.fresh_var()
    }

    fn emit_enum_construction(
        &mut self,
        enum_name: &str,
        info: &EnumInfo,
        type_args: Vec<InferType>,
        variant_name: &str,
        args: &[ast::Expression],
        variant_span: Span,
    ) -> InferType {
        let payload_templates = info.variants.get(variant_name).cloned().unwrap_or_default();

        let mut vars: HashMap<String, InferType> = HashMap::new();
        for (i, p) in info.type_params.iter().enumerate() {
            if i < type_args.len() {
                vars.insert(p.clone(), type_args[i].clone());
            }
        }

        if !payload_templates.is_empty() {
            if args.len() != payload_templates.len() {
                self.error(
                    format!(
                        "Variant '{}' expects {} argument(s), got {}",
                        variant_name,
                        payload_templates.len(),
                        args.len()
                    ),
                    variant_span,
                );
            }
            let id_const = self.add_constant(Value::Int(info.id as i64));
            self.emit_arg(Op::PushConst, id_const);
            let en_const = self.add_constant(Value::Str(enum_name.to_string()));
            self.emit_arg(Op::PushConst, en_const);
            let vn_const = self.add_constant(Value::Str(variant_name.to_string()));
            self.emit_arg(Op::PushConst, vn_const);
            for (i, arg) in args.iter().enumerate() {
                let arg_ty = self.compile_expr(arg);
                if i < payload_templates.len() {
                    let expected = self
                        .engine
                        .type_to_infer_with_vars(&payload_templates[i], &vars);
                    self.engine.unify(&expected, &arg_ty, arg.span());
                }
            }
            self.emit_arg(Op::MakeEnumPayload, args.len() as i32);
        } else {
            if !args.is_empty() {
                self.error(
                    format!("Variant '{}' takes no arguments", variant_name),
                    variant_span,
                );
            }
            let id_const = self.add_constant(Value::Int(info.id as i64));
            self.emit_arg(Op::PushConst, id_const);
            let en_const = self.add_constant(Value::Str(enum_name.to_string()));
            self.emit_arg(Op::PushConst, en_const);
            let vn_const = self.add_constant(Value::Str(variant_name.to_string()));
            self.emit_arg(Op::PushConst, vn_const);
            self.emit(Op::MakeEnum);
        }

        let result_ty = self.engine.icon(enum_name.to_string(), type_args);
        let qualified = format!("{}.{}", enum_name, variant_name);
        let doc = self.env.lookup_doc(&qualified);
        self.record(&qualified, result_ty.clone(), variant_span, doc);

        result_ty
    }

    // ========================================================================
    // Struct init
    // ========================================================================

    fn compile_struct_init(&mut self, expr: &ast::StructInitExpression) -> InferType {
        let struct_name = expr.identifier.name.clone();
        let info = match self.env.lookup_type_info(&struct_name) {
            Some(i) => i,
            None => {
                self.error(
                    format!("Unknown struct '{}'", struct_name),
                    expr.identifier.span,
                );
                for field in &expr.fields {
                    self.compile_expr(&field.init);
                }
                return self.engine.fresh_var();
            }
        };
        let sinfo = match info {
            TypeInfo::Struct(s) => s,
            _ => {
                self.error(
                    format!("'{}' is not a struct", struct_name),
                    expr.identifier.span,
                );
                return self.engine.fresh_var();
            }
        };

        let mut type_args: Vec<InferType> = Vec::with_capacity(sinfo.type_params.len());
        if !expr.type_args.is_empty() {
            if expr.type_args.len() != sinfo.type_params.len() {
                self.error(
                    format!(
                        "Struct '{}' expects {} type argument(s), got {}",
                        struct_name,
                        sinfo.type_params.len(),
                        expr.type_args.len()
                    ),
                    expr.identifier.span,
                );
            }
            for ta in &expr.type_args {
                type_args.push(self.resolve_type(ta, &HashMap::new()));
            }
            while type_args.len() < sinfo.type_params.len() {
                type_args.push(self.engine.fresh_var());
            }
        } else {
            for _ in 0..sinfo.type_params.len() {
                type_args.push(self.engine.fresh_var());
            }
        }

        let mut vars: HashMap<String, InferType> = HashMap::new();
        for (i, p) in sinfo.type_params.iter().enumerate() {
            vars.insert(p.clone(), type_args[i].clone());
        }

        let mut provided: HashMap<String, bool> = HashMap::new();
        for field in &expr.fields {
            let fname = field.identifier.name.clone();
            if provided.contains_key(&fname) {
                self.error(
                    format!("Duplicate field '{}'", fname),
                    field.identifier.span,
                );
            }
            provided.insert(fname.clone(), true);

            let name_idx = self.add_constant(Value::Str(fname.clone()));
            self.emit_arg(Op::PushConst, name_idx);
            let init_ty = self.compile_expr(&field.init);

            if let Some(field_tmpl) = sinfo.fields.get(&fname) {
                let expected = self.engine.type_to_infer_with_vars(field_tmpl, &vars);
                self.engine.unify(&expected, &init_ty, field.init.span());
                let qualified = format!("{}.{}", struct_name, fname);
                let doc = self.env.lookup_doc(&qualified);
                self.record(&qualified, expected, field.identifier.span, doc);
            } else {
                let available: Vec<String> = sinfo.fields.keys().cloned().collect();
                self.error(
                    format!(
                        "Struct '{}' has no field '{}'. Available: {}",
                        struct_name,
                        fname,
                        available.join(", ")
                    ),
                    field.identifier.span,
                );
            }
        }

        let mut missing: Vec<String> = vec![];
        for fname in sinfo.fields.keys() {
            if !provided.contains_key(fname) {
                missing.push(fname.clone());
            }
        }
        if !missing.is_empty() {
            self.error(
                format!(
                    "Missing required fields in '{}': {}",
                    struct_name,
                    missing.join(", ")
                ),
                expr.identifier.span,
            );
        }

        let id_const = self.add_constant(Value::Int(sinfo.id as i64));
        self.emit_arg(Op::PushConst, id_const);
        let name_const = self.add_constant(Value::Str(struct_name.clone()));
        self.emit_arg(Op::PushConst, name_const);
        self.emit_arg(Op::MakeStruct, expr.fields.len() as i32);

        let result_ty = self.engine.icon(struct_name.clone(), type_args);
        let doc = self.env.lookup_doc(&struct_name);
        self.record(&struct_name, result_ty.clone(), expr.identifier.span, doc);

        result_ty
    }

    // ========================================================================
    // Or expression (Result/Option unwrapping)
    // ========================================================================

    fn compile_or(&mut self, expr: &ast::OrExpression) -> InferType {
        let left_ty = self.compile_expr(&expr.expression);

        let success_var = self.engine.fresh_var();
        let err_var = self.engine.fresh_var();

        let resolved = self.engine.find(&left_ty);
        let is_option = matches!(&resolved, InferType::ICon { name, .. } if name == "Option");

        if is_option {
            let opt = self.engine.icon_option(success_var.clone());
            self.engine.unify(&left_ty, &opt, expr.expression.span());
        } else {
            let res = self
                .engine
                .icon_result(success_var.clone(), err_var.clone());
            self.engine.unify(&left_ty, &res, expr.expression.span());
        }

        self.emit(Op::Dup);
        self.emit(Op::IsFailure);
        let not_failure_jump = self.current_addr();
        self.emit_arg(Op::JumpIfFalse, 0);

        self.emit(Op::UnwrapFailure);
        self.env.push_scope();
        if let Some(receiver) = &expr.receiver {
            let idx = self.get_or_create_local(&receiver.name);
            self.emit_arg(Op::StoreLocal, idx);
            let receiver_ty = if is_option {
                self.engine.icon_none()
            } else {
                err_var.clone()
            };
            self.env.define_at(
                &receiver.name,
                mono(receiver_ty.clone()),
                def_loc(receiver.span),
            );
            self.record(&receiver.name, receiver_ty, receiver.span, None);
        } else {
            self.emit(Op::Pop);
        }

        let body_ty = self.compile_expr(&expr.body);
        self.engine.unify(&body_ty, &success_var, expr.body.span());
        self.env.pop_scope();

        let end_jump = self.current_addr();
        self.emit_arg(Op::Jump, 0);

        self.patch(
            not_failure_jump,
            op_arg(Op::JumpIfFalse, self.current_addr()),
        );
        self.patch(end_jump, op_arg(Op::Jump, self.current_addr()));

        success_var
    }

    // ========================================================================
    // Functions
    // ========================================================================

    fn compile_function_common(
        &mut self,
        name: Option<String>,
        params: &[ast::FunctionParameter],
        body: &ast::Expression,
        return_annot: Option<&ast::TypeIdentifier>,
        error_annot: Option<&ast::TypeIdentifier>,
    ) -> InferType {
        let old_locals = self.locals.clone();
        let old_local_count = self.local_count;
        let old_captures = self.captures.clone();
        let old_capture_names = self.capture_names.clone();

        let mut scope_locals = old_locals.clone();
        if let Some(n) = &name {
            scope_locals.insert(n.clone(), self.local_count);
        }
        self.outer_scopes.push(Scope {
            locals: scope_locals,
        });

        let jump_over = self.current_addr();
        self.emit_arg(Op::Jump, 0);

        self.locals = HashMap::new();
        self.local_count = 0;
        self.captures = HashMap::new();
        self.capture_names = vec![];

        self.env.push_scope();

        let annot_param_vars: HashMap<String, InferType> = HashMap::new();
        let mut param_tys: Vec<InferType> = Vec::with_capacity(params.len());
        for param in params {
            let p_ty = if let Some(annot) = &param.typ {
                self.resolve_type(annot, &annot_param_vars)
            } else {
                self.engine.fresh_var()
            };
            param_tys.push(p_ty.clone());
            self.get_or_create_local(&param.identifier.name);
            self.env.define_at(
                &param.identifier.name,
                mono(p_ty.clone()),
                def_loc(param.identifier.span),
            );
            self.record(&param.identifier.name, p_ty, param.identifier.span, None);
        }

        let old_fn_err = self.current_fn_err.clone();
        let declared_err = error_annot.map(|et| self.resolve_type(et, &annot_param_vars));
        self.current_fn_err = declared_err.clone();

        let func_start = self.current_addr();

        let old_binding = self.current_binding.clone();
        if let Some(n) = &name {
            self.current_binding = n.clone();
        }

        let old_tail = self.in_tail_position;
        self.in_tail_position = true;
        let body_ty = self.compile_expr(body);
        self.in_tail_position = old_tail;
        self.current_binding = old_binding;
        self.emit(Op::Ret);

        let ret_ty = if let Some(rt) = return_annot {
            let annot_ty = self.resolve_type(rt, &annot_param_vars);
            let resolved_annot = self.engine.find(&annot_ty);
            if let InferType::ICon {
                name: an,
                args: aargs,
            } = &resolved_annot
            {
                if an == "Option" {
                    let resolved_body = self.engine.find(&body_ty);
                    match &resolved_body {
                        InferType::ICon { name: bn, .. } if bn == "None" => {}
                        InferType::ICon { name: bn, .. } if bn == "Option" => {
                            self.engine.unify(&annot_ty, &body_ty, body.span());
                        }
                        _ => {
                            if !aargs.is_empty() {
                                self.engine.unify(&aargs[0], &body_ty, body.span());
                            }
                        }
                    }
                } else {
                    self.engine.unify(&annot_ty, &body_ty, body.span());
                }
            } else {
                self.engine.unify(&annot_ty, &body_ty, body.span());
            }
            annot_ty
        } else {
            body_ty
        };

        self.current_fn_err = old_fn_err;
        self.env.pop_scope();

        self.patch(jump_over, op_arg(Op::Jump, self.current_addr()));

        let captured_names = self.capture_names.clone();
        let capture_count = captured_names.len() as i32;

        if !self.check_only {
            self.program.functions.push(Function {
                name: name.clone().unwrap_or_else(|| "__anon__".to_string()),
                arity: params.len() as i32,
                locals: self.local_count,
                capture_count,
                code_start: func_start,
                code_len: self.current_addr() - func_start - 1,
            });
        }
        let func_idx = self.program.functions.len() as i32 - 1;

        self.outer_scopes.pop();
        self.locals = old_locals.clone();
        self.local_count = old_local_count;
        self.captures = old_captures.clone();
        self.capture_names = old_capture_names;

        for cap_name in &captured_names {
            if let Some(access) = self.resolve_variable(cap_name) {
                self.emit_load(&access);
            }
        }
        self.emit_arg(Op::MakeClosure, func_idx);

        InferType::IFun {
            params: param_tys,
            ret: Box::new(ret_ty),
            err: declared_err.map(Box::new),
        }
    }

    // join_arms computes the common supertype of a set of branch types, handling
    // AL's implicit Option lifting: None + T → Option(T). Unlike unify, this does
    // NOT force branches to have the same type — it finds a type that all branches
    // are compatible with.
    fn join_arms(&mut self, arms: Vec<InferType>, sp: Span) -> InferType {
        if arms.is_empty() {
            return self.engine.icon_none();
        }

        let is_none = |c: &mut Compiler, t: &InferType| -> bool {
            let r = c.engine.find(t);
            matches!(&r, InferType::ICon { name, .. } if name == "None")
        };
        let is_option = |c: &mut Compiler, t: &InferType| -> Option<InferType> {
            let r = c.engine.find(t);
            if let InferType::ICon { name, args } = &r
                && name == "Option"
                && !args.is_empty()
            {
                return Some(args[0].clone());
            }
            None
        };

        let mut has_none = false;
        let mut non_none: Vec<InferType> = vec![];
        for a in &arms {
            if is_none(self, a) {
                has_none = true;
            } else {
                non_none.push(a.clone());
            }
        }

        if non_none.is_empty() {
            return self.engine.icon_none();
        }

        if has_none {
            let mut inners: Vec<InferType> = Vec::with_capacity(non_none.len());
            for a in &non_none {
                if let Some(inner) = is_option(self, a) {
                    inners.push(inner);
                } else {
                    inners.push(a.clone());
                }
            }
            let result = inners[0].clone();
            for inner in inners.iter().skip(1) {
                self.engine.unify(&result, inner, sp);
            }
            return self.engine.icon_option(result);
        }

        let result = non_none[0].clone();
        for ty in non_none.iter().skip(1) {
            self.engine.unify(&result, ty, sp);
        }
        result
    }

    // ========================================================================
    // Pattern matching
    // ========================================================================

    fn compile_match(&mut self, m: &ast::MatchExpression, is_tail: bool) -> InferType {
        let subject_ty = self.compile_expr(&m.subject);
        let mut arm_tys: Vec<InferType> = vec![];

        let mut end_jumps: Vec<i32> = vec![];
        let mut pats_for_exhaustiveness: Vec<Pat> = vec![];

        for arm in &m.arms {
            self.emit(Op::Dup);
            let mut fail_jumps: Vec<i32> = vec![];

            self.env.push_scope();

            if let Some(detected) = self.detect_enum_pattern(&arm.pattern, &subject_ty) {
                self.compile_enum_arm(
                    m,
                    arm,
                    &detected,
                    &subject_ty,
                    is_tail,
                    &mut arm_tys,
                    &mut fail_jumps,
                    &mut end_jumps,
                    &mut pats_for_exhaustiveness,
                );
                self.env.pop_scope();
                continue;
            }

            if let ast::Expression::OrPattern(op) = &arm.pattern {
                let mut body_jumps: Vec<i32> = vec![];
                for (i, pattern) in op.patterns.iter().enumerate() {
                    if i > 0 {
                        self.emit(Op::Dup);
                    }
                    let mut pattern_fails: Vec<i32> = vec![];
                    self.compile_pattern(pattern, &subject_ty, &mut pattern_fails);
                    if i < op.patterns.len() - 1 {
                        body_jumps.push(self.current_addr());
                        self.emit_arg(Op::Jump, 0);
                        for ja in &pattern_fails {
                            self.patch(*ja, op_arg(Op::JumpIfFalse, self.current_addr()));
                        }
                    } else {
                        fail_jumps.extend(pattern_fails);
                    }
                }
                for ja in &body_jumps {
                    self.patch(*ja, op_arg(Op::Jump, self.current_addr()));
                }
                self.emit(Op::Pop);
                self.in_tail_position = is_tail;
                let body_ty = self.compile_expr(&arm.body);
                self.in_tail_position = false;
                arm_tys.push(body_ty);

                end_jumps.push(self.current_addr());
                self.emit_arg(Op::Jump, 0);
                for ja in &fail_jumps {
                    self.patch(*ja, op_arg(Op::JumpIfFalse, self.current_addr()));
                }

                let resolved_subj = self.engine.resolve(&subject_ty, Some(&self.env));
                pats_for_exhaustiveness.push(ast_pattern_to_pat(&arm.pattern, &resolved_subj));
                self.env.pop_scope();
                continue;
            }

            self.compile_pattern(&arm.pattern, &subject_ty, &mut fail_jumps);

            self.emit(Op::Pop);
            self.in_tail_position = is_tail;
            let body_ty = self.compile_expr(&arm.body);
            self.in_tail_position = false;
            arm_tys.push(body_ty);

            end_jumps.push(self.current_addr());
            self.emit_arg(Op::Jump, 0);
            for ja in &fail_jumps {
                self.patch(*ja, op_arg(Op::JumpIfFalse, self.current_addr()));
            }

            let resolved_subj = self.engine.resolve(&subject_ty, Some(&self.env));
            pats_for_exhaustiveness.push(ast_pattern_to_pat(&arm.pattern, &resolved_subj));
            self.env.pop_scope();
        }

        self.emit(Op::Pop);
        self.emit(Op::PushNone);

        for ja in &end_jumps {
            self.patch(*ja, op_arg(Op::Jump, self.current_addr()));
        }

        let resolved_subj = self.engine.resolve(&subject_ty, Some(&self.env));
        if let Some(missing) = check_exhaustiveness(&pats_for_exhaustiveness, &resolved_subj) {
            self.error(
                format!("Match is not exhaustive, missing: {}", missing),
                m.subject.span(),
            );
        }

        self.join_arms(arm_tys, m.span)
    }

    fn detect_enum_pattern(
        &mut self,
        pattern: &ast::Expression,
        subject_ty: &InferType,
    ) -> Option<EnumPatternInfo> {
        let (enum_name, info, variant_name, payload_args, pattern_span) = match pattern {
            ast::Expression::PropertyAccessExpression(pa) => {
                let left_name = match pa.left.as_ref() {
                    ast::Expression::Identifier(id) => id.name.clone(),
                    _ => return None,
                };
                let found = self.env.lookup_type_info(&left_name)?;
                let efound = match found {
                    TypeInfo::Enum(e) => e,
                    _ => return None,
                };
                let (vn, args, ps) = extract_variant_parts(&pa.right)?;
                if !efound.variants.contains_key(&vn) {
                    return None;
                }
                (left_name, efound, vn, args, ps)
            }
            ast::Expression::FunctionCallExpression(_) | ast::Expression::Identifier(_) => {
                let resolved = self.engine.find(subject_ty);
                let subj_name = match &resolved {
                    InferType::ICon { name, .. } => name.clone(),
                    _ => return None,
                };
                let found = self.env.lookup_type_info(&subj_name)?;
                let efound = match found {
                    TypeInfo::Enum(e) => e,
                    _ => return None,
                };
                let (vn, args, ps) = extract_variant_parts(pattern)?;
                if !efound.variants.contains_key(&vn) {
                    return None;
                }
                (subj_name, efound, vn, args, ps)
            }
            _ => return None,
        };

        Some(EnumPatternInfo {
            enum_name,
            info,
            variant_name,
            payload_args,
            pattern_span,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn compile_enum_arm(
        &mut self,
        m: &ast::MatchExpression,
        arm: &ast::MatchArm,
        d: &EnumPatternInfo,
        subject_ty: &InferType,
        is_tail: bool,
        arm_tys: &mut Vec<InferType>,
        fail_jumps: &mut Vec<i32>,
        end_jumps: &mut Vec<i32>,
        pats: &mut Vec<Pat>,
    ) {
        let info = &d.info;
        let enum_name = &d.enum_name;
        let variant_name = &d.variant_name;

        let mut type_args: Vec<InferType> = Vec::with_capacity(info.type_params.len());
        for _ in 0..info.type_params.len() {
            type_args.push(self.engine.fresh_var());
        }
        let expected = self.engine.icon(enum_name.clone(), type_args.clone());
        self.engine.unify(subject_ty, &expected, m.subject.span());

        let mut vars: HashMap<String, InferType> = HashMap::new();
        for (i, p) in info.type_params.iter().enumerate() {
            vars.insert(p.clone(), type_args[i].clone());
        }

        let payload_templates = info.variants.get(variant_name).cloned().unwrap_or_default();

        let enum_ty = self.engine.icon(enum_name.clone(), type_args);
        let qualified = format!("{}.{}", enum_name, variant_name);
        let doc = self.env.lookup_doc(&qualified);
        self.record(&qualified, enum_ty, d.pattern_span, doc);

        let id_const = self.add_constant(Value::Int(info.id as i64));
        self.emit_arg(Op::PushConst, id_const);
        let en_const = self.add_constant(Value::Str(enum_name.clone()));
        self.emit_arg(Op::PushConst, en_const);
        let vn_const = self.add_constant(Value::Str(variant_name.clone()));
        self.emit_arg(Op::PushConst, vn_const);
        self.emit(Op::MatchEnum);
        fail_jumps.push(self.current_addr());
        self.emit_arg(Op::JumpIfFalse, 0);

        if !d.payload_args.is_empty() {
            if d.payload_args.len() != payload_templates.len() {
                self.error(
                    format!(
                        "Variant '{}' has {} payload field(s), pattern has {}",
                        variant_name,
                        payload_templates.len(),
                        d.payload_args.len()
                    ),
                    d.pattern_span,
                );
            }
            self.emit(Op::Dup);
            self.emit(Op::UnwrapEnum);
            // unwrap_enum pushes payloads in order (last on top). Stash each into a
            // temp local so failed pattern elements leave a clean stack for the next
            // arm's dup to work against.
            let n = d.payload_args.len() as i32;
            let temp_base = self.local_count;
            self.local_count += n;
            let mut i = n - 1;
            while i >= 0 {
                self.emit_arg(Op::StoreLocal, temp_base + i);
                i -= 1;
            }
            for (i, arg) in d.payload_args.iter().enumerate() {
                let arg_ty = if i < payload_templates.len() {
                    self.engine
                        .type_to_infer_with_vars(&payload_templates[i], &vars)
                } else {
                    self.engine.fresh_var()
                };
                self.emit_arg(Op::PushLocal, temp_base + i as i32);
                self.compile_pattern_element(arg, &arg_ty, fail_jumps);
            }
        }

        self.emit(Op::Pop);
        self.in_tail_position = is_tail;
        let body_ty = self.compile_expr(&arm.body);
        self.in_tail_position = false;
        arm_tys.push(body_ty);

        end_jumps.push(self.current_addr());
        self.emit_arg(Op::Jump, 0);

        for ja in fail_jumps.iter() {
            self.patch(*ja, op_arg(Op::JumpIfFalse, self.current_addr()));
        }

        // Build the exhaustiveness Pat directly from the detected variant name.
        // ast_pattern_to_pat can't be used here: for bare patterns arm.pattern is
        // a raw Identifier/FunctionCallExpression which would become PatWildcard.
        let mut arg_pats: Vec<Pat> = Vec::with_capacity(d.payload_args.len());
        for (i, arg) in d.payload_args.iter().enumerate() {
            let arg_type = if i < payload_templates.len() {
                payload_templates[i].clone()
            } else {
                Type::None
            };
            arg_pats.push(ast_pattern_to_pat(arg, &arg_type));
        }
        pats.push(Pat::Ctor {
            name: variant_name.clone(),
            args: arg_pats,
        });
    }

    fn compile_pattern(
        &mut self,
        pattern: &ast::Expression,
        subject_ty: &InferType,
        fail_jumps: &mut Vec<i32>,
    ) {
        match pattern {
            ast::Expression::NumberLiteral(_) => {
                self.emit(Op::Dup);
                let lit_ty = self.compile_expr(pattern);
                self.engine.unify(subject_ty, &lit_ty, pattern.span());
                self.emit(Op::Eq);
                fail_jumps.push(self.current_addr());
                self.emit_arg(Op::JumpIfFalse, 0);
            }
            ast::Expression::StringLiteral(sl) => {
                self.emit(Op::Dup);
                self.compile_expr(pattern);
                let s = self.engine.icon_string();
                self.engine.unify(subject_ty, &s, sl.span);
                self.emit(Op::Eq);
                fail_jumps.push(self.current_addr());
                self.emit_arg(Op::JumpIfFalse, 0);
            }
            ast::Expression::BooleanLiteral(bl) => {
                self.emit(Op::Dup);
                self.compile_expr(pattern);
                let b = self.engine.icon_bool();
                self.engine.unify(subject_ty, &b, bl.span);
                self.emit(Op::Eq);
                fail_jumps.push(self.current_addr());
                self.emit_arg(Op::JumpIfFalse, 0);
            }
            ast::Expression::Identifier(id) => {
                self.emit(Op::Dup);
                let local_idx = self.get_or_create_local(&id.name);
                self.emit_arg(Op::StoreLocal, local_idx);
                self.env
                    .define_at(&id.name, mono(subject_ty.clone()), def_loc(id.span));
                self.record(&id.name, subject_ty.clone(), id.span, None);
            }
            ast::Expression::WildcardPattern(_) => {}
            ast::Expression::RangeExpression(re) => {
                let int_t = self.engine.icon_int();
                self.engine.unify(subject_ty, &int_t, re.span);
                self.emit(Op::Dup);
                self.compile_expr(&re.start);
                self.emit(Op::Gte);
                fail_jumps.push(self.current_addr());
                self.emit_arg(Op::JumpIfFalse, 0);
                self.emit(Op::Dup);
                self.compile_expr(&re.end);
                self.emit(Op::Lt);
                fail_jumps.push(self.current_addr());
                self.emit_arg(Op::JumpIfFalse, 0);
            }
            ast::Expression::TupleExpression(te) => {
                let mut elem_vars: Vec<InferType> = Vec::with_capacity(te.elements.len());
                for _ in 0..te.elements.len() {
                    elem_vars.push(self.engine.fresh_var());
                }
                let tuple_ty = InferType::ITuple {
                    elements: elem_vars.clone(),
                };
                self.engine.unify(subject_ty, &tuple_ty, te.span);

                self.emit(Op::Dup);
                self.emit(Op::ArrayLen);
                let len_const = self.add_constant(Value::Int(te.elements.len() as i64));
                self.emit_arg(Op::PushConst, len_const);
                self.emit(Op::Eq);
                fail_jumps.push(self.current_addr());
                self.emit_arg(Op::JumpIfFalse, 0);

                for (i, elem) in te.elements.iter().enumerate() {
                    self.emit(Op::Dup);
                    self.emit_arg(Op::TupleIndex, i as i32);
                    self.compile_pattern_element(elem, &elem_vars[i], fail_jumps);
                }
            }
            ast::Expression::ArrayExpression(ae) => {
                let elem_var = self.engine.fresh_var();
                let arr_ty = self.engine.icon_array(elem_var.clone());
                self.engine.unify(subject_ty, &arr_ty, ae.span);

                let has_spread = !ae.elements.is_empty()
                    && matches!(
                        ae.elements.last(),
                        Some(ast::ArrayElement::SpreadElement(_))
                    );
                let pre_count = if has_spread {
                    ae.elements.len() - 1
                } else {
                    ae.elements.len()
                };

                self.emit(Op::Dup);
                self.emit(Op::ArrayLen);
                let pc_const = self.add_constant(Value::Int(pre_count as i64));
                self.emit_arg(Op::PushConst, pc_const);
                if has_spread {
                    self.emit(Op::Gte);
                } else {
                    self.emit(Op::Eq);
                }
                fail_jumps.push(self.current_addr());
                self.emit_arg(Op::JumpIfFalse, 0);

                for i in 0..pre_count {
                    let elem = &ae.elements[i];
                    self.emit(Op::Dup);
                    let i_const = self.add_constant(Value::Int(i as i64));
                    self.emit_arg(Op::PushConst, i_const);
                    self.emit(Op::Index);
                    if let ast::ArrayElement::Expression(e) = elem {
                        self.compile_pattern_element(e, &elem_var, fail_jumps);
                    }
                }

                if has_spread
                    && let Some(ast::ArrayElement::SpreadElement(spread)) = ae.elements.last()
                    && let Some(inner) = &spread.expression
                    && let ast::Expression::Identifier(id) = inner
                {
                    self.emit(Op::Dup);
                    self.emit(Op::ArrayLen);
                    let pc2 = self.add_constant(Value::Int(pre_count as i64));
                    self.emit_arg(Op::PushConst, pc2);
                    self.emit(Op::Swap);
                    self.emit(Op::ArraySlice);
                    let local_idx = self.get_or_create_local(&id.name);
                    self.emit_arg(Op::StoreLocal, local_idx);
                    let rest_ty = self.engine.icon_array(elem_var.clone());
                    self.env
                        .define_at(&id.name, mono(rest_ty.clone()), def_loc(id.span));
                    self.record(&id.name, rest_ty, id.span, None);
                }
            }
            _ => {
                self.emit(Op::Dup);
                let pat_ty = self.compile_expr(pattern);
                self.engine.unify(subject_ty, &pat_ty, pattern.span());
                self.emit(Op::Eq);
                fail_jumps.push(self.current_addr());
                self.emit_arg(Op::JumpIfFalse, 0);
            }
        }
    }

    fn compile_pattern_element(
        &mut self,
        pattern: &ast::Expression,
        subject_ty: &InferType,
        fail_jumps: &mut Vec<i32>,
    ) {
        match pattern {
            ast::Expression::NumberLiteral(_)
            | ast::Expression::StringLiteral(_)
            | ast::Expression::BooleanLiteral(_) => {
                let lit_ty = self.compile_expr(pattern);
                self.engine.unify(subject_ty, &lit_ty, pattern.span());
                self.emit(Op::Eq);
                fail_jumps.push(self.current_addr());
                self.emit_arg(Op::JumpIfFalse, 0);
            }
            ast::Expression::Identifier(id) => {
                let local_idx = self.get_or_create_local(&id.name);
                self.emit_arg(Op::StoreLocal, local_idx);
                self.env
                    .define_at(&id.name, mono(subject_ty.clone()), def_loc(id.span));
                self.record(&id.name, subject_ty.clone(), id.span, None);
            }
            ast::Expression::WildcardPattern(_) => {
                self.emit(Op::Pop);
            }
            ast::Expression::RangeExpression(re) => {
                let int_t = self.engine.icon_int();
                self.engine.unify(subject_ty, &int_t, re.span);
                let temp_idx = self.local_count;
                self.local_count += 1;
                self.emit(Op::Dup);
                self.emit_arg(Op::StoreLocal, temp_idx);
                self.compile_expr(&re.start);
                self.emit(Op::Gte);
                fail_jumps.push(self.current_addr());
                self.emit_arg(Op::JumpIfFalse, 0);
                self.emit_arg(Op::PushLocal, temp_idx);
                self.compile_expr(&re.end);
                self.emit(Op::Lt);
                fail_jumps.push(self.current_addr());
                self.emit_arg(Op::JumpIfFalse, 0);
            }
            ast::Expression::TupleExpression(te) => {
                let mut elem_vars: Vec<InferType> = Vec::with_capacity(te.elements.len());
                for _ in 0..te.elements.len() {
                    elem_vars.push(self.engine.fresh_var());
                }
                let tuple_ty = InferType::ITuple {
                    elements: elem_vars.clone(),
                };
                self.engine.unify(subject_ty, &tuple_ty, te.span);

                let temp_idx = self.local_count;
                self.local_count += 1;
                self.emit_arg(Op::StoreLocal, temp_idx);

                self.emit_arg(Op::PushLocal, temp_idx);
                self.emit(Op::ArrayLen);
                let len_const = self.add_constant(Value::Int(te.elements.len() as i64));
                self.emit_arg(Op::PushConst, len_const);
                self.emit(Op::Eq);
                fail_jumps.push(self.current_addr());
                self.emit_arg(Op::JumpIfFalse, 0);

                for (i, elem) in te.elements.iter().enumerate() {
                    self.emit_arg(Op::PushLocal, temp_idx);
                    self.emit_arg(Op::TupleIndex, i as i32);
                    self.compile_pattern_element(elem, &elem_vars[i], fail_jumps);
                }
            }
            ast::Expression::ArrayExpression(ae) => {
                let elem_var = self.engine.fresh_var();
                let arr_ty = self.engine.icon_array(elem_var.clone());
                self.engine.unify(subject_ty, &arr_ty, ae.span);

                let has_spread = !ae.elements.is_empty()
                    && matches!(
                        ae.elements.last(),
                        Some(ast::ArrayElement::SpreadElement(_))
                    );
                let pre_count = if has_spread {
                    ae.elements.len() - 1
                } else {
                    ae.elements.len()
                };

                let temp_idx = self.local_count;
                self.local_count += 1;
                self.emit_arg(Op::StoreLocal, temp_idx);

                self.emit_arg(Op::PushLocal, temp_idx);
                self.emit(Op::ArrayLen);
                let pc_const = self.add_constant(Value::Int(pre_count as i64));
                self.emit_arg(Op::PushConst, pc_const);
                if has_spread {
                    self.emit(Op::Gte);
                } else {
                    self.emit(Op::Eq);
                }
                fail_jumps.push(self.current_addr());
                self.emit_arg(Op::JumpIfFalse, 0);

                for i in 0..pre_count {
                    let elem = &ae.elements[i];
                    self.emit_arg(Op::PushLocal, temp_idx);
                    let i_const = self.add_constant(Value::Int(i as i64));
                    self.emit_arg(Op::PushConst, i_const);
                    self.emit(Op::Index);
                    if let ast::ArrayElement::Expression(e) = elem {
                        self.compile_pattern_element(e, &elem_var, fail_jumps);
                    }
                }

                if has_spread
                    && let Some(ast::ArrayElement::SpreadElement(spread)) = ae.elements.last()
                    && let Some(inner) = &spread.expression
                    && let ast::Expression::Identifier(id) = inner
                {
                    self.emit_arg(Op::PushLocal, temp_idx);
                    let pc2 = self.add_constant(Value::Int(pre_count as i64));
                    self.emit_arg(Op::PushConst, pc2);
                    self.emit_arg(Op::PushLocal, temp_idx);
                    self.emit(Op::ArrayLen);
                    self.emit(Op::ArraySlice);
                    let local_idx = self.get_or_create_local(&id.name);
                    self.emit_arg(Op::StoreLocal, local_idx);
                    let rest_ty = self.engine.icon_array(elem_var.clone());
                    self.env
                        .define_at(&id.name, mono(rest_ty.clone()), def_loc(id.span));
                    self.record(&id.name, rest_ty, id.span, None);
                }
            }
            _ => {
                let pat_ty = self.compile_expr(pattern);
                self.engine.unify(subject_ty, &pat_ty, pattern.span());
                self.emit(Op::Eq);
                fail_jumps.push(self.current_addr());
                self.emit_arg(Op::JumpIfFalse, 0);
            }
        }
    }

    // ========================================================================
    // Builtin calls
    // ========================================================================

    fn compile_builtin_call(&mut self, call: &ast::FunctionCallExpression) -> InferType {
        let name = call.identifier.name.clone();

        let scheme = match self.env.lookup(&name) {
            Some(s) => s,
            None => {
                if let Some(suggestion) = self.env.suggest_name(&name) {
                    self.error(
                        format!(
                            "Unknown function '{}'. Did you mean '{}'?",
                            name, suggestion
                        ),
                        call.identifier.span,
                    );
                } else {
                    self.error(format!("Unknown function '{}'", name), call.identifier.span);
                }
                for arg in &call.arguments {
                    self.compile_expr(arg);
                }
                return self.engine.fresh_var();
            }
        };

        let fn_ty = self.engine.instantiate(&scheme);
        self.record(&name, fn_ty.clone(), call.identifier.span, None);
        let ret_ty = self.call_against(&fn_ty, &call.arguments, call.span);

        match name.as_str() {
            "println" => {
                self.emit(Op::Print);
                self.emit(Op::PushNone);
            }
            "inspect" => {
                self.emit(Op::ToString);
            }
            "__stack_depth__" => {
                self.emit(Op::StackDepth);
            }
            "read_file" => {
                self.emit(Op::FileRead);
            }
            "write_file" => {
                self.emit(Op::FileWrite);
            }
            "tcp_listen" => {
                self.emit(Op::TcpListen);
            }
            "tcp_accept" => {
                self.emit(Op::TcpAccept);
            }
            "tcp_read" => {
                self.emit(Op::TcpRead);
            }
            "tcp_write" => {
                self.emit(Op::TcpWrite);
            }
            "tcp_close" => {
                self.emit(Op::TcpClose);
            }
            "str_split" => {
                self.emit(Op::StrSplit);
            }
            _ => {
                self.error(
                    format!("Internal: '{}' has a builtin scheme but no codegen", name),
                    call.span,
                );
            }
        }

        ret_ty
    }
}

// ============================================================================
// Helpers
// ============================================================================

struct EnumPatternInfo {
    enum_name: String,
    info: EnumInfo,
    variant_name: String,
    payload_args: Vec<ast::Expression>,
    pattern_span: Span,
}

fn extract_variant_parts(expr: &ast::Expression) -> Option<(String, Vec<ast::Expression>, Span)> {
    match expr {
        ast::Expression::FunctionCallExpression(fc) => Some((
            fc.identifier.name.clone(),
            fc.arguments.clone(),
            fc.identifier.span,
        )),
        ast::Expression::Identifier(id) => Some((id.name.clone(), vec![], id.span)),
        _ => None,
    }
}

fn def_loc(sp: Span) -> DefinitionLocation {
    DefinitionLocation {
        line: sp.start_line,
        column: sp.start_column,
        end_col: sp.end_column,
    }
}

fn type_info_params(info: &TypeInfo) -> &[String] {
    match info {
        TypeInfo::Struct(s) => &s.type_params,
        TypeInfo::Enum(e) => &e.type_params,
    }
}
