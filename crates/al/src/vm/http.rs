//! Native HTTP/1.1 request-head parsing, body-framing analysis, header lookup,
//! and response-head serialization — the byte-scanning hot paths behind
//! `al/http/h1` and `al/http/headers`.
//!
//! The split follows Erlang's `erlang:decode_packet(http_bin, ...)`: the *scan*
//! (find CRLF/SP/colon, build zero-copy field views) runs in native code, while
//! every protocol *decision* (keep-alive, 100-continue, response framing) stays
//! in AL. The grammar and every reject here mirror what the AL reference
//! parser enforced, status codes included; `crates/al/tests/programs/http_parse.al`
//! locks that contract.
//!
//! All views returned to AL share the request buffer's backing `Arc` — parsing
//! a head allocates only the result values (method/target/name/value views,
//! `Header` records, the headers array), never intermediate copies.

use std::rc::Rc;
use std::sync::Arc;

use al_core::bytecode::{BinaryValue, HeapValue, Value};

use super::{PreludeTemplates, VmResult, int_to_ascii, parse_int_ascii};

/// Total request-head size cap (request line + all header fields), mirrored by
/// `al/http/h1`'s docs. A head that grows past this without completing is
/// rejected rather than buffered forever.
const MAX_HEAD: usize = 65536;

// ---------------------------------------------------------------------------
// Parsed / Framing / Option constructors over the prelude templates
// ---------------------------------------------------------------------------

fn need_more(t: &PreludeTemplates) -> Value {
    Value::enum_rc(Rc::clone(&t.parsed_need_more))
}

fn bad(t: &PreludeTemplates, status: i64) -> Value {
    PreludeTemplates::with_payload(&t.parsed_bad, Value::int(status))
}

fn invalid(t: &PreludeTemplates, status: i64) -> Value {
    PreludeTemplates::with_payload(&t.framing_invalid, Value::int(status))
}

fn chunked_need_more(t: &PreludeTemplates) -> Value {
    Value::enum_rc(Rc::clone(&t.chunked_need_more))
}

fn chunked_bad(t: &PreludeTemplates, status: i64) -> Value {
    PreludeTemplates::with_payload(&t.chunked_bad, Value::int(status))
}

/// The `(name, value)` payload values of an `al/http/headers.Header`.
fn header_fields(h: &Value) -> Option<(&Value, &Value)> {
    let e = h.as_enum()?;
    match (e.payload.first(), e.payload.get(1)) {
        (Some(n), Some(v)) => Some((n, v)),
        _ => None,
    }
}

/// The logical bytes of a header-field payload value, or `None` if it is not a
/// byte-aligned `Binary` (well-typed programs never hit that).
fn field_bytes(v: &Value) -> Option<std::borrow::Cow<'_, [u8]>> {
    match v.as_heap() {
        Some(HeapValue::Binary(b)) => Some(b.full_bytes()),
        _ => None,
    }
}

const NOT_HEADERS: &str = "expected an Array(Header). This is likely a compiler bug.";

// ---------------------------------------------------------------------------
// Request-head parser
// ---------------------------------------------------------------------------

/// Parse one request head out of `bin` starting at byte offset `off`, pushing
/// an `al/http/h1.Parsed` value.
pub(super) fn parse_head(t: &PreludeTemplates, bin: &BinaryValue, off: i64) -> Value {
    // Socket buffers are always byte-aligned views. A bit-unaligned buffer
    // (hand-built via `<<>>`) is parsed over a materialized aligned copy, so
    // the returned views point into that copy instead of the original.
    if bin.bit_offset.is_multiple_of(8) {
        let base = (bin.bit_offset / 8) as usize;
        let len = (bin.bit_len / 8) as usize;
        parse_head_window(t, &bin.backing, base, len, off)
    } else {
        let aligned: Arc<[u8]> = bin.to_aligned_vec().into();
        let len = (bin.bit_len / 8) as usize;
        parse_head_window(t, &aligned, 0, len, off)
    }
}

