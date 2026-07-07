import al/http/h1.{
	Done,
	NeedMore,
	Bad,
	NoBody,
	Length,
	Chunked,
	Invalid,
	ChunkedDone,
	ChunkedNeedMore,
	ChunkedBad,
	Version,
	Http10,
	Http11,
}
import al/http/body.{Body, Empty, Whole, Streaming}
import al/http/headers.{Header, Headers}
import al/net.{Server}
import al/net/socket.{Socket}
import al/net/error.{NetError}
import al/binary
import al/int

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

pub type Request {
	method Method
	path Binary
	version Version
	headers Headers
	// Trailer fields received after a chunked body. Kept separate from
	// `headers`: RFC 9110 §6.5.1 forbids merging trailers into the header
	// section for message-control decisions (Connection, Expect), so the
	// keep-alive check reads only `headers`.
	trailers Headers
	body Body
}

pub type Response {
	status Int
	headers Headers
	body Body
}

// The outcome of writing one response: whether the connection may stay alive,
// and if so, the batch of bytes still queued for the next vectored write.
type Sent {
	KeepAlive(pending Array(Binary))
	Close
}

// Largest request body the default buffered path will accept. A Content-Length
// above this is rejected with 413 before any of the body is read — the
// mandatory denial-of-service bound (a dribbled multi-gigabyte upload can
// neither exhaust memory nor pin the connection).
const MAX_BODY = 1048576
const READ_SIZE = 65536
const EMPTY = <<>>

const NAME_CONTENT_TYPE = <<'Content-Type'>>
const NAME_CONTENT_LENGTH = <<'Content-Length'>>
const NAME_CONNECTION = <<'Connection'>>
const VAL_TEXT_PLAIN = <<'text/plain; charset=utf-8'>>
const VAL_CLOSE = <<'close'>>
const ZERO = <<'0'>>
const NAME_TRANSFER_ENCODING = <<'Transfer-Encoding'>>
const VAL_CHUNKED = <<'chunked'>>

// The wire method name, matched as bytes. Each arm is a single prefix compare
// against the parser's zero-copy method view — no per-method constants and no
// allocation.
fn to_method(m Binary) Method {
	match m {
		<<'GET'>> -> Get
		<<'POST'>> -> Post
		<<'PUT'>> -> Put
		<<'DELETE'>> -> Delete
		<<'PATCH'>> -> Patch
		<<'HEAD'>> -> Head
		<<'OPTIONS'>> -> Options
		else -> Other(m)
	}
}

// A 200 response whose body is supplied directly as a pull thunk of unknown
// length. It sets NO framing header: the connection driver frames it at send
// time from the request's HTTP version — Transfer-Encoding: chunked on
// HTTP/1.1 (so the connection stays alive), or connection-close on HTTP/1.0
// (which has no chunked encoding). A handler that knows its length should send
// a Content-Length response instead (e.g. `text`); both keep the connection
// alive.
pub fn ok(b Body) Response {
	Response(status: 200, headers: [], body: b)
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
	// Built as a literal: these two names cannot collide, so there is nothing
	// for headers.set's replace-or-append walk to do.
	hdrs = [
		Header(name: NAME_CONTENT_TYPE, value: VAL_TEXT_PLAIN),
		Header(name: NAME_CONTENT_LENGTH, value: len),
	]
	Response(status: code, headers: hdrs, body: body.from_binary(bin))
}

// Set (replace-or-append) a header on a response.
pub fn with_header(r Response, name Binary, value Binary) Response {
	Response(status: r.status, headers: headers.set(r.headers, name, value), body: r.body)
}

fn build_request(
	method Binary,
	target Binary,
	version Version,
	hdrs Headers,
	trailers Headers,
	b Body,
) Request {
	Request(
		method: to_method(method),
		path: target,
		version: version,
		headers: hdrs,
		trailers: trailers,
		body: b,
	)
}

// Listen on host:port and serve each accepted connection on its own process.
// One slow or hostile connection parks only its own process. The accept loop
// runs on every core in parallel (net.serve), each core binding its own
// SO_REUSEPORT socket, and each connection is handled on the core that
// accepted it.
pub fn serve(host String, port Int, handler fn(Request) Response) Result(Nil, NetError) {
	net.serve(host, port, fn(sock) drive(sock, handler))
}

