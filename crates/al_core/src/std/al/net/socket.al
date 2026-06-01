import al/net/address.{SocketAddress}
import al/net/error.{NetError, UnexpectedEof}
import al/binary
import al/time

pub type Connection

pub type Socket {
	conn Connection
	peer SocketAddress
}

// Read whatever data is available, up to `max` bytes. Parks the calling
// process until at least one byte arrives; a zero-byte result means the
// peer closed the connection.
@vm(socket__read)
pub fn read(c Socket, max Int) Result(Binary, NetError)

// Read exactly `count` bytes, parking until they have all arrived. Errs if the
// peer closes the connection before `count` bytes have been read.
pub fn read_exact(c Socket, count Int) Result(Binary, NetError) {
	read_exact_loop(c, count, <<>>)
}

fn read_exact_loop(c Socket, remaining Int, acc Binary) Result(Binary, NetError) {
	match remaining {
		0 -> Ok(acc)
		else -> match read(c, remaining) {
			Ok(data) -> match binary.byte_size(data) {
				0 -> Err(UnexpectedEof)
				else -> read_exact_loop(
					c,
					remaining - binary.byte_size(data),
					binary.append(acc, data),
				)
			}
			Err(e) -> Err(e)
		}
	}
}

// Read whatever data is available, up to `max` bytes, but wait no longer than
// `timeout_ms` milliseconds for data to arrive. The deadline is captured once,
// here in AL, as an absolute monotonic timestamp, so an internal re-run on a
// spurious wakeup never resets the clock. Returns Ok(data) once bytes arrive,
// Ok(<<>>) if the peer closes the connection, or Err(TimedOut) once the
// deadline passes with no data.
pub fn read_within(c Socket, max Int, timeout_ms Int) Result(Binary, NetError) {
	read_until(c, max, time.monotonic() + timeout_ms)
}

// Read up to `max` bytes, parking until data arrives or the absolute monotonic
// deadline `deadline_ms` (compare against `time.monotonic`) is reached, at which
// point it errs with TimedOut. An Ok(<<>>) result signals a peer close,
// matching `read`.
@vm(socket__read_until)
fn read_until(c Socket, max Int, deadline_ms Int) Result(Binary, NetError)

@vm(socket__write)
pub fn write(c Socket, data Binary) Result(Nil, NetError)

// Write every binary in `parts` as one vectored write: the pieces go to the
// kernel in a single syscall without ever being concatenated.
@vm(socket__write_parts)
pub fn write_parts(c Socket, parts Array(Binary)) Result(Nil, NetError)

@vm(socket__close)
pub fn close(c Socket) Result(Nil, NetError)
