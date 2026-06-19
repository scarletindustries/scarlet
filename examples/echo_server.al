// TCP echo server: serve on every core, one process per connection.
// Run it, then in another terminal: nc 127.0.0.1 7777

import al/net
import al/net/socket.{Socket}
import al/binary

// Echo every byte a client sends until it disconnects
fn echo(sock Socket) Nil {
	match socket.read(sock, 65536) {
		Ok(data) -> if binary.byte_size(data) == 0 {
			// Zero bytes means the client closed the connection
			socket.close(sock) or Nil
		} else {
			socket.write(sock, data) or Nil
			echo(sock)
		}
		Err(_) -> socket.close(sock) or Nil
	}
}

// net.serve fans the accept loop out across every core (each binds its own
// SO_REUSEPORT socket) and handles each connection on the core that accepted
// it. The acceptors keep the program alive.
match net.serve('127.0.0.1', 7777, echo) {
	Ok(Nil) -> println('echo server on port 7777')
	Err(e) -> println('serve failed: ${e}')
}
