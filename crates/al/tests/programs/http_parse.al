// The al/http sans-IO contract: request-head parsing, body framing, header
// lookup/mutation/validation, status reason phrases, response-head
// serialization, and the pull-based body reader — including every smuggling
// reject. Pure functions over binaries and thunks, no sockets, so the whole
// protocol surface is golden-testable. The native (VM-builtin) scanners behind
// h1.parse_request / h1.framing / h1.chunk_decode / h1.serialize_head /
// headers.get / headers.has / headers.valid must keep producing exactly these
// results.
//
// al/http/body's socket-bound half (content_length, drain*, DrainError) is the
// one part of the module this file cannot reach: it needs a live Socket. That
// half is covered by examples/http_client.al, whose second server hand-rolls
// the connection driver against a loopback listener.

import al/binary
import al/string
import al/array
import al/http/h1.{
	Done,
	NeedMore,
	Bad,
	NoBody,
	Length,
	Chunked,
	Invalid,
	ChunkedDone,
	ChunkedNeedMore,
	ChunkedBad,
}
// Deliberately NOT importing `Done` from al/http/body — it would clash with
// h1's Parsed.Done. Body's terminator is matched qualified, as `body.Done`.
import al/http/body.{Body, Chunk, Empty, Whole, Streaming}
import al/http/headers.{Header, BadName, BadValue}
import al/http/status
import al/net/error.{UnexpectedEof}

fn str(b Binary) String {
	binary.to_string(b) or '<not utf8>'
}

// Render a binary with its control bytes escaped, so a CRLF-bearing wire blob —
// or a header value carrying the very bytes that must never reach the wire —
// stays on ONE golden line. `string.inspect` of a String does not escape, so a
// raw CR/LF/NUL would otherwise split the line and hide the vector it is testing.
fn vis(b Binary) String {
	vis_from(b, 0, binary.byte_size(b), '')
}

fn vis_from(b Binary, i Int, len Int, acc String) String {
	if i >= len {
		acc
	} else {
		piece = match binary.byte_at(b, i) {
			13 -> '\\r'
			10 -> '\\n'
			9 -> '\\t'
			0 -> '\\0'
			else -> binary.to_string(binary.slice_bytes(b, i, 1)) or '?'
		}
		vis_from(b, i + 1, len, '${acc}${piece}')
	}
}

// Quoted form, so an empty binary is visibly empty rather than nothing at all.
fn q(b Binary) String {
	'"${vis(b)}"'
}

// Join for display. `string.inspect` of a long array wraps across lines; a
// vector per golden LINE is what makes a diff readable, so join by hand.
fn list(parts Array(String)) String {
	match parts {
		[] -> '[]'
		[head, ..rest] -> '[${array.fold(rest, head, fn(acc, p) '${acc}, ${p}')}]'
	}
}

// Doubling grower: there is no binary.repeat, and the head-size caps need a
// buffer bigger than the parser's 64 KiB window.
fn grow(b Binary, n Int) Binary {
	if binary.byte_size(b) >= n {
		b
	} else {
		grow(binary.append(b, b), n)
	}
}

fn show_parse(label String, input Binary) Nil {
	out = match h1.parse_request(input, 0) {
		Done(method, target, version, hdrs, _, consumed) ->
			'Done method=${str(
				method,
			)} target=${str(
				target,
			)} version=${version} headers=${array.length(hdrs)} consumed=${consumed}'
		NeedMore -> 'NeedMore'
		Bad(status_code) -> 'Bad ${status_code}'
	}
	println('${label}: ${out}')
}

fn show_framing(label String, input Binary) Nil {
	out = match h1.parse_request(input, 0) {
		Done(_, _, _, hdrs, _, _) -> match h1.framing(hdrs) {
			NoBody -> 'NoBody'
			Length(n) -> 'Length ${n}'
			Chunked -> 'Chunked'
			Invalid(s) -> 'Invalid ${s}'
		}
		NeedMore -> 'parse: NeedMore'
		Bad(s) -> 'parse: Bad ${s}'
	}
	println('${label}: ${out}')
}

