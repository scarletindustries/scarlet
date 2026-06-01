import al/net/address.{SocketAddress}
import al/net/socket.{Socket}
import al/net/error.{NetError}

pub type Server

@vm(net__listen)
pub fn listen(host String, port Int) Result(Server, NetError)

@vm(net__accept)
pub fn accept(s Server) Result(Socket, NetError)

// Open a connection to a remote host. `host` may be an IP address or a
// hostname; hostname resolution currently blocks the calling scheduler, so
// prefer IP addresses on hot paths.
@vm(net__connect)
pub fn connect(host String, port Int) Result(Socket, NetError)

@vm(net__local_addr)
pub fn local_addr(s Server) Result(SocketAddress, NetError)

@vm(net__close)
pub fn close(s Server) Result(Nil, NetError)
