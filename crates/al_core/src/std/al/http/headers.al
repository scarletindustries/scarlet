import al/binary

// An HTTP header field. Names compare case-insensitively (RFC 9110 section 5.1)
// but the original casing is preserved on the wire.
pub type Header {
	name Binary
	value Binary
}

// An order-preserving association list. Order and duplicates are significant:
// multiple `Set-Cookie` fields must survive intact and stay in their original
// sequence, so this is a list, not a map.
pub type Headers = Array(Header)

// Hoisted so the separator/terminator binaries are built once, not per render.
const colon_space = <<': '>>
const crlf = <<'\r\n'>>
const comma = <<','>>
// ASCII SP / HTAB — the only OWS bytes RFC 9110 defines.
const sp = 32
const htab = 9

// The value of the first field whose name matches `name` (case-insensitive).
// Native walk: one op per lookup instead of one call frame per field.
@vm(http__header_get)
pub fn get(h Headers, name Binary) Option(Binary)

// Whether any field matches `name` (case-insensitive).
@vm(http__header_has)
pub fn has(h Headers, name Binary) Bool

// Replace the value of the first field matching `name` (keeping its position
// and original casing), or append a new field if none matches.
pub fn set(h Headers, name Binary, value Binary) Headers {
	match h {
		[] -> [Header(name: name, value: value)]
		[field, ..rest] -> if binary.eq_ignore_ascii_case(field.name, name) {
			[Header(name: field.name, value: value), ..rest]
		} else {
			[field, ..set(rest, name, value)]
		}
	}
}

// Append a field to the end without touching existing fields — the way to add
// a second `Set-Cookie` rather than overwrite the first.
pub fn append(h Headers, name Binary, value Binary) Headers {
	[..h, Header(name: name, value: value)]
}

// Whether any field named `name` (case-insensitive) carries `token` as one of
// its comma-separated elements. RFC 9110 defines `Connection`, `Expect`, `TE`,
// `Vary` and friends as token lists — a whole-value equality on such a field
// mishandles both a list value (`Connection: keep-alive, Upgrade`) and a
// repeated field. Every field with the name is scanned; each element is OWS-
// trimmed before the case-insensitive compare.
pub fn contains_token(h Headers, name Binary, token Binary) Bool {
	match h {
		[] -> False
		[field, ..rest] -> {
			hit = binary.eq_ignore_ascii_case(field.name, name) && has_token(field.value, token)
			if hit { True } else { contains_token(rest, name, token) }
		}
	}
}

fn has_token(v Binary, token Binary) Bool {
	value_has_token(v, 0, binary.byte_size(v), token)
}

fn value_has_token(v Binary, from Int, len Int, token Binary) Bool {
	to = match binary.index_of(v, comma, from) {
		Some(i) -> i
		None -> len
	}
	lo = skip_ows(v, from, to)
	hi = trim_ows(v, lo, to)
	if binary.eq_ignore_ascii_case(binary.slice_bytes(v, lo, hi - lo), token) {
		True
	} else if to < len {
		value_has_token(v, to + 1, len, token)
	} else {
		False
	}
}

fn skip_ows(v Binary, i Int, hi Int) Int {
	if i < hi && is_ows(binary.byte_at(v, i)) { skip_ows(v, i + 1, hi) } else { i }
}

fn trim_ows(v Binary, lo Int, i Int) Int {
	if i > lo && is_ows(binary.byte_at(v, i - 1)) { trim_ows(v, lo, i - 1) } else { i }
}

fn is_ows(b Option(Int)) Bool {
	b == Some(sp) || b == Some(htab)
}

// The wire bytes of the header block as a parts array — `name`, `": "`,
// `value`, CRLF for each field — ready for socket.write_parts. The pieces are
// never concatenated; the blank line that ends the block is the caller's job.
// (Response heads use h1.serialize_head, which renders natively; this stays
// for trailer blocks and any caller that wants the parts themselves.)
pub fn render(h Headers) Array(Binary) {
	match h {
		[] -> []
		[field, ..rest] -> [field.name, colon_space, field.value, crlf, ..render(rest)]
	}
}
