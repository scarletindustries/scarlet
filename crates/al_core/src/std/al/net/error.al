// Typed errors for network and socket operations, mirroring Rust's
// std::io::ErrorKind (the POSIX/errno failure set) so a caller can `match` the
// exact failure rather than parse a message string.
pub type NetError {
	// No data (or no connection) arrived before the deadline. (ETIMEDOUT)
	TimedOut
	// The peer actively refused the connection. (ECONNREFUSED)
	ConnectionRefused
	// The peer reset an established connection. (ECONNRESET)
	ConnectionReset
	// The connection was aborted (locally or by the network). (ECONNABORTED)
	ConnectionAborted
	// The socket is not connected. (ENOTCONN)
	NotConnected
	// The write end found the read end gone. (EPIPE)
	BrokenPipe
	// The local address is already bound by another socket. (EADDRINUSE)
	AddrInUse
	// The requested local address is not available on this host. (EADDRNOTAVAIL)
	AddrNotAvailable
	// The local network interface is down. (ENETDOWN)
	NetworkDown
	// No route to the destination network. (ENETUNREACH)
	NetworkUnreachable
	// No route to the destination host. (EHOSTUNREACH)
	HostUnreachable
	// The operation was not permitted (e.g. binding a privileged port). (EACCES)
	PermissionDenied
	// The peer closed the connection before a required number of bytes arrived.
	UnexpectedEof
	// A message or body exceeded the maximum size the reader will buffer.
	MessageTooLarge
	// A write was handed a binary that is not a whole number of bytes.
	UnalignedBinary
	// An OS error with no dedicated variant above, carrying its raw errno so it
	// is still matchable — never a string.
	Errno(code Int)
}
