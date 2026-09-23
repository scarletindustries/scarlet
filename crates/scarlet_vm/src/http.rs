//! HTTP/1.1 heads, body framing and chunked bodies, behind `scarlet/http/h1`.
//!
//! Reading a head is `httparse`'s, the parser `hyper` is built on: the
//! request or status line and the header fields, found by SIMD and refused on
//! a byte it does not allow. What the stdlib promises beyond that parse is
//! here, around it:
//!
//! - lines end in CRLF only. `httparse` also takes a bare LF, which a proxy in
//!   front may not, and a disagreement about where a line ends is how requests
//!   are smuggled;
//! - at most four empty lines before a request (RFC 9112 section 2.2 allows a
//!   few), and none before a response, where one means the connection has lost
//!   its place. `httparse` skips any number, of either;
//! - a head is at most 64 KiB, and which status each refusal is: 414 for a
//!   request line that never ends, 431 for a head too large, 505 for a version
//!   other than 1.0 or 1.1, 400 for everything else;
//! - the `Connection` and `Expect` tokens a head carried;
//! - a body's framing, from `Content-Length` and `Transfer-Encoding`, with the
//!   conflicts RFC 9112 section 6.3 says to refuse;
//! - a chunked body's chunks, each size line read by `httparse`.
//!
//! Everything here works on bytes and answers in byte ranges of them, so the
//! views the stdlib hands back (`exec.rs`) are slices of the buffer the program
//! read, never copies.

use std::ops::Range;

/// The most bytes a head may take, from its first line to its blank line.
pub(crate) const MAX_HEAD: usize = 65536;

/// The most empty lines let through before a request line.
const MAX_LEADING_EMPTY: usize = 4;

/// The longest chunk-size line, size and extensions, before its CRLF.
const MAX_CHUNK_SIZE_LINE: usize = 4096;

/// The statuses a request head, framing or chunked body is refused with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reject {
    BadRequest = 400,
    PayloadTooLarge = 413,
    UriTooLong = 414,
    HeaderFieldsTooLarge = 431,
    NotImplemented = 501,
    VersionNotSupported = 505,
}

/// Which of `close` and `keep-alive` the `Connection` fields named. A peer can
/// send both, and that is recorded rather than resolved: which wins is
/// `h1.should_close`'s decision.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnTokens {
    #[default]
    Neither,
    Close,
    KeepAlive,
    Both,
}

impl ConnTokens {
    fn of(close: bool, keep_alive: bool) -> ConnTokens {
        match (close, keep_alive) {
            (false, false) => ConnTokens::Neither,
            (true, false) => ConnTokens::Close,
            (false, true) => ConnTokens::KeepAlive,
            (true, true) => ConnTokens::Both,
        }
    }

    /// A repeated `Connection` field is the same list over more lines, so the
    /// tokens add up: a later `close` does not unsee an earlier `keep-alive`.
    fn and(self, other: ConnTokens) -> ConnTokens {
        let close = |c| matches!(c, ConnTokens::Close | ConnTokens::Both);
        let keep = |c| matches!(c, ConnTokens::KeepAlive | ConnTokens::Both);
        ConnTokens::of(close(self) || close(other), keep(self) || keep(other))
    }
}

/// The `Connection` and `Expect` tokens a head carried: what was found, not
/// what to do about it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HeadFlags {
    pub(crate) conn: ConnTokens,
    pub(crate) expect_100_continue: bool,
}

/// One header field: where its name and its value are in the buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Field {
    pub(crate) name: Range<usize>,
    pub(crate) value: Range<usize>,
}

/// A header block, read: its fields, the tokens among them, and the byte
/// just past its blank line.
#[derive(Debug)]
pub(crate) struct Block {
    pub(crate) fields: Vec<Field>,
    pub(crate) flags: HeadFlags,
    pub(crate) end: usize,
}

#[derive(Debug)]
pub(crate) enum Request {
    Done {
        method: Range<usize>,
        target: Range<usize>,
        http11: bool,
        head: Block,
    },
    NeedMore,
    Bad(Reject),
}

