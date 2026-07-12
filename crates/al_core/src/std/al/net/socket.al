import al/net/address.{SocketAddress}
import al/net/error.{NetError, UnexpectedEof}
import al/binary
import al/time.{Instant}

pub type Connection

pub type Socket {
	conn Connection
	peer SocketAddress
}

// The outcome of a socket read that did not error at the OS level: either the
// bytes that arrived (always non-empty), or a clean peer close. Encoding
// close as its own constructor — rather than the POSIX `read() == 0` sentinel
// smuggled inside `Ok(Binary)` — makes "forgot to check for close" an
// exhaustiveness error, not a silent infinite loop on a dead socket.
pub type Read {
	Data(bytes Binary)
	Closed
}

// Read whatever data is available, up to `max` bytes. Parks the calling
// process until at least one byte arrives or the peer closes the connection.
@vm(socket__read)
pub fn read(c Socket, max Int) Result(Read, NetError)

// Read exactly `count` bytes, parking until they have all arrived. Errs with
// UnexpectedEof if the peer closes the connection before `count` bytes have
// been read. A `count` of zero or less asks for nothing and returns Ok(<<>>)
// without touching the socket.
pub fn read_exact(c Socket, count Int) Result(Binary, NetError) {
	read_exact_loop(c, count, <<>>)
}

// The guard is `<= 0`, not `== 0`: `read` clamps its `max` to at least one
// byte, so a negative `remaining` handed to it would drain the socket until
// error or EOF instead of terminating.
fn read_exact_loop(c Socket, remaining Int, acc Binary) Result(Binary, NetError) {
	if remaining <= 0 {
		Ok(acc)
	} else {
		match read(c, remaining) {
			Ok(Data(b)) ->
				read_exact_loop(c, remaining - binary.byte_size(b), binary.append(acc, b))
			Ok(Closed) -> Err(UnexpectedEof)
			Err(e) -> Err(e)
		}
	}
}

// Read whatever data is available, up to `max` bytes, but wait no longer than
// `timeout_ms` milliseconds for data to arrive. The deadline is captured once,
// here in AL, as an absolute monotonic timestamp, so an internal re-run on a
// spurious wakeup never resets the clock. Returns Ok(Data(b)) once bytes
// arrive, Ok(Closed) if the peer closes the connection, or Err(TimedOut) once
// the deadline passes with no data.
pub fn read_within(c Socket, max Int, timeout_ms Int) Result(Read, NetError) {
	read_until(c, max, time.add_ms(time.monotonic(), timeout_ms))
}

// Read up to `max` bytes, parking until data arrives or the absolute Instant
// `deadline` (from `time.monotonic()`) is reached, at which point it errs with
// TimedOut.
pub fn read_until(c Socket, max Int, deadline Instant) Result(Read, NetError) {
	read_until_ms(c, max, time.to_deadline_ms(deadline))
}

@vm(socket__read_until)
fn read_until_ms(c Socket, max Int, deadline_ms Int) Result(Read, NetError)

@vm(socket__write)
pub fn write(c Socket, data Binary) Result(Nil, NetError)

// Write every binary in `parts` as one vectored write: the pieces go to the
// kernel in a single syscall without ever being concatenated.
@vm(socket__write_parts)
pub fn write_parts(c Socket, parts Array(Binary)) Result(Nil, NetError)

@vm(socket__close)
pub fn close(c Socket) Result(Nil, NetError)
