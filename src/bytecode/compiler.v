module bytecode

import flags { Flags }
import ast
import diagnostic { Diagnostic }
import span { Span }
import type_def { Type }
import types {
	DefinitionLocation,
	EnumInfo,
	ICon,
	IFun,
	ITuple,
	IVar,
	InferEngine,
	InferType,
	Pat,
	PatCtor,
	Scheme,
	StructInfo,
	TypeEnv,
	ast_pattern_to_pat,
	check_exhaustiveness,
	mono,
	new_engine,
	new_env,
}

// ============================================================================
// TypePosition — recorded during compilation for LSP hover/go-to-def.
// We record the live InferType; at the end of compile() we resolve once
// all unifications have happened.
// ============================================================================

pub struct TypePosition {
pub:
	line      int
	column    int
	end_col   int
	name      string @[required]
	type_info Type   @[required]
	def_line  int
	def_col   int
	def_end   int
	doc       ?string
}

struct RecordedPosition {
	span Span      @[required]
	name string    @[required]
	ty   InferType @[required]
	doc  ?string
	def  ?DefinitionLocation
}

// ============================================================================
// CompileResult
// ============================================================================

pub struct CompileResult {
pub:
	program        Program
	diagnostics    []Diagnostic
	type_positions []TypePosition
	success        bool
}

// ============================================================================
// Compiler — single-pass: HM type inference + bytecode emission
// ============================================================================

struct Scope {
	locals map[string]int
}

struct Compiler {
	flags Flags
mut:
	// --- Codegen state ---
	program          Program
	locals           map[string]int
	outer_scopes     []Scope
	local_count      int
	captures         map[string]int
	capture_names    []string
	current_binding  string
	in_tail_position bool
	// --- Type inference state ---
	engine         InferEngine
	env            TypeEnv
	recorded       []RecordedPosition
	current_fn_err ?InferType
	check_only     bool
}

pub fn compile(expr ast.Expression, fl Flags) CompileResult {
	return compile_impl(expr, fl, false)
}

pub fn check(expr ast.Expression, fl Flags) CompileResult {
	return compile_impl(expr, fl, true)
}

fn compile_impl(expr ast.Expression, fl Flags, check_only bool) CompileResult {
	mut c := Compiler{
		flags:         fl
		program:       Program{
			constants: []
			functions: []
			code:      []
			entry:     0
		}
		locals:        {}
		outer_scopes:  []
		local_count:   0
		captures:      {}
		capture_names: []
		engine:        new_engine()
		env:           new_env()
		recorded:      []
		check_only:    check_only
	}
	c.engine.type_env = &c.env

	c.register_builtins()

	main_start := c.program.code.len
	c.compile_expr(expr)
	c.emit(.halt)

	c.program.functions << Function{
		name:          '__main__'
		arity:         0
		locals:        c.local_count
		capture_count: 0
		code_start:    main_start
		code_len:      c.program.code.len - main_start
	}
	c.program.entry = c.program.functions.len - 1

	mut positions := []TypePosition{cap: c.recorded.len}
	for r in c.recorded {
		def_line, def_col, def_end := if loc := r.def {
			loc.line, loc.column, loc.end_col
		} else {
			-1, -1, -1
		}
		positions << TypePosition{
			line:      r.span.start_line
			column:    r.span.start_column
			end_col:   r.span.end_column
			name:      r.name
			type_info: c.engine.resolve(r.ty)
			def_line:  def_line
			def_col:   def_col
			def_end:   def_end
			doc:       r.doc
		}
	}

	return CompileResult{
		program:        c.program
		diagnostics:    c.engine.diagnostics
		type_positions: positions
		success:        !diagnostic.has_errors(c.engine.diagnostics)
	}
}

// ============================================================================
// Builtin registration
// ============================================================================

fn (mut c Compiler) register_builtins() {
	c.env.register_struct('Socket', [], map[string]Type{})

	a := c.engine.fresh_var()
	socket := c.engine.icon('Socket', [])
	i := c.engine.icon_int()
	s := c.engine.icon_string()
	n := c.engine.icon_none()

	a_id := (a as IVar).id

	c.env.define('println', Scheme{
		quantified: [a_id]
		ty:         IFun{
			params: [a]
			ret:    n
			err:    none
		}
	})
	c.env.define('inspect', Scheme{
		quantified: [a_id]
		ty:         IFun{
			params: [a]
			ret:    s
			err:    none
		}
	})
	c.env.define('__stack_depth__', mono(IFun{ params: [], ret: i, err: none }))
	c.env.define('read_file', mono(IFun{ params: [s], ret: s, err: s }))
	c.env.define('write_file', mono(IFun{ params: [s, s], ret: n, err: s }))
	c.env.define('str_split', mono(IFun{ params: [s, s], ret: c.engine.icon_array(s), err: none }))
	c.env.define('tcp_listen', mono(IFun{ params: [i], ret: socket, err: s }))
	c.env.define('tcp_accept', mono(IFun{ params: [socket], ret: socket, err: s }))
	c.env.define('tcp_read', mono(IFun{ params: [socket], ret: s, err: s }))
	c.env.define('tcp_write', mono(IFun{ params: [socket, s], ret: n, err: s }))
	c.env.define('tcp_close', mono(IFun{ params: [socket], ret: n, err: s }))
}

// ============================================================================
// Codegen primitives (no-op in check_only mode)
// ============================================================================

@[inline]
fn (mut c Compiler) emit(o Op) {
	if c.check_only {
		return
	}
	c.program.code << op(o)
}

@[inline]
fn (mut c Compiler) emit_arg(o Op, operand int) {
	if c.check_only {
		return
	}
	c.program.code << op_arg(o, operand)
}

@[inline]
fn (mut c Compiler) patch(addr int, inst Instruction) {
	if c.check_only {
		return
	}
	c.program.code[addr] = inst
}

fn (mut c Compiler) current_addr() int {
	return c.program.code.len
}

fn (mut c Compiler) add_constant(v Value) int {
	if c.check_only {
		return 0
	}
	c.program.constants << v
	return c.program.constants.len - 1
}

fn (mut c Compiler) get_or_create_local(name string) int {
	if idx := c.locals[name] {
		return idx
	}
	idx := c.local_count
	c.locals[name] = idx
	c.local_count += 1
	return idx
}

type VarAccess = LocalAccess | CaptureAccess | SelfAccess

struct LocalAccess {
	index int @[required]
}

struct CaptureAccess {
	index int @[required]
}

struct SelfAccess {}

fn (mut c Compiler) emit_load(access VarAccess) {
	match access {
		LocalAccess { c.emit_arg(.push_local, access.index) }
		CaptureAccess { c.emit_arg(.push_capture, access.index) }
		SelfAccess { c.emit(.push_self) }
	}
}

fn (mut c Compiler) resolve_variable(name string) ?VarAccess {
	if idx := c.locals[name] {
		return LocalAccess{
			index: idx
		}
	}
	if idx := c.captures[name] {
		return CaptureAccess{
			index: idx
		}
	}
	for scope in c.outer_scopes {
		if name in scope.locals {
			if name == c.current_binding {
				return SelfAccess{}
			}
			capture_idx := c.capture_names.len
			c.captures[name] = capture_idx
			c.capture_names << name
			return CaptureAccess{
				index: capture_idx
			}
		}
	}
	return none
}

// ============================================================================
// Diagnostics & LSP recording
// ============================================================================

fn (mut c Compiler) error(msg string, sp Span) {
	c.engine.diagnostics << Diagnostic{
		span:     sp
		severity: .error
		message:  msg
	}
}

// The top-level program is a BlockExpression which pushes one scope on top of
// the global (builtins) scope. So "at top level" means depth ≤ 2.
fn (c &Compiler) at_top_level() bool {
	return c.env.scope_depth() <= 2
}

fn (mut c Compiler) record(name string, ty InferType, sp Span, doc ?string) {
	// Capture the definition location NOW while the correct scope is active.
	// env.definitions is flat (last-write-wins) so looking it up at finalize
	// time gets the wrong answer when names are reused across scopes.
	c.recorded << RecordedPosition{
		span: sp
		name: name
		ty:   ty
		doc:  doc
		def:  c.env.lookup_definition(name)
	}
}

fn def_loc(sp Span) DefinitionLocation {
	return DefinitionLocation{
		line:    sp.start_line
		column:  sp.start_column
		end_col: sp.end_column
	}
}

// ============================================================================
// Type annotation resolution: ast.TypeIdentifier → InferType
// ============================================================================

