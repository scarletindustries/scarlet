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
import al/net/socket.{Socket, Data, Closed}
import al/net/error.{NetError}
import al/binary.{Dec}
import al/int
import al/time.{Instant}

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
	// The request-target as it appeared on the wire (RFC 9112 §3.2): usually an
	// origin-form path plus optional `?query`, but may also be `*` (OPTIONS) or
	// an absolute URI. Use `path`/`query` to split origin-form.
	target Binary
	version Version
	headers Headers
	// Trailer fields received after a chunked body. Kept separate from
	// `headers`: RFC 9110 §6.5.1 forbids merging trailers into the header
	// section for message-control decisions (Connection, Expect), so the
	// keep-alive check reads only `headers`.
	trailers Headers
	body Body
}

// How a response body is framed on the wire, carried by the type instead of
// sniffed from a handler-set Content-Length header. `Fixed` promises exactly
// `len` body bytes: the connection driver emits the Content-Length header
// itself and verifies the stream delivered that many. `Unsized` makes no
// promise: the driver frames it at send time — Transfer-Encoding: chunked on
// HTTP/1.1, connection-close on HTTP/1.0. A handler cannot desync the
// connection by advertising one length and writing another; the framing
// decision and the byte count live in the same value.
pub type ResponseBody {
	Fixed(len Int, b Body)
	Unsized(b Body)
}

pub type Response {
	status Int
	headers Headers
	body ResponseBody
}

// The parsed request head as one value, threaded through the body-reading
// helpers instead of four loose positional params (two of which — method and
// target — are both `Binary` and would silently typecheck if swapped).
type ReqHead {
	method Binary
	target Binary
	version Version
	headers Headers
}

// The outcome of writing one response: whether the connection may stay alive,
// and if so, the batch of bytes still queued for the next vectored write.
type Sent {
	KeepAlive(pending Array(Binary))
	Close
}

// Largest request body the default buffered path will accept. A Content-Length
// above this is rejected with 413 before any of the body is read. Together
// with BODY_TIMEOUT_MS this is the denial-of-service bound: a dribbled upload
// can neither exhaust memory (MAX_BODY) nor pin the connection (the body
// deadline).
const MAX_BODY = 1048576
const READ_SIZE = 65536
// How long the peer has to deliver a complete request head, as ONE absolute
// deadline taken at the start of each request cycle — a slowloris client
// dribbling a header byte per read cannot keep resetting the clock. The same
// deadline bounds how long an idle keep-alive connection is held open.
const HEAD_TIMEOUT_MS = 30000
// How long the peer has to deliver a complete request body, as ONE absolute
// deadline taken when the framing is decided — the same slowloris defense as
// the head: a peer dribbling a body byte per read cannot keep resetting the
// clock, it runs out of time and forfeits the connection (TimedOut).
const BODY_TIMEOUT_MS = 30000
const EMPTY = <<>>
const QUESTION = <<'?'>>

const NAME_CONTENT_TYPE = <<'Content-Type'>>
const NAME_CONTENT_LENGTH = <<'Content-Length'>>
const NAME_CONNECTION = <<'Connection'>>
const VAL_TEXT_PLAIN = <<'text/plain; charset=utf-8'>>
const VAL_CLOSE = <<'close'>>
const VAL_KEEP_ALIVE = <<'keep-alive'>>
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

// The origin-form path component of the request-target: everything before the
// first `?`, or the whole target if there is no query. O(1) zero-copy slice.
pub fn path(r Request) Binary {
	match binary.index_of(r.target, QUESTION, 0) {
		Some(i) -> binary.slice_bytes(r.target, 0, i)
		None -> r.target
	}
}

// The query component of the request-target: everything after the first `?`
// (not including it), or empty if there is no `?`.
pub fn query(r Request) Binary {
	match binary.index_of(r.target, QUESTION, 0) {
		Some(i) -> binary.slice_bytes(r.target, i + 1, binary.byte_size(r.target) - i - 1)
		None -> EMPTY
	}
}

// A 200 response whose body is supplied directly as a pull thunk of unknown
// length (Unsized). The connection driver frames it at send time from the
// request's HTTP version — Transfer-Encoding: chunked on HTTP/1.1 (so the
// connection stays alive), or connection-close on HTTP/1.0 (which has no
// chunked encoding). A handler that knows its length should build a `Fixed`
// body instead (e.g. via `text`); both keep the connection alive.
pub fn ok(b Body) Response {
	Response(status: 200, headers: [], body: Unsized(b))
}

