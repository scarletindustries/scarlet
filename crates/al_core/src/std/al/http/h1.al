import al/http/headers.{Headers}
import al/http/status

// Sans-IO HTTP/1.1 message parsing, framing, and response-head serialization.
//
// Everything here operates on a `Binary` read buffer plus `Int` offsets: it
// never touches a socket and never allocates beyond the zero-copy views it
// hands back (slices over the caller's buffer). It deliberately does NOT
// import al/http — the typed Request/Response live there and that module
// imports this one; keeping h1 on raw bytes + offsets is what keeps the layer
// acyclic. `parse_request` returns the raw pieces (method/target/version/
// headers/consumed-offset); the caller maps them onto its own types.
//
// The byte scanning itself — finding the CRLF/SP/colon boundaries, building
// the field views, the RFC 7230 §3.3.3 framing precedence — runs as native VM
// builtins: one
// op per request head instead of one op per byte. Every protocol DECISION
// (keep-alive, 100-continue, how to frame the response) stays in AL, here and
// in al/http.

// The HTTP/1.x minor version on the wire. HTTP/2 speaks a different framing
// layer entirely and never reaches this parser, so it is deliberately absent —
// a `Version` value proves the message was HTTP/1.0 or HTTP/1.1.
pub type Version {
	Http10
	Http11
}

// The `Connection` and `Expect` token-list answers, recorded by the head
// parser during the single pass it already makes over every field. They are
// the raw tokens found, not a decision: `close` beating `keep_alive` and the
// per-version default stay in `should_close` below. Recording them at parse
// time is what keeps `should_close`/`want_100_continue` field reads instead of
// a fresh walk of the whole header list each.
//
// Only the request head's own fields contribute. A chunked body's trailer
// block never does: trailers carry no connection or expectation semantics
// (RFC 9110 §6.5.1).
pub opaque type HeadFlags {
	conn_close Bool
	conn_keep_alive Bool
	expect_100_continue Bool
}

// The outcome of parsing one request head out of `buf` starting at an offset.
// `Done` threads the consumed-offset so the connection loop can resume parsing
// the next pipelined request from exactly where this one ended.
pub type Parsed {
	Done(
		method Binary
		target Binary
		version Version
		headers Headers
		flags HeadFlags
		consumed Int
	)
	NeedMore
	Bad(status Int)
}

// How a message body is framed, after applying RFC 7230 section 3.3.3
// precedence and the smuggling rejects. Content-Length and chunked
// transfer-encoding are both supported; any other Transfer-Encoding is
// rejected (501) rather than silently ignored (ignoring a transfer coding is
// a classic request-smuggling vector).
pub type Framing {
	NoBody
	Length(n Int)
	Chunked
	Invalid(status Int)
}

// The outcome of decoding a chunked transfer-encoded body out of `buf`
// starting at an offset. Mirrors `Parsed`'s incremental contract:
// `ChunkedNeedMore` means the buffer does not yet hold the whole body — read
// more bytes and call again with the same offset. `ChunkedDone` carries the
// decoded body, any trailer fields, and the offset just past the final CRLF —
// the start of the next pipelined request.
pub type ChunkBody {
	ChunkedDone(body Binary, trailers Headers, consumed Int)
	ChunkedNeedMore
	ChunkedBad(status Int)
}

// Parse a request head out of `buf` starting at `off`. Incomplete input yields
// NeedMore (read more and call again with the same offset); a malformed head
// yields Bad(status). A leading empty line before the request line is skipped
// (RFC 7230 section 3.5).
//
// One native pass over the bytes; the returned method/target/header
// names/values are zero-copy views into `buf`. Obs-fold and whitespace before
// the field colon are rejected (400) — both are request-smuggling vectors —
// a version other than HTTP/1.0 or HTTP/1.1 is rejected (505), and a head
// larger than 64 KiB is rejected (431, or 414 when the request line itself
// never ends) rather than buffered forever.
@vm(http__parse_head)
pub fn parse_request(buf Binary, off Int) Parsed

// Determine body framing from the request headers, enforcing RFC 7230 section
// 3.3.3 precedence and the MUST-reject smuggling conflicts. `Transfer-Encoding:
// chunked` (the only/final coding, case-insensitive) frames the body as
// Chunked. Transfer-Encoding alongside Content-Length is a conflict (400);
// any other transfer coding is unimplemented (501) — never silently ignored.
// Multiple Content-Length fields, or a value that is not a strict run of
// digits / overflows Int / carries a redundant leading zero ("007"), are all
// rejected (400).
@vm(http__framing)
pub fn framing(hs Headers) Framing

// Decode a chunked transfer-encoded body from `buf` starting at `off`,
// refusing to decode more than `max` body bytes (413). Each chunk is
// "<hex-size>[;extensions]CRLF <data> CRLF"; extensions are ignored. A
// zero-size chunk terminates the body, optionally followed by a trailer
// header block (same grammar and smuggling rejects as the request head,
// capped like the head -> 431) and the final blank line. A malformed or
// over-long size line, or chunk data not followed by CRLF, is rejected (400)
// — the decoder never resynchronizes after a framing lie.
//
// One native pass over the buffered bytes per call. The decoded body comes
// back as one owned Binary: the keep-alive-safe bounded-buffer model — the
// caller never hands a half-consumed socket to a handler.
@vm(http__chunk_decode)
pub fn chunk_decode(buf Binary, off Int, max Int) ChunkBody

// Serialize a status line + header block as one contiguous binary:
// "HTTP/1.1 <status> <reason>\r\n", the rendered headers, then the blank line
// that terminates the head. Native single-buffer assembly so the head can be
// the first part of one vectored write alongside the body.
@vm(http__serialize_head)
fn serialize_head_with(code Int, reason Binary, hs Headers) Binary

pub fn serialize_head(code Int, hs Headers) Binary {
	serialize_head_with(code, status.reason_phrase(code), hs)
}

// Whether the connection must close after this message. The `close` option
// wins whenever present, regardless of version (RFC 9112 §9.3). Otherwise
// HTTP/1.0 defaults to close unless `Connection: keep-alive`; HTTP/1.1
// defaults to keep-alive. `Connection` is a token list (RFC 9110 §7.6.1), so
// `keep-alive, Upgrade` still keeps an HTTP/1.0 connection alive.
// The tokens themselves were found by the head parser (`HeadFlags`), so this
// is the decision only — no second walk of the header list.
pub fn should_close(version Version, f HeadFlags) Bool {
	if f.conn_close {
		True
	} else {
		match version {
			Http11 -> False
			Http10 -> !f.conn_keep_alive
		}
	}
}

// Whether the client asked the server to send an interim 100 response before
// it streams the request body (`Expect: 100-continue`).
pub fn want_100_continue(f HeadFlags) Bool {
	f.expect_100_continue
}