fn (mut c Compiler) resolve_type(t ast.TypeIdentifier, param_vars map[string]InferType) InferType {
	match t.kind {
		ast.OptionType {
			inner := c.resolve_type(*t.kind.inner, param_vars)
			return c.engine.icon_option(inner)
		}
		ast.ArrayType {
			elem := c.resolve_type(*t.kind.element, param_vars)
			return c.engine.icon_array(elem)
		}
		ast.FunctionType {
			mut params := []InferType{cap: t.kind.params.len}
			for pt in t.kind.params {
				params << c.resolve_type(pt, param_vars)
			}
			mut ret := c.engine.icon_none()
			if rt := t.kind.return_type {
				ret = c.resolve_type(*rt, param_vars)
			}
			mut err := ?InferType(none)
			if et := t.kind.error_type {
				err = c.resolve_type(*et, param_vars)
			}
			return IFun{
				params: params
				ret:    ret
				err:    err
			}
		}
		ast.NamedType {
			name := t.kind.identifier.name

			if v := param_vars[name] {
				return v
			}

			match name {
				'Int' { return c.engine.icon_int() }
				'Float' { return c.engine.icon_float() }
				'String' { return c.engine.icon_string() }
				'Bool' { return c.engine.icon_bool() }
				'None' { return c.engine.icon_none() }
				else {}
			}

			if info := c.env.lookup_type_info(name) {
				mut args := []InferType{cap: t.kind.type_args.len}
				for ta in t.kind.type_args {
					args << c.resolve_type(ta, param_vars)
				}
				if args.len == 0 && info.type_params.len > 0 {
					for _ in 0 .. info.type_params.len {
						args << c.engine.fresh_var()
					}
				}
				return c.engine.icon(name, args)
			}

			if name.len == 1 && name[0] >= `A` && name[0] <= `Z` {
				return c.engine.fresh_var()
			}

			c.error("Unknown type '${name}'", t.kind.identifier.span)
			return c.engine.fresh_var()
		}
	}
}

// ============================================================================
// Node dispatch
// ============================================================================

fn (mut c Compiler) compile_node(node ast.Node) InferType {
	return match node {
		ast.Statement {
			c.compile_statement(node)
			c.engine.icon_none()
		}
		ast.Expression {
			c.compile_expr(node)
		}
	}
}

// ============================================================================
// Statements
// ============================================================================

fn (mut c Compiler) compile_statement(stmt ast.Statement) {
	match stmt {
		ast.VariableBinding {
			name := stmt.identifier.name
			idx := c.get_or_create_local(name)

			annot_ty := if annot := stmt.typ {
				?InferType(c.resolve_type(annot, map[string]InferType{}))
			} else {
				?InferType(none)
			}

			c.engine.enter_level()
			self_ty := c.engine.fresh_var()
			c.env.define(name, mono(self_ty))

			old_binding := c.current_binding
			c.current_binding = name
			init_ty := c.compile_expr_with_hint(stmt.init, annot_ty)
			c.current_binding = old_binding

			c.engine.unify(self_ty, init_ty, stmt.init.span)
			c.engine.leave_level()

			final_ty := if a := annot_ty {
				c.engine.unify(a, init_ty, stmt.identifier.span)
				a
			} else {
				init_ty
			}

			scheme := c.engine.generalize(final_ty)
			c.env.define_at(name, scheme, def_loc(stmt.identifier.span))
			if doc := stmt.doc {
				c.env.store_doc(name, doc)
			}
			c.record(name, final_ty, stmt.identifier.span, stmt.doc)

			c.emit_arg(.store_local, idx)
		}
		ast.ConstBinding {
			if !c.at_top_level() {
				c.error('const declarations are only allowed at the top level', stmt.span)
			}
			name := stmt.identifier.name

			annot_ty := if annot := stmt.typ {
				?InferType(c.resolve_type(annot, map[string]InferType{}))
			} else {
				?InferType(none)
			}

			c.engine.enter_level()
			init_ty := c.compile_expr_with_hint(stmt.init, annot_ty)
			c.engine.leave_level()

			final_ty := if a := annot_ty {
				c.engine.unify(a, init_ty, stmt.identifier.span)
				a
			} else {
				init_ty
			}

			scheme := c.engine.generalize(final_ty)
			c.env.define_at(name, scheme, def_loc(stmt.identifier.span))
			if doc := stmt.doc {
				c.env.store_doc(name, doc)
			}
			c.record(name, final_ty, stmt.identifier.span, stmt.doc)

			idx := c.get_or_create_local(name)
			c.emit_arg(.store_local, idx)
		}
		ast.TypePatternBinding {
			init_ty := c.compile_expr(stmt.init)
			annot_ty := c.resolve_type(stmt.typ, map[string]InferType{})
			c.engine.unify(annot_ty, init_ty, stmt.typ.span)
			c.emit(.pop)
		}
		ast.TupleDestructuringBinding {
			init_ty := c.compile_expr(stmt.init)

			mut elem_vars := []InferType{cap: stmt.patterns.len}
			for _ in 0 .. stmt.patterns.len {
				elem_vars << c.engine.fresh_var()
			}
			c.engine.unify(init_ty, ITuple{ elements: elem_vars }, stmt.init.span)

			for i, pattern in stmt.patterns {
				elem_ty := elem_vars[i]
				match pattern {
					ast.Identifier {
						c.emit(.dup)
						c.emit_arg(.tuple_index, i)
						idx := c.get_or_create_local(pattern.name)
						c.emit_arg(.store_local, idx)
						c.env.define_at(pattern.name, mono(elem_ty), def_loc(pattern.span))
						c.record(pattern.name, elem_ty, pattern.span, none)
					}
					ast.TypeIdentifier {
						annot_ty := c.resolve_type(pattern, map[string]InferType{})
						c.engine.unify(annot_ty, elem_ty, pattern.span)
					}
					else {
						c.error('tuple destructuring only supports identifiers and type patterns',
							stmt.span)
					}
				}
			}
			c.emit(.pop)
		}
		ast.FunctionDeclaration {
			if !c.at_top_level() {
				c.error('named function declarations are only allowed at the top level',
					stmt.span)
			}
			name := stmt.identifier.name

			c.engine.enter_level()
			self_ty := c.engine.fresh_var()
			c.env.define(name, mono(self_ty))

			fn_ty := c.compile_function_common(stmt.identifier.name, stmt.params, stmt.body,
				stmt.return_type, stmt.error_type)

			c.engine.unify(self_ty, fn_ty, stmt.identifier.span)
			c.engine.leave_level()

			scheme := c.engine.generalize(fn_ty)
			c.env.define_at(name, scheme, def_loc(stmt.identifier.span))
			if doc := stmt.doc {
				c.env.store_doc(name, doc)
			}
			c.record(name, fn_ty, stmt.identifier.span, stmt.doc)

			idx := c.get_or_create_local(name)
			c.emit_arg(.store_local, idx)
		}
		ast.StructDeclaration {
			if !c.at_top_level() {
				c.error('struct declarations are only allowed at the top level', stmt.span)
			}
			name := stmt.identifier.name
			mut type_params := []string{cap: stmt.type_params.len}
			for tp in stmt.type_params {
				type_params << tp.name
			}

			mut param_vars := map[string]InferType{}
			for tp in type_params {
				v := c.engine.fresh_var()
				param_vars[tp] = v
				c.engine.name_var(v, tp)
			}
			mut fields := map[string]Type{}
			for field in stmt.fields {
				field_ty := c.resolve_type(field.typ, param_vars)
				fields[field.identifier.name] = c.engine.resolve(field_ty)
				qualified := '${name}.${field.identifier.name}'
				c.env.definitions[qualified] = def_loc(field.identifier.span)
				if doc := field.doc {
					c.env.store_doc(qualified, doc)
				}
			}

			c.env.register_struct(name, type_params, fields)
			c.env.definitions[name] = def_loc(stmt.identifier.span)
			if doc := stmt.doc {
				c.env.store_doc(name, doc)
			}
		}
		ast.EnumDeclaration {
			if !c.at_top_level() {
				c.error('enum declarations are only allowed at the top level', stmt.span)
			}
			name := stmt.identifier.name
			mut type_params := []string{cap: stmt.type_params.len}
			for tp in stmt.type_params {
				type_params << tp.name
			}

			mut param_vars := map[string]InferType{}
			for tp in type_params {
				v := c.engine.fresh_var()
				param_vars[tp] = v
				c.engine.name_var(v, tp)
			}

			mut variants := map[string][]Type{}
			for variant in stmt.variants {
				mut payload := []Type{cap: variant.payload.len}
				for pt in variant.payload {
					payload_ty := c.resolve_type(pt, param_vars)
					payload << c.engine.resolve(payload_ty)
				}
				variants[variant.identifier.name] = payload
				if doc := variant.doc {
					c.env.store_doc('${name}.${variant.identifier.name}', doc)
				}
				c.env.definitions['${name}.${variant.identifier.name}'] = def_loc(variant.identifier.span)
			}

			c.env.register_enum(name, type_params, variants)
			c.env.definitions[name] = def_loc(stmt.identifier.span)
			if doc := stmt.doc {
				c.env.store_doc(name, doc)
			}
		}
		ast.ImportDeclaration {}
		ast.ExportDeclaration {
			c.compile_statement(stmt.declaration)
		}
	}
}

