// Bit syntax. A `<<>>` literal builds bytes out of fields whose widths are
// measured in *bits*, and the same syntax used as a pattern takes them apart
// again — so a whole frame header is one expression to write and one match arm
// to read. Below: a metrics agent's wire format, encoded, decoded, and then
// scanned byte-by-byte without ever building a String.

import al/binary.{Dec, Hex, Radix}
import al/string

// One frame on the wire:
//
//   'ALM'    3 bytes  magic
//   version  4 bits   protocol version
//   flags    4 bits   bit 0 set = a gauge, clear = a counter
//   length  16 bits   payload length, in bytes
//   payload  `length` bytes of ASCII `name=value unit`
const VERSION = 1
const GAUGE = 1

type Sample {
	name String
	value Int
	unit String
	gauge Bool
}

// Building the frame is a single expression. A bare string segment ('ALM')
// contributes its UTF-8 bytes; an integer segment encodes big-endian into the
// width it is given, so version and flags share one byte; and a `:binary`
// segment splices in bytes as they are.
fn encode(s Sample) Binary {
	payload = binary.from_string('${s.name}=${s.value} ${s.unit}')
	flags = match s.gauge {
		True -> GAUGE
		False -> 0
	}
	<<'ALM', VERSION:4, flags:4, binary.byte_size(payload):16, payload:binary>>
}

// The header alone, ignoring the payload bytes: `_:binary` consumes whatever
// is left, so this arm matches a frame of any length.
fn header(frame Binary) String {
	match frame {
		<<'ALM', version:4, flags:4, len:16, _:binary>> -> {
			'v${version} flags=${flags} len=${len}'
		}
		else -> 'not an ALM frame'
	}
}

// Decoding mirrors the build. A literal segment ('ALM') matches only those
// bytes, `bytes(len)` takes exactly as many bytes as the length field just
// announced — a frame whose length field lies falls through to `else` rather
// than reading past its end — and a guard rejects a version this build does
// not speak.
fn decode(frame Binary) Result(Sample, String) {
	match frame {
		<<'ALM', v:4, _:4, _:16, _:binary>> if v != VERSION -> { Err('unsupported version ${v}') }
		<<'ALM', _:4, flags:4, len:16, payload:bytes(len)>> -> { parse_payload(payload, flags) }
		else -> Err('truncated or foreign frame')
	}
}

// The payload is ASCII, so it is parsed as bytes: find the '=' (61) and the
// ' ' (32) with index_of, take the fields around them with slice_bytes, and
// read the number straight out of its bytes with parse_int.
fn parse_payload(payload Binary, flags Int) Result(Sample, String) {
	match (binary.index_of(payload, <<'='>>, 0), binary.index_of(payload, <<' '>>, 0)) {
		(Some(eq), Some(sp)) -> {
			name = binary.slice_bytes(payload, 0, eq)
			digits = binary.slice_bytes(payload, eq + 1, sp - eq - 1)
			unit = binary.slice_bytes(payload, sp + 1, binary.byte_size(payload) - sp - 1)
			match (binary.to_string(name), binary.parse_int(digits, Dec), binary.to_string(unit)) {
				(Ok(n), Ok(v), Ok(u)) -> Ok(Sample(n, v, u, flags == GAUGE))
				else -> Err('malformed payload')
			}
		}
		else -> Err('payload is not name=value unit')
	}
}

fn show(s Sample) String {
	kind = match s.gauge {
		True -> 'gauge'
		False -> 'counter'
	}
	'${kind} ${s.name} = ${s.value} ${s.unit}'
}

fn round_trip(s Sample) Nil {
	frame = encode(s)
	println('send    ${show(s)}')
	println('  wire  ${string.inspect(frame)}')
	println('  head  ${header(frame)}')
	match decode(frame) {
		Ok(got) -> println('  recv  ${show(got)}')
		Err(reason) -> println('  drop  ${reason}')
	}
}

println('-- frames --')
round_trip(Sample('http.requests', 4210, 'req', False))
round_trip(Sample('heap.used', 65535, 'MiB', True))

// The same match rejects everything that is not a frame this build speaks: a
// length field claiming 99 bytes that are not there, a newer protocol, and
// bytes from some other protocol entirely.
println('')
println('-- rejects --')
fn reject(what String, frame Binary) Nil {
	match decode(frame) {
		Ok(s) -> println('${what}: accepted ${show(s)}')
		Err(reason) -> println('${what}: ${reason}')
	}
}

reject('lying length ', <<'ALM', VERSION:4, 0:4, 99:16, 104, 105>>)
reject('from the futu', <<'ALM', 3:4, 0:4, 0:16>>)
reject('another proto', <<'GET / HTTP/1.1'>>)
reject('empty         ', <<>>)

// Frames do not arrive one per read: a socket hands over whatever bytes have
// turned up, which may be two frames and the front of a third. `..rest` binds
// everything past the frame the pattern just consumed, so the buffer is peeled
// one frame at a time and whatever will not yet parse is left for the next
// read. `<<>>` matches only an empty binary, which is how the loop ends.
fn drain(buffer Binary, seen Int) Int {
	match buffer {
		<<>> -> seen
		<<'ALM', _:4, _:4, len:16, payload:bytes(len), ..rest>> -> {
			text = binary.to_string(payload) or '<not text>'
			println('  frame ${seen + 1}: ${text}')
			drain(rest, seen + 1)
		}
		else -> {
			println('  ${binary.byte_size(buffer)} bytes left over: wait for more')
			seen
		}
	}
}