/// Why a response head was refused: not a status, since a client has nobody
/// to answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BadResponse {
    StatusLine,
    Version,
    Field,
    TooLarge,
}

#[derive(Debug)]
pub(crate) enum Response {
    Done {
        http11: bool,
        code: u16,
        reason: Range<usize>,
        head: Block,
    },
    NeedMore,
    Bad(BadResponse),
}

/// Read a request head from the start of `bytes`. Ranges in the answer are
/// of `bytes`.
pub(crate) fn request(bytes: &[u8]) -> Request {
    // Empty lines before the request line, CRLF only and at most a few, so a
    // stream of them cannot hold a connection open.
    let mut start = 0;
    let mut empty = 0;
    while bytes.get(start..start + 2) == Some(b"\r\n") {
        empty += 1;
        if empty > MAX_LEADING_EMPTY {
            return Request::Bad(Reject::BadRequest);
        }
        start += 2;
    }
    match bytes.get(start..) {
        None | Some([] | [b'\r']) => return Request::NeedMore,
        Some([b'\r' | b'\n', ..]) => return Request::Bad(Reject::BadRequest),
        Some(_) => {}
    }
    let window = capped(bytes, start);
    let mut slots = header_slots(window);
    let mut req = httparse::Request::new(&mut slots);
    match req.parse(window) {
        Ok(httparse::Status::Complete(n)) => {
            if has_bare_lf(&window[..n]) {
                return Request::Bad(Reject::BadRequest);
            }
            let (Some(method), Some(target), Some(version)) = (req.method, req.path, req.version)
            else {
                return Request::Bad(Reject::BadRequest);
            };
            Request::Done {
                method: range_in(bytes, method.as_bytes()),
                target: range_in(bytes, target.as_bytes()),
                http11: version == 1,
                head: block(bytes, req.headers, start + n),
            }
        }
        // A line that never ends, past the cap, is a target too long; one that
        // ended followed by fields that never do is a head too large.
        Ok(httparse::Status::Partial) if window.len() > MAX_HEAD => {
            let request_line_ended = memchr::memmem::find(window, b"\r\n").is_some();
            Request::Bad(if request_line_ended {
                Reject::HeaderFieldsTooLarge
            } else {
                Reject::UriTooLong
            })
        }
        Ok(httparse::Status::Partial) => Request::NeedMore,
        Err(httparse::Error::Version) => Request::Bad(Reject::VersionNotSupported),
        Err(_) => Request::Bad(Reject::BadRequest),
    }
}

/// Read a response head from the start of `bytes`.
pub(crate) fn response(bytes: &[u8]) -> Response {
    // The status line first, by its own rules, so each way it can be wrong has
    // its own answer, which `httparse`'s errors do not tell apart.
    let Some(line_end) = memchr::memmem::find(bytes, b"\r\n") else {
        return if bytes.len() > MAX_HEAD {
            Response::Bad(BadResponse::TooLarge)
        } else {
            Response::NeedMore
        };
    };
    if let Err(why) = status_line(&bytes[..line_end]) {
        return Response::Bad(why);
    }
    let window = capped(bytes, 0);
    let mut slots = header_slots(window);
    let mut resp = httparse::Response::new(&mut slots);
    match resp.parse(window) {
        Ok(httparse::Status::Complete(n)) => {
            if has_bare_lf(&window[..n]) {
                return Response::Bad(BadResponse::Field);
            }
            let (Some(version), Some(code)) = (resp.version, resp.code) else {
                return Response::Bad(BadResponse::StatusLine);
            };
            let reason = match resp.reason {
                Some(r) if !r.is_empty() => range_in(bytes, r.as_bytes()),
                _ => 0..0,
            };
            Response::Done {
                http11: version == 1,
                code,
                reason,
                head: block(bytes, resp.headers, n),
            }
        }
        Ok(httparse::Status::Partial) if window.len() > MAX_HEAD => {
            Response::Bad(BadResponse::TooLarge)
        }
        Ok(httparse::Status::Partial) => Response::NeedMore,
        // The line passed `status_line`, so what `httparse` still refuses in it
        // is a byte a reason may not hold.
        Err(httparse::Error::Version | httparse::Error::Status | httparse::Error::Token) => {
            Response::Bad(BadResponse::StatusLine)
        }
        Err(
            httparse::Error::HeaderName
            | httparse::Error::HeaderValue
            | httparse::Error::NewLine
            | httparse::Error::TooManyHeaders,
        ) => Response::Bad(BadResponse::Field),
    }
}