// ============================================================================
// Expressions
// ============================================================================

fn (mut c Compiler) compile_expr(expr ast.Expression) InferType {
	is_tail := c.in_tail_position
	c.in_tail_position = false

	match expr {
		ast.BlockExpression {
			c.env.push_scope()
			mut last_ty := c.engine.icon_none()

			for i, node in expr.body {
				is_last := i == expr.body.len - 1
				c.in_tail_position = is_tail && is_last
				ty := c.compile_node(node)
				c.in_tail_position = false

				if is_last {
					last_ty = ty
				} else if node is ast.Expression {
					resolved := c.engine.find(ty)
					if resolved is ICon {
						if resolved.name != 'None' {
							ty_str := c.engine.type_to_str(ty)
							c.error("Expression of type '${ty_str}' must be consumed. Assign it to a variable or use '${ty_str} =' to discard",
								ast.node_span(node))
						}
					} else if resolved !is IVar {
						ty_str := c.engine.type_to_str(ty)
						c.error("Expression of type '${ty_str}' must be consumed. Assign it to a variable or use '${ty_str} =' to discard",
							ast.node_span(node))
					}
					c.emit(.pop)
				}
			}

			if expr.body.len == 0 {
				c.emit(.push_none)
			} else if expr.body[expr.body.len - 1] is ast.Statement {
				c.emit(.push_none)
			}

			c.env.pop_scope()
			return last_ty
		}
		ast.NumberLiteral {
			if expr.value.contains('.') {
				idx := c.add_constant(expr.value.f64())
				c.emit_arg(.push_const, idx)
				return c.engine.icon_float()
			} else {
				idx := c.add_constant(expr.value.int())
				c.emit_arg(.push_const, idx)
				return c.engine.icon_int()
			}
		}
		ast.StringLiteral {
			idx := c.add_constant(expr.value)
			c.emit_arg(.push_const, idx)
			return c.engine.icon_string()
		}
		ast.InterpolatedString {
			if expr.parts.len == 0 {
				idx := c.add_constant('')
				c.emit_arg(.push_const, idx)
			} else {
				c.compile_expr(expr.parts[0])
				c.emit(.to_string)
				for i := 1; i < expr.parts.len; i++ {
					c.compile_expr(expr.parts[i])
					c.emit(.to_string)
					c.emit(.str_concat)
				}
			}
			return c.engine.icon_string()
		}
		ast.BooleanLiteral {
			if expr.value {
				c.emit(.push_true)
			} else {
				c.emit(.push_false)
			}
			return c.engine.icon_bool()
		}
		ast.NoneExpression {
			c.emit(.push_none)
			return c.engine.icon_none()
		}
		ast.Identifier {
			return c.compile_identifier(expr)
		}
		ast.BinaryExpression {
			return c.compile_binary(expr)
		}
		ast.UnaryExpression {
			inner_ty := c.compile_expr(expr.expression)
			match expr.op.kind {
				.punc_exclamation_mark {
					c.engine.unify(inner_ty, c.engine.icon_bool(), expr.expression.span)
					c.emit(.not)
					return c.engine.icon_bool()
				}
				.punc_minus {
					result := c.engine.fresh_constrained_var(.numeric)
					c.engine.unify(inner_ty, result, expr.expression.span)
					c.emit(.neg)
					return result
				}
				else {
					c.error('Unknown unary operator: ${expr.op.kind}', expr.span)
					return c.engine.fresh_var()
				}
			}
		}
		ast.IfExpression {
			cond_ty := c.compile_expr(expr.condition)
			c.engine.unify(cond_ty, c.engine.icon_bool(), expr.condition.span)

			else_jump := c.current_addr()
			c.emit_arg(.jump_if_false, 0)

			c.in_tail_position = is_tail
			then_ty := c.compile_expr(expr.body)
			c.in_tail_position = false

			end_jump := c.current_addr()
			c.emit_arg(.jump, 0)

			c.patch(else_jump, op_arg(.jump_if_false, c.current_addr()))

			if else_body := expr.else_body {
				c.in_tail_position = is_tail
				else_ty := c.compile_expr(else_body)
				c.in_tail_position = false
				c.patch(end_jump, op_arg(.jump, c.current_addr()))
				return c.join_arms([then_ty, else_ty], expr.span)
			}

			// No else: result is None. The then-branch runs for side effect only.
			c.emit(.push_none)
			c.patch(end_jump, op_arg(.jump, c.current_addr()))
			return c.engine.icon_none()
		}
		ast.MatchExpression {
			return c.compile_match(expr, is_tail)
		}
		ast.ArrayExpression {
			return c.compile_array(expr)
		}
		ast.TupleExpression {
			mut elem_tys := []InferType{cap: expr.elements.len}
			for elem in expr.elements {
				elem_tys << c.compile_expr(elem)
			}
			c.emit_arg(.make_tuple, expr.elements.len)
			return ITuple{
				elements: elem_tys
			}
		}
		ast.ArrayIndexExpression {
			arr_ty := c.compile_expr(expr.expression)
			elem_var := c.engine.fresh_var()
			c.engine.unify(arr_ty, c.engine.icon_array(elem_var), expr.expression.span)

			if expr.index is ast.RangeExpression {
				r := expr.index as ast.RangeExpression
				start_ty := c.compile_expr(r.start)
				end_ty := c.compile_expr(r.end)
				c.engine.unify(start_ty, c.engine.icon_int(), r.start.span)
				c.engine.unify(end_ty, c.engine.icon_int(), r.end.span)
				c.emit(.array_slice)
				return c.engine.icon_array(elem_var)
			} else {
				idx_ty := c.compile_expr(expr.index)
				c.engine.unify(idx_ty, c.engine.icon_int(), expr.index.span)
				c.emit(.index)
				return elem_var
			}
		}
		ast.RangeExpression {
			start_ty := c.compile_expr(expr.start)
			end_ty := c.compile_expr(expr.end)
			c.engine.unify(start_ty, c.engine.icon_int(), expr.start.span)
			c.engine.unify(end_ty, c.engine.icon_int(), expr.end.span)
			c.emit(.make_range)
			return c.engine.icon_array(c.engine.icon_int())
		}
		ast.FunctionExpression {
			c.engine.enter_level()
			ty := c.compile_function_common(none, expr.params, expr.body, expr.return_type,
				expr.error_type)
			c.engine.leave_level()
			return ty
		}
		ast.FunctionCallExpression {
			return c.compile_call(expr, is_tail)
		}
		ast.PropertyAccessExpression {
			return c.compile_property_access(expr)
		}
		ast.StructInitExpression {
			return c.compile_struct_init(expr)
		}
		ast.ErrorExpression {
			inner_ty := c.compile_expr(expr.expression)
			if fn_err := c.current_fn_err {
				c.engine.unify(inner_ty, fn_err, expr.expression.span)
			} else {
				c.error("'error' expression outside of a failable function", expr.span)
			}
			c.emit(.make_error)
			return c.engine.fresh_var()
		}
		ast.OrExpression {
			return c.compile_or(expr)
		}
		ast.ErrorNode {
			return c.engine.fresh_var()
		}
		ast.OrPattern, ast.WildcardPattern, ast.TypeIdentifier {
			c.error('Pattern used in expression position', expr.span)
			return c.engine.fresh_var()
		}
	}
}

// ============================================================================
// Identifier
// ============================================================================

fn (mut c Compiler) compile_identifier(expr ast.Identifier) InferType {
	name := expr.name

	if scheme := c.env.lookup(name) {
		ty := c.engine.instantiate(scheme)
		c.record(name, ty, expr.span, c.env.lookup_doc(name))

		if access := c.resolve_variable(name) {
			c.emit_load(access)
			return ty
		}

		c.error("'${name}' is a builtin and cannot be used as a value", expr.span)
		c.emit(.push_none)
		return ty
	}

	// Not a scoped name: try nullary enum variant.
	if enum_name, info := c.env.find_variant_owner(name) {
		payload := info.variants[name] or { []Type{} }
		if payload.len == 0 {
			mut type_args := []InferType{cap: info.type_params.len}
			for _ in 0 .. info.type_params.len {
				type_args << c.engine.fresh_var()
			}
			return c.emit_enum_construction(enum_name, info, type_args, name, []ast.Expression{},
				expr.span)
		}
		c.error("Variant '${name}' requires ${payload.len} argument(s); use ${name}(...)",
			expr.span)
		c.emit(.push_none)
		return c.engine.fresh_var()
	}

	if suggestion := c.env.suggest_name(name) {
		c.error("Unknown identifier '${name}'. Did you mean '${suggestion}'?", expr.span)
	} else {
		c.error("Unknown identifier '${name}'", expr.span)
	}
	c.emit(.push_none)
	return c.engine.fresh_var()
}