fn show_chunked(label String, input Binary, max Int) Nil {
	out = match h1.chunk_decode(input, 0, max) {
		ChunkedDone(decoded, trailers, consumed) ->
			'Done body=${q(decoded)} trailers=${array.length(trailers)} consumed=${consumed}'
		ChunkedNeedMore -> 'NeedMore'
		ChunkedBad(status_code) -> 'Bad ${status_code}'
	}
	println('${label}: ${out}')
}

// A header list as `['Name=value', ...]` — one golden line per list.
fn show_headers(label String, hs Array(Header)) Nil {
	parts = array.map(hs, fn(f) '${str(f.name)}=${str(f.value)}')
	println('${label}: ${list(parts)}')
}

fn show_collect(label String, b Body, max Int) Nil {
	out = match body.collect(b, max) {
		Ok(data) -> 'Ok ${q(data)}'
		Err(e) -> 'Err ${string.inspect(e)}'
	}
	println('${label}: ${out}')
}

println('== request line ==')
show_parse('simple get', <<'GET / HTTP/1.1\r\n\r\n'>>)
show_parse('post with path', <<'POST /api/users?id=1 HTTP/1.1\r\nHost: x\r\n\r\n'>>)
show_parse('http10', <<'GET / HTTP/1.0\r\n\r\n'>>)
show_parse('leading empty line', <<'\r\nGET / HTTP/1.1\r\n\r\n'>>)
show_parse('unknown version', <<'GET / HTTP/2.0\r\n\r\n'>>)
show_parse('garbage version', <<'GET / FOO\r\n\r\n'>>)
show_parse('no spaces', <<'GET\r\n\r\n'>>)
show_parse('one space', <<'GET /\r\n\r\n'>>)
show_parse('leading space', <<' GET / HTTP/1.1\r\n\r\n'>>)
show_parse('double space', <<'GET  HTTP/1.1\r\n\r\n'>>)
show_parse('empty target', <<'GET  / HTTP/1.1\r\n\r\n'>>)
show_parse('lowercase version', <<'GET / http/1.1\r\n\r\n'>>)
// A third space makes the version token `HTTP/1.1 extra`, so the reject lands on
// the version check (505) where RFC 9112 section 3 would say 400. It still
// rejects — never a parse — so the exact code is pinned here as-is, not fixed.
show_parse('trailing junk on request line', <<'GET / HTTP/1.1 extra\r\n\r\n'>>)
// The line terminator is strictly CRLF. RFC 9112 section 2.2 permits accepting a
// bare LF, but a parser that does disagrees with every proxy that does not —
// that disagreement IS request smuggling. So a bare LF/CR never ends a line: the
// head simply never completes and the request starves out on the read deadline.
show_parse('bare lf line ending', <<'GET / HTTP/1.1\nHost: x\n\n'>>)
show_parse('bare cr line ending', <<'GET / HTTP/1.1\rHost: x\r\r'>>)
// Leading empty lines before the request line are tolerated (for clients that
// trail a stray CRLF after a previous body) but capped at four.
show_parse('four leading crlfs', <<'\r\n\r\n\r\n\r\nGET / HTTP/1.1\r\n\r\n'>>)
show_parse('five leading crlfs', <<'\r\n\r\n\r\n\r\n\r\nGET / HTTP/1.1\r\n\r\n'>>)

println('')
println('== incomplete heads ==')
show_parse('empty', <<''>>)
show_parse('partial request line', <<'GET / HTT'>>)
show_parse('no blank line yet', <<'GET / HTTP/1.1\r\nHost: x\r\n'>>)

println('')
println('== head size caps ==')
// No CRLF anywhere in more than 64 KiB: the request line can never end, so the
// scanner gives up (414) instead of buffering the peer's stream forever.
show_parse('no crlf past 64k', grow(<<'x'>>, 70000))
// A fine request line followed by more than 64 KiB of header block: the request
// line was not the problem, so this is 431, not 414.
show_parse(
	'header block past 64k',
	binary.append(
		binary.append(<<'GET / HTTP/1.1\r\n'>>, grow(<<'X-Pad: pad\r\n'>>, 70000)),
		<<'\r\n'>>,
	),
)

