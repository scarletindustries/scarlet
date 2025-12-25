module formatter

import ast
import token
import scanner
import parser
import diagnostic
import strings
import span { Span }

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
	scanned_tokens := s.scan_all()
	scanner_diagnostics := s.get_diagnostics()

	if debug {
		for tok in scanned_tokens {
			eprintln('Token: ${tok.kind} "${tok.literal or { '' }}" trivia: ${tok.leading_trivia.len}')
			for t in tok.leading_trivia {
				eprintln('  Trivia: ${t.kind} "${t.text.replace('\n', '\\n')}"')
			}
		}
	}

	mut trivia_map := map[string][]token.Trivia{}
	mut eof_trivia := []token.Trivia{}

	for tok in scanned_tokens {
		if tok.leading_trivia.len > 0 {
			key := '${tok.line}:${tok.column}'
			trivia_map[key] = tok.leading_trivia
		}

		if tok.kind == .eof {
			eof_trivia = tok.leading_trivia.clone()
		}
	}

	mut f := Formatter{
		tokens:     scanned_tokens
		trivia_map: trivia_map
		output:     strings.new_builder(input.len)
		indent:     0
	}

	mut p := parser.new_parser_from_tokens(scanned_tokens, scanner_diagnostics)
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
	f.emit_trivia(eof_trivia)

	mut final := f.output.str()

	final = final.trim_right('\n') + '\n'
	return FormatResult{
		output:      final
		has_errors:  false
		diagnostics: result.diagnostics
	}
}

