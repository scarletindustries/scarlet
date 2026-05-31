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
const colon_space = binary.from_string(': ')
const crlf = binary.from_string('\r\n')

// The value of the first field whose name matches `name` (case-insensitive).
pub fn get(h Headers, name Binary) Option(Binary) {
	match h {
		[] -> None
		[field, ..rest] -> if binary.eq_ignore_ascii_case(field.name, name) {
			Some(field.value)
		} else {
			get(rest, name)
		}
	}
}

// Whether any field matches `name` (case-insensitive).
pub fn has(h Headers, name Binary) Bool {
	match h {
		[] -> False
		[field, ..rest] -> if binary.eq_ignore_ascii_case(field.name, name) {
			True
		} else {
			has(rest, name)
		}
	}
}

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
	match h {
		[] -> [Header(name: name, value: value)]
		[field, ..rest] -> [field, ..append(rest, name, value)]
	}
}

// The wire bytes of the header block as a parts array — `name`, `": "`,
// `value`, CRLF for each field — ready for socket.write_parts. The pieces are
// never concatenated; the blank line that ends the block is the caller's job.
pub fn render(h Headers) Array(Binary) {
	match h {
		[] -> []
		[field, ..rest] -> [field.name, colon_space, field.value, crlf, ..render(rest)]
	}
}
