import al/experiments/scheduler
import al/net.{Server}
import al/net/socket.{Socket}
import al/string
import al/binary

const body = 'Hello from AL!'
const header = 'HTTP/1.1 200 OK\r\nContent-Length: ${string.length(body)}\r\nConnection: keep-alive\r\n\r\n'
const bin = binary.from_string('${header}${body}')

// Serve requests on one connection until the client closes it.
fn respond(sock Socket) Nil {
	match socket.read(sock) {
		Ok(req) -> match binary.byte_size(req) {
			// Zero bytes read: the client closed the connection.
			0 -> socket.close(sock) or Nil
			else -> {
				socket.write(sock, bin) or Nil
				respond(sock)
			}
		}
		else -> socket.close(sock) or Nil
	}
}

// Accept connections forever. Each connection is handled by its own process,
// and the runtime spreads those processes across every CPU core.
fn serve(server Server) {
	match net.accept(server) {
		// Ok(sock) -> respond(sock)
		Ok(sock) -> scheduler.spawn(fn() respond(sock))
		Err(e) -> println('accept failed: ${e}')
	}

	serve(server)
}

match net.listen('0.0.0.0', 8080) {
	Err(e) -> println('listen failed: ${e}')
	Ok(server) -> {
		println('Listening on http://localhost:8080')
		serve(server)
	}
}