/// `backing[base .. base+len]` is the logical byte window of the request
/// buffer. Every offset below is relative to that window (the AL-visible
/// binary), so the `consumed` offset and the returned views line up with the
/// offsets the AL caller passed in.
fn parse_head_window(
    t: &PreludeTemplates,
    backing: &Arc<[u8]>,
    base: usize,
    len: usize,
    off: i64,
) -> Value {
    let bytes = &backing[base..base + len];
    let view = |start: usize, n: usize| -> Value {
        Value::binary_view(
            Arc::clone(backing),
            ((base + start) * 8) as u64,
            (n * 8) as u64,
        )
    };

    let mut off = (off.max(0) as usize).min(len);

    // Request line: skip a leading empty line (RFC 7230 §3.5), then split
    // METHOD SP request-target SP HTTP-version at the first CRLF.
    let eol = loop {
        match find_crlf(bytes, off) {
            None => {
                return if len - off > MAX_HEAD {
                    bad(t, 414)
                } else {
                    need_more(t)
                };
            }
            Some(eol) if eol == off => off += 2,
            Some(eol) => break eol,
        }
    };

    let line = &bytes[off..eol];
    let Some(sp1_rel) = memchr::memchr(b' ', line) else {
        return bad(t, 400);
    };
    if sp1_rel == 0 {
        return bad(t, 400);
    }
    let Some(sp2_rel) = memchr::memchr(b' ', &line[sp1_rel + 1..]) else {
        return bad(t, 400);
    };
    if sp2_rel == 0 {
        return bad(t, 400);
    }
    let sp1 = off + sp1_rel;
    let sp2 = sp1 + 1 + sp2_rel;
    let version = match &bytes[sp2 + 1..eol] {
        b"HTTP/1.1" => 1,
        b"HTTP/1.0" => 0,
        _ => return bad(t, 505),
    };

    // Header block: one field per CRLF-terminated line, ended by a blank line.
    // The size cap measures from the request-line start, so the whole head
    // (request line + every field) shares MAX_HEAD.
    let (headers, consumed) =
        match parse_header_block(t, backing, base, bytes, eol + 2, off, MAX_HEAD, 431) {
            HeaderBlock::Done(headers, consumed) => (headers, consumed),
            HeaderBlock::NeedMore => return need_more(t),
            HeaderBlock::Bad(status) => return bad(t, status),
        };

    PreludeTemplates::with_payloads(
        &t.parsed_done,
        vec![
            view(off, sp1 - off),
            view(sp1 + 1, sp2 - sp1 - 1),
            Value::int(version),
            Value::array(headers),
            Value::int(consumed as i64),
        ],
    )
}

/// Outcome of parsing one CRLF-terminated header block (request-head fields
/// or chunked-body trailers).
enum HeaderBlock {
    /// The parsed fields and the offset just past the blank line that ended
    /// the block.
    Done(Vec<Value>, usize),
    NeedMore,
    Bad(i64),
}

/// Parse a header block at `start`: one field per CRLF-terminated line, ended
/// by a blank line. Obs-fold (a continuation line starting with SP/HTAB) and
/// whitespace between the field name and the colon are rejected — both are
/// request-smuggling vectors different parsers disagree on (RFC 7230 §3.2.4).
///
/// `cap_start`/`cap`/`over_cap_status` bound the block: the request head
/// measures from the request-line start (MAX_HEAD → 431); a chunked body's
/// trailer block measures from the block itself. A block that grows past the
/// cap without completing is rejected rather than buffered forever.
#[allow(clippy::too_many_arguments)]
fn parse_header_block(
    t: &PreludeTemplates,
    backing: &Arc<[u8]>,
    base: usize,
    bytes: &[u8],
    start: usize,
    cap_start: usize,
    cap: usize,
    over_cap_status: i64,
) -> HeaderBlock {
    let view = |s: usize, n: usize| -> Value {
        Value::binary_view(Arc::clone(backing), ((base + s) * 8) as u64, (n * 8) as u64)
    };
    let len = bytes.len();
    let mut pos = start;
    let mut headers: Vec<Value> = Vec::with_capacity(8);
    loop {
        if pos - cap_start > cap {
            return HeaderBlock::Bad(over_cap_status);
        }
        let Some(crlf) = find_crlf(bytes, pos) else {
            return if len - cap_start > cap {
                HeaderBlock::Bad(over_cap_status)
            } else {
                HeaderBlock::NeedMore
            };
        };
        if crlf == pos {
            return HeaderBlock::Done(headers, pos + 2);
        }
        if is_ows(bytes[pos]) {
            return HeaderBlock::Bad(400);
        }
        let Some(colon_rel) = memchr::memchr(b':', &bytes[pos..crlf]) else {
            return HeaderBlock::Bad(400);
        };
        let colon = pos + colon_rel;
        if colon == pos || is_ows(bytes[colon - 1]) {
            return HeaderBlock::Bad(400);
        }
        // Field value with optional whitespace trimmed from both ends.
        let mut vstart = colon + 1;
        while vstart < crlf && is_ows(bytes[vstart]) {
            vstart += 1;
        }
        let mut vend = crlf;
        while vend > vstart && is_ows(bytes[vend - 1]) {
            vend -= 1;
        }
        headers.push(PreludeTemplates::with_payloads(
            &t.header,
            vec![view(pos, colon - pos), view(vstart, vend - vstart)],
        ));
        pos = crlf + 2;
    }
}

#[inline]
fn is_ows(b: u8) -> bool {
    b == b' ' || b == b'\t'
}

/// First index `>= from` where a CRLF pair starts.
#[inline]
fn find_crlf(bytes: &[u8], from: usize) -> Option<usize> {
    if from >= bytes.len() {
        return None;
    }
    memchr::memmem::find(&bytes[from..], b"\r\n").map(|rel| from + rel)
}

// ---------------------------------------------------------------------------
// Body framing (RFC 7230 §3.3.3)
// ---------------------------------------------------------------------------

