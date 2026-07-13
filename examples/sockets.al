// TCP with al/net: a length-prefixed echo protocol, server and client in one
// program.
//
// The listener binds port 0, so the kernel picks a free port and net.local_addr
// reads it back — the program never fights another process for a fixed port.
// The accept loop runs in its own process and the main process drives it as a
// client. Closing the listener at the end wakes the parked acceptor with
// Err(NotConnected), its clean-shutdown signal, and that is what lets this
// program exit.

import al/binary.{Dec}
import al/net.{Server}
import al/net/address.{IpAddress, SocketAddress}
import al/net/error.{InvalidPort, NetError, NotConnected, TimedOut, UnexpectedEof}
import al/net/socket.{Closed, Data, Socket}
import al/scheduler
import al/string
import al/time

// The frame: four ASCII digits of length, then that many bytes of payload. TCP
// is a byte stream with no message boundaries of its own, so a protocol has to
// supply them; a fixed-width length prefix is the smallest one that works.
const HEADER_BYTES = 4

// ---------------------------------------------------------------------------
// The server half.
// ---------------------------------------------------------------------------

fn accept_loop(server Server) Nil {
	match net.accept(server) {
		Ok(sock) -> {
			// One process per connection: a process is a few hundred bytes and
			// about a microsecond to start, so this is the cheap option, not
			// the extravagant one. Accepting parks this process until a client
			// arrives; nothing else in the program stops while it waits.
			scheduler.spawn(fn() serve_one(sock))
			accept_loop(server)
		}
		// Not a failure: this is what net.close delivers to a parked accept.
		Err(NotConnected) -> println('server: listener closed')
		Err(e) -> println('server: accept failed: ${string.inspect(e)}')
	}
}

// The reply goes out as one vectored write: both pieces reach the kernel in a
// single syscall, without being concatenated into a new binary first.
fn serve_one(sock Socket) Nil {
	match read_frame(sock) {
		Ok(payload) -> socket.write_parts(sock, [<<'echo:'>>, payload]) or Nil
		Err(_) -> Nil
	}
	socket.close(sock) or Nil
}

// read_exact keeps reading until it has the bytes it was promised, so a handler
// never sees half a frame — and if the peer hangs up mid-frame it errs with
// UnexpectedEof rather than returning a short read nobody checks.
fn read_frame(sock Socket) Result(Binary, NetError) {
	match socket.read_exact(sock, HEADER_BYTES) {
		Ok(head) -> match binary.parse_int(head, Dec) {
			Ok(length) -> socket.read_exact(sock, length)
			Err(Nil) -> Err(UnexpectedEof)
		}
		Err(e) -> Err(e)
	}
}

// ---------------------------------------------------------------------------
// The client half.
// ---------------------------------------------------------------------------

fn pad(b Binary) Binary {
	if binary.byte_size(b) >= HEADER_BYTES {
		b
	} else {
		pad(binary.append(<<'0'>>, b))
	}
}

// Read until the peer closes. A single read is not a message boundary either:
// it hands back whatever has arrived so far, which is why the reply is
// accumulated rather than trusted whole.
fn read_all(sock Socket, acc Binary) Binary {
	match socket.read(sock, 4096) {
		Ok(Data(chunk)) -> read_all(sock, binary.append(acc, chunk))
		Ok(Closed) -> acc
		Err(_) -> acc
	}
}

// One round trip on a fresh connection: send a framed payload, read the reply.
fn echo_once(server_addr SocketAddress, payload String) String {
	match net.connect_addr(server_addr) {
		Ok(sock) -> {
			bytes = binary.from_string(payload)
			header = pad(binary.from_int_ascii(binary.byte_size(bytes), Dec))
			socket.write(sock, binary.append(header, bytes)) or Nil
			reply = read_all(sock, <<>>)
			socket.close(sock) or Nil
			binary.to_string(reply) or '<not utf-8>'
		}
		Err(e) -> 'connect failed: ${string.inspect(e)}'
	}
}

// The other side of read_exact: ask for more bytes than the server will ever
// send, and the close arrives as a typed error instead of a short read.
fn past_close(server_addr SocketAddress) Nil {
	match net.connect_addr(server_addr) {
		Ok(sock) -> {
			socket.write(sock, <<'0002ab'>>) or Nil
			match socket.read_exact(sock, 1024) {
				Err(UnexpectedEof) -> println('read_exact past close: UnexpectedEof')
				Ok(_) -> println('read_exact: unexpectedly satisfied')
				Err(e) -> println('read_exact: ${string.inspect(e)}')
			}
			socket.close(sock) or Nil
		}
		Err(e) -> println('connect failed: ${string.inspect(e)}')
	}
}

