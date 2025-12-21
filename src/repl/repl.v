module repl

import readline
import ast
import scanner
import parser
import bytecode
import vm
import diagnostic
import types

pub fn run(version string) {
	println('al ${version} REPL')
	println("Type expressions to evaluate. Use 'exit' or Ctrl+D to quit.")
	println('')

	mut input_buffer := ''
	mut continuation := false
	mut definitions := []ast.Expression{}
	mut rl := readline.Readline{}

	for {
		prompt := if continuation { '... ' } else { '>>> ' }

		line := rl.read_line(prompt) or {
			if continuation {
				println('')
				input_buffer = ''
				continuation = false
				continue
			}
			println('')
			break
		}

		line_trimmed := line.trim_right('\r\n')

		if !continuation && line_trimmed == 'exit' {
			break
		}

		if line_trimmed.len == 0 {
			if continuation {
				if !is_input_complete(input_buffer) {
					input_buffer += '\n'
					continue
				}
				if input_buffer.trim_space().len > 0 {
					new_defs := eval_input(input_buffer, definitions)
					definitions << new_defs
				}
				input_buffer = ''
				continuation = false
			}
			continue
		}

		if continuation {
			input_buffer += '\n' + line_trimmed
		} else {
			input_buffer = line_trimmed
		}

		if is_input_complete(input_buffer) {
			if input_buffer.trim_space().len > 0 {
				new_defs := eval_input(input_buffer, definitions)
				definitions << new_defs
			}
			input_buffer = ''
			continuation = false
		} else {
			continuation = true
		}
	}
}

fn is_input_complete(input string) bool {
	mut parens := 0
	mut brackets := 0
	mut braces := 0
	mut in_string := false
	mut prev_char := u8(0)

	for c in input.bytes() {
		if c == `'` && prev_char != `\\` {
			in_string = !in_string
		}
		if !in_string {
			match c {
				`(` { parens += 1 }
				`)` { parens -= 1 }
				`[` { brackets += 1 }
				`]` { brackets -= 1 }
				`{` { braces += 1 }
				`}` { braces -= 1 }
				else {}
			}
		}
		prev_char = c
	}

	return parens <= 0 && brackets <= 0 && braces <= 0
}

fn is_definition_expr(expr ast.Expression) bool {
	return match expr {
		ast.FunctionExpression { expr.identifier != none }
		ast.StructExpression { true }
		ast.EnumExpression { true }
		ast.VariableBinding { true }
		ast.ConstBinding { true }
		else { false }
	}
}

fn has_eof_error(diagnostics []diagnostic.Diagnostic) bool {
	for d in diagnostics {
		if d.message.contains('EOF') {
			return true
		}
	}
	return false
}

fn eval_input(input string, definitions []ast.Expression) []ast.Expression {
	mut input_scanner := scanner.new_scanner(input)
	mut input_parser := parser.new_parser(mut input_scanner)
	input_parse_result := input_parser.parse_program()

	if diagnostic.has_errors(input_parse_result.diagnostics) {
		if has_eof_error(input_parse_result.diagnostics) {
			println('')
			return []
		}
		diagnostic.print_diagnostics(input_parse_result.diagnostics, input, '<repl>')
		return []
	}

	mut combined_body := []ast.Expression{}
	combined_body << definitions
	combined_body << input_parse_result.ast.body

	combined_ast := ast.BlockExpression{
		body:       combined_body
		span:       ast.Span{
			line:   1
			column: 1
		}
		close_span: ast.Span{
			line:   1
			column: 1
		}
	}

	check_result := types.check(combined_ast)

	if check_result.diagnostics.len > 0 {
		diagnostic.print_diagnostics(check_result.diagnostics, input, '<repl>')
		if !check_result.success {
			return []
		}
	}

	program := bytecode.compile(check_result.typed_ast, check_result.env) or {
		eprintln('Compile error: ${err}')
		return []
	}

	mut v := vm.new_vm(program)
	run_result := v.run() or {
		eprintln('Runtime error: ${err}')
		return []
	}

	println(vm.inspect(run_result))

	mut new_definitions := []ast.Expression{}
	for expr in input_parse_result.ast.body {
		if is_definition_expr(expr) {
			new_definitions << expr
		}
	}

	return new_definitions
}
