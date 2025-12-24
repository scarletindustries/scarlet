module printer

import ast
import strings

pub fn print_expr(expr ast.Expression) string {
	return print_expression(expr, 0)
}

fn indent(level int) string {
	return strings.repeat(`\t`, level)
}

fn print_node_at_level(node ast.Node, level int) string {
	return match node {
		ast.Statement { print_statement(node, level) }
		ast.Expression { print_expression(node, level) }
	}
}

fn print_statement(stmt ast.Statement, level int) string {
	return match stmt {
		ast.VariableBinding {
			'${stmt.identifier.name} = ${print_expression(stmt.init, level)}'
		}
		ast.ConstBinding {
			'const ${stmt.identifier.name} = ${print_expression(stmt.init, level)}'
		}
		ast.TypePatternBinding {
			'${print_expression(stmt.typ, level)} = ${print_expression(stmt.init, level)}'
		}
		ast.FunctionDeclaration {
			print_function(level, stmt.identifier, stmt.params, stmt.body, stmt.return_type,
				stmt.error_type)
		}
		ast.StructDeclaration {
			mut s := 'struct ${stmt.identifier.name} {\n'
			for field in stmt.fields {
				s += '${indent(level + 1)}${field.identifier.name} ${print_expression(field.typ,
					level + 1)}'
				if init := field.init {
					s += ' = ${print_expression(init, level + 1)}'
				}
				s += ',\n'
			}
			s += '${indent(level)}}'
			s
		}
		ast.EnumDeclaration {
			mut s := 'enum ${stmt.identifier.name} {\n'
			for variant in stmt.variants {
				s += '${indent(level + 1)}${variant.identifier.name}'
				if variant.payload.len > 0 {
					s += '('
					for i, payload in variant.payload {
						if i > 0 {
							s += ', '
						}
						s += print_expression(payload, level + 1)
					}
					s += ')'
				}
				s += ',\n'
			}
			s += '${indent(level)}}'
			s
		}
		ast.ImportDeclaration {
			mut s := "from '${stmt.path}' import "
			for i, spec in stmt.specifiers {
				if i > 0 {
					s += ', '
				}
				s += spec.identifier.name
			}
			s
		}
		ast.ExportDeclaration {
			'export ${print_statement(stmt.declaration, level)}'
		}
	}
}

fn print_function(level int, identifier ast.Identifier, params []ast.FunctionParameter, body ast.Expression, return_type ?ast.TypeIdentifier, error_type ?ast.TypeIdentifier) string {
	mut s := 'fn ${identifier.name}('
	for i, param in params {
		if i > 0 {
			s += ', '
		}
		s += param.identifier.name
		if typ := param.typ {
			s += ' ${print_expression(typ, level)}'
		}
	}
	s += ')'
	if ret := return_type {
		s += ' ${print_expression(ret, level)}'
	}
	if err := error_type {
		s += '!${print_expression(err, level)}'
	}
	s += ' ${print_expression(body, level)}'
	return s
}

