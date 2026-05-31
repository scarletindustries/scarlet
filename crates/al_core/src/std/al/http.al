import al/http/h1.{Done, NeedMore, Bad, NoBody, Length, Invalid}
import al/http/body.{Body}
import al/http/headers.{Header, Headers}
import al/net.{Server}
import al/net/socket.{Socket}
import al/binary
import al/int
import al/experiments/scheduler

// The HTTP/1.1 server core: the typed Request/Response surface a handler sees,
// and the connection driver that frames requests off a socket and drives
// responses back. The sans-IO parsing/framing lives in al/http/h1 (imported
// one-way; h1 never imports this module, which keeps the layering acyclic);
// the streaming body primitive lives in al/http/body.
//
// Request bodies reach the handler under ONE delivery model, chosen to make
// keep-alive safe against request smuggling: the driver reads the whole
// Content-Length-framed body into a bounded buffer (capped at MAX_BODY) BEFORE
// invoking the handler, and only reuses the connection once that body is fully
// accounted for. A handler therefore never holds a half-consumed socket
// cursor, so leftover body bytes can never be misframed as the next pipelined
// request. An over-cap body is refused with 413 before a byte is read.

pub type Method {
	Get
	Post
	Put
	Delete
	Patch
	Head
	Options
	Other(name Binary)
}

pub type Version {
	Http10
	Http11
	Http2
}

pub type Request {
	method Method
	path Binary
	version Version
	headers Headers
	body Body
}

pub type Response {
	status Int
	headers Headers
	body Body
}

// Largest request body the default buffered path will accept. A Content-Length
// above this is rejected with 413 before any of the body is read — the
// mandatory denial-of-service bound (a dribbled multi-gigabyte upload can
// neither exhaust memory nor pin the connection).
const MAX_BODY = 1048576
const READ_SIZE = 65536
const EMPTY = binary.from_string('')

const M_GET = binary.from_string('GET')
const M_POST = binary.from_string('POST')
const M_PUT = binary.from_string('PUT')
const M_DELETE = binary.from_string('DELETE')
const M_PATCH = binary.from_string('PATCH')
const M_HEAD = binary.from_string('HEAD')
const M_OPTIONS = binary.from_string('OPTIONS')

const NAME_CONTENT_TYPE = binary.from_string('Content-Type')
const NAME_CONTENT_LENGTH = binary.from_string('Content-Length')
const NAME_CONNECTION = binary.from_string('Connection')
const VAL_TEXT_PLAIN = binary.from_string('text/plain; charset=utf-8')
const VAL_CLOSE = binary.from_string('close')
const ZERO = binary.from_string('0')

fn to_method(m Binary) Method {
	if m == M_GET {
		Get
	} else if m == M_POST {
		Post
	} else if m == M_PUT {
		Put
	} else if m == M_DELETE {
		Delete
	} else if m == M_PATCH {
		Patch
	} else if m == M_HEAD {
		Head
	} else if m == M_OPTIONS {
		Options
	} else {
		Other(m)
	}
}

fn to_version(minor Int) Version {
	if minor == 0 { Http10 } else { Http11 }
}

// A 200 response whose body is supplied directly as a pull thunk. The thunk
// carries no length, so on HTTP/1.1 the body can only be framed by closing the
// connection after it: without that, a keep-alive client cannot tell where this
// body ends and the next response begins. So this sets `Connection: close` and
// is the H1 streaming escape hatch. A handler that knows its length should send
// a Content-Length response instead (e.g. `text`), which keeps the connection
// alive.
pub fn ok(b Body) Response {
	Response(status: 200, headers: [Header(name: NAME_CONNECTION, value: VAL_CLOSE)], body: b)
}

pub fn not_found() Response {
	text_response(404, 'Not Found')
}

// A text/plain response with the body buffered and Content-Length set.
pub fn text(s String) Response {
	text_response(200, s)
}

fn text_response(code Int, s String) Response {
	bin = binary.from_string(s)
	len = binary.from_int_ascii(binary.byte_size(bin), 10)
	hdrs = headers.set(headers.set([], NAME_CONTENT_TYPE, VAL_TEXT_PLAIN), NAME_CONTENT_LENGTH, len)
	Response(status: code, headers: hdrs, body: body.from_binary(bin))
}

