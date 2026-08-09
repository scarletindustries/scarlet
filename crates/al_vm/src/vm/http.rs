//! The byte-scanning hot paths behind `al/http/h1` and `al/http/headers`:
//! request-head parsing, body framing, header lookup, response-head
//! serialization.
//!
//! The scan runs here; every protocol decision (keep-alive, 100-continue,
//! response framing) stays in AL. The grammar and every reject status must
//! match the AL reference parser;
//! `crates/al/tests/programs/http_parse.al` locks that contract.
//!
//! Views returned to AL share the request buffer's backing `Arc`, so parsing a
//! head allocates only the result values.

use std::ops::ControlFlow;
use std::sync::Arc;

use crate::bytecode::{BinaryRef, Value};
use crate::heap::ProcHeap;

use super::text::Radix;
use crate::template::EnumTemplate;

use super::templates::H1;
use super::{VmError, VmResult, int_to_ascii, parse_uint_ascii};

/// Total request-head size cap, request line plus all header fields. Also
/// documented in `al/http/h1`.
const MAX_HEAD: usize = 65536;

/// The closed set of HTTP status codes this parser rejects with.
#[derive(Clone, Copy)]
#[repr(i64)]
enum Reject {
    BadRequest = 400,
    PayloadTooLarge = 413,
    UriTooLong = 414,
    HeaderFieldsTooLarge = 431,
    NotImplemented = 501,
    VersionNotSupported = 505,
}

// Prebuilt frozen `NeedMore` variants are cloned at call sites; payload-carrying
// rejects instantiate into the current process arena.
fn reject(tpl: &EnumTemplate, a: &mut ProcHeap, status: Reject) -> Value {
    tpl.instantiate(a, &[Value::small_int(status as i64)])
}

/// The `(name, value)` payload values of an `al/http/headers.Header`.
fn header_fields(h: &Value) -> Option<(&Value, &Value)> {
    let payload = h.as_enum()?.payload();
    match (payload.first(), payload.get(1)) {
        (Some(n), Some(v)) => Some((n, v)),
        _ => None,
    }
}

/// The logical bytes of a header-field payload value, or `None` if it is not a
/// byte-aligned `Binary`.
fn field_bytes(v: &Value) -> Option<std::borrow::Cow<'_, [u8]>> {
    v.as_binary().map(|b| b.full_bytes())
}

#[cold]
fn not_headers(op: &'static str) -> VmError {
    VmError::internal(format!("{op}: expected an Array(Header)"))
}

/// Walk an `Array(Header)`, calling `f(name_bytes, value)` per entry after
/// validating the shape. Every "not a headers array" error is built here, so
/// callers only see well-shaped pairs.
fn for_each_header<B>(
    headers: &Value,
    op: &'static str,
    mut f: impl FnMut(&[u8], &Value) -> VmResult<ControlFlow<B>>,
) -> VmResult<Option<B>> {
    let arr = headers.as_array().ok_or_else(|| not_headers(op))?;
    for h in arr.iter() {
        let (name, value) = header_fields(&h).ok_or_else(|| not_headers(op))?;
        let name_bytes = field_bytes(name).ok_or_else(|| not_headers(op))?;
        if let ControlFlow::Break(b) = f(&name_bytes, value)? {
            return Ok(Some(b));
        }
    }
    Ok(None)
}

/// A byte-aligned window over a `Binary`: `backing[base .. base+len]` is what
/// the AL caller sees. Socket buffers are byte-aligned and share the backing
/// `Arc`. A bit-unaligned buffer (hand-built with `<<>>`) is copied into an
/// aligned buffer, so views over the window point at the copy.
struct ByteWindow {
    backing: Arc<[u8]>,
    base: usize,
    len: usize,
}

impl ByteWindow {
    fn of(bin: &BinaryRef<'_>) -> Self {
        let len = (bin.bit_len() / 8) as usize;
        if bin.bit_offset().is_multiple_of(8) {
            Self {
                backing: bin.backing_arc(),
                base: (bin.bit_offset() / 8) as usize,
                len,
            }
        } else {
            Self {
                backing: bin.to_aligned_vec().into(),
                base: 0,
                len,
            }
        }
    }

    #[inline]
    fn bytes(&self) -> &[u8] {
        &self.backing[self.base..self.base + self.len]
    }

    /// A zero-copy binary view over `n` bytes at offset `start`. Offsets are
    /// AL-visible, so relative to `base`.
    fn view(&self, a: &mut ProcHeap, start: usize, n: usize) -> Value {
        Value::binary_view_in(
            a,
            Arc::clone(&self.backing),
            ((self.base + start) * 8) as u64,
            (n * 8) as u64,
        )
    }
}

/// Parse one request head out of `bin` starting at byte offset `off`, pushing
/// an `al/http/h1.Parsed` value.
pub(super) fn parse_head(t: &H1, a: &mut ProcHeap, bin: &BinaryRef<'_>, off: i64) -> Value {
    parse_head_window(t, a, &ByteWindow::of(bin), off)
}

