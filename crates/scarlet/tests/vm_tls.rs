//! TLS, end to end: a Scarlet program against a real TLS server on loopback.
//!
//! **The negative tests are the point of this file.** A TLS client that accepts
//! any certificate is worse than no TLS client, because it still looks like it
//! works: every test here would pass, the bytes would flow, and the connection
//! would be readable by anyone on the path. So three separate ways of being the
//! wrong peer are checked — an issuer we do not trust, a certificate outside
//! its validity window, and a certificate for a different name — and each must
//! come back as its own typed `TlsError`, not merely as "an error".
//!
//! Every certificate is minted here, per test, by `rcgen`. The alternative is
//! aiming the tests at a public host that is misconfigured on purpose, which
//! makes a security test depend on a third party's uptime and turns a network
//! outage into a green run.
//!
//! Trust is granted the way the documentation tells a user to grant it: through
//! the machine's certificate store, via `SSL_CERT_FILE`, which is what
//! `rustls-native-certs` reads on Unix. There is deliberately no way to ask the
//! runtime to skip verification, so there is nothing here that switches it off
//! — the positive test earns its trust by installing a root, exactly as a
//! program talking to an internal CA would.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;

use rcgen::{
    BasicConstraints, CertificateParams, DnType, IsCa, KeyPair, KeyUsagePurpose, date_time_ymd,
};
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

mod common;
use common::net::{CONNECT_DEADLINE_MS, KERNEL_LOOPBACK_FLOOR_MS, full_accept_queue};
use common::{CHILD_TIMEOUT_SECS, wait_or_kill};

// ---------------------------------------------------------------------------
// Certificate authority and leaves
// ---------------------------------------------------------------------------

/// A throwaway CA, plus the PEM a client trusts it by.
struct Ca {
    params: rcgen::Certificate,
    key: KeyPair,
    pem: String,
}

fn make_ca() -> Ca {
    let mut params = CertificateParams::new(Vec::new()).expect("ca params");
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    params
        .distinguished_name
        .push(DnType::CommonName, "scarlet test ca");
    let key = KeyPair::generate().expect("ca key");
    let cert = params.self_signed(&key).expect("self-sign ca");
    let pem = cert.pem();
    Ca {
        params: cert,
        key,
        pem,
    }
}

/// A leaf for `name`, signed by `ca`. `validity` picks the window, which is how
/// the expired case is built.
enum Validity {
    Current,
    Expired,
}

struct Leaf {
    chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
}

fn make_leaf(ca: &Ca, name: &str, validity: Validity) -> Leaf {
    let mut params = CertificateParams::new(vec![name.to_string()]).expect("leaf params");
    params
        .distinguished_name
        .push(DnType::CommonName, name.to_string());
    if let Validity::Expired = validity {
        // Comfortably in the past, so no clock skew on a test machine can drag
        // it back into validity.
        params.not_before = date_time_ymd(2015, 1, 1);
        params.not_after = date_time_ymd(2016, 1, 1);
    }
    let key = KeyPair::generate().expect("leaf key");
    let cert = params
        .signed_by(&key, &ca.params, &ca.key)
        .expect("sign leaf");
    Leaf {
        chain: vec![cert.der().clone(), ca.params.der().clone()],
        key: PrivateKeyDer::try_from(key.serialize_der()).expect("leaf key der"),
    }
}

// ---------------------------------------------------------------------------
// A one-shot TLS server on loopback
// ---------------------------------------------------------------------------

/// Pick the crypto provider for THIS test process.
///
/// Two are linked in — `scarlet_vm` asks for `aws-lc-rs`, and `ureq` brings
/// `ring` — so rustls refuses to guess and panics on the first `builder()`.
/// The runtime has the same problem and solves it the same way, in
/// `vm/tls.rs`; that call is load-bearing rather than defensive, and this is
/// the evidence.
fn install_provider() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

/// Whether the stalled server looks for bytes arriving BEYOND `expect`.
///
/// The read loop below stops at `expect`, so a writer that sent more can never
/// show up as a larger count on its own — which is what made a duplicated
/// re-send unobservable from the client side. `Probe` takes the second look.
enum Excess {
    /// Reply as soon as `expect` bytes are in. For tests whose subject is the
    /// stall, not the byte count.
    Ignore,
    /// Wait [`EXCESS_WINDOW`] for anything after `expect` and fold it into the
    /// reported count, so a writer that duplicated reports MORE than it wrote.
    Probe,
}

/// How long [`Excess::Probe`] waits for a byte that should never come.
///
/// It is not a race in either direction: a correct writer sends exactly
/// `expect`, so this window only ever expires, and a duplicating one has the
/// extra bytes queued behind the ones just read, so they are already there.
/// The value therefore sets how slow a passing run is, not whether it passes.
const EXCESS_WINDOW: std::time::Duration = std::time::Duration::from_millis(500);

/// Serve one TLS connection that does NOT read for `stall`, then drains exactly
/// `expect` bytes of plaintext and answers `got:<n>:<ok|mismatch>`.
///
/// The listener carries a small `SO_RCVBUF`, inherited by the accepted socket,
/// so the writer's window stays small and its socket fills while the peer is
/// stalled. That is the state the VM's write path is only ever in here — a few
/// hundred bytes over loopback never fills anything.
fn spawn_stalled_tls_server(
    leaf: Leaf,
    expect: usize,
    stall: std::time::Duration,
    excess: Excess,
) -> u16 {
    install_provider();
    let config = Arc::new(
        ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(leaf.chain, leaf.key)
            .expect("server config"),
    );

    let sock = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::STREAM,
        Some(socket2::Protocol::TCP),
    )
    .expect("listener socket");
    let _ = sock.set_recv_buffer_size(4096);
    sock.bind(
        &"127.0.0.1:0"
            .parse::<std::net::SocketAddr>()
            .unwrap()
            .into(),
    )
    .expect("bind");
    sock.listen(8).expect("listen");
    let listener: TcpListener = sock.into();
    let port = listener.local_addr().expect("local_addr").port();

    thread::spawn(move || {
        let Ok((mut sock, _)) = listener.accept() else {
            return;
        };
        let Ok(mut conn) = rustls::ServerConnection::new(config) else {
            return;
        };
        if conn.complete_io(&mut sock).is_err() {
            return;
        }
        thread::sleep(stall);
        let mut all = Vec::with_capacity(expect);
        let mut buf = vec![0u8; 16 * 1024];
        {
            let mut stream = rustls::Stream::new(&mut conn, &mut sock);
            while all.len() < expect {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => all.extend_from_slice(&buf[..n]),
                }
            }
        }

        // Every byte past `expect` is one the writer sent twice. Read on a
        // timeout rather than to EOF: the client is blocked waiting for the
        // reply below and will not close until it has it, so EOF never comes.
        let mut extra = 0usize;
        if let Excess::Probe = excess {
            let _ = sock.set_read_timeout(Some(EXCESS_WINDOW));
            let mut stream = rustls::Stream::new(&mut conn, &mut sock);
            while let Ok(n) = stream.read(&mut buf) {
                if n == 0 {
                    break;
                }
                extra += n;
            }
            let _ = sock.set_read_timeout(None);
        }

        let ok = all.len() == expect
            && extra == 0
            && all.chunks(16).all(|c| c == &PATTERN.as_bytes()[..c.len()]);
        let verdict = if ok { "ok" } else { "mismatch" };
        let mut stream = rustls::Stream::new(&mut conn, &mut sock);
        let _ = stream.write_all(format!("got:{}:{verdict}", all.len() + extra).as_bytes());
        let _ = stream.flush();
    });

    port
}