/// Decide how the message body is framed, pushing an `al/http/h1.Framing`.
/// `Transfer-Encoding: chunked` (the sole/final coding, case-insensitive)
/// frames the body as `Chunked`. Transfer-Encoding alongside Content-Length
/// is a smuggling conflict (400); any other transfer coding is unimplemented
/// (501) — never silently ignored. Multiple Content-Length fields, a
/// non-digit / overflowing value, or a redundant leading zero ("007") are all
/// rejected (400).
pub(super) fn framing(t: &PreludeTemplates, headers_val: &Value) -> VmResult<Value> {
    let Some(arr) = headers_val.as_array() else {
        return Err(format!("h1.framing: {NOT_HEADERS}"));
    };
    let mut te = 0usize;
    let mut te_value: Option<Value> = None;
    let mut cl = 0usize;
    let mut cl_value: Option<Value> = None;
    for h in arr.iter() {
        let Some((name, value)) = header_fields(h) else {
            return Err(format!("h1.framing: {NOT_HEADERS}"));
        };
        let Some(name_bytes) = field_bytes(name) else {
            return Err(format!("h1.framing: {NOT_HEADERS}"));
        };
        if name_bytes.eq_ignore_ascii_case(b"transfer-encoding") {
            te += 1;
            if te_value.is_none() {
                te_value = Some(value.clone());
            }
        } else if name_bytes.eq_ignore_ascii_case(b"content-length") {
            cl += 1;
            if cl_value.is_none() {
                cl_value = Some(value.clone());
            }
        }
    }
    if te > 0 {
        // Transfer-Encoding next to Content-Length is the classic
        // request-smuggling conflict: reject before looking at the coding.
        if cl > 0 {
            return Ok(invalid(t, 400));
        }
        // Only "chunked" as the sole transfer coding is decodable. A coding
        // list ("gzip, chunked"), a repeated TE field, or anything that is not
        // exactly "chunked" changes the body framing in ways this server does
        // not implement → 501, never silently ignored.
        if te == 1
            && let Some(v) = te_value
            && let Some(vb) = field_bytes(&v)
            && vb.eq_ignore_ascii_case(b"chunked")
        {
            return Ok(Value::enum_rc(Rc::clone(&t.framing_chunked)));
        }
        return Ok(invalid(t, 501));
    }
    match (cl, cl_value) {
        (0, _) => Ok(Value::enum_rc(Rc::clone(&t.framing_no_body))),
        (1, Some(v)) => {
            let Some(bytes) = field_bytes(&v) else {
                return Err(format!("h1.framing: {NOT_HEADERS}"));
            };
            match parse_int_ascii(&bytes, 10) {
                Some(n) if !(bytes.len() > 1 && bytes[0] == b'0') => Ok(
                    PreludeTemplates::with_payload(&t.framing_length, Value::int(n)),
                ),
                _ => Ok(invalid(t, 400)),
            }
        }
        _ => Ok(invalid(t, 400)),
    }
}

// ---------------------------------------------------------------------------
// Chunked transfer-encoding decoder (RFC 7230 §4.1)
// ---------------------------------------------------------------------------

/// Longest accepted chunk-size line (hex size + extensions, before its CRLF).
/// Real sizes are a handful of hex digits; anything growing past this without
/// a CRLF is hostile or garbage, never legitimate.
const MAX_CHUNK_SIZE_LINE: usize = 4096;

/// Trailer-block cap, mirroring the request head's `MAX_HEAD` (→ 431).
const MAX_TRAILER_BLOCK: usize = MAX_HEAD;

/// Decode a chunked transfer-encoded body from `bin` starting at byte offset
/// `off`, refusing to decode more than `max` body bytes. Pushes an
/// `al/http/h1.ChunkBody` value.
///
/// Incremental like `parse_head`: `ChunkedNeedMore` means the buffer does not
/// yet hold the complete body + terminator; the AL driver reads more bytes and
/// calls again with the same offset (each call re-scans — state lives in the
/// buffer, not in the VM). `consumed` on `ChunkedDone` is the offset just past
/// the final CRLF, i.e. the start of the next pipelined request.
pub(super) fn chunk_decode(t: &PreludeTemplates, bin: &BinaryValue, off: i64, max: i64) -> Value {
    // Socket buffers are always byte-aligned views; mirror parse_head for the
    // pathological hand-built unaligned case.
    if bin.bit_offset.is_multiple_of(8) {
        let base = (bin.bit_offset / 8) as usize;
        let len = (bin.bit_len / 8) as usize;
        chunk_decode_window(t, &bin.backing, base, len, off, max)
    } else {
        let aligned: Arc<[u8]> = bin.to_aligned_vec().into();
        let len = (bin.bit_len / 8) as usize;
        chunk_decode_window(t, &aligned, 0, len, off, max)
    }
}