println('')
println('== headers ==')
show_parse('two headers', <<'GET / HTTP/1.1\r\nHost: example.com\r\nAccept: */*\r\n\r\n'>>)
show_parse('ows trimmed', <<'GET / HTTP/1.1\r\nHost:   spaced   \r\n\r\n'>>)
show_parse('tab ows trimmed', <<'GET / HTTP/1.1\r\nHost:\t\ttabbed\t\r\n\r\n'>>)
show_parse('empty value', <<'GET / HTTP/1.1\r\nX-E:\r\n\r\n'>>)
show_parse('obs-fold rejected', <<'GET / HTTP/1.1\r\nHost: a\r\n folded\r\n\r\n'>>)
show_parse('ws before colon rejected', <<'GET / HTTP/1.1\r\nHost : a\r\n\r\n'>>)
show_parse('no colon rejected', <<'GET / HTTP/1.1\r\nHost\r\n\r\n'>>)
show_parse('empty name rejected', <<'GET / HTTP/1.1\r\n: value\r\n\r\n'>>)

// A duplicate field is NOT a parse error — only the singleton framing fields are
// (see `framing`). Both survive in order, and `get` answers with the first.
dup = <<'GET / HTTP/1.1\r\nHost: a\r\nHost: b\r\n\r\n'>>
show_parse('duplicate host', dup)
match h1.parse_request(dup, 0) {
	Done(_, _, _, hdrs, _, _) -> {
		first = match headers.get(hdrs, <<'host'>>) {
			Some(v) -> str(v)
			None -> 'missing'
		}
		println('duplicate host: get returns first = ${first}')
	}
	else -> println('duplicate host: parse failed')
}

// Ingress accepts a NUL inside a field value — the head scanner does not
// validate value bytes — but egress refuses it: `headers.valid` is what the
// connection driver checks before a header block reaches the wire. Validation
// happens on serialize, not on parse; both halves of that contract are pinned
// together here.
nul_req = <<'GET / HTTP/1.1\r\nX-N: a\0b\r\n\r\n'>>
show_parse('nul in value parses', nul_req)
match h1.parse_request(nul_req, 0) {
	Done(_, _, _, hdrs, _, _) -> println('nul in value: headers.valid = ${headers.valid(hdrs)}')
	else -> println('nul in value: parse failed')
}

println('')
println('== header values survive zero-copy ==')
req = <<
	'GET /path HTTP/1.1\r\nHost: example.com\r\nX-Custom: hello world\r\nCookie: a=1; b=2\r\n\r\n',
>>
match h1.parse_request(req, 0) {
	Done(_, _, _, hdrs, _, _) -> {
		host = match headers.get(hdrs, <<'host'>>) {
			Some(v) -> str(v)
			None -> 'missing'
		}
		cookie = match headers.get(hdrs, <<'COOKIE'>>) {
			Some(v) -> str(v)
			None -> 'missing'
		}
		missing = match headers.get(hdrs, <<'x-missing'>>) {
			Some(_) -> 'found?!'
			None -> 'missing'
		}
		println('host (lower lookup): ${host}')
		println('cookie (upper lookup): ${cookie}')
		println('x-missing: ${missing}')
		println('has x-custom: ${headers.has(hdrs, <<'X-CUSTOM'>>)}')
	}
	else -> println('parse failed')
}

println('')
println('== header mutation ==')
// `set` must leave exactly ONE field with the name: the first match is replaced
// in place (keeping its position and its ORIGINAL casing) and every later
// same-named field is deleted. Two disagreeing Content-Lengths is a smuggling
// vector, so "replace the first, leave the rest" would be a bug, not a nicety.
mut_hdrs = [
	Header(name: <<'Host'>>, value: <<'a'>>),
	Header(name: <<'Set-Cookie'>>, value: <<'x=1'>>),
	Header(name: <<'host'>>, value: <<'b'>>),
]
show_headers('before', mut_hdrs)
show_headers('set HOST=z (dedups, keeps casing)', headers.set(mut_hdrs, <<'HOST'>>, <<'z'>>))
show_headers('set absent appends', headers.set(mut_hdrs, <<'X-New'>>, <<'1'>>))
show_headers('delete host (case-insensitive, all)', headers.delete(mut_hdrs, <<'HoSt'>>))
show_headers('delete absent is identity', headers.delete(mut_hdrs, <<'X-Nope'>>))
show_headers(
	'append keeps duplicates in order',
	headers.append(mut_hdrs, <<'Set-Cookie'>>, <<'y=2'>>),
)
println('render parts: ${list(array.map(headers.render(mut_hdrs), fn(p) q(p)))}')
println('render of empty: ${list(array.map(headers.render([]), fn(p) q(p)))}')