/// The 16-byte unit the large-write payload is built from, by doubling.
const PATTERN: &str = "0123456789abcdef";

/// Accept `n` connections and hang up on each without speaking TLS, so a
/// client's `tls.handshake` fails the same way `n` times over. Enough
/// repetitions to warm the Scarlet function around it, which is the point.
fn spawn_hangup_server(n: usize) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    thread::spawn(move || {
        for _ in 0..n {
            match listener.accept() {
                Ok((sock, _)) => drop(sock),
                Err(_) => return,
            }
        }
    });
    port
}

/// Accept one connection and then say nothing at all, holding it open.
///
/// This is the peer an unbounded `tls.handshake` has no answer for: the TCP
/// connection completes, the ClientHello goes out, and no byte ever comes back.
/// A line protocol whose parser swallows the ClientHello looks exactly like
/// this from the client side.
///
/// The accepted stream is parked rather than dropped, and that is the whole rig:
/// dropping it closes the connection, which fails the handshake promptly and
/// tests something else entirely.
fn spawn_silent_peer() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    thread::spawn(move || {
        if let Ok((sock, _)) = listener.accept() {
            thread::park();
            drop(sock);
        }
    });
    port
}

/// Serve exactly one TLS connection, echoing back a fixed reply, and return the
/// port it bound. The thread is detached: a test that fails the handshake on
/// purpose leaves it waiting, and the process exiting collects it.
fn spawn_tls_server(leaf: Leaf, reply: &'static str) -> u16 {
    install_provider();
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(leaf.chain, leaf.key)
        .expect("server config");
    let config = Arc::new(config);

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("local_addr").port();

    thread::spawn(move || {
        let Ok((mut sock, _)) = listener.accept() else {
            return;
        };
        let Ok(mut conn) = rustls::ServerConnection::new(config) else {
            return;
        };
        // A failed handshake here is the expected outcome of every negative
        // test, so it is not reported: the assertion lives on the client side.
        if conn.complete_io(&mut sock).is_err() {
            return;
        }
        let mut buf = [0u8; 1024];
        let mut stream = rustls::Stream::new(&mut conn, &mut sock);
        let _ = stream.read(&mut buf);
        let _ = stream.write_all(reply.as_bytes());
        let _ = stream.flush();
    });

    port
}

// ---------------------------------------------------------------------------
// Running a Scarlet program
// ---------------------------------------------------------------------------

fn project_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "scarlet_tls_{tag}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir
}