fn chunk_decode_window(
    t: &PreludeTemplates,
    backing: &Arc<[u8]>,
    base: usize,
    len: usize,
    off: i64,
    max: i64,
) -> Value {
    let bytes = &backing[base..base + len];
    let off = (off.max(0) as usize).min(len);
    let max = max.max(0);

    let mut pos = off;
    let mut body: Vec<u8> = Vec::new();

    loop {
        // ---- chunk-size line: 1*HEXDIG [;extensions] CRLF ------------------
        let Some(eol) = find_crlf(bytes, pos) else {
            // No CRLF yet. A line already over the cap can never become valid;
            // otherwise wait for more bytes.
            return if len - pos > MAX_CHUNK_SIZE_LINE {
                chunked_bad(t, 400)
            } else {
                chunked_need_more(t)
            };
        };
        if eol - pos > MAX_CHUNK_SIZE_LINE {
            return chunked_bad(t, 400);
        }
        let line = &bytes[pos..eol];
        // Chunk extensions (";name=value") are legal and ignored; the size is
        // everything before the first ';'. The size itself is strict 1*HEXDIG —
        // no sign, no whitespace — and overflow comes back as None.
        let size_end = memchr::memchr(b';', line).unwrap_or(line.len());
        let Some(size) = parse_int_ascii(&line[..size_end], 16) else {
            return chunked_bad(t, 400);
        };

        if size == 0 {
            // ---- terminator: optional trailer block, ended by a blank line --
            // The trailer grammar and smuggling rejects are exactly the request
            // head's; the cap measures from the trailer block itself.
            let trailer_start = eol + 2;
            return match parse_header_block(
                t,
                backing,
                base,
                bytes,
                trailer_start,
                trailer_start,
                MAX_TRAILER_BLOCK,
                431,
            ) {
                HeaderBlock::Done(trailers, consumed) => PreludeTemplates::with_payloads(
                    &t.chunked_done,
                    vec![
                        Value::binary(body),
                        Value::array(trailers),
                        Value::int(consumed as i64),
                    ],
                ),
                HeaderBlock::NeedMore => chunked_need_more(t),
                HeaderBlock::Bad(status) => chunked_bad(t, status),
            };
        }

        // ---- decoded-size cap: checked BEFORE the data is buffered ---------
        // The check is in i64 space so a hostile 16-hex-digit size cannot wrap.
        if (body.len() as i64).saturating_add(size) > max {
            return chunked_bad(t, 413);
        }
        let size = size as usize;

        // ---- chunk data + its trailing CRLF --------------------------------
        let data_start = eol + 2;
        let Some(data_end) = data_start.checked_add(size) else {
            return chunked_bad(t, 400);
        };
        if data_end + 2 > len {
            return chunked_need_more(t);
        }
        if &bytes[data_end..data_end + 2] != b"\r\n" {
            // The size said the data ends here; if a CRLF does not follow, the
            // framing is lying. Never resynchronize — reject.
            return chunked_bad(t, 400);
        }
        body.extend_from_slice(&bytes[data_start..data_end]);
        pos = data_end + 2;
    }
}

// ---------------------------------------------------------------------------
// Header lookup
// ---------------------------------------------------------------------------

/// The value of the first header whose name matches (ASCII-case-insensitive),
/// as `Option(Binary)`.
pub(super) fn header_get(
    t: &PreludeTemplates,
    headers_val: &Value,
    name: &BinaryValue,
) -> VmResult<Value> {
    let Some(arr) = headers_val.as_array() else {
        return Err(format!("headers.get: {NOT_HEADERS}"));
    };
    let needle = name.full_bytes();
    for h in arr.iter() {
        let Some((hname, hvalue)) = header_fields(h) else {
            return Err(format!("headers.get: {NOT_HEADERS}"));
        };
        let Some(hname_bytes) = field_bytes(hname) else {
            return Err(format!("headers.get: {NOT_HEADERS}"));
        };
        if hname_bytes.eq_ignore_ascii_case(&needle) {
            return Ok(PreludeTemplates::with_payload(&t.some, hvalue.clone()));
        }
    }
    Ok(Value::enum_rc(Rc::clone(&t.none)))
}

/// Whether any header name matches (ASCII-case-insensitive).
pub(super) fn header_has(headers_val: &Value, name: &BinaryValue) -> VmResult<Value> {
    let Some(arr) = headers_val.as_array() else {
        return Err(format!("headers.has: {NOT_HEADERS}"));
    };
    let needle = name.full_bytes();
    for h in arr.iter() {
        let Some((hname, _)) = header_fields(h) else {
            return Err(format!("headers.has: {NOT_HEADERS}"));
        };
        let Some(hname_bytes) = field_bytes(hname) else {
            return Err(format!("headers.has: {NOT_HEADERS}"));
        };
        if hname_bytes.eq_ignore_ascii_case(&needle) {
            return Ok(Value::bool(true));
        }
    }
    Ok(Value::bool(false))
}

// ---------------------------------------------------------------------------
// Response-head serialization
// ---------------------------------------------------------------------------

