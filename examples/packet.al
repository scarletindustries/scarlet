// Build a network packet with bit syntax, then take it apart with a binary
// pattern match. Segment widths are in bits, so the version and kind fields
// below share a single byte on the wire.

import al/binary
import al/string

// Wire format: the magic string "AL" (2 bytes), version (4 bits), kind
// (4 bits), payload length in bytes (16 bits), then the payload itself.
const VERSION = 1

type Message {
	Ping
	Data(text String)
	Bye
}

// Building a packet is one expression. A string segment contributes its UTF-8
// bytes; integer segments encode big-endian into the width they are given; a
// :binary segment splices bytes in as-is.
fn encode(msg Message) Binary {
	(kind, payload) = match msg {
		Ping -> (0, <<>>)
		Data(text) -> (1, binary.from_string(text))
		Bye -> (2, <<>>)
	}
	<<'AL', VERSION:4, kind:4, binary.byte_size(payload):16, payload:binary>>
}

// Read the raw header fields, ignoring the payload bytes. A string literal in
// a pattern matches its bytes as a prefix — one compare for the whole magic.
fn header(wire Binary) String {
	match wire {
		<<'AL', version:4, kind:4, len:16, _:binary>> -> {
			'version ${version}, kind ${kind}, ${len} payload bytes'
		}
		else -> 'not a packet'
	}
}

// Parse a packet back into a Message. The pattern mirrors the build syntax:
// a literal segment like 'AL' or 1:4 matches only that value, and bytes(len)
// takes exactly the number of bytes the length field announced.
fn decode(wire Binary) Result(Message, String) {
	match wire {
		<<'AL', v:4, _:4, _:16, _:binary>> if v != VERSION -> Err('unsupported version ${v}')
		<<'AL', _:4, 0:4, 0:16>> -> Ok(Ping)
		<<'AL', _:4, 1:4, len:16, payload:bytes(len)>> -> {
			match binary.to_string(payload) {
				Ok(text) -> Ok(Data(text))
				Err(_) -> Err('payload is not text')
			}
		}
		<<'AL', _:4, 2:4, 0:16>> -> Ok(Bye)
		else -> Err('truncated or corrupt packet')
	}
}

// The receiving side: show the header fields, then what the packet decoded to.
fn receive(wire Binary) Nil {
	println('  header:  ${header(wire)}')
	match decode(wire) {
		Ok(msg) -> println('  decoded: ${string.inspect(msg)}')
		Err(reason) -> println('  refused: ${reason}')
	}
}

// Encode a message, show the bytes it became, then decode it back.
fn round_trip(msg Message) Nil {
	wire = encode(msg)
	println('send ${string.inspect(msg)}')
	println('  wire:    ${string.inspect(wire)}')
	receive(wire)
}

round_trip(Data('hi from al'))
round_trip(Ping)
round_trip(Bye)
println('')

// Forged packets are rejected by the same match: one whose length field
// claims 99 bytes it does not have, one from a newer protocol version, and
// one that is not this protocol at all (wrong magic).
println('receive a packet with a lying length field')
receive(<<'AL', VERSION:4, 1:4, 99:16, 104, 105>>)
println('receive a version 3 packet')
receive(<<'AL', 3:4, 0:4, 0:16>>)
println('receive bytes from some other protocol')
receive(<<'HTTP/1.1 200 OK'>>)