/// Every offset here is relative to `win`, the AL-visible binary, so
/// `consumed` and the returned views line up with the caller's offsets.
fn parse_head_window(t: &H1, a: &mut ProcHeap, win: &ByteWindow, off: i64) -> Value {
    let bytes = win.bytes();
    let mut off = (off.max(0) as usize).min(win.len);
    let head_start = off;

    // Skip leading empty lines (RFC 7230 §3.5), then split
    // METHOD SP request-target SP HTTP-version at the first CRLF. The RFC wants
    // at least one tolerated; cap at four so a stream of bare CRLFs cannot hold
    // the connection open forever.
    let eol = loop {
        match find_crlf(bytes, off) {
            None => {
                return if win.len - head_start > MAX_HEAD {
                    reject(&t.parsed_bad, a, Reject::UriTooLong)
                } else {
                    t.parsed_need_more.clone()
                };
            }
            Some(eol) if eol == off => {
                off += 2;
                if off - head_start > 8 {
                    return reject(&t.parsed_bad, a, Reject::BadRequest);
                }
            }
            Some(eol) => break eol,
        }
    };

    let line = &bytes[off..eol];
    let Some(sp1_rel) = memchr::memchr(b' ', line) else {
        return reject(&t.parsed_bad, a, Reject::BadRequest);
    };
    if sp1_rel == 0 {
        return reject(&t.parsed_bad, a, Reject::BadRequest);
    }
    let Some(sp2_rel) = memchr::memchr(b' ', &line[sp1_rel + 1..]) else {
        return reject(&t.parsed_bad, a, Reject::BadRequest);
    };
    if sp2_rel == 0 {
        return reject(&t.parsed_bad, a, Reject::BadRequest);
    }
    let sp1 = off + sp1_rel;
    let sp2 = sp1 + 1 + sp2_rel;
    let version = match &bytes[sp2 + 1..eol] {
        b"HTTP/1.1" => t.version_http11.clone(),
        b"HTTP/1.0" => t.version_http10.clone(),
        _ => return reject(&t.parsed_bad, a, Reject::VersionNotSupported),
    };

    // The cap measures from the request-line start, so the whole head shares
    // MAX_HEAD.
    let (headers, flags, consumed) = match parse_header_block(
        t,
        a,
        win,
        eol + 2,
        off,
        MAX_HEAD,
        Reject::HeaderFieldsTooLarge,
    ) {
        HeaderBlock::Done(headers, flags, consumed) => (headers, flags, consumed),
        HeaderBlock::NeedMore => return t.parsed_need_more.clone(),
        HeaderBlock::Bad(status) => return reject(&t.parsed_bad, a, status),
    };

    let method = win.view(a, off, sp1 - off);
    let target = win.view(a, sp1 + 1, sp2 - sp1 - 1);
    let headers = Value::array_in(a, &headers);
    let flags = if flags == HeadFlags::default() {
        // Share the frozen all-false record instead of one per request.
        t.head_flags_none.clone()
    } else {
        t.head_flags.instantiate(
            a,
            &[
                Value::bool(flags.conn_close),
                Value::bool(flags.conn_keep_alive),
                Value::bool(flags.expect_100_continue),
            ],
        )
    };
    t.parsed_done.instantiate(
        a,
        &[
            method,
            target,
            version,
            headers,
            flags,
            Value::small_int(consumed as i64),
        ],
    )
}

/// The `Connection`/`Expect` token-list answers an HTTP/1.1 server needs from
/// every request head, recorded by `parse_header_block` while it already has
/// each field's trimmed name and value in hand. Raw findings, not decisions:
/// precedence lives in `al/http/h1.should_close`.
#[derive(Default, Clone, Copy, PartialEq)]
struct HeadFlags {
    conn_close: bool,
    conn_keep_alive: bool,
    expect_100_continue: bool,
}

/// Whether `value` carries `token` as one of its comma-separated elements.
/// Must match `al/http/headers.has_token`: elements are OWS-trimmed, compared
/// case-insensitively, and empty elements never match.
fn has_token(value: &[u8], token: &[u8]) -> bool {
    value.split(|&b| b == b',').any(|el| {
        let el = trim_ows(el);
        !el.is_empty() && el.eq_ignore_ascii_case(token)
    })
}

fn trim_ows(mut el: &[u8]) -> &[u8] {
    while let [first, rest @ ..] = el
        && is_ows(*first)
    {
        el = rest;
    }
    while let [rest @ .., last] = el
        && is_ows(*last)
    {
        el = rest;
    }
    el
}

/// Outcome of parsing one CRLF-terminated header block (request-head fields
/// or chunked-body trailers).
enum HeaderBlock {
    /// Fields, tokens seen among them, and the offset past the blank line.
    Done(Vec<Value>, HeadFlags, usize),
    NeedMore,
    Bad(Reject),
}