/// Serialize "HTTP/1.1 <code> <reason>\r\n" + the header block + the blank
/// line into one contiguous buffer, ready for a single socket write.
pub(super) fn serialize_head(
    code: i64,
    reason: &BinaryValue,
    headers_val: &Value,
) -> VmResult<Value> {
    let Some(arr) = headers_val.as_array() else {
        return Err(format!("h1.serialize_head: {NOT_HEADERS}"));
    };
    let reason_bytes = reason.full_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(64 + reason_bytes.len() + arr.len() * 64);
    out.extend_from_slice(b"HTTP/1.1 ");
    out.extend_from_slice(&int_to_ascii(code, 10));
    out.push(b' ');
    out.extend_from_slice(&reason_bytes);
    out.extend_from_slice(b"\r\n");
    for h in arr.iter() {
        let Some((name, value)) = header_fields(h) else {
            return Err(format!("h1.serialize_head: {NOT_HEADERS}"));
        };
        let (Some(name_bytes), Some(value_bytes)) = (field_bytes(name), field_bytes(value)) else {
            return Err(format!("h1.serialize_head: {NOT_HEADERS}"));
        };
        out.extend_from_slice(&name_bytes);
        out.extend_from_slice(b": ");
        out.extend_from_slice(&value_bytes);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"\r\n");
    Ok(Value::binary(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use al_core::bytecode::ValueView;

    fn templates() -> PreludeTemplates {
        PreludeTemplates::new()
    }

    fn bin(s: &str) -> BinaryValue {
        let bytes: Vec<u8> = s.as_bytes().to_vec();
        let bit_len = (bytes.len() as u64) * 8;
        BinaryValue::view(bytes.into(), 0, bit_len)
    }

    fn variant_of(v: &Value) -> String {
        match v.kind() {
            ValueView::Heap(HeapValue::Enum(e)) => e.variant_name.to_string(),
            _ => panic!("expected enum"),
        }
    }

    fn payload_int(v: &Value, i: usize) -> i64 {
        v.as_enum().unwrap().payload[i].as_int().unwrap()
    }

    fn payload_bytes(v: &Value, i: usize) -> Vec<u8> {
        match v.as_enum().unwrap().payload[i].as_heap() {
            Some(HeapValue::Binary(b)) => b.full_bytes().into_owned(),
            _ => panic!("expected binary payload"),
        }
    }

    #[test]
    fn parses_simple_get() {
        let t = templates();
        let buf = bin("GET /path HTTP/1.1\r\nHost: example.com\r\n\r\n");
        let parsed = parse_head(&t, &buf, 0);
        assert_eq!(variant_of(&parsed), "Done");
        assert_eq!(payload_bytes(&parsed, 0), b"GET");
        assert_eq!(payload_bytes(&parsed, 1), b"/path");
        assert_eq!(payload_int(&parsed, 2), 1);
        // consumed == whole buffer
        assert_eq!(payload_int(&parsed, 4), buf.bit_len as i64 / 8);
    }

    #[test]
    fn header_views_share_backing_zero_copy() {
        let t = templates();
        let buf = bin("GET / HTTP/1.1\r\nHost:  spaced.example  \r\n\r\n");
        let parsed = parse_head(&t, &buf, 0);
        assert_eq!(variant_of(&parsed), "Done");
        let headers = &parsed.as_enum().unwrap().payload[3];
        let arr = headers.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        let h = arr.get(0).unwrap();
        let (name, value) = header_fields(h).unwrap();
        // OWS around the value is trimmed; the name keeps its case.
        assert_eq!(field_bytes(name).unwrap().as_ref(), b"Host");
        assert_eq!(field_bytes(value).unwrap().as_ref(), b"spaced.example");
        // Zero-copy: the view's backing is the request buffer's backing.
        match (name.as_heap(), h.as_enum()) {
            (Some(HeapValue::Binary(nb)), Some(_)) => {
                assert!(Arc::ptr_eq(&nb.backing, &buf.backing));
            }
            _ => panic!("expected binary"),
        }
    }

    #[test]
    fn incomplete_head_needs_more() {
        let t = templates();
        for partial in [
            "",
            "GET",
            "GET / HTTP/1.1",
            "GET / HTTP/1.1\r\n",
            "GET / HTTP/1.1\r\nHost: e",
            "GET / HTTP/1.1\r\nHost: e\r\n",
        ] {
            let parsed = parse_head(&t, &bin(partial), 0);
            assert_eq!(variant_of(&parsed), "NeedMore", "for {partial:?}");
        }
    }

    #[test]
    fn rejects_malformed_heads() {
        let t = templates();
        // (input, expected status)
        let cases: &[(&str, i64)] = &[
            // No spaces in request line.
            ("GET\r\n\r\n", 400),
            // One space only.
            ("GET /\r\n\r\n", 400),
            // Leading space.
            (" GET / HTTP/1.1\r\n\r\n", 400),
            // Two adjacent spaces: empty target.
            ("GET  HTTP/1.1\r\n\r\n", 400),
            // Unknown version.
            ("GET / HTTP/2.0\r\n\r\n", 505),
            ("GET / FOO\r\n\r\n", 505),
            // Obs-fold continuation line.
            ("GET / HTTP/1.1\r\nHost: a\r\n b\r\n\r\n", 400),
            // Whitespace before the colon.
            ("GET / HTTP/1.1\r\nHost : a\r\n\r\n", 400),
            // Header line with no colon.
            ("GET / HTTP/1.1\r\nHost\r\n\r\n", 400),
            // Empty header name.
            ("GET / HTTP/1.1\r\n: a\r\n\r\n", 400),
        ];
        for (input, status) in cases {
            let parsed = parse_head(&t, &bin(input), 0);
            assert_eq!(variant_of(&parsed), "Bad", "for {input:?}");
            assert_eq!(payload_int(&parsed, 0), *status, "for {input:?}");
        }
    }

    #[test]
    fn skips_leading_empty_line_and_reports_consumed() {
        let t = templates();
        let buf = bin("\r\nGET / HTTP/1.1\r\n\r\nGET /next HTTP/1.1\r\n\r\n");
        let parsed = parse_head(&t, &buf, 0);
        assert_eq!(variant_of(&parsed), "Done");
        // consumed points just past the first head's blank line.
        let consumed = payload_int(&parsed, 4);
        assert_eq!(consumed, "\r\nGET / HTTP/1.1\r\n\r\n".len() as i64);
        // Parsing again from `consumed` yields the pipelined request.
        let second = parse_head(&t, &buf, consumed);
        assert_eq!(variant_of(&second), "Done");
        assert_eq!(payload_bytes(&second, 1), b"/next");
    }

    #[test]
    fn oversized_head_is_rejected() {
        let t = templates();
        // A single header line longer than MAX_HEAD with no terminating CRLF.
        let mut s = String::from("GET / HTTP/1.1\r\nX: ");
        s.push_str(&"a".repeat(MAX_HEAD + 16));
        let parsed = parse_head(&t, &bin(&s), 0);
        assert_eq!(variant_of(&parsed), "Bad");
        assert_eq!(payload_int(&parsed, 0), 431);

        // No CRLF at all and oversized: URI-too-long reject.
        let long_line = "G".repeat(MAX_HEAD + 16);
        let parsed = parse_head(&t, &bin(&long_line), 0);
        assert_eq!(variant_of(&parsed), "Bad");
        assert_eq!(payload_int(&parsed, 0), 414);
    }

    fn header_value(t: &PreludeTemplates, src: &str, name: &str) -> Option<Vec<u8>> {
        let parsed = parse_head(t, &bin(src), 0);
        let headers = parsed.as_enum().unwrap().payload[3].clone();
        let got = header_get(t, &headers, &bin(name)).unwrap();
        match got.kind() {
            ValueView::Heap(HeapValue::Enum(e)) if &*e.variant_name == "Some" => {
                match e.payload[0].as_heap() {
                    Some(HeapValue::Binary(b)) => Some(b.full_bytes().into_owned()),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    #[test]
    fn header_get_is_case_insensitive() {
        let t = templates();
        let src = "GET / HTTP/1.1\r\nContent-Type: text/plain\r\nHost: h\r\n\r\n";
        assert_eq!(
            header_value(&t, src, "content-type").as_deref(),
            Some(b"text/plain".as_ref())
        );
        assert_eq!(
            header_value(&t, src, "CONTENT-TYPE").as_deref(),
            Some(b"text/plain".as_ref())
        );
        assert_eq!(header_value(&t, src, "missing"), None);
    }

    fn framing_of(t: &PreludeTemplates, src: &str) -> (String, Option<i64>) {
        let parsed = parse_head(t, &bin(src), 0);
        assert_eq!(variant_of(&parsed), "Done", "for {src:?}");
        let headers = parsed.as_enum().unwrap().payload[3].clone();
        let f = framing(t, &headers).unwrap();
        let variant = variant_of(&f);
        let arg = f
            .as_enum()
            .unwrap()
            .payload
            .first()
            .and_then(|v| v.as_int());
        (variant, arg)
    }

    #[test]
    fn framing_decisions() {
        let t = templates();
        let base = "GET / HTTP/1.1\r\n";
        // (headers, expected variant, expected payload)
        let cases: &[(&str, &str, Option<i64>)] = &[
            // No body headers at all.
            ("Host: h\r\n\r\n", "NoBody", None),
            // Clean Content-Length.
            ("Content-Length: 42\r\n\r\n", "Length", Some(42)),
            // A lone zero is fine.
            ("Content-Length: 0\r\n\r\n", "Length", Some(0)),
            // Leading-zero smuggling vector.
            ("Content-Length: 007\r\n\r\n", "Invalid", Some(400)),
            // Non-numeric / negative / overflow.
            ("Content-Length: abc\r\n\r\n", "Invalid", Some(400)),
            ("Content-Length: -1\r\n\r\n", "Invalid", Some(400)),
            (
                "Content-Length: 99999999999999999999\r\n\r\n",
                "Invalid",
                Some(400),
            ),
            // Duplicate Content-Length.
            (
                "Content-Length: 5\r\nContent-Length: 5\r\n\r\n",
                "Invalid",
                Some(400),
            ),
            // Transfer-Encoding: chunked — the one decodable transfer coding.
            ("Transfer-Encoding: chunked\r\n\r\n", "Chunked", None),
            // Case-insensitive coding token.
            ("Transfer-Encoding: CHUNKED\r\n\r\n", "Chunked", None),
            // Any other coding (or a coding list) is unimplemented, never ignored.
            ("Transfer-Encoding: gzip\r\n\r\n", "Invalid", Some(501)),
            (
                "Transfer-Encoding: gzip, chunked\r\n\r\n",
                "Invalid",
                Some(501),
            ),
            // Repeated TE fields: framing is ambiguous → unimplemented.
            (
                "Transfer-Encoding: chunked\r\nTransfer-Encoding: chunked\r\n\r\n",
                "Invalid",
                Some(501),
            ),
            // TE + CL conflict: the classic smuggling vector.
            (
                "Transfer-Encoding: chunked\r\nContent-Length: 5\r\n\r\n",
                "Invalid",
                Some(400),
            ),
        ];
        for (headers, variant, arg) in cases {
            assert_eq!(
                framing_of(&t, &format!("{base}{headers}")),
                (variant.to_string(), *arg),
                "for {headers:?}"
            );
        }
    }

    // --- Chunked transfer-encoding decoder ---------------------------------

    fn chunk_of(t: &PreludeTemplates, input: &str, max: i64) -> Value {
        chunk_decode(t, &bin(input), 0, max)
    }

    #[test]
    fn decodes_single_chunk() {
        let t = templates();
        let v = chunk_of(&t, "5\r\nhello\r\n0\r\n\r\n", 1 << 20);
        assert_eq!(variant_of(&v), "ChunkedDone");
        assert_eq!(payload_bytes(&v, 0), b"hello");
        // No trailers; consumed covers the whole message.
        let trailers = &v.as_enum().unwrap().payload[1];
        assert_eq!(trailers.as_array().unwrap().len(), 0);
        assert_eq!(payload_int(&v, 2), "5\r\nhello\r\n0\r\n\r\n".len() as i64);
    }

    #[test]
    fn decodes_multiple_chunks_and_empty_body() {
        let t = templates();
        let v = chunk_of(&t, "5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n", 1 << 20);
        assert_eq!(variant_of(&v), "ChunkedDone");
        assert_eq!(payload_bytes(&v, 0), b"hello world");

        // A body that is just the terminator decodes to zero bytes.
        let v = chunk_of(&t, "0\r\n\r\n", 1 << 20);
        assert_eq!(variant_of(&v), "ChunkedDone");
        assert_eq!(payload_bytes(&v, 0), b"");
        assert_eq!(payload_int(&v, 2), 5);
    }

    #[test]
    fn every_proper_prefix_needs_more() {
        let t = templates();
        let full = "5\r\nhello\r\n6\r\n world\r\n0\r\nX-Sum: abc\r\n\r\n";
        for cut in 0..full.len() {
            let v = chunk_of(&t, &full[..cut], 1 << 20);
            assert_eq!(
                variant_of(&v),
                "ChunkedNeedMore",
                "prefix of {cut} bytes must be NeedMore"
            );
        }
        let v = chunk_of(&t, full, 1 << 20);
        assert_eq!(variant_of(&v), "ChunkedDone");
        assert_eq!(payload_bytes(&v, 0), b"hello world");
    }

    #[test]
    fn buffer_boundary_split_then_done() {
        let t = templates();
        // The decoder is stateless: a partial buffer yields NeedMore, and the
        // same call with the completed buffer yields the full body.
        let part = "5\r\nhel";
        assert_eq!(variant_of(&chunk_of(&t, part, 1 << 20)), "ChunkedNeedMore");
        let full = "5\r\nhello\r\n0\r\n\r\n";
        let v = chunk_of(&t, full, 1 << 20);
        assert_eq!(variant_of(&v), "ChunkedDone");
        assert_eq!(payload_bytes(&v, 0), b"hello");
    }

    #[test]
    fn trailers_round_trip() {
        let t = templates();
        let v = chunk_of(
            &t,
            "5\r\nhello\r\n0\r\nX-Checksum: abc123\r\nX-Count: 2\r\n\r\n",
            1 << 20,
        );
        assert_eq!(variant_of(&v), "ChunkedDone");
        let trailers = v.as_enum().unwrap().payload[1].clone();
        let arr = trailers.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        let h = arr.get(0).unwrap();
        let (name, value) = header_fields(h).unwrap();
        assert_eq!(field_bytes(name).unwrap().as_ref(), b"X-Checksum");
        assert_eq!(field_bytes(value).unwrap().as_ref(), b"abc123");
    }

    #[test]
    fn chunk_extensions_are_ignored() {
        let t = templates();
        let v = chunk_of(&t, "5;foo=bar;baz\r\nhello\r\n0;done\r\n\r\n", 1 << 20);
        assert_eq!(variant_of(&v), "ChunkedDone");
        assert_eq!(payload_bytes(&v, 0), b"hello");
    }

    #[test]
    fn rejects_malformed_chunks() {
        let t = templates();
        // (input, expected status)
        let cases: &[(&str, i64)] = &[
            // Non-hex size.
            ("zz\r\nhello\r\n0\r\n\r\n", 400),
            // Empty size line.
            ("\r\nhello\r\n0\r\n\r\n", 400),
            // Negative size (sign is not a HEXDIG).
            ("-5\r\nhello\r\n0\r\n\r\n", 400),
            // Chunk data not followed by CRLF (the size lied).
            ("5\r\nhelloXX\r\n0\r\n\r\n", 400),
            // Whitespace in the size.
            ("5 \r\nhello\r\n0\r\n\r\n", 400),
            // Trailer smuggling rejects: obs-fold and ws-before-colon.
            ("5\r\nhello\r\n0\r\nX-A: 1\r\n folded\r\n\r\n", 400),
            ("5\r\nhello\r\n0\r\nX-A : 1\r\n\r\n", 400),
        ];
        for (input, status) in cases {
            let v = chunk_of(&t, input, 1 << 20);
            assert_eq!(variant_of(&v), "ChunkedBad", "for {input:?}");
            assert_eq!(payload_int(&v, 0), *status, "for {input:?}");
        }
    }

    #[test]
    fn rejects_oversize_decoded_body() {
        let t = templates();
        // A single chunk larger than max: rejected from the size line alone,
        // before any data needs to be buffered.
        let v = chunk_of(&t, "FFFFFFFF\r\n", 1024);
        assert_eq!(variant_of(&v), "ChunkedBad");
        assert_eq!(payload_int(&v, 0), 413);

        // Cumulative chunks crossing max are also rejected.
        let v = chunk_of(&t, "400\r\n", 1024); // 0x400 = 1024 fits...
        assert_eq!(variant_of(&v), "ChunkedNeedMore");
        let body_1024 = "a".repeat(1024);
        let two_chunks = format!("400\r\n{body_1024}\r\n400\r\n{body_1024}\r\n0\r\n\r\n");
        let v = chunk_of(&t, &two_chunks, 1024);
        assert_eq!(variant_of(&v), "ChunkedBad");
        assert_eq!(payload_int(&v, 0), 413);

        // A 16-hex-digit size cannot wrap the cap check.
        let v = chunk_of(&t, "FFFFFFFFFFFFFFF\r\n", 1024);
        assert_eq!(variant_of(&v), "ChunkedBad");
        assert_eq!(payload_int(&v, 0), 413);
    }

    #[test]
    fn rejects_unbounded_size_line_and_trailers() {
        let t = templates();
        // A "size line" that grows past the cap without a CRLF is hostile.
        let long_line = "F".repeat(5000);
        let v = chunk_of(&t, &long_line, 1 << 20);
        assert_eq!(variant_of(&v), "ChunkedBad");
        assert_eq!(payload_int(&v, 0), 400);

        // A trailer block growing past MAX_HEAD without its blank line → 431.
        let mut s = String::from("5\r\nhello\r\n0\r\n");
        s.push_str("X-Long: ");
        s.push_str(&"a".repeat(MAX_TRAILER_BLOCK + 16));
        let v = chunk_of(&t, &s, 1 << 20);
        assert_eq!(variant_of(&v), "ChunkedBad");
        assert_eq!(payload_int(&v, 0), 431);
    }

    #[test]
    fn consumed_is_the_pipelining_offset() {
        let t = templates();
        let body = "5\r\nhello\r\n0\r\n\r\n";
        let s = format!("{body}GET /next HTTP/1.1\r\n\r\n");
        let v = chunk_of(&t, &s, 1 << 20);
        assert_eq!(variant_of(&v), "ChunkedDone");
        let consumed = payload_int(&v, 2) as usize;
        assert_eq!(consumed, body.len());
        assert!(s[consumed..].starts_with("GET /next"));

        // And decoding can start at a non-zero offset (the head's consumed).
        let with_head = format!("POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n{body}");
        let head_len = with_head.len() - body.len();
        let v = chunk_decode(&t, &bin(&with_head), head_len as i64, 1 << 20);
        assert_eq!(variant_of(&v), "ChunkedDone");
        assert_eq!(payload_bytes(&v, 0), b"hello");
        assert_eq!(payload_int(&v, 2) as usize, with_head.len());
    }

    #[test]
    fn serializes_response_head() {
        let t = templates();
        // Build a headers array via the parser (zero-copy views), then render.
        let parsed = parse_head(
            &t,
            &bin("GET / HTTP/1.1\r\nContent-Type: text/plain\r\nContent-Length: 2\r\n\r\n"),
            0,
        );
        let headers = parsed.as_enum().unwrap().payload[3].clone();
        let head = serialize_head(200, &bin("OK"), &headers).unwrap();
        let bytes = match head.as_heap() {
            Some(HeapValue::Binary(b)) => b.full_bytes().into_owned(),
            _ => panic!("expected binary"),
        };
        assert_eq!(
            bytes,
            b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\n\r\n"
        );
    }
}
