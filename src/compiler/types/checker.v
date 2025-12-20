module types

import compiler.ast
import compiler.diagnostic
import compiler.type_def {
	Type,
	TypeArray,
	TypeEnum,
	TypeFunction,
	TypeOption,
	TypeResult,
	TypeStruct,
	TypeVar,
	is_numeric,
	substitute,
	t_array,
	t_bool,
	t_float,
	t_int,
	t_none,
	t_option,
	t_string,
	t_var,
	type_to_string,
	types_equal,
}

pub struct TypeChecker {
mut:
	env         TypeEnv
	diagnostics []diagnostic.Diagnostic
	in_function bool
}

pub struct CheckResult {
pub:
	diagnostics []diagnostic.Diagnostic
	success     bool
	env         TypeEnv
}

pub fn check(mut program ast.BlockExpression) CheckResult {
	mut checker := TypeChecker{
		env:         new_env()
		diagnostics: []diagnostic.Diagnostic{}
	}

	checker.register_builtins()

	checker.check_block(mut program)

	return CheckResult{
		diagnostics: checker.diagnostics
		success:     checker.diagnostics.len == 0
		env:         checker.env
	}
}

fn (mut c TypeChecker) error_at_span(message string, span ast.Span) {
	c.diagnostics << diagnostic.error_at(span.line, span.column, message)
}

fn get_expr_span(expr ast.Expression) ast.Span {
	return match expr {
		ast.NumberLiteral { expr.span }
		ast.StringLiteral { expr.span }
		ast.BooleanLiteral { expr.span }
		ast.Identifier { expr.span }
		ast.VariableBinding { expr.span }
		ast.ConstBinding { expr.span }
		ast.BinaryExpression { expr.span }
		ast.FunctionCallExpression { expr.span }
		ast.ArrayExpression { expr.span }
		ast.ArrayIndexExpression { expr.span }
		ast.IfExpression { expr.span }
		else { ast.Span{} }
	}
}

fn (mut c TypeChecker) register_builtins() {
	a := t_var('a')

	socket := TypeStruct{
		name:   'Socket'
		fields: map[string]Type{}
	}
	c.env.register_struct(socket)

	c.env.register_function('println', TypeFunction{
		params: [a]
		ret:    t_none()
	})

	c.env.register_function('inspect', TypeFunction{
		params: [a]
		ret:    t_string()
	})

	c.env.register_function('read_file', TypeFunction{
		params: [t_string()]
		ret:    t_string()
	})

	c.env.register_function('write_file', TypeFunction{
		params: [t_string(), t_string()]
		ret:    t_none()
	})

	c.env.register_function('tcp_listen', TypeFunction{
		params: [t_int()]
		ret:    socket
	})

	c.env.register_function('tcp_accept', TypeFunction{
		params: [socket]
		ret:    socket
	})

	c.env.register_function('tcp_read', TypeFunction{
		params: [socket]
		ret:    t_string()
	})

	c.env.register_function('tcp_write', TypeFunction{
		params: [socket, t_string()]
		ret:    t_none()
	})

	c.env.register_function('tcp_close', TypeFunction{
		params: [socket]
		ret:    t_none()
	})
}

fn (mut c TypeChecker) expect_type(actual Type, expected Type, span ast.Span, context string) bool {
	if types_equal(actual, expected) {
		return true
	}
	// Allow T to match T!E (error path is implicit via assert)
	if expected is TypeResult {
		if types_equal(actual, expected.success) {
			return true
		}
	}
	// Allow T to match ?T
	if expected is TypeOption {
		if types_equal(actual, expected.inner) {
			return true
		}
	}
	c.error_at_span('expected ${type_to_string(expected)}, got ${type_to_string(actual)} ${context}',
		span)
	return false
}

fn (c TypeChecker) resolve_type_identifier(t ast.TypeIdentifier) ?Type {
	if t.is_function {
		mut param_types := []Type{}
		for param_type in t.param_types {
			resolved := c.resolve_type_identifier(param_type) or { return none }
			param_types << resolved
		}

		mut ret_type := t_none()
		if rt := t.return_type {
			ret_type = c.resolve_type_identifier(*rt) or { return none }
		}

		mut err_type := ?Type(none)
		if et := t.error_type {
			err_type = c.resolve_type_identifier(*et) or { return none }
		}

		mut base_type := Type(TypeFunction{
			params:     param_types
			ret:        ret_type
			error_type: err_type
		})

		if t.is_option {
			base_type = t_option(base_type)
		}

		return base_type
	}

	name := t.identifier.name

	is_type_var := name.len > 0 && name[0] >= `a` && name[0] <= `z`

	mut base_type := if is_type_var {
		t_var(name)
	} else {
		c.env.lookup_type(name) or { return none }
	}

	if t.is_array {
		base_type = t_array(base_type)
	}

	if t.is_option {
		base_type = t_option(base_type)
	}

	return base_type
}