println('')
println('== header validation ==')
show_field = fn(label String, name String, value String) {
	out = match headers.field(binary.from_string(name), binary.from_string(value)) {
		Ok(f) -> 'Ok ${q(f.name)}=${q(f.value)}'
		Err(BadName(n)) -> 'Err BadName ${q(n)}'
		Err(BadValue(v)) -> 'Err BadValue ${q(v)}'
	}
	println('${label}: ${out}')
}
show_field('plain field', 'X-Ok', 'value')
show_field('full token charset name', 'X-Ok!#\$%*+-.^_|~', 'v')
show_field('empty value is fine', 'X-Ok', '')
show_field('space in name', 'X Bad', 'v')
show_field('colon in name', 'X:Bad', 'v')
show_field('empty name', '', 'v')
show_field('cr in value', 'X-Ok', 'a\rb')
show_field('lf in value', 'X-Ok', 'a\nb')
show_field('nul in value', 'X-Ok', 'a\0b')

show_valid = fn(label String, hs Array(Header)) println('${label}: ${headers.valid(hs)}')
show_valid('valid: empty list', [])
show_valid(
	'valid: token name + clean value',
	[Header(name: <<'X-Ok!#\$%*+-.^_|~'>>, value: <<'fine'>>)],
)
show_valid('invalid: colon in name', [Header(name: <<'X:Bad'>>, value: <<'v'>>)])
show_valid('invalid: lf in value', [Header(name: <<'X-Ok'>>, value: <<'a\nb'>>)])
show_valid(
	'invalid: one bad field among good',
	[Header(name: <<'Good'>>, value: <<'1'>>), Header(name: <<'Bad Name'>>, value: <<'2'>>)],
)

println('')
println('== pipelined requests ==')
pipelined = <<'GET /first HTTP/1.1\r\nHost: a\r\n\r\nGET /second HTTP/1.1\r\nHost: b\r\n\r\n'>>
match h1.parse_request(pipelined, 0) {
	Done(_, target1, _, _, _, consumed1) -> {
		println('first: ${str(target1)} (consumed ${consumed1})')
		match h1.parse_request(pipelined, consumed1) {
			Done(_, target2, _, _, _, consumed2) ->
				println('second: ${str(target2)} (consumed ${consumed2})')
			else -> println('second parse failed')
		}
	}
	else -> println('first parse failed')
}

