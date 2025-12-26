module lsp

import os
import x.json2
import json

pub struct LspServer {
mut:
	running   bool
	documents map[string]string           // uri -> content
	type_info map[string][]TypeAtPosition // uri -> types at positions
}

pub struct TypeAtPosition {
pub:
	line      int
	col_start int
	col_end   int
	type_str  string
	name      string
	def_line  int // definition location (0 if unknown)
	def_col   int
	def_end   int
	doc       ?string
}

pub struct Position {
pub:
	line      int
	character int
}

pub struct Range {
pub:
	start Position
	end   Position
}

pub struct Location {
pub:
	uri   string
	range Range
}

pub struct Diagnostic {
pub:
	range    Range
	severity int = 1
	message  string
}

pub struct MarkupContent {
pub:
	kind  string
	value string
}

pub fn new_server() &LspServer {
	return &LspServer{
		running:   true
		documents: map[string]string{}
		type_info: map[string][]TypeAtPosition{}
	}
}

pub fn (mut s LspServer) run() {
	for s.running {
		content := s.read_message() or {
			s.log('Failed to read message: ${err}')
			continue
		}

		if content.len == 0 {
			continue
		}

		s.handle_message(content)
	}
}

fn (mut s LspServer) read_message() !string {
	mut content_length := 0

	for {
		line := os.get_raw_line()
		if line.len == 0 {
			s.running = false
			return ''
		}

		trimmed := line.trim_space()
		if trimmed.len == 0 {
			break
		}

		if trimmed.starts_with('Content-Length:') {
			length_str := trimmed.all_after(':').trim_space()
			content_length = length_str.int()
		}
	}

	if content_length == 0 {
		return ''
	}

	mut content := []u8{len: content_length}
	mut read := 0
	for read < content_length {
		c := os.stdin().read_bytes(content_length - read)
		for i, b in c {
			content[read + i] = b
		}
		read += c.len
		if c.len == 0 {
			break
		}
	}

	return content.bytestr()
}

fn (mut s LspServer) handle_message(content string) {
	raw := json2.decode[json2.Any](content) or {
		s.log('Failed to parse JSON: ${err}')
		return
	}

	obj := raw.as_map()
	method := obj['method'] or { json2.Any('') }.str()
	id := obj['id'] or { json2.Any(json2.Null{}) }
	params := obj['params'] or { json2.Any(json2.Null{}) }

	s.log('Received: ${method}')

	match method {
		'initialize' {
			s.handle_initialize(id)
		}
		'initialized', '$/setTrace', '$/cancelRequest' {
			// Notifications, no response needed
		}
		'shutdown' {
			s.handle_shutdown(id)
		}
		'exit' {
			s.running = false
		}
		'textDocument/didOpen' {
			s.handle_did_open(params)
		}
		'textDocument/didChange' {
			s.handle_did_change(params)
		}
		'textDocument/didClose' {
			s.handle_did_close(params)
		}
		'textDocument/hover' {
			s.handle_hover(id, params)
		}
		'textDocument/definition' {
			s.handle_definition(id, params)
		}
		else {
			s.log('Unknown method: ${method}')
		}
	}
}

struct JsonRpcResponse[T] {
	jsonrpc string = '2.0'
	id      int
	result  T
}

struct JsonRpcNullResponse {
	jsonrpc string = '2.0'
	id      int
	result  json2.Null
}

struct JsonRpcNotification[T] {
	jsonrpc string = '2.0'
	method  string
	params  T
}

fn (mut s LspServer) send_response[T](id json2.Any, result T) {
	response := JsonRpcResponse[T]{
		id:     id.int()
		result: result
	}
	s.send_message(json.encode(response))
}

fn (mut s LspServer) send_null_response(id json2.Any) {
	response := JsonRpcNullResponse{
		id: id.int()
	}
	s.send_message(json.encode(response))
}

fn (mut s LspServer) send_notification[T](method string, params T) {
	notification := JsonRpcNotification[T]{
		method: method
		params: params
	}
	s.send_message(json.encode(notification))
}

fn (s LspServer) send_message(content string) {
	header := 'Content-Length: ${content.len}\r\n\r\n'
	print(header)
	print(content)
	os.flush()
}

fn (s LspServer) log(msg string) {
	eprintln('[AL LSP] ${msg}')
}
