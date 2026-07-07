import al/net/address.{SocketAddress, IpAddress}
import al/net/socket.{Socket}
import al/net/error.{NetError}
import al/experiments/scheduler

pub type Server

// Bind a listener to an already-resolved address. The port is validated to be
// in 0..=65535 and the bind never blocks the scheduler (the address is already
// an IP literal — no name resolution happens here).
@vm(net__listen)
pub fn listen_addr(addr SocketAddress) Result(Server, NetError)

// Bind a listener to `host:port`. Hostnames are resolved off-scheduler via
// `resolve` before binding, so a name that requires DNS never stalls the
// scheduler thread. IP literals resolve synchronously.
pub fn listen(host String, port Int) Result(Server, NetError) {
	match resolve(host) {
		Ok(ip) -> listen_addr(SocketAddress(ip, port))
		Err(e) -> Err(e)
	}
}

@vm(net__accept)
pub fn accept(s Server) Result(Socket, NetError)

// Listen on host:port and serve connections across every CPU core in parallel.
//
// Each core binds its own SO_REUSEPORT socket to the same address, so the
// kernel load-balances incoming connections across cores — there is no shared
// accept lock and no single-core accept bottleneck. Every accepted connection
// is handled on the core that accepted it (the connection's socket never moves
// between cores), and `handler` runs once per connection in its own
// lightweight process.
//
// Returns once the listeners are bound and the acceptors are running; the
// acceptors keep the program alive. A bind failure is reported as `Err`.
pub fn serve(host String, port Int, handler fn(Socket) Nil) Result(Nil, NetError) {
	match listen(host, port) {
		Ok(server) -> serve_on(server, handler)
		Err(e) -> Err(e)
	}
}

// Serve connections from a listener already bound with `listen`, fanning the
// accept loop out across every core exactly as `serve` does. Useful to bind on
// port 0, read the kernel-assigned port back with `local_addr`, then serve.
pub fn serve_on(server Server, handler fn(Socket) Nil) Result(Nil, NetError) {
	scheduler.spawn_on_each(fn() accept_loop(server, handler))
	Ok(Nil)
}

// The per-core accept loop: accept a connection, hand it to its own process on
// this same core (so the socket stays local), then loop. One copy of this runs
// on every core, each draining its own kernel accept queue.
fn accept_loop(server Server, handler fn(Socket) Nil) Nil {
	match accept(server) {
		Ok(sock) -> {
			scheduler.spawn_local(fn() handler(sock))
			accept_loop(server, handler)
		}
		Err(_) -> Nil
	}
}

// Resolve `host` to an `IpAddress`. IP literals pass through unchanged; hostname
// resolution (getaddrinfo) runs on the blocking thread pool so it never stalls
// the scheduler.
@vm(net__resolve)
pub fn resolve(host String) Result(IpAddress, NetError)

// Open a connection to a remote host. `host` may be an IP address or a
// hostname; the hostname is resolved off-scheduler via `resolve`, then the
// connection is established with the calling process parked — neither step
// blocks the scheduler.
pub fn connect(host String, port Int) Result(Socket, NetError) {
	match resolve(host) {
		Ok(ip) -> connect_addr(SocketAddress(ip, port))
		Err(e) -> Err(e)
	}
}

// Connect to an already-resolved address. Useful to resolve once and connect
// many times, or to connect to an `IpAddress` obtained elsewhere. The port is
// validated to be in 0..=65535.
@vm(net__connect)
pub fn connect_addr(addr SocketAddress) Result(Socket, NetError)

@vm(net__local_addr)
pub fn local_addr(s Server) Result(SocketAddress, NetError)

@vm(net__close)
pub fn close(s Server) Result(Nil, NetError)