/// Write `src` as a one-file program and run it, giving up after `secs`.
/// `trust` is the PEM installed as the machine's certificate store for the
/// child, or `None` to leave the child with the roots it would ordinarily
/// have. Returns the exit code and the combined streams.
///
/// Bounded rather than `Command::output()`, which waits forever: one test here
/// is about a program that used to never end, and a wedge must be one red test
/// rather than a hung suite. `wait_or_kill` reports a killed child as no exit
/// code at all, which is the discriminator that test asserts on — a program
/// that hangs and one that exits having printed nothing are otherwise the same
/// empty string.
fn run_bounded(tag: &str, src: &str, trust: Option<&str>, secs: u64) -> (Option<i32>, String) {
    let dir = project_dir(tag);
    let entry = dir.join("main.scrl");
    std::fs::write(&entry, src).expect("write program");
    std::fs::write(dir.join("package.scrl"), "name = 'tls_test'\n").expect("write package");

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_scarlet"));
    cmd.arg("run")
        .arg(&entry)
        .current_dir(&dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    match trust {
        Some(pem) => {
            let roots = dir.join("roots.pem");
            std::fs::write(&roots, pem).expect("write roots");
            cmd.env("SSL_CERT_FILE", &roots);
        }
        // An empty file is a certificate store with nothing in it, which is
        // what makes "this issuer is not trusted" the test's own doing rather
        // than a property of whatever CAs happen to be on the build machine.
        None => {
            let empty = dir.join("empty.pem");
            std::fs::write(&empty, "").expect("write empty roots");
            cmd.env("SSL_CERT_FILE", &empty);
        }
    }
    let out = wait_or_kill(cmd.spawn().expect("run scarlet"), secs);
    (
        out.status.code(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

/// [`run_bounded`] for the tests that only read the output.
fn run_program(tag: &str, src: &str, trust: Option<&str>) -> String {
    run_bounded(tag, src, trust, CHILD_TIMEOUT_SECS).1
}

/// A program that connects and reports the outcome.
///
/// The TCP connect goes to the literal `127.0.0.1` while the certificate is
/// verified against `localhost`. That split is not a workaround for the test —
/// it is the reason `handshake` takes the name as its own argument. Connecting
/// by address and verifying by name is what a connection pool does, and what a
/// client behind a proxy does. Using `tls.connect('localhost', …)` here would
/// instead resolve `localhost` to `::1` on this host, where nothing is bound,
/// and every assertion below would pass on `ConnectionRefused` without a
/// certificate ever being examined.
fn connect_program(name: &str, port: u16) -> String {
    format!(
        r#"import scarlet/net
import scarlet/net/tls
import scarlet/string

pub fn main() {{
    match net.connect('127.0.0.1', {port}) {{
        Ok(sock) -> match tls.handshake(sock, '{name}') {{
            Ok(conn) -> {{
                println('connected')
                tls.close(conn) or Nil
            }}
            Err(e) -> println('failed: ${{string.inspect(e)}}')
        }}
        Err(e) -> println('connect failed: ${{string.inspect(e)}}')
    }}
}}
"#
    )
}

// ---------------------------------------------------------------------------
// The negative tests
// ---------------------------------------------------------------------------

#[test]
#[ignore = "needs the VM"]
fn an_untrusted_issuer_is_rejected() {
    let ca = make_ca();
    let leaf = make_leaf(&ca, "localhost", Validity::Current);
    let port = spawn_tls_server(leaf, "HTTP/1.1 200 OK\r\n\r\n");

    // The CA is deliberately NOT installed.
    let out = run_program("untrusted", &connect_program("localhost", port), None);

    assert!(
        out.contains("CertificateUnknownIssuer"),
        "a certificate from an issuer we do not trust must be rejected as \
         CertificateUnknownIssuer, got:\n{out}"
    );
    assert!(
        !out.contains("connected"),
        "the handshake must not succeed against an untrusted issuer:\n{out}"
    );
}

#[test]
#[ignore = "needs the VM"]
fn an_expired_certificate_is_rejected() {
    let ca = make_ca();
    let leaf = make_leaf(&ca, "localhost", Validity::Expired);
    let port = spawn_tls_server(leaf, "HTTP/1.1 200 OK\r\n\r\n");

    // The issuer IS trusted, so the only thing wrong is the validity window.
    let out = run_program(
        "expired",
        &connect_program("localhost", port),
        Some(&ca.pem),
    );

    assert!(
        out.contains("CertificateExpired"),
        "a certificate outside its validity window must be rejected as \
         CertificateExpired, got:\n{out}"
    );
    assert!(
        !out.contains("connected"),
        "the handshake must not succeed with an expired certificate:\n{out}"
    );
}

#[test]
#[ignore = "needs the VM"]
fn a_hostname_mismatch_is_rejected() {
    let ca = make_ca();
    // Valid, current, and signed by a CA the client trusts — issued for the
    // wrong name. This is the case a connection pool gets wrong by reusing a
    // connection under the wrong key, and the only thing that catches it.
    let leaf = make_leaf(&ca, "wrong.example", Validity::Current);
    let port = spawn_tls_server(leaf, "HTTP/1.1 200 OK\r\n\r\n");

    let out = run_program(
        "mismatch",
        &connect_program("localhost", port),
        Some(&ca.pem),
    );

    assert!(
        out.contains("HostnameMismatch"),
        "a certificate issued for another name must be rejected as \
         HostnameMismatch, got:\n{out}"
    );
    assert!(
        !out.contains("connected"),
        "the handshake must not succeed against a certificate for another \
         name:\n{out}"
    );
}

// ---------------------------------------------------------------------------
// An address is a name to verify, not a name to refuse
// ---------------------------------------------------------------------------
//
// `ServerName::try_from` parses an IP literal into `ServerName::IpAddress`
// rather than rejecting it, so rustls checks it against the certificate's IP
// SANs like any other name. Three comments used to say the opposite — that an
// address has nothing to verify against and lands on `InvalidServerName` — and
// nothing in this file could tell the difference, which is why they survived.
// The pair below is what separates the two stories: same address, same trusted
// issuer, and only the certificate's SAN differs.

#[test]
#[ignore = "needs the VM"]
fn an_ip_literal_against_a_certificate_without_it_is_a_hostname_mismatch() {
    let ca = make_ca();
    // `DNS:localhost` and no IP SAN, so 127.0.0.1 is simply not on this
    // certificate — the ordinary wrong-name case, reached by an address.
    let leaf = make_leaf(&ca, "localhost", Validity::Current);
    let port = spawn_tls_server(leaf, "HTTP/1.1 200 OK\r\n\r\n");

    let out = run_program(
        "ip_no_san",
        &connect_program("127.0.0.1", port),
        Some(&ca.pem),
    );

    assert!(
        out.contains("HostnameMismatch"),
        "an address is verified against the certificate like any other name, \
         so one carrying no matching IP SAN is a HostnameMismatch, got:\n{out}"
    );
    assert!(
        !out.contains("InvalidServerName"),
        "refusing the address before the certificate is examined is the \
         behaviour the old comments described, and it is not what happens:\n{out}"
    );
    assert!(
        !out.contains("connected"),
        "the handshake must not succeed against a certificate that does not \
         carry this address:\n{out}"
    );
}

#[test]
#[ignore = "needs the VM"]
fn an_ip_literal_against_a_matching_ip_san_completes_the_handshake() {
    let ca = make_ca();
    // `CertificateParams::new` sorts a SAN that parses as an address into
    // `SanType::IpAddress`, so this leaf carries `IP:127.0.0.1` — the class of
    // certificate a refusal of IP literals would make unusable.
    let leaf = make_leaf(&ca, "127.0.0.1", Validity::Current);
    let port = spawn_tls_server(leaf, "HTTP/1.1 200 OK\r\n\r\n");

    let out = run_program("ip_san", &connect_program("127.0.0.1", port), Some(&ca.pem));

    assert!(
        out.contains("connected"),
        "an address matching the certificate's IP SAN verifies, and the \
         handshake completes, got:\n{out}"
    );
    assert!(
        !out.contains("failed:"),
        "no TLS error is expected when the address is on the certificate:\n{out}"
    );
}

/// The variant keeps a witness: `InvalidServerName` is alive, and this is the
/// whole of what reaches it — a name that is neither a DNS name nor an address.
///
/// Only the split `net.connect` + `tls.handshake` form can get here.
/// `tls.connect` resolves the host first and fails as `Transport(NetError)`
/// before the name is ever handed to rustls.
#[test]
#[ignore = "needs the VM"]
fn a_name_that_is_neither_dns_nor_address_is_an_invalid_server_name() {
    let ca = make_ca();
    let leaf = make_leaf(&ca, "localhost", Validity::Current);
    let port = spawn_tls_server(leaf, "HTTP/1.1 200 OK\r\n\r\n");

    let out = run_program(
        "bad_name",
        &connect_program("bad_name!", port),
        Some(&ca.pem),
    );

    assert!(
        out.contains("InvalidServerName"),
        "a name that parses as neither a DNS name nor an address has nothing \
         a certificate could be checked against, got:\n{out}"
    );
    assert!(
        !out.contains("connected"),
        "the handshake must not succeed with an unusable name:\n{out}"
    );
}

// ---------------------------------------------------------------------------
// The positive test, so the negatives are not passing by never connecting
// ---------------------------------------------------------------------------

#[test]
#[ignore = "needs the VM"]
fn a_trusted_certificate_completes_the_handshake_and_moves_bytes() {
    let ca = make_ca();
    let leaf = make_leaf(&ca, "localhost", Validity::Current);
    let port = spawn_tls_server(leaf, "hello over tls");

    let src = format!(
        r#"import scarlet/net
import scarlet/net/tls
import scarlet/net/socket
import scarlet/binary
import scarlet/string

pub fn main() {{
    match net.connect('127.0.0.1', {port}) {{
        Ok(sock) -> match tls.handshake(sock, 'localhost') {{
            Ok(conn) -> {{
                tls.write(conn, <<'ping'>>) or Nil
                match tls.read(conn, 1024) {{
                    Ok(socket.Data(b)) -> println('read: ${{binary.to_string(b) or "<not utf-8>"}}')
                    Ok(socket.Closed) -> println('closed')
                    Err(e) -> println('read failed: ${{string.inspect(e)}}')
                }}
                tls.close(conn) or Nil
            }}
            Err(e) -> println('failed: ${{string.inspect(e)}}')
        }}
        Err(e) -> println('connect failed: ${{string.inspect(e)}}')
    }}
}}
"#
    );

    let out = run_program("trusted", &src, Some(&ca.pem));

    assert!(
        out.contains("read: hello over tls"),
        "a trusted certificate must complete the handshake and carry bytes \
         both ways, got:\n{out}"
    );
}

/// `tls.read_within` against a peer that completes the handshake but never
/// sends application data must hit its deadline as `Transport(TimedOut)`
/// rather than parking forever. The server stays connected and silent, so
/// only the deadline timer can wake the parked read.
///
/// The deadline is captured once in Scarlet as an absolute Instant; the VM
/// re-pushes that same monotonic-ms on every park. A re-run that reset the
/// clock would still look like a timeout here (one park, then the timer),
/// so this test witnesses the typed timeout, not the re-run discipline.
#[test]
#[ignore = "needs the VM"]
fn tls_read_within_times_out_as_transport_timed_out() {
    let ca = make_ca();
    let leaf = make_leaf(&ca, "localhost", Validity::Current);
    // Handshake, then sit unread. The client never writes, so the peer never
    // replies, and the 100 ms deadline is the only wake.
    let port =
        spawn_stalled_tls_server(leaf, 1, std::time::Duration::from_secs(30), Excess::Ignore);

    let src = format!(
        r#"import scarlet/net
import scarlet/net/tls
import scarlet/net/error
import scarlet/string

pub fn main() {{
    match net.connect('127.0.0.1', {port}) {{
        Ok(sock) -> match tls.handshake(sock, 'localhost') {{
            Ok(conn) -> {{
                match tls.read_within(conn, 4096, 100) {{
                    Err(tls.Transport(error.TimedOut)) -> println('timed-out: Transport(TimedOut)')
                    Ok(_) -> println('unexpected-data')
                    Err(e) -> println('other-error: ${{string.inspect(e)}}')
                }}
                tls.close(conn) or Nil
            }}
            Err(e) -> println('handshake failed: ${{string.inspect(e)}}')
        }}
        Err(e) -> println('connect failed: ${{string.inspect(e)}}')
    }}
}}
"#
    );

    let (code, out) = run_bounded("read_within_timeout", &src, Some(&ca.pem), 30);
    assert_eq!(
        code,
        Some(0),
        "read_within must return, not hang, got:\n{out}"
    );
    assert!(
        out.contains("timed-out: Transport(TimedOut)"),
        "read_within must take its Err(Transport(TimedOut)) arm; got:\n{out}"
    );
    assert!(
        !out.contains("unexpected-data"),
        "read_within must not return Ok when no plaintext ever arrives:\n{out}"
    );
}

/// `tls.handshake_within` against a peer that accepts the connection and then
/// says nothing must hit its deadline as `Transport(TimedOut)`.
///
/// This is the wedge itself, not a proxy for it. `tls.handshake` parks on
/// readability alone; the peer never becomes readable, so no event and no timer
/// can wake the process, and the program never ends. The exit code is therefore
/// asserted as hard as the arm taken — `run_bounded` reports a killed child as
/// no exit code at all, which is the discriminator between a program that hung
/// and one that ran and printed nothing.
///
/// No trust root is installed, and none is needed: the deadline must fire
/// before a certificate is ever offered. That also keeps the test independent
/// of the certificate machinery every other test here depends on.
///
/// What it does NOT witness: the deadline surviving a re-run. One park and then
/// the timer produces this output, and a re-run that reset the clock would look
/// identical.
#[test]
#[ignore = "needs the VM"]
fn tls_handshake_within_times_out_against_a_silent_peer() {
    let port = spawn_silent_peer();

    let src = format!(
        r#"import scarlet/net
import scarlet/net/tls
import scarlet/net/error
import scarlet/string

pub fn main() {{
    match net.connect('127.0.0.1', {port}) {{
        Ok(sock) -> match tls.handshake_within(sock, 'localhost', 200) {{
            Err(tls.Transport(error.TimedOut)) -> println('timed-out: Transport(TimedOut)')
            Ok(_) -> println('connected')
            Err(e) -> println('other-error: ${{string.inspect(e)}}')
        }}
        Err(e) -> println('connect failed: ${{string.inspect(e)}}')
    }}
}}
"#
    );

    let (code, out) = run_bounded("handshake_within_timeout", &src, None, 30);
    assert_eq!(
        code,
        Some(0),
        "handshake_within must return against a silent peer, not hang, got:\n{out}"
    );
    assert!(
        out.contains("timed-out: Transport(TimedOut)"),
        "handshake_within must take its Err(Transport(TimedOut)) arm; got:\n{out}"
    );
    assert!(
        !out.contains("connected"),
        "a peer that never spoke TLS must not be reported as a TLS connection:\n{out}"
    );
}

/// Control for the test above: `tls.handshake_within` against a peer that does
/// speak TLS completes, and the connection carries bytes afterwards.
///
/// Without this the timeout test is an instrument that cannot fail — a
/// `handshake_within` hard-wired to return `Transport(TimedOut)` would satisfy
/// it. The deadline here is 10 s against loopback, generous on purpose: this
/// arm is about the deadline NOT firing, so it must not become a timing test of
/// a host that may be busy.
#[test]
#[ignore = "needs the VM"]
fn tls_handshake_within_completes_against_a_live_peer() {
    let ca = make_ca();
    let leaf = make_leaf(&ca, "localhost", Validity::Current);
    let port = spawn_tls_server(leaf, "hello inside the deadline");

    let src = format!(
        r#"import scarlet/net
import scarlet/net/tls
import scarlet/net/socket
import scarlet/binary
import scarlet/string

pub fn main() {{
    match net.connect('127.0.0.1', {port}) {{
        Ok(sock) -> match tls.handshake_within(sock, 'localhost', 10000) {{
            Ok(conn) -> {{
                tls.write(conn, <<'ping'>>) or Nil
                match tls.read(conn, 1024) {{
                    Ok(socket.Data(b)) -> println('read: ${{binary.to_string(b) or "<not utf-8>"}}')
                    Ok(socket.Closed) -> println('closed')
                    Err(e) -> println('read failed: ${{string.inspect(e)}}')
                }}
                tls.close(conn) or Nil
            }}
            Err(e) -> println('failed: ${{string.inspect(e)}}')
        }}
        Err(e) -> println('connect failed: ${{string.inspect(e)}}')
    }}
}}
"#
    );

    let out = run_program("handshake_within_ok", &src, Some(&ca.pem));

    assert!(
        out.contains("read: hello inside the deadline"),
        "handshake_within must complete a handshake that finishes inside its \
         deadline, and carry bytes afterwards, got:\n{out}"
    );
}

/// `tls.connect_within` against a listener that never accepts must hit its own
/// deadline rather than the kernel's.
///
/// This is the witness for the middle of the three steps: a full accept queue
/// leaves the call inside the TCP connect, so the deadline has to have survived
/// the resolve and reached `net.connect_addr_until` for this to return early.
///
/// The elapsed time is the assertion, not the error variant. Both the bounded
/// and the unbounded call end in `Transport(TimedOut)` here — the kernel gives
/// up on its own after `KERNEL_LOOPBACK_FLOOR_MS` — so only *when* it returns
/// says whose clock produced it. The sibling test in `vm_io.rs` was once
/// written the other way and passed with its deadline made inert.
///
/// What it does NOT witness: that the three steps share one Instant rather than
/// a fresh copy each. An IP literal resolves for free and the handshake is
/// never reached, so only one step spends any of the budget, and any bound
/// between the two constants produces this output.
///
/// Portability: this rests on a full accept queue dropping the SYN. A platform
/// that completes every connect panics inside `full_accept_queue`. A platform
/// that RSTs instead of dropping returns unmeasurable: the kernel answered,
/// so the deadline-vs-kernel-floor gap does not exist.
#[test]
#[ignore = "needs the VM"]
fn tls_connect_within_times_out_against_a_peer_that_never_accepts() {
    let Some((_listener, _fillers, port)) = full_accept_queue() else {
        return;
    };

    let src = format!(
        r#"import scarlet/net/tls
import scarlet/net/error
import scarlet/time
import scarlet/string

pub fn main() {{
    started = time.monotonic()
    outcome = tls.connect_within('127.0.0.1', {port}, {CONNECT_DEADLINE_MS})
    ms = time.since_ms(time.monotonic(), started)
    match outcome {{
        Err(tls.Transport(error.TimedOut)) -> println('timed-out ${{ms}}')
        Ok(_) -> println('connected ${{ms}}')
        Err(e) -> println('other-error: ${{string.inspect(e)}}')
    }}
}}
"#
    );

    let (code, out) = run_bounded("connect_within_timeout", &src, None, 30);
    assert_eq!(
        code,
        Some(0),
        "expected a clean exit; no exit code means it was killed\n--- output ---\n{out}"
    );
    // Parsed rather than substring-matched, and it panics on a missing line:
    // an absent number and a slow one must not read the same.
    let elapsed_ms: u64 = out
        .lines()
        .find_map(|l| l.trim().strip_prefix("timed-out "))
        .unwrap_or_else(|| panic!("expected the Transport(TimedOut) arm\n--- output ---\n{out}"))
        .trim()
        .parse()
        .expect("elapsed ms should be an integer");
    assert!(
        elapsed_ms < KERNEL_LOOPBACK_FLOOR_MS / 2,
        "returned after {elapsed_ms} ms, which is the kernel's floor \
         ({KERNEL_LOOPBACK_FLOOR_MS} ms) rather than the {CONNECT_DEADLINE_MS} ms \
         deadline: the deadline did not fire\n--- output ---\n{out}"
    );
}

/// `tls.connect_within` against a peer that accepts and then never speaks TLS
/// must hit its deadline in the handshake.
///
/// This is the witness for the last of the three steps, and its unbounded floor
/// is not a kernel timer but infinity: `tls.connect` parks on readability alone
/// against this peer, so a deadline that never reached `handshake_until` leaves
/// the program to be killed by `run_bounded`. The exit code is therefore
/// asserted as hard as the elapsed time — a killed child has no exit code at
/// all, which is what separates a wedge from a program that printed nothing.
///
/// No trust root is installed and none is needed: the deadline must fire before
/// a certificate is ever offered.
#[test]
#[ignore = "needs the VM"]
fn tls_connect_within_times_out_against_a_peer_that_never_speaks_tls() {
    let port = spawn_silent_peer();

    let src = format!(
        r#"import scarlet/net/tls
import scarlet/net/error
import scarlet/time
import scarlet/string

pub fn main() {{
    started = time.monotonic()
    outcome = tls.connect_within('127.0.0.1', {port}, {CONNECT_DEADLINE_MS})
    ms = time.since_ms(time.monotonic(), started)
    match outcome {{
        Err(tls.Transport(error.TimedOut)) -> println('timed-out ${{ms}}')
        Ok(_) -> println('connected ${{ms}}')
        Err(e) -> println('other-error: ${{string.inspect(e)}}')
    }}
}}
"#
    );

    let (code, out) = run_bounded("connect_within_silent", &src, None, 30);
    assert_eq!(
        code,
        Some(0),
        "connect_within must return against a silent peer, not hang\n--- output ---\n{out}"
    );
    let elapsed_ms: u64 = out
        .lines()
        .find_map(|l| l.trim().strip_prefix("timed-out "))
        .unwrap_or_else(|| panic!("expected the Transport(TimedOut) arm\n--- output ---\n{out}"))
        .trim()
        .parse()
        .expect("elapsed ms should be an integer");
    assert!(
        elapsed_ms < KERNEL_LOOPBACK_FLOOR_MS / 2,
        "returned after {elapsed_ms} ms against a peer that never speaks TLS, \
         far past the {CONNECT_DEADLINE_MS} ms deadline\n--- output ---\n{out}"
    );
}

/// Control for the two tests above, and the only test of `connect_until`'s own
/// signature: an absolute deadline the connection comfortably beats completes,
/// and the connection carries bytes afterwards.
///
/// Without this the timeout tests are instruments that cannot fail — a
/// `connect_within` hard-wired to return `Transport(TimedOut)`, or one whose
/// resolve step rejected every host, would satisfy both. The deadline is 10 s
/// against loopback, generous on purpose: this arm is about the deadline NOT
/// firing, so it must not become a timing test of a host that may be busy.
///
/// The certificate carries `IP:127.0.0.1` because `connect_until` verifies
/// against the same string it connects to. `localhost` cannot be used here: it
/// resolves to `::1` on this host, where nothing is bound.
#[test]
#[ignore = "needs the VM"]
fn tls_connect_until_completes_against_a_live_peer() {
    let ca = make_ca();
    let leaf = make_leaf(&ca, "127.0.0.1", Validity::Current);
    let port = spawn_tls_server(leaf, "hello inside the deadline");

    let src = format!(
        r#"import scarlet/net/tls
import scarlet/net/socket
import scarlet/binary
import scarlet/time
import scarlet/string

pub fn main() {{
    deadline = time.add_ms(time.monotonic(), 10000)
    match tls.connect_until('127.0.0.1', {port}, deadline) {{
        Ok(conn) -> {{
            tls.write(conn, <<'ping'>>) or Nil
            match tls.read(conn, 1024) {{
                Ok(socket.Data(b)) -> println('read: ${{binary.to_string(b) or "<not utf-8>"}}')
                Ok(socket.Closed) -> println('closed')
                Err(e) -> println('read failed: ${{string.inspect(e)}}')
            }}
            tls.close(conn) or Nil
        }}
        Err(e) -> println('failed: ${{string.inspect(e)}}')
    }}
}}
"#
    );

    let out = run_program("connect_until_ok", &src, Some(&ca.pem));

    assert!(
        out.contains("read: hello inside the deadline"),
        "connect_until must complete every step inside a deadline none of them \
         comes close to, and carry bytes afterwards, got:\n{out}"
    );
}

/// `client.secure` + `send_until` against a peer that completes the handshake
/// but never sends an HTTP response must hit `Tls(Transport(TimedOut))`.
/// The hang is after the handshake: `tls.connect`/`handshake` still have no
/// deadline of their own, so this test speaks TLS first and then waits.
#[test]
#[ignore = "needs the VM"]
fn https_send_until_times_out_against_a_silent_peer() {
    let ca = make_ca();
    let leaf = make_leaf(&ca, "localhost", Validity::Current);
    let port =
        spawn_stalled_tls_server(leaf, 1, std::time::Duration::from_secs(30), Excess::Ignore);

    let src = format!(
        r#"import scarlet/http/client
import scarlet/http/url
import scarlet/net
import scarlet/net/error
import scarlet/net/tls
import scarlet/string
import scarlet/time

pub fn main() {{
    match net.connect('127.0.0.1', {port}) {{
        Ok(sock) -> match tls.handshake(sock, 'localhost') {{
            Ok(conn) -> match url.parse('https://localhost/') {{
                Err(e) -> println('url failed: ${{string.inspect(e)}}')
                Ok(u) -> {{
                    io = client.secure(conn)
                    req = client.Request(method: <<'GET'>>, url: u, headers: [], body: <<>>)
                    match client.send_until(io, req, 1024, time.deadline_in_ms(200)) {{
                        Err(client.Tls(tls.Transport(error.TimedOut))) -> println('https-timeout: Tls(Transport(TimedOut))')
                        Ok(r) -> println('https-ok: ${{r.status}}')
                        Err(e) -> println('other: ${{string.inspect(e)}}')
                    }}
                    shut = io.close
                    shut() or Nil
                }}
            }}
            Err(e) -> println('handshake failed: ${{string.inspect(e)}}')
        }}
        Err(e) -> println('connect failed: ${{string.inspect(e)}}')
    }}
}}
"#
    );

    let (code, out) = run_bounded("https_client_hang", &src, Some(&ca.pem), 30);
    assert_eq!(
        code,
        Some(0),
        "https send_until must return, not hang, got:\n{out}"
    );
    assert!(
        out.contains("https-timeout: Tls(Transport(TimedOut))"),
        "https send_until must take its Err(Tls(Transport(TimedOut))) arm; got:\n{out}"
    );
    assert!(
        !out.contains("https-ok:"),
        "https send_until must not return Ok when no response ever arrives:\n{out}"
    );
}

/// Control: the same `client.secure` + `send_until` path returns a response
/// the peer does send. A red hang test next to this is a deadline, not a
/// broken HTTPS client.
#[test]
#[ignore = "needs the VM"]
fn https_send_until_returns_a_response() {
    let ca = make_ca();
    let leaf = make_leaf(&ca, "localhost", Validity::Current);
    let port = spawn_tls_server(leaf, "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");

    let src = format!(
        r#"import scarlet/http/client
import scarlet/http/url
import scarlet/net
import scarlet/net/error
import scarlet/net/tls
import scarlet/string
import scarlet/time

pub fn main() {{
    match net.connect('127.0.0.1', {port}) {{
        Ok(sock) -> match tls.handshake(sock, 'localhost') {{
            Ok(conn) -> match url.parse('https://localhost/') {{
                Err(e) -> println('url failed: ${{string.inspect(e)}}')
                Ok(u) -> {{
                    io = client.secure(conn)
                    req = client.Request(method: <<'GET'>>, url: u, headers: [], body: <<>>)
                    match client.send_until(io, req, 1024, time.deadline_in_ms(5000)) {{
                        Err(client.Tls(tls.Transport(error.TimedOut))) -> println('https-timeout: Tls(Transport(TimedOut))')
                        Ok(r) -> println('https-ok: ${{r.status}}')
                        Err(e) -> println('other: ${{string.inspect(e)}}')
                    }}
                    shut = io.close
                    shut() or Nil
                }}
            }}
            Err(e) -> println('handshake failed: ${{string.inspect(e)}}')
        }}
        Err(e) -> println('connect failed: ${{string.inspect(e)}}')
    }}
}}
"#
    );

    let out = run_program("https_client_ok", &src, Some(&ca.pem));
    assert!(
        out.contains("https-ok: 200"),
        "https send_until must return the response that arrived; got:\n{out}"
    );
    assert!(
        !out.contains("https-timeout:"),
        "https send_until must not time out when the peer answered:\n{out}"
    );
}

/// `tls.read_within` returns the plaintext that arrives before the deadline.
#[test]
#[ignore = "needs the VM"]
fn tls_read_within_returns_data() {
    let ca = make_ca();
    let leaf = make_leaf(&ca, "localhost", Validity::Current);
    let port = spawn_tls_server(leaf, "hello over tls");

    let src = format!(
        r#"import scarlet/net
import scarlet/net/tls
import scarlet/net/socket
import scarlet/binary
import scarlet/string

pub fn main() {{
    match net.connect('127.0.0.1', {port}) {{
        Ok(sock) -> match tls.handshake(sock, 'localhost') {{
            Ok(conn) -> {{
                tls.write(conn, <<'ping'>>) or Nil
                match tls.read_within(conn, 1024, 5000) {{
                    Ok(socket.Data(b)) -> println('read: ${{binary.to_string(b) or "<not utf-8>"}}')
                    Ok(socket.Closed) -> println('closed')
                    Err(e) -> println('read failed: ${{string.inspect(e)}}')
                }}
                tls.close(conn) or Nil
            }}
            Err(e) -> println('failed: ${{string.inspect(e)}}')
        }}
        Err(e) -> println('connect failed: ${{string.inspect(e)}}')
    }}
}}
"#
    );

    let out = run_program("read_within_data", &src, Some(&ca.pem));
    assert!(
        out.contains("read: hello over tls"),
        "read_within must return the bytes that arrived before the deadline, \
         got:\n{out}"
    );
}

/// The cleartext handle is stale once the connection has been secured.
///
/// This is the runtime half of the guarantee whose compile-time half is that
/// `Socket` and `TlsSocket` are different types: `tls.handshake` re-keys the
/// connection, so a caller holding the `Socket` it passed in cannot go around
/// the encryption with it.
#[test]
#[ignore = "needs the VM"]
fn the_cleartext_handle_is_dead_after_an_upgrade() {
    let ca = make_ca();
    let leaf = make_leaf(&ca, "localhost", Validity::Current);
    let port = spawn_tls_server(leaf, "hello over tls");

    let src = format!(
        r#"import scarlet/net
import scarlet/net/tls
import scarlet/net/socket
import scarlet/string

pub fn main() {{
    match net.connect('127.0.0.1', {port}) {{
        Ok(plain) -> match tls.handshake(plain, 'localhost') {{
            Ok(secure) -> {{
                // The same connection, named by the handle that predates the
                // upgrade. It must not reach the wire.
                match socket.write(plain, <<'cleartext'>>) {{
                    Ok(Nil) -> println('LEAKED: cleartext write succeeded')
                    Err(e) -> println('cleartext refused: ${{string.inspect(e)}}')
                }}
                tls.close(secure) or Nil
            }}
            Err(e) -> println('handshake failed: ${{string.inspect(e)}}')
        }}
        Err(e) -> println('connect failed: ${{string.inspect(e)}}')
    }}
}}
"#
    );

    let out = run_program("stale", &src, Some(&ca.pem));

    assert!(
        !out.contains("LEAKED"),
        "a cleartext write on the pre-upgrade handle must not reach the \
         wire:\n{out}"
    );
    assert!(
        out.contains("cleartext refused: NotConnected"),
        "the pre-upgrade handle must be reported as a gone socket, got:\n{out}"
    );
}

/// A TLS opcode must survive the Scarlet function around it being compiled.
///
/// `NativeTable::WARM_CALLS` is 8, so the ninth call to a function runs
/// natively and every opcode in its body goes through `native_shims`'
/// dispatchers instead of the interpreter's own match. The TLS opcodes
/// were classified there — `TlsHandshake`, `TlsRead`, `TlsReadUntil` and
/// `TlsWrite` as `Park`, `TlsClose` as `Bridge` — and given an arm in
/// NEITHER dispatcher, so the ninth call fell through to `proof_violation`
/// and killed the program with `internal invariant violated: run_park_op
/// on an op is_native_park_op excludes (compiler bug)`.
///
/// Nothing caught it because every other test in this file calls TLS from the
/// top level of `main`, which is interpreted and never warms. The standard
/// library does not have that luxury: `tls.read_exact` recurses through
/// `read_exact_loop`, and `tls.connect` calls `handshake` from inside a
/// function body. A program that read a connection in a loop for nine
/// iterations would have died.
///
/// Each of the five is called TWELVE times, which is what makes this a test of
/// the dispatch rather than of TLS: three past the threshold, and the outcome
/// asserted is only that the program finished and reported typed results.
#[test]
#[ignore = "needs the VM"]
fn a_tls_op_survives_the_function_around_it_being_compiled() {
    const REPS: usize = 12;
    let ca = make_ca();
    let leaf = make_leaf(&ca, "localhost", Validity::Current);
    // Twelve 16-byte writes; the peer drains them all and reports.
    let expect = PATTERN.len() * REPS;
    let port = spawn_stalled_tls_server(leaf, expect, std::time::Duration::ZERO, Excess::Ignore);
    let dead = spawn_hangup_server(REPS);

    let src = format!(
        r#"import scarlet/net
import scarlet/net/tls
import scarlet/net/socket
import scarlet/binary
import scarlet/string

fn send_n(c tls.TlsSocket, b Binary, n Int) Result(Nil, tls.TlsError) {{
    if n <= 0 {{
        Ok(Nil)
    }} else {{
        match tls.write(c, b) {{
            Ok(Nil) -> send_n(c, b, n - 1)
            Err(e) -> Err(e)
        }}
    }}
}}

fn recv_n(c tls.TlsSocket, n Int, acc Int) Int {{
    if n <= 0 {{
        acc
    }} else {{
        match tls.read(c, 1) {{
            Ok(socket.Data(b)) -> recv_n(c, n - 1, acc + binary.byte_size(b))
            Ok(socket.Closed) -> acc
            Err(_) -> acc
        }}
    }}
}}

// A 1 ms deadline against a peer that has not sent yet. The point is that
// TlsReadUntil is dispatchable from a compiled body, not the timeout itself.
fn timeout_n(c tls.TlsSocket, n Int, acc Int) Int {{
    if n <= 0 {{
        acc
    }} else {{
        match tls.read_within(c, 1, 1) {{
            Err(_) -> timeout_n(c, n - 1, acc + 1)
            Ok(_) -> timeout_n(c, n - 1, acc)
        }}
    }}
}}

fn close_n(c tls.TlsSocket, n Int) Nil {{
    if n <= 0 {{
        Nil
    }} else {{
        {{
            tls.close(c) or Nil
            close_n(c, n - 1)
        }}
    }}
}}

// A handshake that fails the same way every time. The point is that the OP
// runs at all once the function around it is compiled, not what it returns.
fn shake_n(n Int, acc Int) Int {{
    if n <= 0 {{
        acc
    }} else {{
        match net.connect('127.0.0.1', {dead}) {{
            Ok(s) -> match tls.handshake(s, 'localhost') {{
                Ok(_) -> shake_n(n - 1, acc)
                Err(_) -> shake_n(n - 1, acc + 1)
            }}
            Err(_) -> shake_n(n - 1, acc)
        }}
    }}
}}

pub fn main() {{
    println('handshakes refused ${{shake_n({REPS}, 0)}}')

    match net.connect('127.0.0.1', {port}) {{
        Ok(sock) -> match tls.handshake(sock, 'localhost') {{
            Ok(conn) -> {{
                println('timeouts ${{timeout_n(conn, {REPS}, 0)}}')
                match send_n(conn, <<'{PATTERN}'>>, {REPS}) {{
                    Ok(Nil) -> println('wrote {REPS}')
                    Err(e) -> println('write failed: ${{string.inspect(e)}}')
                }}
                println('read back ${{recv_n(conn, {REPS}, 0)}}')
                close_n(conn, {REPS})
                println('closed {REPS}')
            }}
            Err(e) -> println('handshake failed: ${{string.inspect(e)}}')
        }}
        Err(e) -> println('connect failed: ${{string.inspect(e)}}')
    }}
}}
"#
    );

    let (code, out) = run_bounded("native_shim", &src, Some(&ca.pem), 60);

    assert_eq!(
        code,
        Some(0),
        "no TLS opcode may halt the VM once its function is compiled, got:\n{out}"
    );
    assert!(
        out.contains(&format!("handshakes refused {REPS}")),
        "every handshake against a peer that hangs up must come back as a \
         typed error, got:\n{out}"
    );
    assert!(
        out.contains(&format!("timeouts {REPS}")),
        "`tls.read_within` must be dispatchable from a compiled body, got:\n{out}"
    );
    assert!(
        out.contains(&format!("wrote {REPS}")),
        "all {REPS} writes must complete, got:\n{out}"
    );
    // The peer's whole reply, a byte per `tls.read`, then end of stream: the
    // reply is shorter than `REPS`, so the last reads see `Closed`.
    let reply_len = format!("got:{expect}:ok").len();
    assert!(
        out.contains(&format!("read back {reply_len}")),
        "the reads must return the bytes the peer sent, got:\n{out}"
    );
    assert!(
        out.contains(&format!("closed {REPS}")),
        "`tls.close` must be dispatchable from a compiled body, got:\n{out}"
    );
}

/// `tls.write` against a peer that has stopped reading parks and resumes, and
/// every byte arrives exactly once and in order.
///
/// This is the VM half of the flush guarantee. The mechanism itself —
/// `drain_and_flush` answering `Flushing` rather than `Done` while the session
/// still owes ciphertext — is asserted in `vm::tls::tests`, where the returned
/// value can be read directly. What this covers is what the VM does with it:
/// `tls_write`'s `Flushing` arm parks at `bytes.len()`, so the re-run carries an
/// EMPTY tail and only finishes the flush. A wedged re-run shows up here as no
/// exit code at all.
///
/// The duplication half rests entirely on [`Excess::Probe`]. Every write sends
/// the same repeating 16-byte [`PATTERN`], so a duplicated re-send is
/// byte-identical to a correct stream and no content assertion can separate the
/// two — the only surviving observable is the COUNT, and a server that stops
/// reading at `expect` cannot report one that is too large. Without the probe
/// this test passes against a VM that re-sends the whole payload on every
/// `Flushing` park.
///
/// The write is a LOOP of chunks rather than one big binary, and that is
/// load-bearing in two directions at once. Each chunk is under rustls's 64 KiB
/// send-buffer limit, so the session accepts it whole and `drain_write` reaches
/// `Done` — the only way out other than `Done` is then the flush. And the loop
/// is what fills the socket at all: this host's send buffer takes 1.79 MB
/// before it refuses a byte, which no single write under the 64 KiB limit could
/// ever reach.
#[test]
#[ignore = "needs the VM"]
fn a_large_tls_write_parks_and_resumes_without_duplicating_a_byte() {
    // 16 << 11 = 32 KiB per write, under rustls's 64 KiB limit; 96 of them is
    // 3 MiB, comfortably past a loopback socket's capacity with the peer's
    // receive window pinned small.
    const DOUBLINGS: u32 = 11;
    const WRITES: usize = 96;
    let chunk = PATTERN.len() << DOUBLINGS;
    let total = chunk * WRITES;

    let ca = make_ca();
    let leaf = make_leaf(&ca, "localhost", Validity::Current);
    let port = spawn_stalled_tls_server(
        leaf,
        total,
        std::time::Duration::from_secs(1),
        Excess::Probe,
    );

    let src = format!(
        r#"import scarlet/net
import scarlet/net/tls
import scarlet/net/socket
import scarlet/binary
import scarlet/string

fn grow(b Binary, n Int) Binary {{
    if n <= 0 {{
        b
    }} else {{
        grow(binary.append(b, b), n - 1)
    }}
}}

fn send_n(c tls.TlsSocket, b Binary, n Int) Result(Nil, tls.TlsError) {{
    if n <= 0 {{
        Ok(Nil)
    }} else {{
        match tls.write(c, b) {{
            Ok(Nil) -> send_n(c, b, n - 1)
            Err(e) -> Err(e)
        }}
    }}
}}

pub fn main() {{
    payload = grow(<<'{PATTERN}'>>, {DOUBLINGS})

    match net.connect('127.0.0.1', {port}) {{
        Ok(sock) -> match tls.handshake(sock, 'localhost') {{
            Ok(conn) -> {{
                println('sending ${{binary.byte_size(payload)}} x {WRITES}')
                match send_n(conn, payload, {WRITES}) {{
                    Ok(Nil) -> match tls.read(conn, 1024) {{
                        Ok(socket.Data(b)) -> println('reply: ${{binary.to_string(b) or "<not utf-8>"}}')
                        Ok(socket.Closed) -> println('reply: closed')
                        Err(e) -> println('read failed: ${{string.inspect(e)}}')
                    }}
                    Err(e) -> println('write failed: ${{string.inspect(e)}}')
                }}
                tls.close(conn) or Nil
            }}
            Err(e) -> println('handshake failed: ${{string.inspect(e)}}')
        }}
        Err(e) -> println('connect failed: ${{string.inspect(e)}}')
    }}
}}
"#
    );

    let (code, out) = run_bounded("large_write", &src, Some(&ca.pem), 60);

    assert_eq!(
        code,
        Some(0),
        "a TLS write past the socket's capacity must complete, not wedge, \
         got:\n{out}"
    );
    assert!(
        out.contains(&format!("sending {chunk} x {WRITES}")),
        "the payload must be the size this test is about, got:\n{out}"
    );
    assert!(
        out.contains(&format!("reply: got:{total}:ok")),
        "the peer must receive exactly the bytes written, once each and in \
         order, got:\n{out}"
    );
}

/// An upgrade must not orphan a process already parked on the cleartext id.
///
/// `tls.handshake` re-keys the connection under a new id, and it takes the old
/// entry out WITHOUT `evict_connection` on purpose, so the fd survives into the
/// TLS entry instead of being torn down. But a park names an ID, not an fd, and
/// `evict_connection` is also what fails the parks on an id that has gone away.
/// A sibling parked in `socket.read` when the upgrade happens is then waiting on
/// an id that nothing can ever resolve — no readiness event, because the fd is
/// registered under the new id; no eviction, because the entry left by another
/// door; and no owner death, because `release_connections_of` skips an id with
/// no entry. The program ends only when every process does, so it never ends.
///
/// STARTTLS and a proxied `CONNECT` are the shapes this module advertises, and
/// they are exactly the ones where a reader is most likely to be parked
/// already.
///
/// The assertion is on the program EXITING, and on the sibling reaching the
/// same gone-socket error `socket.close` gives it. The handshake's own outcome
/// is deliberately not asserted: it is the hang that is the defect, and it
/// happens whether the handshake succeeds or fails — the failure path evicts
/// the NEW id, which is not the one the sibling is on.
#[test]
#[ignore = "needs the VM"]
fn an_upgrade_wakes_a_sibling_parked_on_the_cleartext_id() {
    let ca = make_ca();
    let leaf = make_leaf(&ca, "localhost", Validity::Current);
    let port = spawn_tls_server(leaf, "hello over tls");

    let src = format!(
        r#"import scarlet/net
import scarlet/net/tls
import scarlet/net/socket
import scarlet/process
import scarlet/string

pub fn main() {{
    match net.connect('127.0.0.1', {port}) {{
        Ok(sock) -> {{
            _ = process.spawn(fn() {{
                println('sibling: parking in socket.read')
                match socket.read(sock, 16) {{
                    Ok(r) -> println('sibling: read ${{string.inspect(r)}}')
                    Err(e) -> println('sibling: err ${{string.inspect(e)}}')
                }}
            }})
            // Nothing is on the wire until the ClientHello, so the sibling has
            // nothing to read and is parked well before the upgrade re-keys it.
            process.sleep(500)
            println('parent: upgrading')
            match tls.handshake(sock, 'localhost') {{
                Ok(conn) -> {{
                    println('parent: handshake ok')
                    tls.close(conn) or Nil
                }}
                Err(e) -> println('parent: handshake err ${{string.inspect(e)}}')
            }}
            println('parent: done')
        }}
        Err(e) -> println('connect failed: ${{string.inspect(e)}}')
    }}
}}
"#
    );

    let (code, out) = run_bounded("upgrade_sibling", &src, Some(&ca.pem), 60);

    assert!(
        out.contains("sibling: parking in socket.read"),
        "the sibling must reach its read before the upgrade, or this test is \
         not exercising the park at all, got:\n{out}"
    );
    assert!(
        code.is_some(),
        "the program never ended: a process parked on the pre-upgrade id was \
         left waiting on an id nothing can resolve, got:\n{out}"
    );
    assert!(
        out.contains("sibling: err NotConnected"),
        "the sibling must be woken onto the gone-socket error, exactly as \
         `socket.close` on the same id gives it, got:\n{out}"
    );
}

/// Connecting with TLS to a plaintext port is a protocol error, not a hang and
/// not a silent success.
#[test]
#[ignore = "needs the VM"]
fn a_plaintext_peer_is_a_protocol_error() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    thread::spawn(move || {
        if let Ok((mut sock, _)) = listener.accept() {
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf);
            // An HTTP server's answer to a ClientHello: not TLS at all.
            let _ = sock.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n");
        }
    });

    let out = run_program("plaintext", &connect_program("localhost", port), None);

    assert!(
        out.contains("ProtocolError") || out.contains("HandshakeFailed"),
        "a peer that answers a ClientHello in cleartext must fail the \
         handshake with a typed error, got:\n{out}"
    );
    assert!(
        !out.contains("connected"),
        "a plaintext peer must not be reported as a TLS connection:\n{out}"
    );
}
