module types

import ast
import typed_ast
import diagnostic
import span { Span, range_span }
import type_def {
	Type,
	TypeArray,
	TypeEnum,
	TypeFunction,
	TypeOption,
	TypeResult,
	TypeStruct,
	TypeTuple,
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
	t_tuple,
	t_var,
	type_to_string,
	types_equal,
}

pub struct TypePosition {
pub:
	line      int
	column    int
	end_col   int
	name      string
	type_info Type
	def_line  int // definition location (0 if unknown)
	def_col   int
	def_end   int
	doc       ?string
}

pub struct TypeChecker {
mut:
	env                    TypeEnv
	diagnostics            []diagnostic.Diagnostic
	in_function            bool
	current_fn_return_type ?Type
	param_subs             map[string]Type // tracks inferred parameter types
	type_positions         []TypePosition
}

pub struct CheckResult {
pub:
	diagnostics    []diagnostic.Diagnostic
	success        bool
	env            TypeEnv
	typed_ast      typed_ast.BlockExpression
	program_type   Type
	type_positions []TypePosition
}

pub fn check(program ast.BlockExpression) CheckResult {
	mut checker := TypeChecker{
		env:         new_env()
		diagnostics: []diagnostic.Diagnostic{}
	}

	checker.register_builtins()

	typed_block, program_type := checker.check_block(program)

	return CheckResult{
		diagnostics:    checker.diagnostics
		success:        !diagnostic.has_errors(checker.diagnostics)
		env:            checker.env
		typed_ast:      typed_block
		program_type:   program_type
		type_positions: checker.type_positions
	}
}

fn (mut c TypeChecker) error_at_span(message string, s Span) {
	c.diagnostics << diagnostic.error_at(s.start_line, s.start_column, message)
}

fn (mut c TypeChecker) warning_at_span(message string, s Span) {
	c.diagnostics << diagnostic.warning_at(s.start_line, s.start_column, message)
}

// error_at_token creates an error with a proper range for a token
fn (mut c TypeChecker) error_at_token(message string, s Span, token_len int) {
	c.diagnostics << diagnostic.Diagnostic{
		span:     range_span(s.start_line, s.start_column, s.end_column)
		severity: .error
		message:  message
	}
}

fn (mut c TypeChecker) record_type(name string, typ Type, s Span, doc ?string) {
	// Look up definition location
	mut def_line := 0
	mut def_col := 0
	mut def_end := 0
	if def_loc := c.env.lookup_definition(name) {
		def_line = def_loc.line
		def_col = def_loc.column
		def_end = def_loc.end_col
	}

	c.type_positions << TypePosition{
		line:      s.start_line
		column:    s.start_column
		end_col:   s.end_column
		name:      name
		type_info: typ
		def_line:  def_line
		def_col:   def_col
		def_end:   def_end
		doc:       doc
	}
}

fn type_var_name_from_index(id int) string {
	mut result := ''
	mut n := id
	for {
		result = [u8(`a` + n % 26)].bytestr() + result
		n = n / 26 - 1
		if n < 0 {
			break
		}
	}
	return result
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

	c.env.register_function('str_split', TypeFunction{
		params: [t_string(), t_string()]
		ret:    t_array(t_string())
	})
}

fn (mut c TypeChecker) expect_type(actual Type, expected Type, s Span, context string) bool {
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
		s)
	return false
}

