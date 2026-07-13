// An HTTP client and the server it talks to, in one program.
//
// There is no HTTP client in the standard library yet, so the request bytes are
// written by hand and the response is read straight back off the socket — which
// is all an HTTP client is.
//
// The server is bound to port 0: the kernel picks a free port and net.local_addr
// reads it back, so the program never fights another process for 8080. Closing
// the listener at the end wakes the parked acceptors, and that is what lets this
// program exit — an unclosed net.serve/http.serve runs forever.

import al/array
import al/binary.{Dec}
import al/http.{Post, Request, Response}
import al/http/body.{
	Body,
	Chunk,
	Complete,
	DrainError,
	Drained,
	Overran,
	Short,
	SinkFailed,
	SourceFailed,
}
import al/http/h1.{Done, Length, NeedMore, Parsed}
import al/http/headers.{Header, Headers}
import al/int
import al/net.{Server}
import al/net/address
import al/net/error.{NetError, UnexpectedEof}
import al/net/socket.{Closed, Data, Socket}
import al/scheduler
import al/string
import al/time.{Instant}

const MAX_BODY = 8192

// ---------------------------------------------------------------------------
// The server half.
// ---------------------------------------------------------------------------

fn route(req Request) Response {
	match http.path(req) {
		<<'/'>> -> http.text('root')
		// http.text knows how long its body is, so the driver sends a
		// Content-Length. http.ok takes a Body — a pull stream, whose length is
		// not known until it ends — so the driver frames it chunked instead:
		// each chunk is a hex length, the bytes, then CRLF, and a zero-length
		// chunk ends the body. The reply below shows the framing on the wire.
		<<'/stream'>> -> http.ok(body.from_binary(<<'streamed'>>))
		<<'/echo'>> -> match req.method {
			Post -> match binary.to_string(body.collect(req.body, MAX_BODY) or <<>>) {
				Ok(text) -> http.text('echo: ${text}')
				Err(Nil) -> http.text('echo: <not utf-8>')
			}
			else -> http.not_found()
		}
		else -> http.not_found()
	}
}

// ---------------------------------------------------------------------------
// The client half.
// ---------------------------------------------------------------------------

// Read until the peer closes. One socket.read is not a message boundary: TCP
// hands back whatever has arrived, so a client that trusts a single read will
// sooner or later see half a response. `Connection: close` below makes the end
// of the response the end of the stream, which is the simplest frame there is.
fn read_to_close(conn Socket, acc Binary) Binary {
	match socket.read(conn, 65536) {
		Ok(Data(bytes)) -> read_to_close(conn, binary.append(acc, bytes))
		Ok(Closed) -> acc
		Err(_) -> acc
	}
}

// One request/response round trip on a fresh connection. Content-Length is in
// bytes, not characters, so it is measured on the encoded body.
fn request(port Int, method String, target String, payload String) String {
	bytes = binary.from_string(payload)
	length = binary.byte_size(bytes)
	head = '${method} ${target} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nContent-Length: ${length}\r\n\r\n'
	match net.connect('127.0.0.1', port) {
		Ok(conn) -> {
			socket.write(conn, binary.append(binary.from_string(head), bytes)) or Nil
			response = read_to_close(conn, <<>>)
			socket.close(conn) or Nil
			binary.to_string(response) or '<not utf-8>'
		}
		Err(e) -> 'connect failed: ${e}'
	}
}

// Responses are CRLF-framed; print them one line per line, indented, so the
// blank line between head and body stays visible.
fn show(label String, response String) Nil {
	println(label)
	array.each(string.split(response, '\r\n'), fn(line) println('  | ${line}'))
}

// ---------------------------------------------------------------------------
// The response path, by hand.
//
// http.serve_on above hands the handler a Request and takes a Response back; it
// never shows you a socket. Underneath, al/http/body is what actually moves the
// bytes, and it has two halves. A request body is a PULL stream read off the
// socket by body.content_length. A response body is DRAINED back to the socket,
// five ways: with or without the head riding the first write, raw or chunked,
// and — when a length has already been promised — under a byte budget.
//
// This second server does all of that by hand, one target per way.
// ---------------------------------------------------------------------------

// The head is read in small bites on purpose. A body bigger than this cannot
// arrive whole inside the head's buffer, so the Content-Length reader below
// always has real socket reads left to do.
const HEAD_READ = 128
const DEADLINE_MS = 5000

