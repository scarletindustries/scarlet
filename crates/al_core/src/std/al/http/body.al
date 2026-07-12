import al/binary.{Hex}
import al/int
import al/net/socket.{Socket, Data, Closed}
import al/net/error.{NetError, UnexpectedEof, MessageTooLarge}
import al/http/headers.{Header}
import al/time.{Instant}

// The socket read size for the streaming readers below: large enough that a
// full body arrives in a handful of pulls, small enough that a slow consumer
// never has more than one chunk of memory pinned.
const READ_SIZE = 65536
const CRLF = <<'\r\n'>>
const ZERO = <<'0'>>
// 0-size chunk + empty trailer block: the terminator for a trailer-free body.
const LAST_CHUNK = <<'0\r\n\r\n'>>

// A message body as a pull thunk: calling it advances the stream by one step,
// returning the next chunk plus its own continuation, or the end of the body
// with any trailers. The continuation lives *inside* the result so a body is a
// plain `fn() Result(Pull, NetError)` — a transparent alias, never a nominal
// wrapper, so there is no per-chunk allocation beyond the chunk itself. Errors
// are `NetError` to pass socket failures through verbatim.
//
// `Pull` and `Body` are mutually recursive and must stay in one module: the
// continuation in `Chunk` is a `Body`, and a `Body` yields a `Pull`. Splitting
// them would force an import cycle, which the compiler rejects.
pub type Pull {
	Chunk(data Binary, next Body)
	Done(trailers Array(Header))
}

pub type Body = fn() Result(Pull, NetError)

// A body that yields nothing — one step straight to Done with no trailers.
pub fn empty() Body {
	fn() Ok(Done([]))
}

// A body that yields `b` as a single chunk, then ends. A zero-length `b` still
// produces one (empty) chunk before Done.
pub fn from_binary(b Binary) Body {
	fn() Ok(Chunk(b, empty()))
}

// A Content-Length-framed reader over `reader`: yields exactly `n` more bytes
// across as many chunks as the socket hands back, then Done. `n` is threaded
// immutably — the socket fd is the real cursor — and capped at 64 KiB per read.
// A peer close before `n` is reached is an error (UnexpectedEof), not a clean
// end. `deadline` is the ONE absolute deadline for the whole body, taken by
// the caller when the framing was decided: every read counts against it, so a
// peer dribbling a byte per read cannot keep resetting the clock — once the
// deadline passes the next read errs with TimedOut.
pub fn content_length(reader Socket, n Int, deadline Instant) Body {
	fn() match n {
		0 -> Ok(Done([]))
		else -> match socket.read_until(reader, int.min(n, READ_SIZE), deadline) {
			Ok(Data(chunk)) ->
				Ok(Chunk(chunk, content_length(reader, n - binary.byte_size(chunk), deadline)))
			Ok(Closed) -> Err(UnexpectedEof)
			Err(e) -> Err(e)
		}
	}
}

// Advance a body by one step. A body is just a thunk; this names the operation.
pub fn pull(b Body) Result(Pull, NetError) {
	b()
}

// What `take_buffered` found a body to be.
pub type Buffered {
	// No bytes at all.
	Empty
	// The whole body was one in-memory chunk (http.text and friends).
	Whole(data Binary)
	// A real stream: the chunks pulled while finding that out, plus the rest.
	Streaming(first Binary, second Binary, rest Body)
}

// Dissect a body without writing it anywhere. A body that is already a single
// in-memory chunk comes back as `Whole(data)` so the caller can batch those
// bytes with other writes; a body that keeps producing chunks comes back as
// `Streaming` so the caller can drain it for real. This is what lets a server
// coalesce a batch of pipelined buffered responses into one vectored write
// while still streaming the responses that need it.
pub fn take_buffered(b Body) Result(Buffered, NetError) {
	match pull(b) {
		Err(e) -> Err(e)
		Ok(Done(_)) -> Ok(Empty)
		Ok(Chunk(data, next)) -> match pull(next) {
			Err(e) -> Err(e)
			Ok(Done(_)) -> Ok(Whole(data))
			Ok(Chunk(second, rest)) -> Ok(Streaming(data, second, rest))
		}
	}
}

// Pump a whole body out to `sock`: pull a chunk, write it, recurse. The write
// parks on backpressure, so a slow consumer naturally rate-limits the source —
// the next chunk is not pulled until the previous one has drained. Nothing is
// buffered: memory stays O(chunk). Trailers on Done are dropped — a raw
// (non-chunked) body has no wire representation for them.
pub fn drain(body Body, sock Socket) Result(Nil, NetError) {
	drain_prefixed([], body, sock)
}

// Like `drain`, but `head` (a serialized response head) rides in the same
// vectored write as the first body chunk: a buffered single-chunk response
// (http.text) reaches the kernel in exactly ONE syscall, head and body
// together. Subsequent chunks drain as usual.
pub fn drain_with_head(head Binary, body Body, sock Socket) Result(Nil, NetError) {
	drain_prefixed([head], body, sock)
}

