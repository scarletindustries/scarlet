// Run with --experimental-shitty-io
import al/net.{Server}
import al/net/socket.{Socket}
import al/net/address
import al/string
import al/binary

fn respond(sock Socket, body String) Nil {
	header = 'HTTP/1.1 200 OK\r\nContent-Length: ${string.length(body)}\r\nConnection: close\r\n\r\n'
	socket.write(sock, binary.from_string('${header}${body}')) or Nil
	socket.close(sock) or Nil
}

fn serve(server Server, count) {
	match net.accept(server) {
		Ok(sock) -> {
			println('Received connection from ${address.to_string(sock.peer)}')
			respond(sock, 'Hello from AL!')
		}
		Err(e) -> println('accept failed: ${e}')
	}

	serve(server, count)
}

match net.listen('0.0.0.0', 8080) {
	Err(e) -> println('listen failed: ${e}')
	Ok(server) -> {
		println('Listening on http://localhost:8080')
		serve(server, 0)
	}
}
