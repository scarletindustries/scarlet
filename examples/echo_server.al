// TCP echo server: listen, accept in a loop, one process per connection.
// Run it, then in another terminal: nc 127.0.0.1 7777

import al/net
import al/net/socket.{Socket}
import al/binary
import al/experiments/scheduler

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

fn serve(server) {
	match net.accept(server) {
		Ok(sock) -> scheduler.spawn(fn() echo(sock))
		Err(e) -> println('accept failed: ${e}')
	}
	serve(server)
}

match net.listen('127.0.0.1', 7777) {
	Ok(server) -> {
		println('echo server on port 7777')
		serve(server)
	}
	Err(e) -> println('listen failed: ${e}')
}
