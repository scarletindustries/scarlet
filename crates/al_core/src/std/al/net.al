import al/net/address.{SocketAddress, IpAddress}
import al/net/socket.{Socket}
import al/net/error.{
	NetError,
	ConnectionAborted,
	ConnectionReset,
	Errno,
	HostUnreachable,
	NetworkDown,
	NetworkUnreachable,
	NotConnected,
}
import al/scheduler
import al/string

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
// One listening socket, one accept queue, an acceptor process on every core:
// the kernel hands each connection to exactly one accepter, so no core can be
// starved and no connection can be routed where nobody accepts. Every accepted
// connection is handled on the core that accepted it (the connection's socket
// never moves between cores), and `handler` runs once per connection in its
// own lightweight process.
//
// Returns the bound `Server` once the listeners are bound and the acceptors
// are running; the acceptors keep the program alive. The handle is what lets
// the caller shut the listener down later with `close`, or read a kernel-
// assigned port back with `local_addr` after binding port 0. A bind failure
// is reported as `Err`.
pub fn serve(host String, port Int, handler fn(Socket) Nil) Result(Server, NetError) {
	match listen(host, port) {
		Ok(server) -> {
			serve_on(server, handler)
			Ok(server)
		}
		Err(e) -> Err(e)
	}
}

// Serve connections from a listener already bound with `listen`, fanning the
// accept loop out across every core exactly as `serve` does. Useful to bind on
// port 0, read the kernel-assigned port back with `local_addr`, then serve.
// Returns `Nil`: once the listener is bound the acceptors cannot fail here —
// the only fallible step is the bind, which the caller has already done.
pub fn serve_on(server Server, handler fn(Socket) Nil) Nil {
	scheduler.spawn_on_each(fn() accept_loop(server, handler))
}

// The per-core accept loop: accept a connection, hand it to its own process on
// this same core (so the connection's socket stays local), then loop. One copy
// runs on every core, all draining the listener's single shared accept queue —
// spreading the accepts is a locality preference, not a correctness need.
//
// A per-connection accept error (ECONNABORTED, ECONNRESET, an unmapped errno
// such as EMFILE/EPROTO) means one incoming connection was lost, not that the
// listening socket is dead — the loop retries. ENETDOWN/ENETUNREACH/
// EHOSTUNREACH are per-connection too: accept(2) documents them as
// already-pending errors of the ACCEPTED connection — one that died in the
// queue — not as listener death, so they retry as well. An unmapped errno is usually
// descriptor exhaustion (EMFILE/ENFILE), where accept fails without dequeuing
// anything, so that retry backs off for 100ms first — retrying immediately
// would spin this core at full speed against the same full descriptor table.
// NotConnected is the shutdown signal a `net.close` delivers to parked
// acceptors, so the loop exits quietly; any other variant means the listener
// itself died unexpectedly, which is reported before this core stops
// accepting.
fn accept_loop(server Server, handler fn(Socket) Nil) Nil {
	match accept(server) {
		Ok(sock) -> {
			// Close after the handler returns, explicitly: the connection
			// belongs to this acceptor process (its creator), which loops
			// forever — a handler that forgot to close would otherwise leak
			// the socket for the server's whole life. Double-closing a
			// handler that did close is a harmless Err, swallowed here.
			scheduler.spawn_local(fn() {
				handler(sock)
				socket.close(sock) or Nil
			})
			accept_loop(server, handler)
		}
		Err(ConnectionAborted) -> accept_loop(server, handler)
		Err(ConnectionReset) -> accept_loop(server, handler)
		Err(NetworkDown) | Err(NetworkUnreachable) | Err(HostUnreachable) ->
			accept_loop(server, handler)
		Err(Errno(_)) -> {
			scheduler.sleep(100)
			accept_loop(server, handler)
		}
		Err(NotConnected) -> Nil
		Err(e) -> println('accept failed: ${string.inspect(e)}')
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
