// An IP address. Opaque: the `V4`/`V6` variants carry a canonical textual
// address that only the VM guarantees is well-formed, so construction goes
// through `parse` (or `al/net.resolve`) and the type system rules out
// `V4("garbage")` reaching the VM.
pub opaque type IpAddress {
	V4(addr String)
	V6(addr String)
}

// A socket address: an IP plus a port. Opaque for the same reason as
// `IpAddress`: the VM trusts the port to be in 0..=65535, so construction
// goes through `socket_address` and an out-of-range port is unrepresentable
// rather than a deep-VM `InvalidPort`.
pub opaque type SocketAddress {
	ip IpAddress
	port Int
}

// Build a `SocketAddress`, rejecting ports outside 0..=65535 with `Err(Nil)`.
pub fn socket_address(ip IpAddress, port Int) Result(SocketAddress, Nil) {
	if port < 0 || port > 65535 {
		Err(Nil)
	} else {
		Ok(SocketAddress(ip, port))
	}
}

pub fn ip(a SocketAddress) IpAddress {
	a.ip
}

pub fn port(a SocketAddress) Int {
	a.port
}

@vm(address__parse)
fn parse_raw(s String) Option(IpAddress)

// Parse an IPv4 or IPv6 literal into an `IpAddress`. Anything that is not a
// valid IP literal (hostnames included; resolve those with `al/net.resolve`)
// is an `Err(Nil)` — a parse is a fallible operation, so it returns Result
// per the stdlib convention.
pub fn parse(s String) Result(IpAddress, Nil) {
	match parse_raw(s) {
		Some(ip) -> Ok(ip)
		None -> Err(Nil)
	}
}

pub fn ip_to_string(ip IpAddress) String {
	match ip {
		V4(a) -> a
		V6(a) -> a
	}
}

pub fn is_v6(ip IpAddress) Bool {
	match ip {
		V4(_) -> False
		V6(_) -> True
	}
}

pub fn to_string(sa SocketAddress) String {
	match sa.ip {
		V4(a) -> '${a}:${sa.port}'
		V6(a) -> '[${a}]:${sa.port}'
	}
}
