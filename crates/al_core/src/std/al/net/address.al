pub type IpAddress {
	V4(addr String)
	V6(addr String)
}

pub type SocketAddress {
	ip IpAddress
	port Int
}

pub fn ip_to_string(ip IpAddress) String {
	match ip {
		V4(a) -> a
		V6(a) -> a
	}
}

pub fn to_string(sa SocketAddress) String {
	match sa.ip {
		V4(a) -> '${a}:${sa.port}'
		V6(a) -> '[${a}]:${sa.port}'
	}
}
