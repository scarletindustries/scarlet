// Idle-timeout echo server. Like echo_server, but every read is bounded by a
// deadline via socket.read_within: a client that connects then goes silent
// (a slowloris) is dropped when the read times out instead of pinning the
// connection open forever. time.monotonic reports how long each session ran.
//
// Run it, then in another terminal: nc 127.0.0.1 7788

import al/net
import al/net/socket.{Socket, Data, Closed}
import al/time
import al/scheduler

// Echo bytes until the client closes or stays idle past the 30s deadline.
fn echo(sock Socket) Nil {
	match socket.read_within(sock, 65536, 30000) {
		Ok(Data(data)) -> {
			socket.write(sock, data) or Nil
			echo(sock)
		}
		Ok(Closed) -> socket.close(sock) or Nil
		// An Err here is the idle timeout (or a dead socket): drop the client.
		Err(_) -> socket.close(sock) or Nil
	}
}

fn serve(server) {
	match net.accept(server) {
		Ok(sock) -> {
			start = time.monotonic()
			scheduler.spawn(fn() {
				echo(sock)
				println('session lasted ${time.since_ms(time.monotonic(), start)}ms')
			})
		}
		Err(e) -> println('accept failed: ${e}')
	}
	serve(server)
}

match net.listen('127.0.0.1', 7788) {
	Ok(server) -> {
		println('idle-timeout echo server on port 7788')
		serve(server)
	}
	Err(e) -> println('listen failed: ${e}')
}
