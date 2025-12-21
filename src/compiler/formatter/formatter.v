module formatter

import compiler
import compiler.ast
import compiler.token
import compiler.scanner
import compiler.parser
import compiler.diagnostic
import strings

pub struct FormatResult {
pub:
	output      string
	has_errors  bool
	diagnostics []diagnostic.Diagnostic
}

pub fn format(input string) FormatResult {
	return format_with_debug(input, false)
}

pub fn format_with_debug(input string, debug bool) FormatResult {
	mut s := scanner.new_scanner(input)
	tokens := s.scan_all()

	if debug {
		for tok in tokens {
			eprintln('Token: ${tok.kind} "${tok.literal or { '' }}" trivia: ${tok.leading_trivia.len}')
			for t in tok.leading_trivia {
				eprintln('  Trivia: ${t.kind} "${t.text.replace('\n', '\\n')}"')
			}
		}
	}

	mut trivia_map := map[string][]token.Trivia{}
	for tok in tokens {
		if tok.leading_trivia.len > 0 {
			key := '${tok.line}:${tok.column}'
			trivia_map[key] = tok.leading_trivia
		}
	}

	mut f := Formatter{
		tokens:     tokens
		trivia_map: trivia_map
		output:     strings.new_builder(input.len)
		indent:     0
	}

	mut scanner2 := scanner.new_scanner(input)
	mut p := parser.new_parser(mut scanner2)
	result := p.parse_program()

	errors := result.diagnostics.filter(it.severity == .error)
	if errors.len > 0 {
		return FormatResult{
			output:      input
			has_errors:  true
			diagnostics: errors
		}
	}

	f.format_block(result.ast, true)

	mut final := f.output.str()

	final = final.trim_right('\n') + '\n'
	return FormatResult{
		output:      final
		has_errors:  false
		diagnostics: result.diagnostics
	}
}

struct Formatter {
	tokens     []compiler.Token
	trivia_map map[string][]token.Trivia
mut:
	output        strings.Builder
	indent        int
	at_line_start bool = true
}

fn (mut f Formatter) emit(s string) {
	f.output.write_string(s)
	if s.len > 0 {
		f.at_line_start = s[s.len - 1] == `\n`
	}
}

fn (mut f Formatter) emit_indent() {
	for _ in 0 .. f.indent {
		f.output.write_u8(`\t`)
	}
	f.at_line_start = false
}

fn (mut f Formatter) emit_trivia_for_span(span ast.Span) {
	key := '${span.line}:${span.column}'
	if trivia := f.trivia_map[key] {
		mut consecutive_newlines := 0

		for t in trivia {
			match t.kind {
				.newline {
					consecutive_newlines++
				}
				.line_comment {
					if consecutive_newlines > 1 {
						for _ in 0 .. consecutive_newlines - 1 {
							f.emit('\n')
						}
					}

					if !f.at_line_start {
						f.emit('\n')
					}
					f.emit_indent()
					f.emit(t.text)
					f.emit('\n')
					consecutive_newlines = 0
				}
				.whitespace {}
			}
		}

		if consecutive_newlines > 1 {
			for _ in 0 .. consecutive_newlines - 1 {
				f.emit('\n')
			}
		}
	}
}

fn (mut f Formatter) format_block(block ast.BlockExpression, is_top_level bool) {
	for i, expr in block.body {
		span := ast.get_span(expr)
		if span.line > 0 {
			f.emit_trivia_for_span(span)
		}

		if !f.at_line_start && i > 0 {
			f.emit('\n')
		}

		if f.at_line_start && !is_top_level {
			f.emit_indent()
		} else if f.at_line_start && is_top_level && i > 0 {
		}

		f.format_expr(expr)

		if !f.at_line_start {
			f.emit('\n')
		}
	}
}

