module printer

import compiler.ast
import strings

pub fn print_expr(expr ast.Expression) string {
	return print_expression(expr, 0)
}

fn indent(level int) string {
	return strings.repeat(`\t`, level)
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
		ast.VariableBinding {
			'${expr.identifier.name} = ${print_expression(expr.init, level)}'
		}
		ast.ConstBinding {
			'const ${expr.identifier.name} = ${print_expression(expr.init, level)}'
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
		ast.PropagateExpression {
			'${print_expression(expr.expression, level)}!'
		}
		ast.BlockExpression {
			if expr.body.len == 0 {
				'{}'
			} else if expr.body.len == 1 {
				'{ ${print_expression(expr.body[0], level)} }'
			} else {
				mut s := '{\n'
				for e in expr.body {
					s += '${indent(level + 1)}${print_expression(e, level + 1)}\n'
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
				s += '${indent(level + 1)}${print_expression(arm.pattern, level + 1)} => ${print_expression(arm.body,
					level + 1)},\n'
			}
			s += '${indent(level)}}'
			s
		}
		ast.OrExpression {
			mut s := '${print_expression(expr.expression, level)} or '
			if receiver := expr.receiver {
				s += '${receiver.name} => '
			}
			s += print_expression(expr.body, level)
			s
		}
		ast.ErrorExpression {
			'error ${print_expression(expr.expression, level)}'
		}
		ast.FunctionExpression {
			mut s := 'fn '
			if id := expr.identifier {
				s += id.name
			}
			s += '('
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
		ast.StructExpression {
			mut s := 'struct ${expr.identifier.name} {\n'
			for field in expr.fields {
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
		ast.StructInitExpression {
			mut s := '${expr.identifier.name}{\n'
			for field in expr.fields {
				s += '${indent(level + 1)}${field.identifier.name}: ${print_expression(field.init,
					level + 1)},\n'
			}
			s += '${indent(level)}}'
			s
		}
		ast.EnumExpression {
			mut s := 'enum ${expr.identifier.name} {\n'
			for variant in expr.variants {
				s += '${indent(level + 1)}${variant.identifier.name}'
				if payload := variant.payload {
					s += '(${print_expression(payload, level + 1)})'
				}
				s += ',\n'
			}
			s += '${indent(level)}}'
			s
		}
		ast.ImportDeclaration {
			mut s := "from '${expr.path}' import "
			for i, spec in expr.specifiers {
				if i > 0 {
					s += ', '
				}
				s += spec.identifier.name
			}
			s
		}
		ast.ExportExpression {
			'export ${print_expression(expr.expression, level)}'
		}
		ast.AssertExpression {
			'assert ${print_expression(expr.expression, level)}, ${print_expression(expr.message,
				level)}'
		}
		ast.PostfixExpression {
			op := match expr.op.kind {
				.punc_plusplus { '++' }
				.punc_minusminus { '--' }
				else { '?' }
			}
			'${print_expression(expr.expression, level)}${op}'
		}
		ast.WildcardPattern {
			'else'
		}
	}
}