/// Check a status line, its CRLF taken off: `HTTP/1.x`, a space, three
/// digits, then nothing or a space and a reason with no CR or LF in it. The
/// space and reason may be left out: RFC 9112 section 4 wants the space, but
/// enough servers leave it out that refusing a reply over a byte a client must
/// ignore would be the wrong call.
///
/// A line with no room for a code, or that is not `HTTP/` at all (a server
/// speaking another protocol, or a TLS record answering plain text), is not a
/// status line; `HTTP/` and a version not spoken is a version.
fn status_line(line: &[u8]) -> Result<(), BadResponse> {
    let (Some(version), Some(b' '), Some(code)) = (line.get(..8), line.get(8), line.get(9..12))
    else {
        return Err(BadResponse::StatusLine);
    };
    match version {
        b"HTTP/1.1" | b"HTTP/1.0" => {}
        v if v.starts_with(b"HTTP/") => return Err(BadResponse::Version),
        _ => return Err(BadResponse::StatusLine),
    }
    if !code.iter().all(u8::is_ascii_digit) {
        return Err(BadResponse::StatusLine);
    }
    match line.get(12..) {
        None | Some([]) => Ok(()),
        Some([b' ', reason @ ..]) if !reason.iter().any(|&b| b == b'\r' || b == b'\n') => Ok(()),
        Some(_) => Err(BadResponse::StatusLine),
    }
}

/// `bytes` from `start`, up to a whole head and its blank line past the cap:
/// what the parser is shown, so its work is bounded however much is
/// buffered.
fn capped(bytes: &[u8], start: usize) -> &[u8] {
    let end = bytes.len().min(start + MAX_HEAD + 3);
    bytes.get(start..end).unwrap_or(&[])
}

/// Room for as many fields as `window` has lines, so a head within the cap is
/// never refused for having many fields.
fn header_slots(window: &[u8]) -> Vec<httparse::Header<'_>> {
    let lines = memchr::memchr_iter(b'\n', window).count();
    vec![httparse::EMPTY_HEADER; lines + 1]
}

/// Whether a line in `bytes` ends in a bare LF rather than CRLF.
fn has_bare_lf(bytes: &[u8]) -> bool {
    memchr::memchr_iter(b'\n', bytes).any(|i| i == 0 || bytes.get(i - 1) != Some(&b'\r'))
}

/// Where the slice `part` of `whole` is in it.
fn range_in(whole: &[u8], part: &[u8]) -> Range<usize> {
    let start = (part.as_ptr() as usize).saturating_sub(whole.as_ptr() as usize);
    start..start + part.len()
}

