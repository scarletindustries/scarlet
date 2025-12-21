module bytecode

import typed_ast
import type_def { TypeEnum, TypeOption, TypeResult, type_to_string }
import types { TypeEnv }

fn (c Compiler) find_enum_for_variant(variant_name string) ?string {
	if enum_type := c.type_env.lookup_enum_by_variant(variant_name) {
		return enum_type.name
	}
	return none
}

struct Scope {
	locals map[string]int
}

@[params]
pub struct CompileOptions {
pub:
	expose_debug_builtins bool
}

struct Compiler {
	options  CompileOptions
	type_env TypeEnv
mut:
	program          Program
	locals           map[string]int
	outer_scopes     []Scope
	local_count      int
	current_func_idx int
	captures         map[string]int
	capture_names    []string
	current_binding  string
	in_tail_position bool
}

pub fn compile(expr typed_ast.Expression, type_env TypeEnv, options CompileOptions) !Program {
	mut c := Compiler{
		options:          options
		type_env:         type_env
		program:          Program{
			constants: []
			functions: []
			code:      []
			entry:     0
		}
		locals:           {}
		outer_scopes:     []
		local_count:      0
		current_func_idx: -1
		captures:         {}
		capture_names:    []
	}

	main_start := c.program.code.len

	c.compile_expr(expr)!
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

	return c.program
}

fn (mut c Compiler) emit(o Op) {
	c.program.code << op(o)
}

fn (mut c Compiler) emit_arg(o Op, operand int) {
	c.program.code << op_arg(o, operand)
}

fn (mut c Compiler) current_addr() int {
	return c.program.code.len
}

