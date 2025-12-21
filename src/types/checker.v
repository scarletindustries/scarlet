module types

import ast
import typed_ast
import diagnostic
import type_def {
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
	env                    TypeEnv
	diagnostics            []diagnostic.Diagnostic
	in_function            bool
	current_fn_return_type ?Type
}

pub struct CheckResult {
pub:
	diagnostics  []diagnostic.Diagnostic
	success      bool
	env          TypeEnv
	typed_ast    typed_ast.BlockExpression
	program_type Type
}

pub fn check(program ast.BlockExpression) CheckResult {
	mut checker := TypeChecker{
		env:         new_env()
		diagnostics: []diagnostic.Diagnostic{}
	}

	checker.register_builtins()

	typed_block, program_type := checker.check_block(program)

	return CheckResult{
		diagnostics:  checker.diagnostics
		success:      checker.diagnostics.len == 0
		env:          checker.env
		typed_ast:    typed_block
		program_type: program_type
	}
}

fn (mut c TypeChecker) error_at_span(message string, span ast.Span) {
	c.diagnostics << diagnostic.error_at(span.line, span.column, message)
}

fn (c TypeChecker) find_similar_name(name string) ?string {
	all_names := c.env.all_names()
	mut best_match := ''
	mut best_distance := 3 // max distance threshold

	for candidate in all_names {
		dist := levenshtein_distance(name, candidate)
		if dist < best_distance {
			best_distance = dist
			best_match = candidate
		}
	}

	if best_match.len > 0 {
		return best_match
	}
	return none
}

fn levenshtein_distance(a string, b string) int {
	if a.len == 0 {
		return b.len
	}
	if b.len == 0 {
		return a.len
	}

	mut prev := []int{len: b.len + 1, init: index}
	mut curr := []int{len: b.len + 1}

	for i := 1; i <= a.len; i++ {
		curr[0] = i
		for j := 1; j <= b.len; j++ {
			cost := if a[i - 1] == b[j - 1] { 0 } else { 1 }
			deletion := prev[j] + 1
			insertion := curr[j - 1] + 1
			substitution := prev[j - 1] + cost

			mut min_val := deletion
			if insertion < min_val {
				min_val = insertion
			}
			if substitution < min_val {
				min_val = substitution
			}
			curr[j] = min_val
		}

		prev = curr.clone()
		curr = []int{len: b.len + 1}
	}

	return prev[b.len]
}

