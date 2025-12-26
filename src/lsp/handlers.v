module lsp

import x.json2

struct InitializeResult {
	capabilities ServerCapabilities
}

struct ServerCapabilities {
	text_document_sync   int  = 1    @[json: 'textDocumentSync']
	hover_provider       bool = true @[json: 'hoverProvider']
	definition_provider  bool = true @[json: 'definitionProvider']
}

struct HoverResult {
	contents MarkupContent
}

fn clean_doc_comment(doc string) string {
	content := doc.trim_space().trim_left('/*').trim_right('*/').trim_space()
	
	lines := content.split('\n')
	mut cleaned := []string{}
	
	for line in lines {
		trimmed := line.trim_left(' \t')
		if trimmed.starts_with('* ') {
			cleaned << trimmed[2..]
		} else if trimmed.starts_with('*') {
			cleaned << trimmed[1..].trim_left(' ')
		} else {
			cleaned << trimmed
		}
	}
	
	return cleaned.join('\n\n')
}

fn (mut s LspServer) handle_initialize(id json2.Any) {
	s.send_response(id, InitializeResult{})
}

fn (mut s LspServer) handle_shutdown(id json2.Any) {
	s.send_null_response(id)
}

fn (mut s LspServer) handle_did_open(params json2.Any) {
	obj := params.as_map()
	text_doc := obj['textDocument'] or { return }.as_map()
	uri := text_doc['uri'] or { return }.str()
	text := text_doc['text'] or { return }.str()

	s.documents[uri] = text
	s.analyze_document(uri, text)
}

fn (mut s LspServer) handle_did_change(params json2.Any) {
	obj := params.as_map()
	text_doc := obj['textDocument'] or { return }.as_map()
	uri := text_doc['uri'] or { return }.str()
	changes := obj['contentChanges'] or { return }.arr()

	if changes.len > 0 {
		last_change := changes[changes.len - 1].as_map()
		text := last_change['text'] or { return }.str()
		s.documents[uri] = text
		s.analyze_document(uri, text)
	}
}

fn (mut s LspServer) handle_did_close(params json2.Any) {
	obj := params.as_map()
	text_doc := obj['textDocument'] or { return }.as_map()
	uri := text_doc['uri'] or { return }.str()

	s.documents.delete(uri)
	s.type_info.delete(uri)
}

fn (mut s LspServer) handle_hover(id json2.Any, params json2.Any) {
	obj := params.as_map()
	text_doc := obj['textDocument'] or {
		s.send_null_response(id)
		return
	}.as_map()
	pos := obj['position'] or {
		s.send_null_response(id)
		return
	}.as_map()

	uri := text_doc['uri'] or {
		s.send_null_response(id)
		return
	}.str()
	line := pos['line'] or {
		s.send_null_response(id)
		return
	}.int()
	col := pos['character'] or {
		s.send_null_response(id)
		return
	}.int()

	if types := s.type_info[uri] {
		for t in types {
			if t.line == line && col >= t.col_start && col < t.col_end {
				type_sig := '${t.name}: ${t.type_str}'
				mut value := '```al\n${type_sig}\n```'
				if doc := t.doc {
					clean_doc := clean_doc_comment(doc)
					value = value + '\n\n---\n\n' + clean_doc
				}
				s.send_response(id, HoverResult{
					contents: MarkupContent{
						kind:  'markdown'
						value: value
					}
				})
				return
			}
		}
	}

	s.send_null_response(id)
}

fn (mut s LspServer) handle_definition(id json2.Any, params json2.Any) {
	obj := params.as_map()

	text_doc := obj['textDocument'] or {
		s.send_null_response(id)
		return
	}.as_map()

	pos := obj['position'] or {
		s.send_null_response(id)
		return
	}.as_map()

	uri := text_doc['uri'] or {
		s.send_null_response(id)
		return
	}.str()

	line := pos['line'] or {
		s.send_null_response(id)
		return
	}.int()

	col := pos['character'] or {
		s.send_null_response(id)
		return
	}.int()

	if types := s.type_info[uri] {
		for t in types {
			if t.line == line && col >= t.col_start && col < t.col_end {
				if t.def_line >= 0 && t.def_col >= 0 {
					s.send_response(id, Location{
						uri:   uri
						range: Range{
							start: Position{line: t.def_line, character: t.def_col}
							end:   Position{line: t.def_line, character: t.def_end}
						}
					})
					return
				}
			}
		}
	}

	s.send_null_response(id)
}