// Set (replace-or-append) a header on a response.
pub fn with_header(r Response, name Binary, value Binary) Response {
	Response(status: r.status, headers: headers.set(r.headers, name, value), body: r.body)
}

fn build_request(method Binary, target Binary, version Int, hdrs Headers, b Body) Request {
	Request(
		method: to_method(method),
		path: target,
		version: to_version(version),
		headers: hdrs,
		body: b,
	)
}

// Listen on host:port and serve each accepted connection on its own process.
// One slow or hostile connection parks only its own process; the runtime
// spreads connection processes across every core.
pub fn serve(host String, port Int, handler fn(Request) Response) Result(Nil, String) {
	match net.listen(host, port) {
		Err(e) -> Err(e)
		Ok(server) -> accept_loop(server, handler)
	}
}

fn accept_loop(server Server, handler fn(Request) Response) Result(Nil, String) {
	match net.accept(server) {
		Ok(sock) -> {
			scheduler.spawn(fn() drive(sock, handler))
			accept_loop(server, handler)
		}
		Err(e) -> Err(e)
	}
}

// Own one connection start-to-finish and always close it on the way out. The
// loop itself never closes the socket: it returns to here, which closes once.
fn drive(sock Socket, handler fn(Request) Response) Nil {
	serve_conn(sock, EMPTY, 0, handler) or Nil
	socket.close(sock) or Nil
}

// The keep-alive / pipelining loop. `buf`/`off` carry the bytes already read
// but not yet consumed (the start of the next request when pipelined).
// Returning Ok(Nil) stops the loop so `drive` closes; recursing keeps the
// connection alive. The connection is only ever reused from the one place
// below where the previous request's body has been fully read.
fn serve_conn(sock Socket, buf Binary, off Int, handler fn(Request) Response) Result(Nil, String) {
	match h1.parse_request(buf, off) {
		NeedMore -> match socket.read(sock, READ_SIZE) {
			Ok(more) -> if binary.byte_size(more) == 0 {
				Ok(Nil)
			} else {
				serve_conn(sock, carry(buf, off, more), 0, handler)
			}
			Err(e) -> Err(e)
		}
		Bad(status) -> {
			write_error(sock, status)
			Ok(Nil)
		}
		Done(method, target, version, hdrs, consumed) -> handle(
			sock,
			buf,
			consumed,
			method,
			target,
			version,
			hdrs,
			handler,
		)
	}
}

// Concatenate the unconsumed tail of `buf` (from `off`) with freshly read
// bytes, resetting the parse offset to 0.
fn carry(buf Binary, off Int, more Binary) Binary {
	binary.append(binary.slice_bytes(buf, off, binary.byte_size(buf) - off), more)
}

// Decide framing, then deliver the body under the bounded-buffer model.
fn handle(
	sock Socket,
	buf Binary,
	consumed Int,
	method Binary,
	target Binary,
	version Int,
	hdrs Headers,
	handler fn(Request) Response,
) Result(Nil, String) {
	match h1.framing(hdrs) {
		Invalid(status) -> {
			write_error(sock, status)
			Ok(Nil)
		}
		NoBody -> respond_and_continue(
			sock,
			buf,
			consumed,
			version,
			hdrs,
			build_request(method, target, version, hdrs, body.empty()),
			handler,
		)
		Length(n) -> if n > MAX_BODY {
			write_error(sock, 413)
			Ok(Nil)
		} else {
			read_body(sock, buf, consumed, n, method, target, version, hdrs, handler)
		}
	}
}