struct Formatter {
	tokens     []token.Token
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

fn (mut f Formatter) emit_trivia_for_span(s Span) {
	key := '${s.start_line}:${s.start_column}'
	if trivia := f.trivia_map[key] {
		f.emit_trivia(trivia)
	}
}

fn (mut f Formatter) emit_trivia_at_span_end(s Span) {
	key := '${s.end_line}:${s.end_column - 1}'
	if trivia := f.trivia_map[key] {
		f.emit_trivia(trivia)
	}
}

fn (mut f Formatter) emit_trivia(trivia []token.Trivia) {
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

fn (mut f Formatter) format_block(block ast.BlockExpression, is_top_level bool) {
	for i, node in block.body {
		node_span := ast.node_span(node)
		if node_span.start_line > 0 {
			f.emit_trivia_for_span(node_span)
		}

		if !f.at_line_start && i > 0 {
			f.emit('\n')
		}

		if f.at_line_start && !is_top_level {
			f.emit_indent()
		} else if f.at_line_start && is_top_level && i > 0 {
		}

		f.format_node(node)

		if !f.at_line_start {
			f.emit('\n')
		}
	}
}

fn (mut f Formatter) format_node(node ast.Node) {
	match node {
		ast.Statement { f.format_statement(node) }
		ast.Expression { f.format_expr(node) }
	}
}

fn (mut f Formatter) format_statement(stmt ast.Statement) {
	match stmt {
		ast.VariableBinding {
			f.emit(stmt.identifier.name)
			if typ := stmt.typ {
				f.emit(' ')
				f.format_type(typ)
			}
			f.emit(' = ')
			f.format_expr(stmt.init)
		}
		ast.ConstBinding {
			f.emit('const ')
			f.emit(stmt.identifier.name)
			if typ := stmt.typ {
				f.emit(' ')
				f.format_type(typ)
			}
			f.emit(' = ')
			f.format_expr(stmt.init)
		}
		ast.TypePatternBinding {
			f.format_type(stmt.typ)
			f.emit(' = ')
			f.format_expr(stmt.init)
		}
		ast.FunctionDeclaration {
			f.format_function_declaration(stmt)
		}
		ast.StructDeclaration {
			f.emit('struct ')
			f.emit(stmt.identifier.name)
			f.emit(' {\n')
			f.indent++
			for field in stmt.fields {
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
			f.indent--
			f.emit_indent()
			f.emit('}')
		}
		ast.EnumDeclaration {
			f.emit('enum ')
			f.emit(stmt.identifier.name)
			f.emit(' {\n')
			f.indent++
			for variant in stmt.variants {
				f.emit_indent()
				f.emit(variant.identifier.name)
				if variant.payload.len > 0 {
					f.emit('(')
					for i, p in variant.payload {
						if i > 0 {
							f.emit(', ')
						}
						f.format_type(p)
					}
					f.emit(')')
				}
				f.emit('\n')
			}
			f.indent--
			f.emit_indent()
			f.emit('}')
		}
		ast.ImportDeclaration {
			f.emit("from '")
			f.emit(stmt.path)
			f.emit("' import ")
			for i, spec in stmt.specifiers {
				if i > 0 {
					f.emit(', ')
				}
				f.emit(spec.identifier.name)
			}
		}
		ast.ExportDeclaration {
			f.emit('export ')
			f.format_statement(stmt.declaration)
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
			if expr.span.end_line > 0 {
				f.emit_trivia_at_span_end(expr.span)
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
			f.format_function_expression(expr)
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
		ast.SpreadExpression {
			f.emit('..')
			if inner := expr.expression {
				f.format_expr(inner)
			}
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
		ast.WildcardPattern {
			f.emit('else')
		}
		ast.OrPattern {
			for i, pattern in expr.patterns {
				if i > 0 {
					f.emit(' | ')
				}
				f.format_expr(pattern)
			}
		}
		ast.ErrorNode {
			f.emit('/* error: ${expr.message} */')
		}
	}
}

fn (mut f Formatter) format_function_declaration(func ast.FunctionDeclaration) {
	f.emit('fn ')
	f.emit(func.identifier.name)
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

fn (mut f Formatter) format_function_expression(func ast.FunctionExpression) {
	f.emit('fn(')
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
		if elem := typ.element_type {
			f.format_type(*elem)
			return
		}
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
	has_close_comment := f.has_comment_trivia_at_span_end(block.span)
	if block.body.len == 0 && !has_close_comment {
		f.emit('{}')
	} else if block.body.len == 1 && f.is_simple_node(block.body[0])
		&& !f.has_comment_trivia_node(block.body[0]) && !has_close_comment {
		f.emit('{ ')
		f.format_node(block.body[0])
		f.emit(' }')
	} else {
		f.emit('{\n')
		f.indent++
		for node in block.body {
			node_span := ast.node_span(node)
			if node_span.start_line > 0 {
				f.emit_trivia_for_span(node_span)
			}
			if f.at_line_start {
				f.emit_indent()
			}
			f.format_node(node)
			f.emit('\n')
		}
		if block.span.end_line > 0 {
			f.emit_trivia_at_span_end(block.span)
		}
		f.indent--
		f.emit_indent()
		f.emit('}')
	}
}

fn (mut f Formatter) format_block_inline(expr ast.Expression) {
	if expr is ast.BlockExpression {
		f.format_block_expr(expr)
	} else if f.is_simple_expr(expr) {
		f.format_expr(expr)
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

fn (f Formatter) is_simple_node(node ast.Node) bool {
	return match node {
		ast.Statement { false }
		ast.Expression { f.is_simple_expr(node) }
	}
}

fn (f Formatter) has_trivia(expr ast.Expression) bool {
	expr_span := expr.span
	return f.has_trivia_at_span(expr_span)
}

fn (f Formatter) has_comment_trivia(expr ast.Expression) bool {
	expr_span := expr.span
	return f.has_comment_trivia_at_span(expr_span)
}

fn (f Formatter) has_comment_trivia_node(node ast.Node) bool {
	node_span := ast.node_span(node)
	return f.has_comment_trivia_at_span(node_span)
}

fn (f Formatter) has_trivia_at_span(s Span) bool {
	if s.start_line > 0 {
		key := '${s.start_line}:${s.start_column}'
		if _ := f.trivia_map[key] {
			return true
		}
	}
	return false
}

fn (f Formatter) has_comment_trivia_at_span(s Span) bool {
	if s.start_line > 0 {
		key := '${s.start_line}:${s.start_column}'
		if trivia := f.trivia_map[key] {
			for t in trivia {
				if t.kind == .line_comment {
					return true
				}
			}
		}
	}
	return false
}

fn (f Formatter) has_comment_trivia_at_span_end(s Span) bool {
	if s.end_line > 0 {
		key := '${s.end_line}:${s.end_column - 1}'
		if trivia := f.trivia_map[key] {
			for t in trivia {
				if t.kind == .line_comment {
					return true
				}
			}
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