fn print_expression(expr ast.Expression, level int) string {
	return match expr {
		ast.StringLiteral {
			"'${expr.value}'"
		}
		ast.InterpolatedString {
			mut s := "'"
			for part in expr.parts {
				if part is ast.StringLiteral {
					s += part.value
				} else if part is ast.Identifier {
					s += '\$${part.name}'
				} else {
					s += '\${${print_expression(part, level)}}'
				}
			}
			s += "'"
			s
		}
		ast.NumberLiteral {
			expr.value
		}
		ast.BooleanLiteral {
			if expr.value {
				'true'
			} else {
				'false'
			}
		}
		ast.NoneExpression {
			'none'
		}
		ast.Identifier {
			expr.name
		}
		ast.TypeIdentifier {
			mut s := ''
			if expr.is_option {
				s += '?'
			}
			if expr.is_array {
				s += '[]'
			}
			s += expr.identifier.name
			s
		}
		ast.BinaryExpression {
			op := match expr.op.kind {
				.punc_plus { '+' }
				.punc_minus { '-' }
				.punc_mul { '*' }
				.punc_div { '/' }
				.punc_mod { '%' }
				.punc_equals_comparator { '==' }
				.punc_not_equal { '!=' }
				.punc_gt { '>' }
				.punc_lt { '<' }
				.punc_gte { '>=' }
				.punc_lte { '<=' }
				.logical_and { '&&' }
				.logical_or { '||' }
				else { '?' }
			}
			'${print_expression(expr.left, level)} ${op} ${print_expression(expr.right,
				level)}'
		}
		ast.UnaryExpression {
			op := match expr.op.kind {
				.punc_exclamation_mark { '!' }
				else { '?' }
			}
			'${op}${print_expression(expr.expression, level)}'
		}
		ast.PropagateNoneExpression {
			'${print_expression(expr.expression, level)}?'
		}
		ast.BlockExpression {
			if expr.body.len == 0 {
				'{}'
			} else if expr.body.len == 1 {
				'{ ${print_node_at_level(expr.body[0], level)} }'
			} else {
				mut s := '{\n'
				for e in expr.body {
					s += '${indent(level + 1)}${print_node_at_level(e, level + 1)}\n'
				}
				s += '${indent(level)}}'
				s
			}
		}
		ast.IfExpression {
			mut s := 'if ${print_expression(expr.condition, level)} ${print_expression(expr.body,
				level)}'
			if else_body := expr.else_body {
				s += ' else ${print_expression(else_body, level)}'
			}
			s
		}
		ast.MatchExpression {
			mut s := 'match ${print_expression(expr.subject, level)} {\n'
			for arm in expr.arms {
				s += '${indent(level + 1)}${print_expression(arm.pattern, level + 1)} -> ${print_expression(arm.body,
					level + 1)},\n'
			}
			s += '${indent(level)}}'
			s
		}
		ast.OrExpression {
			mut s := '${print_expression(expr.expression, level)} or '
			if receiver := expr.receiver {
				s += '${receiver.name} -> '
			}
			s += print_expression(expr.body, level)
			s
		}
		ast.ErrorExpression {
			'error ${print_expression(expr.expression, level)}'
		}
		ast.FunctionExpression {
			mut s := 'fn('
			for i, param in expr.params {
				if i > 0 {
					s += ', '
				}
				s += param.identifier.name
				if typ := param.typ {
					s += ' ${print_expression(typ, level)}'
				}
			}
			s += ')'
			if ret := expr.return_type {
				s += ' ${print_expression(ret, level)}'
			}
			if err := expr.error_type {
				s += '!${print_expression(err, level)}'
			}
			s += ' ${print_expression(expr.body, level)}'
			s
		}
		ast.FunctionCallExpression {
			mut s := '${expr.identifier.name}('
			for i, arg in expr.arguments {
				if i > 0 {
					s += ', '
				}
				s += print_expression(arg, level)
			}
			s += ')'
			s
		}
		ast.PropertyAccessExpression {
			'${print_expression(expr.left, level)}.${print_expression(expr.right, level)}'
		}
		ast.ArrayExpression {
			mut s := '['
			for i, elem in expr.elements {
				if i > 0 {
					s += ', '
				}
				s += print_expression(elem, level)
			}
			s += ']'
			s
		}
		ast.ArrayIndexExpression {
			'${print_expression(expr.expression, level)}[${print_expression(expr.index,
				level)}]'
		}
		ast.RangeExpression {
			'${print_expression(expr.start, level)}..${print_expression(expr.end, level)}'
		}
		ast.StructInitExpression {
			mut s := '${expr.identifier.name}{\n'
			for field in expr.fields {
				s += '${indent(level + 1)}${field.identifier.name}: ${print_expression(field.init,
					level + 1)},\n'
			}
			s += '${indent(level)}}'
			s
		}
		ast.SpreadExpression {
			if inner := expr.expression {
				'..${print_expression(inner, level)}'
			} else {
				'..'
			}
		}
		ast.AssertExpression {
			'assert ${print_expression(expr.expression, level)}, ${print_expression(expr.message,
				level)}'
		}
		ast.WildcardPattern {
			'else'
		}
		ast.OrPattern {
			mut s := ''
			for i, pattern in expr.patterns {
				if i > 0 {
					s += ' | '
				}
				s += print_expression(pattern, level)
			}
			s
		}
		ast.ErrorNode {
			'/* could not parse: ${expr.message} */'
		}
	}
}