/// Parse a header block at `start`: one field per CRLF-terminated line, ended
/// by a blank line. Obs-fold and whitespace before the colon are rejected;
/// both are request-smuggling vectors parsers disagree on (RFC 7230 §3.2.4).
///
/// `cap_start`/`cap`/`over_cap_status` bound the block. A request head
/// measures from the request-line start; a trailer block from itself.
fn parse_header_block(
    t: &H1,
    a: &mut ProcHeap,
    win: &ByteWindow,
    start: usize,
    cap_start: usize,
    cap: usize,
    over_cap_status: Reject,
) -> HeaderBlock {
    let bytes = win.bytes();
    let len = bytes.len();
    let mut pos = start;
    let mut headers: Vec<Value> = Vec::with_capacity(8);
    let mut flags = HeadFlags::default();
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
            return HeaderBlock::Done(headers, flags, pos + 2);
        }
        if is_ows(bytes[pos]) {
            return HeaderBlock::Bad(Reject::BadRequest);
        }
        let Some(colon_rel) = memchr::memchr(b':', &bytes[pos..crlf]) else {
            return HeaderBlock::Bad(Reject::BadRequest);
        };
        let colon = pos + colon_rel;
        if colon == pos || is_ows(bytes[colon - 1]) {
            return HeaderBlock::Bad(Reject::BadRequest);
        }
        let mut vstart = colon + 1;
        while vstart < crlf && is_ows(bytes[vstart]) {
            vstart += 1;
        }
        let mut vend = crlf;
        while vend > vstart && is_ows(bytes[vend - 1]) {
            vend -= 1;
        }
        // Answer the token questions here, while the trimmed value is in hand,
        // rather than re-walking the field list once per question.
        let name_bytes = &bytes[pos..colon];
        if name_bytes.eq_ignore_ascii_case(b"connection") {
            let value_bytes = &bytes[vstart..vend];
            flags.conn_close |= has_token(value_bytes, b"close");
            flags.conn_keep_alive |= has_token(value_bytes, b"keep-alive");
        } else if name_bytes.eq_ignore_ascii_case(b"expect") {
            flags.expect_100_continue |= has_token(&bytes[vstart..vend], b"100-continue");
        }
        let name = win.view(a, pos, colon - pos);
        let value = win.view(a, vstart, vend - vstart);
        headers.push(t.header.instantiate(a, &[name, value]));
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

/// Decide how the message body is framed (RFC 7230 §3.3.3), pushing an
/// `al/http/h1.Framing`. Only `Transfer-Encoding: chunked` as the sole coding
/// is `Chunked`. Transfer-Encoding alongside Content-Length is a smuggling
/// conflict (400); any other coding is unimplemented (501). A duplicated,
/// non-digit, overflowing, or leading-zero Content-Length is a 400.
pub(super) fn framing(t: &H1, a: &mut ProcHeap, headers_val: &Value) -> VmResult<Value> {
    enum Seen {
        Zero,
        Once(Value),
        Many,
    }
    impl Seen {
        fn record(&mut self, v: &Value) {
            *self = match std::mem::replace(self, Seen::Zero) {
                Seen::Zero => Seen::Once(v.clone()),
                _ => Seen::Many,
            };
        }
    }
    let mut te = Seen::Zero;
    let mut cl = Seen::Zero;
    for_each_header(headers_val, "h1.framing", |name_bytes, value| {
        if name_bytes.eq_ignore_ascii_case(b"transfer-encoding") {
            te.record(value);
        } else if name_bytes.eq_ignore_ascii_case(b"content-length") {
            cl.record(value);
        }
        Ok(ControlFlow::<()>::Continue(()))
    })?;
    if !matches!(te, Seen::Zero) {
        // The smuggling conflict: reject before looking at the coding.
        if !matches!(cl, Seen::Zero) {
            return Ok(reject(&t.framing_invalid, a, Reject::BadRequest));
        }
        // A coding list, a repeated TE field, or anything that is not exactly
        // "chunked" must be 501, never silently ignored.
        if let Seen::Once(v) = &te
            && let Some(vb) = field_bytes(v)
            && vb.eq_ignore_ascii_case(b"chunked")
        {
            return Ok(t.framing_chunked.clone());
        }
        return Ok(reject(&t.framing_invalid, a, Reject::NotImplemented));
    }
    match cl {
        Seen::Zero => Ok(t.framing_no_body.clone()),
        Seen::Once(v) => {
            let parsed = match field_bytes(&v) {
                Some(bytes) => {
                    let leading_zero = bytes.len() > 1 && bytes[0] == b'0';
                    parse_uint_ascii(&bytes, Radix::Dec)
                        .filter(|&n| !leading_zero && Value::fits_small_int(n))
                }
                None => return Err(not_headers("h1.framing")),
            };
            match parsed {
                Some(n) => Ok(t.framing_length.instantiate(a, &[Value::small_int(n)])),
                None => Ok(reject(&t.framing_invalid, a, Reject::BadRequest)),
            }
        }
        Seen::Many => Ok(reject(&t.framing_invalid, a, Reject::BadRequest)),
    }
}

/// Longest accepted chunk-size line, hex size plus extensions, before its CRLF.
const MAX_CHUNK_SIZE_LINE: usize = 4096;

const MAX_TRAILER_BLOCK: usize = MAX_HEAD;

/// Decode a chunked body (RFC 7230 §4.1) from `bin` at byte offset `off`,
/// refusing more than `max` body bytes. Pushes an `al/http/h1.ChunkBody`.
///
/// Incremental like `parse_head`: on `ChunkedNeedMore` the AL driver reads more
/// bytes and calls again with the same offset. Every call re-scans, since state
/// lives in the buffer, not the VM. `consumed` on `ChunkedDone` is the start of
/// the next pipelined request.
pub(super) fn chunk_decode(
    t: &H1,
    a: &mut ProcHeap,
    bin: &BinaryRef<'_>,
    off: i64,
    max: i64,
) -> Value {
    chunk_decode_window(t, a, &ByteWindow::of(bin), off, max)
}

