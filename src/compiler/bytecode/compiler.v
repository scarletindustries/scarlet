module bytecode

import compiler.ast

struct StructDef {
	name   string
	fields map[string]string // field name -> type name
}

struct EnumDef {
	name     string
	variants map[string]string // variant name -> payload type (empty string if no payload)
}

// Find which enum a variant belongs to (returns enum name or none)
fn (c Compiler) find_enum_for_variant(variant_name string) ?string {
	for enum_name, enum_def in c.enums {
		if variant_name in enum_def.variants {
			return enum_name
		}
	}
	return none
}

struct FuncSig {
	name        string
	param_types []string // type name for each parameter (enum name, struct name, or empty)
}

struct Scope {
	locals map[string]int
}

struct Compiler {
mut:
	program          Program
	locals           map[string]int
	outer_scopes     []Scope // for closures: stack of enclosing scopes
	local_count      int
	current_func_idx int
	structs          map[string]StructDef
	enums            map[string]EnumDef
	functions        map[string]FuncSig // function signatures for type inference
	captures         map[string]int     // captured var name -> capture index
	capture_names    []string           // ordered list of captured var names
}

pub fn compile(expr ast.Expression) !Program {
	mut c := Compiler{
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
		structs:          {}
		enums:            {}
		functions:        {}
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

// Represents how a variable should be accessed
struct VarAccess {
	is_local   bool
	is_capture bool
	index      int
}

// Resolve a variable: check current scope locals, then captures, then outer scopes
fn (mut c Compiler) resolve_variable(name string) ?VarAccess {
	// Check current scope's locals first
	if idx := c.locals[name] {
		return VarAccess{
			is_local: true
			index:    idx
		}
	}

	// Check if already captured
	if idx := c.captures[name] {
		return VarAccess{
			is_capture: true
			index:      idx
		}
	}

	// Search outer scopes (for closures)
	for scope in c.outer_scopes {
		if name in scope.locals {
			// Found in outer scope - add to captures
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

// Compile expression with an optional type hint for inference
fn (mut c Compiler) compile_expr_with_hint(expr ast.Expression, expected_type string) ! {
	// If we have an expected enum type and this looks like a bare variant, compile it as such
	if expected_type != '' && expected_type in c.enums {
		enum_def := c.enums[expected_type]

		// Check for Variant(payload) form
		if expr is ast.FunctionCallExpression {
			call := expr as ast.FunctionCallExpression
			if call.identifier.name in enum_def.variants {
				// This is a bare enum variant with payload
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

		// Check for bare Variant form (no payload)
		if expr is ast.Identifier {
			ident := expr as ast.Identifier
			if ident.name in enum_def.variants {
				// This is a bare enum variant without payload
				enum_idx := c.add_constant(expected_type)
				c.emit_arg(.push_const, enum_idx)
				variant_idx := c.add_constant(ident.name)
				c.emit_arg(.push_const, variant_idx)
				c.emit(.make_enum)
				return
			}
		}
	}

	// No inference needed, compile normally
	c.compile_expr(expr)!
}

fn (mut c Compiler) compile_expr(expr ast.Expression) ! {
	match expr {
		ast.BlockExpression {
			for i, e in expr.body {
				c.compile_expr(e)!
				if i < expr.body.len - 1 {
					c.emit(.pop)
				}
			}

			if expr.body.len == 0 {
				c.emit(.push_none)
			}
		}
		ast.NumberLiteral {
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
		ast.StringLiteral {
			idx := c.add_constant(expr.value)
			c.emit_arg(.push_const, idx)
		}
		ast.InterpolatedString {
			// Compile each part and concatenate with str_concat
			if expr.parts.len == 0 {
				idx := c.add_constant('')
				c.emit_arg(.push_const, idx)
			} else {
				// First part
				c.compile_expr(expr.parts[0])!
				c.emit(.to_string) // convert to string if needed

				// Remaining parts: compile and concatenate
				for i := 1; i < expr.parts.len; i++ {
					c.compile_expr(expr.parts[i])!
					c.emit(.to_string)
					c.emit(.str_concat)
				}
			}
		}
		ast.BooleanLiteral {
			if expr.value {
				c.emit(.push_true)
			} else {
				c.emit(.push_false)
			}
		}
		ast.NoneExpression {
			c.emit(.push_none)
		}
		ast.Identifier {
			if access := c.resolve_variable(expr.name) {
				if access.is_local {
					c.emit_arg(.push_local, access.index)
				} else if access.is_capture {
					c.emit_arg(.push_capture, access.index)
				}
			} else {
				return error('Undefined variable: ${expr.name}')
			}
		}
		ast.VariableBinding {
			c.compile_expr(expr.init)!
			idx := c.get_or_create_local(expr.identifier.name)
			c.emit_arg(.store_local, idx)

			c.emit(.push_none)
		}
		ast.ConstBinding {
			c.compile_expr(expr.init)!
			idx := c.get_or_create_local(expr.identifier.name)
			c.emit_arg(.store_local, idx)
			c.emit(.push_none)
		}
		ast.BinaryExpression {
			// Short-circuit evaluation for && and ||
			if expr.op.kind == .logical_and {
				c.compile_expr(expr.left)!
				c.emit(.dup) // keep copy for short-circuit result
				// If left is false, skip right and leave false on stack
				end_jump := c.current_addr()
				c.emit_arg(.jump_if_false, 0)
				c.emit(.pop) // left was true, discard it
				c.compile_expr(expr.right)!
				c.program.code[end_jump] = op_arg(.jump_if_false, c.current_addr())
				return
			}
			if expr.op.kind == .logical_or {
				c.compile_expr(expr.left)!
				c.emit(.dup) // keep copy for short-circuit result
				// If left is true, skip right and leave true on stack
				end_jump := c.current_addr()
				c.emit_arg(.jump_if_true, 0)
				c.emit(.pop) // left was false, discard it
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
		ast.UnaryExpression {
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
		ast.IfExpression {
			c.compile_expr(expr.condition)!

			else_jump := c.current_addr()
			c.emit_arg(.jump_if_false, 0)

			c.compile_expr(expr.body)!

			end_jump := c.current_addr()
			c.emit_arg(.jump, 0)

			else_addr := c.current_addr()
			c.program.code[else_jump] = op_arg(.jump_if_false, else_addr)

			if else_body := expr.else_body {
				c.compile_expr(else_body)!
			} else {
				c.emit(.push_none)
			}

			end_addr := c.current_addr()
			c.program.code[end_jump] = op_arg(.jump, end_addr)
		}
		ast.MatchExpression {
			c.compile_match(expr)!
		}
		ast.ArrayExpression {
			for elem in expr.elements {
				c.compile_expr(elem)!
			}
			c.emit_arg(.make_array, expr.elements.len)
		}
		ast.ArrayIndexExpression {
			c.compile_expr(expr.expression)!
			c.compile_expr(expr.index)!
			c.emit(.index)
		}
		ast.RangeExpression {
			c.compile_expr(expr.start)!
			c.compile_expr(expr.end)!
			c.emit(.make_range)
		}
		ast.FunctionExpression {
			c.compile_function(expr)!
		}
		ast.FunctionCallExpression {
			// Get expected parameter types if we have a signature for this function
			func_sig := c.functions[expr.identifier.name] or { FuncSig{} }

			for i, arg in expr.arguments {
				// Check if this argument should be inferred as an enum variant
				expected_type := if i < func_sig.param_types.len {
					func_sig.param_types[i]
				} else {
					''
				}
				c.compile_expr_with_hint(arg, expected_type)!
			}

			if idx := c.locals[expr.identifier.name] {
				c.emit_arg(.push_local, idx)
				c.emit_arg(.call, expr.arguments.len)
			} else {
				c.compile_builtin_call(expr)!
			}
		}
		ast.PropertyAccessExpression {
			// Check if this is an enum variant access (MyEnum.Variant or MyEnum.Variant(payload))
			if expr.left is ast.Identifier {
				left_id := expr.left as ast.Identifier
				if enum_def := c.enums[left_id.name] {
					// This is an enum variant construction
					enum_name := left_id.name

					if expr.right is ast.FunctionCallExpression {
						// MyEnum.Variant(payload)
						call := expr.right as ast.FunctionCallExpression
						variant_name := call.identifier.name

						if variant_name !in enum_def.variants {
							return error('Unknown variant "${variant_name}" in enum ${enum_name}')
						}

						expected_payload := enum_def.variants[variant_name]
						if expected_payload == '' {
							return error('Variant "${variant_name}" does not take a payload')
						}

						if call.arguments.len != 1 {
							return error('Variant "${variant_name}" expects exactly 1 payload argument')
						}

						// Push enum_name, variant_name, then payload
						enum_idx := c.add_constant(enum_name)
						c.emit_arg(.push_const, enum_idx)
						variant_idx := c.add_constant(variant_name)
						c.emit_arg(.push_const, variant_idx)
						c.compile_expr(call.arguments[0])!
						c.emit(.make_enum_payload)
					} else if expr.right is ast.Identifier {
						// MyEnum.Variant (no payload)
						variant_id := expr.right as ast.Identifier
						variant_name := variant_id.name

						if variant_name !in enum_def.variants {
							return error('Unknown variant "${variant_name}" in enum ${enum_name}')
						}

						expected_payload := enum_def.variants[variant_name]
						if expected_payload != '' {
							return error('Variant "${variant_name}" requires a payload of type ${expected_payload}')
						}

						// Push enum_name, variant_name
						enum_idx := c.add_constant(enum_name)
						c.emit_arg(.push_const, enum_idx)
						variant_idx := c.add_constant(variant_name)
						c.emit_arg(.push_const, variant_idx)
						c.emit(.make_enum)
					}
					return
				}
			}

			// Regular property access (struct field)
			c.compile_expr(expr.left)!

			if expr.right is ast.FunctionCallExpression {
				call := expr.right as ast.FunctionCallExpression

				for arg in call.arguments {
					c.compile_expr(arg)!
				}

				return error('Method calls not yet implemented')
			} else if expr.right is ast.Identifier {
				id := expr.right as ast.Identifier

				idx := c.add_constant(id.name)
				c.emit_arg(.get_field, idx)
			}
		}
		ast.StructExpression {
			// Register struct definition
			struct_name := expr.identifier.name
			if struct_name in c.structs {
				return error('Struct already defined: ${struct_name}')
			}
			mut fields := map[string]string{}
			for field in expr.fields {
				fields[field.identifier.name] = field.typ.identifier.name
			}
			c.structs[struct_name] = StructDef{
				name:   struct_name
				fields: fields
			}
			// Struct declarations don't produce a value at runtime
			c.emit(.push_none)
		}
		ast.EnumExpression {
			// Register enum definition
			enum_name := expr.identifier.name
			if enum_name in c.enums {
				return error('Enum already defined: ${enum_name}')
			}
			mut variants := map[string]string{}
			for variant in expr.variants {
				payload_type := if p := variant.payload {
					p.identifier.name
				} else {
					''
				}
				variants[variant.identifier.name] = payload_type
			}
			c.enums[enum_name] = EnumDef{
				name:     enum_name
				variants: variants
			}
			// Enum declarations don't produce a value at runtime
			c.emit(.push_none)
		}
		ast.StructInitExpression {
			struct_name := expr.identifier.name

			struct_def := c.structs[struct_name] or {
				return error('Unknown struct type: ${struct_name}')
			}

			mut provided := map[string]bool{}
			for field in expr.fields {
				field_name := field.identifier.name
				if field_name !in struct_def.fields {
					return error('Unknown field "${field_name}" in struct ${struct_name}')
				}
				if field_name in provided {
					return error('Duplicate field "${field_name}" in struct ${struct_name}')
				}
				provided[field_name] = true
			}

			for field_name, _ in struct_def.fields {
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
		ast.AssertExpression {
			c.compile_expr(expr.expression)!

			// If true, jump over error path
			ok_jump := c.current_addr()
			c.emit_arg(.jump_if_true, 0)

			// Condition was false - create and return AssertionError
			c.compile_expr(expr.message)!
			c.emit(.make_error)
			c.emit(.ret)

			// Condition was true - continue
			c.program.code[ok_jump] = op_arg(.jump_if_true, c.current_addr())
			c.emit(.push_none)
		}
		ast.ExportExpression {
			// TODO: exports not implemented yet, just compile the inner expression
			c.compile_expr(expr.expression)!
		}
		ast.ImportDeclaration {
			// TODO: imports not implemented yet
			c.emit(.push_none)
		}
		ast.ErrorExpression {
			// error 'message' -> creates an ErrorValue
			c.compile_expr(expr.expression)!
			c.emit(.make_error)
		}
		ast.OrExpression {
			// expr or { default } or expr or |e| { handle(e) }
			// Handles both error values and none values (for optional types)
			c.compile_expr(expr.expression)!

			// Duplicate to check if error or none
			c.emit(.dup)
			c.emit(.is_error_or_none)

			// If not error/none, jump over the or-body
			not_error_jump := c.current_addr()
			c.emit_arg(.jump_if_false, 0)

			// Is error/none - pop the value (or bind it if error) and execute body
			if receiver := expr.receiver {
				// Check if it's actually an error (not none) before unwrapping
				c.emit(.dup)
				c.emit(.is_error)
				not_an_error := c.current_addr()
				c.emit_arg(.jump_if_false, 0)

				// It's an error - unwrap and bind
				c.emit(.unwrap_error)
				idx := c.get_or_create_local(receiver.name)
				c.emit_arg(.store_local, idx)
				after_bind := c.current_addr()
				c.emit_arg(.jump, 0)

				// It's none - just pop it and push none for the receiver
				c.program.code[not_an_error] = op_arg(.jump_if_false, c.current_addr())
				c.emit(.pop)
				idx2 := c.get_or_create_local(receiver.name)
				c.emit(.push_none)
				c.emit_arg(.store_local, idx2)

				c.program.code[after_bind] = op_arg(.jump, c.current_addr())
			} else {
				c.emit(.pop) // discard error/none
			}

			c.compile_expr(expr.body)!

			// Jump to end
			end_jump := c.current_addr()
			c.emit_arg(.jump, 0)

			// Patch not_error_jump to here
			c.program.code[not_error_jump] = op_arg(.jump_if_false, c.current_addr())

			// Not error/none - value is already on stack, nothing to do

			// Patch end_jump
			c.program.code[end_jump] = op_arg(.jump, c.current_addr())
		}
		ast.PropagateExpression {
			// expr! -> if error, return it; else unwrap
			c.compile_expr(expr.expression)!

			// Check if error
			c.emit(.dup)
			c.emit(.is_error)

			// If not error, jump to unwrap
			not_error_jump := c.current_addr()
			c.emit_arg(.jump_if_false, 0)

			// Is error - return it
			c.emit(.ret)

			// Patch jump
			c.program.code[not_error_jump] = op_arg(.jump_if_false, c.current_addr())

			// Not error - value is on stack (it's already the unwrapped value)
		}
		else {
			return error('Cannot compile expression type: ${expr.type_name()}')
		}
	}
}

fn (mut c Compiler) compile_function(func ast.FunctionExpression) ! {
	// Save current state and push to outer scopes
	old_locals := c.locals.clone()
	old_local_count := c.local_count
	old_captures := c.captures.clone()
	old_capture_names := c.capture_names.clone()

	// Push current locals to outer scopes for closure capture
	c.outer_scopes << Scope{
		locals: old_locals.clone()
	}

	// Jump over function body
	jump_over := c.current_addr()
	c.emit_arg(.jump, 0)

	// Reset for new function
	c.locals = {}
	c.local_count = 0
	c.captures = {}
	c.capture_names = []

	// Collect parameter types for function signature
	mut param_types := []string{}
	for param in func.params {
		c.get_or_create_local(param.identifier.name)
		if typ := param.typ {
			param_types << typ.identifier.name
		} else {
			param_types << ''
		}
	}

	func_start := c.current_addr()

	// Compile function body (this may populate c.captures)
	c.compile_expr(func.body)!
	c.emit(.ret)

	c.program.code[jump_over] = op_arg(.jump, c.current_addr())

	// Save capture info before restoring state
	captured_names := c.capture_names.clone()
	capture_count := captured_names.len

	func_idx := c.program.functions.len
	mut name := '__anon__'
	if id := func.identifier {
		name = id.name
		// Store function signature for type inference
		c.functions[name] = FuncSig{
			name:        name
			param_types: param_types
		}
	}
	c.program.functions << Function{
		name:          name
		arity:         func.params.len
		locals:        c.local_count
		capture_count: capture_count
		code_start:    func_start
		code_len:      c.current_addr() - func_start - 1
	}

	c.outer_scopes.pop() // remove the scope we pushed

	// Restore previous state
	c.locals = old_locals.clone()
	c.local_count = old_local_count
	c.captures = old_captures.clone()
	c.capture_names = old_capture_names.clone()

	// Now emit code to push captured values (from the outer scope's perspective)
	// and create the closure
	for cap_name in captured_names {
		// The captured variable should be accessible from the restored (outer) scope
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

fn (mut c Compiler) compile_match(m ast.MatchExpression) ! {
	c.compile_expr(m.subject)!

	mut end_jumps := []int{}

	for arm in m.arms {
		c.emit(.dup)

		// Check if this is an enum destructuring pattern
		mut binding_name := ?string(none)
		mut enum_name := ?string(none)
		mut variant_name := ?string(none)

		// Full form: EnumName.Variant(binding)
		if arm.pattern is ast.PropertyAccessExpression {
			prop := arm.pattern as ast.PropertyAccessExpression
			if prop.left is ast.Identifier {
				left_id := prop.left as ast.Identifier
				if left_id.name in c.enums {
					enum_name = left_id.name
					if prop.right is ast.FunctionCallExpression {
						call := prop.right as ast.FunctionCallExpression
						variant_name = call.identifier.name
						if call.arguments.len == 1 {
							if call.arguments[0] is ast.Identifier {
								binding_id := call.arguments[0] as ast.Identifier
								binding_name = binding_id.name
							}
						}
					} else if prop.right is ast.Identifier {
						// EnumName.Variant (no payload)
						right_id := prop.right as ast.Identifier
						variant_name = right_id.name
					}
				}
			}
		}

		// Shorthand form: Variant(binding) - infer enum from variant name
		if arm.pattern is ast.FunctionCallExpression {
			call := arm.pattern as ast.FunctionCallExpression
			if inferred_enum := c.find_enum_for_variant(call.identifier.name) {
				enum_name = inferred_enum
				variant_name = call.identifier.name
				if call.arguments.len == 1 {
					if call.arguments[0] is ast.Identifier {
						binding_id := call.arguments[0] as ast.Identifier
						binding_name = binding_id.name
					}
				}
			}
		}

		// Shorthand form: Variant (no payload) - infer enum from variant name
		if arm.pattern is ast.Identifier {
			ident := arm.pattern as ast.Identifier
			if inferred_enum := c.find_enum_for_variant(ident.name) {
				enum_name = inferred_enum
				variant_name = ident.name
			}
		}

		if ename := enum_name {
			if vname := variant_name {
				// Enum pattern (with or without destructuring)
				// Use match_enum to compare variant only (ignores payload)
				enum_idx := c.add_constant(ename)
				c.emit_arg(.push_const, enum_idx)
				variant_idx := c.add_constant(vname)
				c.emit_arg(.push_const, variant_idx)
				c.emit(.match_enum)

				next_arm := c.current_addr()
				c.emit_arg(.jump_if_false, 0)

				// If there's a binding, extract the payload
				if bname := binding_name {
					c.emit(.dup) // dup subject for unwrap
					c.emit(.unwrap_enum) // get payload
					local_idx := c.get_or_create_local(bname)
					c.emit_arg(.store_local, local_idx)
				}

				c.emit(.pop) // pop the subject
				c.compile_expr(arm.body)!

				end_jumps << c.current_addr()
				c.emit_arg(.jump, 0)

				c.program.code[next_arm] = op_arg(.jump_if_false, c.current_addr())
				continue
			}
		}
		// Check for wildcard pattern (else =>)
		if arm.pattern is ast.WildcardPattern {
			// Wildcard always matches - no comparison needed
			c.emit(.pop) // pop the dup'd subject
			c.emit(.pop) // pop the original subject
			c.compile_expr(arm.body)!

			end_jumps << c.current_addr()
			c.emit_arg(.jump, 0)
			continue
		}

		// Regular pattern (literal or simple enum variant)
		c.compile_expr(arm.pattern)!
		c.emit(.eq)

		next_arm := c.current_addr()
		c.emit_arg(.jump_if_false, 0)

		c.emit(.pop)
		c.compile_expr(arm.body)!

		end_jumps << c.current_addr()
		c.emit_arg(.jump, 0)

		c.program.code[next_arm] = op_arg(.jump_if_false, c.current_addr())
	}

	// No match - push none (only reached if no wildcard)
	c.emit(.pop)
	c.emit(.push_none)

	end_addr := c.current_addr()
	for jump_addr in end_jumps {
		c.program.code[jump_addr] = op_arg(.jump, end_addr)
	}
}

fn (mut c Compiler) compile_builtin_call(call ast.FunctionCallExpression) ! {
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
		else {
			return error('Unknown function: ${call.identifier.name}')
		}
	}
}
