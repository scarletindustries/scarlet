module types

import ast
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
	program_type   Type
	type_positions []TypePosition
}

pub fn check(program ast.BlockExpression) CheckResult {
	mut checker := TypeChecker{
		env:         new_env()
		diagnostics: []diagnostic.Diagnostic{}
	}

	checker.register_builtins()

	program_type := checker.check_block(program, none)

	return CheckResult{
		diagnostics:    checker.diagnostics
		success:        !diagnostic.has_errors(checker.diagnostics)
		env:            checker.env
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

fn (mut c TypeChecker) record_type_annotation(annot ast.TypeIdentifier, typ Type) {
	type_name := annot.identifier.name
	doc := c.env.lookup_doc(type_name)
	c.record_type(type_name, typ, annot.identifier.span, doc)

	for ta in annot.type_args {
		if resolved := c.resolve_type_identifier(ta) {
			c.record_type_annotation(ta, resolved)
		}
	}
}

fn type_var_name_from_index(id int) string {
	mut result := ''
	mut n := id
	for {
		result = [u8(`A` + n % 26)].bytestr() + result
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

	// __stack__depth__ only works with --expose-debug-builtins
	c.env.register_function('__stack_depth__', TypeFunction{
		params: []
		ret:    t_int()
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

fn (mut c TypeChecker) infer_type_args(expected Type, actual Type, mut subs map[string]Type, s Span) {
	match expected {
		TypeVar {
			if existing := subs[expected.name] {
				if !types_equal(existing, actual) {
					c.error_at_span("Conflicting types for type parameter '${expected.name}': expected '${type_to_string(existing)}', got '${type_to_string(actual)}'",
						s)
				}
			} else {
				subs[expected.name] = actual
			}
		}
		TypeArray {
			if actual is TypeArray {
				c.infer_type_args(expected.element, actual.element, mut subs, s)
			}
		}
		TypeOption {
			if actual is TypeOption {
				c.infer_type_args(expected.inner, actual.inner, mut subs, s)
			}
		}
		TypeTuple {
			if actual is TypeTuple {
				if expected.elements.len == actual.elements.len {
					for i, exp_elem in expected.elements {
						c.infer_type_args(exp_elem, actual.elements[i], mut subs, s)
					}
				}
			}
		}
		TypeResult {
			if actual is TypeResult {
				c.infer_type_args(expected.success, actual.success, mut subs, s)
				c.infer_type_args(expected.error, actual.error, mut subs, s)
			}
		}
		TypeStruct {
			if actual is TypeStruct {
				for field_name, field_type in expected.fields {
					if actual_field := actual.fields[field_name] {
						c.infer_type_args(field_type, actual_field, mut subs, s)
					}
				}
			}
		}
		else {}
	}
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

	is_type_var := name.len == 1 && name[0] >= `A` && name[0] <= `Z`

	mut base_type := if is_type_var {
		t_var(name)
	} else {
		c.env.lookup_type(name) or { return none }
	}

	if t.type_args.len > 0 {
		mut resolved_args := []Type{}
		for arg in t.type_args {
			resolved_arg := c.resolve_type_identifier(arg) or { return none }
			resolved_args << resolved_arg
		}

		base_type = instantiate_generic_type(base_type, resolved_args) or { return none }
	}

	if t.is_option {
		base_type = t_option(base_type)
	}

	return base_type
}

fn (mut c TypeChecker) check_block(block ast.BlockExpression, expected ?Type) Type {
	mut last_type := t_none()

	for i, node in block.body {
		is_last := i == block.body.len - 1
		typ := if is_last {
			c.check_node_with_hint(node, expected)
		} else {
			c.check_node(node)
		}
		last_type = typ

		// For all expressions except the last one (which is the return value),
		// check that non-None values are consumed (statements always return None)
		if !is_last && node is ast.Expression && !types_equal(typ, t_none()) {
			node_span := ast.node_span(node)
			c.error_at_span("Expression of type '${type_to_string(typ)}' must be consumed. Assign it to a variable or use '${type_to_string(typ)} =' to discard",
				node_span)
		}
	}

	return last_type
}

fn (mut c TypeChecker) check_node(node ast.Node) Type {
	return c.check_node_with_hint(node, none)
}

fn (mut c TypeChecker) check_node_with_hint(node ast.Node, expected ?Type) Type {
	match node {
		ast.Statement {
			return c.check_statement(node)
		}
		ast.Expression {
			return c.check_expr_with_hint(node, expected)
		}
	}
}

fn (mut c TypeChecker) check_statement(stmt ast.Statement) Type {
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
			return t_none()
		}
		ast.ExportDeclaration {
			return c.check_statement(stmt.declaration)
		}
	}
}

fn (mut c TypeChecker) check_expr(expr ast.Expression) Type {
	return c.check_expr_with_hint(expr, none)
}

fn (mut c TypeChecker) check_expr_with_hint(expr ast.Expression, expected ?Type) Type {
	match expr {
		ast.NumberLiteral {
			return if expr.value.contains('.') { t_float() } else { t_int() }
		}
		ast.StringLiteral {
			return t_string()
		}
		ast.InterpolatedString {
			for part in expr.parts {
				c.check_expr(part)
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
			return typ
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
			return c.check_call(expr, expected)
		}
		ast.BlockExpression {
			c.env.push_scope()
			last_type := c.check_block(expr, expected)
			c.env.pop_scope()
			return last_type
		}
		ast.IfExpression {
			return c.check_if(expr, expected)
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
			return c.check_property_access(expr, expected)
		}
		ast.MatchExpression {
			return c.check_match(expr)
		}
		ast.OrExpression {
			return c.check_or(expr)
		}
		ast.ErrorExpression {
			typ := c.check_expr(expr.expression)
			return Type(TypeResult{
				success: t_none()
				error:   typ
			})
		}
		ast.RangeExpression {
			return c.check_range(expr)
		}
		ast.WildcardPattern {
			return t_none()
		}
		ast.OrPattern {
			for pattern in expr.patterns {
				c.check_expr(pattern)
			}
			return t_none()
		}
		ast.ErrorNode {
			return t_none()
		}
		ast.TypeIdentifier {
			return t_none()
		}
	}
}

fn instantiate_generic_type(base_type Type, resolved_args []Type) ?Type {
	match base_type {
		TypeStruct {
			if base_type.type_params.len != resolved_args.len {
				return none // arity mismatch
			}
			mut subs := map[string]Type{}
			for i, param in base_type.type_params {
				subs[param] = resolved_args[i]
			}
			mut new_fields := map[string]Type{}
			for field_name, field_type in base_type.fields {
				new_fields[field_name] = substitute(field_type, subs)
			}
			return TypeStruct{
				id:          base_type.id
				name:        base_type.name
				type_params: base_type.type_params
				type_args:   resolved_args
				fields:      new_fields
			}
		}
		TypeEnum {
			if base_type.type_params.len != resolved_args.len {
				return none // arity mismatch
			}
			mut subs := map[string]Type{}
			for i, param in base_type.type_params {
				subs[param] = resolved_args[i]
			}
			mut new_variants := map[string][]Type{}
			for variant_name, payload_types in base_type.variants {
				mut new_payloads := []Type{}
				for pt in payload_types {
					new_payloads << substitute(pt, subs)
				}
				new_variants[variant_name] = new_payloads
			}
			return TypeEnum{
				id:          base_type.id
				name:        base_type.name
				type_params: base_type.type_params
				type_args:   resolved_args
				variants:    new_variants
			}
		}
		else {
			return none // not a generic type
		}
	}
}

fn (c TypeChecker) def_loc_from_span(name string, s Span) DefinitionLocation {
	return DefinitionLocation{
		line:    s.start_line
		column:  s.start_column
		end_col: s.end_column
	}
}

fn (mut c TypeChecker) check_binding_type(name string, name_span Span, annotation ?ast.TypeIdentifier, init ast.Expression, init_type Type, context string) Type {
	loc := c.def_loc_from_span(name, name_span)
	if annot := annotation {
		if expected := c.resolve_type_identifier(annot) {
			init_span := init.span
			mut subs := map[string]Type{}
			c.unify(init_type, expected, mut subs)
			final_type := substitute(expected, subs)
			c.expect_type(init_type, final_type, init_span, context)
			c.env.define_at(name, final_type, loc)
			c.record_type_annotation(annot, final_type)
			return final_type
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

fn (mut c TypeChecker) check_variable_binding(expr ast.VariableBinding) Type {
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

	mut init_type := t_none()

	if annotated := expr.typ {
		if expected_type := c.resolve_type_identifier(annotated) {
			if expr.init is ast.ArrayExpression {
				arr := expr.init as ast.ArrayExpression
				if arr.elements.len == 0 {
					init_type = expected_type
				} else {
					init_type = c.check_expr_with_hint(expr.init, expected_type)
				}
			} else {
				init_type = c.check_expr_with_hint(expr.init, expected_type)
			}
		} else {
			init_type = c.check_expr(expr.init)
		}
	} else {
		init_type = c.check_expr(expr.init)
	}

	final_type := c.check_binding_type(expr.identifier.name, expr.identifier.span, expr.typ,
		expr.init, init_type, 'in variable binding')

	if doc := expr.doc {
		c.env.store_doc(expr.identifier.name, doc)
	}
	c.record_type(expr.identifier.name, final_type, expr.identifier.span, expr.doc)

	return t_none()
}

fn (mut c TypeChecker) check_const_binding(expr ast.ConstBinding) Type {
	if c.in_function {
		c.error_at_span("'const' declarations are only allowed at the top level, not inside functions",
			expr.span)
	}

	init_type := c.check_expr(expr.init)
	final_type := c.check_binding_type(expr.identifier.name, expr.identifier.span, expr.typ,
		expr.init, init_type, 'in const binding')

	if doc := expr.doc {
		c.env.store_doc(expr.identifier.name, doc)
	}
	c.record_type(expr.identifier.name, final_type, expr.identifier.span, expr.doc)

	return t_none()
}

fn (mut c TypeChecker) check_binary(expr ast.BinaryExpression) Type {
	left_type := c.check_expr(expr.left)
	right_type := c.check_expr(expr.right)

	op_str := expr.op.kind.str()
	return match expr.op.kind {
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
			c.expect_type(left_type, t_bool(), expr.span, 'in logical expression')
			c.expect_type(right_type, t_bool(), expr.span, 'in logical expression')
			t_bool()
		}
		else {
			t_none()
		}
	}
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

fn (mut c TypeChecker) check_unary(expr ast.UnaryExpression) Type {
	operand_type := c.check_expr(expr.expression)
	op_str := expr.op.kind.str()

	return match expr.op.kind {
		.punc_minus {
			if !is_numeric(operand_type) {
				c.error_at_span("Operator '${op_str}' requires a numeric operand, got '${type_to_string(operand_type)}'",
					expr.expression.span)
			}
			operand_type
		}
		.punc_exclamation_mark {
			c.expect_type(operand_type, t_bool(), expr.expression.span, "for operator '${op_str}'")
			t_bool()
		}
		else {
			t_none()
		}
	}
}

fn (mut c TypeChecker) check_function_declaration(expr ast.FunctionDeclaration) Type {
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
			c.record_type_annotation(rt, resolved)
		} else {
			c.error_at_span("Unknown return type '${rt.identifier.name}'", rt.identifier.span)
		}
	}

	mut declared_err_type := ?Type(none)
	if et := expr.error_type {
		if resolved := c.resolve_type_identifier(et) {
			declared_err_type = resolved
			c.record_type_annotation(et, resolved)
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
				c.record_type_annotation(pt, resolved)
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
	body_type := c.check_expr_with_hint(expr.body, declared_ret_type)

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
		expected_ret := if et := declared_err_type {
			Type(TypeResult{
				success: final_ret_type
				error:   et
			})
		} else {
			final_ret_type
		}
		c.expect_type(body_type, expected_ret, expr.body.span, 'in function return')
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

	return t_none()
}

fn (mut c TypeChecker) check_function_expression(expr ast.FunctionExpression) Type {
	mut param_types := []Type{}
	mut seen_params := map[string]bool{}

	mut declared_ret_type := ?Type(none)
	if rt := expr.return_type {
		if resolved := c.resolve_type_identifier(rt) {
			declared_ret_type = resolved
			c.record_type_annotation(rt, resolved)
		} else {
			c.error_at_span("Unknown return type '${rt.identifier.name}'", rt.identifier.span)
		}
	}

	mut declared_err_type := ?Type(none)
	if et := expr.error_type {
		if resolved := c.resolve_type_identifier(et) {
			declared_err_type = resolved
			c.record_type_annotation(et, resolved)
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
				c.record_type_annotation(pt, resolved)
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
	body_type := c.check_expr_with_hint(expr.body, declared_ret_type)

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
		expected_ret := if et := declared_err_type {
			Type(TypeResult{
				success: final_ret_type
				error:   et
			})
		} else {
			final_ret_type
		}
		c.expect_type(body_type, expected_ret, expr.body.span, 'in function return')
	}

	return TypeFunction{
		params:     param_types
		ret:        final_ret_type
		error_type: final_err_type
	}
}

fn (mut c TypeChecker) check_type_pattern_binding(expr ast.TypePatternBinding) Type {
	init_type := c.check_expr(expr.init)

	if expected := c.resolve_type_identifier(expr.typ) {
		c.expect_type(init_type, expected, expr.init.span, 'in type pattern')
	} else {
		c.error_at_span("Unknown type '${expr.typ.identifier.name}'", expr.typ.identifier.span)
	}

	return t_none()
}

fn (mut c TypeChecker) check_tuple_destructuring(expr ast.TupleDestructuringBinding) Type {
	init_type := c.check_expr(expr.init)

	if init_type !is TypeTuple {
		c.error_at_span('Tuple destructuring requires a tuple type, got ${type_to_string(init_type)}',
			expr.span)
		for pattern in expr.patterns {
			c.check_expr(pattern)
		}
		return t_none()
	}

	tuple_type := init_type as TypeTuple

	if expr.patterns.len != tuple_type.elements.len {
		c.error_at_span('Tuple destructuring pattern has ${expr.patterns.len} elements, but tuple has ${tuple_type.elements.len}',
			expr.span)
	}

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
		} else if pattern is ast.TypeIdentifier {
			if expected := c.resolve_type_identifier(pattern) {
				if !types_equal(elem_type, expected) {
					c.error_at_span("Type mismatch in destructuring: expected '${type_to_string(expected)}', got '${type_to_string(elem_type)}'",
						pattern.span)
				}
			} else {
				c.error_at_span("Unknown type '${pattern.identifier.name}'", pattern.identifier.span)
			}
		} else {
			c.check_expr(pattern)
		}
	}

	return t_none()
}

fn (mut c TypeChecker) check_call(expr ast.FunctionCallExpression, expected ?Type) Type {
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
			for arg in expr.arguments {
				arg_type := c.check_expr(arg)
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
			return ret_type
		}
	}

	// Check if expected type is an enum and this is a variant name
	if exp := expected {
		if exp is TypeEnum {
			variant_name := expr.identifier.name
			if variant_name in exp.variants {
				return c.check_enum_variant_call(expr, exp, variant_name)
			}
		}
	}

	if suggestion := c.find_similar_name(expr.identifier.name) {
		c.error_at_span("'${expr.identifier.name}' is not defined. Did you mean '${suggestion}'?",
			expr.span)
	} else {
		c.error_at_span("'${expr.identifier.name}' is not defined", expr.span)
	}

	for arg in expr.arguments {
		c.check_expr(arg)
	}

	return t_none()
}

fn (mut c TypeChecker) check_enum_variant_call(expr ast.FunctionCallExpression, enum_type TypeEnum, variant_name string) Type {
	payload_types := enum_type.variants[variant_name] or { []Type{} }

	qualified_name := '${enum_type.name}.${variant_name}'
	variant_doc := c.env.lookup_doc(qualified_name)
	c.record_type(qualified_name, enum_type, expr.identifier.span, variant_doc)

	mut subs := map[string]Type{}

	if payload_types.len > 0 {
		if expr.arguments.len != payload_types.len {
			c.error_at_span("Enum variant '${variant_name}' expects ${payload_types.len} argument(s), got ${expr.arguments.len}",
				expr.span)
		}
		for i, arg in expr.arguments {
			arg_type := c.check_expr(arg)
			if i < payload_types.len {
				c.unify(arg_type, payload_types[i], mut subs)
			}
		}
	} else {
		if expr.arguments.len != 0 {
			c.error_at_span("Enum variant '${variant_name}' expects no arguments, got ${expr.arguments.len}",
				expr.span)
		}
	}

	mut result_enum := enum_type
	if enum_type.type_params.len > 0 && enum_type.type_args.len == 0 {
		mut resolved_args := []Type{}
		for param in enum_type.type_params {
			if arg := subs[param] {
				resolved_args << arg
			} else {
				resolved_args << t_var(param)
			}
		}

		mut new_variants := map[string][]Type{}
		for vname, vtypes in enum_type.variants {
			mut new_payloads := []Type{}
			for vt in vtypes {
				new_payloads << substitute(vt, subs)
			}
			new_variants[vname] = new_payloads
		}
		result_enum = TypeEnum{
			id:          enum_type.id
			name:        enum_type.name
			type_params: enum_type.type_params
			type_args:   resolved_args
			variants:    new_variants
		}
	}

	return result_enum
}

fn (mut c TypeChecker) check_call_with_type(expr ast.FunctionCallExpression, func_type TypeFunction) Type {
	if expr.arguments.len != func_type.params.len {
		c.error_at_span("Function '${expr.identifier.name}' expects ${func_type.params.len} arguments, got ${expr.arguments.len}",
			expr.span)

		for arg in expr.arguments {
			c.check_expr(arg)
		}

		return func_type.ret
	}

	mut subs := map[string]Type{}

	for i, arg in expr.arguments {
		param_type := func_type.params[i]
		arg_type := c.check_expr_with_hint(arg, param_type)

		if !c.unify(arg_type, param_type, mut subs) {
			instantiated_param := substitute(param_type, subs)
			c.expect_type(arg_type, instantiated_param, arg.span, "in argument ${i + 1} of '${expr.identifier.name}'")
		}
	}

	ret := substitute(func_type.ret, subs)
	return if err_type := func_type.error_type {
		Type(TypeResult{
			success: ret
			error:   substitute(err_type, subs)
		})
	} else {
		ret
	}
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

	if actual is TypeEnum && expected is TypeEnum {
		if actual.id != expected.id {
			return false
		}

		for i, actual_arg in actual.type_args {
			if i < expected.type_args.len {
				if !c.unify(actual_arg, expected.type_args[i], mut subs) {
					return false
				}
			}
		}

		for i, param in expected.type_params {
			if i >= expected.type_args.len && i < actual.type_args.len {
				subs[param] = actual.type_args[i]
			}
		}
		return true
	}

	if actual is TypeStruct && expected is TypeStruct {
		if actual.id != expected.id {
			return false
		}

		for i, actual_arg in actual.type_args {
			if i < expected.type_args.len {
				if !c.unify(actual_arg, expected.type_args[i], mut subs) {
					return false
				}
			}
		}

		for i, param in expected.type_params {
			if i >= expected.type_args.len && i < actual.type_args.len {
				subs[param] = actual.type_args[i]
			}
		}
		return true
	}

	return types_equal(actual, expected)
}

fn (mut c TypeChecker) check_if(expr ast.IfExpression, expected ?Type) Type {
	cond_type := c.check_expr(expr.condition)
	c.expect_type(cond_type, t_bool(), expr.condition.span, 'in if condition')

	then_type := c.check_expr_with_hint(expr.body, expected)

	mut result_type := then_type

	if else_body := expr.else_body {
		else_type := c.check_expr_with_hint(else_body, expected)
		result_type = c.unify_arm_types(then_type, else_type, expr.span)
	}

	return result_type
}

fn (mut c TypeChecker) check_array(expr ast.ArrayExpression) Type {
	if expr.elements.len == 0 {
		c.error_at_span("Cannot infer type of empty array. Provide a type annotation, e.g.: 'items []Int = []'",
			expr.span)
		return t_array(t_none())
	}

	mut first_type := t_none()
	mut first_type_set := false

	for elem in expr.elements {
		match elem {
			ast.SpreadElement {
				// Spread element: ..arr - inner must be an array
				inner := elem.expression or {
					c.error_at_span('Spread in array literal requires an expression',
						elem.span)
					continue
				}

				inner_type := c.check_expr(inner)

				element_type := if inner_type is TypeArray {
					inner_type.element
				} else {
					c.error_at_span('Spread operator requires an array, got ${type_to_string(inner_type)}',
						elem.span)
					t_none()
				}

				if !first_type_set {
					first_type = element_type
					first_type_set = true
				} else {
					c.expect_type(element_type, first_type, elem.span, 'in spread element')
				}
			}
			ast.Expression {
				// Regular element
				elem_type := c.check_expr(elem)

				if !first_type_set {
					first_type = elem_type
					first_type_set = true
				} else {
					c.expect_type(elem_type, first_type, elem.span, 'in array element')
				}
			}
		}
	}

	return t_array(first_type)
}

fn (mut c TypeChecker) check_tuple(expr ast.TupleExpression) Type {
	mut element_types := []Type{}

	for elem in expr.elements {
		elem_type := c.check_expr(elem)
		element_types << elem_type
	}

	return t_tuple(element_types)
}

fn (mut c TypeChecker) check_array_index(expr ast.ArrayIndexExpression) Type {
	arr_type := c.check_expr(expr.expression)

	if expr.index is ast.RangeExpression {
		range_expr := expr.index as ast.RangeExpression
		start_type := c.check_expr(range_expr.start)
		end_type := c.check_expr(range_expr.end)

		c.expect_type(start_type, t_int(), range_expr.start.span, 'as slice start')
		c.expect_type(end_type, t_int(), range_expr.end.span, 'as slice end')

		if arr_type !is TypeArray {
			c.error_at_span('Cannot slice non-array type ${type_to_string(arr_type)}',
				expr.span)
		}

		return arr_type
	}

	idx_type := c.check_expr(expr.index)

	c.expect_type(idx_type, t_int(), expr.index.span, 'as array index')

	element_type := if arr_type is TypeArray {
		arr_type.element
	} else {
		c.error_at_span('Cannot index non-array type ${type_to_string(arr_type)}', expr.span)
		t_none()
	}

	return t_option(element_type)
}

fn (mut c TypeChecker) check_struct_decl(stmt ast.StructDeclaration) Type {
	if c.in_function {
		c.error_at_span('Struct definitions are only allowed at the top level', stmt.span)
	}

	mut type_params := []string{}
	for tp in stmt.type_params {
		type_params << tp.name
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
		name:        stmt.identifier.name
		type_params: type_params
		fields:      fields
	}

	loc := c.def_loc_from_span(stmt.identifier.name, stmt.identifier.span)
	registered_struct := c.env.register_struct_at(struct_type, loc)

	if doc := stmt.doc {
		c.env.store_doc(stmt.identifier.name, doc)
	}

	c.record_type(stmt.identifier.name, Type(registered_struct), stmt.identifier.span,
		stmt.doc)

	// Check field initializers
	for f in stmt.fields {
		if init := f.init {
			c.check_expr(init)
		}
	}

	return t_none()
}

fn (mut c TypeChecker) check_struct_init(expr ast.StructInitExpression) Type {
	mut struct_type := if struct_def := c.env.lookup_struct(expr.identifier.name) {
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

	if expr.type_args.len > 0 {
		if struct_type.type_params.len != expr.type_args.len {
			c.error_at_span("Struct '${expr.identifier.name}' expects ${struct_type.type_params.len} type argument(s), got ${expr.type_args.len}",
				expr.identifier.span)
		} else {
			mut resolved_args := []Type{}
			for arg in expr.type_args {
				if resolved := c.resolve_type_identifier(arg) {
					resolved_args << resolved
				} else {
					c.error_at_span("Unknown type '${arg.identifier.name}'", arg.identifier.span)
					resolved_args << t_none()
				}
			}

			mut subs := map[string]Type{}
			for i, param in struct_type.type_params {
				subs[param] = resolved_args[i]
			}
			mut new_fields := map[string]Type{}
			for field_name, field_type in struct_type.fields {
				new_fields[field_name] = substitute(field_type, subs)
			}
			struct_type = TypeStruct{
				id:          struct_type.id
				name:        struct_type.name
				type_params: struct_type.type_params
				type_args:   resolved_args
				fields:      new_fields
			}
		}
	} else if struct_type.type_params.len > 0 {
		mut subs := map[string]Type{}

		for field in expr.fields {
			if expected_type := struct_type.fields[field.identifier.name] {
				actual_type := c.check_expr(field.init)
				c.infer_type_args(expected_type, actual_type, mut subs, field.init.span)
			}
		}

		mut resolved_args := []Type{}
		for param in struct_type.type_params {
			if inferred := subs[param] {
				resolved_args << inferred
			} else {
				c.error_at_span("Could not infer type parameter '${param}' for struct '${expr.identifier.name}'",
					expr.identifier.span)
				resolved_args << t_none()
			}
		}

		mut new_fields := map[string]Type{}
		for field_name, field_type in struct_type.fields {
			new_fields[field_name] = substitute(field_type, subs)
		}
		struct_type = TypeStruct{
			id:          struct_type.id
			name:        struct_type.name
			type_params: struct_type.type_params
			type_args:   resolved_args
			fields:      new_fields
		}
	}

	mut provided_fields := map[string]bool{}

	for field in expr.fields {
		if field.identifier.name in provided_fields {
			c.error_at_span("Duplicate field '${field.identifier.name}' in struct initializer",
				field.identifier.span)
		}
		provided_fields[field.identifier.name] = true

		actual_type := c.check_expr(field.init)
		if expected_type := struct_type.fields[field.identifier.name] {
			// Record type for field name hover in struct literals
			qualified_name := '${expr.identifier.name}.${field.identifier.name}'
			field_doc := c.env.lookup_doc(qualified_name)
			c.record_type(qualified_name, expected_type, field.identifier.span, field_doc)

			c.expect_type(actual_type, expected_type, field.init.span, "in field '${field.identifier.name}'")
		} else {
			available := struct_type.fields.keys().join(', ')
			c.error_at_span("Struct '${expr.identifier.name}' has no field '${field.identifier.name}'. Available fields: ${available}",
				field.identifier.span)
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

	return struct_type
}

fn (mut c TypeChecker) check_enum_decl(stmt ast.EnumDeclaration) Type {
	if c.in_function {
		c.error_at_span('Enum definitions are only allowed at the top level', stmt.span)
	}

	mut type_params := []string{}
	for tp in stmt.type_params {
		type_params << tp.name
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
		name:        stmt.identifier.name
		type_params: type_params
		variants:    variants
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

	return t_none()
}

fn (mut c TypeChecker) check_property_access(expr ast.PropertyAccessExpression, expected ?Type) Type {
	if expr.left is ast.Identifier {
		left_id := expr.left as ast.Identifier
		if looked_up := c.env.lookup_type(left_id.name) {
			if looked_up is TypeEnum {
				enum_type := looked_up

				enum_doc := c.env.lookup_doc(left_id.name)
				c.record_type(left_id.name, Type(enum_type), left_id.span, enum_doc)

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
					return t_none()
				}

				variant_doc := c.env.lookup_doc('${left_id.name}.${variant_name}')
				c.record_type('${left_id.name}.${variant_name}', Type(enum_type), variant_span,
					variant_doc)

				payload_types := enum_type.variants[variant_name] or { []Type{} }
				mut subs := map[string]Type{}

				if payload_types.len > 0 {
					if args.len != payload_types.len {
						c.error_at_span("Enum variant '${variant_name}' expects ${payload_types.len} argument(s), got ${args.len}",
							variant_span)
					}
					for i, arg in args {
						arg_type := c.check_expr(arg)
						if i < payload_types.len {
							c.unify(arg_type, payload_types[i], mut subs)
						}
					}
				} else if args.len > 0 {
					c.error_at_span("Enum variant '${variant_name}' takes no arguments",
						variant_span)
				}

				mut result_enum := enum_type
				if enum_type.type_params.len > 0 {
					expected_type_args := if exp := expected {
						if exp is TypeEnum && exp.id == enum_type.id {
							exp.type_args
						} else {
							[]Type{}
						}
					} else {
						[]Type{}
					}

					mut resolved_args := []Type{}
					for i, param in enum_type.type_params {
						if arg := subs[param] {
							resolved_args << arg
						} else if i < expected_type_args.len {
							resolved_args << expected_type_args[i]
						} else {
							resolved_args << t_var(param)
						}
					}

					mut new_variants := map[string][]Type{}
					for vname, vtypes in enum_type.variants {
						mut new_payloads := []Type{}
						for vt in vtypes {
							new_payloads << substitute(vt, subs)
						}
						new_variants[vname] = new_payloads
					}
					result_enum = TypeEnum{
						id:          enum_type.id
						name:        enum_type.name
						type_params: enum_type.type_params
						type_args:   resolved_args
						variants:    new_variants
					}
				}

				return Type(result_enum)
			}
		}
	}

	left_type := c.check_expr(expr.left)

	if expr.right is ast.FunctionCallExpression {
		right_type := c.check_expr(expr.right)
		return right_type
	}

	if expr.right is ast.NumberLiteral {
		num_lit := expr.right as ast.NumberLiteral

		if left_type is TypeTuple {
			index := num_lit.value.int()
			if index < 0 || index >= left_type.elements.len {
				c.error_at_span('Tuple index ${index} out of bounds. Tuple has ${left_type.elements.len} elements.',
					num_lit.span)
				return t_none()
			}
			return left_type.elements[index]
		} else {
			c.error_at_span('Cannot use numeric index on type ${type_to_string(left_type)}. Only tuples support .0 .1 etc.',
				num_lit.span)
			return t_none()
		}
	}

	if expr.right !is ast.Identifier {
		err_span := expr.right.span
		c.error_at_span('Expected identifier in property access', err_span)
		return t_none()
	}

	right := expr.right as ast.Identifier

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

	return result_type
}

fn (mut c TypeChecker) check_match(expr ast.MatchExpression) Type {
	subject_type := c.check_expr(expr.subject)

	if expr.arms.len == 0 {
		return t_none()
	}

	mut first_type := t_none()
	mut pats := []Pat{}

	for i, arm in expr.arms {
		c.env.push_scope()

		c.check_pattern(arm.pattern, subject_type)

		pat := ast_pattern_to_pat(arm.pattern, subject_type)
		if !check_pattern_useful(pats, pat, subject_type) {
			if arm.pattern is ast.WildcardPattern {
				c.warning_at_span('Previous arms already match all cases, else branch is unreachable',
					arm.pattern.span)
			} else {
				c.warning_at_span('Unreachable pattern', arm.pattern.span)
			}
		}
		pats << pat

		arm_type := c.check_expr(arm.body)
		c.env.pop_scope()

		if i == 0 {
			first_type = arm_type
		} else {
			first_type = c.unify_arm_types(first_type, arm_type, arm.body.span)
		}
	}

	if missing := check_exhaustiveness(pats, subject_type) {
		c.error_at_span('Match is not exhaustive, missing: ${missing}', expr.subject.span)
	}

	return first_type
}

fn (mut c TypeChecker) check_pattern(pattern ast.Expression, subject_type Type) Type {
	if pattern is ast.OrPattern {
		for p in pattern.patterns {
			c.check_pattern(p, subject_type)
		}
		return subject_type
	}

	// Handle qualified enum patterns like MyEnum.Variant or MyEnum.Variant(binding)
	if pattern is ast.PropertyAccessExpression {
		if pattern.left is ast.Identifier {
			left_id := pattern.left as ast.Identifier
			if looked_up := c.env.lookup_type(left_id.name) {
				if looked_up is TypeEnum {
					enum_type := looked_up

					variant_name, args, _ := if pattern.right is ast.FunctionCallExpression {
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
						raw_payload_types := enum_type.variants[variant_name] or { []Type{} }
						// Substitute type parameters with concrete type arguments from subject_type
						mut subs := map[string]Type{}
						if subject_type is TypeEnum {
							for i, param in subject_type.type_params {
								if i < subject_type.type_args.len {
									subs[param] = subject_type.type_args[i]
								}
							}
						}
						mut payload_types := []Type{}
						for pt in raw_payload_types {
							payload_types << substitute(pt, subs)
						}

						// Bind pattern variables to their corresponding payload types
						for i, arg in args {
							if arg is ast.Identifier && i < payload_types.len {
								c.env.define(arg.name, payload_types[i])
								c.record_type(arg.name, payload_types[i], arg.span, none)
							}
						}

						for arg in args {
							c.check_expr(arg)
						}

						return subject_type
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
			if elem is ast.SpreadElement && i != pattern.elements.len - 1 {
				c.error_at_span('Spread pattern must be at the end of the array pattern',
					elem.span)
			}
		}

		for elem in pattern.elements {
			match elem {
				ast.SpreadElement {
					// Spread pattern: ..rest or just ..
					if inner := elem.expression {
						if inner is ast.Identifier {
							// Named spread: bind to array type
							c.env.define(inner.name, subject_type)
							c.record_type(inner.name, subject_type, inner.span, none)
						} else {
							// Other expression (shouldn't happen in patterns)
							c.check_expr(inner)
						}
					}
					// Anonymous spread (..): just match, don't bind
				}
				ast.Expression {
					if elem is ast.Identifier {
						// Named binding: bind to element type
						c.env.define(elem.name, element_type)
						c.record_type(elem.name, element_type, elem.span, none)
					} else {
						// Other patterns (literals, nested patterns)
						c.check_pattern(elem, element_type)
					}
				}
			}
		}

		return subject_type
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

		for i, elem in pattern.elements {
			elem_type := if i < tuple_type.elements.len {
				tuple_type.elements[i]
			} else {
				t_none()
			}

			if elem is ast.Identifier {
				c.env.define(elem.name, elem_type)
				c.record_type(elem.name, elem_type, elem.span, none)
			} else {
				c.check_pattern(elem, elem_type)
			}
		}

		return subject_type
	}

	if pattern is ast.FunctionCallExpression {
		variant_name := pattern.identifier.name

		if subject_type is TypeEnum {
			if variant_name !in subject_type.variants {
				c.error_at_span("Enum '${subject_type.name}' has no variant '${variant_name}'",
					pattern.identifier.span)
				for arg in pattern.arguments {
					c.check_expr(arg)
				}
				return subject_type
			}

			raw_payload_types := subject_type.variants[variant_name] or { []Type{} }
			// Substitute type parameters with concrete type arguments
			mut subs := map[string]Type{}
			for i, param in subject_type.type_params {
				if i < subject_type.type_args.len {
					subs[param] = subject_type.type_args[i]
				}
			}
			mut payload_types := []Type{}
			for pt in raw_payload_types {
				payload_types << substitute(pt, subs)
			}
			for i, arg in pattern.arguments {
				if arg is ast.Identifier && i < payload_types.len {
					c.env.define(arg.name, payload_types[i])
					c.record_type(arg.name, payload_types[i], arg.span, none)
				}
			}
			// Record variant for go-to-definition and hover using qualified name
			qualified_name := '${subject_type.name}.${variant_name}'
			doc := c.env.lookup_doc(qualified_name)
			c.record_type(qualified_name, subject_type, pattern.identifier.span, doc)

			for arg in pattern.arguments {
				c.check_expr(arg)
			}

			return subject_type
		}

		expr_type := c.check_expr(pattern)
		if !types_equal(expr_type, subject_type) {
			c.error_at_span("Pattern type '${type_to_string(expr_type)}' does not match subject type '${type_to_string(subject_type)}'",
				pattern.span)
		}
		return subject_type
	}

	if pattern is ast.Identifier {
		if subject_type is TypeEnum {
			if pattern.name in subject_type.variants {
				qualified_name := '${subject_type.name}.${pattern.name}'
				doc := c.env.lookup_doc(qualified_name)
				c.record_type(qualified_name, subject_type, pattern.span, doc)
				return subject_type
			}
		}
	}

	if pattern is ast.RangeExpression {
		start_type := c.check_expr(pattern.start)
		end_type := c.check_expr(pattern.end)

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

		return subject_type
	}

	// UnaryExpression is valid for negative number literals like -5
	// and for boolean negation like !cond when matching Bool
	if pattern is ast.UnaryExpression {
		if pattern.op.kind == .punc_minus {
			if pattern.expression is ast.NumberLiteral {
				return c.check_expr(pattern)
			}
		}
		// Allow !expr when matching against Bool (e.g., match true { !cond -> ... })
		if pattern.op.kind == .punc_exclamation_mark && types_equal(subject_type, t_bool()) {
			expr_type := c.check_expr(pattern)
			if types_equal(expr_type, t_bool()) {
				return expr_type
			}
		}
		c.error_at_span('Invalid pattern: unary expressions are only allowed for negative number literals',
			pattern.span)
		return c.check_expr(pattern)
	}

	// Only allow valid pattern types - literals, identifiers, wildcards
	match pattern {
		ast.NumberLiteral, ast.StringLiteral, ast.BooleanLiteral, ast.NoneExpression,
		ast.Identifier, ast.WildcardPattern {
			return c.check_expr(pattern)
		}
		else {
			// Allow any boolean expression as a pattern when matching against Bool
			// This enables the "match true { cond -> result }" idiom
			if types_equal(subject_type, t_bool()) {
				expr_type := c.check_expr(pattern)
				if types_equal(expr_type, t_bool()) {
					return expr_type
				}
			}
			c.error_at_span('Invalid pattern: only literals, identifiers, arrays, tuples, enum variants, ranges, and or-patterns are allowed',
				pattern.span)
			return c.check_expr(pattern)
		}
	}
}

fn (mut c TypeChecker) check_or(expr ast.OrExpression) Type {
	inner_type := c.check_expr(expr.expression)

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

	body_type := c.check_expr(expr.body)

	c.expect_type(body_type, success_type, expr.body.span, "in 'or' fallback")

	if expr.receiver != none {
		c.env.pop_scope()
	}

	return success_type
}

fn (mut c TypeChecker) check_range(expr ast.RangeExpression) Type {
	start_type := c.check_expr(expr.start)
	end_type := c.check_expr(expr.end)

	if !types_equal(start_type, t_int()) {
		c.error_at_span('Range start must be Int, got ${type_to_string(start_type)}',
			expr.start.span)
	}

	if !types_equal(end_type, t_int()) {
		c.error_at_span('Range end must be Int, got ${type_to_string(end_type)}', expr.end.span)
	}

	return t_array(t_int())
}