fn (mut c Compiler) add_constant(v Value) int {
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

struct VarAccess {
	is_local   bool
	is_capture bool
	is_self    bool
	index      int
}

fn (mut c Compiler) resolve_variable(name string) ?VarAccess {
	if idx := c.locals[name] {
		return VarAccess{
			is_local: true
			index:    idx
		}
	}

	if idx := c.captures[name] {
		return VarAccess{
			is_capture: true
			index:      idx
		}
	}

	for scope in c.outer_scopes {
		if name in scope.locals {
			if name == c.current_binding {
				return VarAccess{
					is_self: true
				}
			}

			capture_idx := c.capture_names.len
			c.captures[name] = capture_idx
			c.capture_names << name
			return VarAccess{
				is_capture: true
				index:      capture_idx
			}
		}
	}

	return none
}

fn (mut c Compiler) compile_expr_with_hint(expr typed_ast.Expression, expected_type string) ! {
	if expected_type != '' {
		if enum_type := c.type_env.lookup_type(expected_type) {
			if enum_type is TypeEnum {
				if expr is typed_ast.FunctionCallExpression {
					call := expr as typed_ast.FunctionCallExpression
					if call.identifier.name in enum_type.variants {
						enum_idx := c.add_constant(expected_type)
						c.emit_arg(.push_const, enum_idx)
						variant_idx := c.add_constant(call.identifier.name)
						c.emit_arg(.push_const, variant_idx)

						if call.arguments.len == 1 {
							c.compile_expr(call.arguments[0])!
							c.emit(.make_enum_payload)
						} else if call.arguments.len == 0 {
							c.emit(.make_enum)
						} else {
							return error('Enum variant takes 0 or 1 argument')
						}
						return
					}
				}

				if expr is typed_ast.Identifier {
					ident := expr as typed_ast.Identifier
					if ident.name in enum_type.variants {
						enum_idx := c.add_constant(expected_type)
						c.emit_arg(.push_const, enum_idx)
						variant_idx := c.add_constant(ident.name)
						c.emit_arg(.push_const, variant_idx)
						c.emit(.make_enum)
						return
					}
				}
			}
		}
	}

	c.compile_expr(expr)!
}

fn (mut c Compiler) compile_expr(expr typed_ast.Expression) ! {
	is_tail := c.in_tail_position
	c.in_tail_position = false

	match expr {
		typed_ast.BlockExpression {
			for i, e in expr.body {
				c.in_tail_position = is_tail && i == expr.body.len - 1
				c.compile_expr(e)!
				c.in_tail_position = false
				if i < expr.body.len - 1 {
					c.emit(.pop)
				}
			}

			if expr.body.len == 0 {
				c.emit(.push_none)
			}
		}
		typed_ast.NumberLiteral {
			if expr.value.contains('.') {
				val := expr.value.f64()
				idx := c.add_constant(val)
				c.emit_arg(.push_const, idx)
			} else {
				val := expr.value.int()
				idx := c.add_constant(val)
				c.emit_arg(.push_const, idx)
			}
		}
		typed_ast.StringLiteral {
			idx := c.add_constant(expr.value)
			c.emit_arg(.push_const, idx)
		}
		typed_ast.InterpolatedString {
			if expr.parts.len == 0 {
				idx := c.add_constant('')
				c.emit_arg(.push_const, idx)
			} else {
				c.compile_expr(expr.parts[0])!
				c.emit(.to_string)

				for i := 1; i < expr.parts.len; i++ {
					c.compile_expr(expr.parts[i])!
					c.emit(.to_string)
					c.emit(.str_concat)
				}
			}
		}
		typed_ast.BooleanLiteral {
			if expr.value {
				c.emit(.push_true)
			} else {
				c.emit(.push_false)
			}
		}
		typed_ast.NoneExpression {
			c.emit(.push_none)
		}
		typed_ast.Identifier {
			if access := c.resolve_variable(expr.name) {
				if access.is_local {
					c.emit_arg(.push_local, access.index)
				} else if access.is_capture {
					c.emit_arg(.push_capture, access.index)
				} else if access.is_self {
					c.emit(.push_self)
				}
			} else {
				return error('Undefined variable: ${expr.name}')
			}
		}
		typed_ast.VariableBinding {
			idx := c.get_or_create_local(expr.identifier.name)

			old_binding := c.current_binding
			c.current_binding = expr.identifier.name
			c.compile_expr(expr.init)!
			c.current_binding = old_binding

			c.emit_arg(.store_local, idx)
			c.emit(.push_none)
		}
		typed_ast.ConstBinding {
			c.compile_expr(expr.init)!
			idx := c.get_or_create_local(expr.identifier.name)
			c.emit_arg(.store_local, idx)
			c.emit(.push_none)
		}
		typed_ast.BinaryExpression {
			if expr.op.kind == .logical_and {
				c.compile_expr(expr.left)!
				c.emit(.dup)

				end_jump := c.current_addr()
				c.emit_arg(.jump_if_false, 0)
				c.emit(.pop)
				c.compile_expr(expr.right)!
				c.program.code[end_jump] = op_arg(.jump_if_false, c.current_addr())
				return
			}
			if expr.op.kind == .logical_or {
				c.compile_expr(expr.left)!
				c.emit(.dup)

				end_jump := c.current_addr()
				c.emit_arg(.jump_if_true, 0)
				c.emit(.pop)
				c.compile_expr(expr.right)!
				c.program.code[end_jump] = op_arg(.jump_if_true, c.current_addr())
				return
			}

			c.compile_expr(expr.left)!
			c.compile_expr(expr.right)!
			match expr.op.kind {
				.punc_plus {
					c.emit(.add)
				}
				.punc_minus {
					c.emit(.sub)
				}
				.punc_mul {
					c.emit(.mul)
				}
				.punc_div {
					c.emit(.div)
				}
				.punc_mod {
					c.emit(.mod)
				}
				.punc_equals_comparator {
					c.emit(.eq)
				}
				.punc_not_equal {
					c.emit(.neq)
				}
				.punc_lt {
					c.emit(.lt)
				}
				.punc_gt {
					c.emit(.gt)
				}
				.punc_lte {
					c.emit(.lte)
				}
				.punc_gte {
					c.emit(.gte)
				}
				else {
					return error('Unknown binary operator: ${expr.op.kind}')
				}
			}
		}
		typed_ast.UnaryExpression {
			c.compile_expr(expr.expression)!
			match expr.op.kind {
				.punc_exclamation_mark {
					c.emit(.not)
				}
				.punc_minus {
					c.emit(.neg)
				}
				else {
					return error('Unknown unary operator: ${expr.op.kind}')
				}
			}
		}
		typed_ast.IfExpression {
			c.compile_expr(expr.condition)!

			else_jump := c.current_addr()
			c.emit_arg(.jump_if_false, 0)

			c.in_tail_position = is_tail
			c.compile_expr(expr.body)!
			c.in_tail_position = false

			end_jump := c.current_addr()
			c.emit_arg(.jump, 0)

			else_addr := c.current_addr()
			c.program.code[else_jump] = op_arg(.jump_if_false, else_addr)

			c.in_tail_position = is_tail
			if else_body := expr.else_body {
				c.compile_expr(else_body)!
			} else {
				c.emit(.push_none)
			}
			c.in_tail_position = false

			end_addr := c.current_addr()
			c.program.code[end_jump] = op_arg(.jump, end_addr)
		}
		typed_ast.MatchExpression {
			c.compile_match(expr, is_tail)!
		}
		typed_ast.ArrayExpression {
			for elem in expr.elements {
				c.compile_expr(elem)!
			}
			c.emit_arg(.make_array, expr.elements.len)
		}
		typed_ast.ArrayIndexExpression {
			c.compile_expr(expr.expression)!
			c.compile_expr(expr.index)!
			c.emit(.index)
		}
		typed_ast.RangeExpression {
			c.compile_expr(expr.start)!
			c.compile_expr(expr.end)!
			c.emit(.make_range)
		}
		typed_ast.FunctionExpression {
			c.compile_function(expr)!
		}
		typed_ast.FunctionCallExpression {
			func_type := c.type_env.lookup_function(expr.identifier.name)

			for i, arg in expr.arguments {
				expected_type := if ft := func_type {
					if i < ft.params.len {
						type_to_string(ft.params[i])
					} else {
						''
					}
				} else {
					''
				}
				c.compile_expr_with_hint(arg, expected_type)!
			}

			if access := c.resolve_variable(expr.identifier.name) {
				if access.is_local {
					c.emit_arg(.push_local, access.index)
				} else if access.is_capture {
					c.emit_arg(.push_capture, access.index)
				} else if access.is_self {
					c.emit(.push_self)
				}

				if is_tail {
					c.emit_arg(.tail_call, expr.arguments.len)
				} else {
					c.emit_arg(.call, expr.arguments.len)
				}
			} else {
				c.compile_builtin_call(expr)!
			}
		}
		typed_ast.PropertyAccessExpression {
			if expr.left is typed_ast.Identifier {
				left_id := expr.left as typed_ast.Identifier
				if enum_type := c.type_env.lookup_type(left_id.name) {
					if enum_type is TypeEnum {
						enum_name := left_id.name

						if expr.right is typed_ast.FunctionCallExpression {
							call := expr.right as typed_ast.FunctionCallExpression
							variant_name := call.identifier.name

							if variant_name !in enum_type.variants {
								return error('Unknown variant "${variant_name}" in enum ${enum_name}')
							}

							if _ := enum_type.variants[variant_name] {
								if call.arguments.len != 1 {
									return error('Variant "${variant_name}" expects exactly 1 payload argument')
								}

								enum_idx := c.add_constant(enum_name)
								c.emit_arg(.push_const, enum_idx)
								variant_idx := c.add_constant(variant_name)
								c.emit_arg(.push_const, variant_idx)
								c.compile_expr(call.arguments[0])!
								c.emit(.make_enum_payload)
							} else {
								return error('Variant "${variant_name}" does not take a payload')
							}
						} else if expr.right is typed_ast.Identifier {
							variant_id := expr.right as typed_ast.Identifier
							variant_name := variant_id.name

							if variant_name !in enum_type.variants {
								return error('Unknown variant "${variant_name}" in enum ${enum_name}')
							}

							if payload_type := enum_type.variants[variant_name] {
								return error('Variant "${variant_name}" requires a payload of type ${type_to_string(payload_type)}')
							}

							enum_idx := c.add_constant(enum_name)
							c.emit_arg(.push_const, enum_idx)
							variant_idx := c.add_constant(variant_name)
							c.emit_arg(.push_const, variant_idx)
							c.emit(.make_enum)
						}
						return
					}
				}
			}

			c.compile_expr(expr.left)!

			if expr.right is typed_ast.FunctionCallExpression {
				call := expr.right as typed_ast.FunctionCallExpression

				for arg in call.arguments {
					c.compile_expr(arg)!
				}

				return error('Method calls not yet implemented')
			} else if expr.right is typed_ast.Identifier {
				id := expr.right as typed_ast.Identifier

				idx := c.add_constant(id.name)
				c.emit_arg(.get_field, idx)
			}
		}
		typed_ast.StructExpression {
			c.emit(.push_none)
		}
		typed_ast.EnumExpression {
			c.emit(.push_none)
		}
		typed_ast.StructInitExpression {
			struct_name := expr.identifier.name

			struct_type := c.type_env.lookup_struct(struct_name) or {
				return error('Unknown struct type: ${struct_name}')
			}

			mut provided := map[string]bool{}
			for field in expr.fields {
				field_name := field.identifier.name
				if field_name !in struct_type.fields {
					return error('Unknown field "${field_name}" in struct ${struct_name}')
				}
				if field_name in provided {
					return error('Duplicate field "${field_name}" in struct ${struct_name}')
				}
				provided[field_name] = true
			}

			for field_name, _ in struct_type.fields {
				if field_name !in provided {
					return error('Missing field "${field_name}" in struct ${struct_name}')
				}
			}

			for field in expr.fields {
				name_idx := c.add_constant(field.identifier.name)
				c.emit_arg(.push_const, name_idx)
				c.compile_expr(field.init)!
			}
			type_idx := c.add_constant(expr.identifier.name)
			c.emit_arg(.push_const, type_idx)
			c.emit_arg(.make_struct, expr.fields.len)
		}
		typed_ast.AssertExpression {
			c.compile_expr(expr.expression)!

			ok_jump := c.current_addr()
			c.emit_arg(.jump_if_true, 0)

			c.compile_expr(expr.message)!
			c.emit(.make_error)
			c.emit(.ret)

			c.program.code[ok_jump] = op_arg(.jump_if_true, c.current_addr())
			c.emit(.push_none)
		}
		typed_ast.ExportExpression {
			c.compile_expr(expr.expression)!
		}
		typed_ast.ImportDeclaration {
			c.emit(.push_none)
		}
		typed_ast.ErrorExpression {
			c.compile_expr(expr.expression)!
			c.emit(.make_error)
		}
		typed_ast.OrExpression {
			c.compile_expr(expr.expression)!

			resolved := expr.resolved_type
			if resolved is TypeResult {
				c.emit(.dup)
				c.emit(.is_error)

				not_error_jump := c.current_addr()
				c.emit_arg(.jump_if_false, 0)

				c.emit(.unwrap_error)
				if receiver := expr.receiver {
					idx := c.get_or_create_local(receiver.name)
					c.emit_arg(.store_local, idx)
				} else {
					c.emit(.pop)
				}

				c.compile_expr(expr.body)!

				end_jump := c.current_addr()
				c.emit_arg(.jump, 0)

				c.program.code[not_error_jump] = op_arg(.jump_if_false, c.current_addr())
				c.program.code[end_jump] = op_arg(.jump, c.current_addr())
			} else if resolved is TypeOption {
				c.emit(.dup)
				c.emit(.is_none)

				not_none_jump := c.current_addr()
				c.emit_arg(.jump_if_false, 0)

				c.emit(.pop)

				c.compile_expr(expr.body)!

				end_jump := c.current_addr()
				c.emit_arg(.jump, 0)

				c.program.code[not_none_jump] = op_arg(.jump_if_false, c.current_addr())
				c.program.code[end_jump] = op_arg(.jump, c.current_addr())
			}
		}
		typed_ast.PropagateNoneExpression {
			c.compile_expr(expr.expression)!

			resolved := expr.resolved_type
			if resolved is TypeOption {
				c.emit(.dup)
				c.emit(.is_none)

				not_none_jump := c.current_addr()
				c.emit_arg(.jump_if_false, 0)

				c.emit(.ret)

				c.program.code[not_none_jump] = op_arg(.jump_if_false, c.current_addr())
			}
		}
		else {
			return error('Cannot compile expression type: ${expr.type_name()}')
		}
	}
}

fn (mut c Compiler) compile_function(func typed_ast.FunctionExpression) ! {
	old_locals := c.locals.clone()
	old_local_count := c.local_count
	old_captures := c.captures.clone()
	old_capture_names := c.capture_names.clone()

	mut scope_locals := old_locals.clone()
	if id := func.identifier {
		scope_locals[id.name] = c.local_count
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

	for param in func.params {
		c.get_or_create_local(param.identifier.name)
	}

	func_start := c.current_addr()

	old_binding := c.current_binding
	if id := func.identifier {
		c.current_binding = id.name
	}

	old_tail := c.in_tail_position
	c.in_tail_position = true
	c.compile_expr(func.body)!
	c.in_tail_position = old_tail
	c.current_binding = old_binding
	c.emit(.ret)

	c.program.code[jump_over] = op_arg(.jump, c.current_addr())

	captured_names := c.capture_names.clone()
	capture_count := captured_names.len

	func_idx := c.program.functions.len
	mut name := '__anon__'
	if id := func.identifier {
		name = id.name
	}
	c.program.functions << Function{
		name:          name
		arity:         func.params.len
		locals:        c.local_count
		capture_count: capture_count
		code_start:    func_start
		code_len:      c.current_addr() - func_start - 1
	}

	c.outer_scopes.pop()

	c.locals = old_locals.clone()
	c.local_count = old_local_count
	c.captures = old_captures.clone()
	c.capture_names = old_capture_names.clone()

	for cap_name in captured_names {
		if access := c.resolve_variable(cap_name) {
			if access.is_local {
				c.emit_arg(.push_local, access.index)
			} else if access.is_capture {
				c.emit_arg(.push_capture, access.index)
			}
		}
	}

	c.emit_arg(.make_closure, func_idx)

	if id := func.identifier {
		idx := c.get_or_create_local(id.name)
		c.emit_arg(.store_local, idx)
		c.emit(.push_none)
	}
}

fn (mut c Compiler) compile_match(m typed_ast.MatchExpression, is_tail bool) ! {
	c.compile_expr(m.subject)!

	mut end_jumps := []int{}

	for arm in m.arms {
		c.emit(.dup)

		mut binding_name := ?string(none)
		mut literal_pattern := ?typed_ast.Expression(none)
		mut enum_name := ?string(none)
		mut variant_name := ?string(none)

		if arm.pattern is typed_ast.PropertyAccessExpression {
			prop := arm.pattern as typed_ast.PropertyAccessExpression
			if prop.left is typed_ast.Identifier {
				left_id := prop.left as typed_ast.Identifier
				if enum_type := c.type_env.lookup_type(left_id.name) {
					if enum_type is TypeEnum {
						enum_name = left_id.name
						if prop.right is typed_ast.FunctionCallExpression {
							call := prop.right as typed_ast.FunctionCallExpression
							variant_name = call.identifier.name
							if call.arguments.len == 1 {
								arg := call.arguments[0]
								if arg is typed_ast.Identifier {
									binding_id := arg as typed_ast.Identifier
									binding_name = binding_id.name
								} else if arg is typed_ast.StringLiteral
									|| arg is typed_ast.NumberLiteral
									|| arg is typed_ast.BooleanLiteral {
									literal_pattern = arg
								}
							}
						} else if prop.right is typed_ast.Identifier {
							right_id := prop.right as typed_ast.Identifier
							variant_name = right_id.name
						}
					}
				}
			}
		}

		if arm.pattern is typed_ast.FunctionCallExpression {
			call := arm.pattern as typed_ast.FunctionCallExpression
			if inferred_enum := c.find_enum_for_variant(call.identifier.name) {
				enum_name = inferred_enum
				variant_name = call.identifier.name
				if call.arguments.len == 1 {
					arg := call.arguments[0]
					if arg is typed_ast.Identifier {
						binding_id := arg as typed_ast.Identifier
						binding_name = binding_id.name
					} else if arg is typed_ast.StringLiteral || arg is typed_ast.NumberLiteral
						|| arg is typed_ast.BooleanLiteral {
						literal_pattern = arg
					}
				}
			}
		}

		if arm.pattern is typed_ast.Identifier {
			ident := arm.pattern as typed_ast.Identifier
			if inferred_enum := c.find_enum_for_variant(ident.name) {
				enum_name = inferred_enum
				variant_name = ident.name
			}
		}

		if ename := enum_name {
			if vname := variant_name {
				enum_idx := c.add_constant(ename)
				c.emit_arg(.push_const, enum_idx)
				variant_idx := c.add_constant(vname)
				c.emit_arg(.push_const, variant_idx)
				c.emit(.match_enum)

				next_arm := c.current_addr()
				c.emit_arg(.jump_if_false, 0)

				if lit := literal_pattern {
					c.emit(.dup)
					c.emit(.unwrap_enum)
					c.compile_expr(lit)!
					c.emit(.eq)
					payload_match := c.current_addr()
					c.emit_arg(.jump_if_false, 0)

					c.emit(.pop)
					c.in_tail_position = is_tail
					c.compile_expr(arm.body)!
					c.in_tail_position = false

					end_jumps << c.current_addr()
					c.emit_arg(.jump, 0)

					c.program.code[next_arm] = op_arg(.jump_if_false, c.current_addr())
					c.program.code[payload_match] = op_arg(.jump_if_false, c.current_addr())
					continue
				}

				if bname := binding_name {
					c.emit(.dup)
					c.emit(.unwrap_enum)
					local_idx := c.get_or_create_local(bname)
					c.emit_arg(.store_local, local_idx)
				}

				c.emit(.pop)
				c.in_tail_position = is_tail
				c.compile_expr(arm.body)!
				c.in_tail_position = false

				end_jumps << c.current_addr()
				c.emit_arg(.jump, 0)

				c.program.code[next_arm] = op_arg(.jump_if_false, c.current_addr())
				continue
			}
		}

		if arm.pattern is typed_ast.WildcardPattern {
			c.emit(.pop)
			c.emit(.pop)
			c.in_tail_position = is_tail
			c.compile_expr(arm.body)!
			c.in_tail_position = false

			end_jumps << c.current_addr()
			c.emit_arg(.jump, 0)
			continue
		}

		c.compile_expr(arm.pattern)!
		c.emit(.eq)

		next_arm := c.current_addr()
		c.emit_arg(.jump_if_false, 0)

		c.emit(.pop)
		c.in_tail_position = is_tail
		c.compile_expr(arm.body)!
		c.in_tail_position = false

		end_jumps << c.current_addr()
		c.emit_arg(.jump, 0)

		c.program.code[next_arm] = op_arg(.jump_if_false, c.current_addr())
	}

	c.emit(.pop)
	c.emit(.push_none)

	end_addr := c.current_addr()
	for jump_addr in end_jumps {
		c.program.code[jump_addr] = op_arg(.jump, end_addr)
	}
}

fn (mut c Compiler) compile_builtin_call(call typed_ast.FunctionCallExpression) ! {
	match call.identifier.name {
		'println' {
			if call.arguments.len != 1 {
				return error('println expects 1 argument')
			}
			c.compile_expr(call.arguments[0])!
			c.emit(.print)
			c.emit(.push_none)
		}
		'inspect' {
			if call.arguments.len != 1 {
				return error('inspect expects 1 argument')
			}
			c.compile_expr(call.arguments[0])!
			c.emit(.to_string)
		}
		'__stack_depth__' {
			if !c.options.expose_debug_builtins {
				return error('Unknown function: ${call.identifier.name}')
			}
			if call.arguments.len != 0 {
				return error('__stack_depth__ expects 0 arguments')
			}
			c.emit(.stack_depth)
		}
		'read_file' {
			if call.arguments.len != 1 {
				return error('read_file expects 1 argument (path)')
			}
			c.compile_expr(call.arguments[0])!
			c.emit(.file_read)
		}
		'write_file' {
			if call.arguments.len != 2 {
				return error('write_file expects 2 arguments (path, content)')
			}
			c.compile_expr(call.arguments[0])!
			c.compile_expr(call.arguments[1])!
			c.emit(.file_write)
		}
		'tcp_listen' {
			if call.arguments.len != 1 {
				return error('tcp_listen expects 1 argument (port)')
			}
			c.compile_expr(call.arguments[0])!
			c.emit(.tcp_listen)
		}
		'tcp_accept' {
			if call.arguments.len != 1 {
				return error('tcp_accept expects 1 argument (listener)')
			}
			c.compile_expr(call.arguments[0])!
			c.emit(.tcp_accept)
		}
		'tcp_read' {
			if call.arguments.len != 1 {
				return error('tcp_read expects 1 argument (socket)')
			}
			c.compile_expr(call.arguments[0])!
			c.emit(.tcp_read)
		}
		'tcp_write' {
			if call.arguments.len != 2 {
				return error('tcp_write expects 2 arguments (socket, data)')
			}
			c.compile_expr(call.arguments[0])!
			c.compile_expr(call.arguments[1])!
			c.emit(.tcp_write)
		}
		'tcp_close' {
			if call.arguments.len != 1 {
				return error('tcp_close expects 1 argument (socket)')
			}
			c.compile_expr(call.arguments[0])!
			c.emit(.tcp_close)
		}
		else {
			return error('Unknown function: ${call.identifier.name}')
		}
	}
}