println('')
println('== body framing ==')
show_framing('no body headers', <<'GET / HTTP/1.1\r\nHost: x\r\n\r\n'>>)
show_framing('content-length 42', <<'POST / HTTP/1.1\r\nContent-Length: 42\r\n\r\n'>>)
show_framing('content-length 0', <<'POST / HTTP/1.1\r\nContent-Length: 0\r\n\r\n'>>)
show_framing('case-insensitive name', <<'POST / HTTP/1.1\r\ncontent-LENGTH: 7\r\n\r\n'>>)
show_framing('leading zero rejected', <<'POST / HTTP/1.1\r\nContent-Length: 007\r\n\r\n'>>)
show_framing('non-numeric rejected', <<'POST / HTTP/1.1\r\nContent-Length: abc\r\n\r\n'>>)
show_framing('negative rejected', <<'POST / HTTP/1.1\r\nContent-Length: -1\r\n\r\n'>>)
show_framing('empty cl rejected', <<'POST / HTTP/1.1\r\nContent-Length:\r\n\r\n'>>)
// `5 5` is the internal-space trick: a value a lenient parser reads as 5 and a
// stricter one as 55 — a length disagreement between two hops is a smuggle.
show_framing('spaced cl rejected', <<'POST / HTTP/1.1\r\nContent-Length: 5 5\r\n\r\n'>>)
show_framing(
	'overflow rejected',
	<<'POST / HTTP/1.1\r\nContent-Length: 99999999999999999999\r\n\r\n'>>,
)
show_framing(
	'duplicate cl rejected',
	<<'POST / HTTP/1.1\r\nContent-Length: 5\r\nContent-Length: 5\r\n\r\n'>>,
)
show_framing('te chunked', <<'POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n'>>)
show_framing(
	'te chunked case-insensitive',
	<<'POST / HTTP/1.1\r\ntransfer-encoding: CHUNKED\r\n\r\n'>>,
)
// The head parser trims OWS around a value, so a padded `chunked` still frames.
show_framing('te chunked with ows', <<'POST / HTTP/1.1\r\nTransfer-Encoding:  chunked \r\n\r\n'>>)
show_framing('te gzip unimplemented', <<'POST / HTTP/1.1\r\nTransfer-Encoding: gzip\r\n\r\n'>>)
show_framing(
	'te coding list unimplemented',
	<<'POST / HTTP/1.1\r\nTransfer-Encoding: gzip, chunked\r\n\r\n'>>,
)
show_framing(
	'te identity unimplemented',
	<<'POST / HTTP/1.1\r\nTransfer-Encoding: identity\r\n\r\n'>>,
)
show_framing('empty te unimplemented', <<'POST / HTTP/1.1\r\nTransfer-Encoding:\r\n\r\n'>>)
// Even two IDENTICAL `chunked` fields are refused: a repeated Transfer-Encoding
// is exactly the shape a front-end/back-end framing disagreement is built from.
show_framing(
	'repeated te chunked unimplemented',
	<<'POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\nTransfer-Encoding: chunked\r\n\r\n'>>,
)
show_framing(
	'te + cl conflict',
	<<'POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\nContent-Length: 5\r\n\r\n'>>,
)

println('')
println('== chunked body decode ==')
show_chunked('single chunk', <<'5\r\nhello\r\n0\r\n\r\n'>>, 1048576)
show_chunked('multiple chunks', <<'5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n'>>, 1048576)
show_chunked('empty body', <<'0\r\n\r\n'>>, 1048576)
show_chunked('uppercase hex size', <<'A\r\n0123456789\r\n0\r\n\r\n'>>, 1048576)
show_chunked('extensions ignored', <<'5;foo=bar\r\nhello\r\n0;last\r\n\r\n'>>, 1048576)
show_chunked('trailers', <<'5\r\nhello\r\n0\r\nX-Checksum: abc\r\n\r\n'>>, 1048576)
show_chunked('two trailers', <<'5\r\nhello\r\n0\r\nX-A: 1\r\nX-B: 2\r\n\r\n'>>, 1048576)
show_chunked('partial needs more', <<'5\r\nhel'>>, 1048576)
show_chunked('terminator missing', <<'5\r\nhello\r\n'>>, 1048576)
show_chunked('last chunk without final crlf', <<'0\r\n'>>, 1048576)
show_chunked('bad hex rejected', <<'zz\r\nhello\r\n0\r\n\r\n'>>, 1048576)
show_chunked('empty size line rejected', <<'\r\nhello\r\n0\r\n\r\n'>>, 1048576)
show_chunked('signed size rejected', <<'+5\r\nhello\r\n0\r\n\r\n'>>, 1048576)
show_chunked('size lie rejected', <<'3\r\nhello\r\n0\r\n\r\n'>>, 1048576)
// The cap is `total + size > max`, checked from the size line BEFORE any data is
// copied: an over-long body never materializes, not even transiently.
show_chunked('exactly at max', <<'5\r\nhello\r\n0\r\n\r\n'>>, 5)
show_chunked('one byte over max', <<'5\r\nhello\r\n0\r\n\r\n'>>, 4)
show_chunked('zero max, empty body', <<'0\r\n\r\n'>>, 0)
show_chunked('oversize rejected', <<'FFFFFFFF\r\n'>>, 1024)
// The size line has its own 4 KiB cap, so a peer cannot make the decoder buffer
// an unbounded "size" it has no data to back.
show_chunked(
	'size line past 4k rejected',
	binary.append(grow(<<'F'>>, 5000), <<'\r\nx\r\n0\r\n\r\n'>>),
	1048576,
)
show_chunked(
	'trailer obs-fold rejected',
	<<'5\r\nhello\r\n0\r\nX-A: 1\r\n folded\r\n\r\n'>>,
	1048576,
)

