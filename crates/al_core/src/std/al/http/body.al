import al/binary
import al/int
import al/net/socket.{Socket}
import al/http/headers.{Header}

// A message body as a pull thunk: calling it advances the stream by one step,
// returning the next chunk plus its own continuation, or the end of the body
// with any trailers. The continuation lives *inside* the result so a body is a
// plain `fn() Result(Pull, String)` — a transparent alias, never a nominal
// wrapper, so there is no per-chunk allocation beyond the chunk itself. Errors
// are `String` to pass socket errors through verbatim.
//
// `Pull` and `Body` are mutually recursive and must stay in one module: the
// continuation in `Chunk` is a `Body`, and a `Body` yields a `Pull`. Splitting
// them would force an import cycle, which the compiler rejects.
pub type Pull {
	Chunk(data Binary next Body)
	Done(trailers Array(Header))
}

pub type Body = fn() Result(Pull, String)

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
// A zero-byte read before `n` is reached means the peer closed mid-body, which
// is an error, not a clean end.
pub fn content_length(reader Socket, n Int) Body {
	fn() match n {
		0 -> Ok(Done([]))
		else -> match socket.read(reader, int.min(n, 65536)) {
			Ok(chunk) -> {
				got = binary.byte_size(chunk)
				match got {
					0 -> Err('connection closed mid-body')
					else -> Ok(Chunk(chunk, content_length(reader, n - got)))
				}
			}
			Err(e) -> Err(e)
		}
	}
}

// A reader that streams `sock` until the peer closes it: each step reads up to
// 64 KiB, and a zero-byte read is the clean end of the body (Done). Used for
// responses framed by connection close rather than a known length.
pub fn eof_reader(sock Socket) Body {
	fn() match socket.read(sock, 65536) {
		Ok(chunk) -> match binary.byte_size(chunk) {
			0 -> Ok(Done([]))
			else -> Ok(Chunk(chunk, eof_reader(sock)))
		}
		Err(e) -> Err(e)
	}
}

// Advance a body by one step. A body is just a thunk; this names the operation.
pub fn pull(b Body) Result(Pull, String) {
	b()
}

// Pump a whole body out to `sock`: pull a chunk, write it, recurse. The write
// parks on backpressure, so a slow consumer naturally rate-limits the source —
// the next chunk is not pulled until the previous one has drained. Returns the
// trailers carried by Done. Nothing is buffered: memory stays O(chunk).
pub fn drain(body Body, sock Socket) Result(Array(Header), String) {
	match pull(body) {
		Ok(Chunk(data, next)) -> match socket.write(sock, data) {
			Ok(_) -> drain(next, sock)
			Err(e) -> Err(e)
		}
		Ok(Done(trailers)) -> Ok(trailers)
		Err(e) -> Err(e)
	}
}

// Buffer a whole body into one binary, refusing to grow past `max` bytes. The
// cap is a denial-of-service guard: the size is checked before each append, so
// an over-long body errs without ever materializing past the limit. Done
// returns whatever has accumulated; trailers are discarded.
pub fn collect(body Body, max Int) Result(Binary, String) {
	collect_into(body, max, binary.from_string(''))
}

fn collect_into(body Body, max Int, acc Binary) Result(Binary, String) {
	match pull(body) {
		Ok(Chunk(data, next)) -> if binary.byte_size(acc) + binary.byte_size(data) > max {
			Err('body exceeds maximum size')
		} else {
			collect_into(next, max, binary.append(acc, data))
		}
		Ok(Done(_)) -> Ok(acc)
		Err(e) -> Err(e)
	}
}
