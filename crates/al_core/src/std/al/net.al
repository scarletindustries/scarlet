import al/net/address.{SocketAddress, IpAddress}
import al/net/socket.{Socket}
import al/net/error.{NetError}

pub type Server

@vm(net__listen)
pub fn listen(host String, port Int) Result(Server, NetError)

@vm(net__accept)
pub fn accept(s Server) Result(Socket, NetError)

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
		Ok(ip) -> connect_addr(ip, port)
		Err(e) -> Err(e)
	}
}

// Connect to an already-resolved address. Useful to resolve once and connect
// many times, or to connect to an `IpAddress` obtained elsewhere.
@vm(net__connect)
pub fn connect_addr(ip IpAddress, port Int) Result(Socket, NetError)

@vm(net__local_addr)
pub fn local_addr(s Server) Result(SocketAddress, NetError)

@vm(net__close)
pub fn close(s Server) Result(Nil, NetError)
