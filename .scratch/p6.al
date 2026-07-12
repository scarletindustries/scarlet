import al/net
import al/net/address
import al/net/socket.{Data, Closed}
import al/scheduler
import al/binary
import al/result
import al/string.{inspect}

fn serve_one(server Server) Nil {
	match net.accept(server) {
		Err(e) -> println('accept failed: ${inspect(e)}')
		Ok(conn) -> {
			match socket.read(conn, 64) {
				Ok(Data(bytes)) -> {
					Nil = socket.write(
						conn,
						binary.append(binary.from_string('echo: '), bytes),
					) or _e -> Nil
					Nil
				}
				Ok(Closed) -> Nil
				Err(_) -> Nil
			}
			Nil = socket.close(conn) or _e -> Nil
			Nil
		}
	}
}

fn talk(port Int) Nil {
	match net.connect('127.0.0.1', port) {
		Err(e) -> println('connect failed: ${inspect(e)}')
		Ok(client) -> {
			Nil = socket.write(client, binary.from_string('ping')) or _e -> Nil
			match socket.read(client, 64) {
				Ok(Data(b)) -> println(inspect(binary.to_string(b)))
				Ok(Closed) -> println('closed')
				Err(e) -> println('read failed: ${inspect(e)}')
			}
			socket.close(client) or Nil
		}
	}
}

match net.listen('127.0.0.1', 0) {
	Err(e) -> println('listen failed: ${inspect(e)}')
	Ok(server) -> {
		port = result.map(net.local_addr(server), address.port) or 0
		scheduler.spawn(fn() serve_one(server))
		talk(port)
		Nil = net.close(server) or _e -> Nil
		println('done')
	}
}