pub fn not_found() Response {
	text_response(404, 'Not Found')
}

// A text/plain response with the body buffered and its length pinned (Fixed);
// the driver emits the Content-Length header from that length.
pub fn text(s String) Response {
	text_response(200, s)
}

fn text_response(code Int, s String) Response {
	bin = binary.from_string(s)
	Response(
		status: code,
		headers: [Header(name: NAME_CONTENT_TYPE, value: VAL_TEXT_PLAIN)],
		body: Fixed(binary.byte_size(bin), body.from_binary(bin)),
	)
}

// Set a header on a response: after this exactly one field carries `name`
// (headers.set semantics — replace the first match, drop later duplicates,
// append if absent).
pub fn with_header(r Response, name Binary, value Binary) Response {
	Response(status: r.status, headers: headers.set(r.headers, name, value), body: r.body)
}

fn build_request(head ReqHead, trailers Headers, b Body) Request {
	Request(
		method: to_method(head.method),
		target: head.target,
		version: head.version,
		headers: head.headers,
		trailers: trailers,
		body: b,
	)
}

// Listen on host:port and serve each accepted connection on its own process.
// One slow or hostile connection parks only its own process. The accept loop
// runs on every core in parallel (net.serve), all draining one shared accept
// queue, and each connection is handled on the core that accepted it.
pub fn serve(host String, port Int, handler fn(Request) Response) Result(Nil, NetError) {
	net.serve(host, port, fn(sock) drive(sock, handler))
}

// Serve connections from a listener the caller already bound. This is the
// entry point when the caller needs the bound address before serving — e.g.
// listen on port 0, read the kernel-assigned port back with net.local_addr,
// then hand the listener over. `serve` is the listen-and-serve convenience on
// top of this.
pub fn serve_on(server Server, handler fn(Request) Response) Nil {
	net.serve_on(server, fn(sock) drive(sock, handler))
}

// Own one connection start-to-finish and always close it on the way out. The
// loop itself never closes the socket: it returns to here, which closes once.
fn drive(sock Socket, handler fn(Request) Response) Nil {
	serve_conn(sock, EMPTY, 0, head_deadline(), handler, []) or Nil
	socket.close(sock) or Nil
}

// The absolute deadline for reading the next request head, taken once at the
// top of each request cycle so partial reads never extend it.
fn head_deadline() Instant {
	time.add_ms(time.monotonic(), HEAD_TIMEOUT_MS)
}

