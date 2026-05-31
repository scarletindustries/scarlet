import al/binary
import al/http/headers.{Header}
import al/http/body.{Body}
import al/http/status
import al/net/socket.{Socket}

// Sans-IO HTTP/1.1 message parsing, framing, and response-head serialization.
//
// Everything here operates on a `Binary` read buffer plus `Int` offsets: it
// never touches a socket and never allocates beyond the zero-copy views it
// hands back (slices over the caller's buffer). It deliberately does NOT
// import al/http — the typed Request/Response live there and that module
// imports this one; keeping h1 on raw bytes + offsets is what keeps the layer
// acyclic. `parse_request` returns the raw pieces (method/target/version/
// headers/consumed-offset); the caller maps them onto its own types.

const CRLF = binary.from_string('\r\n')
const SP = binary.from_string(' ')
const HTAB = binary.from_string('\t')
const COLON = binary.from_string(':')
const ZERO = binary.from_string('0')
const HTTP_1_1 = binary.from_string('HTTP/1.1')
const HTTP_1_0 = binary.from_string('HTTP/1.0')
const STATUS_LINE_PREFIX = binary.from_string('HTTP/1.1 ')

const CONNECTION = binary.from_string('connection')
const CLOSE = binary.from_string('close')
const KEEP_ALIVE = binary.from_string('keep-alive')
const EXPECT = binary.from_string('expect')
const CONTINUE_100 = binary.from_string('100-continue')
const CONTENT_LENGTH = binary.from_string('content-length')
const TRANSFER_ENCODING = binary.from_string('transfer-encoding')

// Total request-head size cap (request line + all header fields). A head that
// grows past this without completing is rejected rather than buffered forever.
const MAX_HEAD = 65536

// The outcome of parsing one request head out of `buf` starting at an offset.
// `Done` threads the consumed-offset so the connection loop can resume parsing
// the next pipelined request from exactly where this one ended. `version` is
// the HTTP minor version: 0 for HTTP/1.0, 1 for HTTP/1.1.
pub type Parsed {
	Done(method Binary target Binary version Int headers Array(Header) consumed Int)
	NeedMore
	Bad(status Int)
}

// How a message body is framed, after applying RFC 7230 section 3.3.3
// precedence and the smuggling rejects. P1 supports Content-Length and
// no-body; Transfer-Encoding is out of scope and is rejected rather than
// silently ignored (ignoring it is a classic request-smuggling vector).
pub type Framing {
	NoBody
	Length(n Int)
	Invalid(status Int)
}

type ReqLine {
	Line(method Binary target Binary version Int)
	BadLine(status Int)
}

type HeaderResult {
	HHeaders(headers Array(Header) consumed Int)
	HNeedMore
	HBad(status Int)
}

fn is_ows(buf Binary, i Int) Bool {
	b = binary.slice_bytes(buf, i, 1)
	b == SP || b == HTAB
}

// First non-OWS index in [lo, hi), or hi if all OWS.
fn trim_left(buf Binary, lo Int, hi Int) Int {
	if lo < hi && is_ows(buf, lo) {
		trim_left(buf, lo + 1, hi)
	} else {
		lo
	}
}

// One past the last non-OWS index in [lo, hi), or lo if all OWS.
fn trim_right(buf Binary, lo Int, hi Int) Int {
	if hi > lo && is_ows(buf, hi - 1) {
		trim_right(buf, lo, hi - 1)
	} else {
		hi
	}
}

// Split METHOD SP request-target SP HTTP-version out of the request line and
// resolve the version token to a minor-version Int.
fn build_line(buf Binary, start Int, sp1 Int, sp2 Int, eol Int) ReqLine {
	method = binary.slice_bytes(buf, start, sp1 - start)
	target = binary.slice_bytes(buf, sp1 + 1, sp2 - sp1 - 1)
	vtok = binary.slice_bytes(buf, sp2 + 1, eol - sp2 - 1)
	if vtok == HTTP_1_1 {
		Line(method, target, 1)
	} else if vtok == HTTP_1_0 {
		Line(method, target, 0)
	} else {
		BadLine(505)
	}
}

fn parse_request_line(buf Binary, start Int, eol Int) ReqLine {
	match binary.index_of(buf, SP, start) {
		None -> BadLine(400)
		Some(sp1) -> if sp1 >= eol || sp1 == start {
			BadLine(400)
		} else {
			match binary.index_of(buf, SP, sp1 + 1) {
				None -> BadLine(400)
				Some(sp2) -> if sp2 >= eol || sp2 <= sp1 + 1 {
					BadLine(400)
				} else {
					build_line(buf, start, sp1, sp2, eol)
				}
			}
		}
	}
}

// Parse one header field at `pos` (its line ends at the CRLF at `crlf`), then
// recurse for the rest. Rejects obs-fold (a line beginning with SP/HTAB) and
// whitespace between the field name and the colon — both are smuggling
// vectors that different parsers disagree on (RFC 7230 section 3.2.4).
fn parse_header_line(buf Binary, pos Int, crlf Int, head_start Int) HeaderResult {
	if is_ows(buf, pos) {
		HBad(400)
	} else {
		match binary.index_of(buf, COLON, pos) {
			None -> HBad(400)
			Some(colon) -> if colon >= crlf || colon == pos || is_ows(buf, colon - 1) {
				HBad(400)
			} else {
				name = binary.slice_bytes(buf, pos, colon - pos)
				vstart = trim_left(buf, colon + 1, crlf)
				vend = trim_right(buf, vstart, crlf)
				value = binary.slice_bytes(buf, vstart, vend - vstart)
				match parse_headers(buf, crlf + 2, head_start) {
					HHeaders(rest, consumed) -> HHeaders([Header(name, value), ..rest], consumed)
					HNeedMore -> HNeedMore
					HBad(s) -> HBad(s)
				}
			}
		}
	}
}

