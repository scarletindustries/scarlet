// An IP address. Opaque: the `V4`/`V6` variants carry a canonical textual
// address that only the VM guarantees is well-formed, so construction goes
// through `parse` (or `al/net.resolve`) and the type system rules out
// `V4("garbage")` reaching the VM.
pub opaque type IpAddress {
	V4(addr String)
	V6(addr String)
}

pub type SocketAddress {
	ip IpAddress
	port Int
}

// Parse an IPv4 or IPv6 literal into an `IpAddress`. Returns `None` for
// anything that is not a valid IP literal (hostnames included; resolve those
// with `al/net.resolve`).
@vm(address__parse)
pub fn parse(s String) Option(IpAddress)

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