println('')
println('-- streaming --')
buffered = binary.append(
	binary.append(
		encode(Sample('cpu.busy', 91, 'pct', True)),
		encode(Sample('disk.reads', 12, 'iops', False)),
	),
	<<'ALM', VERSION:4, 0:4>>,
)
println('read ${binary.byte_size(buffered)} bytes')
println('drained ${drain(buffered, 0)} whole frames')

// A `<<>>` pattern with no `..` rest is an exact-length match, so this arm
// fires only on a one-byte binary — the two nibbles of a byte, read apart.
fn nibbles(b Binary) String {
	match b {
		<<hi:4, lo:4>> -> 'hi=${hi} lo=${lo}'
		else -> 'not exactly one byte'
	}
}

println('')
println('-- widths --')
println('nibbles of <<161>>:   ${nibbles(<<161>>)}')
println('nibbles of <<1, 2>>:  ${nibbles(<<1, 2>>)}')

// Integer segments are unsigned, big-endian, and truncate to the width they
// are given: only the low N bits survive.
println('<<300:8>> truncates:  ${string.inspect(<<300:8>>)}')
println('<<-1:8>> wraps:       ${string.inspect(<<-1:8>>)}')
println('<<65535:16>>:         ${string.inspect(<<65535:16>>)}')

// Widths need not be literal, and they need not be whole bytes. A compact
// field writes its own bit width, then the value in exactly that many bits;
// `size(w)` reads the width back out of the binary being matched.
fn compact(n Int, width Int) Binary {
	<<width:8, n:size(width)>>
}

fn read_compact(b Binary) Result(Int, Nil) {
	match b {
		<<width:8, n:size(width)>> -> Ok(n)
		else -> Err(Nil)
	}
}

packed = compact(7, 4)
println('compact(7, 4):        ${string.inspect(packed)}')
println('  bit_size            ${binary.bit_size(packed)}')
println('  byte_size           ${binary.byte_size(packed)}')
println('  read back           ${read_compact(packed)}')
println('compact(4210, 13):    ${string.inspect(compact(4210, 13))}')
println('  read back           ${read_compact(compact(4210, 13))}')

// Text segments. In an expression `:utf8` writes a whole string's bytes; in a
// pattern it reads ONE codepoint, which is how a scanner peeks at the first
// character of a payload without decoding the rest.
fn tag(label String, body Binary) Binary {
	binary.append(<<label:utf8>>, body)
}

fn first_codepoint(b Binary) Option(Int) {
	match b {
		<<c:utf8, ..>> -> Some(c)
		else -> None
	}
}

println('')
println('-- text --')
tagged = tag('µs:', <<'12'>>)
println('tag:                  ${string.inspect(tagged)}')
println('  as a string         ${binary.to_string(tagged)}')
println('  first codepoint     ${first_codepoint(tagged)}')
println('  of <<>>             ${first_codepoint(<<>>)}')
println('  of <<104, 105>>     ${first_codepoint(<<104, 105>>)}')

// A binary is bytes, not text: bytes that are not valid UTF-8 have no String.
println('to_string of raw bytes: ${binary.to_string(<<255, 254>>)}')

// Byte-level scanning of a header block, without allocating a String for any
// of it. This is what an HTTP-shaped parser does: find the separator, take
// views either side of it, compare case-insensitively, read numbers.
headers = <<'Content-Length: 4210\r\nRetry-After: ff\r\n'>>

println('')
println('-- scanning --')
println('bytes:                ${binary.byte_size(headers)}')
println('index_of ": ":        ${binary.index_of(headers, <<': '>>, 0)}')
println('index_of "\\r\\n":      ${binary.index_of(headers, <<13, 10>>, 0)}')
println('index_of "zz":        ${binary.index_of(headers, <<'zz'>>, 0)}')

name = binary.slice_bytes(headers, 0, 14)
println('field name:           ${string.inspect(binary.to_string(name))}')
println('lowercased:           ${string.inspect(binary.to_string(binary.to_ascii_lower(name)))}')
println('is content-length:    ${binary.eq_ignore_ascii_case(name, <<'content-length'>>)}')
println('is content-type:      ${binary.eq_ignore_ascii_case(name, <<'content-type'>>)}')

// slice_bytes is total: an out-of-range view is empty rather than an error, so
// a scanner can take a field by offset without handling a failure per call.
println('past the end:         ${string.inspect(binary.slice_bytes(headers, 900, 4))}')

// byte_at is total too, and returns -1 rather than boxing an Option per probe.
println('byte_at 0:            ${binary.byte_at(headers, 0)}')
println('byte_at 900:          ${binary.byte_at(headers, 900)}')

// Numbers are read straight out of the bytes, in whichever base the protocol
// wrote them: decimal for a length, hex for a retry-after cookie.
println('parse_int(Dec):       ${binary.parse_int(binary.slice_bytes(headers, 16, 4), Dec)}')
println('"ff" as Dec:          ${binary.parse_int(<<'ff'>>, Dec)}')
println('"ff" as Hex:          ${binary.parse_int(<<'ff'>>, Hex)}')
// Dec and Hex are the two constructors of `Radix`, so a helper that works in
// either base takes one as an ordinary argument.
fn ascii(n Int, base Radix) String {
	string.inspect(binary.from_int_ascii(n, base))
}

println('255 to Dec ascii:     ${ascii(255, Dec)}')
println('255 to Hex ascii:     ${ascii(255, Hex)}')
println('hex ascii round trip: ${binary.parse_int(binary.from_int_ascii(4210, Hex), Hex)}')

// `slice` is the bit-precise version — offsets and lengths in bits — and it
// is the one binary operation that can fail: a view past the end is Err(Nil).
println('slice bits 0..4:      ${binary.slice(headers, 0, 4)}')
println('slice bits 8000..8:   ${binary.slice(headers, 8000, 8)}')