fn (mut c TypeChecker) check_block(mut block ast.BlockExpression) Type {
	mut last_type := t_none()

	for mut expr in block.body {
		last_type = c.check_expr(mut expr)
	}

	return last_type
}

fn (mut c TypeChecker) check_expr(mut expr ast.Expression) Type {
	match mut expr {
		ast.NumberLiteral {
			if expr.value.contains('.') {
				return t_float()
			}
			return t_int()
		}
		ast.StringLiteral {
			return t_string()
		}
		ast.InterpolatedString {
			for mut part in expr.parts {
				c.check_expr(mut part)
			}
			return t_string()
		}
		ast.BooleanLiteral {
			return t_bool()
		}
		ast.NoneExpression {
			return t_none()
		}
		ast.Identifier {
			if t := c.env.lookup(expr.name) {
				return t
			}
			c.error_at_span("Unknown identifier '${expr.name}'", expr.span)
			return t_none()
		}
		ast.VariableBinding {
			// support recursive functions by checking if the rhs is a function expression
			if expr.init is ast.FunctionExpression {
				func_expr := expr.init as ast.FunctionExpression

				mut param_types := []Type{}
				for param in func_expr.params {
					if pt := param.typ {
						if resolved := c.resolve_type_identifier(pt) {
							param_types << resolved
						} else {
							param_types << t_none()
						}
					} else {
						param_types << t_none()
					}
				}

				mut ret_type := t_none()
				if rt := func_expr.return_type {
					if resolved := c.resolve_type_identifier(rt) {
						ret_type = resolved
					}
				}

				mut err_type := ?Type(none)
				if et := func_expr.error_type {
					if resolved := c.resolve_type_identifier(et) {
						err_type = resolved
					}
				}

				preliminary_func_type := TypeFunction{
					params:     param_types
					ret:        ret_type
					error_type: err_type
				}

				// and register it so we can call itself
				c.env.define(expr.identifier.name, preliminary_func_type)
			}

			init_type := c.check_expr(mut expr.init)
			if annotation := expr.typ {
				if expected := c.resolve_type_identifier(annotation) {
					init_span := get_expr_span(expr.init)
					c.expect_type(init_type, expected, init_span, 'in variable binding')
					c.env.define(expr.identifier.name, expected)
					return expected
				} else {
					c.error_at_span("Unknown type '${annotation.identifier.name}'", annotation.identifier.span)
				}
			}
			c.env.define(expr.identifier.name, init_type)
			return init_type
		}
		ast.ConstBinding {
			if c.in_function {
				c.error_at_span('const bindings are only allowed at the top level', expr.span)
			}
			init_type := c.check_expr(mut expr.init)
			if annotation := expr.typ {
				if expected := c.resolve_type_identifier(annotation) {
					init_span := get_expr_span(expr.init)
					c.expect_type(init_type, expected, init_span, 'in const binding')
					c.env.define(expr.identifier.name, expected)
					return expected
				} else {
					c.error_at_span("Unknown type '${annotation.identifier.name}'", annotation.identifier.span)
				}
			}
			c.env.define(expr.identifier.name, init_type)
			return init_type
		}
		ast.BinaryExpression {
			return c.check_binary(mut expr)
		}
		ast.UnaryExpression {
			return c.check_unary(mut expr)
		}
		ast.FunctionExpression {
			return c.check_function(mut expr)
		}
		ast.FunctionCallExpression {
			return c.check_call(mut expr)
		}
		ast.BlockExpression {
			c.env.push_scope()
			result := c.check_block(mut expr)
			c.env.pop_scope()
			return result
		}
		ast.IfExpression {
			return c.check_if(mut expr)
		}
		ast.ArrayExpression {
			return c.check_array(mut expr)
		}
		ast.ArrayIndexExpression {
			return c.check_array_index(mut expr)
		}
		ast.StructExpression {
			return c.check_struct_def(expr)
		}
		ast.StructInitExpression {
			return c.check_struct_init(mut expr)
		}
		ast.EnumExpression {
			return c.check_enum_def(expr)
		}
		ast.PropertyAccessExpression {
			return c.check_property_access(mut expr)
		}
		ast.MatchExpression {
			return c.check_match(mut expr)
		}
		ast.OrExpression {
			inner_type := c.check_expr(mut expr.expression)

			// Store the resolved type for the bytecode compiler
			expr.resolved_type = inner_type

			mut success_type := inner_type
			mut error_type := t_none()

			if inner_type is TypeOption {
				// Unwrapping ?T gives T, error is None
				success_type = inner_type.inner
				error_type = t_none()
			} else if inner_type is TypeResult {
				success_type = inner_type.success
				error_type = inner_type.error
			}

			if receiver := expr.receiver {
				c.env.push_scope()
				c.env.define(receiver.name, error_type)
			}

			body_type := c.check_expr(mut expr.body)
			body_span := get_expr_span(expr.body)

			c.expect_type(body_type, success_type, body_span, "in 'or' fallback")

			if expr.receiver != none {
				c.env.pop_scope()
			}

			return success_type
		}
		ast.PostfixExpression {
			inner_type := c.check_expr(mut expr.expression)

			match expr.op.kind {
				.punc_exclamation_mark {
					if inner_type is TypeOption {
						return inner_type.inner
					}
					if inner_type is TypeResult {
						return inner_type.success
					}
					return inner_type
				}
				else {
					return t_none()
				}
			}
		}
		ast.ErrorExpression {
			return c.check_expr(mut expr.expression)
		}
		ast.RangeExpression {
			start_type := c.check_expr(mut expr.start)
			end_type := c.check_expr(mut expr.end)

			if !types_equal(start_type, t_int()) {
				start_span := get_expr_span(expr.start)
				c.error_at_span('Range start must be Int, got ${type_to_string(start_type)}',
					start_span)
			}

			if !types_equal(end_type, t_int()) {
				end_span := get_expr_span(expr.end)
				c.error_at_span('Range end must be Int, got ${type_to_string(end_type)}',
					end_span)
			}

			return t_array(t_int())
		}
		ast.AssertExpression {
			cond_type := c.check_expr(mut expr.expression)
			cond_span := get_expr_span(expr.expression)
			c.expect_type(cond_type, t_bool(), cond_span, 'in assert condition')

			c.check_expr(mut expr.message)

			return t_none()
		}
		ast.PropagateExpression {
			inner_type := c.check_expr(mut expr.expression)

			expr.resolved_type = inner_type

			if inner_type is TypeOption {
				return inner_type.inner
			}

			if inner_type is TypeResult {
				return inner_type.success
			}

			return inner_type
		}
		else {
			return t_none()
		}
	}
}