// Walk the header block from `pos`. A blank line ends the block; the consumed
// offset points just past it (the start of the body / next request).
fn parse_headers(buf Binary, pos Int, head_start Int) HeaderResult {
	if pos - head_start > MAX_HEAD {
		HBad(431)
	} else {
		match binary.index_of(buf, CRLF, pos) {
			None -> if binary.byte_size(buf) - head_start > MAX_HEAD {
				HBad(431)
			} else {
				HNeedMore
			}
			Some(crlf) -> if crlf == pos {
				HHeaders([], pos + 2)
			} else {
				parse_header_line(buf, pos, crlf, head_start)
			}
		}
	}
}

// Parse a request head out of `buf` starting at `off`. Incomplete input yields
// NeedMore (read more and call again with the same offset); a malformed head
// yields Bad(status). A leading empty line before the request line is skipped
// (RFC 7230 section 3.5).
pub fn parse_request(buf Binary, off Int) Parsed {
	match binary.index_of(buf, CRLF, off) {
		None -> if binary.byte_size(buf) - off > MAX_HEAD {
			Bad(414)
		} else {
			NeedMore
		}
		Some(eol) -> if eol == off {
			parse_request(buf, off + 2)
		} else {
			match parse_request_line(buf, off, eol) {
				BadLine(s) -> Bad(s)
				Line(method, target, version) -> match parse_headers(buf, eol + 2, off) {
					HHeaders(hs, consumed) -> Done(method, target, version, hs, consumed)
					HNeedMore -> NeedMore
					HBad(s) -> Bad(s)
				}
			}
		}
	}
}

fn header_count(hs Array(Header), name Binary) Int {
	match hs {
		[] -> 0
		[h, ..t] -> if binary.eq_ignore_ascii_case(h.name, name) {
			1 + header_count(t, name)
		} else {
			header_count(t, name)
		}
	}
}

// A Content-Length with a redundant leading zero ("007") is rejected: it is a
// documented inter-parser-disagreement smuggling vector. A lone "0" is fine.
fn leading_zero(v Binary) Bool {
	if binary.byte_size(v) > 1 {
		binary.slice_bytes(v, 0, 1) == ZERO
	} else {
		False
	}
}

// Determine body framing from the request headers, enforcing RFC 7230 section
// 3.3.3 precedence and the MUST-reject smuggling conflicts. Transfer-Encoding
// is unsupported in P1: present alongside Content-Length it is a conflict
// (400); alone it is unimplemented (501). Multiple Content-Length fields, or a
// value that is not a strict run of digits / overflows Int (parse_int returns
// None) / carries a leading zero, are all rejected (400).
pub fn framing(hs Array(Header)) Framing {
	te = header_count(hs, TRANSFER_ENCODING)
	cl = header_count(hs, CONTENT_LENGTH)
	if te > 0 {
		if cl > 0 {
			Invalid(400)
		} else {
			Invalid(501)
		}
	} else {
		match cl {
			0 -> NoBody
			1 -> match headers.get(hs, CONTENT_LENGTH) {
				None -> NoBody
				Some(v) -> match binary.parse_int(v, 10) {
					None -> Invalid(400)
					Some(n) -> if leading_zero(v) {
						Invalid(400)
					} else {
						Length(n)
					}
				}
			}
			else -> Invalid(400)
		}
	}
}

// Build a Content-Length-framed request-body reader over `sock`.
pub fn content_length_reader(sock Socket, n Int) Body {
	body.content_length(sock, n)
}

fn connection_is(hs Array(Header), token Binary) Bool {
	match headers.get(hs, CONNECTION) {
		None -> False
		Some(v) -> binary.eq_ignore_ascii_case(v, token)
	}
}

// Whether the connection must close after this message. HTTP/1.0 defaults to
// close unless `Connection: keep-alive`; HTTP/1.1 defaults to keep-alive
// unless `Connection: close`.
pub fn should_close(version Int, hs Array(Header)) Bool {
	if version == 1 {
		connection_is(hs, CLOSE)
	} else {
		if connection_is(hs, KEEP_ALIVE) { False } else { True }
	}
}

// Whether the client asked the server to send an interim 100 response before
// it streams the request body (`Expect: 100-continue`).
pub fn want_100_continue(hs Array(Header)) Bool {
	match headers.get(hs, EXPECT) {
		None -> False
		Some(v) -> binary.eq_ignore_ascii_case(v, CONTINUE_100)
	}
}

// Serialize a response status line + header block as a parts array for
// socket.write_parts — "HTTP/1.1 <status> <reason>\r\n", the rendered headers,
// then the blank line that terminates the head. Nothing is concatenated; the
// parts share the head constants and the rendered header views.
pub fn serialize_head(code Int, hs Array(Header)) Array(Binary) {
	[
		STATUS_LINE_PREFIX,
		binary.from_int_ascii(code, 10),
		SP,
		status.reason_phrase(code),
		CRLF,
		..headers.render(hs),
		CRLF,
	]
}