/// The fields `httparse` read, as ranges of `bytes`, and their tokens.
fn block(bytes: &[u8], headers: &[httparse::Header<'_>], end: usize) -> Block {
    let mut flags = HeadFlags::default();
    let mut fields = Vec::with_capacity(headers.len());
    for h in headers {
        if h.name.eq_ignore_ascii_case("connection") {
            let found = ConnTokens::of(
                has_token(h.value, b"close"),
                has_token(h.value, b"keep-alive"),
            );
            flags.conn = flags.conn.and(found);
        } else if h.name.eq_ignore_ascii_case("expect") {
            flags.expect_100_continue |= has_token(h.value, b"100-continue");
        }
        fields.push(Field {
            name: range_in(bytes, h.name.as_bytes()),
            value: range_in(bytes, trim_ows(h.value)),
        });
    }
    Block { fields, flags, end }
}

/// Whether the list `value` holds `token`: its comma-separated elements, OWS
/// trimmed and compared ignoring ASCII case, as `headers.has_token` has it.
fn has_token(value: &[u8], token: &[u8]) -> bool {
    value.split(|&b| b == b',').any(|el| {
        let el = trim_ows(el);
        !el.is_empty() && el.eq_ignore_ascii_case(token)
    })
}

fn trim_ows(mut bytes: &[u8]) -> &[u8] {
    while let [b' ' | b'\t', rest @ ..] = bytes {
        bytes = rest;
    }
    while let [rest @ .., b' ' | b'\t'] = bytes {
        bytes = rest;
    }
    bytes
}

/// How a body is framed, from a message's `Transfer-Encoding` and
/// `Content-Length` values.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Framing {
    NoBody,
    Length(u64),
    Chunked,
    Invalid(Reject),
}

/// RFC 9112 section 6.3: a `Transfer-Encoding` beside a `Content-Length` is
/// refused, as the conflict smuggling is built from. `chunked` alone is
/// chunked, and any other coding is refused as not built rather than ignored.
/// One `Content-Length`, a run of digits with no redundant leading zero and
/// at most 2^63 - 1; two, or anything else, is refused.
pub(crate) fn framing(transfer_encodings: &[&[u8]], content_lengths: &[&[u8]]) -> Framing {
    if !transfer_encodings.is_empty() {
        return match (transfer_encodings, content_lengths) {
            (_, [_, ..]) => Framing::Invalid(Reject::BadRequest),
            ([te], []) if te.eq_ignore_ascii_case(b"chunked") => Framing::Chunked,
            _ => Framing::Invalid(Reject::NotImplemented),
        };
    }
    match content_lengths {
        [] => Framing::NoBody,
        [cl] => match length(cl) {
            Some(n) => Framing::Length(n),
            None => Framing::Invalid(Reject::BadRequest),
        },
        _ => Framing::Invalid(Reject::BadRequest),
    }
}

fn length(digits: &[u8]) -> Option<u64> {
    let leading_zero = digits.len() > 1 && digits.first() == Some(&b'0');
    if digits.is_empty() || leading_zero || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(digits)
        .ok()?
        .parse::<u64>()
        .ok()
        .filter(|&n| i64::try_from(n).is_ok())
}

#[derive(Debug)]
pub(crate) enum Chunked {
    /// The body's pieces, as ranges of the buffer, the trailer block after
    /// it, and where the next message starts.
    Done {
        pieces: Vec<Range<usize>>,
        trailers: Block,
    },
    NeedMore,
    Bad(Reject),
}

/// Read a chunked body (RFC 9112 section 7.1) from the start of `bytes`,
/// refusing more than `max` bytes of body (413). Each chunk is its size in
/// hex, any extensions, CRLF, the data and CRLF; a size of 0 ends it, then a
/// trailer block of fields and a blank line.
///
/// The buffer holds all the state, so a call that ends in `NeedMore` is made
/// again, over more bytes, from the same start. The pieces are only recorded
/// here, and copied once, by the caller, when the whole body has arrived.
pub(crate) fn chunked(bytes: &[u8], max: u64) -> Chunked {
    let mut pos = 0usize;
    let mut total = 0u64;
    let mut pieces = Vec::new();
    loop {
        let rest = bytes.get(pos..).unwrap_or(&[]);
        let line_end = memchr::memmem::find(rest, b"\r\n");
        if line_end.is_none_or(|e| e > MAX_CHUNK_SIZE_LINE) {
            // A size line already past the cap can never become valid.
            return if line_end.is_some() || rest.len() > MAX_CHUNK_SIZE_LINE {
                Chunked::Bad(Reject::BadRequest)
            } else {
                Chunked::NeedMore
            };
        }
        let (data_start, size) = match httparse::parse_chunk_size(rest) {
            Ok(httparse::Status::Complete((n, size))) if !has_bare_lf(&rest[..n]) => {
                (pos + n, size)
            }
            Ok(httparse::Status::Partial) => return Chunked::NeedMore,
            Ok(httparse::Status::Complete(_)) | Err(_) => return Chunked::Bad(Reject::BadRequest),
        };
        if size == 0 {
            return trailers(bytes, data_start, pieces);
        }
        // Capped before the data is waited for, so a size no body could fill
        // is refused at once.
        let Some(sum) = total.checked_add(size).filter(|&s| s <= max) else {
            return Chunked::Bad(Reject::PayloadTooLarge);
        };
        let Some(data_end) = usize::try_from(size)
            .ok()
            .and_then(|s| data_start.checked_add(s))
        else {
            return Chunked::Bad(Reject::PayloadTooLarge);
        };
        match bytes.get(data_end..data_end + 2) {
            None => return Chunked::NeedMore,
            Some(b"\r\n") => {}
            // The size lied about where the data ends: never read on.
            Some(_) => return Chunked::Bad(Reject::BadRequest),
        }
        pieces.push(data_start..data_end);
        total = sum;
        pos = data_end + 2;
    }
}