fn (mut c TypeChecker) check_binary(mut expr ast.BinaryExpression) Type {
	left_type := c.check_expr(mut expr.left)
	right_type := c.check_expr(mut expr.right)

	match expr.op.kind {
		.punc_plus {
			if types_equal(left_type, t_string()) && types_equal(right_type, t_string()) {
				return t_string()
			}
			if !is_numeric(left_type) {
				c.error_at_span('Left operand of ${expr.op.kind} must be numeric or string, got ${type_to_string(left_type)}',
					expr.span)
				return t_int()
			}
			if !is_numeric(right_type) {
				c.error_at_span('Right operand of ${expr.op.kind} must be numeric or string, got ${type_to_string(right_type)}',
					expr.span)
				return t_int()
			}
			if !types_equal(left_type, right_type) {
				c.error_at_span('Operands of ${expr.op.kind} must have same type, got ${type_to_string(left_type)} and ${type_to_string(right_type)}',
					expr.span)
			}
			return left_type
		}
		.punc_minus, .punc_mul, .punc_div, .punc_mod {
			if !is_numeric(left_type) {
				c.error_at_span('Left operand of ${expr.op.kind} must be numeric, got ${type_to_string(left_type)}',
					expr.span)
				return t_int()
			}
			if !is_numeric(right_type) {
				c.error_at_span('Right operand of ${expr.op.kind} must be numeric, got ${type_to_string(right_type)}',
					expr.span)
				return t_int()
			}
			if !types_equal(left_type, right_type) {
				c.error_at_span('Operands of ${expr.op.kind} must have same type, got ${type_to_string(left_type)} and ${type_to_string(right_type)}',
					expr.span)
			}
			return left_type
		}
		.punc_lt, .punc_gt, .punc_lte, .punc_gte {
			if !is_numeric(left_type) || !is_numeric(right_type) {
				c.error_at_span('Comparison operators require numeric operands', expr.span)
			}
			return t_bool()
		}
		.punc_equals_comparator, .punc_not_equal {
			if !types_equal(left_type, right_type) {
				c.error_at_span('Cannot compare ${type_to_string(left_type)} with ${type_to_string(right_type)}',
					expr.span)
			}
			return t_bool()
		}
		.logical_and, .logical_or {
			c.expect_type(left_type, t_bool(), expr.span, 'in logical expression')
			c.expect_type(right_type, t_bool(), expr.span, 'in logical expression')
			return t_bool()
		}
		else {
			return t_none()
		}
	}
}

