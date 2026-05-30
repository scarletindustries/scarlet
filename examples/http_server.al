// Run with --experimental-shitty-io
import al/net.{Server}
import al/net/socket.{Socket}
import al/string
import al/binary

const body = 'Hello from AL!'
const header = 'HTTP/1.1 200 OK\r\nContent-Length: ${string.length(body)}\r\nConnection: close\r\n\r\n'
const bin = binary.from_string('${header}${body}')

println('Bin: ${bin}')

fn respond(sock Socket) Nil {
	socket.write(sock, bin) or Nil
	socket.close(sock) or Nil
}

fn serve(server Server, count) {
	match net.accept(server) {
		Ok(sock) -> respond(sock)
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