// ============================================================================
// Binary ops
// ============================================================================

fn (mut c Compiler) compile_binary(expr ast.BinaryExpression) InferType {
	if expr.op.kind == .logical_and {
		l_ty := c.compile_expr(expr.left)
		c.engine.unify(l_ty, c.engine.icon_bool(), expr.left.span)
		c.emit(.dup)
		end_jump := c.current_addr()
		c.emit_arg(.jump_if_false, 0)
		c.emit(.pop)
		r_ty := c.compile_expr(expr.right)
		c.engine.unify(r_ty, c.engine.icon_bool(), expr.right.span)
		c.patch(end_jump, op_arg(.jump_if_false, c.current_addr()))
		return c.engine.icon_bool()
	}
	if expr.op.kind == .logical_or {
		l_ty := c.compile_expr(expr.left)
		c.engine.unify(l_ty, c.engine.icon_bool(), expr.left.span)
		c.emit(.dup)
		end_jump := c.current_addr()
		c.emit_arg(.jump_if_true, 0)
		c.emit(.pop)
		r_ty := c.compile_expr(expr.right)
		c.engine.unify(r_ty, c.engine.icon_bool(), expr.right.span)
		c.patch(end_jump, op_arg(.jump_if_true, c.current_addr()))
		return c.engine.icon_bool()
	}

	l_ty := c.compile_expr(expr.left)
	r_ty := c.compile_expr(expr.right)

	match expr.op.kind {
		.punc_plus {
			result := c.engine.fresh_constrained_var(.addable)
			c.engine.unify(l_ty, result, expr.left.span)
			c.engine.unify(r_ty, result, expr.right.span)
			c.emit(.add)
			return result
		}
		.punc_minus, .punc_mul, .punc_div, .punc_mod {
			result := c.engine.fresh_constrained_var(.numeric)
			c.engine.unify(l_ty, result, expr.left.span)
			c.engine.unify(r_ty, result, expr.right.span)
			opc := match expr.op.kind {
				.punc_minus { Op.sub }
				.punc_mul { Op.mul }
				.punc_div { Op.div }
				.punc_mod { Op.mod }
				else { Op.sub }
			}
			c.emit(opc)
			return result
		}
		.punc_lt, .punc_gt, .punc_lte, .punc_gte {
			operand := c.engine.fresh_constrained_var(.numeric)
			c.engine.unify(l_ty, operand, expr.left.span)
			c.engine.unify(r_ty, operand, expr.right.span)
			opc := match expr.op.kind {
				.punc_lt { Op.lt }
				.punc_gt { Op.gt }
				.punc_lte { Op.lte }
				.punc_gte { Op.gte }
				else { Op.lt }
			}
			c.emit(opc)
			return c.engine.icon_bool()
		}
		.punc_equals_comparator, .punc_not_equal {
			c.engine.unify(l_ty, r_ty, expr.span)
			if expr.op.kind == .punc_equals_comparator {
				c.emit(.eq)
			} else {
				c.emit(.neq)
			}
			return c.engine.icon_bool()
		}
		else {
			c.error('Unknown binary operator: ${expr.op.kind}', expr.span)
			return c.engine.fresh_var()
		}
	}
}

// ============================================================================
// Array
// ============================================================================

fn (mut c Compiler) compile_array(expr ast.ArrayExpression) InferType {
	elem_var := c.engine.fresh_var()
	has_spread := expr.elements.any(it is ast.SpreadElement)

	if !has_spread {
		for elem in expr.elements {
			if elem is ast.Expression {
				ty := c.compile_expr(elem)
				c.engine.unify(ty, elem_var, elem.span)
			}
		}
		c.emit_arg(.make_array, expr.elements.len)
		return c.engine.icon_array(elem_var)
	}

	mut have_result := false
	mut i := 0
	for i < expr.elements.len {
		elem := expr.elements[i]
		if elem is ast.SpreadElement {
			inner := elem.expression or {
				c.error('Spread in array literal missing expression', elem.span)
				i++
				continue
			}

			spread_ty := c.compile_expr(inner)
			c.engine.unify(spread_ty, c.engine.icon_array(elem_var), inner.span)
			if have_result {
				c.emit(.array_concat)
			} else {
				have_result = true
			}
			i++
		} else {
			mut group_count := 0
			for j := i; j < expr.elements.len; j++ {
				if expr.elements[j] is ast.SpreadElement {
					break
				}
				ae := expr.elements[j]
				if ae is ast.Expression {
					ty := c.compile_expr(ae)
					c.engine.unify(ty, elem_var, ae.span)
				}
				group_count++
			}
			c.emit_arg(.make_array, group_count)
			if have_result {
				c.emit(.array_concat)
			} else {
				have_result = true
			}
			i += group_count
		}
	}

	if !have_result {
		c.emit_arg(.make_array, 0)
	}
	return c.engine.icon_array(elem_var)
}

// ============================================================================
// Function call
// ============================================================================

fn (mut c Compiler) compile_call(expr ast.FunctionCallExpression, is_tail bool) InferType {
	name := expr.identifier.name

	if access := c.resolve_variable(name) {
		scheme := c.env.lookup(name) or {
			c.error("Internal: '${name}' resolved as local but has no type scheme", expr.identifier.span)
			return c.engine.fresh_var()
		}
		fn_ty := c.engine.instantiate(scheme)
		c.record(name, fn_ty, expr.identifier.span, c.env.lookup_doc(name))

		ret_ty := c.call_against(fn_ty, expr.arguments, expr.span)

		c.emit_load(access)
		if is_tail {
			c.emit_arg(.tail_call, expr.arguments.len)
		} else {
			c.emit_arg(.call, expr.arguments.len)
		}

		return ret_ty
	}

	// Not a local: try enum variant constructor (bare form, unambiguous).
	if enum_name, info := c.env.find_variant_owner(name) {
		mut type_args := []InferType{cap: info.type_params.len}
		for _ in 0 .. info.type_params.len {
			type_args << c.engine.fresh_var()
		}
		return c.emit_enum_construction(enum_name, info, type_args, name, expr.arguments,
			expr.span)
	}

	return c.compile_builtin_call(expr)
}

fn (mut c Compiler) call_against(fn_ty InferType, args []ast.Expression, call_span Span) InferType {
	resolved := c.engine.find(fn_ty)

	if resolved is IFun {
		if resolved.params.len != args.len {
			c.error('Expected ${resolved.params.len} argument(s), got ${args.len}', call_span)
		}
		for i, arg in args {
			hint := if i < resolved.params.len {
				?InferType(resolved.params[i])
			} else {
				?InferType(none)
			}
			arg_ty := c.compile_expr_with_hint(arg, hint)
			if i < resolved.params.len {
				c.engine.unify(resolved.params[i], arg_ty, arg.span)
			}
		}
		if err := resolved.err {
			return c.engine.icon_result(resolved.ret, err)
		}
		return resolved.ret
	}

	mut param_tys := []InferType{cap: args.len}
	for arg in args {
		arg_ty := c.compile_expr(arg)
		param_tys << arg_ty
	}
	ret_var := c.engine.fresh_var()
	c.engine.unify(fn_ty, IFun{ params: param_tys, ret: ret_var, err: none }, call_span)
	return ret_var
}

fn (mut c Compiler) compile_expr_with_hint(expr ast.Expression, hint ?InferType) InferType {
	h := hint or { return c.compile_expr(expr) }
	resolved := c.engine.find(h)
	if resolved !is ICon {
		return c.compile_expr(expr)
	}
	hint_name := (resolved as ICon).name
	info := c.env.lookup_type_info(hint_name) or { return c.compile_expr(expr) }
	if info !is EnumInfo {
		return c.compile_expr(expr)
	}
	einfo := info as EnumInfo

	variant_name, variant_args, variant_span := if expr is ast.FunctionCallExpression {
		expr.identifier.name, expr.arguments, expr.span
	} else if expr is ast.Identifier {
		expr.name, []ast.Expression{}, expr.span
	} else {
		return c.compile_expr(expr)
	}

	if variant_name !in einfo.variants {
		return c.compile_expr(expr)
	}

	return c.emit_enum_construction(hint_name, einfo, (resolved as ICon).args, variant_name,
		variant_args, variant_span)
}