fn (mut c TypeChecker) check_unary(mut expr ast.UnaryExpression) Type {
	operand_type := c.check_expr(mut expr.expression)
	span := get_expr_span(expr.expression)

	match expr.op.kind {
		.punc_minus {
			if !is_numeric(operand_type) {
				c.error_at_span('Unary minus requires numeric operand, got ${type_to_string(operand_type)}',
					span)
			}
			return operand_type
		}
		.punc_exclamation_mark {
			c.expect_type(operand_type, t_bool(), span, 'in logical not')
			return t_bool()
		}
		else {
			return t_none()
		}
	}
}

fn (mut c TypeChecker) check_function(mut expr ast.FunctionExpression) Type {
	mut param_types := []Type{}

	mut ret_type := t_none()
	if rt := expr.return_type {
		if resolved := c.resolve_type_identifier(rt) {
			ret_type = resolved
		} else {
			c.error_at_span("Unknown return type '${rt.identifier.name}'", rt.identifier.span)
		}
	}

	mut err_type := ?Type(none)
	if et := expr.error_type {
		if resolved := c.resolve_type_identifier(et) {
			err_type = resolved
		} else {
			c.error_at_span("Unknown error type '${et.identifier.name}'", et.identifier.span)
		}
	}

	for param in expr.params {
		if pt := param.typ {
			if resolved := c.resolve_type_identifier(pt) {
				param_types << resolved
			} else {
				c.error_at_span("Unknown type '${pt.identifier.name}'", pt.identifier.span)
				param_types << t_none()
			}
		} else {
			c.error_at_span("Parameter '${param.identifier.name}' requires a type annotation",
				param.identifier.span)
			param_types << t_none()
		}
	}

	func_type := TypeFunction{
		params:     param_types
		ret:        ret_type
		error_type: err_type
	}

	if id := expr.identifier {
		c.env.register_function(id.name, func_type)
		c.env.define(id.name, func_type)
	}

	c.env.push_scope()
	for i, param in expr.params {
		c.env.define(param.identifier.name, param_types[i])
	}

	prev_in_function := c.in_function
	c.in_function = true
	errors_before := c.diagnostics.len
	body_type := c.check_expr(mut expr.body)
	c.in_function = prev_in_function
	c.env.pop_scope()

	// Only check return type if body had no errors (avoid cascading errors)
	if expr.return_type != none && c.diagnostics.len == errors_before {
		body_span := get_expr_span(expr.body)
		// if function declares an error type, expect T!E instead of just T
		expected_ret := if et := err_type {
			Type(TypeResult{
				success: ret_type
				error:   et
			})
		} else {
			ret_type
		}
		c.expect_type(body_type, expected_ret, body_span, 'in function return')
	} else {
		// infer return type from body when not explicitly annotated
		ret_type = body_type
	}

	// build final function type with correct return type (either annotated or inferred)
	final_func_type := TypeFunction{
		params:     param_types
		ret:        ret_type
		error_type: err_type
	}

	// re-register the function with the correct return type if it has a name
	if id := expr.identifier {
		c.env.register_function(id.name, final_func_type)
		c.env.define(id.name, final_func_type)
	}

	return final_func_type
}