fn (mut f Formatter) format_expr(expr ast.Expression) {
	match expr {
		ast.StringLiteral {
			f.emit("'${escape_string(expr.value)}'")
		}
		ast.InterpolatedString {
			mut s := "'"
			for part in expr.parts {
				if part is ast.StringLiteral {
					s += escape_string(part.value)
				} else if part is ast.Identifier {
					s += '\${${part.name}}'
				} else {
					s += '\${${f.format_expr_to_string(part)}}'
				}
			}
			s += "'"
			f.emit(s)
		}
		ast.NumberLiteral {
			f.emit(expr.value)
		}
		ast.BooleanLiteral {
			f.emit(if expr.value { 'true' } else { 'false' })
		}
		ast.NoneExpression {
			f.emit('none')
		}
		ast.Identifier {
			f.emit(expr.name)
		}
		ast.TypeIdentifier {
			f.format_type(expr)
		}
		ast.VariableBinding {
			f.emit(expr.identifier.name)
			f.emit(' = ')
			f.format_expr(expr.init)
		}
		ast.ConstBinding {
			f.emit('const ')
			f.emit(expr.identifier.name)
			f.emit(' = ')
			f.format_expr(expr.init)
		}
		ast.BinaryExpression {
			f.format_expr(expr.left)
			f.emit(' ${op_to_string(expr.op.kind)} ')
			f.format_expr(expr.right)
		}
		ast.UnaryExpression {
			op := match expr.op.kind {
				.punc_exclamation_mark { '!' }
				.punc_minus { '-' }
				else { '?' }
			}
			f.emit(op)
			f.format_expr(expr.expression)
		}
		ast.PropagateNoneExpression {
			f.format_expr(expr.expression)
			f.emit('?')
		}
		ast.BlockExpression {
			f.format_block_expr(expr)
		}
		ast.IfExpression {
			f.emit('if ')
			f.format_expr(expr.condition)
			f.emit(' ')
			f.format_block_inline(expr.body)
			if else_body := expr.else_body {
				f.emit(' else ')
				if else_body is ast.IfExpression {
					f.format_expr(else_body)
				} else {
					f.format_block_inline(else_body)
				}
			}
		}
		ast.MatchExpression {
			f.emit('match ')
			f.format_expr(expr.subject)
			f.emit(' {\n')
			f.indent++
			for arm in expr.arms {
				f.emit_indent()
				f.format_expr(arm.pattern)
				f.emit(' -> ')
				f.format_expr(arm.body)
				f.emit(',\n')
			}
			if expr.close_span.line > 0 {
				f.emit_trivia_for_span(expr.close_span)
			}
			f.indent--
			f.emit_indent()
			f.emit('}')
		}
		ast.OrExpression {
			f.format_expr(expr.expression)
			f.emit(' or ')
			if receiver := expr.receiver {
				f.emit(receiver.name)
				f.emit(' -> ')
			}
			f.format_expr(expr.body)
		}
		ast.ErrorExpression {
			f.emit('error ')
			f.format_expr(expr.expression)
		}
		ast.FunctionExpression {
			f.format_function(expr)
		}
		ast.FunctionCallExpression {
			f.emit(expr.identifier.name)
			f.emit('(')
			for i, arg in expr.arguments {
				if i > 0 {
					f.emit(', ')
				}
				f.format_expr(arg)
			}
			f.emit(')')
		}
		ast.PropertyAccessExpression {
			f.format_expr(expr.left)
			f.emit('.')
			f.format_expr(expr.right)
		}
		ast.ArrayExpression {
			f.emit('[')
			for i, elem in expr.elements {
				if i > 0 {
					f.emit(', ')
				}
				f.format_expr(elem)
			}
			f.emit(']')
		}
		ast.ArrayIndexExpression {
			f.format_expr(expr.expression)
			f.emit('[')
			f.format_expr(expr.index)
			f.emit(']')
		}
		ast.RangeExpression {
			f.format_expr(expr.start)
			f.emit('..')
			f.format_expr(expr.end)
		}
		ast.StructExpression {
			f.emit('struct ')
			f.emit(expr.identifier.name)
			f.emit(' {\n')
			f.indent++
			for field in expr.fields {
				f.emit_indent()
				f.emit(field.identifier.name)
				f.emit(' ')
				f.format_type(field.typ)
				if init := field.init {
					f.emit(' = ')
					f.format_expr(init)
				}
				f.emit(',\n')
			}
			if expr.close_span.line > 0 {
				f.emit_trivia_for_span(expr.close_span)
			}
			f.indent--
			f.emit_indent()
			f.emit('}')
		}
		ast.StructInitExpression {
			f.emit(expr.identifier.name)
			f.emit('{')

			if expr.fields.len <= 2 && f.is_short_struct_init(expr) {
				f.emit(' ')
				for i, field in expr.fields {
					if i > 0 {
						f.emit(', ')
					}
					f.emit(field.identifier.name)
					f.emit(': ')
					f.format_expr(field.init)
				}
				f.emit(' }')
			} else {
				f.emit('\n')
				f.indent++
				for field in expr.fields {
					f.emit_indent()
					f.emit(field.identifier.name)
					f.emit(': ')
					f.format_expr(field.init)
					f.emit(',\n')
				}
				f.indent--
				f.emit_indent()
				f.emit('}')
			}
		}
		ast.EnumExpression {
			f.emit('enum ')
			f.emit(expr.identifier.name)
			f.emit(' {\n')
			f.indent++
			for variant in expr.variants {
				f.emit_indent()
				f.emit(variant.identifier.name)
				if payload := variant.payload {
					f.emit('(')
					f.format_type(payload)
					f.emit(')')
				}
				f.emit('\n')
			}
			if expr.close_span.line > 0 {
				f.emit_trivia_for_span(expr.close_span)
			}
			f.indent--
			f.emit_indent()
			f.emit('}')
		}
		ast.ImportDeclaration {
			f.emit("from '")
			f.emit(expr.path)
			f.emit("' import ")
			for i, spec in expr.specifiers {
				if i > 0 {
					f.emit(', ')
				}
				f.emit(spec.identifier.name)
			}
		}
		ast.ExportExpression {
			f.emit('export ')
			f.format_expr(expr.expression)
		}
		ast.AssertExpression {
			f.emit('assert ')
			f.format_expr(expr.expression)
			f.emit(', ')
			f.format_expr(expr.message)
		}
		ast.WildcardPattern {
			f.emit('else')
		}
		ast.ErrorNode {
			f.emit('/* error: ${expr.message} */')
		}
	}
}

