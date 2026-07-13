// An HTTP/1.1 server on al/http. A handler is a plain fn(Request) Response:
// the library owns the parsing, keep-alive, pipelining and body framing, and
// runs each connection in its own process.
//
// Run it, then in another terminal:
//   curl localhost:8080/
//   curl 'localhost:8080/hello?name=al'
//   curl -d 'ledger v1' localhost:8080/echo
//   curl -i localhost:8080/nope

import al/binary
import al/http.{Fixed, Get, Post, Request, Response}
import al/http/body
import al/http/headers.{Header}

// Largest body this service will read. body.collect stops at the cap instead
// of letting a client stream memory away from us.
const MAX_BODY = 8192

const NAME_CONTENT_TYPE = <<'Content-Type'>>
const NAME_SERVER = <<'Server'>>
const VAL_TEXT = <<'text/plain; charset=utf-8'>>
const VAL_AL = <<'al'>>

// A response whose length is pinned by its type: Fixed(len, body) promises
// exactly len bytes, and the driver emits the Content-Length header from it —
// a handler cannot advertise one length and write another.
fn respond(code Int, s String) Response {
	bin = binary.from_string(s)
	Response(
		status: code,
		headers: [Header(name: NAME_CONTENT_TYPE, value: VAL_TEXT)],
		body: Fixed(binary.byte_size(bin), body.from_binary(bin)),
	)
}

// http.query is the part of the target after `?`, as bytes: `name=al` for
// /hello?name=al. Everything the parser hands a handler is a zero-copy slice
// of the bytes that came off the socket.
fn hello(req Request) Response {
	match binary.to_string(http.query(req)) {
		Ok('') -> respond(200, 'hello, stranger\n')
		Ok(q) -> respond(200, 'hello, ${q}\n')
		Err(Nil) -> respond(400, 'query is not utf-8\n')
	}
}

// A request body arrives as a pull stream; body.collect buffers it, bounded.
fn echo(req Request) Response {
	match body.collect(req.body, MAX_BODY) {
		Ok(bytes) -> match binary.to_string(bytes) {
			Ok(text) -> respond(200, 'echo: ${text}\n')
			Err(Nil) -> respond(400, 'body is not utf-8\n')
		}
		Err(e) -> respond(400, 'read failed: ${e}\n')
	}
}

// Route on the method and the path, both matched as values: the method is a
// Method enum, the path a Binary matched against byte literals.
fn route(req Request) Response {
	response = match req.method {
		Get -> match http.path(req) {
			<<'/'>> -> respond(200, 'al http server\n')
			<<'/hello'>> -> hello(req)
			else -> http.not_found()
		}
		Post -> match http.path(req) {
			<<'/echo'>> -> echo(req)
			else -> http.not_found()
		}
		else -> respond(405, 'method not allowed\n')
	}
	http.with_header(response, NAME_SERVER, VAL_AL)
}

// serve binds the port and runs an acceptor on every core, all draining the
// one listener. It returns as soon as the acceptors are running; they keep the
// program alive. examples/http_client.al is the other half of this: a client,
// and a server it drives to completion, in one process.
match http.serve('0.0.0.0', 8080, route) {
	Ok(_) -> println('Listening on http://localhost:8080')
	Err(e) -> println('serve failed: ${e}')
}
