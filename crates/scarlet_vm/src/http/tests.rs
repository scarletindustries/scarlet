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
