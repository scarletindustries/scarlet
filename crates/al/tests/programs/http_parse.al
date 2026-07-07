// The al/http/h1 sans-IO contract: request-head parsing, body framing, header
// lookup, and response-head serialization — including every smuggling reject.
// Pure functions over binaries, no sockets, so the whole protocol surface is
// golden-testable. The native (VM-builtin) scanners behind h1.parse_request /
// h1.framing / h1.serialize_head must keep producing exactly these results.

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
import al/http/headers.{Header}

fn show_parse(label String, input Binary) Nil {
	out = match h1.parse_request(input, 0) {
		Done(method, target, version, hdrs, consumed) -> {
			m = binary.to_string(method) or '?'
			t = binary.to_string(target) or '?'
			'Done method=${m} target=${t} version=${version} headers=${array.length(hdrs)} consumed=${consumed}'
		}
		NeedMore -> 'NeedMore'
		Bad(status) -> 'Bad ${status}'
	}
	println('${label}: ${out}')
}

fn show_framing(label String, input Binary) Nil {
	out = match h1.parse_request(input, 0) {
		Done(_, _, _, hdrs, _) -> match h1.framing(hdrs) {
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
		ChunkedDone(body, trailers, consumed) -> {
			s = binary.to_string(body) or '<not utf8>'
			'Done body=${string.inspect(s)} trailers=${array.length(trailers)} consumed=${consumed}'
		}
		ChunkedNeedMore -> 'NeedMore'
		ChunkedBad(status) -> 'Bad ${status}'
	}
	println('${label}: ${out}')
}

println('== request line ==')
show_parse('simple get', binary.from_string('GET / HTTP/1.1\r\n\r\n'))
show_parse('post with path', binary.from_string('POST /api/users?id=1 HTTP/1.1\r\nHost: x\r\n\r\n'))
show_parse('http10', binary.from_string('GET / HTTP/1.0\r\n\r\n'))
show_parse('leading empty line', binary.from_string('\r\nGET / HTTP/1.1\r\n\r\n'))
show_parse('unknown version', binary.from_string('GET / HTTP/2.0\r\n\r\n'))
show_parse('garbage version', binary.from_string('GET / FOO\r\n\r\n'))
show_parse('no spaces', binary.from_string('GET\r\n\r\n'))
show_parse('one space', binary.from_string('GET /\r\n\r\n'))
show_parse('leading space', binary.from_string(' GET / HTTP/1.1\r\n\r\n'))
show_parse('double space', binary.from_string('GET  HTTP/1.1\r\n\r\n'))

println('')
println('== incomplete heads ==')
show_parse('empty', binary.from_string(''))
show_parse('partial request line', binary.from_string('GET / HTT'))
show_parse('no blank line yet', binary.from_string('GET / HTTP/1.1\r\nHost: x\r\n'))

println('')
println('== headers ==')
show_parse(
	'two headers',
	binary.from_string('GET / HTTP/1.1\r\nHost: example.com\r\nAccept: */*\r\n\r\n'),
)
show_parse('ows trimmed', binary.from_string('GET / HTTP/1.1\r\nHost:   spaced   \r\n\r\n'))
show_parse('obs-fold rejected', binary.from_string('GET / HTTP/1.1\r\nHost: a\r\n folded\r\n\r\n'))
show_parse('ws before colon rejected', binary.from_string('GET / HTTP/1.1\r\nHost : a\r\n\r\n'))
show_parse('no colon rejected', binary.from_string('GET / HTTP/1.1\r\nHost\r\n\r\n'))
show_parse('empty name rejected', binary.from_string('GET / HTTP/1.1\r\n: value\r\n\r\n'))

println('')
println('== header values survive zero-copy ==')
req = binary.from_string(
	'GET /path HTTP/1.1\r\nHost: example.com\r\nX-Custom: hello world\r\nCookie: a=1; b=2\r\n\r\n',
)
match h1.parse_request(req, 0) {
	Done(_, _, _, hdrs, _) -> {
		host = match headers.get(hdrs, binary.from_string('host')) {
			Some(v) -> binary.to_string(v) or '?'
			None -> 'missing'
		}
		cookie = match headers.get(hdrs, binary.from_string('COOKIE')) {
			Some(v) -> binary.to_string(v) or '?'
			None -> 'missing'
		}
		missing = match headers.get(hdrs, binary.from_string('x-missing')) {
			Some(_) -> 'found?!'
			None -> 'missing'
		}
		println('host (lower lookup): ${host}')
		println('cookie (upper lookup): ${cookie}')
		println('x-missing: ${missing}')
		println('has x-custom: ${headers.has(hdrs, binary.from_string('X-CUSTOM'))}')
	}
	else -> println('parse failed')
}

println('')
println('== pipelined requests ==')
pipelined = binary.from_string(
	'GET /first HTTP/1.1\r\nHost: a\r\n\r\nGET /second HTTP/1.1\r\nHost: b\r\n\r\n',
)
match h1.parse_request(pipelined, 0) {
	Done(_, target1, _, _, consumed1) -> {
		t1 = binary.to_string(target1) or '?'
		println('first: ${t1} (consumed ${consumed1})')
		match h1.parse_request(pipelined, consumed1) {
			Done(_, target2, _, _, consumed2) -> {
				t2 = binary.to_string(target2) or '?'
				println('second: ${t2} (consumed ${consumed2})')
			}
			else -> println('second parse failed')
		}
	}
	else -> println('first parse failed')
}

println('')
println('== body framing ==')
show_framing('no body headers', binary.from_string('GET / HTTP/1.1\r\nHost: x\r\n\r\n'))
show_framing(
	'content-length 42',
	binary.from_string('POST / HTTP/1.1\r\nContent-Length: 42\r\n\r\n'),
)
show_framing('content-length 0', binary.from_string('POST / HTTP/1.1\r\nContent-Length: 0\r\n\r\n'))
show_framing(
	'case-insensitive name',
	binary.from_string('POST / HTTP/1.1\r\ncontent-LENGTH: 7\r\n\r\n'),
)
show_framing(
	'leading zero rejected',
	binary.from_string('POST / HTTP/1.1\r\nContent-Length: 007\r\n\r\n'),
)
show_framing(
	'non-numeric rejected',
	binary.from_string('POST / HTTP/1.1\r\nContent-Length: abc\r\n\r\n'),
)
show_framing(
	'negative rejected',
	binary.from_string('POST / HTTP/1.1\r\nContent-Length: -1\r\n\r\n'),
)
show_framing(
	'overflow rejected',
	binary.from_string('POST / HTTP/1.1\r\nContent-Length: 99999999999999999999\r\n\r\n'),
)
show_framing(
	'duplicate cl rejected',
	binary.from_string('POST / HTTP/1.1\r\nContent-Length: 5\r\nContent-Length: 5\r\n\r\n'),
)
show_framing(
	'te chunked',
	binary.from_string('POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n'),
)
show_framing(
	'te chunked case-insensitive',
	binary.from_string('POST / HTTP/1.1\r\ntransfer-encoding: CHUNKED\r\n\r\n'),
)
show_framing(
	'te gzip unimplemented',
	binary.from_string('POST / HTTP/1.1\r\nTransfer-Encoding: gzip\r\n\r\n'),
)
show_framing(
	'te coding list unimplemented',
	binary.from_string('POST / HTTP/1.1\r\nTransfer-Encoding: gzip, chunked\r\n\r\n'),
)
show_framing(
	'te + cl conflict',
	binary.from_string('POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\nContent-Length: 5\r\n\r\n'),
)

println('')
println('== chunked body decode ==')
show_chunked('single chunk', binary.from_string('5\r\nhello\r\n0\r\n\r\n'), 1048576)
show_chunked(
	'multiple chunks',
	binary.from_string('5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n'),
	1048576,
)
show_chunked('empty body', binary.from_string('0\r\n\r\n'), 1048576)
show_chunked(
	'extensions ignored',
	binary.from_string('5;foo=bar\r\nhello\r\n0;last\r\n\r\n'),
	1048576,
)
show_chunked('trailers', binary.from_string('5\r\nhello\r\n0\r\nX-Checksum: abc\r\n\r\n'), 1048576)
show_chunked('partial needs more', binary.from_string('5\r\nhel'), 1048576)
show_chunked('terminator missing', binary.from_string('5\r\nhello\r\n'), 1048576)
show_chunked('bad hex rejected', binary.from_string('zz\r\nhello\r\n0\r\n\r\n'), 1048576)
show_chunked('size lie rejected', binary.from_string('3\r\nhello\r\n0\r\n\r\n'), 1048576)
show_chunked('oversize rejected', binary.from_string('FFFFFFFF\r\n'), 1024)
show_chunked(
	'trailer obs-fold rejected',
	binary.from_string('5\r\nhello\r\n0\r\nX-A: 1\r\n folded\r\n\r\n'),
	1048576,
)

// Decoding resumes mid-buffer: parse the head first, then decode the body
// from the head's consumed offset, and find the next pipelined request after
// the body's consumed offset.
println('')
println('== chunked pipelining ==')
chunked_req = binary.from_string(
	'POST /upload HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\nGET /after HTTP/1.1\r\n\r\n',
)
match h1.parse_request(chunked_req, 0) {
	Done(_, target1, _, hdrs1, head_end) -> {
		t1 = binary.to_string(target1) or '?'
		f = match h1.framing(hdrs1) {
			Chunked -> 'chunked'
			else -> 'other'
		}
		println('head: target=${t1} framing=${f} consumed=${head_end}')
		match h1.chunk_decode(chunked_req, head_end, 1048576) {
			ChunkedDone(body1, _, body_end) -> {
				b = binary.to_string(body1) or '?'
				println('body: ${b} (ends at ${body_end})')
				match h1.parse_request(chunked_req, body_end) {
					Done(_, target2, _, _, _) -> {
						t2 = binary.to_string(target2) or '?'
						println('next pipelined request: ${t2}')
					}
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
keep = binary.from_string('GET / HTTP/1.1\r\nHost: x\r\n\r\n')
close = binary.from_string('GET / HTTP/1.1\r\nConnection: close\r\n\r\n')
old_keep = binary.from_string('GET / HTTP/1.0\r\nConnection: keep-alive\r\n\r\n')
old_plain = binary.from_string('GET / HTTP/1.0\r\n\r\n')
expect = binary.from_string('POST / HTTP/1.1\r\nExpect: 100-continue\r\nContent-Length: 5\r\n\r\n')
show_close = fn(label String, req2 Binary) match h1.parse_request(req2, 0) {
	Done(_, _, version, hdrs, _) ->
		println(
			'${label}: close=${h1.should_close(version, hdrs)} want100=${h1.want_100_continue(hdrs)}',
		)
	else -> println('${label}: parse failed')
}
show_close('http11 default', keep)
show_close('http11 close', close)
show_close('http10 keep-alive', old_keep)
show_close('http10 default', old_plain)
show_close('expect 100', expect)

println('')
println('== response head serialization ==')
resp_headers = [
	Header(name: binary.from_string('Content-Type'), value: binary.from_string('text/plain')),
	Header(name: binary.from_string('Content-Length'), value: binary.from_string('5')),
]
head = h1.serialize_head(200, resp_headers)
println(string.inspect(binary.to_string(head)))
not_found_head = h1.serialize_head(404, [])
println(string.inspect(binary.to_string(not_found_head)))
teapot_head = h1.serialize_head(418, [])
println(string.inspect(binary.to_string(teapot_head)))

println('')
println('== method dispatch via binary patterns ==')
methods = ['GET', 'POST', 'PUT', 'DELETE', 'PATCH', 'HEAD', 'OPTIONS', 'BREW']
shown = array.map(methods, fn(name) {
	req3 = binary.from_string('${name} / HTTP/1.1\r\n\r\n')
	match h1.parse_request(req3, 0) {
		Done(m, _, _, _, _) -> match m {
			<<'GET'>> -> 'Get'
			<<'POST'>> -> 'Post'
			<<'PUT'>> -> 'Put'
			<<'DELETE'>> -> 'Delete'
			<<'PATCH'>> -> 'Patch'
			<<'HEAD'>> -> 'Head'
			<<'OPTIONS'>> -> 'Options'
			else -> 'Other(${binary.to_string(m) or '?'})'
		}
		else -> 'parse failed'
	}
})
println(string.inspect(shown))