const NAME_LENGTH = <<'Content-Length'>>
const NAME_TE = <<'Transfer-Encoding'>>
const VAL_CHUNKED = <<'chunked'>>

// A head that promises exactly `n` bytes of body, and one that promises no
// length at all — chunked framing carries the lengths inline instead.
fn fixed_head(n Int) Binary {
	length = Header(name: NAME_LENGTH, value: binary.from_int_ascii(n, Dec))
	h1.serialize_head(200, [length])
}

fn chunked_head() Binary {
	h1.serialize_head(200, [Header(name: NAME_TE, value: VAL_CHUNKED)])
}

// A Body is a pull thunk: calling it yields one chunk plus the continuation
// that yields the rest. body.from_binary makes a one-chunk body; a real source —
// a file, a proxied upstream — yields as many as it has. These two hand-written
// bodies yield two chunks, which is what makes the framing below visible.
fn two_chunks() Body {
	fn() Ok(Chunk(<<'first '>>, body.from_binary(<<'second'>>)))
}

// 5 bytes, then 4. Under a 5-byte budget the second chunk would cross the
// promise, so it never reaches the wire.
fn budget_body() Body {
	fn() Ok(Chunk(<<'five!'>>, body.from_binary(<<'more'>>)))
}

// A drain couples a SOURCE (the body's thunk) to a SINK (the socket), and the
// error names the side that failed: a failed source leaves the socket healthy
// and an error response still deliverable, a failed sink leaves nothing
// deliverable at all. Neither happens on a loopback, so this only names them.
fn sent(what String, r Result(Nil, DrainError)) Nil {
	match r {
		Ok(_) -> Nil
		Err(SourceFailed(e)) -> println('${what}: source failed: ${string.inspect(e)}')
		Err(SinkFailed(e)) -> println('${what}: sink failed: ${string.inspect(e)}')
	}
}

// drain_counted reports how the body squared with its budget, rather than
// handing back a bare count the caller would have to compare itself.
fn counted(what String, r Result(Drained, DrainError)) Nil {
	match r {
		Ok(Complete) -> println('${what}: Complete')
		Ok(Short(written)) -> println('${what}: Short(${written})')
		Ok(Overran(written)) -> println('${what}: Overran(${written})')
		Err(SourceFailed(e)) -> println('${what}: source failed: ${string.inspect(e)}')
		Err(SinkFailed(e)) -> println('${what}: sink failed: ${string.inspect(e)}')
	}
}

// Read until h1 says the head is complete. One read is not a message boundary
// here either, so the buffer grows until the parser stops asking for more.
fn read_head(sock Socket, buf Binary, deadline Instant) Result((Binary, Parsed), NetError) {
	match h1.parse_request(buf, 0) {
		NeedMore -> match socket.read_until(sock, HEAD_READ, deadline) {
			Ok(Data(chunk)) -> read_head(sock, binary.append(buf, chunk), deadline)
			Ok(Closed) -> Err(UnexpectedEof)
			Err(e) -> Err(e)
		}
		parsed -> Ok((buf, parsed))
	}
}

// The request body: whatever already arrived in the head's buffer, plus the
// rest pulled off the socket by the Content-Length reader. `need` can be zero —
// a small body arrives whole with the head, and a GET has none — and
// content_length then ends without touching the socket at all. The deadline is
// taken ONCE for the whole body, so a peer dribbling a byte per read cannot
// keep resetting the clock.
fn read_body(
	sock Socket,
	buf Binary,
	consumed Int,
	hs Headers,
	deadline Instant,
) Result(Binary, NetError) {
	match h1.framing(hs) {
		Length(n) -> {
			avail = binary.byte_size(buf) - consumed
			buffered = int.min(avail, n)
			head_bytes = binary.slice_bytes(buf, consumed, buffered)
			rest = body.content_length(sock, n - buffered, deadline)
			match body.collect(rest, MAX_BODY) {
				Ok(tail) -> Ok(binary.append(head_bytes, tail))
				Err(e) -> Err(e)
			}
		}
		// The client above only ever sends a Content-Length body.
		else -> Ok(<<>>)
	}
}

