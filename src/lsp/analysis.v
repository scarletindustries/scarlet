module lsp

import scanner
import parser
import types
import type_def
import diagnostic
import json

fn (mut s LspServer) analyze_document(uri string, text string) {
	s.log('Analyzing: ${uri}')

	mut sc := scanner.new_scanner(text)
	mut p := parser.new_parser(mut sc)
	parse_result := p.parse_program()

	mut lsp_diagnostics := []Diagnostic{}

	for diag in parse_result.diagnostics {
		lsp_diagnostics << Diagnostic{
			range:    Range{
				start: Position{
					line:      diag.span.start_line
					character: diag.span.start_column
				}
				end:   Position{
					line:      diag.span.end_line
					character: diag.span.end_column
				}
			}
			severity: if diag.severity == .error { 1 } else { 2 }
			message:  diag.message
		}
	}

	has_errors := diagnostic.has_errors(parse_result.diagnostics)
	if !has_errors {
		check_result := types.check(parse_result.ast)

		for diag in check_result.diagnostics {
			lsp_diagnostics << Diagnostic{
				range:    Range{
					start: Position{
						line:      diag.span.start_line
						character: diag.span.start_column
					}
					end:   Position{
						line:      diag.span.end_line
						character: diag.span.end_column
					}
				}
				severity: if diag.severity == .error { 1 } else { 2 }
				message:  diag.message
			}
		}

		s.type_info[uri] = extract_types(check_result)
	}

	diag_json := json.encode(lsp_diagnostics)
	params := '{"uri":"${uri}","diagnostics":${diag_json}}'
	s.send_notification('textDocument/publishDiagnostics', params)
}

fn extract_types(check_result types.CheckResult) []TypeAtPosition {
	mut result := []TypeAtPosition{}

	for tp in check_result.type_positions {
		result << TypeAtPosition{
			line:      tp.line
			col_start: tp.column
			col_end:   tp.end_col
			name:      tp.name
			type_str:  type_def.type_to_string(tp.type_info)
			def_line:  tp.def_line
			def_col:   tp.def_col
			def_end:   tp.def_end
			doc:       tp.doc
		}
	}

	return result
}