// ============================================================================
// Property access
// ============================================================================

fn (mut c Compiler) compile_property_access(expr ast.PropertyAccessExpression) InferType {
	if expr.left is ast.Identifier {
		left_id := expr.left as ast.Identifier
		if info := c.env.lookup_type_info(left_id.name) {
			if info is EnumInfo {
				enum_name := left_id.name

				variant_name, args, variant_span := if expr.right is ast.FunctionCallExpression {
					call := expr.right as ast.FunctionCallExpression
					call.identifier.name, call.arguments, call.identifier.span
				} else if expr.right is ast.Identifier {
					r := expr.right as ast.Identifier
					r.name, []ast.Expression{}, r.span
				} else {
					c.error('Invalid enum variant access', expr.right.span)
					return c.engine.fresh_var()
				}

				if variant_name !in info.variants {
					c.error("Enum '${enum_name}' has no variant '${variant_name}'", variant_span)
					return c.engine.fresh_var()
				}

				mut type_args := []InferType{cap: info.type_params.len}
				for _ in 0 .. info.type_params.len {
					type_args << c.engine.fresh_var()
				}

				enum_ty := c.engine.icon(enum_name, type_args)
				c.record(enum_name, enum_ty, left_id.span, c.env.lookup_doc(enum_name))

				return c.emit_enum_construction(enum_name, info, type_args, variant_name,
					args, variant_span)
			}
		}
	}

	left_ty := c.compile_expr(expr.left)

	if expr.right is ast.FunctionCallExpression {
		call := expr.right as ast.FunctionCallExpression
		c.error("Cannot call '${call.identifier.name}' as a method. AL does not have methods — use '${call.identifier.name}(...)' as a free function instead.",
			call.span)
		for arg in call.arguments {
			c.compile_expr(arg)
		}
		return c.engine.fresh_var()
	}

	if expr.right is ast.NumberLiteral {
		num := expr.right as ast.NumberLiteral
		index := num.value.int()
		c.emit_arg(.tuple_index, index)

		resolved := c.engine.find(left_ty)
		if resolved is ITuple {
			if index < resolved.elements.len {
				return resolved.elements[index]
			}
			c.error('Tuple index ${index} out of bounds (tuple has ${resolved.elements.len} elements)',
				expr.right.span)
			return c.engine.fresh_var()
		}
		c.error('Cannot index .${index} on non-tuple type', expr.right.span)
		return c.engine.fresh_var()
	}

	if expr.right is ast.Identifier {
		field := expr.right as ast.Identifier
		idx := c.add_constant(field.name)
		c.emit_arg(.get_field, idx)

		resolved := c.engine.find(left_ty)
		if resolved is ICon {
			if info := c.env.lookup_type_info(resolved.name) {
				if info is StructInfo {
					if field_tmpl := info.fields[field.name] {
						mut vars := map[string]InferType{}
						for i, p in info.type_params {
							if i < resolved.args.len {
								vars[p] = resolved.args[i]
							}
						}
						field_ty := c.engine.type_to_infer_with_vars(field_tmpl, vars)
						qualified := '${resolved.name}.${field.name}'
						c.record(qualified, field_ty, field.span, c.env.lookup_doc(qualified))
						return field_ty
					}
					available := info.fields.keys().join(', ')
					c.error("Struct '${resolved.name}' has no field '${field.name}'. Available: ${available}",
						field.span)
					return c.engine.fresh_var()
				}
			}
		}
		ty_str := c.engine.type_to_str(left_ty)
		c.error("Cannot access field '${field.name}' on type '${ty_str}'", field.span)
		return c.engine.fresh_var()
	}

	c.error('Invalid property access', expr.right.span)
	return c.engine.fresh_var()
}

fn (mut c Compiler) emit_enum_construction(enum_name string, info EnumInfo, type_args []InferType, variant_name string, args []ast.Expression, variant_span Span) InferType {
	payload_templates := info.variants[variant_name] or { []Type{} }

	mut vars := map[string]InferType{}
	for i, p in info.type_params {
		if i < type_args.len {
			vars[p] = type_args[i]
		}
	}

	if payload_templates.len > 0 {
		if args.len != payload_templates.len {
			c.error("Variant '${variant_name}' expects ${payload_templates.len} argument(s), got ${args.len}",
				variant_span)
		}
		c.emit_arg(.push_const, c.add_constant(info.id))
		c.emit_arg(.push_const, c.add_constant(enum_name))
		c.emit_arg(.push_const, c.add_constant(variant_name))
		for i, arg in args {
			arg_ty := c.compile_expr(arg)
			if i < payload_templates.len {
				expected := c.engine.type_to_infer_with_vars(payload_templates[i], vars)
				c.engine.unify(expected, arg_ty, arg.span)
			}
		}
		c.emit_arg(.make_enum_payload, args.len)
	} else {
		if args.len > 0 {
			c.error("Variant '${variant_name}' takes no arguments", variant_span)
		}
		c.emit_arg(.push_const, c.add_constant(info.id))
		c.emit_arg(.push_const, c.add_constant(enum_name))
		c.emit_arg(.push_const, c.add_constant(variant_name))
		c.emit(.make_enum)
	}

	result_ty := c.engine.icon(enum_name, type_args)
	qualified := '${enum_name}.${variant_name}'
	c.record(qualified, result_ty, variant_span, c.env.lookup_doc(qualified))

	return result_ty
}

// ============================================================================
// Struct init
// ============================================================================

fn (mut c Compiler) compile_struct_init(expr ast.StructInitExpression) InferType {
	struct_name := expr.identifier.name
	info := c.env.lookup_type_info(struct_name) or {
		c.error("Unknown struct '${struct_name}'", expr.identifier.span)
		for field in expr.fields {
			c.compile_expr(field.init)
		}
		return c.engine.fresh_var()
	}
	if info !is StructInfo {
		c.error("'${struct_name}' is not a struct", expr.identifier.span)
		return c.engine.fresh_var()
	}
	sinfo := info as StructInfo

	mut type_args := []InferType{cap: sinfo.type_params.len}
	if expr.type_args.len > 0 {
		if expr.type_args.len != sinfo.type_params.len {
			c.error("Struct '${struct_name}' expects ${sinfo.type_params.len} type argument(s), got ${expr.type_args.len}",
				expr.identifier.span)
		}
		for ta in expr.type_args {
			type_args << c.resolve_type(ta, map[string]InferType{})
		}
		for type_args.len < sinfo.type_params.len {
			type_args << c.engine.fresh_var()
		}
	} else {
		for _ in 0 .. sinfo.type_params.len {
			type_args << c.engine.fresh_var()
		}
	}

	mut vars := map[string]InferType{}
	for i, p in sinfo.type_params {
		vars[p] = type_args[i]
	}

	mut provided := map[string]bool{}
	for field in expr.fields {
		fname := field.identifier.name
		if fname in provided {
			c.error("Duplicate field '${fname}'", field.identifier.span)
		}
		provided[fname] = true

		name_idx := c.add_constant(fname)
		c.emit_arg(.push_const, name_idx)
		init_ty := c.compile_expr(field.init)

		if field_tmpl := sinfo.fields[fname] {
			expected := c.engine.type_to_infer_with_vars(field_tmpl, vars)
			c.engine.unify(expected, init_ty, field.init.span)
			qualified := '${struct_name}.${fname}'
			c.record(qualified, expected, field.identifier.span, c.env.lookup_doc(qualified))
		} else {
			available := sinfo.fields.keys().join(', ')
			c.error("Struct '${struct_name}' has no field '${fname}'. Available: ${available}",
				field.identifier.span)
		}
	}

	mut missing := []string{}
	for fname, _ in sinfo.fields {
		if fname !in provided {
			missing << fname
		}
	}
	if missing.len > 0 {
		c.error("Missing required fields in '${struct_name}': ${missing.join(', ')}",
			expr.identifier.span)
	}

	c.emit_arg(.push_const, c.add_constant(sinfo.id))
	c.emit_arg(.push_const, c.add_constant(struct_name))
	c.emit_arg(.make_struct, expr.fields.len)

	result_ty := c.engine.icon(struct_name, type_args)
	c.record(struct_name, result_ty, expr.identifier.span, c.env.lookup_doc(struct_name))

	return result_ty
}

// ============================================================================
// Or expression (Result/Option unwrapping)
// ============================================================================