// Decoding resumes mid-buffer: parse the head first, then decode the body
// from the head's consumed offset, and find the next pipelined request after
// the body's consumed offset.
println('')
println('== chunked pipelining ==')
chunked_req = <<
	'POST /upload HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\nGET /after HTTP/1.1\r\n\r\n',
>>
match h1.parse_request(chunked_req, 0) {
	Done(_, target1, _, hdrs1, _, head_end) -> {
		f = match h1.framing(hdrs1) {
			Chunked -> 'chunked'
			else -> 'other'
		}
		println('head: target=${str(target1)} framing=${f} consumed=${head_end}')
		match h1.chunk_decode(chunked_req, head_end, 1048576) {
			ChunkedDone(decoded, _, body_end) -> {
				println('body: ${str(decoded)} (ends at ${body_end})')
				match h1.parse_request(chunked_req, body_end) {
					Done(_, target2, _, _, _, _) ->
						println('next pipelined request: ${str(target2)}')
					else -> println('next request parse failed')
				}
			}
			else -> println('body decode failed')
		}
	}
	else -> println('head parse failed')
}

println('')
println('== connection semantics ==')
keep = <<'GET / HTTP/1.1\r\nHost: x\r\n\r\n'>>
close = <<'GET / HTTP/1.1\r\nConnection: close\r\n\r\n'>>
old_keep = <<'GET / HTTP/1.0\r\nConnection: keep-alive\r\n\r\n'>>
old_plain = <<'GET / HTTP/1.0\r\n\r\n'>>
old_both = <<'GET / HTTP/1.0\r\nConnection: close, keep-alive\r\n\r\n'>>
expect = <<'POST / HTTP/1.1\r\nExpect: 100-continue\r\nContent-Length: 5\r\n\r\n'>>
show_close = fn(label String, req2 Binary) match h1.parse_request(req2, 0) {
	Done(_, _, version, _, flags, _) ->
		println(
			'${label}: close=${h1.should_close(
				version,
				flags,
			)} want100=${h1.want_100_continue(flags)}',
		)
	else -> println('${label}: parse failed')
}
show_close('http11 default', keep)
show_close('http11 close', close)
show_close('http10 keep-alive', old_keep)
show_close('http10 default', old_plain)
show_close('http10 close beats keep-alive', old_both)
show_close('expect 100', expect)

println('')
println('== token list lookup ==')
list_hdrs = [
	Header(name: <<'Connection'>>, value: <<'a,,b'>>),
	Header(name: <<'X-Empty'>>, value: <<''>>),
	Header(name: <<'X-Trailing'>>, value: <<'gzip,'>>),
]
show_token = fn(label String, name String, token String) {
	got = headers.contains_token(list_hdrs, binary.from_string(name), binary.from_string(token))
	println('${label}: ${got}')
}
show_token('a in a,,b', 'Connection', 'a')
show_token('b in a,,b', 'Connection', 'b')
show_token('empty token in a,,b', 'Connection', '')
show_token('empty token in empty value', 'X-Empty', '')
show_token('empty token after trailing comma', 'X-Trailing', '')
show_token('gzip with trailing comma', 'X-Trailing', 'gzip')

println('')
println('== status reason phrases ==')
// One code per class, plus the unregistered cases. 418 is deliberately NOT in
// the registry (IANA lists it as Unused), so it gets the same empty phrase as
// any unassigned code — an empty reason-phrase is valid per RFC 9110 section 15.
codes = [100, 200, 204, 301, 400, 404, 418, 431, 500, 503, 599]
array.each(codes, fn(code) println('${code}: ${q(status.reason_phrase(code))}'))