// Read the full Content-Length body into one bounded buffer, then dispatch.
// Body bytes that already arrived in `buf` (past `consumed`) are taken from
// there; the rest is streamed off the socket through the same Content-Length
// reader and bounded `collect` (the only buffering path, capped at MAX_BODY).
// Whatever follows the body in `buf` is the next pipelined request and is
// carried forward; if the body had to be read from the socket there is no such
// leftover and the next request is read fresh.
fn read_body(
	sock Socket,
	buf Binary,
	consumed Int,
	n Int,
	method Binary,
	target Binary,
	version Int,
	hdrs Headers,
	handler fn(Request) Response,
) Result(Nil, String) {
	match maybe_continue(sock, hdrs) {
		Err(e) -> Err(e)
		Ok(_) -> {
			avail = binary.byte_size(buf) - consumed
			buffered = int.min(avail, n)
			head_bytes = binary.slice_bytes(buf, consumed, buffered)
			need = n - buffered
			match body.collect(h1.content_length_reader(sock, need), MAX_BODY) {
				Err(e) -> Err(e)
				Ok(tail) -> respond_and_continue(
					sock,
					if avail > n { buf } else { EMPTY },
					if avail > n {
						consumed + n
					} else {
						0
					},
					version,
					hdrs,
					build_request(
						method,
						target,
						version,
						hdrs,
						body.from_binary(binary.append(head_bytes, tail)),
					),
					handler,
				)
			}
		}
	}
}

// Honor Expect: 100-continue by writing the interim response before the body
// is read, so a client that waits for it will start sending.
fn maybe_continue(sock Socket, hdrs Headers) Result(Nil, String) {
	if h1.want_100_continue(hdrs) {
		socket.write_parts(sock, h1.serialize_head(100, []))
	} else {
		Ok(Nil)
	}
}

// Run the handler, write the response, then decide keep-alive vs close. This
// is the ONLY place the connection is reused, and it is reached only after the
// request body was fully read above — so the socket sits exactly at the next
// request boundary and there is no unconsumed body to be misframed.
fn respond_and_continue(
	sock Socket,
	leftover_buf Binary,
	leftover_off Int,
	version Int,
	hdrs Headers,
	req Request,
	handler fn(Request) Response,
) Result(Nil, String) {
	resp = handler(req)
	match respond(sock, req.method, resp) {
		Err(e) -> Err(e)
		Ok(_) -> if h1.should_close(version, hdrs) || response_close(resp) {
			Ok(Nil)
		} else {
			serve_conn(sock, leftover_buf, leftover_off, handler)
		}
	}
}

// Write the response head, then drain its body — unless the request method or
// the status forbids a body, in which case the head is written and the body is
// skipped. The head still advertises whatever Content-Length the handler set (a
// HEAD response reports the length its GET would have had), so declining to
// drain here is exactly what keeps a kept-alive connection in frame: those body
// bytes would otherwise be read by the client as the start of the next response.
fn respond(sock Socket, method Method, resp Response) Result(Nil, String) {
	match socket.write_parts(sock, h1.serialize_head(resp.status, resp.headers)) {
		Err(e) -> Err(e)
		Ok(_) -> if suppress_body(method, resp.status) {
			Ok(Nil)
		} else {
			match body.drain(resp.body, sock) {
				Ok(_) -> Ok(Nil)
				Err(e) -> Err(e)
			}
		}
	}
}

// RFC 9110: a HEAD response, and any 1xx/204/304 response, carries headers but
// MUST NOT carry a body (1xx is 100..199).
fn suppress_body(method Method, status Int) Bool {
	match method {
		Head -> True
		else -> status >= 100 && status < 200 || status == 204 || status == 304
	}
}

// Whether the handler's response asked to close the connection. The streaming
// `ok` escape hatch sets `Connection: close` because its body has no
// Content-Length and so can only be framed by close on HTTP/1.1; honoring it
// here, alongside the request-side should_close, stops an unframed streaming
// response from desyncing a kept-alive connection.
fn response_close(resp Response) Bool {
	match headers.get(resp.headers, NAME_CONNECTION) {
		None -> False
		Some(v) -> binary.eq_ignore_ascii_case(v, VAL_CLOSE)
	}
}

// Write a minimal framed error response (Content-Length: 0, Connection: close)
// so the client sees the status; the caller then stops the loop and closes.
fn write_error(sock Socket, status Int) Nil {
	hdrs = [
		Header(name: NAME_CONTENT_LENGTH, value: ZERO),
		Header(name: NAME_CONNECTION, value: VAL_CLOSE),
	]
	socket.write_parts(sock, h1.serialize_head(status, hdrs)) or Nil
}
