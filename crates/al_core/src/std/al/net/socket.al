import al/net/address.{SocketAddress}

pub type Connection

pub type Socket {
	conn Connection
	peer SocketAddress
}

@vm(socket__read)
pub fn read(c Socket) Result(Binary, String)

@vm(socket__write)
pub fn write(c Socket, data Binary) Result(Nil, String)

@vm(socket__close)
pub fn close(c Socket) Result(Nil, String)