fn chunk_decode_window(t: &H1, a: &mut ProcHeap, win: &ByteWindow, off: i64, max: i64) -> Value {
    let bytes = win.bytes();
    let off = (off.max(0) as usize).min(win.len);
    let max = max.max(0);

    // Scan the framing recording data ranges without copying, so the common
    // NeedMore return costs O(chunk count), not O(decoded bytes). The body is
    // copied once, only when the terminator and trailers are all present.
    let mut pos = off;
    let mut total: i64 = 0;
    let mut segments: Vec<(usize, usize)> = Vec::new();

    loop {
        // Chunk-size line: 1*HEXDIG [;extensions] CRLF.
        let Some(eol) = find_crlf(bytes, pos) else {
            // A line already over the cap can never become valid.
            return if win.len - pos > MAX_CHUNK_SIZE_LINE {
                reject(&t.chunked_bad, a, Reject::BadRequest)
            } else {
                t.chunked_need_more.clone()
            };
        };
        if eol - pos > MAX_CHUNK_SIZE_LINE {
            return reject(&t.chunked_bad, a, Reject::BadRequest);
        }
        let line = &bytes[pos..eol];
        // Chunk extensions are legal and ignored; the size is everything before
        // the first ';', strict 1*HEXDIG, with overflow coming back as None.
        let size_end = memchr::memchr(b';', line).unwrap_or(line.len());
        let Some(size) = parse_uint_ascii(&line[..size_end], Radix::Hex) else {
            return reject(&t.chunked_bad, a, Reject::BadRequest);
        };

        if size == 0 {
            // Terminator: an optional trailer block ended by a blank line. Same
            // grammar as the request head, capped from the block itself.
            let trailer_start = eol + 2;
            return match parse_header_block(
                t,
                a,
                win,
                trailer_start,
                trailer_start,
                MAX_TRAILER_BLOCK,
                Reject::HeaderFieldsTooLarge,
            ) {
                // Tokens the trailer block recorded are dropped: a trailer field
                // carries no connection or expectation semantics (RFC 9110 §6.5.1).
                HeaderBlock::Done(trailers, _, consumed) => {
                    let mut body: Vec<u8> = Vec::with_capacity(total as usize);
                    for &(start, end) in &segments {
                        body.extend_from_slice(&bytes[start..end]);
                    }
                    let body = Value::binary_in(a, body);
                    let trailers = Value::array_in(a, &trailers);
                    t.chunked_done
                        .instantiate(a, &[body, trailers, Value::small_int(consumed as i64)])
                }
                HeaderBlock::NeedMore => t.chunked_need_more.clone(),
                HeaderBlock::Bad(status) => reject(&t.chunked_bad, a, status),
            };
        }

        // Cap the decoded size before accepting data, in i64 space so a hostile
        // 16-hex-digit size cannot wrap.
        if total.saturating_add(size) > max {
            return reject(&t.chunked_bad, a, Reject::PayloadTooLarge);
        }
        let size = size as usize;

        let data_start = eol + 2;
        let Some(data_end) = data_start.checked_add(size) else {
            return reject(&t.chunked_bad, a, Reject::BadRequest);
        };
        if data_end + 2 > win.len {
            return t.chunked_need_more.clone();
        }
        if &bytes[data_end..data_end + 2] != b"\r\n" {
            // The size lied about where the data ends. Never resynchronize.
            return reject(&t.chunked_bad, a, Reject::BadRequest);
        }
        segments.push((data_start, data_end));
        total += size as i64;
        pos = data_end + 2;
    }
}

/// The value of the first header whose name matches (ASCII-case-insensitive).
/// `op` names the caller for error messages.
fn find_header(
    headers_val: &Value,
    name: &BinaryRef<'_>,
    op: &'static str,
) -> VmResult<Option<Value>> {
    let needle = name.full_bytes();
    for_each_header(headers_val, op, |hname_bytes, hvalue| {
        Ok(if hname_bytes.eq_ignore_ascii_case(&needle) {
            ControlFlow::Break(hvalue.clone())
        } else {
            ControlFlow::Continue(())
        })
    })
}

/// The value of the first header whose name matches (ASCII-case-insensitive),
/// as `Option(Binary)`.
pub(super) fn header_get(
    t: &H1,
    a: &mut ProcHeap,
    headers_val: &Value,
    name: &BinaryRef<'_>,
) -> VmResult<Value> {
    Ok(match find_header(headers_val, name, "headers.get")? {
        Some(hvalue) => t.some.instantiate(a, &[hvalue]),
        None => t.none.clone(),
    })
}

