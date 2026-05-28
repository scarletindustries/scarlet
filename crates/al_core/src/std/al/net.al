import al/net/address.{SocketAddress}
import al/net/socket.{Socket}

pub type Server

@vm(net__listen)
pub fn listen(host String, port Int) Result(Server, String)

@vm(net__accept)
pub fn accept(s Server) Result(Socket, String)

@vm(net__local_addr)
pub fn local_addr(s Server) Result(SocketAddress, String)

@vm(net__close)
pub fn close(s Server) Result(Nil, String)