fn (mut c Compiler) compile_or(expr ast.OrExpression) InferType {
	left_ty := c.compile_expr(expr.expression)

	success_var := c.engine.fresh_var()
	err_var := c.engine.fresh_var()

	resolved := c.engine.find(left_ty)
	is_option := if resolved is ICon { resolved.name == 'Option' } else { false }

	if is_option {
		c.engine.unify(left_ty, c.engine.icon_option(success_var), expr.expression.span)
	} else {
		c.engine.unify(left_ty, c.engine.icon_result(success_var, err_var), expr.expression.span)
	}

	c.emit(.dup)
	c.emit(.is_failure)
	not_failure_jump := c.current_addr()
	c.emit_arg(.jump_if_false, 0)

	c.emit(.unwrap_failure)
	c.env.push_scope()
	if receiver := expr.receiver {
		idx := c.get_or_create_local(receiver.name)
		c.emit_arg(.store_local, idx)
		receiver_ty := if is_option { c.engine.icon_none() } else { err_var }
		c.env.define_at(receiver.name, mono(receiver_ty), def_loc(receiver.span))
		c.record(receiver.name, receiver_ty, receiver.span, none)
	} else {
		c.emit(.pop)
	}

	body_ty := c.compile_expr(expr.body)
	c.engine.unify(body_ty, success_var, expr.body.span)
	c.env.pop_scope()

	end_jump := c.current_addr()
	c.emit_arg(.jump, 0)

	c.patch(not_failure_jump, op_arg(.jump_if_false, c.current_addr()))
	c.patch(end_jump, op_arg(.jump, c.current_addr()))

	return success_var
}

// ============================================================================
// Functions
// ============================================================================

fn (mut c Compiler) compile_function_common(name ?string, params []ast.FunctionParameter, body ast.Expression, return_annot ?ast.TypeIdentifier, error_annot ?ast.TypeIdentifier) InferType {
	old_locals := c.locals.clone()
	old_local_count := c.local_count
	old_captures := c.captures.clone()
	old_capture_names := c.capture_names.clone()

	mut scope_locals := old_locals.clone()
	if n := name {
		scope_locals[n] = c.local_count
	}
	c.outer_scopes << Scope{
		locals: scope_locals
	}

	jump_over := c.current_addr()
	c.emit_arg(.jump, 0)

	c.locals = {}
	c.local_count = 0
	c.captures = {}
	c.capture_names = []

	c.env.push_scope()

	mut annot_param_vars := map[string]InferType{}
	mut param_tys := []InferType{cap: params.len}
	for param in params {
		p_ty := if annot := param.typ {
			c.resolve_type(annot, annot_param_vars)
		} else {
			c.engine.fresh_var()
		}
		param_tys << p_ty
		c.get_or_create_local(param.identifier.name)
		c.env.define_at(param.identifier.name, mono(p_ty), def_loc(param.identifier.span))
		c.record(param.identifier.name, p_ty, param.identifier.span, none)
	}

	old_fn_err := c.current_fn_err
	declared_err := if et := error_annot {
		?InferType(c.resolve_type(et, annot_param_vars))
	} else {
		?InferType(none)
	}
	c.current_fn_err = declared_err

	func_start := c.current_addr()

	old_binding := c.current_binding
	if n := name {
		c.current_binding = n
	}

	old_tail := c.in_tail_position
	c.in_tail_position = true
	body_ty := c.compile_expr(body)
	c.in_tail_position = old_tail
	c.current_binding = old_binding
	c.emit(.ret)

	ret_ty := if rt := return_annot {
		annot_ty := c.resolve_type(rt, annot_param_vars)
		resolved_annot := c.engine.find(annot_ty)
		if resolved_annot is ICon && resolved_annot.name == 'Option' {
			resolved_body := c.engine.find(body_ty)
			if resolved_body is ICon && resolved_body.name == 'None' {
				// none → Option(T): compatible, no unify needed
			} else if resolved_body is ICon && resolved_body.name == 'Option' {
				c.engine.unify(annot_ty, body_ty, body.span)
			} else {
				if resolved_annot.args.len > 0 {
					c.engine.unify(resolved_annot.args[0], body_ty, body.span)
				}
			}
		} else {
			c.engine.unify(annot_ty, body_ty, body.span)
		}
		annot_ty
	} else {
		body_ty
	}

	c.current_fn_err = old_fn_err
	c.env.pop_scope()

	c.patch(jump_over, op_arg(.jump, c.current_addr()))

	captured_names := c.capture_names.clone()
	capture_count := captured_names.len

	if !c.check_only {
		c.program.functions << Function{
			name:          name or { '__anon__' }
			arity:         params.len
			locals:        c.local_count
			capture_count: capture_count
			code_start:    func_start
			code_len:      c.current_addr() - func_start - 1
		}
	}
	func_idx := c.program.functions.len - 1

	c.outer_scopes.pop()
	c.locals = old_locals.clone()
	c.local_count = old_local_count
	c.captures = old_captures.clone()
	c.capture_names = old_capture_names

	for cap_name in captured_names {
		if access := c.resolve_variable(cap_name) {
			c.emit_load(access)
		}
	}
	c.emit_arg(.make_closure, func_idx)

	return IFun{
		params: param_tys
		ret:    ret_ty
		err:    declared_err
	}
}

// join_arms computes the common supertype of a set of branch types, handling
// AL's implicit Option lifting: None + T → Option(T). Unlike unify, this does
// NOT force branches to have the same type — it finds a type that all branches
// are compatible with.
fn (mut c Compiler) join_arms(arms []InferType, sp Span) InferType {
	if arms.len == 0 {
		return c.engine.icon_none()
	}

	is_none := fn [mut c] (t InferType) bool {
		r := c.engine.find(t)
		return r is ICon && r.name == 'None'
	}
	is_option := fn [mut c] (t InferType) ?InferType {
		r := c.engine.find(t)
		if r is ICon && r.name == 'Option' && r.args.len > 0 {
			return r.args[0]
		}
		return none
	}

	mut has_none := false
	mut non_none := []InferType{}
	for a in arms {
		if is_none(a) {
			has_none = true
		} else {
			non_none << a
		}
	}

	if non_none.len == 0 {
		return c.engine.icon_none()
	}

	// If None appeared, the result is Option of the join of the non-None arms.
	// Option(T) in that set contributes its inner T (so None + T + Option(T) → Option(T)).
	if has_none {
		mut inners := []InferType{cap: non_none.len}
		for a in non_none {
			if inner := is_option(a) {
				inners << inner
			} else {
				inners << a
			}
		}
		result := inners[0]
		for i := 1; i < inners.len; i++ {
			c.engine.unify(result, inners[i], sp)
		}
		return c.engine.icon_option(result)
	}

	// No None — straight unification.
	result := non_none[0]
	for i := 1; i < non_none.len; i++ {
		c.engine.unify(result, non_none[i], sp)
	}
	return result
}

// ============================================================================
// Pattern matching
// ============================================================================

fn (mut c Compiler) compile_match(m ast.MatchExpression, is_tail bool) InferType {
	subject_ty := c.compile_expr(m.subject)
	mut arm_tys := []InferType{}

	mut end_jumps := []int{}
	mut pats_for_exhaustiveness := []Pat{}

	for arm in m.arms {
		c.emit(.dup)
		mut fail_jumps := []int{}

		c.env.push_scope()

		if detected := c.detect_enum_pattern(arm.pattern, subject_ty) {
			c.compile_enum_arm(m, arm, detected, subject_ty, is_tail, mut arm_tys, mut
				fail_jumps, mut end_jumps, mut pats_for_exhaustiveness)
			c.env.pop_scope()
			continue
		}

		if arm.pattern is ast.OrPattern {
			mut body_jumps := []int{}
			for i, pattern in arm.pattern.patterns {
				if i > 0 {
					c.emit(.dup)
				}
				mut pattern_fails := []int{}
				c.compile_pattern(pattern, subject_ty, mut pattern_fails)
				if i < arm.pattern.patterns.len - 1 {
					body_jumps << c.current_addr()
					c.emit_arg(.jump, 0)
					for ja in pattern_fails {
						c.patch(ja, op_arg(.jump_if_false, c.current_addr()))
					}
				} else {
					fail_jumps << pattern_fails
				}
			}
			for ja in body_jumps {
				c.patch(ja, op_arg(.jump, c.current_addr()))
			}
			c.emit(.pop)
			c.in_tail_position = is_tail
			body_ty := c.compile_expr(arm.body)
			c.in_tail_position = false
			arm_tys << body_ty

			end_jumps << c.current_addr()
			c.emit_arg(.jump, 0)
			for ja in fail_jumps {
				c.patch(ja, op_arg(.jump_if_false, c.current_addr()))
			}

			resolved_subj := c.engine.resolve(subject_ty)
			pats_for_exhaustiveness << ast_pattern_to_pat(arm.pattern, resolved_subj)
			c.env.pop_scope()
			continue
		}

		c.compile_pattern(arm.pattern, subject_ty, mut fail_jumps)

		c.emit(.pop)
		c.in_tail_position = is_tail
		body_ty := c.compile_expr(arm.body)
		c.in_tail_position = false
		arm_tys << body_ty

		end_jumps << c.current_addr()
		c.emit_arg(.jump, 0)
		for ja in fail_jumps {
			c.patch(ja, op_arg(.jump_if_false, c.current_addr()))
		}

		resolved_subj := c.engine.resolve(subject_ty)
		pats_for_exhaustiveness << ast_pattern_to_pat(arm.pattern, resolved_subj)
		c.env.pop_scope()
	}

	c.emit(.pop)
	c.emit(.push_none)

	for ja in end_jumps {
		c.patch(ja, op_arg(.jump, c.current_addr()))
	}

	resolved_subj := c.engine.resolve(subject_ty)
	if missing := check_exhaustiveness(pats_for_exhaustiveness, resolved_subj) {
		c.error('Match is not exhaustive, missing: ${missing}', m.subject.span)
	}

	return c.join_arms(arm_tys, m.span)
}