/// Whether every field is safe to serialize: a non-empty RFC 9110 token name
/// (§5.6.2) and a CR/LF/NUL-free value. Native because the connection driver
/// runs it on every response it writes.
pub(super) fn headers_valid(headers_val: &Value) -> VmResult<Value> {
    fn is_token_byte(b: u8) -> bool {
        b.is_ascii_alphanumeric()
            | matches!(
                b,
                b'!' | b'#'..=b'\'' | b'*' | b'+' | b'-' | b'.' | b'^'..=b'`' | b'|' | b'~'
            )
    }
    let bad = for_each_header(headers_val, "headers.valid", |name, value| {
        if name.is_empty() || !name.iter().copied().all(is_token_byte) {
            return Ok(ControlFlow::Break(()));
        }
        let value_bytes = field_bytes(value).ok_or_else(|| not_headers("headers.valid"))?;
        if value_bytes
            .iter()
            .any(|&b| b == b'\r' || b == b'\n' || b == 0)
        {
            return Ok(ControlFlow::Break(()));
        }
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(Value::bool(bad.is_none()))
}

/// Whether any header name matches (ASCII-case-insensitive).
pub(super) fn header_has(headers_val: &Value, name: &BinaryRef<'_>) -> VmResult<Value> {
    Ok(Value::bool(
        find_header(headers_val, name, "headers.has")?.is_some(),
    ))
}

/// Serialize `HTTP/1.1 <code> <reason>` plus the header block and the blank
/// line into one contiguous buffer, ready for a single socket write.
pub(super) fn serialize_head(
    a: &mut ProcHeap,
    code: i64,
    reason: &BinaryRef<'_>,
    headers_val: &Value,
) -> VmResult<Value> {
    let reason_bytes = reason.full_bytes();
    // Capacity hint only; for_each_header owns the real shape check.
    let est = headers_val.as_array().map_or(0, |arr| arr.len());
    let mut out: Vec<u8> = Vec::with_capacity(64 + reason_bytes.len() + est * 64);
    out.extend_from_slice(b"HTTP/1.1 ");
    out.extend_from_slice(int_to_ascii(code, Radix::Dec).as_bytes());
    out.push(b' ');
    out.extend_from_slice(&reason_bytes);
    out.extend_from_slice(b"\r\n");
    for_each_header(headers_val, "h1.serialize_head", |name_bytes, value| {
        let value_bytes = field_bytes(value).ok_or_else(|| not_headers("h1.serialize_head"))?;
        out.extend_from_slice(name_bytes);
        out.extend_from_slice(b": ");
        out.extend_from_slice(&value_bytes);
        out.extend_from_slice(b"\r\n");
        Ok(ControlFlow::<()>::Continue(()))
    })?;
    out.extend_from_slice(b"\r\n");
    Ok(Value::binary_in(a, out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frozen::FrozenArea;

    /// The templates point into `_frozen`, so it must outlive them.
    struct Fix {
        _frozen: Arc<FrozenArea>,
        t: H1,
        h: ProcHeap,
    }

    fn fix() -> Fix {
        let frozen = Arc::new(FrozenArea::new());
        let mut fb = frozen.builder();
        let (templates, abi) = crate::template::test_fixture::build(&mut fb);
        let t = super::super::templates::Templates::resolve(&abi, &templates, &mut fb)
            .h1_owned()
            .expect("fixture binds every H1 slot");
        drop(fb);
        let h = ProcHeap::new();
        Fix {
            _frozen: frozen,
            t,
            h,
        }
    }

    impl Fix {
        fn bin(&mut self, s: &str) -> Value {
            Value::binary_in(&mut self.h, s.as_bytes().to_vec())
        }

        fn parse(&mut self, src: &str, off: i64) -> Value {
            let buf = self.bin(src);
            parse_head(&self.t, &mut self.h, &buf.as_binary().unwrap(), off)
        }

        fn chunk(&mut self, input: &str, off: i64, max: i64) -> Value {
            let buf = self.bin(input);
            chunk_decode(&self.t, &mut self.h, &buf.as_binary().unwrap(), off, max)
        }
    }

    fn variant_of(v: &Value) -> String {
        match v.as_enum() {
            Some(e) => e.variant_name().to_string(),
            None => panic!("expected enum"),
        }
    }

    fn payload_int(v: &Value, i: usize) -> i64 {
        v.as_enum().unwrap().payload()[i].as_int().unwrap()
    }

    fn payload_bytes(v: &Value, i: usize) -> Vec<u8> {
        match v.as_enum().unwrap().payload()[i].as_binary() {
            Some(b) => b.full_bytes().into_owned(),
            None => panic!("expected binary payload"),
        }
    }

    #[test]
    fn parses_simple_get() {
        let mut x = fix();
        let buf = x.bin("GET /path HTTP/1.1\r\nHost: example.com\r\n\r\n");
        let parsed = parse_head(&x.t, &mut x.h, &buf.as_binary().unwrap(), 0);
        assert_eq!(variant_of(&parsed), "Done");
        assert_eq!(payload_bytes(&parsed, 0), b"GET");
        assert_eq!(payload_bytes(&parsed, 1), b"/path");
        assert_eq!(
            variant_of(&parsed.as_enum().unwrap().payload()[2]),
            "Http11"
        );
        assert_eq!(
            payload_int(&parsed, 5),
            buf.as_binary().unwrap().bit_len() as i64 / 8
        );
    }

    #[test]
    fn header_views_share_backing_zero_copy() {
        let mut x = fix();
        let buf = x.bin("GET / HTTP/1.1\r\nHost:  spaced.example  \r\n\r\n");
        let parsed = parse_head(&x.t, &mut x.h, &buf.as_binary().unwrap(), 0);
        assert_eq!(variant_of(&parsed), "Done");
        let headers = &parsed.as_enum().unwrap().payload()[3];
        let arr = headers.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        let h = arr.get(0).unwrap();
        let (name, value) = header_fields(&h).unwrap();
        assert_eq!(field_bytes(name).unwrap().as_ref(), b"Host");
        assert_eq!(field_bytes(value).unwrap().as_ref(), b"spaced.example");
        let nb = name.as_binary().expect("expected binary");
        assert!(std::ptr::eq(
            nb.backing().as_ptr(),
            buf.as_binary().unwrap().backing().as_ptr()
        ));
    }

    #[test]
    fn incomplete_head_needs_more() {
        let mut x = fix();
        for partial in [
            "",
            "GET",
            "GET / HTTP/1.1",
            "GET / HTTP/1.1\r\n",
            "GET / HTTP/1.1\r\nHost: e",
            "GET / HTTP/1.1\r\nHost: e\r\n",
        ] {
            let parsed = x.parse(partial, 0);
            assert_eq!(variant_of(&parsed), "NeedMore", "for {partial:?}");
        }
    }

    #[test]
    fn rejects_malformed_heads() {
        let mut x = fix();
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
            let parsed = x.parse(input, 0);
            assert_eq!(variant_of(&parsed), "Bad", "for {input:?}");
            assert_eq!(payload_int(&parsed, 0), *status, "for {input:?}");
        }
    }

    #[test]
    fn caps_leading_empty_lines() {
        let mut x = fix();
        // Up to four leading blank lines are tolerated.
        let parsed = x.parse("\r\n\r\n\r\n\r\nGET / HTTP/1.1\r\n\r\n", 0);
        assert_eq!(variant_of(&parsed), "Done");
        // A fifth is rejected: a CRLF stream cannot hold the connection open.
        let parsed = x.parse("\r\n\r\n\r\n\r\n\r\nGET / HTTP/1.1\r\n\r\n", 0);
        assert_eq!(variant_of(&parsed), "Bad");
        assert_eq!(payload_int(&parsed, 0), 400);
    }

    #[test]
    fn skips_leading_empty_line_and_reports_consumed() {
        let mut x = fix();
        let buf = x.bin("\r\nGET / HTTP/1.1\r\n\r\nGET /next HTTP/1.1\r\n\r\n");
        let parsed = parse_head(&x.t, &mut x.h, &buf.as_binary().unwrap(), 0);
        assert_eq!(variant_of(&parsed), "Done");
        let consumed = payload_int(&parsed, 5);
        assert_eq!(consumed, "\r\nGET / HTTP/1.1\r\n\r\n".len() as i64);
        // Parsing again from `consumed` yields the pipelined request.
        let second = parse_head(&x.t, &mut x.h, &buf.as_binary().unwrap(), consumed);
        assert_eq!(variant_of(&second), "Done");
        assert_eq!(payload_bytes(&second, 1), b"/next");
    }

    /// `(conn_close, conn_keep_alive, expect_100_continue)` as recorded on a
    /// parsed head.
    fn flags_of(x: &mut Fix, src: &str) -> (bool, bool, bool) {
        let parsed = x.parse(src, 0);
        assert_eq!(variant_of(&parsed), "Done", "for {src:?}");
        let f = parsed.as_enum().unwrap().payload()[4].clone();
        let p = f.as_enum().expect("expected a HeadFlags record").payload();
        (
            p[0].as_bool().unwrap(),
            p[1].as_bool().unwrap(),
            p[2].as_bool().unwrap(),
        )
    }

    #[test]
    fn records_connection_and_expect_tokens() {
        let mut x = fix();
        let base = "GET / HTTP/1.1\r\n";
        // (header block, close, keep-alive, 100-continue)
        let cases: &[(&str, bool, bool, bool)] = &[
            ("Host: h\r\n\r\n", false, false, false),
            ("Connection: close\r\n\r\n", true, false, false),
            // Case-insensitive on both the name and the token.
            ("CONNECTION: Close\r\n\r\n", true, false, false),
            ("connection: KEEP-ALIVE\r\n\r\n", false, true, false),
            // Token list: OWS-trimmed elements, non-matching ones ignored.
            (
                "Connection: keep-alive, Upgrade\r\n\r\n",
                false,
                true,
                false,
            ),
            ("Connection: Upgrade,close\r\n\r\n", true, false, false),
            // Both options present: recorded raw, the precedence is AL's call.
            ("Connection: close, keep-alive\r\n\r\n", true, true, false),
            // Empty elements are ignored, never a match.
            ("Connection: ,,\r\n\r\n", false, false, false),
            ("Connection: \r\n\r\n", false, false, false),
            // A token must match whole, not by prefix/substring.
            ("Connection: closed\r\n\r\n", false, false, false),
            ("Connection: no-keep-alive\r\n\r\n", false, false, false),
            // Repeated fields union, as a single list value would.
            (
                "Connection: keep-alive\r\nConnection: close\r\n\r\n",
                true,
                true,
                false,
            ),
            ("Expect: 100-continue\r\n\r\n", false, false, true),
            ("expect: 100-Continue\r\n\r\n", false, false, true),
            ("Expect: other\r\n\r\n", false, false, false),
            // A name merely containing "connection"/"expect" is another field.
            ("X-Connection: close\r\n\r\n", false, false, false),
            ("Expectation: 100-continue\r\n\r\n", false, false, false),
            (
                "Connection: close\r\nExpect: 100-continue\r\n\r\n",
                true,
                false,
                true,
            ),
        ];
        for (block, close, keep_alive, expect) in cases {
            assert_eq!(
                flags_of(&mut x, &format!("{base}{block}")),
                (*close, *keep_alive, *expect),
                "for {block:?}"
            );
        }
    }

    #[test]
    fn trailers_never_contribute_flags() {
        // RFC 9110 §6.5.1: only the head's own fields are a source of flags.
        let mut x = fix();
        let v = chunk_of(
            &mut x,
            "5\r\nhello\r\n0\r\nConnection: close\r\nExpect: 100-continue\r\n\r\n",
            1 << 20,
        );
        assert_eq!(variant_of(&v), "ChunkedDone");
        // The trailers themselves still round-trip as ordinary fields.
        assert_eq!(
            v.as_enum().unwrap().payload()[1].as_array().unwrap().len(),
            2
        );
        assert_eq!(
            flags_of(
                &mut x,
                "POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n"
            ),
            (false, false, false)
        );
    }

    #[test]
    fn oversized_head_is_rejected() {
        let mut x = fix();
        // A single header line longer than MAX_HEAD with no terminating CRLF.
        let mut s = String::from("GET / HTTP/1.1\r\nX: ");
        s.push_str(&"a".repeat(MAX_HEAD + 16));
        let parsed = x.parse(&s, 0);
        assert_eq!(variant_of(&parsed), "Bad");
        assert_eq!(payload_int(&parsed, 0), 431);

        // No CRLF at all and oversized: URI-too-long reject.
        let long_line = "G".repeat(MAX_HEAD + 16);
        let parsed = x.parse(&long_line, 0);
        assert_eq!(variant_of(&parsed), "Bad");
        assert_eq!(payload_int(&parsed, 0), 414);
    }

    fn header_value(x: &mut Fix, src: &str, name: &str) -> Option<Vec<u8>> {
        let parsed = x.parse(src, 0);
        let headers = parsed.as_enum().unwrap().payload()[3].clone();
        let name_v = x.bin(name);
        let got = header_get(&x.t, &mut x.h, &headers, &name_v.as_binary().unwrap()).unwrap();
        match got.as_enum() {
            Some(e) if e.variant_name() == "Some" => e.payload()[0]
                .as_binary()
                .map(|b| b.full_bytes().into_owned()),
            _ => None,
        }
    }

    #[test]
    fn header_get_is_case_insensitive() {
        let mut x = fix();
        let src = "GET / HTTP/1.1\r\nContent-Type: text/plain\r\nHost: h\r\n\r\n";
        assert_eq!(
            header_value(&mut x, src, "content-type").as_deref(),
            Some(b"text/plain".as_ref())
        );
        assert_eq!(
            header_value(&mut x, src, "CONTENT-TYPE").as_deref(),
            Some(b"text/plain".as_ref())
        );
        assert_eq!(header_value(&mut x, src, "missing"), None);
    }

    fn framing_of(x: &mut Fix, src: &str) -> (String, Option<i64>) {
        let parsed = x.parse(src, 0);
        assert_eq!(variant_of(&parsed), "Done", "for {src:?}");
        let headers = parsed.as_enum().unwrap().payload()[3].clone();
        let f = framing(&x.t, &mut x.h, &headers).unwrap();
        let variant = variant_of(&f);
        let arg = f
            .as_enum()
            .unwrap()
            .payload()
            .first()
            .and_then(|v| v.as_int());
        (variant, arg)
    }

    #[test]
    fn framing_decisions() {
        let mut x = fix();
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
            // Fits i64 but not the 48-bit payload: reject, never truncate.
            (
                "Content-Length: 200000000000000\r\n\r\n",
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
                framing_of(&mut x, &format!("{base}{headers}")),
                (variant.to_string(), *arg),
                "for {headers:?}"
            );
        }
    }

    fn chunk_of(x: &mut Fix, input: &str, max: i64) -> Value {
        x.chunk(input, 0, max)
    }

    #[test]
    fn decodes_single_chunk() {
        let mut x = fix();
        let v = chunk_of(&mut x, "5\r\nhello\r\n0\r\n\r\n", 1 << 20);
        assert_eq!(variant_of(&v), "ChunkedDone");
        assert_eq!(payload_bytes(&v, 0), b"hello");
        // No trailers; consumed covers the whole message.
        let trailers = &v.as_enum().unwrap().payload()[1];
        assert_eq!(trailers.as_array().unwrap().len(), 0);
        assert_eq!(payload_int(&v, 2), "5\r\nhello\r\n0\r\n\r\n".len() as i64);
    }

    #[test]
    fn decodes_multiple_chunks_and_empty_body() {
        let mut x = fix();
        let v = chunk_of(&mut x, "5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n", 1 << 20);
        assert_eq!(variant_of(&v), "ChunkedDone");
        assert_eq!(payload_bytes(&v, 0), b"hello world");

        // A body that is just the terminator decodes to zero bytes.
        let v = chunk_of(&mut x, "0\r\n\r\n", 1 << 20);
        assert_eq!(variant_of(&v), "ChunkedDone");
        assert_eq!(payload_bytes(&v, 0), b"");
        assert_eq!(payload_int(&v, 2), 5);
    }

    #[test]
    fn every_proper_prefix_needs_more() {
        let mut x = fix();
        let full = "5\r\nhello\r\n6\r\n world\r\n0\r\nX-Sum: abc\r\n\r\n";
        for cut in 0..full.len() {
            let v = chunk_of(&mut x, &full[..cut], 1 << 20);
            assert_eq!(
                variant_of(&v),
                "ChunkedNeedMore",
                "prefix of {cut} bytes must be NeedMore"
            );
        }
        let v = chunk_of(&mut x, full, 1 << 20);
        assert_eq!(variant_of(&v), "ChunkedDone");
        assert_eq!(payload_bytes(&v, 0), b"hello world");
    }

    #[test]
    fn buffer_boundary_split_then_done() {
        let mut x = fix();
        // The decoder is stateless, so the completed buffer just works.
        let part = "5\r\nhel";
        assert_eq!(
            variant_of(&chunk_of(&mut x, part, 1 << 20)),
            "ChunkedNeedMore"
        );
        let full = "5\r\nhello\r\n0\r\n\r\n";
        let v = chunk_of(&mut x, full, 1 << 20);
        assert_eq!(variant_of(&v), "ChunkedDone");
        assert_eq!(payload_bytes(&v, 0), b"hello");
    }

    #[test]
    fn trailers_round_trip() {
        let mut x = fix();
        let v = chunk_of(
            &mut x,
            "5\r\nhello\r\n0\r\nX-Checksum: abc123\r\nX-Count: 2\r\n\r\n",
            1 << 20,
        );
        assert_eq!(variant_of(&v), "ChunkedDone");
        let trailers = v.as_enum().unwrap().payload()[1].clone();
        let arr = trailers.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        let h = arr.get(0).unwrap();
        let (name, value) = header_fields(&h).unwrap();
        assert_eq!(field_bytes(name).unwrap().as_ref(), b"X-Checksum");
        assert_eq!(field_bytes(value).unwrap().as_ref(), b"abc123");
    }

    #[test]
    fn chunk_extensions_are_ignored() {
        let mut x = fix();
        let v = chunk_of(&mut x, "5;foo=bar;baz\r\nhello\r\n0;done\r\n\r\n", 1 << 20);
        assert_eq!(variant_of(&v), "ChunkedDone");
        assert_eq!(payload_bytes(&v, 0), b"hello");
    }

    #[test]
    fn rejects_malformed_chunks() {
        let mut x = fix();
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
            let v = chunk_of(&mut x, input, 1 << 20);
            assert_eq!(variant_of(&v), "ChunkedBad", "for {input:?}");
            assert_eq!(payload_int(&v, 0), *status, "for {input:?}");
        }
    }

    #[test]
    fn rejects_oversize_decoded_body() {
        let mut x = fix();
        // Rejected from the size line alone, before any data is buffered.
        let v = chunk_of(&mut x, "FFFFFFFF\r\n", 1024);
        assert_eq!(variant_of(&v), "ChunkedBad");
        assert_eq!(payload_int(&v, 0), 413);

        // Cumulative chunks crossing max are also rejected.
        let v = chunk_of(&mut x, "400\r\n", 1024); // 0x400 = 1024 fits...
        assert_eq!(variant_of(&v), "ChunkedNeedMore");
        let body_1024 = "a".repeat(1024);
        let two_chunks = format!("400\r\n{body_1024}\r\n400\r\n{body_1024}\r\n0\r\n\r\n");
        let v = chunk_of(&mut x, &two_chunks, 1024);
        assert_eq!(variant_of(&v), "ChunkedBad");
        assert_eq!(payload_int(&v, 0), 413);

        // A 16-hex-digit size cannot wrap the cap check.
        let v = chunk_of(&mut x, "FFFFFFFFFFFFFFF\r\n", 1024);
        assert_eq!(variant_of(&v), "ChunkedBad");
        assert_eq!(payload_int(&v, 0), 413);
    }

    #[test]
    fn rejects_unbounded_size_line_and_trailers() {
        let mut x = fix();
        // A "size line" that grows past the cap without a CRLF is hostile.
        let long_line = "F".repeat(5000);
        let v = chunk_of(&mut x, &long_line, 1 << 20);
        assert_eq!(variant_of(&v), "ChunkedBad");
        assert_eq!(payload_int(&v, 0), 400);

        // A trailer block growing past MAX_HEAD without its blank line → 431.
        let mut s = String::from("5\r\nhello\r\n0\r\n");
        s.push_str("X-Long: ");
        s.push_str(&"a".repeat(MAX_TRAILER_BLOCK + 16));
        let v = chunk_of(&mut x, &s, 1 << 20);
        assert_eq!(variant_of(&v), "ChunkedBad");
        assert_eq!(payload_int(&v, 0), 431);
    }

    #[test]
    fn consumed_is_the_pipelining_offset() {
        let mut x = fix();
        let body = "5\r\nhello\r\n0\r\n\r\n";
        let s = format!("{body}GET /next HTTP/1.1\r\n\r\n");
        let v = chunk_of(&mut x, &s, 1 << 20);
        assert_eq!(variant_of(&v), "ChunkedDone");
        let consumed = payload_int(&v, 2) as usize;
        assert_eq!(consumed, body.len());
        assert!(s[consumed..].starts_with("GET /next"));

        // And decoding can start at a non-zero offset (the head's consumed).
        let with_head = format!("POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n{body}");
        let head_len = with_head.len() - body.len();
        let v = x.chunk(&with_head, head_len as i64, 1 << 20);
        assert_eq!(variant_of(&v), "ChunkedDone");
        assert_eq!(payload_bytes(&v, 0), b"hello");
        assert_eq!(payload_int(&v, 2) as usize, with_head.len());
    }

    #[test]
    fn serializes_response_head() {
        let mut x = fix();
        let parsed = x.parse(
            "GET / HTTP/1.1\r\nContent-Type: text/plain\r\nContent-Length: 2\r\n\r\n",
            0,
        );
        let headers = parsed.as_enum().unwrap().payload()[3].clone();
        let reason = x.bin("OK");
        let head = serialize_head(&mut x.h, 200, &reason.as_binary().unwrap(), &headers).unwrap();
        let bytes = head
            .as_binary()
            .expect("expected binary")
            .full_bytes()
            .into_owned();
        assert_eq!(
            bytes,
            b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\n\r\n"
        );
    }
}