fn get_typed_span(expr typed_ast.Expression) typed_ast.Span {
	return match expr {
		typed_ast.NumberLiteral {
			expr.span
		}
		typed_ast.StringLiteral {
			expr.span
		}
		typed_ast.BooleanLiteral {
			expr.span
		}
		typed_ast.Identifier {
			expr.span
		}
		typed_ast.VariableBinding {
			expr.span
		}
		typed_ast.ConstBinding {
			expr.span
		}
		typed_ast.BinaryExpression {
			expr.span
		}
		typed_ast.FunctionCallExpression {
			expr.span
		}
		typed_ast.ArrayExpression {
			expr.span
		}
		typed_ast.ArrayIndexExpression {
			expr.span
		}
		typed_ast.IfExpression {
			expr.span
		}
		typed_ast.BlockExpression {
			expr.span
		}
		typed_ast.FunctionExpression {
			if id := expr.identifier {
				id.span
			} else {
				get_typed_span(expr.body)
			}
		}
		typed_ast.MatchExpression {
			get_typed_span(expr.subject)
		}
		typed_ast.OrExpression {
			get_typed_span(expr.expression)
		}
		typed_ast.PropagateNoneExpression {
			get_typed_span(expr.expression)
		}
		typed_ast.ErrorExpression {
			get_typed_span(expr.expression)
		}
		typed_ast.UnaryExpression {
			get_typed_span(expr.expression)
		}
		typed_ast.PropertyAccessExpression {
			get_typed_span(expr.left)
		}
		typed_ast.RangeExpression {
			get_typed_span(expr.start)
		}
		typed_ast.StructExpression {
			expr.identifier.span
		}
		typed_ast.StructInitExpression {
			expr.identifier.span
		}
		typed_ast.EnumExpression {
			expr.identifier.span
		}
		typed_ast.AssertExpression {
			get_typed_span(expr.expression)
		}
		typed_ast.ExportExpression {
			get_typed_span(expr.expression)
		}
		typed_ast.InterpolatedString {
			expr.span
		}
		typed_ast.TypeIdentifier {
			expr.identifier.span
		}
		typed_ast.NoneExpression {
			expr.span
		}
		typed_ast.ErrorNode {
			expr.span
		}
		typed_ast.WildcardPattern {
			expr.span
		}
		typed_ast.ImportDeclaration {
			expr.span
		}
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

fn (mut c TypeChecker) expect_type(actual Type, expected Type, span typed_ast.Span, context string) bool {
	if types_equal(actual, expected) {
		return true
	}
	if expected is TypeResult {
		if types_equal(actual, expected.success) {
			return true
		}
	}
	if expected is TypeOption {
		if types_equal(actual, expected.inner) {
			return true
		}
		if types_equal(actual, t_none()) {
			return true
		}
	}
	c.error_at_span("Type mismatch ${context}: expected '${type_to_string(expected)}', got '${type_to_string(actual)}'",
		ast.Span{ line: span.line, column: span.column })
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

fn (mut c TypeChecker) check_block(block ast.BlockExpression) (typed_ast.BlockExpression, Type) {
	mut typed_body := []typed_ast.Expression{}
	mut last_type := t_none()

	for expr in block.body {
		typed_expr, typ := c.check_expr(expr)
		typed_body << typed_expr
		last_type = typ
	}

	return typed_ast.BlockExpression{
		body:       typed_body
		span:       typed_ast.Span{
			line:   block.span.line
			column: block.span.column
		}
		close_span: typed_ast.Span{
			line:   block.close_span.line
			column: block.close_span.column
		}
	}, last_type
}

fn (mut c TypeChecker) check_expr(expr ast.Expression) (typed_ast.Expression, Type) {
	match expr {
		ast.NumberLiteral {
			typ := if expr.value.contains('.') { t_float() } else { t_int() }
			return typed_ast.NumberLiteral{
				value: expr.value
				span:  typed_ast.Span{
					line:   expr.span.line
					column: expr.span.column
				}
			}, typ
		}
		ast.StringLiteral {
			return typed_ast.StringLiteral{
				value: expr.value
				span:  typed_ast.Span{
					line:   expr.span.line
					column: expr.span.column
				}
			}, t_string()
		}
		ast.InterpolatedString {
			mut typed_parts := []typed_ast.Expression{}
			for part in expr.parts {
				typed_part, _ := c.check_expr(part)
				typed_parts << typed_part
			}
			return typed_ast.InterpolatedString{
				parts: typed_parts
				span:  typed_ast.Span{
					line:   expr.span.line
					column: expr.span.column
				}
			}, t_string()
		}
		ast.BooleanLiteral {
			return typed_ast.BooleanLiteral{
				value: expr.value
				span:  typed_ast.Span{
					line:   expr.span.line
					column: expr.span.column
				}
			}, t_bool()
		}
		ast.NoneExpression {
			return typed_ast.NoneExpression{
				span: convert_span(expr.span)
			}, t_none()
		}
		ast.Identifier {
			typ := if t := c.env.lookup(expr.name) {
				t
			} else {
				if suggestion := c.find_similar_name(expr.name) {
					c.error_at_span("Unknown identifier '${expr.name}'. Did you mean '${suggestion}'?",
						expr.span)
				} else {
					c.error_at_span("Unknown identifier '${expr.name}'", expr.span)
				}
				t_none()
			}
			return typed_ast.Identifier{
				name: expr.name
				span: typed_ast.Span{
					line:   expr.span.line
					column: expr.span.column
				}
			}, typ
		}
		ast.VariableBinding {
			return c.check_variable_binding(expr)
		}
		ast.ConstBinding {
			return c.check_const_binding(expr)
		}
		ast.BinaryExpression {
			return c.check_binary(expr)
		}
		ast.UnaryExpression {
			return c.check_unary(expr)
		}
		ast.FunctionExpression {
			return c.check_function(expr)
		}
		ast.FunctionCallExpression {
			return c.check_call(expr)
		}
		ast.BlockExpression {
			c.env.push_scope()
			typed_block, last_type := c.check_block(expr)
			c.env.pop_scope()
			return typed_block, last_type
		}
		ast.IfExpression {
			return c.check_if(expr)
		}
		ast.ArrayExpression {
			return c.check_array(expr)
		}
		ast.ArrayIndexExpression {
			return c.check_array_index(expr)
		}
		ast.StructExpression {
			return c.check_struct_def(expr)
		}
		ast.StructInitExpression {
			return c.check_struct_init(expr)
		}
		ast.EnumExpression {
			return c.check_enum_def(expr)
		}
		ast.PropertyAccessExpression {
			return c.check_property_access(expr)
		}
		ast.MatchExpression {
			return c.check_match(expr)
		}
		ast.OrExpression {
			return c.check_or(expr)
		}
		ast.ErrorExpression {
			typed_inner, typ := c.check_expr(expr.expression)
			return typed_ast.ErrorExpression{
				expression: typed_inner
			}, typ
		}
		ast.RangeExpression {
			return c.check_range(expr)
		}
		ast.AssertExpression {
			return c.check_assert(expr)
		}
		ast.PropagateNoneExpression {
			return c.check_propagate_none(expr)
		}
		ast.ImportDeclaration {
			return typed_ast.ImportDeclaration{
				path:       expr.path
				specifiers: expr.specifiers.map(fn (s ast.ImportSpecifier) typed_ast.ImportSpecifier {
					return typed_ast.ImportSpecifier{
						identifier: typed_ast.Identifier{
							name: s.identifier.name
							span: typed_ast.Span{
								line:   s.identifier.span.line
								column: s.identifier.span.column
							}
						}
					}
				})
				span:       typed_ast.Span{
					line:   expr.span.line
					column: expr.span.column
				}
			}, t_none()
		}
		ast.ExportExpression {
			typed_inner, typ := c.check_expr(expr.expression)
			return typed_ast.ExportExpression{
				expression: typed_inner
			}, typ
		}
		ast.WildcardPattern {
			return typed_ast.WildcardPattern{
				span: convert_span(expr.span)
			}, t_none()
		}
		ast.ErrorNode {
			return typed_ast.ErrorNode{
				message: expr.message
				span:    convert_span(expr.span)
			}, t_none()
		}
		ast.TypeIdentifier {
			return convert_type_identifier(expr), t_none()
		}
	}
}

fn convert_type_identifier(t ast.TypeIdentifier) typed_ast.TypeIdentifier {
	return typed_ast.TypeIdentifier{
		is_array:    t.is_array
		is_option:   t.is_option
		is_function: t.is_function
		identifier:  typed_ast.Identifier{
			name: t.identifier.name
			span: typed_ast.Span{
				line:   t.identifier.span.line
				column: t.identifier.span.column
			}
		}
		param_types: t.param_types.map(fn (pt ast.TypeIdentifier) typed_ast.TypeIdentifier {
			return convert_type_identifier(pt)
		})
		return_type: convert_optional_type_identifier(t.return_type)
		error_type:  convert_optional_type_identifier(t.error_type)
	}
}

fn convert_optional_type_identifier(t ?&ast.TypeIdentifier) ?&typed_ast.TypeIdentifier {
	if ti := t {
		converted := convert_type_identifier(*ti)
		return &converted
	}
	return none
}

fn convert_optional_type_id(t ?ast.TypeIdentifier) ?typed_ast.TypeIdentifier {
	if ti := t {
		return convert_type_identifier(ti)
	}
	return none
}

fn convert_optional_identifier(id ?ast.Identifier) ?typed_ast.Identifier {
	if i := id {
		return convert_identifier(i)
	}
	return none
}

fn convert_identifier(id ast.Identifier) typed_ast.Identifier {
	return typed_ast.Identifier{
		name: id.name
		span: typed_ast.Span{
			line:   id.span.line
			column: id.span.column
		}
	}
}

fn convert_span(s ast.Span) typed_ast.Span {
	return typed_ast.Span{
		line:   s.line
		column: s.column
	}
}

fn (mut c TypeChecker) check_binding_type(name string, annotation ?ast.TypeIdentifier, typed_init typed_ast.Expression, init_type Type, context string) Type {
	if annot := annotation {
		if expected := c.resolve_type_identifier(annot) {
			init_span := get_typed_span(typed_init)
			c.expect_type(init_type, expected, init_span, context)
			c.env.define(name, expected)
			return expected
		} else {
			c.error_at_span("Unknown type '${annot.identifier.name}'", annot.identifier.span)
			c.env.define(name, init_type)
			return init_type
		}
	} else {
		c.env.define(name, init_type)
		return init_type
	}
}

fn (mut c TypeChecker) check_variable_binding(expr ast.VariableBinding) (typed_ast.Expression, Type) {
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

		c.env.define(expr.identifier.name, preliminary_func_type)
	}

	typed_init, init_type := c.check_expr(expr.init)
	final_type := c.check_binding_type(expr.identifier.name, expr.typ, typed_init, init_type,
		'in variable binding')

	return typed_ast.VariableBinding{
		identifier: convert_identifier(expr.identifier)
		typ:        convert_optional_type_id(expr.typ)
		init:       typed_init
		span:       convert_span(expr.span)
	}, final_type
}

fn (mut c TypeChecker) check_const_binding(expr ast.ConstBinding) (typed_ast.Expression, Type) {
	if c.in_function {
		c.error_at_span("'const' declarations are only allowed at the top level, not inside functions",
			expr.span)
	}

	typed_init, init_type := c.check_expr(expr.init)
	final_type := c.check_binding_type(expr.identifier.name, expr.typ, typed_init, init_type,
		'in const binding')

	return typed_ast.ConstBinding{
		identifier: convert_identifier(expr.identifier)
		typ:        convert_optional_type_id(expr.typ)
		init:       typed_init
		span:       convert_span(expr.span)
	}, final_type
}

fn (mut c TypeChecker) check_binary(expr ast.BinaryExpression) (typed_ast.Expression, Type) {
	typed_left, left_type := c.check_expr(expr.left)
	typed_right, right_type := c.check_expr(expr.right)

	op_str := expr.op.kind.str()
	result_type := match expr.op.kind {
		.punc_plus {
			if types_equal(left_type, t_string()) && types_equal(right_type, t_string()) {
				t_string()
			} else if types_equal(left_type, t_string()) || types_equal(right_type, t_string()) {
				c.error_at_span("Cannot concatenate '${type_to_string(left_type)}' with '${type_to_string(right_type)}': use string interpolation instead",
					expr.span)
				t_string()
			} else if !is_numeric(left_type) {
				c.error_at_span("Left operand of '${op_str}' must be numeric, got '${type_to_string(left_type)}'",
					expr.span)
				t_int()
			} else if !is_numeric(right_type) {
				c.error_at_span("Right operand of '${op_str}' must be numeric, got '${type_to_string(right_type)}'",
					expr.span)
				t_int()
			} else {
				if !types_equal(left_type, right_type) {
					c.error_at_span("Cannot apply '${op_str}' to '${type_to_string(left_type)}' and '${type_to_string(right_type)}': operands must have the same type",
						expr.span)
				}
				left_type
			}
		}
		.punc_minus, .punc_mul, .punc_div, .punc_mod {
			if !is_numeric(left_type) {
				c.error_at_span("Left operand of '${op_str}' must be numeric, got '${type_to_string(left_type)}'",
					expr.span)
				t_int()
			} else if !is_numeric(right_type) {
				c.error_at_span("Right operand of '${op_str}' must be numeric, got '${type_to_string(right_type)}'",
					expr.span)
				t_int()
			} else {
				if !types_equal(left_type, right_type) {
					c.error_at_span("Cannot apply '${op_str}' to '${type_to_string(left_type)}' and '${type_to_string(right_type)}': operands must have the same type",
						expr.span)
				}
				left_type
			}
		}
		.punc_lt, .punc_gt, .punc_lte, .punc_gte {
			if !is_numeric(left_type) || !is_numeric(right_type) {
				c.error_at_span("Cannot compare '${type_to_string(left_type)}' with '${type_to_string(right_type)}': operator '${op_str}' requires numeric operands",
					expr.span)
			}
			t_bool()
		}
		.punc_equals_comparator, .punc_not_equal {
			if !types_equal(left_type, right_type) {
				c.error_at_span('Cannot compare ${type_to_string(left_type)} with ${type_to_string(right_type)}',
					expr.span)
			}
			t_bool()
		}
		.logical_and, .logical_or {
			c.expect_type(left_type, t_bool(), convert_span(expr.span), 'in logical expression')
			c.expect_type(right_type, t_bool(), convert_span(expr.span), 'in logical expression')
			t_bool()
		}
		else {
			t_none()
		}
	}

	return typed_ast.BinaryExpression{
		left:  typed_left
		right: typed_right
		op:    typed_ast.Operator{
			kind: expr.op.kind
		}
		span:  convert_span(expr.span)
	}, result_type
}

fn (mut c TypeChecker) check_unary(expr ast.UnaryExpression) (typed_ast.Expression, Type) {
	typed_inner, operand_type := c.check_expr(expr.expression)
	span := get_typed_span(typed_inner)
	op_str := expr.op.kind.str()

	result_type := match expr.op.kind {
		.punc_minus {
			if !is_numeric(operand_type) {
				c.error_at_span("Operator '${op_str}' requires a numeric operand, got '${type_to_string(operand_type)}'",
					ast.Span{ line: span.line, column: span.column })
			}
			operand_type
		}
		.punc_exclamation_mark {
			c.expect_type(operand_type, t_bool(), span, "for operator '${op_str}'")
			t_bool()
		}
		else {
			t_none()
		}
	}

	return typed_ast.UnaryExpression{
		expression: typed_inner
		op:         typed_ast.Operator{
			kind: expr.op.kind
		}
	}, result_type
}

fn (mut c TypeChecker) check_function(expr ast.FunctionExpression) (typed_ast.Expression, Type) {
	mut param_types := []Type{}
	mut seen_params := map[string]bool{}

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
		if param.identifier.name in seen_params {
			c.error_at_span("Duplicate parameter '${param.identifier.name}'", param.identifier.span)
		}
		seen_params[param.identifier.name] = true

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
	prev_fn_return_type := c.current_fn_return_type
	c.in_function = true
	c.current_fn_return_type = if expr.return_type != none {
		if et := err_type {
			Type(TypeResult{
				success: ret_type
				error:   et
			})
		} else {
			ret_type
		}
	} else {
		none
	}
	errors_before := c.diagnostics.len
	typed_body, body_type := c.check_expr(expr.body)
	c.in_function = prev_in_function
	c.current_fn_return_type = prev_fn_return_type
	c.env.pop_scope()

	if expr.return_type != none && c.diagnostics.len == errors_before {
		body_span := get_typed_span(typed_body)
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
		ret_type = body_type
	}

	final_func_type := TypeFunction{
		params:     param_types
		ret:        ret_type
		error_type: err_type
	}

	if id := expr.identifier {
		c.env.register_function(id.name, final_func_type)
		c.env.define(id.name, final_func_type)
	}

	mut typed_params := []typed_ast.FunctionParameter{}
	for p in expr.params {
		typed_params << typed_ast.FunctionParameter{
			identifier: convert_identifier(p.identifier)
			typ:        convert_optional_type_id(p.typ)
		}
	}

	return typed_ast.FunctionExpression{
		identifier:  convert_optional_identifier(expr.identifier)
		return_type: convert_optional_type_id(expr.return_type)
		error_type:  convert_optional_type_id(expr.error_type)
		params:      typed_params
		body:        typed_body
		span:        convert_span(expr.span)
	}, final_func_type
}

fn (mut c TypeChecker) check_call(expr ast.FunctionCallExpression) (typed_ast.Expression, Type) {
	if func_type := c.env.lookup_function(expr.identifier.name) {
		return c.check_call_with_type(expr, func_type)
	}

	if var_type := c.env.lookup(expr.identifier.name) {
		if var_type is TypeFunction {
			return c.check_call_with_type(expr, var_type)
		}
	}

	if enum_type := c.env.lookup_enum_by_variant(expr.identifier.name) {
		variant_name := expr.identifier.name

		mut typed_args := []typed_ast.Expression{}
		if payload_type := enum_type.variants[variant_name] {
			if expr.arguments.len != 1 {
				c.error_at_span("Enum variant '${variant_name}' expects 1 argument, got ${expr.arguments.len}",
					expr.span)
			} else {
				typed_arg, arg_type := c.check_expr(expr.arguments[0])
				typed_args << typed_arg
				arg_span := get_typed_span(typed_arg)
				c.expect_type(arg_type, payload_type, arg_span, "in enum variant '${variant_name}'")
			}
		} else {
			if expr.arguments.len != 0 {
				c.error_at_span("Enum variant '${variant_name}' expects no arguments, got ${expr.arguments.len}",
					expr.span)
			}
		}

		return typed_ast.FunctionCallExpression{
			identifier: convert_identifier(expr.identifier)
			arguments:  typed_args
			span:       convert_span(expr.span)
		}, enum_type
	}

	if suggestion := c.find_similar_name(expr.identifier.name) {
		c.error_at_span("'${expr.identifier.name}' is not defined. Did you mean '${suggestion}'?",
			expr.span)
	} else {
		c.error_at_span("'${expr.identifier.name}' is not defined", expr.span)
	}

	mut typed_args := []typed_ast.Expression{}
	for arg in expr.arguments {
		typed_arg, _ := c.check_expr(arg)
		typed_args << typed_arg
	}

	return typed_ast.FunctionCallExpression{
		identifier: convert_identifier(expr.identifier)
		arguments:  typed_args
		span:       convert_span(expr.span)
	}, t_none()
}

fn (mut c TypeChecker) check_call_with_type(expr ast.FunctionCallExpression, func_type TypeFunction) (typed_ast.Expression, Type) {
	if expr.arguments.len != func_type.params.len {
		c.error_at_span("Function '${expr.identifier.name}' expects ${func_type.params.len} arguments, got ${expr.arguments.len}",
			expr.span)

		mut typed_args := []typed_ast.Expression{}
		for arg in expr.arguments {
			typed_arg, _ := c.check_expr(arg)
			typed_args << typed_arg
		}

		return typed_ast.FunctionCallExpression{
			identifier: convert_identifier(expr.identifier)
			arguments:  typed_args
			span:       convert_span(expr.span)
		}, func_type.ret
	}

	mut subs := map[string]Type{}
	mut typed_args := []typed_ast.Expression{}

	for i, arg in expr.arguments {
		typed_arg, arg_type := c.check_expr(arg)
		typed_args << typed_arg
		param_type := func_type.params[i]
		arg_span := get_typed_span(typed_arg)

		if !c.unify(arg_type, param_type, mut subs) {
			instantiated_param := substitute(param_type, subs)
			c.expect_type(arg_type, instantiated_param, arg_span, "in argument ${i + 1} of '${expr.identifier.name}'")
		}
	}

	ret := substitute(func_type.ret, subs)
	result_type := if err_type := func_type.error_type {
		Type(TypeResult{
			success: ret
			error:   substitute(err_type, subs)
		})
	} else {
		ret
	}

	return typed_ast.FunctionCallExpression{
		identifier: convert_identifier(expr.identifier)
		arguments:  typed_args
		span:       convert_span(expr.span)
	}, result_type
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

fn (mut c TypeChecker) check_if(expr ast.IfExpression) (typed_ast.Expression, Type) {
	typed_cond, cond_type := c.check_expr(expr.condition)
	cond_span := get_typed_span(typed_cond)
	c.expect_type(cond_type, t_bool(), cond_span, 'in if condition')

	typed_body, then_type := c.check_expr(expr.body)

	typed_else, result_type := if else_body := expr.else_body {
		typed_else_body, else_type := c.check_expr(else_body)
		final_type := if !types_equal(then_type, else_type) {
			if types_equal(then_type, t_none()) {
				t_option(else_type)
			} else if types_equal(else_type, t_none()) {
				t_option(then_type)
			} else if then_type is TypeStruct {
				Type(TypeResult{
					success: else_type
					error:   then_type
				})
			} else if else_type is TypeStruct {
				Type(TypeResult{
					success: then_type
					error:   else_type
				})
			} else {
				c.error_at_span("'if' branch returns '${type_to_string(then_type)}' but 'else' branch returns '${type_to_string(else_type)}'",
					expr.span)
				then_type
			}
		} else {
			then_type
		}
		?typed_ast.Expression(typed_else_body), final_type
	} else {
		?typed_ast.Expression(none), then_type
	}

	return typed_ast.IfExpression{
		condition: typed_cond
		body:      typed_body
		span:      convert_span(expr.span)
		else_body: typed_else
	}, result_type
}

fn (mut c TypeChecker) check_array(expr ast.ArrayExpression) (typed_ast.Expression, Type) {
	if expr.elements.len == 0 {
		c.error_at_span("Cannot infer type of empty array. Provide a type annotation, e.g.: 'items: []Int = []'",
			expr.span)
		return typed_ast.ArrayExpression{
			elements: []
			span:     convert_span(expr.span)
		}, t_array(t_none())
	}

	mut typed_elements := []typed_ast.Expression{}
	mut first_type := t_none()

	for i, elem in expr.elements {
		typed_elem, elem_type := c.check_expr(elem)
		typed_elements << typed_elem
		if i == 0 {
			first_type = elem_type
		} else {
			elem_span := get_typed_span(typed_elem)
			c.expect_type(elem_type, first_type, elem_span, 'in array element')
		}
	}

	return typed_ast.ArrayExpression{
		elements: typed_elements
		span:     convert_span(expr.span)
	}, t_array(first_type)
}

fn (mut c TypeChecker) check_array_index(expr ast.ArrayIndexExpression) (typed_ast.Expression, Type) {
	typed_arr, arr_type := c.check_expr(expr.expression)
	typed_idx, idx_type := c.check_expr(expr.index)
	idx_span := get_typed_span(typed_idx)

	c.expect_type(idx_type, t_int(), idx_span, 'as array index')

	element_type := if arr_type is TypeArray {
		arr_type.element
	} else {
		c.error_at_span('Cannot index non-array type ${type_to_string(arr_type)}', expr.span)
		t_none()
	}

	return typed_ast.ArrayIndexExpression{
		expression: typed_arr
		index:      typed_idx
		span:       convert_span(expr.span)
	}, element_type
}

fn (mut c TypeChecker) check_struct_def(expr ast.StructExpression) (typed_ast.Expression, Type) {
	mut fields := map[string]Type{}

	for field in expr.fields {
		if field.identifier.name in fields {
			c.error_at_span("Duplicate field '${field.identifier.name}' in struct '${expr.identifier.name}'",
				field.identifier.span)
			continue
		}
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

	mut typed_fields := []typed_ast.StructField{}
	for f in expr.fields {
		mut typed_init := ?typed_ast.Expression(none)
		if init := f.init {
			typed_expr, _ := c.check_expr(init)
			typed_init = typed_expr
		}
		typed_fields << typed_ast.StructField{
			identifier: convert_identifier(f.identifier)
			typ:        convert_type_identifier(f.typ)
			init:       typed_init
		}
	}

	return typed_ast.StructExpression{
		identifier: convert_identifier(expr.identifier)
		fields:     typed_fields
		span:       convert_span(expr.span)
		close_span: convert_span(expr.close_span)
	}, struct_type
}

fn (mut c TypeChecker) check_struct_init(expr ast.StructInitExpression) (typed_ast.Expression, Type) {
	struct_type := if struct_def := c.env.lookup_struct(expr.identifier.name) {
		struct_def
	} else {
		c.error_at_span("Unknown struct '${expr.identifier.name}'", expr.identifier.span)
		TypeStruct{
			name:   expr.identifier.name
			fields: map[string]Type{}
		}
	}

	mut provided_fields := map[string]bool{}
	mut typed_fields := []typed_ast.StructInitField{}

	for field in expr.fields {
		if field.identifier.name in provided_fields {
			c.error_at_span("Duplicate field '${field.identifier.name}' in struct initializer",
				field.identifier.span)
		}
		provided_fields[field.identifier.name] = true

		typed_init, actual_type := c.check_expr(field.init)
		if expected_type := struct_type.fields[field.identifier.name] {
			init_span := get_typed_span(typed_init)
			c.expect_type(actual_type, expected_type, init_span, "in field '${field.identifier.name}'")
		} else {
			available := struct_type.fields.keys().join(', ')
			c.error_at_span("Struct '${expr.identifier.name}' has no field '${field.identifier.name}'. Available fields: ${available}",
				field.identifier.span)
		}
		typed_fields << typed_ast.StructInitField{
			identifier: convert_identifier(field.identifier)
			init:       typed_init
		}
	}

	mut missing_fields := []string{}
	for field_name, _ in struct_type.fields {
		if field_name !in provided_fields {
			missing_fields << field_name
		}
	}
	if missing_fields.len > 0 {
		c.error_at_span("Missing required fields in '${expr.identifier.name}': ${missing_fields.join(', ')}",
			expr.identifier.span)
	}

	return typed_ast.StructInitExpression{
		identifier: convert_identifier(expr.identifier)
		fields:     typed_fields
	}, struct_type
}

fn (mut c TypeChecker) check_enum_def(expr ast.EnumExpression) (typed_ast.Expression, Type) {
	mut variants := map[string]?Type{}

	for variant in expr.variants {
		if variant.identifier.name in variants {
			c.error_at_span("Duplicate variant '${variant.identifier.name}' in enum '${expr.identifier.name}'",
				variant.identifier.span)
			continue
		}
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

	typed_variants := expr.variants.map(fn (v ast.EnumVariant) typed_ast.EnumVariant {
		return typed_ast.EnumVariant{
			identifier: convert_identifier(v.identifier)
			payload:    if p := v.payload {
				?typed_ast.TypeIdentifier(convert_type_identifier(p))
			} else {
				none
			}
		}
	})

	return typed_ast.EnumExpression{
		identifier: convert_identifier(expr.identifier)
		variants:   typed_variants
		span:       convert_span(expr.span)
		close_span: convert_span(expr.close_span)
	}, enum_type
}

fn (mut c TypeChecker) check_property_access(expr ast.PropertyAccessExpression) (typed_ast.Expression, Type) {
	typed_left, left_type := c.check_expr(expr.left)

	if expr.right is ast.FunctionCallExpression {
		typed_right, right_type := c.check_expr(expr.right)
		return typed_ast.PropertyAccessExpression{
			left:  typed_left
			right: typed_right
		}, right_type
	}

	if expr.right !is ast.Identifier {
		span := ast.get_span(expr.right)
		c.error_at_span('Expected identifier in property access', span)
		return typed_ast.PropertyAccessExpression{
			left:  typed_left
			right: typed_ast.ErrorNode{
				message: 'Expected identifier'
				span:    convert_span(span)
			}
		}, t_none()
	}

	right := expr.right as ast.Identifier

	typed_right := typed_ast.Identifier{
		name: right.name
		span: convert_span(right.span)
	}

	result_type := if left_type is TypeStruct {
		if field_type := left_type.fields[right.name] {
			field_type
		} else {
			available := left_type.fields.keys().join(', ')
			c.error_at_span("Struct '${left_type.name}' has no field '${right.name}'. Available fields: ${available}",
				right.span)
			t_none()
		}
	} else {
		c.error_at_span("Cannot access property '${right.name}' on type '${type_to_string(left_type)}'",
			right.span)
		t_none()
	}

	return typed_ast.PropertyAccessExpression{
		left:  typed_left
		right: typed_right
	}, result_type
}

fn (mut c TypeChecker) check_match(expr ast.MatchExpression) (typed_ast.Expression, Type) {
	typed_subject, subject_type := c.check_expr(expr.subject)

	if expr.arms.len == 0 {
		return typed_ast.MatchExpression{
			subject:    typed_subject
			arms:       []
			span:       convert_span(expr.span)
			close_span: convert_span(expr.close_span)
		}, t_none()
	}

	mut first_type := t_none()
	mut typed_arms := []typed_ast.MatchArm{}
	mut covered_variants := map[string]bool{}
	mut has_wildcard := false

	for i, arm in expr.arms {
		c.env.push_scope()

		if arm.pattern is ast.WildcardPattern {
			has_wildcard = true
		} else if arm.pattern is ast.FunctionCallExpression {
			covered_variants[arm.pattern.identifier.name] = true
		} else if arm.pattern is ast.Identifier {
			covered_variants[arm.pattern.name] = true
		}

		typed_pattern, _ := c.check_pattern(arm.pattern, subject_type)

		typed_body, arm_type := c.check_expr(arm.body)
		c.env.pop_scope()

		typed_arms << typed_ast.MatchArm{
			pattern: typed_pattern
			body:    typed_body
		}

		if i == 0 {
			first_type = arm_type
		} else {
			arm_span := get_typed_span(typed_body)
			c.expect_type(arm_type, first_type, arm_span, 'in match arm')
		}
	}

	if subject_type is TypeEnum && !has_wildcard {
		mut missing := []string{}
		for variant_name, _ in subject_type.variants {
			if variant_name !in covered_variants {
				missing << variant_name
			}
		}
		if missing.len > 0 {
			subject_span := get_typed_span(typed_subject)
			c.error_at_span('Match is not exhaustive, missing variants: ${missing.join(', ')}',
				ast.Span{ line: subject_span.line, column: subject_span.column })
		}
	}

	return typed_ast.MatchExpression{
		subject:    typed_subject
		arms:       typed_arms
		span:       convert_span(expr.span)
		close_span: convert_span(expr.close_span)
	}, first_type
}

fn (mut c TypeChecker) check_pattern(pattern ast.Expression, subject_type Type) (typed_ast.Expression, Type) {
	if pattern is ast.FunctionCallExpression {
		variant_name := pattern.identifier.name

		if subject_type is TypeEnum {
			if payload_type := subject_type.variants[variant_name] {
				for arg in pattern.arguments {
					if arg is ast.Identifier {
						c.env.define(arg.name, payload_type)
					}
				}
			}
		}

		mut typed_args := []typed_ast.Expression{}
		for arg in pattern.arguments {
			typed_arg, _ := c.check_expr(arg)
			typed_args << typed_arg
		}

		return typed_ast.FunctionCallExpression{
			identifier: convert_identifier(pattern.identifier)
			arguments:  typed_args
			span:       convert_span(pattern.span)
		}, subject_type
	}

	return c.check_expr(pattern)
}

fn (mut c TypeChecker) check_or(expr ast.OrExpression) (typed_ast.Expression, Type) {
	typed_inner, inner_type := c.check_expr(expr.expression)

	mut success_type := inner_type
	mut error_type := t_none()

	if inner_type is TypeOption {
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

	typed_body, body_type := c.check_expr(expr.body)
	body_span := get_typed_span(typed_body)

	c.expect_type(body_type, success_type, body_span, "in 'or' fallback")

	if expr.receiver != none {
		c.env.pop_scope()
	}

	return typed_ast.OrExpression{
		expression:    typed_inner
		receiver:      convert_optional_identifier(expr.receiver)
		body:          typed_body
		resolved_type: inner_type
	}, success_type
}

fn (mut c TypeChecker) check_range(expr ast.RangeExpression) (typed_ast.Expression, Type) {
	typed_start, start_type := c.check_expr(expr.start)
	typed_end, end_type := c.check_expr(expr.end)

	if !types_equal(start_type, t_int()) {
		start_span := get_typed_span(typed_start)
		c.error_at_span('Range start must be Int, got ${type_to_string(start_type)}',
			ast.Span{ line: start_span.line, column: start_span.column })
	}

	if !types_equal(end_type, t_int()) {
		end_span := get_typed_span(typed_end)
		c.error_at_span('Range end must be Int, got ${type_to_string(end_type)}', ast.Span{
			line:   end_span.line
			column: end_span.column
		})
	}

	return typed_ast.RangeExpression{
		start: typed_start
		end:   typed_end
	}, t_array(t_int())
}

fn (mut c TypeChecker) check_assert(expr ast.AssertExpression) (typed_ast.Expression, Type) {
	typed_cond, cond_type := c.check_expr(expr.expression)
	cond_span := get_typed_span(typed_cond)
	c.expect_type(cond_type, t_bool(), cond_span, 'in assert condition')

	typed_msg, _ := c.check_expr(expr.message)

	return typed_ast.AssertExpression{
		expression: typed_cond
		message:    typed_msg
		span:       convert_span(expr.span)
	}, t_none()
}

fn (mut c TypeChecker) check_propagate_none(expr ast.PropagateNoneExpression) (typed_ast.Expression, Type) {
	typed_inner, inner_type := c.check_expr(expr.expression)
	inner_span := get_typed_span(typed_inner)

	if !c.in_function {
		c.error_at_span("'?' can only be used inside a function", ast.Span{
			line:   inner_span.line
			column: inner_span.column
		})
	} else if fn_ret := c.current_fn_return_type {
		if fn_ret !is TypeOption {
			c.error_at_span("'?' can only be used in a function that returns an Option type, but this function returns '${type_to_string(fn_ret)}'",
				ast.Span{ line: inner_span.line, column: inner_span.column })
		}
	} else {
		c.error_at_span("'?' can only be used in a function that declares an Option return type",
			ast.Span{ line: inner_span.line, column: inner_span.column })
	}

	result_type := if inner_type is TypeOption {
		inner_type.inner
	} else {
		c.error_at_span("'?' can only be used on Option types, got '${type_to_string(inner_type)}'",
			ast.Span{ line: inner_span.line, column: inner_span.column })
		inner_type
	}

	return typed_ast.PropagateNoneExpression{
		expression:    typed_inner
		resolved_type: inner_type
	}, result_type
}
