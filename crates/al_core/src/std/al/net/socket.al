import al/net/address.{SocketAddress}
import al/binary

pub type Connection

pub type Socket {
	conn Connection
	peer SocketAddress
}

// Read whatever data is available, up to `max` bytes. Parks the calling
// process until at least one byte arrives; a zero-byte result means the
// peer closed the connection.
@vm(socket__read)
pub fn read(c Socket, max Int) Result(Binary, String)

// Read exactly `count` bytes, parking until they have all arrived.
// Errs if the peer closes the connection first.
pub fn read_exact(c Socket, count Int) Result(Binary, String) {
	read_exact_loop(c, count, <<>>)
}

fn read_exact_loop(c Socket, remaining Int, acc Binary) Result(Binary, String) {
	match remaining {
		0 -> Ok(acc)
		else -> match read(c, remaining) {
			Ok(data) -> match binary.byte_size(data) {
				0 -> Err('connection closed early')
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

@vm(socket__write)
pub fn write(c Socket, data Binary) Result(Nil, String)

// Write every binary in `parts` as one vectored write: the pieces go to the
// kernel in a single syscall without ever being concatenated.
@vm(socket__write_parts)
pub fn write_parts(c Socket, parts Array(Binary)) Result(Nil, String)

@vm(socket__close)
pub fn close(c Socket) Result(Nil, String)
