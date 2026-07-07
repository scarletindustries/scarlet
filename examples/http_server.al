// An HTTP server built directly on al/net sockets: served on every core, one
// process per connection, pipelined requests answered with a single vectored
// write.

import al/net
import al/net/socket.{Socket}
import al/string
import al/binary
import al/list
import al.{Ok}

const body = 'Hello from AL!'
const header = 'HTTP/1.1 200 OK\r\nContent-Length: ${string.length(body)}\r\nConnection: keep-alive\r\n\r\n'
const response = binary.from_string('${header}${body}')

// How many complete HTTP requests this read contains. Requests end with a
// blank line; clients may pipeline several into one packet.
fn count_requests(data Binary) Int {
	match binary.to_string(data) {
		Some(text) -> list.length(string.split(text, '\r\n\r\n')) - 1
		None -> 0
	}
}

// One response per pipelined request, sent to the kernel as a single
// vectored write; the parts are never concatenated.
fn responses(n Int, parts Array(Binary)) Array(Binary) {
	match n {
		0 -> parts
		else -> responses(n - 1, [response, ..parts])
	}
}

// Serve one connection: answer every request it sends until it closes.
fn respond(sock Socket) Nil {
	match socket.read(sock, 65536) {
		Ok(data) -> match binary.byte_size(data) {
			// Zero bytes read: the client closed the connection.
			0 -> socket.close(sock) or Nil
			else -> {
				n = count_requests(data)
				match n {
					// No complete request yet; read more.
					0 -> respond(sock)
					else -> {
						socket.write_parts(sock, responses(n, [])) or Nil
						respond(sock)
					}
				}
			}
		}
		else -> socket.close(sock) or Nil
	}
}

// net.serve runs an accept loop on every core, each binding its own
// SO_REUSEPORT socket so the kernel load-balances connections across cores;
// every connection is handled on the core that accepted it.
match net.serve('0.0.0.0', 8080, respond) {
	Ok(_) -> println('Listening on http://localhost:8080')
	Err(e) -> println('serve failed: ${e}')
}