fn (mut f Formatter) format_function(func ast.FunctionExpression) {
	f.emit('fn')
	if id := func.identifier {
		f.emit(' ')
		f.emit(id.name)
	}
	f.emit('(')
	for i, param in func.params {
		if i > 0 {
			f.emit(', ')
		}
		f.emit(param.identifier.name)
		if typ := param.typ {
			f.emit(' ')
			f.format_type(typ)
		}
	}
	f.emit(')')
	if ret := func.return_type {
		f.emit(' ')
		f.format_type(ret)
	}
	if err := func.error_type {
		f.emit('!')
		f.format_type(err)
	}
	f.emit(' ')
	f.format_block_inline(func.body)
}

fn (mut f Formatter) format_type(typ ast.TypeIdentifier) {
	if typ.is_option {
		f.emit('?')
	}
	if typ.is_array {
		f.emit('[]')
	}
	if typ.is_function {
		f.emit('fn(')
		for i, p in typ.param_types {
			if i > 0 {
				f.emit(', ')
			}
			f.format_type(p)
		}
		f.emit(')')
		if ret := typ.return_type {
			f.emit(' ')
			f.format_type(*ret)
		}
	} else {
		f.emit(typ.identifier.name)
	}
	if err := typ.error_type {
		f.emit('!')
		f.format_type(*err)
	}
}

fn (mut f Formatter) format_block_expr(block ast.BlockExpression) {
	has_close_trivia := f.has_trivia_at_span(block.close_span)
	if block.body.len == 0 && !has_close_trivia {
		f.emit('{}')
	} else if block.body.len == 1 && f.is_simple_expr(block.body[0]) && !f.has_trivia(block.body[0])
		&& !has_close_trivia {
		f.emit('{ ')
		f.format_expr(block.body[0])
		f.emit(' }')
	} else {
		f.emit('{\n')
		f.indent++
		for expr in block.body {
			span := ast.get_span(expr)
			if span.line > 0 {
				f.emit_trivia_for_span(span)
			}
			if f.at_line_start {
				f.emit_indent()
			}
			f.format_expr(expr)
			f.emit('\n')
		}
		if block.close_span.line > 0 {
			f.emit_trivia_for_span(block.close_span)
		}
		f.indent--
		f.emit_indent()
		f.emit('}')
	}
}

fn (mut f Formatter) format_block_inline(expr ast.Expression) {
	if expr is ast.BlockExpression {
		f.format_block_expr(expr)
	} else {
		f.emit('{ ')
		f.format_expr(expr)
		f.emit(' }')
	}
}

fn (f Formatter) is_simple_expr(expr ast.Expression) bool {
	return match expr {
		ast.Identifier, ast.NumberLiteral, ast.StringLiteral, ast.BooleanLiteral,
		ast.NoneExpression {
			true
		}
		ast.BinaryExpression {
			f.is_simple_expr(expr.left) && f.is_simple_expr(expr.right)
		}
		ast.UnaryExpression {
			f.is_simple_expr(expr.expression)
		}
		else {
			false
		}
	}
}

fn (f Formatter) has_trivia(expr ast.Expression) bool {
	span := ast.get_span(expr)
	return f.has_trivia_at_span(span)
}

fn (f Formatter) has_trivia_at_span(span ast.Span) bool {
	if span.line > 0 {
		key := '${span.line}:${span.column}'
		if _ := f.trivia_map[key] {
			return true
		}
	}
	return false
}

fn (f Formatter) is_short_struct_init(init ast.StructInitExpression) bool {
	for field in init.fields {
		if !f.is_simple_expr(field.init) {
			return false
		}
	}
	return true
}

fn (f Formatter) format_expr_to_string(expr ast.Expression) string {
	mut temp := Formatter{
		tokens:     f.tokens
		trivia_map: f.trivia_map
		output:     strings.new_builder(64)
		indent:     0
	}
	temp.format_expr(expr)
	return temp.output.str()
}

fn op_to_string(kind token.Kind) string {
	return match kind {
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
}

fn escape_string(s string) string {
	mut result := strings.new_builder(s.len)
	for c in s {
		match c {
			`\n` { result.write_string('\\n') }
			`\r` { result.write_string('\\r') }
			`\t` { result.write_string('\\t') }
			`\\` { result.write_string('\\\\') }
			`'` { result.write_string("\\'") }
			0 { result.write_string('\\0') }
			else { result.write_u8(c) }
		}
	}
	return result.str()
}