struct EnumPatternInfo {
	enum_name    string           @[required]
	info         EnumInfo         @[required]
	variant_name string           @[required]
	payload_args []ast.Expression @[required]
	pattern_span Span             @[required]
}

// Detect whether `pattern` is an enum variant pattern, either qualified
// (Enum.Variant / Enum.Variant(args)) or bare (Variant / Variant(args) when
// the subject type resolves to an enum that has that variant).
fn extract_variant_parts(expr ast.Expression) ?(string, []ast.Expression, Span) {
	if expr is ast.FunctionCallExpression {
		return expr.identifier.name, expr.arguments, expr.identifier.span
	}
	if expr is ast.Identifier {
		return expr.name, []ast.Expression{}, expr.span
	}
	return none
}

fn (mut c Compiler) detect_enum_pattern(pattern ast.Expression, subject_ty InferType) ?EnumPatternInfo {
	enum_name, info, variant_name, payload_args, pattern_span := match pattern {
		ast.PropertyAccessExpression {
			left := pattern.left
			if left !is ast.Identifier {
				return none
			}
			left_name := (left as ast.Identifier).name
			found := c.env.lookup_type_info(left_name) or { return none }
			if found !is EnumInfo {
				return none
			}
			efound := found as EnumInfo
			vn, args, ps := extract_variant_parts(pattern.right) or { return none }
			if vn !in efound.variants {
				return none
			}
			left_name, efound, vn, args, ps
		}
		ast.FunctionCallExpression, ast.Identifier {
			resolved := c.engine.find(subject_ty)
			if resolved !is ICon {
				return none
			}
			subj_name := (resolved as ICon).name
			found := c.env.lookup_type_info(subj_name) or { return none }
			if found !is EnumInfo {
				return none
			}
			efound := found as EnumInfo
			vn, args, ps := extract_variant_parts(pattern) or { return none }
			if vn !in efound.variants {
				return none
			}
			subj_name, efound, vn, args, ps
		}
		else {
			return none
		}
	}

	return EnumPatternInfo{
		enum_name:    enum_name
		info:         info
		variant_name: variant_name
		payload_args: payload_args
		pattern_span: pattern_span
	}
}

fn (mut c Compiler) compile_enum_arm(m ast.MatchExpression, arm ast.MatchArm, d EnumPatternInfo, subject_ty InferType, is_tail bool, mut arm_tys []InferType, mut fail_jumps []int, mut end_jumps []int, mut pats []Pat) {
	info := d.info
	enum_name := d.enum_name
	variant_name := d.variant_name

	mut type_args := []InferType{cap: info.type_params.len}
	for _ in 0 .. info.type_params.len {
		type_args << c.engine.fresh_var()
	}
	c.engine.unify(subject_ty, c.engine.icon(enum_name, type_args), m.subject.span)

	mut vars := map[string]InferType{}
	for i, p in info.type_params {
		vars[p] = type_args[i]
	}

	payload_templates := info.variants[variant_name] or { []Type{} }

	enum_ty := c.engine.icon(enum_name, type_args)
	qualified := '${enum_name}.${variant_name}'
	c.record(qualified, enum_ty, d.pattern_span, c.env.lookup_doc(qualified))

	c.emit_arg(.push_const, c.add_constant(info.id))
	c.emit_arg(.push_const, c.add_constant(enum_name))
	c.emit_arg(.push_const, c.add_constant(variant_name))
	c.emit(.match_enum)
	fail_jumps << c.current_addr()
	c.emit_arg(.jump_if_false, 0)

	if d.payload_args.len > 0 {
		if d.payload_args.len != payload_templates.len {
			c.error("Variant '${variant_name}' has ${payload_templates.len} payload field(s), pattern has ${d.payload_args.len}",
				d.pattern_span)
		}
		c.emit(.dup)
		c.emit(.unwrap_enum)
		// unwrap_enum pushes payloads in order (last on top). Stash each into a
		// temp local so failed pattern elements leave a clean stack for the next
		// arm's dup to work against.
		n := d.payload_args.len
		temp_base := c.local_count
		c.local_count += n
		for i := n - 1; i >= 0; i-- {
			c.emit_arg(.store_local, temp_base + i)
		}
		for i, arg in d.payload_args {
			arg_ty := if i < payload_templates.len {
				c.engine.type_to_infer_with_vars(payload_templates[i], vars)
			} else {
				c.engine.fresh_var()
			}
			c.emit_arg(.push_local, temp_base + i)
			c.compile_pattern_element(arg, arg_ty, mut fail_jumps)
		}
	}

	c.emit(.pop)
	c.in_tail_position = is_tail
	body_ty := c.compile_expr(arm.body)
	c.in_tail_position = false
	arm_tys << body_ty

	end_jumps << c.current_addr()
	c.emit_arg(.jump, 0)

	for ja in fail_jumps {
		c.patch(ja, op_arg(.jump_if_false, c.current_addr()))
	}

	// Build the exhaustiveness Pat directly from the detected variant name.
	// ast_pattern_to_pat can't be used here: for bare patterns arm.pattern is
	// a raw Identifier/FunctionCallExpression which would become PatWildcard.
	mut arg_pats := []Pat{cap: d.payload_args.len}
	for i, arg in d.payload_args {
		arg_type := if i < payload_templates.len {
			payload_templates[i]
		} else {
			Type(type_def.TypeNone{})
		}
		arg_pats << ast_pattern_to_pat(arg, arg_type)
	}
	pats << PatCtor{
		name: variant_name
		args: arg_pats
	}
}