fn (mut c TypeChecker) unify_arm_types(a Type, b Type, s Span) Type {
	if types_equal(a, b) {
		return a
	}

	// None!E + T → T!E
	if a is TypeResult && types_equal(a.success, t_none()) {
		if b is TypeResult {
			c.expect_type(b.error, a.error, s, 'in match arm')
			if types_equal(b.success, t_none()) {
				return a
			}
			return Type(TypeResult{
				success: b.success
				error:   a.error
			})
		}
		return Type(TypeResult{
			success: b
			error:   a.error
		})
	}

	// T + None!E → T!E
	if b is TypeResult && types_equal(b.success, t_none()) {
		if a is TypeResult {
			c.expect_type(b.error, a.error, s, 'in match arm')
			return a
		}
		return Type(TypeResult{
			success: a
			error:   b.error
		})
	}

	// T!E + T!E → verify same
	if a is TypeResult && b is TypeResult {
		c.expect_type(b.success, a.success, s, 'in match arm')
		c.expect_type(b.error, a.error, s, 'in match arm')
		return a
	}

	// None + T → ?T
	if types_equal(a, t_none()) && !types_equal(b, t_none()) {
		return t_option(b)
	}
	if types_equal(b, t_none()) && !types_equal(a, t_none()) {
		return t_option(a)
	}

	c.expect_type(b, a, s, 'in match arm')
	return a
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

	if t.is_array {
		elem := t.element_type or { return none }
		elem_type := c.resolve_type_identifier(*elem) or { return none }
		mut base_type := t_array(elem_type)
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

	if t.is_option {
		base_type = t_option(base_type)
	}

	return base_type
}

fn (mut c TypeChecker) check_block(block ast.BlockExpression) (typed_ast.BlockExpression, Type) {
	mut typed_body := []typed_ast.Node{}
	mut last_type := t_none()

	for i, node in block.body {
		typed_node, typ := c.check_node(node)
		typed_body << typed_node
		last_type = typ

		// For all expressions except the last one (which is the return value),
		// check that non-None values are consumed (statements always return None)
		is_last := i == block.body.len - 1
		if !is_last && node is ast.Expression && !types_equal(typ, t_none()) {
			node_span := ast.node_span(node)
			c.error_at_span("Expression of type '${type_to_string(typ)}' must be consumed. Assign it to a variable or use '${type_to_string(typ)} =' to discard",
				node_span)
		}
	}

	return typed_ast.BlockExpression{
		body: typed_body
		span: block.span
	}, last_type
}

fn (mut c TypeChecker) check_node(node ast.Node) (typed_ast.Node, Type) {
	match node {
		ast.Statement {
			return c.check_statement(node)
		}
		ast.Expression {
			expr, typ := c.check_expr(node)
			return typed_ast.Node(expr), typ
		}
	}
}

fn (mut c TypeChecker) check_statement(stmt ast.Statement) (typed_ast.Node, Type) {
	match stmt {
		ast.VariableBinding {
			return c.check_variable_binding(stmt)
		}
		ast.ConstBinding {
			return c.check_const_binding(stmt)
		}
		ast.TypePatternBinding {
			return c.check_type_pattern_binding(stmt)
		}
		ast.TupleDestructuringBinding {
			return c.check_tuple_destructuring(stmt)
		}
		ast.FunctionDeclaration {
			return c.check_function_declaration(stmt)
		}
		ast.StructDeclaration {
			return c.check_struct_decl(stmt)
		}
		ast.EnumDeclaration {
			return c.check_enum_decl(stmt)
		}
		ast.ImportDeclaration {
			s := typed_ast.Statement(typed_ast.ImportDeclaration{
				path:       stmt.path
				specifiers: stmt.specifiers.map(fn (s ast.ImportSpecifier) typed_ast.ImportSpecifier {
					return typed_ast.ImportSpecifier{
						identifier: typed_ast.Identifier{
							name: s.identifier.name
							span: s.identifier.span
						}
					}
				})
				span:       stmt.span
			})
			return typed_ast.Node(s), t_none()
		}
		ast.ExportDeclaration {
			typed_inner, typ := c.check_statement(stmt.declaration)
			inner_stmt := typed_inner as typed_ast.Statement
			s := typed_ast.Statement(typed_ast.ExportDeclaration{
				declaration: inner_stmt
				span:        convert_span(stmt.span)
			})
			return typed_ast.Node(s), typ
		}
	}
}

fn (mut c TypeChecker) check_expr(expr ast.Expression) (typed_ast.Expression, Type) {
	match expr {
		ast.NumberLiteral {
			typ := if expr.value.contains('.') { t_float() } else { t_int() }
			return typed_ast.NumberLiteral{
				value: expr.value
				span:  expr.span
			}, typ
		}
		ast.StringLiteral {
			return typed_ast.StringLiteral{
				value: expr.value
				span:  expr.span
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
				span:  expr.span
			}, t_string()
		}
		ast.BooleanLiteral {
			return typed_ast.BooleanLiteral{
				value: expr.value
				span:  expr.span
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
			c.record_type(expr.name, typ, expr.span, c.env.lookup_doc(expr.name))
			return typed_ast.Identifier{
				name: expr.name
				span: expr.span
			}, typ
		}
		ast.BinaryExpression {
			return c.check_binary(expr)
		}
		ast.UnaryExpression {
			return c.check_unary(expr)
		}
		ast.FunctionExpression {
			return c.check_function_expression(expr)
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
		ast.TupleExpression {
			return c.check_tuple(expr)
		}
		ast.ArrayIndexExpression {
			return c.check_array_index(expr)
		}
		ast.StructInitExpression {
			return c.check_struct_init(expr)
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
				span:       expr.span
			}, Type(TypeResult{
				success: t_none()
				error:   typ
			})
		}
		ast.RangeExpression {
			return c.check_range(expr)
		}
		ast.SpreadExpression {
			// SpreadExpression is handled specially inside array expressions
			// If we get here, it means spread was used outside an array context
			c.error_at_span('Spread operator can only be used inside array literals or patterns',
				expr.span)
			if inner := expr.expression {
				typed_inner, inner_type := c.check_expr(inner)
				return typed_ast.SpreadExpression{
					expression: typed_inner
					span:       convert_span(expr.span)
				}, inner_type
			}
			return typed_ast.SpreadExpression{
				expression: none
				span:       convert_span(expr.span)
			}, t_none()
		}
		ast.WildcardPattern {
			return typed_ast.WildcardPattern{
				span: convert_span(expr.span)
			}, t_none()
		}
		ast.OrPattern {
			mut typed_patterns := []typed_ast.Expression{}
			for pattern in expr.patterns {
				typed_pattern, _ := c.check_expr(pattern)
				typed_patterns << typed_pattern
			}
			return typed_ast.OrPattern{
				patterns: typed_patterns
				span:     convert_span(expr.span)
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
		is_array:     t.is_array
		is_option:    t.is_option
		is_function:  t.is_function
		identifier:   typed_ast.Identifier{
			name: t.identifier.name
			span: t.identifier.span
		}
		element_type: convert_optional_type_identifier(t.element_type)
		param_types:  t.param_types.map(fn (pt ast.TypeIdentifier) typed_ast.TypeIdentifier {
			return convert_type_identifier(pt)
		})
		return_type:  convert_optional_type_identifier(t.return_type)
		error_type:   convert_optional_type_identifier(t.error_type)
		span:         t.span
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
		span: id.span
	}
}

fn convert_span(s Span) Span {
	return s
}

fn (c TypeChecker) def_loc_from_span(name string, s Span) DefinitionLocation {
	return DefinitionLocation{
		line:    s.start_line
		column:  s.start_column
		end_col: s.end_column
	}
}

fn (mut c TypeChecker) check_binding_type(name string, name_span Span, annotation ?ast.TypeIdentifier, typed_init typed_ast.Expression, init_type Type, context string) Type {
	loc := c.def_loc_from_span(name, name_span)
	if annot := annotation {
		if expected := c.resolve_type_identifier(annot) {
			init_span := typed_init.span
			c.expect_type(init_type, expected, init_span, context)
			c.env.define_at(name, expected, loc)
			return expected
		} else {
			c.error_at_span("Unknown type '${annot.identifier.name}'", annot.identifier.span)
			c.env.define_at(name, init_type, loc)
			return init_type
		}
	} else {
		c.env.define_at(name, init_type, loc)
		return init_type
	}
}

fn (mut c TypeChecker) check_variable_binding(expr ast.VariableBinding) (typed_ast.Node, Type) {
	if expr.init is ast.FunctionExpression {
		func_expr := expr.init as ast.FunctionExpression

		mut param_types := []Type{}
		mut type_var_index := 0
		for param in func_expr.params {
			if pt := param.typ {
				if resolved := c.resolve_type_identifier(pt) {
					param_types << resolved
				} else {
					param_types << t_none()
				}
			} else {
				param_types << t_var(type_var_name_from_index(type_var_index))
				type_var_index++
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

	// if we have an annotation and init is empty/not inferrable, we should use the annotation
	mut typed_init := typed_ast.Expression(typed_ast.NoneExpression{
		span: convert_span(expr.span)
	})
	mut init_type := t_none()

	if annotated := expr.typ {
		if expected_type := c.resolve_type_identifier(annotated) {
			if expr.init is ast.ArrayExpression {
				arr := expr.init as ast.ArrayExpression
				if arr.elements.len == 0 {
					// empy array with type annotation so we use the annotated type
					typed_init = typed_ast.ArrayExpression{
						elements: []
						span:     convert_span(arr.span)
					}
					init_type = expected_type
				} else {
					typed_init, init_type = c.check_expr(expr.init)
				}
			} else {
				typed_init, init_type = c.check_expr(expr.init)
			}
		} else {
			typed_init, init_type = c.check_expr(expr.init)
		}
	} else {
		typed_init, init_type = c.check_expr(expr.init)
	}

	final_type := c.check_binding_type(expr.identifier.name, expr.identifier.span, expr.typ,
		typed_init, init_type, 'in variable binding')

	if doc := expr.doc {
		c.env.store_doc(expr.identifier.name, doc)
	}
	c.record_type(expr.identifier.name, final_type, expr.identifier.span, expr.doc)

	stmt := typed_ast.Statement(typed_ast.VariableBinding{
		identifier: convert_identifier(expr.identifier)
		typ:        convert_optional_type_id(expr.typ)
		init:       typed_init
		span:       convert_span(expr.span)
	})

	return typed_ast.Node(stmt), t_none()
}

fn (mut c TypeChecker) check_const_binding(expr ast.ConstBinding) (typed_ast.Node, Type) {
	if c.in_function {
		c.error_at_span("'const' declarations are only allowed at the top level, not inside functions",
			expr.span)
	}

	typed_init, init_type := c.check_expr(expr.init)
	final_type := c.check_binding_type(expr.identifier.name, expr.identifier.span, expr.typ,
		typed_init, init_type, 'in const binding')

	if doc := expr.doc {
		c.env.store_doc(expr.identifier.name, doc)
	}
	c.record_type(expr.identifier.name, final_type, expr.identifier.span, expr.doc)

	stmt := typed_ast.Statement(typed_ast.ConstBinding{
		identifier: convert_identifier(expr.identifier)
		typ:        convert_optional_type_id(expr.typ)
		init:       typed_init
		span:       convert_span(expr.span)
	})

	return typed_ast.Node(stmt), t_none()
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
				// handle TypeVar + String -> infer TypeVar as String
				if left_type is TypeVar {
					c.unify(left_type, t_string(), mut c.param_subs)
					t_string()
				} else if right_type is TypeVar {
					c.unify(right_type, t_string(), mut c.param_subs)
					t_string()
				} else {
					c.error_at_span("Cannot concatenate '${type_to_string(left_type)}' with '${type_to_string(right_type)}': use string interpolation instead",
						expr.span)
					t_string()
				}
			} else if !is_numeric(left_type) {
				c.error_at_span("Left operand of '${op_str}' must be numeric, got '${type_to_string(left_type)}'",
					expr.span)
				t_int()
			} else if !is_numeric(right_type) {
				c.error_at_span("Right operand of '${op_str}' must be numeric, got '${type_to_string(right_type)}'",
					expr.span)
				t_int()
			} else {
				// both are numeric (or TypeVars) - unify TypeVars with concrete types
				c.unify_binary_operands(left_type, right_type)
				if !c.check_binary_operand_types(left_type, right_type) {
					c.error_at_span("Cannot apply '${op_str}' to '${type_to_string(left_type)}' and '${type_to_string(right_type)}': operands must have the same type",
						expr.span)
				}
				c.infer_binary_result_type(left_type, right_type)
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
				// Both are numeric (or TypeVars) - unify TypeVars with concrete types
				c.unify_binary_operands(left_type, right_type)
				if !c.check_binary_operand_types(left_type, right_type) {
					c.error_at_span("Cannot apply '${op_str}' to '${type_to_string(left_type)}' and '${type_to_string(right_type)}': operands must have the same type",
						expr.span)
				}
				c.infer_binary_result_type(left_type, right_type)
			}
		}
		.punc_lt, .punc_gt, .punc_lte, .punc_gte {
			if !is_numeric(left_type) || !is_numeric(right_type) {
				c.error_at_span("Cannot compare '${type_to_string(left_type)}' with '${type_to_string(right_type)}': operator '${op_str}' requires numeric operands",
					expr.span)
			} else {
				// unify TypeVars with numeric types
				c.unify_binary_operands(left_type, right_type)
			}
			t_bool()
		}
		.punc_equals_comparator, .punc_not_equal {
			// unify TypeVars for equality comparison
			c.unify_binary_operands(left_type, right_type)
			if !c.check_binary_operand_types(left_type, right_type) {
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

fn (mut c TypeChecker) unify_binary_operands(left Type, right Type) {
	if left is TypeVar && right !is TypeVar {
		c.unify(left, right, mut c.param_subs)
	} else if right is TypeVar && left !is TypeVar {
		c.unify(right, left, mut c.param_subs)
	} else if left is TypeVar && right is TypeVar {
		// both are TypeVars - unify them with each other
		c.unify(left, right, mut c.param_subs)
	}
}

// Check if two types are compatible for binary operations, considering TypeVars
fn (c TypeChecker) check_binary_operand_types(left Type, right Type) bool {
	// if either is a TypeVar, they're compatible (TypeVar will be resolved later)
	if left is TypeVar || right is TypeVar {
		return true
	}
	return types_equal(left, right)
}

fn (c TypeChecker) infer_binary_result_type(left Type, right Type) Type {
	// Prefer concrete type over TypeVar
	if left is TypeVar {
		return right
	}
	return left
}

fn (mut c TypeChecker) check_unary(expr ast.UnaryExpression) (typed_ast.Expression, Type) {
	typed_inner, operand_type := c.check_expr(expr.expression)
	expr_span := typed_inner.span
	op_str := expr.op.kind.str()

	result_type := match expr.op.kind {
		.punc_minus {
			if !is_numeric(operand_type) {
				c.error_at_span("Operator '${op_str}' requires a numeric operand, got '${type_to_string(operand_type)}'",
					expr_span)
			}
			operand_type
		}
		.punc_exclamation_mark {
			c.expect_type(operand_type, t_bool(), expr_span, "for operator '${op_str}'")
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
		span:       expr.span
	}, result_type
}

fn (mut c TypeChecker) check_function_declaration(expr ast.FunctionDeclaration) (typed_ast.Node, Type) {
	if c.in_function {
		c.error_at_span('Named function declarations are only allowed at the top level. Use an anonymous function instead: callback = fn() { ... }',
			expr.identifier.span)
	}

	mut param_types := []Type{}
	mut seen_params := map[string]bool{}

	mut declared_ret_type := ?Type(none)
	if rt := expr.return_type {
		if resolved := c.resolve_type_identifier(rt) {
			declared_ret_type = resolved
		} else {
			c.error_at_span("Unknown return type '${rt.identifier.name}'", rt.identifier.span)
		}
	}

	mut declared_err_type := ?Type(none)
	if et := expr.error_type {
		if resolved := c.resolve_type_identifier(et) {
			declared_err_type = resolved
		} else {
			c.error_at_span("Unknown error type '${et.identifier.name}'", et.identifier.span)
		}
	}

	mut type_var_index := 0
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
			param_types << t_var(type_var_name_from_index(type_var_index))
			type_var_index++
		}
	}

	preliminary_ret := declared_ret_type or { t_none() }
	func_type := TypeFunction{
		params:     param_types
		ret:        preliminary_ret
		error_type: declared_err_type
	}

	loc := c.def_loc_from_span(expr.identifier.name, expr.identifier.span)
	c.env.register_function_at(expr.identifier.name, func_type, loc)
	c.env.define_at(expr.identifier.name, func_type, loc)

	c.env.push_scope()
	for i, param in expr.params {
		param_loc := c.def_loc_from_span(param.identifier.name, param.identifier.span)
		c.env.define_at(param.identifier.name, param_types[i], param_loc)
	}

	prev_in_function := c.in_function
	prev_fn_return_type := c.current_fn_return_type
	prev_param_subs := c.param_subs.clone()
	c.in_function = true
	c.current_fn_return_type = if drt := declared_ret_type {
		if et := declared_err_type {
			Type(TypeResult{
				success: drt
				error:   et
			})
		} else {
			drt
		}
	} else {
		none
	}

	c.param_subs = map[string]Type{}
	errors_before := c.diagnostics.len
	typed_body, body_type := c.check_expr(expr.body)

	for i, pt in param_types {
		param_types[i] = substitute(pt, c.param_subs)
	}

	for i, param in expr.params {
		c.record_type(param.identifier.name, param_types[i], param.identifier.span, none)
	}

	mut final_ret_type := if drt := declared_ret_type {
		drt
	} else {
		substitute(body_type, c.param_subs)
	}

	final_err_type := declared_err_type

	c.in_function = prev_in_function
	c.current_fn_return_type = prev_fn_return_type
	c.param_subs = prev_param_subs.clone()
	c.env.pop_scope()

	if declared_ret_type != none && c.diagnostics.len == errors_before {
		body_span := typed_body.span
		expected_ret := if et := declared_err_type {
			Type(TypeResult{
				success: final_ret_type
				error:   et
			})
		} else {
			final_ret_type
		}
		c.expect_type(body_type, expected_ret, body_span, 'in function return')
	}

	final_func_type := TypeFunction{
		params:     param_types
		ret:        final_ret_type
		error_type: final_err_type
	}

	c.env.register_function_at(expr.identifier.name, final_func_type, loc)
	c.env.define_at(expr.identifier.name, final_func_type, loc)
	if doc := expr.doc {
		c.env.store_doc(expr.identifier.name, doc)
	}
	c.record_type(expr.identifier.name, final_func_type, expr.identifier.span, expr.doc)

	mut typed_params := []typed_ast.FunctionParameter{}
	for p in expr.params {
		typed_params << typed_ast.FunctionParameter{
			identifier: convert_identifier(p.identifier)
			typ:        convert_optional_type_id(p.typ)
		}
	}

	stmt := typed_ast.Statement(typed_ast.FunctionDeclaration{
		identifier:  convert_identifier(expr.identifier)
		return_type: convert_optional_type_id(expr.return_type)
		error_type:  convert_optional_type_id(expr.error_type)
		params:      typed_params
		body:        typed_body
		span:        convert_span(expr.span)
	})

	return typed_ast.Node(stmt), t_none()
}

fn (mut c TypeChecker) check_function_expression(expr ast.FunctionExpression) (typed_ast.Expression, Type) {
	mut param_types := []Type{}
	mut seen_params := map[string]bool{}

	mut declared_ret_type := ?Type(none)
	if rt := expr.return_type {
		if resolved := c.resolve_type_identifier(rt) {
			declared_ret_type = resolved
		} else {
			c.error_at_span("Unknown return type '${rt.identifier.name}'", rt.identifier.span)
		}
	}

	mut declared_err_type := ?Type(none)
	if et := expr.error_type {
		if resolved := c.resolve_type_identifier(et) {
			declared_err_type = resolved
		} else {
			c.error_at_span("Unknown error type '${et.identifier.name}'", et.identifier.span)
		}
	}

	mut type_var_index := 0
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
			param_types << t_var(type_var_name_from_index(type_var_index))
			type_var_index++
		}
	}

	c.env.push_scope()
	for i, param in expr.params {
		loc := c.def_loc_from_span(param.identifier.name, param.identifier.span)
		c.env.define_at(param.identifier.name, param_types[i], loc)
	}

	prev_in_function := c.in_function
	prev_fn_return_type := c.current_fn_return_type
	prev_param_subs := c.param_subs.clone()
	c.in_function = true
	c.current_fn_return_type = if drt := declared_ret_type {
		if et := declared_err_type {
			Type(TypeResult{
				success: drt
				error:   et
			})
		} else {
			drt
		}
	} else {
		none
	}

	c.param_subs = map[string]Type{}
	errors_before := c.diagnostics.len
	typed_body, body_type := c.check_expr(expr.body)

	for i, pt in param_types {
		param_types[i] = substitute(pt, c.param_subs)
	}

	for i, param in expr.params {
		c.record_type(param.identifier.name, param_types[i], param.identifier.span, none)
	}

	mut final_ret_type := if drt := declared_ret_type {
		drt
	} else {
		substitute(body_type, c.param_subs)
	}

	final_err_type := declared_err_type

	c.in_function = prev_in_function
	c.current_fn_return_type = prev_fn_return_type
	c.param_subs = prev_param_subs.clone()
	c.env.pop_scope()

	if declared_ret_type != none && c.diagnostics.len == errors_before {
		body_span := typed_body.span
		expected_ret := if et := declared_err_type {
			Type(TypeResult{
				success: final_ret_type
				error:   et
			})
		} else {
			final_ret_type
		}
		c.expect_type(body_type, expected_ret, body_span, 'in function return')
	}

	final_func_type := TypeFunction{
		params:     param_types
		ret:        final_ret_type
		error_type: final_err_type
	}

	mut typed_params := []typed_ast.FunctionParameter{}
	for p in expr.params {
		typed_params << typed_ast.FunctionParameter{
			identifier: convert_identifier(p.identifier)
			typ:        convert_optional_type_id(p.typ)
		}
	}

	return typed_ast.FunctionExpression{
		return_type: convert_optional_type_id(expr.return_type)
		error_type:  convert_optional_type_id(expr.error_type)
		params:      typed_params
		body:        typed_body
		span:        convert_span(expr.span)
	}, final_func_type
}

fn (mut c TypeChecker) check_type_pattern_binding(expr ast.TypePatternBinding) (typed_ast.Node, Type) {
	typed_init, init_type := c.check_expr(expr.init)

	if expected := c.resolve_type_identifier(expr.typ) {
		init_span := typed_init.span
		c.expect_type(init_type, expected, init_span, 'in type pattern')
	} else {
		c.error_at_span("Unknown type '${expr.typ.identifier.name}'", expr.typ.identifier.span)
	}

	stmt := typed_ast.Statement(typed_ast.TypePatternBinding{
		typ:  convert_type_identifier(expr.typ)
		init: typed_init
		span: convert_span(expr.span)
	})
	return typed_ast.Node(stmt), t_none()
}

fn (mut c TypeChecker) check_tuple_destructuring(expr ast.TupleDestructuringBinding) (typed_ast.Node, Type) {
	typed_init, init_type := c.check_expr(expr.init)

	if init_type !is TypeTuple {
		c.error_at_span('Tuple destructuring requires a tuple type, got ${type_to_string(init_type)}',
			expr.span)
		mut typed_patterns := []typed_ast.Expression{}
		for pattern in expr.patterns {
			typed_p, _ := c.check_expr(pattern)
			typed_patterns << typed_p
		}
		stmt := typed_ast.Statement(typed_ast.TupleDestructuringBinding{
			patterns: typed_patterns
			init:     typed_init
			span:     convert_span(expr.span)
		})
		return typed_ast.Node(stmt), t_none()
	}

	tuple_type := init_type as TypeTuple

	if expr.patterns.len != tuple_type.elements.len {
		c.error_at_span('Tuple destructuring pattern has ${expr.patterns.len} elements, but tuple has ${tuple_type.elements.len}',
			expr.span)
	}

	mut typed_patterns := []typed_ast.Expression{}
	for i, pattern in expr.patterns {
		elem_type := if i < tuple_type.elements.len {
			tuple_type.elements[i]
		} else {
			t_none()
		}

		if pattern is ast.Identifier {
			loc := c.def_loc_from_span(pattern.name, pattern.span)
			c.env.define_at(pattern.name, elem_type, loc)
			c.record_type(pattern.name, elem_type, pattern.span, none)
			typed_patterns << typed_ast.Identifier{
				name: pattern.name
				span: convert_span(pattern.span)
			}
		} else if pattern is ast.TypeIdentifier {
			if expected := c.resolve_type_identifier(pattern) {
				if !types_equal(elem_type, expected) {
					c.error_at_span("Type mismatch in destructuring: expected '${type_to_string(expected)}', got '${type_to_string(elem_type)}'",
						pattern.span)
				}
			} else {
				c.error_at_span("Unknown type '${pattern.identifier.name}'", pattern.identifier.span)
			}
			typed_patterns << convert_type_identifier(pattern)
		} else {
			typed_p, _ := c.check_expr(pattern)
			typed_patterns << typed_p
		}
	}

	stmt := typed_ast.Statement(typed_ast.TupleDestructuringBinding{
		patterns: typed_patterns
		init:     typed_init
		span:     convert_span(expr.span)
	})
	return typed_ast.Node(stmt), t_none()
}

fn (mut c TypeChecker) check_call(expr ast.FunctionCallExpression) (typed_ast.Expression, Type) {
	doc := c.env.lookup_doc(expr.identifier.name)
	if func_type := c.env.lookup_function(expr.identifier.name) {
		c.record_type(expr.identifier.name, func_type, expr.identifier.span, doc)
		return c.check_call_with_type(expr, func_type)
	}

	if var_type := c.env.lookup(expr.identifier.name) {
		if var_type is TypeFunction {
			c.record_type(expr.identifier.name, var_type, expr.identifier.span, doc)
			return c.check_call_with_type(expr, var_type)
		}
		if var_type is TypeVar {
			// TypeVar is being called - infer it as a function type
			mut param_types := []Type{}
			mut typed_args := []typed_ast.Expression{}
			for arg in expr.arguments {
				typed_arg, arg_type := c.check_expr(arg)
				typed_args << typed_arg
				param_types << arg_type
			}

			ret_type := t_var(type_var_name_from_index(expr.arguments.len))
			inferred_func_type := TypeFunction{
				params: param_types
				ret:    ret_type
			}

			c.unify(var_type, inferred_func_type, mut c.param_subs)
			c.record_type(expr.identifier.name, inferred_func_type, expr.identifier.span,
				doc)
			return typed_ast.FunctionCallExpression{
				identifier: convert_identifier(expr.identifier)
				arguments:  typed_args
				span:       convert_span(expr.span)
			}, ret_type
		}
	}

	if enum_type := c.env.lookup_enum_by_variant(expr.identifier.name) {
		variant_name := expr.identifier.name
		payload_types := enum_type.variants[variant_name] or { []Type{} }

		mut typed_args := []typed_ast.Expression{}
		if payload_types.len > 0 {
			if expr.arguments.len != payload_types.len {
				c.error_at_span("Enum variant '${variant_name}' expects ${payload_types.len} argument(s), got ${expr.arguments.len}",
					expr.span)
			}
			for i, arg in expr.arguments {
				typed_arg, arg_type := c.check_expr(arg)
				typed_args << typed_arg
				if i < payload_types.len {
					arg_span := typed_arg.span
					c.expect_type(arg_type, payload_types[i], arg_span, "in enum variant '${variant_name}'")
				}
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
		arg_span := typed_arg.span

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

	if actual is TypeVar {
		if existing := subs[actual.name] {
			return types_equal(existing, expected)
		}
		subs[actual.name] = expected
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

	if actual is TypeFunction && expected is TypeFunction {
		if actual.params.len != expected.params.len {
			return false
		}
		for i, actual_param in actual.params {
			if !c.unify(actual_param, expected.params[i], mut subs) {
				return false
			}
		}
		return c.unify(actual.ret, expected.ret, mut subs)
	}

	return types_equal(actual, expected)
}

fn (mut c TypeChecker) check_if(expr ast.IfExpression) (typed_ast.Expression, Type) {
	typed_cond, cond_type := c.check_expr(expr.condition)
	cond_span := typed_cond.span
	c.expect_type(cond_type, t_bool(), cond_span, 'in if condition')

	typed_body, then_type := c.check_expr(expr.body)

	mut typed_else := ?typed_ast.Expression(none)
	mut result_type := then_type

	if else_body := expr.else_body {
		typed_else_body, else_type := c.check_expr(else_body)
		result_type = c.unify_arm_types(then_type, else_type, convert_span(expr.span))
		typed_else = typed_else_body
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
		c.error_at_span("Cannot infer type of empty array. Provide a type annotation, e.g.: 'items []Int = []'",
			expr.span)
		return typed_ast.ArrayExpression{
			elements: []
			span:     convert_span(expr.span)
		}, t_array(t_none())
	}

	mut typed_elements := []typed_ast.Expression{}
	mut first_type := t_none()
	mut first_type_set := false

	for elem in expr.elements {
		if elem is ast.SpreadExpression {
			// Spread expression: ..arr - inner must be an array
			inner := elem.expression or {
				c.error_at_span('Spread in array literal requires an expression', elem.span)
				typed_elements << typed_ast.SpreadExpression{
					expression: none
					span:       convert_span(elem.span)
				}
				continue
			}

			typed_inner, inner_type := c.check_expr(inner)

			element_type := if inner_type is TypeArray {
				inner_type.element
			} else {
				c.error_at_span('Spread operator requires an array, got ${type_to_string(inner_type)}',
					elem.span)
				t_none()
			}

			typed_spread := typed_ast.SpreadExpression{
				expression: typed_inner
				span:       convert_span(elem.span)
			}
			typed_elements << typed_spread

			if !first_type_set {
				first_type = element_type
				first_type_set = true
			} else {
				c.expect_type(element_type, first_type, convert_span(elem.span), 'in spread element')
			}
		} else {
			// Regular element
			typed_elem, elem_type := c.check_expr(elem)
			typed_elements << typed_elem

			if !first_type_set {
				first_type = elem_type
				first_type_set = true
			} else {
				elem_span := typed_elem.span
				c.expect_type(elem_type, first_type, elem_span, 'in array element')
			}
		}
	}

	return typed_ast.ArrayExpression{
		elements: typed_elements
		span:     convert_span(expr.span)
	}, t_array(first_type)
}

fn (mut c TypeChecker) check_tuple(expr ast.TupleExpression) (typed_ast.Expression, Type) {
	mut typed_elements := []typed_ast.Expression{}
	mut element_types := []Type{}

	for elem in expr.elements {
		typed_elem, elem_type := c.check_expr(elem)
		typed_elements << typed_elem
		element_types << elem_type
	}

	return typed_ast.TupleExpression{
		elements: typed_elements
		span:     convert_span(expr.span)
	}, t_tuple(element_types)
}

fn (mut c TypeChecker) check_array_index(expr ast.ArrayIndexExpression) (typed_ast.Expression, Type) {
	typed_arr, arr_type := c.check_expr(expr.expression)

	if expr.index is ast.RangeExpression {
		range_expr := expr.index as ast.RangeExpression
		typed_start, start_type := c.check_expr(range_expr.start)
		typed_end, end_type := c.check_expr(range_expr.end)

		c.expect_type(start_type, t_int(), range_expr.start.span, 'as slice start')
		c.expect_type(end_type, t_int(), range_expr.end.span, 'as slice end')

		if arr_type !is TypeArray {
			c.error_at_span('Cannot slice non-array type ${type_to_string(arr_type)}',
				expr.span)
		}

		return typed_ast.ArrayIndexExpression{
			expression: typed_arr
			index:      typed_ast.RangeExpression{
				start: typed_start
				end:   typed_end
				span:  convert_span(range_expr.span)
			}
			span:       convert_span(expr.span)
		}, arr_type
	}

	typed_idx, idx_type := c.check_expr(expr.index)
	idx_span := typed_idx.span

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
	}, t_option(element_type)
}

fn (mut c TypeChecker) check_struct_decl(stmt ast.StructDeclaration) (typed_ast.Node, Type) {
	if c.in_function {
		c.error_at_span('Struct definitions are only allowed at the top level', stmt.span)
	}

	mut fields := map[string]Type{}

	for field in stmt.fields {
		if field.identifier.name in fields {
			c.error_at_span("Duplicate field '${field.identifier.name}' in struct '${stmt.identifier.name}'",
				field.identifier.span)
			continue
		}
		if resolved := c.resolve_type_identifier(field.typ) {
			fields[field.identifier.name] = resolved

			qualified_name := '${stmt.identifier.name}.${field.identifier.name}'
			loc := c.def_loc_from_span(qualified_name, field.identifier.span)
			c.env.definitions[qualified_name] = loc

			c.record_type(qualified_name, resolved, field.identifier.span, field.doc)

			if doc := field.doc {
				c.env.store_doc(qualified_name, doc)
			}
		} else {
			c.error_at_span("Unknown type '${field.typ.identifier.name}' for field '${field.identifier.name}'",
				field.identifier.span)
		}
	}

	struct_type := TypeStruct{
		name:   stmt.identifier.name
		fields: fields
	}

	loc := c.def_loc_from_span(stmt.identifier.name, stmt.identifier.span)
	registered_struct := c.env.register_struct_at(struct_type, loc)

	if doc := stmt.doc {
		c.env.store_doc(stmt.identifier.name, doc)
	}

	c.record_type(stmt.identifier.name, Type(registered_struct), stmt.identifier.span,
		stmt.doc)

	mut typed_fields := []typed_ast.StructField{}
	for f in stmt.fields {
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

	s := typed_ast.Statement(typed_ast.StructDeclaration{
		identifier: convert_identifier(stmt.identifier)
		fields:     typed_fields
		span:       convert_span(stmt.span)
	})

	return typed_ast.Node(s), t_none()
}

fn (mut c TypeChecker) check_struct_init(expr ast.StructInitExpression) (typed_ast.Expression, Type) {
	struct_type := if struct_def := c.env.lookup_struct(expr.identifier.name) {
		// Record type for struct name hover
		doc := c.env.lookup_doc(expr.identifier.name)
		c.record_type(expr.identifier.name, Type(struct_def), expr.identifier.span, doc)
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
			init_span := typed_init.span
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
		span:       convert_span(expr.span)
	}, struct_type
}

fn (mut c TypeChecker) check_enum_decl(stmt ast.EnumDeclaration) (typed_ast.Node, Type) {
	if c.in_function {
		c.error_at_span('Enum definitions are only allowed at the top level', stmt.span)
	}

	mut variants := map[string][]Type{}

	for variant in stmt.variants {
		if variant.identifier.name in variants {
			c.error_at_span("Duplicate variant '${variant.identifier.name}' in enum '${stmt.identifier.name}'",
				variant.identifier.span)
			continue
		}

		mut payload_types := []Type{}
		for payload in variant.payload {
			if resolved := c.resolve_type_identifier(payload) {
				payload_types << resolved
			} else {
				c.error_at_span("Unknown type '${payload.identifier.name}' in variant '${variant.identifier.name}'",
					variant.identifier.span)
			}
		}

		if doc := variant.doc {
			c.env.store_doc('${stmt.identifier.name}.${variant.identifier.name}', doc)
		}

		variants[variant.identifier.name] = payload_types
	}

	enum_type := TypeEnum{
		name:     stmt.identifier.name
		variants: variants
	}

	loc := c.def_loc_from_span(stmt.identifier.name, stmt.identifier.span)
	registered_enum := c.env.register_enum_at(enum_type, loc)

	if doc := stmt.doc {
		c.env.store_doc(stmt.identifier.name, doc)
	}

	c.record_type(stmt.identifier.name, Type(registered_enum), stmt.identifier.span, stmt.doc)

	for variant in stmt.variants {
		qualified_name := '${stmt.identifier.name}.${variant.identifier.name}'
		variant_loc := c.def_loc_from_span(qualified_name, variant.identifier.span)
		c.env.definitions[qualified_name] = variant_loc

		c.record_type(qualified_name, Type(registered_enum), variant.identifier.span,
			variant.doc)
	}

	typed_variants := stmt.variants.map(fn (v ast.EnumVariant) typed_ast.EnumVariant {
		return typed_ast.EnumVariant{
			identifier: convert_identifier(v.identifier)
			payload:    v.payload.map(convert_type_identifier)
		}
	})

	s := typed_ast.Statement(typed_ast.EnumDeclaration{
		identifier: convert_identifier(stmt.identifier)
		variants:   typed_variants
		span:       convert_span(stmt.span)
	})

	return typed_ast.Node(s), t_none()
}

fn (mut c TypeChecker) check_property_access(expr ast.PropertyAccessExpression) (typed_ast.Expression, Type) {
	// Check for qualified enum access like MyEnum.Variant or MyEnum.Variant(payload)
	if expr.left is ast.Identifier {
		left_id := expr.left as ast.Identifier
		if looked_up := c.env.lookup_type(left_id.name) {
			if looked_up is TypeEnum {
				enum_type := looked_up

				enum_doc := c.env.lookup_doc(left_id.name)
				c.record_type(left_id.name, Type(enum_type), left_id.span, enum_doc)

				typed_left := typed_ast.Identifier{
					name: left_id.name
					span: convert_span(left_id.span)
				}

				variant_name, args, variant_span := if expr.right is ast.FunctionCallExpression {
					call := expr.right as ast.FunctionCallExpression
					call.identifier.name, call.arguments, call.span
				} else if expr.right is ast.Identifier {
					r := expr.right as ast.Identifier
					r.name, []ast.Expression{}, r.span
				} else {
					return c.check_expr(expr.left)
				}

				if variant_name !in enum_type.variants {
					c.error_at_span("Enum '${left_id.name}' has no variant '${variant_name}'",
						variant_span)
					return typed_ast.PropertyAccessExpression{
						left:  typed_left
						right: typed_ast.ErrorNode{
							message: 'Unknown variant'
							span:    convert_span(variant_span)
						}
						span:  convert_span(expr.span)
					}, t_none()
				}

				variant_doc := c.env.lookup_doc('${left_id.name}.${variant_name}')
				c.record_type('${left_id.name}.${variant_name}', Type(enum_type), variant_span,
					variant_doc)

				payload_types := enum_type.variants[variant_name] or { []Type{} }
				mut typed_args := []typed_ast.Expression{}

				if payload_types.len > 0 {
					if args.len != payload_types.len {
						c.error_at_span("Enum variant '${variant_name}' expects ${payload_types.len} argument(s), got ${args.len}",
							variant_span)
					}
					for i, arg in args {
						typed_arg, arg_type := c.check_expr(arg)
						typed_args << typed_arg
						if i < payload_types.len {
							c.expect_type(arg_type, payload_types[i], convert_span(variant_span),
								"in enum variant '${variant_name}'")
						}
					}
				} else if args.len > 0 {
					c.error_at_span("Enum variant '${variant_name}' takes no arguments",
						variant_span)
				}

				typed_right := if args.len > 0 || payload_types.len > 0 {
					typed_ast.Expression(typed_ast.FunctionCallExpression{
						identifier: typed_ast.Identifier{
							name: variant_name
							span: convert_span(variant_span)
						}
						arguments:  typed_args
						span:       convert_span(variant_span)
					})
				} else {
					typed_ast.Expression(typed_ast.Identifier{
						name: variant_name
						span: convert_span(variant_span)
					})
				}

				return typed_ast.PropertyAccessExpression{
					left:  typed_left
					right: typed_right
					span:  convert_span(expr.span)
				}, Type(enum_type)
			}
		}
	}

	typed_left, left_type := c.check_expr(expr.left)

	if expr.right is ast.FunctionCallExpression {
		typed_right, right_type := c.check_expr(expr.right)
		return typed_ast.PropertyAccessExpression{
			left:  typed_left
			right: typed_right
			span:  convert_span(expr.span)
		}, right_type
	}

	if expr.right is ast.NumberLiteral {
		num_lit := expr.right as ast.NumberLiteral
		typed_right := typed_ast.NumberLiteral{
			value: num_lit.value
			span:  convert_span(num_lit.span)
		}

		if left_type is TypeTuple {
			index := num_lit.value.int()
			if index < 0 || index >= left_type.elements.len {
				c.error_at_span('Tuple index ${index} out of bounds. Tuple has ${left_type.elements.len} elements.',
					num_lit.span)
				return typed_ast.PropertyAccessExpression{
					left:  typed_left
					right: typed_right
					span:  convert_span(expr.span)
				}, t_none()
			}
			return typed_ast.PropertyAccessExpression{
				left:  typed_left
				right: typed_right
				span:  convert_span(expr.span)
			}, left_type.elements[index]
		} else {
			c.error_at_span('Cannot use numeric index on type ${type_to_string(left_type)}. Only tuples support .0 .1 etc.',
				num_lit.span)
			return typed_ast.PropertyAccessExpression{
				left:  typed_left
				right: typed_right
				span:  convert_span(expr.span)
			}, t_none()
		}
	}

	if expr.right !is ast.Identifier {
		err_span := expr.right.span
		c.error_at_span('Expected identifier in property access', err_span)
		return typed_ast.PropertyAccessExpression{
			left:  typed_left
			right: typed_ast.ErrorNode{
				message: 'Expected identifier'
				span:    convert_span(err_span)
			}
			span:  convert_span(expr.span)
		}, t_none()
	}

	right := expr.right as ast.Identifier

	typed_right := typed_ast.Identifier{
		name: right.name
		span: convert_span(right.span)
	}

	result_type := if left_type is TypeStruct {
		if field_type := left_type.fields[right.name] {
			qualified_name := '${left_type.name}.${right.name}'
			field_doc := c.env.lookup_doc(qualified_name)
			c.record_type(qualified_name, field_type, right.span, field_doc)
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
		span:  convert_span(expr.span)
	}, result_type
}

fn (mut c TypeChecker) check_match(expr ast.MatchExpression) (typed_ast.Expression, Type) {
	typed_subject, subject_type := c.check_expr(expr.subject)

	if expr.arms.len == 0 {
		return typed_ast.MatchExpression{
			subject: typed_subject
			arms:    []
			span:    convert_span(expr.span)
		}, t_none()
	}

	mut first_type := t_none()
	mut typed_arms := []typed_ast.MatchArm{}
	mut pats := []Pat{}

	for i, arm in expr.arms {
		c.env.push_scope()

		typed_pattern, _ := c.check_pattern(arm.pattern, subject_type)

		pat := ast_pattern_to_pat(typed_pattern, subject_type)
		if !check_pattern_useful(pats, pat, subject_type) {
			if arm.pattern is ast.WildcardPattern {
				c.warning_at_span('Previous arms already match all cases, else branch is unreachable',
					arm.pattern.span)
			} else {
				c.warning_at_span('Unreachable pattern', arm.pattern.span)
			}
		}
		pats << pat

		typed_body, arm_type := c.check_expr(arm.body)
		c.env.pop_scope()

		typed_arms << typed_ast.MatchArm{
			pattern: typed_pattern
			body:    typed_body
		}

		if i == 0 {
			first_type = arm_type
		} else {
			first_type = c.unify_arm_types(first_type, arm_type, typed_body.span)
		}
	}

	if missing := check_exhaustiveness(pats, subject_type) {
		subject_span := typed_subject.span
		c.error_at_span('Match is not exhaustive, missing: ${missing}', subject_span)
	}

	return typed_ast.MatchExpression{
		subject: typed_subject
		arms:    typed_arms
		span:    convert_span(expr.span)
	}, first_type
}

fn (mut c TypeChecker) check_pattern(pattern ast.Expression, subject_type Type) (typed_ast.Expression, Type) {
	if pattern is ast.OrPattern {
		mut typed_patterns := []typed_ast.Expression{}
		for p in pattern.patterns {
			typed_p, _ := c.check_pattern(p, subject_type)
			typed_patterns << typed_p
		}
		return typed_ast.OrPattern{
			patterns: typed_patterns
			span:     convert_span(pattern.span)
		}, subject_type
	}

	// Handle qualified enum patterns like MyEnum.Variant or MyEnum.Variant(binding)
	if pattern is ast.PropertyAccessExpression {
		if pattern.left is ast.Identifier {
			left_id := pattern.left as ast.Identifier
			if looked_up := c.env.lookup_type(left_id.name) {
				if looked_up is TypeEnum {
					enum_type := looked_up
					typed_left := typed_ast.Identifier{
						name: left_id.name
						span: convert_span(left_id.span)
					}

					variant_name, args, pattern_span := if pattern.right is ast.FunctionCallExpression {
						call := pattern.right as ast.FunctionCallExpression
						call.identifier.name, call.arguments, call.span
					} else if pattern.right is ast.Identifier {
						r := pattern.right as ast.Identifier
						r.name, []ast.Expression{}, r.span
					} else {
						// Not a valid pattern form, fall through to normal check_expr
						return c.check_expr(pattern)
					}

					if variant_name in enum_type.variants {
						payload_types := enum_type.variants[variant_name] or { []Type{} }

						// Bind pattern variables to their corresponding payload types
						for i, arg in args {
							if arg is ast.Identifier && i < payload_types.len {
								c.env.define(arg.name, payload_types[i])
								c.record_type(arg.name, payload_types[i], arg.span, none)
							}
						}

						mut typed_args := []typed_ast.Expression{}
						for arg in args {
							typed_arg, _ := c.check_expr(arg)
							typed_args << typed_arg
						}

						typed_right := if args.len > 0 || payload_types.len > 0 {
							typed_ast.Expression(typed_ast.FunctionCallExpression{
								identifier: typed_ast.Identifier{
									name: variant_name
									span: convert_span(pattern_span)
								}
								arguments:  typed_args
								span:       convert_span(pattern_span)
							})
						} else {
							typed_ast.Expression(typed_ast.Identifier{
								name: variant_name
								span: convert_span(pattern_span)
							})
						}

						return typed_ast.PropertyAccessExpression{
							left:  typed_left
							right: typed_right
							span:  convert_span(pattern.span)
						}, subject_type
					}
				}
			}
		}
	}

	if pattern is ast.ArrayExpression {
		element_type := if subject_type is TypeArray {
			subject_type.element
		} else {
			c.error_at_span('Cannot match array pattern against non-array type ${type_to_string(subject_type)}',
				pattern.span)
			t_none()
		}

		// spread can only be at the end
		for i, elem in pattern.elements {
			if elem is ast.SpreadExpression && i != pattern.elements.len - 1 {
				c.error_at_span('Spread pattern must be at the end of the array pattern',
					elem.span)
			}
		}

		mut typed_elements := []typed_ast.Expression{}

		for elem in pattern.elements {
			if elem is ast.SpreadExpression {
				// Spread pattern: ..rest or just ..
				if inner := elem.expression {
					if inner is ast.Identifier {
						// Named spread: bind to array type
						c.env.define(inner.name, subject_type)
						c.record_type(inner.name, subject_type, inner.span, none)
						typed_elements << typed_ast.SpreadExpression{
							expression: typed_ast.Identifier{
								name: inner.name
								span: convert_span(inner.span)
							}
							span:       convert_span(elem.span)
						}
					} else {
						// Other expression (shouldn't happen in patterns)
						typed_inner, _ := c.check_expr(inner)
						typed_elements << typed_ast.SpreadExpression{
							expression: typed_inner
							span:       convert_span(elem.span)
						}
					}
				} else {
					// Anonymous spread (..): just match, don't bind
					typed_elements << typed_ast.SpreadExpression{
						expression: none
						span:       convert_span(elem.span)
					}
				}
			} else if elem is ast.Identifier {
				// Named binding: bind to element type
				c.env.define(elem.name, element_type)
				c.record_type(elem.name, element_type, elem.span, none)
				typed_elements << typed_ast.Identifier{
					name: elem.name
					span: convert_span(elem.span)
				}
			} else {
				// Other patterns (literals, nested patterns)
				typed_elem, _ := c.check_pattern(elem, element_type)
				typed_elements << typed_elem
			}
		}

		return typed_ast.ArrayExpression{
			elements: typed_elements
			span:     convert_span(pattern.span)
		}, subject_type
	}

	if pattern is ast.TupleExpression {
		tuple_type := if subject_type is TypeTuple {
			subject_type
		} else {
			c.error_at_span('Cannot match tuple pattern against non-tuple type ${type_to_string(subject_type)}',
				pattern.span)
			TypeTuple{
				elements: []
			}
		}

		if pattern.elements.len != tuple_type.elements.len && tuple_type.elements.len > 0 {
			c.error_at_span('Tuple pattern has ${pattern.elements.len} elements, but tuple has ${tuple_type.elements.len}',
				pattern.span)
		}

		mut typed_elements := []typed_ast.Expression{}

		for i, elem in pattern.elements {
			elem_type := if i < tuple_type.elements.len {
				tuple_type.elements[i]
			} else {
				t_none()
			}

			if elem is ast.Identifier {
				c.env.define(elem.name, elem_type)
				c.record_type(elem.name, elem_type, elem.span, none)
				typed_elements << typed_ast.Identifier{
					name: elem.name
					span: convert_span(elem.span)
				}
			} else {
				typed_elem, _ := c.check_pattern(elem, elem_type)
				typed_elements << typed_elem
			}
		}

		return typed_ast.TupleExpression{
			elements: typed_elements
			span:     convert_span(pattern.span)
		}, subject_type
	}

	if pattern is ast.FunctionCallExpression {
		variant_name := pattern.identifier.name

		if subject_type is TypeEnum {
			payload_types := subject_type.variants[variant_name] or { []Type{} }
			for i, arg in pattern.arguments {
				if arg is ast.Identifier && i < payload_types.len {
					c.env.define(arg.name, payload_types[i])
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

	if pattern is ast.RangeExpression {
		typed_start, start_type := c.check_expr(pattern.start)
		typed_end, end_type := c.check_expr(pattern.end)

		if !types_equal(start_type, t_int()) {
			c.error_at_span('Range pattern start must be Int, got ${type_to_string(start_type)}',
				pattern.start.span)
		}
		if !types_equal(end_type, t_int()) {
			c.error_at_span('Range pattern end must be Int, got ${type_to_string(end_type)}',
				pattern.end.span)
		}
		if !types_equal(subject_type, t_int()) {
			c.error_at_span('Range pattern can only match Int, got ${type_to_string(subject_type)}',
				pattern.span)
		}

		return typed_ast.RangeExpression{
			start: typed_start
			end:   typed_end
			span:  convert_span(pattern.span)
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
	} else {
		c.error_at_token("'or' can only be used on Result or Option types, got '${type_to_string(inner_type)}'",
			expr.span, 2)
	}

	if receiver := expr.receiver {
		c.env.push_scope()
		c.env.define(receiver.name, error_type)
	}

	typed_body, body_type := c.check_expr(expr.body)
	body_span := typed_body.span

	c.expect_type(body_type, success_type, body_span, "in 'or' fallback")

	if expr.receiver != none {
		c.env.pop_scope()
	}

	return typed_ast.OrExpression{
		expression:    typed_inner
		receiver:      convert_optional_identifier(expr.receiver)
		body:          typed_body
		resolved_type: inner_type
		span:          convert_span(expr.span)
	}, success_type
}

fn (mut c TypeChecker) check_range(expr ast.RangeExpression) (typed_ast.Expression, Type) {
	typed_start, start_type := c.check_expr(expr.start)
	typed_end, end_type := c.check_expr(expr.end)

	if !types_equal(start_type, t_int()) {
		start_span := typed_start.span
		c.error_at_span('Range start must be Int, got ${type_to_string(start_type)}',
			start_span)
	}

	if !types_equal(end_type, t_int()) {
		end_span := typed_end.span
		c.error_at_span('Range end must be Int, got ${type_to_string(end_type)}', end_span)
	}

	return typed_ast.RangeExpression{
		start: typed_start
		end:   typed_end
		span:  convert_span(expr.span)
	}, t_array(t_int())
}