/// The trailer block after a chunked body's last chunk, at `start`.
fn trailers(bytes: &[u8], start: usize, pieces: Vec<Range<usize>>) -> Chunked {
    let window = capped(bytes, start);
    let mut slots = header_slots(window);
    match httparse::parse_headers(window, &mut slots) {
        Ok(httparse::Status::Complete((n, headers))) if !has_bare_lf(&window[..n]) => {
            let mut trailers = block(bytes, headers, start + n);
            // A trailer carries no connection or expectation meaning
            // (RFC 9110 section 6.5.1).
            trailers.flags = HeadFlags::default();
            Chunked::Done { pieces, trailers }
        }
        Ok(httparse::Status::Partial) if window.len() > MAX_HEAD => {
            Chunked::Bad(Reject::HeaderFieldsTooLarge)
        }
        Ok(httparse::Status::Partial) => Chunked::NeedMore,
        Ok(httparse::Status::Complete(_)) | Err(_) => Chunked::Bad(Reject::BadRequest),
    }
}

/// Whether `name` is an RFC 9110 token (section 5.6.2): what a field name may
/// be.
pub(crate) fn is_token(name: &[u8]) -> bool {
    !name.is_empty()
        && name.iter().all(|&b| {
            b.is_ascii_alphanumeric()
                || matches!(b, b'!' | b'#'..=b'\'' | b'*' | b'+' | b'-' | b'.' | b'^'..=b'`' | b'|' | b'~')
        })
}