// The single drain worker. `prefix` is whatever bytes must precede the first
// write — the response head on the first call, `[]` after — so the head rides
// the same vectored write as the first chunk (or as Done, if the body is
// empty) without a separate code path per case.
fn drain_prefixed(prefix Array(Binary), body Body, sock Socket) Result(Nil, NetError) {
	match pull(body) {
		Ok(Chunk(data, next)) -> match socket.write_parts(sock, [..prefix, data]) {
			Ok(_) -> drain_prefixed([], next, sock)
			Err(e) -> Err(e)
		}
		Ok(Done(_)) -> match prefix {
			[] -> Ok(Nil)
			else -> match socket.write_parts(sock, prefix) {
				Ok(_) -> Ok(Nil)
				Err(e) -> Err(e)
			}
		}
		Err(e) -> Err(e)
	}
}

// The outcome of `drain_counted`: how the body squared with the byte budget
// it was drained under. One variant per case the caller must handle, instead
// of a bare count the caller would have to compare against the budget itself.
pub type Drained {
	// The body ended having written exactly the budget.
	Complete
	// The body ended early: only `written` of the budgeted bytes went out.
	Short(written Int)
	// A chunk would have crossed the budget. It was NOT written — `written`
	// (all within budget) is everything that reached the wire.
	Overran(written Int)
}

// Like `drain`, but bounded by a byte budget: writes at most `limit` bytes and
// reports how the body squared with that budget. A chunk that would push past
// the budget is NOT written — the caller (who already advertised the length on
// the wire) sees `Overran` without a single unpromised byte having been sent.
pub fn drain_counted(body Body, sock Socket, limit Int) Result(Drained, NetError) {
	drain_counted_from(body, sock, limit, 0)
}

fn drain_counted_from(body Body, sock Socket, limit Int, written Int) Result(Drained, NetError) {
	match pull(body) {
		Ok(Chunk(data, next)) -> {
			total = written + binary.byte_size(data)
			if total > limit {
				Ok(Overran(written))
			} else {
				match socket.write(sock, data) {
					Ok(_) -> drain_counted_from(next, sock, limit, total)
					Err(e) -> Err(e)
				}
			}
		}
		Ok(Done(_)) -> if written == limit {
			Ok(Complete)
		} else {
			Ok(Short(written))
		}
		Err(e) -> Err(e)
	}
}

// Pump a body out to `sock` framed as chunked transfer-encoding: each non-empty
// chunk becomes "<hex-size>\r\n<data>\r\n"; the stream ends with "0\r\n\r\n", or
// "0\r\n<trailers>\r\n" when Done carries trailers. One vectored write per chunk
// (no concatenation of the payload). An empty chunk is SKIPPED, never emitted —
// a zero-size chunk is the end-of-body marker, so writing one mid-stream would
// truncate the body. Like `drain`, the write parks on backpressure, so a slow
// consumer rate-limits the source. Lets an unknown-length body keep the
// connection alive on HTTP/1.1 instead of framing by close.
pub fn drain_chunked(body Body, sock Socket) Result(Nil, NetError) {
	drain_chunked_prefixed([], body, sock)
}

// Like `drain_chunked`, but `head` (a serialized response head) rides in the
// same vectored write as the first chunk's framing — one syscall for head +
// first chunk, then the stream drains as usual. Empty leading chunks are
// skipped with the head still pending; a body that ends without producing a
// chunk writes head + terminator together.
pub fn drain_chunked_with_head(head Binary, body Body, sock Socket) Result(Nil, NetError) {
	drain_chunked_prefixed([head], body, sock)
}

// The single chunked-drain worker. `prefix` is threaded exactly like
// `drain_prefixed`: it prepends the first vectored write (head + first chunk,
// or head + terminator for an empty body) and is `[]` thereafter. Keeping this
// as one function means the empty-chunk skip and the trailer terminator are
// implemented once, not once per with_head variant.
fn drain_chunked_prefixed(prefix Array(Binary), body Body, sock Socket) Result(Nil, NetError) {
	match pull(body) {
		Ok(Chunk(data, next)) -> match binary.byte_size(data) {
			0 -> drain_chunked_prefixed(prefix, next, sock)
			size -> {
				header = binary.append(binary.from_int_ascii(size, Hex), CRLF)
				match socket.write_parts(sock, [..prefix, header, data, CRLF]) {
					Ok(_) -> drain_chunked_prefixed([], next, sock)
					Err(e) -> Err(e)
				}
			}
		}
		Ok(Done(trailers)) -> {
			terminator = match trailers {
				[] -> [LAST_CHUNK]
				else -> [ZERO, CRLF, ..headers.render(trailers), CRLF]
			}
			match socket.write_parts(sock, [..prefix, ..terminator]) {
				Ok(_) -> Ok(Nil)
				Err(e) -> Err(e)
			}
		}
		Err(e) -> Err(e)
	}
}

// Buffer a whole body into one binary, refusing to grow past `max` bytes. The
// cap is a denial-of-service guard: the size is checked before each append, so
// an over-long body errs (MessageTooLarge) without ever materializing past the
// limit. Done returns whatever has accumulated; trailers are discarded.
pub fn collect(body Body, max Int) Result(Binary, NetError) {
	collect_into(body, max, <<>>)
}

fn collect_into(body Body, max Int, acc Binary) Result(Binary, NetError) {
	match pull(body) {
		Ok(Chunk(data, next)) -> if binary.byte_size(acc) + binary.byte_size(data) > max {
			Err(MessageTooLarge(max))
		} else {
			collect_into(next, max, binary.append(acc, data))
		}
		Ok(Done(_)) -> Ok(acc)
		Err(e) -> Err(e)
	}
}
