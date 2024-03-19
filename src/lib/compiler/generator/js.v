module generator

import lib.compiler.parser.ast
import lib.compiler

pub fn generate_js(node ast.Statement) string {
	match node {
		ast.AssertStatement {
			result := '
				if (!${generate_js_from_expression(node.expression)}) {
					throw new Error(${generate_js_from_expression(node.message)})
				}
			'

			return result
		}
		ast.AssignmentStatement {
			return '${node.identifier.name} = ${generate_js_from_expression(node.expression)};\n'
		}
		ast.DeclarationStatement {
			return 'let ${node.identifier.name} = ${generate_js_from_expression(node.expression)};\n'
		}
		ast.BreakStatement {
			return 'break;'
		}
		ast.ContinueStatement {
			return 'continue;'
		}
		ast.ConstStatement {
			return 'const ${node.identifier.name} = ${generate_js_from_expression(node.init)};'
		}
		ast.ExportStatement {
			return '/*exported*/ ${generate_js(node.declaration)};'
		}
		ast.Expression {
			return '${generate_js_from_expression(node)};\n'
		}
		ast.ForInStatement {
			return 'for (const ${node.identifier.name} in ${generate_js_from_expression(node.expression)}) {
				${generate_js_from_statements(node.body)}
			}\n\n'
		}
		ast.ForStatement {
			return 'while (true) {
				${generate_js_from_statements(node.body)}
			}\n\n'
		}
		ast.FunctionParameter {
			return node.identifier.name
		}
		ast.FunctionStatement {
			return 'function ${node.identifier.name}(${node.params.map(generate_js(it)).join(', ')}) {
				${generate_js_from_statements(node.body)}
			}\n\n'
		}
		ast.IfStatement {
			if else_statement := node.else_statement {
				return 'if (${generate_js_from_expression(node.condition)}) {
					${generate_js_from_statements(node.body)}
				} else {
					${generate_js_from_statements([
					else_statement,
				])}
				}'
			}

			return 'if (${generate_js_from_expression(node.condition)}) {
				${generate_js_from_statements(node.body)}
			}'
		}
		ast.ImportDeclaration {
			return "import {${generate_js_from_import_specifiers(node.specifiers)}} from '${node.path}';"
		}
		ast.OrStatement {
			return '
				try {
					
				} catch (e) {
					${generate_js_from_statements(node.body)}
				}

			'
		}
		ast.ReturnStatement {
			return 'return ${generate_js_from_optional_expression(node.expression, 'undefined')};'
		}
		ast.StructDeclarationStatement {
			return 'class ${node.identifier.name} {
				${node.fields.map(generate_js(it)).join('')}
			}\n\n'
		}
		ast.StructField {
			return generate_js_from_struct_field(node)
		}
		ast.ThrowStatement {
			return 'throw ${generate_js_from_expression(node.expression)};'
		}
	}
}

pub fn generate_js_from_struct_field(node ast.StructField) string {
	return '
		${node.identifier.name} = ${generate_js_from_optional_expression(node.init,
		'undefined')}
	\n'
}

pub fn generate_js_from_optional_expression(node ?ast.Expression, default string) string {
	if unwrapped := node {
		return generate_js_from_expression(unwrapped)
	}

	return default
}

pub fn generate_js_from_import_specifiers(nodes []ast.ImportSpecifier) string {
	return nodes.map(it.identifier.name).join(', ')
}

pub fn generate_js_from_statements(nodes []ast.Statement) string {
	return nodes.map(generate_js(it)).join('')
}

pub fn generate_js_from_block_expression(node ast.BlockExpression) string {
	content := node.body.map(generate_js(it)).join('')

	return '(() => {
		${content}
	})()'
}

pub fn generate_js_from_operator(node ast.Operator) string {
	return compiler.Token{
		kind: node.kind
	}.str()
}

pub fn generate_js_from_expression(node ast.Expression) string {
	match node {
		ast.BinaryExpression {
			return '(${generate_js_from_expression(node.left)} ${generate_js_from_operator(node.op)} ${generate_js_from_expression(node.right)})'
		}
		ast.ArrayExpression {
			return '[${node.elements.map(generate_js_from_expression(it)).join(', ')}]'
		}
		ast.NumberLiteral {
			return node.value.str()
		}
		ast.StringLiteral {
			return "'${node.value.replace("'", "\\'")}'"
		}
		ast.Identifier {
			return node.name
		}
		ast.BooleanLiteral {
			return node.value.str()
		}
		ast.BlockExpression {
			return generate_js_from_block_expression(node)
		}
		ast.FunctionCallExpression {
			return '${generate_js_from_expression(node.identifier)}(${node.arguments.map(generate_js_from_expression(it)).join(', ')})'
		}
		ast.NoneExpression {
			return 'null'
		}
		ast.PostfixExpression {
			return '${generate_js_from_expression(node.expression)}${generate_js_from_operator(node.op)}'
		}
		ast.PropertyAccessExpression {
			return '${node.identifier.name}.${generate_js_from_expression(node.expression)}'
		}
		ast.RangeExpression {
			return 'Array.from({length: ${generate_js_from_expression(node.end)} - ${generate_js_from_expression(node.start)} + 1}, (_, i) => ${generate_js_from_expression(node.start)} + i)'
		}
		ast.StructInitialisation {
			return 'new ${node.identifier.name}({
				${node.fields.map(generate_js_from_expression(it)).join(', ')}
			})'
		}
		ast.StructInitialisationField {
			return '${node.identifier.name}: ${generate_js_from_expression(node.init)}'
		}
		ast.UnaryExpression {
			return '${generate_js_from_operator(node.op)}${generate_js_from_expression(node.expression)}'
		}
	}
}