// Serve connections from a listener the caller already bound. This is the
// entry point when the caller needs the bound address before serving — e.g.
// listen on port 0, read the kernel-assigned port back with net.local_addr,
// then hand the listener over. `serve` is the listen-and-serve convenience on
// top of this.
pub fn serve_on(server Server, handler fn(Request) Response) Result(Nil, NetError) {
	net.serve_on(server, fn(sock) drive(sock, handler))
}

// Own one connection start-to-finish and always close it on the way out. The
// loop itself never closes the socket: it returns to here, which closes once.
fn drive(sock Socket, handler fn(Request) Response) Nil {
	serve_conn(sock, EMPTY, 0, handler, []) or Nil
	socket.close(sock) or Nil
}

// The keep-alive / pipelining loop. `buf`/`off` carry the bytes already read
// but not yet consumed (the start of the next request when pipelined).
// `pending` carries the responses to requests already handled but not yet
// written: they accumulate while complete pipelined requests keep coming out
// of `buf`, and the whole batch goes to the kernel as ONE vectored write at
// the next read boundary — N pipelined requests cost one read and one write,
// not one write each.
// Returning Ok(Nil) stops the loop so `drive` closes; recursing keeps the
// connection alive. The connection is only ever reused from the one place
// below where the previous request's body has been fully read.
fn serve_conn(
	sock Socket,
	buf Binary,
	off Int,
	handler fn(Request) Response,
	pending Array(Binary),
) Result(Nil, NetError) {
	match h1.parse_request(buf, off) {
		NeedMore -> match flush(sock, pending) {
			Err(e) -> Err(e)
			Ok(_) -> match socket.read(sock, READ_SIZE) {
				Ok(more) -> if binary.byte_size(more) == 0 {
					Ok(Nil)
				} else {
					serve_conn(sock, carry(buf, off, more), 0, handler, [])
				}
				Err(e) -> Err(e)
			}
		}
		Bad(status) -> {
			flush(sock, pending) or Nil
			write_error(sock, status)
			Ok(Nil)
		}
		Done(method, target, version, hdrs, consumed) ->
			handle(sock, buf, consumed, method, target, version, hdrs, handler, pending)
	}
}