fn respond(sock Socket, target Binary, payload Binary) Nil {
	match target {
		// Head and body reach the kernel in ONE vectored write — no syscall of
		// its own for the head, and no copy to concatenate the two.
		<<'/pump'>> -> {
			bin = binary.from_string('pumped ${binary.byte_size(payload)} bytes')
			head = fixed_head(binary.byte_size(bin))
			sent('pump', body.drain_with_head(head, body.from_binary(bin), sock))
		}
		// The head has already gone out on its own, so the body drains alone:
		// one write per chunk, nothing buffered, memory stays O(chunk).
		<<'/bare'>> -> {
			bin = <<'drained bare'>>
			socket.write(sock, fixed_head(binary.byte_size(bin))) or Nil
			sent('bare', body.drain(body.from_binary(bin), sock))
		}
		// Chunked: a hex length in front of every chunk, and a zero-length chunk
		// to end the body. The total length is never needed, so a body of unknown
		// length still keeps the connection alive instead of framing by close.
		<<'/chunked'>> -> {
			socket.write(sock, chunked_head()) or Nil
			sent('chunked', body.drain_chunked(two_chunks(), sock))
		}
		// The same framing with the head riding the first chunk's write.
		<<'/streamed'>> ->
			sent('streamed', body.drain_chunked_with_head(chunked_head(), two_chunks(), sock))
		// Budgeted. The head promised 12 bytes and the body is exactly 12: Complete.
		<<'/counted'>> -> {
			bin = <<'counted body'>>
			n = binary.byte_size(bin)
			socket.write(sock, fixed_head(n)) or Nil
			counted('counted', body.drain_counted(body.from_binary(bin), sock, n))
		}
		// The head promised 5. The first chunk is 5 bytes and goes out; the
		// second would make 9, so it is NOT written and the caller hears Overran.
		// A server that already advertised a length cannot send an unpromised byte.
		<<'/overrun'>> -> {
			socket.write(sock, fixed_head(5)) or Nil
			counted('overrun', body.drain_counted(budget_body(), sock, 5))
		}
		else -> socket.write(sock, h1.serialize_head(404, [])) or Nil
	}
}

fn serve_raw(sock Socket) Nil {
	deadline = time.add_ms(time.monotonic(), DEADLINE_MS)
	match read_head(sock, <<>>, deadline) {
		Ok((buf, Done(target: t, headers: hs, consumed: c, ..))) ->
			respond(sock, t, read_body(sock, buf, c, hs, deadline) or <<>>)
		// A malformed head, or a peer that hung up mid-head: nothing to answer.
		else -> Nil
	}
	socket.close(sock) or Nil
}

fn accept_loop(server Server) Nil {
	match net.accept(server) {
		Ok(sock) -> {
			scheduler.spawn(fn() serve_raw(sock))
			accept_loop(server)
		}
		// net.close delivers this to the parked accept: the shutdown signal.
		Err(_) -> Nil
	}
}

// 2 KiB — comfortably past HEAD_READ, so the reader has to go back to the
// socket for the rest of it.
fn pump_payload() String {
	array.fold(1..129, '', fn(acc, _i) '${acc}0123456789abcdef')
}

fn by_hand() Nil {
	match net.listen('127.0.0.1', 0) {
		Ok(server) -> match net.local_addr(server) {
			Ok(addr) -> {
				port = address.port(addr)
				scheduler.spawn(fn() accept_loop(server))

				show('POST /pump', request(port, 'POST', '/pump', pump_payload()))
				show('GET /bare', request(port, 'GET', '/bare', ''))
				show('GET /chunked', request(port, 'GET', '/chunked', ''))
				show('GET /streamed', request(port, 'GET', '/streamed', ''))
				show('GET /counted', request(port, 'GET', '/counted', ''))
				show('GET /overrun', request(port, 'GET', '/overrun', ''))

				net.close(server) or Nil
			}
			Err(e) -> println('local_addr failed: ${e}')
		}
		Err(e) -> println('listen failed: ${e}')
	}
}

// ---------------------------------------------------------------------------
// Both halves, one process tree.
// ---------------------------------------------------------------------------

match net.listen('127.0.0.1', 0) {
	Ok(server) -> match net.local_addr(server) {
		Ok(addr) -> {
			port = address.port(addr)
			http.serve_on(server, route)

			show('GET /', request(port, 'GET', '/', ''))
			show('POST /echo', request(port, 'POST', '/echo', 'hello'))
			show('GET /stream', request(port, 'GET', '/stream', ''))
			show('GET /nope', request(port, 'GET', '/nope', ''))

			// Wakes every acceptor with NotConnected, their clean-shutdown
			// signal, so the accept loops return and the program ends.
			net.close(server) or Nil
		}
		Err(e) -> println('local_addr failed: ${e}')
	}
	Err(e) -> println('listen failed: ${e}')
}

by_hand()