// A peer that connects and then says nothing must not pin a process open for
// ever, so every read gets a deadline. read_within takes a relative timeout;
// read_until takes an absolute Instant, which is what you want when several
// reads share one budget — the deadline is captured once and cannot drift on a
// retry.
fn timeouts(server_addr SocketAddress) Nil {
	match net.connect_addr(server_addr) {
		Ok(sock) -> {
			deadline = time.add_ms(time.monotonic(), 50)
			match socket.read_until(sock, 16, deadline) {
				Err(TimedOut) -> println('read_until: deadline passed')
				Ok(_) -> println('read_until: unexpected data')
				Err(e) -> println('read_until: ${string.inspect(e)}')
			}
			start = time.monotonic()
			match socket.read_within(sock, 16, 50) {
				Err(TimedOut) -> {
					// time.monotonic only ever moves forward, so an elapsed
					// time survives a wall-clock adjustment mid-read.
					elapsed = time.since_ms(time.monotonic(), start)
					println('read_within: waited out the 50ms: ${elapsed >= 50}')
				}
				Ok(_) -> println('read_within: unexpected data')
				Err(e) -> println('read_within: ${string.inspect(e)}')
			}
			socket.close(sock) or Nil
		}
		Err(e) -> println('connect failed: ${string.inspect(e)}')
	}
}

// ---------------------------------------------------------------------------
// Bind, serve, drive, shut down.
// ---------------------------------------------------------------------------

// A SocketAddress pairs an ip with a port, and socket_address is the only way
// to build one: a port outside 0..65535 is rejected there, so it can never
// reach a syscall. to_string renders the pair the way the world writes it.
fn render(ip IpAddress, port Int) String {
	match address.socket_address(ip, port) {
		Ok(addr) -> address.to_string(addr)
		Err(Nil) -> 'port ${port} is out of range'
	}
}

// A second listener, driven by net.serve_on instead of a hand-rolled loop.
fn served(bind_addr SocketAddress) Nil {
	match net.listen_addr(bind_addr) {
		Err(e) -> println('second listen failed: ${string.inspect(e)}')
		Ok(server) -> match net.local_addr(server) {
			Err(e) -> println('second local_addr failed: ${string.inspect(e)}')
			Ok(local) -> {
				net.serve_on(server, serve_one)
				println(echo_once(local, 'served'))
				net.close(server) or Nil
			}
		}
	}
}

// An IpAddress is opaque: it only exists as the output of a parse — or of
// net.resolve, which is the same thing with DNS behind it and passes IP
// literals straight through — so a malformed address cannot reach the VM.
match address.parse('127.0.0.1') {
	Err(Nil) -> println('not an ip literal')
	Ok(ip) -> {
		resolved = net.resolve('127.0.0.1') or ip
		agrees = address.ip_to_string(resolved) == address.ip_to_string(ip)
		println('loopback is v6: ${address.is_v6(ip)}, resolve agrees: ${agrees}')
		println('a constructed address: ${render(ip, 8080)}')
		println('an impossible one: ${render(ip, 70000)}')

		// The same guard one layer up: net.connect builds the address for you,
		// and an impossible port comes back as a typed NetError instead of
		// reaching a syscall.
		match net.connect('127.0.0.1', 70000) {
			Ok(_) -> println('connected on a port that cannot exist')
			Err(InvalidPort) -> println('connect to port 70000: InvalidPort')
			Err(e) -> println('connect failed: ${string.inspect(e)}')
		}

		// Port 0: bind to whatever port the kernel has free.
		match address.socket_address(ip, 0) {
			Err(Nil) -> println('port out of range')
			Ok(bind_addr) -> match net.listen_addr(bind_addr) {
				Err(e) -> println('listen failed: ${string.inspect(e)}')
				Ok(server) -> match net.local_addr(server) {
					Err(e) -> println('local_addr failed: ${string.inspect(e)}')
					Ok(local) -> {
						// The port is the kernel's pick and differs on every
						// run, so print a fact about it, not the number.
						host = address.ip_to_string(address.ip(local))
						assigned = address.port(local) > 0
						println('bound ${host}, kernel picked a port: ${assigned}')

						scheduler.spawn(fn() accept_loop(server))

						println(echo_once(local, 'hello'))
						println(echo_once(local, 'sockets'))
						past_close(local)
						timeouts(local)

						// The accept loop above, spelled out, is what serve_on
						// does for you: it runs an acceptor on every core against
						// a listener that is already bound, and hands each
						// connection to the handler in its own process. net.serve
						// is this plus the bind.
						served(bind_addr)

						net.close(server) or Nil
					}
				}
			}
		}
	}
}