println('')
println('== response head serialization ==')
resp_headers = [
	Header(name: <<'Content-Type'>>, value: <<'text/plain'>>),
	Header(name: <<'Content-Length'>>, value: <<'5'>>),
]
println('200 + two fields: ${vis(h1.serialize_head(200, resp_headers))}')
println('404 + no fields: ${vis(h1.serialize_head(404, []))}')
// An unregistered code still serializes: empty reason phrase, trailing SP kept.
println('599 unregistered: ${vis(h1.serialize_head(599, []))}')

println('')
println('== sans-IO bodies ==')
match body.pull(body.empty()) {
	Ok(body.Done(trailers)) -> println('empty body: Done trailers=${array.length(trailers)}')
	Ok(Chunk(_, _)) -> println('empty body: Chunk?!')
	Err(e) -> println('empty body: Err ${string.inspect(e)}')
}
show_collect('from_binary round-trip', body.from_binary(<<'hello'>>), 1024)
show_collect('from_binary empty', body.from_binary(<<>>), 1024)
// The cap is checked before each append, so an over-long body errs without ever
// materializing past the limit.
show_collect('collect past max', body.from_binary(<<'hello'>>), 3)

// A bare closure IS a Body — the alias is transparent, so no wrapper is needed.
two_chunks = fn() Ok(Chunk(<<'ab'>>, fn() Ok(Chunk(<<'cd'>>, body.empty()))))
show_collect('hand-written two-chunk body', two_chunks, 1024)
show_collect('failing body propagates', fn() Err(UnexpectedEof), 1024)

three_chunks = fn() Ok(
	Chunk(<<'ab'>>, fn() Ok(Chunk(<<'cd'>>, fn() Ok(Chunk(<<'ef'>>, body.empty()))))),
)
show_buffered = fn(label String, b Body) {
	out = match body.take_buffered(b) {
		Ok(Empty) -> 'Empty'
		Ok(Whole(data)) -> 'Whole ${q(data)}'
		Ok(Streaming(first, second, rest)) -> {
			tail = match body.collect(rest, 1024) {
				Ok(data) -> q(data)
				Err(e) -> 'Err ${string.inspect(e)}'
			}
			'Streaming first=${q(first)} second=${q(second)} rest=${tail}'
		}
		Err(e) -> 'Err ${string.inspect(e)}'
	}
	println('${label}: ${out}')
}
show_buffered('take_buffered empty', body.empty())
show_buffered('take_buffered whole', body.from_binary(<<'hello'>>))
show_buffered('take_buffered streaming', three_chunks)
show_buffered('take_buffered of failing body', fn() Err(UnexpectedEof))

// `probe` pulls the first step without consuming it: the body it hands back
// replays that step, so a caller can learn the source produces BEFORE committing
// a response head to the wire.
match body.probe(body.from_binary(<<'hello'>>)) {
	Ok(replayed) -> show_collect('collect after probe', replayed, 1024)
	Err(e) -> println('collect after probe: Err ${string.inspect(e)}')
}
match body.probe(fn() Err(UnexpectedEof)) {
	Ok(_) -> println('probe of failing body: Ok?!')
	Err(e) -> println('probe of failing body: Err ${string.inspect(e)}')
}

println('')
println('== method dispatch via binary patterns ==')
methods = ['GET', 'POST', 'PUT', 'DELETE', 'PATCH', 'HEAD', 'OPTIONS', 'BREW']
shown = array.map(methods, fn(name) {
	req3 = <<'${name} / HTTP/1.1\r\n\r\n'>>
	match h1.parse_request(req3, 0) {
		Done(m, _, _, _, _, _) -> match m {
			<<'GET'>> -> 'Get'
			<<'POST'>> -> 'Post'
			<<'PUT'>> -> 'Put'
			<<'DELETE'>> -> 'Delete'
			<<'PATCH'>> -> 'Patch'
			<<'HEAD'>> -> 'Head'
			<<'OPTIONS'>> -> 'Options'
			else -> 'Other(${str(m)})'
		}
		else -> 'parse failed'
	}
})
println(list(shown))