// Write out accumulated response parts, if any, as one vectored write.
fn flush(sock Socket, pending Array(Binary)) Result(Nil, NetError) {
	match pending {
		[] -> Ok(Nil)
		else -> match socket.write_parts(sock, pending) {
			Ok(_) -> Ok(Nil)
			Err(e) -> Err(e)
		}
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
	version Version,
	hdrs Headers,
	handler fn(Request) Response,
	pending Array(Binary),
) Result(Nil, NetError) {
	match h1.framing(hdrs) {
		Invalid(status) -> {
			flush(sock, pending) or Nil
			write_error(sock, status)
			Ok(Nil)
		}
		NoBody ->
			respond_and_continue(
				sock,
				buf,
				consumed,
				build_request(method, target, version, hdrs, [], body.empty()),
				handler,
				pending,
			)
		Length(n) -> if n > MAX_BODY {
			flush(sock, pending) or Nil
			write_error(sock, 413)
			Ok(Nil)
		} else {
			// The body is read off the socket: anything still pending must go
			// out first, or the client could sit waiting for those responses
			// before it sends the body we are about to wait for.
			match flush(sock, pending) {
				Err(e) -> Err(e)
				Ok(_) -> read_body(sock, buf, consumed, n, method, target, version, hdrs, handler)
			}
		}
		// Chunked body: same flush-first reasoning as Content-Length.
		Chunked -> match flush(sock, pending) {
			Err(e) -> Err(e)
			Ok(_) -> read_chunked_body(sock, buf, consumed, method, target, version, hdrs, handler)
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
	version Version,
	hdrs Headers,
	handler fn(Request) Response,
) Result(Nil, NetError) {
	match maybe_continue(sock, hdrs) {
		Err(e) -> Err(e)
		Ok(_) -> {
			avail = binary.byte_size(buf) - consumed
			buffered = int.min(avail, n)
			head_bytes = binary.slice_bytes(buf, consumed, buffered)
			need = n - buffered
			match body.collect(body.content_length(sock, need), MAX_BODY) {
				Err(e) -> Err(e)
				// Pending responses were flushed before the body was read, so
				// this request starts a fresh batch.
				Ok(tail) -> respond_and_continue(
					sock,
					if avail > n { buf } else { EMPTY },
					if avail > n {
						consumed + n
					} else {
						0
					},
					build_request(
						method,
						target,
						version,
						hdrs,
						[],
						body.from_binary(binary.append(head_bytes, tail)),
					),
					handler,
					[],
				)
			}
		}
	}
}

// Read and decode a chunked request body into one bounded buffer, then
// dispatch. The 100-continue interim response (if asked for) goes out once,
// before any body bytes are awaited.
fn read_chunked_body(
	sock Socket,
	buf Binary,
	off Int,
	method Binary,
	target Binary,
	version Version,
	hdrs Headers,
	handler fn(Request) Response,
) Result(Nil, NetError) {
	match maybe_continue(sock, hdrs) {
		Err(e) -> Err(e)
		Ok(_) -> chunked_loop(sock, buf, off, method, target, version, hdrs, handler)
	}
}

// The chunked read/decode loop. The native decoder (h1.chunk_decode) scans
// whatever is buffered so far; this loop owns the I/O decision —
// ChunkedNeedMore means read more off the socket (parking on backpressure)
// and retry from the same offset, exactly like serve_conn's head-parsing
// loop. The decoder caps the decoded size at MAX_BODY (413), so memory stays
// bounded no matter what the wire claims, and `consumed` points at the first
// byte after the terminator so pipelined requests carry forward through
// respond_and_continue unchanged.
fn chunked_loop(
	sock Socket,
	buf Binary,
	off Int,
	method Binary,
	target Binary,
	version Version,
	hdrs Headers,
	handler fn(Request) Response,
) Result(Nil, NetError) {
	match h1.chunk_decode(buf, off, MAX_BODY) {
		ChunkedNeedMore -> match socket.read(sock, READ_SIZE) {
			Ok(more) -> if binary.byte_size(more) == 0 {
				// Peer closed mid-body: there is no complete request to answer.
				Ok(Nil)
			} else {
				chunked_loop(
					sock,
					binary.append(buf, more),
					off,
					method,
					target,
					version,
					hdrs,
					handler,
				)
			}
			Err(e) -> Err(e)
		}
		ChunkedBad(status) -> {
			write_error(sock, status)
			Ok(Nil)
		}
		// Pending responses were flushed before the body was read, so this
		// request starts a fresh batch.
		ChunkedDone(decoded, trailers, consumed) ->
			respond_and_continue(
				sock,
				buf,
				consumed,
				build_request(
					method,
					target,
					version,
					hdrs,
					trailers,
					body.from_binary(decoded),
				),
				handler,
				[],
			)
	}
}

// Honor Expect: 100-continue by writing the interim response before the body
// is read, so a client that waits for it will start sending.
fn maybe_continue(sock Socket, hdrs Headers) Result(Nil, NetError) {
	if h1.want_100_continue(hdrs) {
		socket.write(sock, h1.serialize_head(100, []))
	} else {
		Ok(Nil)
	}
}

// Run the handler, hand the response to `respond`, then decide keep-alive vs
// close. This is the ONLY place the connection is reused, and it is reached
// only after the request body was fully read above — so the socket sits
// exactly at the next request boundary and there is no unconsumed body to be
// misframed.
fn respond_and_continue(
	sock Socket,
	leftover_buf Binary,
	leftover_off Int,
	req Request,
	handler fn(Request) Response,
	pending Array(Binary),
) Result(Nil, NetError) {
	resp = handler(req)
	match respond(sock, pending, req.method, req.version, resp) {
		Err(e) -> Err(e)
		Ok(Close) -> Ok(Nil)
		Ok(
			KeepAlive(still_pending),
		) -> if h1.should_close(req.version, req.headers) || response_close(resp) {
			flush(sock, still_pending)
		} else {
			serve_conn(sock, leftover_buf, leftover_off, handler, still_pending)
		}
	}
}

// Turn a response into wire bytes with the correct framing. Returns whether
// the connection may stay alive, and the updated batch of unwritten parts.
//
// Buffered responses (Content-Length set and the body already in memory — the
// http.text case — or a bodiless HEAD/1xx/204/304 response) are NOT written
// here: their head + body parts are appended to `pending` and ride the next
// batched vectored write. Streaming responses first flush `pending` (their
// bytes must follow the earlier responses on the wire) and then write
// directly:
//   * unknown-length body (from `ok`) on HTTP/1.1 → Transfer-Encoding:
//     chunked; the connection stays alive.
//   * unknown-length body on HTTP/1.0 (no chunked) → Connection: close and
//     raw drain; the connection MUST close (close is the only body delimiter).
//   * a Content-Length body that turns out to be multi-chunk → head + first
//     chunks flush, the rest drains chunk by chunk.
// Declining to drain a suppressed body is what keeps a kept-alive connection
// in frame — those bytes would otherwise be read as the next response.
fn respond(
	sock Socket,
	pending Array(Binary),
	method Method,
	version Version,
	resp Response,
) Result(Sent, NetError) {
	if suppress_body(method, resp.status) {
		Ok(KeepAlive([..pending, h1.serialize_head(resp.status, resp.headers)]))
	} else if headers.has(resp.headers, NAME_CONTENT_LENGTH) {
		buffer_body(sock, pending, h1.serialize_head(resp.status, resp.headers), resp.body)
	} else {
		match version {
			Http11 -> {
				hdrs = headers.set(resp.headers, NAME_TRANSFER_ENCODING, VAL_CHUNKED)
				match flush(sock, pending) {
					Err(e) -> Err(e)
					Ok(
						_,
					) -> match body.drain_chunked_with_head(
						h1.serialize_head(resp.status, hdrs),
						resp.body,
						sock,
					) {
						Ok(_) -> Ok(KeepAlive([]))
						Err(e) -> Err(e)
					}
				}
			}
			Http10 -> {
				hdrs = headers.set(resp.headers, NAME_CONNECTION, VAL_CLOSE)
				match flush(sock, pending) {
					Err(e) -> Err(e)
					Ok(
						_,
					) -> match body.drain_with_head(
						h1.serialize_head(resp.status, hdrs),
						resp.body,
						sock,
					) {
						Ok(_) -> Ok(Close)
						Err(e) -> Err(e)
					}
				}
			}
		}
	}
}

// Append a Content-Length-framed response to the pending batch. The body is
// dissected here: one already in memory (http.text) just becomes parts; one
// that keeps producing chunks is a real stream, so everything accumulated so
// far flushes and the rest drains directly.
fn buffer_body(
	sock Socket,
	pending Array(Binary),
	head Binary,
	b Body,
) Result(Sent, NetError) {
	match body.take_buffered(b) {
		Err(e) -> Err(e)
		Ok(Empty) -> Ok(KeepAlive([..pending, head]))
		Ok(Whole(data)) -> Ok(KeepAlive([..pending, head, data]))
		Ok(Streaming(first, second, rest)) -> match flush(sock, [..pending, head, first, second]) {
			Err(e) -> Err(e)
			Ok(_) -> match body.drain(rest, sock) {
				Ok(_) -> Ok(KeepAlive([]))
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

// Whether the handler's response asked to close the connection. `Connection`
// is a token list, so `close` is matched as one element among possibly several.
fn response_close(resp Response) Bool {
	headers.contains_token(resp.headers, NAME_CONNECTION, VAL_CLOSE)
}

// Write a minimal framed error response (Content-Length: 0, Connection: close)
// so the client sees the status; the caller then stops the loop and closes.
fn write_error(sock Socket, status Int) Nil {
	hdrs = [
		Header(name: NAME_CONTENT_LENGTH, value: ZERO),
		Header(name: NAME_CONNECTION, value: VAL_CLOSE),
	]
	socket.write(sock, h1.serialize_head(status, hdrs)) or Nil
}