/// Whether a field value is safe to write: no CR, LF or NUL, any of which
/// could end the line early and split the message.
pub(crate) fn is_safe_value(value: &[u8]) -> bool {
    !value.iter().any(|&b| matches!(b, b'\r' | b'\n' | 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text<'a>(bytes: &'a [u8], r: &Range<usize>) -> &'a str {
        std::str::from_utf8(&bytes[r.clone()]).expect("UTF-8")
    }

    #[test]
    fn a_request_head_reads_to_ranges_of_the_buffer() {
        let buf = b"\r\nPOST /up?x=1 HTTP/1.1\r\nHost: a\r\nX-Pad:  v v \r\n\r\nbody";
        let Request::Done {
            method,
            target,
            http11,
            head,
        } = request(buf)
        else {
            panic!("{:?}", request(buf));
        };
        assert_eq!(
            (text(buf, &method), text(buf, &target), http11),
            ("POST", "/up?x=1", true)
        );
        let fields: Vec<_> = head
            .fields
            .iter()
            .map(|f| (text(buf, &f.name), text(buf, &f.value)))
            .collect();
        assert_eq!(fields, [("Host", "a"), ("X-Pad", "v v")]);
        assert_eq!(&buf[head.end..], b"body");
    }

    #[test]
    fn every_proper_prefix_of_a_head_needs_more() {
        let buf = b"GET / HTTP/1.0\r\nA: b\r\n\r\n";
        for n in 0..buf.len() {
            assert!(matches!(request(&buf[..n]), Request::NeedMore), "{n}");
        }
        assert!(matches!(request(buf), Request::Done { http11: false, .. }));
    }

    #[test]
    fn what_a_request_head_is_refused_with() {
        let bad = |b: &[u8]| match request(b) {
            Request::Bad(r) => Some(r),
            _ => None,
        };
        assert_eq!(
            bad(b"GET / HTTP/2.0\r\n\r\n"),
            Some(Reject::VersionNotSupported)
        );
        assert_eq!(
            bad(b"GET / HTTP/1.1\r\nA : b\r\n\r\n"),
            Some(Reject::BadRequest)
        );
        assert_eq!(
            bad(b"GET / HTTP/1.1\r\nA: b\r\n c\r\n\r\n"),
            Some(Reject::BadRequest)
        );
        assert_eq!(
            bad(b"GET / HTTP/1.1\nA: b\r\n\r\n"),
            Some(Reject::BadRequest)
        );
        assert_eq!(
            bad(b"GET / HTTP/1.1\r\nA: b\n\r\n"),
            Some(Reject::BadRequest)
        );
        assert_eq!(bad(b"GET  / HTTP/1.1\r\n\r\n"), Some(Reject::BadRequest));
        assert_eq!(
            bad(b"\r\n\r\n\r\n\r\n\r\nGET / HTTP/1.1\r\n\r\n"),
            Some(Reject::BadRequest)
        );
        assert_eq!(bad(b"\nGET / HTTP/1.1\r\n\r\n"), Some(Reject::BadRequest));
        assert_eq!(bad(&[b'a'; MAX_HEAD + 10]), Some(Reject::UriTooLong));
        let mut big = b"GET / HTTP/1.1\r\n".to_vec();
        big.extend(std::iter::repeat_n(b"A: b\r\n".as_slice(), MAX_HEAD / 6 + 2).flatten());
        assert_eq!(bad(&big), Some(Reject::HeaderFieldsTooLarge));
    }

    #[test]
    fn connection_and_expect_tokens_add_up_over_fields() {
        let flags = |b: &[u8]| match request(b) {
            Request::Done { head, .. } => head.flags,
            other => panic!("{other:?}"),
        };
        let f = flags(b"GET / HTTP/1.1\r\nConnection: Keep-Alive, Upgrade\r\nconnection: close\r\nExpect: 100-continue\r\n\r\n");
        assert_eq!(
            f,
            HeadFlags {
                conn: ConnTokens::Both,
                expect_100_continue: true
            }
        );
        assert_eq!(
            flags(b"GET / HTTP/1.1\r\nConnection: closed\r\n\r\n"),
            HeadFlags::default()
        );
    }

    #[test]
    fn a_response_head_reads_and_refuses() {
        let buf = b"HTTP/1.1 404 Not Found\r\nA: b\r\n\r\n";
        let Response::Done {
            code,
            reason,
            http11,
            head,
        } = response(buf)
        else {
            panic!();
        };
        assert_eq!(
            (code, text(buf, &reason), http11, head.fields.len()),
            (404, "Not Found", true, 1)
        );
        assert!(matches!(
            response(b"HTTP/1.0 204\r\n\r\n"),
            Response::Done { code: 204, .. }
        ));
        let bad = |b: &[u8]| match response(b) {
            Response::Bad(r) => Some(r),
            _ => None,
        };
        assert_eq!(
            bad(b"\r\nHTTP/1.1 200 OK\r\n\r\n"),
            Some(BadResponse::StatusLine)
        );
        assert_eq!(bad(b"HTTP/2.0 200 OK\r\n\r\n"), Some(BadResponse::Version));
        assert_eq!(
            bad(b"SSH-2.0 200 OK\r\n\r\n"),
            Some(BadResponse::StatusLine)
        );
        assert_eq!(
            bad(b"HTTP/1.1 2x0 OK\r\n\r\n"),
            Some(BadResponse::StatusLine)
        );
        assert_eq!(
            bad(b"HTTP/1.1 200 OK\r\nA b\r\n\r\n"),
            Some(BadResponse::Field)
        );
        assert_eq!(
            bad(b"HTTP/1.1 200 OK\r\nA: b\n\r\n"),
            Some(BadResponse::Field)
        );
        assert_eq!(bad(b"HTTP/1.1\r\n\r\n"), Some(BadResponse::StatusLine));
        assert_eq!(
            bad(b"HTTP/1.1_200 OK\r\n\r\n"),
            Some(BadResponse::StatusLine)
        );
        assert_eq!(
            bad(b"HTTP/1.1 200 O\nK\r\n\r\n"),
            Some(BadResponse::StatusLine)
        );
        assert!(matches!(response(b"HTTP/1.1 20"), Response::NeedMore));
    }

    #[test]
    fn framing_follows_rfc_9112() {
        assert_eq!(framing(&[], &[]), Framing::NoBody);
        assert_eq!(framing(&[], &[b"12"]), Framing::Length(12));
        assert_eq!(framing(&[b"Chunked"], &[]), Framing::Chunked);
        assert_eq!(
            framing(&[b"chunked"], &[b"3"]),
            Framing::Invalid(Reject::BadRequest)
        );
        assert_eq!(
            framing(&[b"gzip"], &[]),
            Framing::Invalid(Reject::NotImplemented)
        );
        assert_eq!(
            framing(&[b"chunked", b"chunked"], &[]),
            Framing::Invalid(Reject::NotImplemented)
        );
        for cl in [
            &b"007"[..],
            b"",
            b"1a",
            b"-1",
            b"99999999999999999999",
            b"9223372036854775808",
        ] {
            assert_eq!(
                framing(&[], &[cl]),
                Framing::Invalid(Reject::BadRequest),
                "{cl:?}"
            );
        }
        assert_eq!(framing(&[], &[b"0"]), Framing::Length(0));
        assert_eq!(
            framing(&[], &[b"1", b"1"]),
            Framing::Invalid(Reject::BadRequest)
        );
    }

    #[test]
    fn a_chunked_body_reads_its_pieces_and_trailers() {
        let buf = b"4\r\nWiki\r\n5;ext=1\r\npedia\r\n0\r\nT: v\r\n\r\nNEXT";
        let Chunked::Done { pieces, trailers } = chunked(buf, 100) else {
            panic!("{:?}", chunked(buf, 100));
        };
        let body: Vec<u8> = pieces
            .iter()
            .flat_map(|r| buf[r.clone()].to_vec())
            .collect();
        assert_eq!(body, b"Wikipedia");
        assert_eq!(trailers.fields.len(), 1);
        assert_eq!(&buf[trailers.end..], b"NEXT");
        for n in 0..buf.len() - 4 {
            assert!(matches!(chunked(&buf[..n], 100), Chunked::NeedMore), "{n}");
        }
        assert!(matches!(
            chunked(buf, 8),
            Chunked::Bad(Reject::PayloadTooLarge)
        ));
        assert!(matches!(
            chunked(b"4\r\nWikiXX\r\n", 100),
            Chunked::Bad(Reject::BadRequest)
        ));
        assert!(matches!(
            chunked(b"zz\r\n", 100),
            Chunked::Bad(Reject::BadRequest)
        ));
        assert!(matches!(
            chunked(b"4\nWiki\r\n", 100),
            Chunked::Bad(Reject::BadRequest)
        ));
    }

    #[test]
    fn tokens_and_safe_values() {
        assert!(is_token(b"Content-Type") && is_token(b"x!#$%&'*+-.^_`|~9"));
        assert!(!is_token(b"") && !is_token(b"a b") && !is_token(b"a:") && !is_token(b"\xc3\xa9"));
        assert!(is_safe_value(b"text/html; q=0.9\t\x80"));
        assert!(!is_safe_value(b"a\r\nb") && !is_safe_value(b"a\0"));
    }
}