fn (mut c TypeChecker) check_call(mut expr ast.FunctionCallExpression) Type {
	if func_type := c.env.lookup_function(expr.identifier.name) {
		return c.check_call_with_type(mut expr, func_type)
	}

	if var_type := c.env.lookup(expr.identifier.name) {
		if var_type is TypeFunction {
			return c.check_call_with_type(mut expr, var_type)
		}
	}

	if enum_type := c.env.lookup_enum_by_variant(expr.identifier.name) {
		variant_name := expr.identifier.name

		if payload_type := enum_type.variants[variant_name] {
			// variant has a payload - check the argument matches
			if expr.arguments.len != 1 {
				c.error_at_span("Enum variant '${variant_name}' expects 1 argument, got ${expr.arguments.len}",
					expr.span)
			} else {
				mut first_arg := expr.arguments[0]
				arg_type := c.check_expr(mut first_arg)
				arg_span := get_expr_span(first_arg)
				c.expect_type(arg_type, payload_type, arg_span, "in enum variant '${variant_name}'")
			}
		} else {
			// Variant has no payload - should have no arguments
			if expr.arguments.len != 0 {
				c.error_at_span("Enum variant '${variant_name}' expects no arguments, got ${expr.arguments.len}",
					expr.span)
			}
		}

		return enum_type
	}

	c.error_at_span("Unknown function '${expr.identifier.name}'", expr.span)
	return t_none()
}

fn (mut c TypeChecker) check_call_with_type(mut expr ast.FunctionCallExpression, func_type TypeFunction) Type {
	if expr.arguments.len != func_type.params.len {
		c.error_at_span("Function '${expr.identifier.name}' expects ${func_type.params.len} arguments, got ${expr.arguments.len}",
			expr.span)
		return func_type.ret
	}

	mut subs := map[string]Type{}

	for i, mut arg in expr.arguments {
		arg_type := c.check_expr(mut arg)
		param_type := func_type.params[i]
		arg_span := get_expr_span(arg)

		if !c.unify(arg_type, param_type, mut subs) {
			instantiated_param := substitute(param_type, subs)
			c.expect_type(arg_type, instantiated_param, arg_span, "in argument ${i + 1} of '${expr.identifier.name}'")
		}
	}

	ret := substitute(func_type.ret, subs)
	if err_type := func_type.error_type {
		return TypeResult{
			success: ret
			error:   substitute(err_type, subs)
		}
	}
	return ret
}

fn (mut c TypeChecker) unify(actual Type, expected Type, mut subs map[string]Type) bool {
	if expected is TypeVar {
		if existing := subs[expected.name] {
			return types_equal(actual, existing)
		}
		subs[expected.name] = actual
		return true
	}

	if actual is TypeArray && expected is TypeArray {
		return c.unify(actual.element, expected.element, mut subs)
	}

	if actual is TypeOption && expected is TypeOption {
		return c.unify(actual.inner, expected.inner, mut subs)
	}

	if actual is TypeResult && expected is TypeResult {
		return c.unify(actual.success, expected.success, mut subs)
			&& c.unify(actual.error, expected.error, mut subs)
	}

	return types_equal(actual, expected)
}

fn (mut c TypeChecker) check_if(mut expr ast.IfExpression) Type {
	cond_type := c.check_expr(mut expr.condition)
	cond_span := get_expr_span(expr.condition)
	c.expect_type(cond_type, t_bool(), cond_span, 'in if condition')

	then_type := c.check_expr(mut expr.body)

	if mut else_body := expr.else_body {
		else_type := c.check_expr(mut else_body)
		if !types_equal(then_type, else_type) {
			// Check if branches are compatible for optional types
			// none + T -> ?T
			if types_equal(then_type, t_none()) {
				return t_option(else_type)
			}
			if types_equal(else_type, t_none()) {
				return t_option(then_type)
			}
			// Check if one branch is an error type (for result types)
			// error E + T -> T!E
			if then_type is TypeStruct {
				return TypeResult{
					success: else_type
					error:   then_type
				}
			}
			if else_type is TypeStruct {
				return TypeResult{
					success: then_type
					error:   else_type
				}
			}
			c.error_at_span('If branches have different types: ${type_to_string(then_type)} and ${type_to_string(else_type)}',
				expr.span)
		}
		return then_type
	}

	return then_type
}

fn (mut c TypeChecker) check_array(mut expr ast.ArrayExpression) Type {
	if expr.elements.len == 0 {
		c.error_at_span('Cannot infer type of empty array literal', expr.span)
		return t_array(t_none())
	}

	mut first_type := t_none()

	for i, mut elem in expr.elements {
		elem_type := c.check_expr(mut elem)
		if i == 0 {
			first_type = elem_type
		} else {
			elem_span := get_expr_span(elem)
			c.expect_type(elem_type, first_type, elem_span, 'in array element')
		}
	}

	return t_array(first_type)
}