fn (mut c Compiler) compile_pattern(pattern ast.Expression, subject_ty InferType, mut fail_jumps []int) {
	match pattern {
		ast.NumberLiteral {
			c.emit(.dup)
			lit_ty := c.compile_expr(pattern)
			c.engine.unify(subject_ty, lit_ty, pattern.span)
			c.emit(.eq)
			fail_jumps << c.current_addr()
			c.emit_arg(.jump_if_false, 0)
		}
		ast.StringLiteral {
			c.emit(.dup)
			c.compile_expr(pattern)
			c.engine.unify(subject_ty, c.engine.icon_string(), pattern.span)
			c.emit(.eq)
			fail_jumps << c.current_addr()
			c.emit_arg(.jump_if_false, 0)
		}
		ast.BooleanLiteral {
			c.emit(.dup)
			c.compile_expr(pattern)
			c.engine.unify(subject_ty, c.engine.icon_bool(), pattern.span)
			c.emit(.eq)
			fail_jumps << c.current_addr()
			c.emit_arg(.jump_if_false, 0)
		}
		ast.Identifier {
			c.emit(.dup)
			local_idx := c.get_or_create_local(pattern.name)
			c.emit_arg(.store_local, local_idx)
			c.env.define_at(pattern.name, mono(subject_ty), def_loc(pattern.span))
			c.record(pattern.name, subject_ty, pattern.span, none)
		}
		ast.WildcardPattern {}
		ast.RangeExpression {
			c.engine.unify(subject_ty, c.engine.icon_int(), pattern.span)
			c.emit(.dup)
			c.compile_expr(pattern.start)
			c.emit(.gte)
			fail_jumps << c.current_addr()
			c.emit_arg(.jump_if_false, 0)
			c.emit(.dup)
			c.compile_expr(pattern.end)
			c.emit(.lt)
			fail_jumps << c.current_addr()
			c.emit_arg(.jump_if_false, 0)
		}
		ast.TupleExpression {
			mut elem_vars := []InferType{cap: pattern.elements.len}
			for _ in 0 .. pattern.elements.len {
				elem_vars << c.engine.fresh_var()
			}
			c.engine.unify(subject_ty, ITuple{ elements: elem_vars }, pattern.span)

			c.emit(.dup)
			c.emit(.array_len)
			c.emit_arg(.push_const, c.add_constant(pattern.elements.len))
			c.emit(.eq)
			fail_jumps << c.current_addr()
			c.emit_arg(.jump_if_false, 0)

			for i, elem in pattern.elements {
				c.emit(.dup)
				c.emit_arg(.tuple_index, i)
				c.compile_pattern_element(elem, elem_vars[i], mut fail_jumps)
			}
		}
		ast.ArrayExpression {
			elem_var := c.engine.fresh_var()
			c.engine.unify(subject_ty, c.engine.icon_array(elem_var), pattern.span)

			has_spread := pattern.elements.len > 0 && pattern.elements.last() is ast.SpreadElement
			pre_count := if has_spread { pattern.elements.len - 1 } else { pattern.elements.len }

			c.emit(.dup)
			c.emit(.array_len)
			c.emit_arg(.push_const, c.add_constant(pre_count))
			if has_spread {
				c.emit(.gte)
			} else {
				c.emit(.eq)
			}
			fail_jumps << c.current_addr()
			c.emit_arg(.jump_if_false, 0)

			for i in 0 .. pre_count {
				elem := pattern.elements[i]
				c.emit(.dup)
				c.emit_arg(.push_const, c.add_constant(i))
				c.emit(.index)
				if elem is ast.Expression {
					c.compile_pattern_element(elem, elem_var, mut fail_jumps)
				}
			}

			if has_spread {
				spread := pattern.elements.last()
				if spread is ast.SpreadElement {
					if inner := spread.expression {
						if inner is ast.Identifier {
							c.emit(.dup)
							c.emit(.array_len)
							c.emit_arg(.push_const, c.add_constant(pre_count))
							c.emit(.swap)
							c.emit(.array_slice)
							local_idx := c.get_or_create_local(inner.name)
							c.emit_arg(.store_local, local_idx)
							rest_ty := c.engine.icon_array(elem_var)
							c.env.define_at(inner.name, mono(rest_ty), def_loc(inner.span))
							c.record(inner.name, rest_ty, inner.span, none)
						}
					}
				}
			}
		}
		else {
			c.emit(.dup)
			pat_ty := c.compile_expr(pattern)
			c.engine.unify(subject_ty, pat_ty, pattern.span)
			c.emit(.eq)
			fail_jumps << c.current_addr()
			c.emit_arg(.jump_if_false, 0)
		}
	}
}

fn (mut c Compiler) compile_pattern_element(pattern ast.Expression, subject_ty InferType, mut fail_jumps []int) {
	match pattern {
		ast.NumberLiteral, ast.StringLiteral, ast.BooleanLiteral {
			lit_ty := c.compile_expr(pattern)
			c.engine.unify(subject_ty, lit_ty, pattern.span)
			c.emit(.eq)
			fail_jumps << c.current_addr()
			c.emit_arg(.jump_if_false, 0)
		}
		ast.Identifier {
			local_idx := c.get_or_create_local(pattern.name)
			c.emit_arg(.store_local, local_idx)
			c.env.define_at(pattern.name, mono(subject_ty), def_loc(pattern.span))
			c.record(pattern.name, subject_ty, pattern.span, none)
		}
		ast.WildcardPattern {
			c.emit(.pop)
		}
		ast.RangeExpression {
			c.engine.unify(subject_ty, c.engine.icon_int(), pattern.span)
			temp_idx := c.local_count
			c.local_count++
			c.emit(.dup)
			c.emit_arg(.store_local, temp_idx)
			c.compile_expr(pattern.start)
			c.emit(.gte)
			fail_jumps << c.current_addr()
			c.emit_arg(.jump_if_false, 0)
			c.emit_arg(.push_local, temp_idx)
			c.compile_expr(pattern.end)
			c.emit(.lt)
			fail_jumps << c.current_addr()
			c.emit_arg(.jump_if_false, 0)
		}
		ast.TupleExpression {
			mut elem_vars := []InferType{cap: pattern.elements.len}
			for _ in 0 .. pattern.elements.len {
				elem_vars << c.engine.fresh_var()
			}
			c.engine.unify(subject_ty, ITuple{ elements: elem_vars }, pattern.span)

			temp_idx := c.local_count
			c.local_count++
			c.emit_arg(.store_local, temp_idx)

			c.emit_arg(.push_local, temp_idx)
			c.emit(.array_len)
			c.emit_arg(.push_const, c.add_constant(pattern.elements.len))
			c.emit(.eq)
			fail_jumps << c.current_addr()
			c.emit_arg(.jump_if_false, 0)

			for i, elem in pattern.elements {
				c.emit_arg(.push_local, temp_idx)
				c.emit_arg(.tuple_index, i)
				c.compile_pattern_element(elem, elem_vars[i], mut fail_jumps)
			}
		}
		ast.ArrayExpression {
			elem_var := c.engine.fresh_var()
			c.engine.unify(subject_ty, c.engine.icon_array(elem_var), pattern.span)

			has_spread := pattern.elements.len > 0 && pattern.elements.last() is ast.SpreadElement
			pre_count := if has_spread { pattern.elements.len - 1 } else { pattern.elements.len }

			temp_idx := c.local_count
			c.local_count++
			c.emit_arg(.store_local, temp_idx)

			c.emit_arg(.push_local, temp_idx)
			c.emit(.array_len)
			c.emit_arg(.push_const, c.add_constant(pre_count))
			if has_spread {
				c.emit(.gte)
			} else {
				c.emit(.eq)
			}
			fail_jumps << c.current_addr()
			c.emit_arg(.jump_if_false, 0)

			for i in 0 .. pre_count {
				elem := pattern.elements[i]
				c.emit_arg(.push_local, temp_idx)
				c.emit_arg(.push_const, c.add_constant(i))
				c.emit(.index)
				if elem is ast.Expression {
					c.compile_pattern_element(elem, elem_var, mut fail_jumps)
				}
			}

			if has_spread {
				spread := pattern.elements.last()
				if spread is ast.SpreadElement {
					if inner := spread.expression {
						if inner is ast.Identifier {
							c.emit_arg(.push_local, temp_idx)
							c.emit_arg(.push_const, c.add_constant(pre_count))
							c.emit_arg(.push_local, temp_idx)
							c.emit(.array_len)
							c.emit(.array_slice)
							local_idx := c.get_or_create_local(inner.name)
							c.emit_arg(.store_local, local_idx)
							rest_ty := c.engine.icon_array(elem_var)
							c.env.define_at(inner.name, mono(rest_ty), def_loc(inner.span))
							c.record(inner.name, rest_ty, inner.span, none)
						}
					}
				}
			}
		}
		else {
			pat_ty := c.compile_expr(pattern)
			c.engine.unify(subject_ty, pat_ty, pattern.span)
			c.emit(.eq)
			fail_jumps << c.current_addr()
			c.emit_arg(.jump_if_false, 0)
		}
	}
}

// ============================================================================
// Builtin calls
// ============================================================================

fn (mut c Compiler) compile_builtin_call(call ast.FunctionCallExpression) InferType {
	name := call.identifier.name

	scheme := c.env.lookup(name) or {
		if suggestion := c.env.suggest_name(name) {
			c.error("Unknown function '${name}'. Did you mean '${suggestion}'?", call.identifier.span)
		} else {
			c.error("Unknown function '${name}'", call.identifier.span)
		}
		for arg in call.arguments {
			c.compile_expr(arg)
		}
		return c.engine.fresh_var()
	}

	fn_ty := c.engine.instantiate(scheme)
	c.record(name, fn_ty, call.identifier.span, none)
	ret_ty := c.call_against(fn_ty, call.arguments, call.span)

	match name {
		'println' {
			c.emit(.print)
			c.emit(.push_none)
		}
		'inspect' {
			c.emit(.to_string)
		}
		'__stack_depth__' {
			c.emit(.stack_depth)
		}
		'read_file' {
			c.emit(.file_read)
		}
		'write_file' {
			c.emit(.file_write)
		}
		'tcp_listen' {
			c.emit(.tcp_listen)
		}
		'tcp_accept' {
			c.emit(.tcp_accept)
		}
		'tcp_read' {
			c.emit(.tcp_read)
		}
		'tcp_write' {
			c.emit(.tcp_write)
		}
		'tcp_close' {
			c.emit(.tcp_close)
		}
		'str_split' {
			c.emit(.str_split)
		}
		else {
			c.error("Internal: '${name}' has a builtin scheme but no codegen", call.span)
		}
	}

	return ret_ty
}
