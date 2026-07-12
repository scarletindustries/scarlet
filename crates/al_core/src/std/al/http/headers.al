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
// The three bytes a field value must never contain, as single-byte needles
// for the native index_of scans in valid_value.
const cr = <<'\r'>>
const lf = <<'\n'>>
const nul = <<'\0'>>
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

// Set `name` to `value` so that exactly ONE field carries the name afterwards:
// the first matching field (case-insensitive) is replaced in place, keeping
// its position and original casing, every later same-named field is deleted,
// and a new field is appended if none matched. Replacing only the first match
// would leave stale duplicates behind — fatal for singleton fields like
// Content-Length, where two disagreeing values are a smuggling vector.
pub fn set(h Headers, name Binary, value Binary) Headers {
	match h {
		[] -> [Header(name: name, value: value)]
		[field, ..rest] -> if binary.eq_ignore_ascii_case(field.name, name) {
			[Header(name: field.name, value: value), ..delete(rest, name)]
		} else {
			[field, ..set(rest, name, value)]
		}
	}
}

// Remove every field whose name matches `name` (case-insensitive).
//
// Identity when the name is absent: the walk-and-rebuild allocates a fresh
// cons per field, and the response hot path deletes framing headers that are
// almost never present — the native `has` probe makes that case free.
pub fn delete(h Headers, name Binary) Headers {
	if has(h, name) {
		delete_walk(h, name)
	} else {
		h
	}
}

fn delete_walk(h Headers, name Binary) Headers {
	match h {
		[] -> []
		[field, ..rest] -> if binary.eq_ignore_ascii_case(field.name, name) {
			delete_walk(rest, name)
		} else {
			[field, ..delete_walk(rest, name)]
		}
	}
}

// Append a field to the end without touching existing fields — the way to add
// a second `Set-Cookie` rather than overwrite the first.
pub fn append(h Headers, name Binary, value Binary) Headers {
	[..h, Header(name: name, value: value)]
}

// Why `field` refused to build a Header.
pub type InvalidHeader {
	BadName(name Binary)
	BadValue(value Binary)
}

// The validating constructor for handler code: a Header comes back only when
// `name` is an RFC 9110 token and `value` is free of CR/LF/NUL, so a field
// built from external input can never smuggle a line break onto the wire.
pub fn field(name Binary, value Binary) Result(Header, InvalidHeader) {
	if !valid_name(name) {
		Err(BadName(name))
	} else if !valid_value(value) {
		Err(BadValue(value))
	} else {
		Ok(Header(name: name, value: value))
	}
}

// Whether every field is safe to serialize: token name, CR/LF/NUL-free value.
// The connection driver checks this before a response's header block (or
// trailer list) reaches the wire, so a handler that built raw Header values
// from unvalidated input cannot split the response.
@vm(http__headers_valid)
pub fn valid(h Headers) Bool

// A field name must be a non-empty RFC 9110 token (§5.6.2).
fn valid_name(name Binary) Bool {
	binary.byte_size(name) > 0 && token_bytes_from(name, 0, binary.byte_size(name))
}

fn token_bytes_from(name Binary, i Int, len Int) Bool {
	i >= len || is_token_byte(binary.byte_at(name, i)) && token_bytes_from(name, i + 1, len)
}

// Token bytes are ALPHA / DIGIT plus the 15 symbols !#$%&'*+-.^_`|~ — nothing
// else (crucially not colon, space, CR or LF) may appear in a field name. The
// symbols are grouped by the contiguous ASCII runs #$%&' (35-39), *+ (42-43),
// -. (45-46) and ^_` (94-96). One flat test: `valid` walks every name byte on
// every response, so the per-byte cost is a single call frame.
fn is_token_byte(b Int) Bool {
	b >= 97 && b <= 122 || b >= 65 && b <= 90 || b >= 48 && b <= 57 || b == 33 ||
	b >= 35 && b <= 39 ||
	b == 42 ||
	b == 43 ||
	b == 45 ||
	b == 46 ||
	b >= 94 && b <= 96 ||
	b == 124 ||
	b == 126
}

// A field value must not contain CR, LF or NUL: a bare CR/LF ends the field
// line early — the response-splitting primitive — and NUL is flatly forbidden
// by RFC 9110 §5.5. Each forbidden byte is ruled out by one native index_of
// scan rather than an AL call per value byte — `valid` runs on every response
// the driver writes, and values carry most of a header block's bytes.
fn valid_value(value Binary) Bool {
	lacks(value, cr) && lacks(value, lf) && lacks(value, nul)
}

fn lacks(v Binary, byte Binary) Bool {
	match binary.index_of(v, byte, 0) {
		Some(_) -> False
		None -> True
	}
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
			if hit {
				True
			} else {
				contains_token(rest, name, token)
			}
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
	// Empty list elements (`a,,b`, a trailing comma, an empty value) are
	// ignored per RFC 9110 — without the hi > lo guard an empty element would
	// "match" an empty token.
	if hi > lo && binary.eq_ignore_ascii_case(binary.slice_bytes(v, lo, hi - lo), token) {
		True
	} else if to < len {
		value_has_token(v, to + 1, len, token)
	} else {
		False
	}
}

fn skip_ows(v Binary, i Int, hi Int) Int {
	if i < hi && is_ows(binary.byte_at(v, i)) {
		skip_ows(v, i + 1, hi)
	} else {
		i
	}
}

fn trim_ows(v Binary, lo Int, i Int) Int {
	if i > lo && is_ows(binary.byte_at(v, i - 1)) {
		trim_ows(v, lo, i - 1)
	} else {
		i
	}
}

fn is_ows(b Int) Bool {
	b == sp || b == htab
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