fn (mut c TypeChecker) check_array_index(mut expr ast.ArrayIndexExpression) Type {
	arr_type := c.check_expr(mut expr.expression)
	idx_type := c.check_expr(mut expr.index)
	idx_span := get_expr_span(expr.index)

	c.expect_type(idx_type, t_int(), idx_span, 'as array index')

	if arr_type is TypeArray {
		return arr_type.element
	}

	c.error_at_span('Cannot index non-array type ${type_to_string(arr_type)}', expr.span)
	return t_none()
}

fn (mut c TypeChecker) check_struct_def(expr ast.StructExpression) Type {
	mut fields := map[string]Type{}

	for field in expr.fields {
		if resolved := c.resolve_type_identifier(field.typ) {
			fields[field.identifier.name] = resolved
		} else {
			c.error_at_span("Unknown type '${field.typ.identifier.name}' for field '${field.identifier.name}'",
				field.identifier.span)
		}
	}

	struct_type := TypeStruct{
		name:   expr.identifier.name
		fields: fields
	}

	c.env.register_struct(struct_type)
	return struct_type
}

fn (mut c TypeChecker) check_struct_init(mut expr ast.StructInitExpression) Type {
	if struct_def := c.env.lookup_struct(expr.identifier.name) {
		for mut field in expr.fields {
			if expected_type := struct_def.fields[field.identifier.name] {
				actual_type := c.check_expr(mut field.init)
				init_span := get_expr_span(field.init)
				c.expect_type(actual_type, expected_type, init_span, "in field '${field.identifier.name}'")
			} else {
				c.error_at_span("Unknown field '${field.identifier.name}' in struct '${expr.identifier.name}'",
					field.identifier.span)
			}
		}
		return struct_def
	}

	c.error_at_span("Unknown struct '${expr.identifier.name}'", expr.identifier.span)
	return t_none()
}

fn (mut c TypeChecker) check_enum_def(expr ast.EnumExpression) Type {
	mut variants := map[string]?Type{}

	for variant in expr.variants {
		if payload := variant.payload {
			if resolved := c.resolve_type_identifier(payload) {
				variants[variant.identifier.name] = resolved
			} else {
				c.error_at_span("Unknown type '${payload.identifier.name}' in variant '${variant.identifier.name}'",
					variant.identifier.span)
				variants[variant.identifier.name] = none
			}
		} else {
			variants[variant.identifier.name] = none
		}
	}

	enum_type := TypeEnum{
		name:     expr.identifier.name
		variants: variants
	}

	c.env.register_enum(enum_type)
	return enum_type
}

fn (mut c TypeChecker) check_property_access(mut expr ast.PropertyAccessExpression) Type {
	left_type := c.check_expr(mut expr.left)

	if left_type is TypeStruct {
		right := expr.right
		if right is ast.Identifier {
			if field_type := left_type.fields[right.name] {
				return field_type
			}
			c.error_at_span("Struct '${left_type.name}' has no field '${right.name}'",
				right.span)
		}
	}

	return t_none()
}

fn (mut c TypeChecker) check_match(mut expr ast.MatchExpression) Type {
	subject_type := c.check_expr(mut expr.subject)

	if expr.arms.len == 0 {
		return t_none()
	}

	mut first_type := t_none()

	for i, mut arm in expr.arms {
		c.env.push_scope()

		// like Ok(a, b, c)
		pattern := arm.pattern
		if pattern is ast.FunctionCallExpression {
			variant_name := pattern.identifier.name

			// subject is an enum, look up the variant's payload type
			if subject_type is TypeEnum {
				if payload_type := subject_type.variants[variant_name] {
					// bind each argument as a variable to the payload type
					for arg in pattern.arguments {
						if arg is ast.Identifier {
							c.env.define(arg.name, payload_type)
						}
					}
				}
			}
		}

		arm_type := c.check_expr(mut arm.body)
		c.env.pop_scope()

		if i == 0 {
			first_type = arm_type
		} else {
			arm_span := get_expr_span(arm.body)
			c.expect_type(arm_type, first_type, arm_span, 'in match arm')
		}
	}

	return first_type
}
