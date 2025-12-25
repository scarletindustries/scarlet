module bytecode

import flags { Flags }
import typed_ast
import type_def { TypeEnum, TypeOption, TypeResult, type_to_string }
import types { TypeEnv }

struct Scope {
	locals map[string]int
}

struct Compiler {
	flags    Flags
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

pub fn compile(expr typed_ast.Expression, type_env TypeEnv, fl Flags) !Program {
	mut c := Compiler{
		flags:            fl
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

fn (mut c Compiler) compile_node(node typed_ast.Node) ! {
	match node {
		typed_ast.Statement { c.compile_statement(node)! }
		typed_ast.Expression { c.compile_expr(node)! }
	}
}

fn (mut c Compiler) compile_statement(stmt typed_ast.Statement) ! {
	match stmt {
		typed_ast.VariableBinding {
			idx := c.get_or_create_local(stmt.identifier.name)

			old_binding := c.current_binding
			c.current_binding = stmt.identifier.name
			c.compile_expr(stmt.init)!
			c.current_binding = old_binding

			c.emit_arg(.store_local, idx)
		}
		typed_ast.ConstBinding {
			c.compile_expr(stmt.init)!
			idx := c.get_or_create_local(stmt.identifier.name)
			c.emit_arg(.store_local, idx)
		}
		typed_ast.TypePatternBinding {
			c.compile_expr(stmt.init)!
			c.emit(.pop)
		}
		typed_ast.TupleDestructuringBinding {
			c.compile_expr(stmt.init)!
			for i, pattern in stmt.patterns {
				if pattern is typed_ast.Identifier {
					c.emit(.dup)
					c.emit_arg(.tuple_index, i)
					idx := c.get_or_create_local(pattern.name)
					c.emit_arg(.store_local, idx)
				}
			}
			c.emit(.pop)
		}
		typed_ast.FunctionDeclaration {
			c.compile_function_common(stmt.identifier.name, stmt.params, stmt.body)!
			idx := c.get_or_create_local(stmt.identifier.name)
			c.emit_arg(.store_local, idx)
		}
		typed_ast.StructDeclaration {}
		typed_ast.EnumDeclaration {}
		typed_ast.ImportDeclaration {}
		typed_ast.ExportDeclaration {
			c.compile_statement(stmt.declaration)!
		}
	}
}

fn (mut c Compiler) compile_expr_with_hint(expr typed_ast.Expression, expected_type string) ! {
	if expected_type != '' {
		if enum_type := c.type_env.lookup_type(expected_type) {
			if enum_type is TypeEnum {
				if expr is typed_ast.FunctionCallExpression {
					call := expr as typed_ast.FunctionCallExpression
					if call.identifier.name in enum_type.variants {
						c.emit_arg(.push_const, c.add_constant(enum_type.id))
						enum_idx := c.add_constant(expected_type)
						c.emit_arg(.push_const, enum_idx)
						variant_idx := c.add_constant(call.identifier.name)
						c.emit_arg(.push_const, variant_idx)

						if call.arguments.len >= 1 {
							for arg in call.arguments {
								c.compile_expr(arg)!
							}
							c.emit_arg(.make_enum_payload, call.arguments.len)
						} else {
							c.emit(.make_enum)
						}
						return
					}
				}

				if expr is typed_ast.Identifier {
					ident := expr as typed_ast.Identifier
					if ident.name in enum_type.variants {
						c.emit_arg(.push_const, c.add_constant(enum_type.id))
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
			for i, node in expr.body {
				is_last := i == expr.body.len - 1
				c.in_tail_position = is_tail && is_last
				c.compile_node(node)!
				c.in_tail_position = false
				// only pop after expressions, not statements (statements don't push)
				if !is_last && node is typed_ast.Expression {
					c.emit(.pop)
				}
			}

			// push none if block was empty or last item was a statement
			if expr.body.len == 0 {
				c.emit(.push_none)
			} else if expr.body[expr.body.len - 1] is typed_ast.Statement {
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
			// Check if there are any spread expressions
			has_spread := expr.elements.any(it is typed_ast.SpreadExpression)

			if !has_spread {
				// Simple case: no spreads, just compile each element
				for elem in expr.elements {
					c.compile_expr(elem)!
				}
				c.emit_arg(.make_array, expr.elements.len)
			} else {
				// Complex case: handle spreads by building array incrementally
				mut have_result := false

				mut i := 0
				for i < expr.elements.len {
					elem := expr.elements[i]

					if elem is typed_ast.SpreadExpression {
						// Spread: compile inner array and concat
						inner := elem.expression or {
							return error('Spread in array literal missing expression')
						}

						c.compile_expr(inner)!
						if have_result {
							c.emit(.array_concat)
						} else {
							have_result = true
						}
						i++
					} else {
						// Group consecutive non-spread elements
						mut group_count := 0
						for j := i; j < expr.elements.len; j++ {
							if expr.elements[j] is typed_ast.SpreadExpression {
								break
							}
							c.compile_expr(expr.elements[j])!
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

				// Handle empty array case (no elements at all)
				if !have_result {
					c.emit_arg(.make_array, 0)
				}
			}
		}
		typed_ast.TupleExpression {
			for elem in expr.elements {
				c.compile_expr(elem)!
			}
			c.emit_arg(.make_tuple, expr.elements.len)
		}
		typed_ast.ArrayIndexExpression {
			c.compile_expr(expr.expression)!
			if expr.index is typed_ast.RangeExpression {
				range_idx := expr.index as typed_ast.RangeExpression
				c.compile_expr(range_idx.start)!
				c.compile_expr(range_idx.end)!
				c.emit(.array_slice)
			} else {
				c.compile_expr(expr.index)!
				c.emit(.index)
			}
		}
		typed_ast.RangeExpression {
			c.compile_expr(expr.start)!
			c.compile_expr(expr.end)!
			c.emit(.make_range)
		}
		typed_ast.SpreadExpression {
			return error('SpreadExpression should only appear inside ArrayExpression')
		}
		typed_ast.FunctionExpression {
			c.compile_function_expression(expr)!
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

							payload_types := enum_type.variants[variant_name] or {
								[]type_def.Type{}
							}
							if payload_types.len > 0 {
								if call.arguments.len != payload_types.len {
									return error('Variant "${variant_name}" expects ${payload_types.len} payload argument(s)')
								}

								c.emit_arg(.push_const, c.add_constant(enum_type.id))
								enum_idx := c.add_constant(enum_name)
								c.emit_arg(.push_const, enum_idx)
								variant_idx := c.add_constant(variant_name)
								c.emit_arg(.push_const, variant_idx)
								for arg in call.arguments {
									c.compile_expr(arg)!
								}
								c.emit_arg(.make_enum_payload, call.arguments.len)
							} else {
								return error('Variant "${variant_name}" does not take a payload')
							}
						} else if expr.right is typed_ast.Identifier {
							variant_id := expr.right as typed_ast.Identifier
							variant_name := variant_id.name

							if variant_name !in enum_type.variants {
								return error('Unknown variant "${variant_name}" in enum ${enum_name}')
							}

							payload_types := enum_type.variants[variant_name] or {
								[]type_def.Type{}
							}
							if payload_types.len > 0 {
								type_strs := payload_types.map(type_to_string)
								return error('Variant "${variant_name}" requires payload(s) of type (${type_strs.join(', ')})')
							}

							c.emit_arg(.push_const, c.add_constant(enum_type.id))
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

				return error("Cannot call '${call.identifier.name}' as a method. AL does not have methods - use '${call.identifier.name}(...)' as a regular function call instead.")
			} else if expr.right is typed_ast.NumberLiteral {
				num := expr.right as typed_ast.NumberLiteral
				index := num.value.int()
				c.emit_arg(.tuple_index, index)
			} else if expr.right is typed_ast.Identifier {
				id := expr.right as typed_ast.Identifier

				idx := c.add_constant(id.name)
				c.emit_arg(.get_field, idx)
			}
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
			// push type_id first, then type_name
			c.emit_arg(.push_const, c.add_constant(struct_type.id))
			type_idx := c.add_constant(expr.identifier.name)
			c.emit_arg(.push_const, type_idx)
			c.emit_arg(.make_struct, expr.fields.len)
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
		else {
			return error("Internal error: unhandled expression type '${expr.type_name()}'. This is a compiler bug.")
		}
	}
}

fn (mut c Compiler) compile_function_expression(func typed_ast.FunctionExpression) ! {
	c.compile_function_common(none, func.params, func.body)!
}

fn (mut c Compiler) compile_function_common(name ?string, params []typed_ast.FunctionParameter, body typed_ast.Expression) ! {
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

	for param in params {
		c.get_or_create_local(param.identifier.name)
	}

	func_start := c.current_addr()

	old_binding := c.current_binding
	if n := name {
		c.current_binding = n
	}

	old_tail := c.in_tail_position
	c.in_tail_position = true
	c.compile_expr(body)!
	c.in_tail_position = old_tail
	c.current_binding = old_binding
	c.emit(.ret)

	c.program.code[jump_over] = op_arg(.jump, c.current_addr())

	captured_names := c.capture_names.clone()
	capture_count := captured_names.len

	func_idx := c.program.functions.len
	c.program.functions << Function{
		name:          name or { '__anon__' }
		arity:         params.len
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
}

fn (mut c Compiler) compile_match(m typed_ast.MatchExpression, is_tail bool) ! {
	c.compile_expr(m.subject)!

	mut end_jumps := []int{}

	for arm in m.arms {
		c.emit(.dup)

		mut binding_names := []string{}
		mut literal_pattern := ?typed_ast.Expression(none)
		mut enum_name := ?string(none)
		mut enum_type_id := ?int(none)
		mut variant_name := ?string(none)

		if arm.pattern is typed_ast.PropertyAccessExpression {
			prop := arm.pattern as typed_ast.PropertyAccessExpression
			if prop.left is typed_ast.Identifier {
				left_id := prop.left as typed_ast.Identifier
				if enum_type := c.type_env.lookup_type(left_id.name) {
					if enum_type is TypeEnum {
						enum_name = left_id.name
						enum_type_id = enum_type.id
						if prop.right is typed_ast.FunctionCallExpression {
							call := prop.right as typed_ast.FunctionCallExpression
							variant_name = call.identifier.name
							for arg in call.arguments {
								if arg is typed_ast.Identifier {
									binding_id := arg as typed_ast.Identifier
									binding_names << binding_id.name
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
			if enum_type := c.type_env.lookup_enum_by_variant(call.identifier.name) {
				enum_name = enum_type.name
				enum_type_id = enum_type.id
				variant_name = call.identifier.name
				for arg in call.arguments {
					if arg is typed_ast.Identifier {
						binding_id := arg as typed_ast.Identifier
						binding_names << binding_id.name
					} else if arg is typed_ast.StringLiteral || arg is typed_ast.NumberLiteral
						|| arg is typed_ast.BooleanLiteral {
						literal_pattern = arg
					}
				}
			}
		}

		if arm.pattern is typed_ast.Identifier {
			ident := arm.pattern as typed_ast.Identifier
			if enum_type := c.type_env.lookup_enum_by_variant(ident.name) {
				enum_name = enum_type.name
				enum_type_id = enum_type.id
				variant_name = ident.name
			}
		}

		if ename := enum_name {
			if vname := variant_name {
				type_id := enum_type_id or {
					return error('Internal error: enum_type_id not set for ${ename}.${vname}')
				}

				c.emit_arg(.push_const, c.add_constant(type_id))
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

				if binding_names.len > 0 {
					c.emit(.dup)
					c.emit(.unwrap_enum)
					// unwrap_enum pushes all payloads in order, so pop in reverse
					for i := binding_names.len - 1; i >= 0; i-- {
						local_idx := c.get_or_create_local(binding_names[i])
						c.emit_arg(.store_local, local_idx)
					}
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

		// Handle array patterns like [], [x], [first, ..rest]
		if arm.pattern is typed_ast.ArrayExpression {
			arr := arm.pattern as typed_ast.ArrayExpression

			// Check if last element is a spread (e.g., [a, b, ..rest])
			has_spread := arr.elements.len > 0 && arr.elements.last() is typed_ast.SpreadExpression
			pre_count := if has_spread { arr.elements.len - 1 } else { arr.elements.len }

			// Check length constraint
			c.emit(.dup)
			c.emit(.array_len)
			c.emit_arg(.push_const, c.add_constant(pre_count))
			if has_spread {
				c.emit(.gte) // length >= pre_count
			} else {
				c.emit(.eq) // length == exactly this many
			}

			next_arm := c.current_addr()
			c.emit_arg(.jump_if_false, 0)

			mut literal_checks := []int{}
			for i in 0 .. pre_count {
				elem := arr.elements[i]
				if elem is typed_ast.NumberLiteral || elem is typed_ast.StringLiteral
					|| elem is typed_ast.BooleanLiteral {
					c.emit(.dup)
					c.emit_arg(.push_const, c.add_constant(i))
					c.emit(.index)
					c.compile_expr(elem)!
					c.emit(.eq)
					literal_checks << c.current_addr()
					c.emit_arg(.jump_if_false, 0)
				}
			}

			for i in 0 .. pre_count {
				elem := arr.elements[i]
				if elem is typed_ast.Identifier {
					c.emit(.dup)
					c.emit_arg(.push_const, c.add_constant(i))
					c.emit(.index)
					local_idx := c.get_or_create_local(elem.name)
					c.emit_arg(.store_local, local_idx)
				}
			}

			if has_spread {
				spread_elem := arr.elements.last()
				if spread_elem is typed_ast.SpreadExpression {
					if inner := spread_elem.expression {
						if inner is typed_ast.Identifier {
							// Slice from pre_count to end
							c.emit(.dup)
							c.emit(.array_len)
							c.emit_arg(.push_const, c.add_constant(pre_count))
							c.emit(.swap)
							c.emit(.array_slice)
							local_idx := c.get_or_create_local(inner.name)
							c.emit_arg(.store_local, local_idx)
						}
					}
					// else: anonymous spread, nothing to bind
				}
			}

			c.emit(.pop)
			c.in_tail_position = is_tail
			c.compile_expr(arm.body)!
			c.in_tail_position = false

			end_jumps << c.current_addr()
			c.emit_arg(.jump, 0)

			next_arm_addr := c.current_addr()
			c.program.code[next_arm] = op_arg(.jump_if_false, next_arm_addr)
			for check_addr in literal_checks {
				c.program.code[check_addr] = op_arg(.jump_if_false, next_arm_addr)
			}
			continue
		}

		if arm.pattern is typed_ast.TupleExpression {
			tup := arm.pattern as typed_ast.TupleExpression

			c.emit(.dup)
			c.emit(.array_len)
			c.emit_arg(.push_const, c.add_constant(tup.elements.len))
			c.emit(.eq)

			next_arm := c.current_addr()
			c.emit_arg(.jump_if_false, 0)

			mut literal_checks := []int{}
			for i, elem in tup.elements {
				if elem is typed_ast.NumberLiteral || elem is typed_ast.StringLiteral
					|| elem is typed_ast.BooleanLiteral {
					c.emit(.dup)
					c.emit_arg(.tuple_index, i)
					c.compile_expr(elem)!
					c.emit(.eq)
					literal_checks << c.current_addr()
					c.emit_arg(.jump_if_false, 0)
				}
			}

			for i, elem in tup.elements {
				if elem is typed_ast.Identifier {
					c.emit(.dup)
					c.emit_arg(.tuple_index, i)
					local_idx := c.get_or_create_local(elem.name)
					c.emit_arg(.store_local, local_idx)
				}
			}

			c.emit(.pop)
			c.in_tail_position = is_tail
			c.compile_expr(arm.body)!
			c.in_tail_position = false

			end_jumps << c.current_addr()
			c.emit_arg(.jump, 0)

			next_arm_addr := c.current_addr()
			c.program.code[next_arm] = op_arg(.jump_if_false, next_arm_addr)
			for check_addr in literal_checks {
				c.program.code[check_addr] = op_arg(.jump_if_false, next_arm_addr)
			}
			continue
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

		if arm.pattern is typed_ast.OrPattern {
			// For or-patterns, check each pattern in sequence
			// If any matches, jump to body; otherwise fall through to next arm
			mut body_jumps := []int{}
			mut fail_jumps := []int{}

			for i, pattern in arm.pattern.patterns {
				if i > 0 {
					// Need to dup the subject again for subsequent patterns
					c.emit(.dup)
				}

				if pattern is typed_ast.RangeExpression {
					range_expr := pattern as typed_ast.RangeExpression

					// subject >= start
					c.emit(.dup)
					c.compile_expr(range_expr.start)!
					c.emit(.gte)

					first_check := c.current_addr()
					c.emit_arg(.jump_if_false, 0)

					// subject < end
					c.emit(.dup)
					c.compile_expr(range_expr.end)!
					c.emit(.lt)

					if i < arm.pattern.patterns.len - 1 {
						body_jumps << c.current_addr()
						c.emit_arg(.jump_if_true, 0)
						c.program.code[first_check] = op_arg(.jump_if_false, c.current_addr())
					} else {
						fail_jumps << c.current_addr()
						c.emit_arg(.jump_if_false, 0)
						fail_jumps << first_check
					}
				} else {
					c.compile_expr(pattern)!
					c.emit(.eq)

					if i < arm.pattern.patterns.len - 1 {
						body_jumps << c.current_addr()
						c.emit_arg(.jump_if_true, 0)
					} else {
						fail_jumps << c.current_addr()
						c.emit_arg(.jump_if_false, 0)
					}
				}
			}

			body_addr := c.current_addr()
			for jump_addr in body_jumps {
				c.program.code[jump_addr] = op_arg(.jump_if_true, body_addr)
			}

			c.emit(.pop)
			c.in_tail_position = is_tail
			c.compile_expr(arm.body)!
			c.in_tail_position = false

			end_jumps << c.current_addr()
			c.emit_arg(.jump, 0)

			next_arm_addr := c.current_addr()
			for jump_addr in fail_jumps {
				c.program.code[jump_addr] = op_arg(.jump_if_false, next_arm_addr)
			}
			continue
		}

		// Range patterns: 0..10 matches [0, 9]
		if arm.pattern is typed_ast.RangeExpression {
			range_expr := arm.pattern as typed_ast.RangeExpression

			// subject >= start
			c.emit(.dup)
			c.compile_expr(range_expr.start)!
			c.emit(.gte)

			first_check := c.current_addr()
			c.emit_arg(.jump_if_false, 0)

			// subject < end
			c.emit(.dup)
			c.compile_expr(range_expr.end)!
			c.emit(.lt)

			second_check := c.current_addr()
			c.emit_arg(.jump_if_false, 0)

			c.emit(.pop)
			c.in_tail_position = is_tail
			c.compile_expr(arm.body)!
			c.in_tail_position = false

			end_jumps << c.current_addr()
			c.emit_arg(.jump, 0)
			next_arm_addr := c.current_addr()
			c.program.code[first_check] = op_arg(.jump_if_false, next_arm_addr)
			c.program.code[second_check] = op_arg(.jump_if_false, next_arm_addr)
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
			if !c.flags.expose_debug_builtins {
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
		'str_split' {
			if call.arguments.len != 2 {
				return error('str_split expects 2 arguments (string, delimiter)')
			}
			c.compile_expr(call.arguments[0])!
			c.compile_expr(call.arguments[1])!
			c.emit(.str_split)
		}
		else {
			return error('Unknown function: ${call.identifier.name}')
		}
	}
}