// The absolute deadline for receiving the whole request body, taken once when
// the framing is decided so partial reads never extend it.
fn body_deadline() Instant {
	time.add_ms(time.monotonic(), BODY_TIMEOUT_MS)
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
// `deadline` is the request cycle's head deadline: NeedMore recursions carry
// it unchanged, so a peer dribbling the head one byte at a time runs out of
// time instead of holding the connection open (TimedOut errs out through
// `drive`, which closes).
fn serve_conn(
	sock Socket,
	buf Binary,
	off Int,
	deadline Instant,
	handler fn(Request) Response,
	pending Array(Binary),
) Result(Nil, NetError) {
	match h1.parse_request(buf, off) {
		NeedMore -> match flush(sock, pending) {
			Err(e) -> Err(e)
			Ok(_) -> match socket.read_until(sock, READ_SIZE, deadline) {
				Ok(Data(more)) -> serve_conn(sock, carry(buf, off, more), 0, deadline, handler, [])
				Ok(Closed) -> Ok(Nil)
				Err(e) -> Err(e)
			}
		}
		Bad(status) -> {
			reject(sock, pending, status)
			Ok(Nil)
		}
		Done(method, target, version, hdrs, consumed) -> {
			head = ReqHead(method: method, target: target, version: version, headers: hdrs)
			handle(sock, buf, consumed, head, handler, pending)
		}
	}
}

// Refuse a request: flush anything still pending (so earlier pipelined
// responses are not lost), then write a framed error response. The caller
// stops its loop afterwards so the connection closes.
fn reject(sock Socket, pending Array(Binary), status Int) Nil {
	flush(sock, pending) or Nil
	write_error(sock, status)
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
	head ReqHead,
	handler fn(Request) Response,
	pending Array(Binary),
) Result(Nil, NetError) {
	match h1.framing(head.headers) {
		Invalid(status) -> {
			reject(sock, pending, status)
			Ok(Nil)
		}
		NoBody ->
			respond_and_continue(
				sock,
				buf,
				consumed,
				build_request(head, [], body.empty()),
				handler,
				pending,
			)
		Length(n) -> if n > MAX_BODY {
			reject(sock, pending, 413)
			Ok(Nil)
		} else {
			// The whole body must arrive by one absolute deadline, taken here —
			// where the framing is decided — exactly like the head deadline.
			deadline = body_deadline()
			// The body is read off the socket: anything still pending must go
			// out first, or the client could sit waiting for those responses
			// before it sends the body we are about to wait for.
			match flush(sock, pending) {
				Err(e) -> Err(e)
				Ok(_) -> read_body(sock, buf, consumed, n, deadline, head, handler)
			}
		}
		// Chunked body: same flush-first reasoning as Content-Length, and the
		// same absolute body deadline.
		Chunked -> {
			deadline = body_deadline()
			match flush(sock, pending) {
				Err(e) -> Err(e)
				Ok(_) -> read_chunked_body(sock, buf, consumed, deadline, head, handler)
			}
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
	deadline Instant,
	head ReqHead,
	handler fn(Request) Response,
) Result(Nil, NetError) {
	match maybe_continue(sock, head.headers) {
		Err(e) -> Err(e)
		Ok(_) -> {
			avail = binary.byte_size(buf) - consumed
			buffered = int.min(avail, n)
			head_bytes = binary.slice_bytes(buf, consumed, buffered)
			need = n - buffered
			match body.collect(body.content_length(sock, need, deadline), MAX_BODY) {
				Err(e) -> Err(e)
				// Pending responses were flushed before the body was read, so
				// this request starts a fresh batch.
				Ok(tail) -> {
					req = build_request(head, [], body.from_binary(binary.append(head_bytes, tail)))
					if avail > n {
						respond_and_continue(sock, buf, consumed + n, req, handler, [])
					} else {
						respond_and_continue(sock, EMPTY, 0, req, handler, [])
					}
				}
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
	deadline Instant,
	head ReqHead,
	handler fn(Request) Response,
) Result(Nil, NetError) {
	match maybe_continue(sock, head.headers) {
		Err(e) -> Err(e)
		Ok(_) -> chunked_loop(sock, buf, off, deadline, head, handler)
	}
}

// The chunked read/decode loop. The native decoder (h1.chunk_decode) scans
// whatever is buffered so far; this loop owns the I/O decision —
// ChunkedNeedMore means read more off the socket (parking on backpressure,
// every read counting against the one absolute body deadline) and retry from
// the same offset, exactly like serve_conn's head-parsing loop. The decoder
// caps the decoded size at MAX_BODY (413), so memory stays bounded no matter
// what the wire claims, and `consumed` points at the first byte after the
// terminator so pipelined requests carry forward through respond_and_continue
// unchanged.
fn chunked_loop(
	sock Socket,
	buf Binary,
	off Int,
	deadline Instant,
	head ReqHead,
	handler fn(Request) Response,
) Result(Nil, NetError) {
	match h1.chunk_decode(buf, off, MAX_BODY) {
		ChunkedNeedMore -> match socket.read_until(sock, READ_SIZE, deadline) {
			Ok(Data(more)) ->
				chunked_loop(sock, binary.append(buf, more), off, deadline, head, handler)
			// Peer closed mid-body: there is no complete request to answer.
			Ok(Closed) -> Ok(Nil)
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
				build_request(head, trailers, body.from_binary(decoded)),
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
	close_after = h1.should_close(req.version, req.headers) || response_close(resp)
	match respond(sock, pending, req.method, req.version, close_after, resp) {
		Err(e) -> Err(e)
		Ok(Close) -> Ok(Nil)
		Ok(KeepAlive(still_pending)) -> if close_after {
			flush(sock, still_pending)
		} else {
			serve_conn(sock, leftover_buf, leftover_off, head_deadline(), handler, still_pending)
		}
	}
}

// Turn a response into wire bytes with the framing chosen by the body's own
// constructor. Returns whether the connection may stay alive, and the updated
// batch of unwritten parts.
//
// Buffered responses (a Fixed body already in memory — the http.text case —
// or a bodiless HEAD/204/304 response) are NOT written here: their head +
// body parts are appended to `pending` and ride the next batched vectored
// write. Streaming responses first flush `pending` (their bytes must follow
// the earlier responses on the wire) and then write directly:
//   * Unsized body (from `ok`) on HTTP/1.1 → Transfer-Encoding: chunked; the
//     connection stays alive.
//   * Unsized body on HTTP/1.0 (no chunked) → Connection: close and raw
//     drain; the connection MUST close (close is the only body delimiter).
//   * a Fixed body that turns out to be multi-chunk → head + first chunks
//     flush, the rest drains chunk by chunk, counted against the promise.
// Declining to drain a suppressed body is what keeps a kept-alive connection
// in frame — those bytes would otherwise be read as the next response.
//
// Three rejects guard the wire before anything is serialized. A status outside
// 200..=999 cannot be a final status line (the only 1xx the driver writes is
// maybe_continue's 100), so the response is replaced with a framed
// 500-and-close. The header block must pass headers.valid: a handler that
// reflected external input into a raw Header (rather than going through
// headers.field) could otherwise smuggle a CR/LF onto the wire and split the
// response — a poisoned response is discarded whole, same 500-and-close. And a
// Fixed body promising a negative length is rejected here too: buffer_body
// would catch the impossible promise on a body-bearing response (no byte
// count matches it), but the HEAD path serializes the advertised length
// without ever counting body bytes, and `Content-Length: -1` is not a valid
// field value (1*DIGIT) — a lenient client could desync on it.
fn respond(
	sock Socket,
	pending Array(Binary),
	method Method,
	version Version,
	close_after Bool,
	resp Response,
) Result(Sent, NetError) {
	if resp.status < 200 || resp.status > 999 {
		replace_with_500(sock, pending)
	} else if !headers.valid(resp.headers) {
		replace_with_500(sock, pending)
	} else if negative_fixed_length(resp.body) {
		replace_with_500(sock, pending)
	} else {
		hdrs = conn_headers(version, close_after, strip_framing(resp.headers))
		if suppress_body(method, resp.status) {
			head = h1.serialize_head(resp.status, head_only_headers(method, hdrs, resp.body))
			Ok(KeepAlive([..pending, head]))
		} else {
			match resp.body {
				Fixed(len, b) ->
					buffer_body(sock, pending, resp.status, with_length(hdrs, len), len, b)
				Unsized(b) -> match version {
					Http11 -> {
						chunked = headers.set(hdrs, NAME_TRANSFER_ENCODING, VAL_CHUNKED)
						match flush(sock, pending) {
							Err(e) -> Err(e)
							Ok(
								_,
							) -> match body.drain_chunked_with_head(
								h1.serialize_head(resp.status, chunked),
								sanitize_trailers(b),
								sock,
							) {
								Ok(_) -> Ok(KeepAlive([]))
								Err(e) -> Err(e)
							}
						}
					}
					Http10 -> {
						closing = headers.set(hdrs, NAME_CONNECTION, VAL_CLOSE)
						match flush(sock, pending) {
							Err(e) -> Err(e)
							Ok(
								_,
							) -> match body.drain_with_head(
								h1.serialize_head(resp.status, closing),
								b,
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
	}
}

// Discard the handler's response and answer with a framed 500 instead,
// closing the connection.
fn replace_with_500(sock Socket, pending Array(Binary)) Result(Sent, NetError) {
	reject(sock, pending, 500)
	Ok(Close)
}

// The driver owns framing: any Content-Length or Transfer-Encoding the
// handler put in the header list is dropped, and the real framing header is
// re-derived from the ResponseBody constructor — so the advertised framing
// can never disagree with the bytes actually written.
fn strip_framing(hs Headers) Headers {
	headers.delete(headers.delete(hs, NAME_CONTENT_LENGTH), NAME_TRANSFER_ENCODING)
}

// HTTP/1.0 defaults to close, so when the driver keeps a 1.0 connection alive
// (the client sent Connection: keep-alive) the response must say so
// explicitly — a 1.0 client that never sees `Connection: keep-alive` echoed
// back assumes the response is close-delimited and hangs or drops the
// connection. HTTP/1.1 defaults to keep-alive, so the inverse applies: when
// the driver has decided to close, the final response advertises
// `Connection: close` (RFC 9112 §9.6) so the client stops reusing the
// connection instead of discovering the close on its next request.
fn conn_headers(version Version, close_after Bool, hs Headers) Headers {
	match version {
		Http10 -> if close_after {
			hs
		} else {
			headers.set(hs, NAME_CONNECTION, VAL_KEEP_ALIVE)
		}
		Http11 -> if close_after {
			headers.set(hs, NAME_CONNECTION, VAL_CLOSE)
		} else {
			hs
		}
	}
}

// Headers for a bodiless response. A HEAD response still advertises the
// length of the body a GET would have carried (RFC 9110 §8.6), so a Fixed
// body's length is emitted even though its bytes are not; 204/304 stay
// length-free.
fn head_only_headers(method Method, hs Headers, b ResponseBody) Headers {
	match (method, b) {
		(Head, Fixed(len, _)) -> with_length(hs, len)
		else -> hs
	}
}

fn with_length(hs Headers, len Int) Headers {
	headers.set(hs, NAME_CONTENT_LENGTH, binary.from_int_ascii(len, Dec))
}

// A Fixed body advertising a negative length — an unkeepable promise. respond
// rejects it before any serialization so GET and HEAD agree: GET would catch
// it in buffer_body, but HEAD emits the advertised length without draining
// the body and would put `Content-Length: -1` on the wire.
fn negative_fixed_length(b ResponseBody) Bool {
	match b {
		Fixed(len, _) -> len < 0
		Unsized(_) -> False
	}
}

// Append a Fixed response to the pending batch, holding the body to its
// promise of exactly `len` bytes. The body is dissected here: one already in
// memory (http.text) just becomes parts; one that keeps producing chunks is a
// real stream, so everything accumulated so far flushes and the rest drains
// directly. A body whose bytes contradict `len` before the head has gone out
// is replaced with a 500-and-close; a stream that turns out short or long
// AFTER the head (and its Content-Length) is on the wire can only close —
// keeping the connection would let the next response be read out of frame.
// The drain is budgeted at the promised remainder, so an over-long stream is
// cut off at the boundary: no byte past the advertised length is ever sent.
fn buffer_body(
	sock Socket,
	pending Array(Binary),
	status Int,
	hdrs Headers,
	len Int,
	b Body,
) Result(Sent, NetError) {
	match body.take_buffered(b) {
		Err(e) -> Err(e)
		Ok(Empty) -> if len == 0 {
			Ok(KeepAlive([..pending, h1.serialize_head(status, hdrs)]))
		} else {
			replace_with_500(sock, pending)
		}
		Ok(Whole(data)) -> if binary.byte_size(data) == len {
			Ok(KeepAlive([..pending, h1.serialize_head(status, hdrs), data]))
		} else {
			replace_with_500(sock, pending)
		}
		Ok(Streaming(first, second, rest)) -> {
			got = binary.byte_size(first) + binary.byte_size(second)
			if got > len {
				replace_with_500(sock, pending)
			} else {
				match flush(sock, [..pending, h1.serialize_head(status, hdrs), first, second]) {
					Err(e) -> Err(e)
					Ok(_) -> match body.drain_counted(rest, sock, len - got) {
						Err(e) -> Err(e)
						Ok(body.Complete) -> Ok(KeepAlive([]))
						// Short or over-long after the head (and its
						// Content-Length) is on the wire: closing is the only
						// way to keep the next response in frame.
						Ok(body.Short(_)) -> Ok(Close)
						Ok(body.Overran(_)) -> Ok(Close)
					}
				}
			}
		}
	}
}

// Trailer fields ride the chunked terminator that body.drain_chunked_with_head
// writes, so they need the same gate as the header block. This wraps a body so
// a trailer list failing headers.valid is dropped at the moment it surfaces —
// dropping is always legal (trailers are optional metadata), while serializing
// it would reopen the response-splitting hole the gate in `respond` closes.
fn sanitize_trailers(b Body) Body {
	fn() match body.pull(b) {
		Ok(body.Chunk(data, next)) -> Ok(body.Chunk(data, sanitize_trailers(next)))
		Ok(body.Done(trailers)) -> if headers.valid(trailers) {
			Ok(body.Done(trailers))
		} else {
			Ok(body.Done([]))
		}
		Err(e) -> Err(e)
	}
}

// RFC 9110: a HEAD response, and any 204/304 response, carries headers but
// MUST NOT carry a body. (1xx would qualify too, but a handler 1xx never
// reaches here — respond rejects statuses below 200.)
fn suppress_body(method Method, status Int) Bool {
	match method {
		Head -> True
		else -> status == 204 || status == 304
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
