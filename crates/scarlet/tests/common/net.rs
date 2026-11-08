//! Harness for tests that spawn an `scarlet` subprocess as a TCP server and drive
//! it from a Rust client. The server binds `127.0.0.1:0` and prints
//! `listening <ip>:<port>` first; parsing that announcement means the client
//! can only ever reach the server's own listener.

use std::io::{ErrorKind, Read};
use std::net::TcpStream;
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

use super::{Project, wait_or_kill};

/// A spawned `scarlet` server subprocess plus the port it announced. `Drop` kills
/// and reaps the child so a panicking test leaks no server. Consuming methods
/// `take` the child first, making `Drop` a no-op after them.
pub struct AlServer {
    child: Option<Child>,
    pub port: u16,
}

impl Drop for AlServer {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl AlServer {
    /// Open a client connection to this server's announced port.
    pub fn connect(&self) -> TcpStream {
        connect(self.port)
    }

    /// Tear down a forever-looping server and assert it neither reported a
    /// serve failure nor panicked.
    pub fn shutdown_clean(mut self, stream: TcpStream) {
        drop(stream);
        let mut child = self.child.take().unwrap();
        child.kill().ok();
        let out = child.wait_with_output().expect("await server shutdown");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !stdout.contains("serve failed"),
            "server reported a failure: {stdout}"
        );
        assert!(!stderr.contains("panicked"), "server panicked: {stderr}");
    }

    /// Bounded wait for a self-terminating server; see [`super::wait_or_kill`].
    pub fn wait_or_kill(mut self, secs: u64) -> Output {
        wait_or_kill(self.child.take().unwrap(), secs)
    }

    /// Bounded wait asserting a clean exit; returns captured stdout.
    pub fn wait_ok(self, secs: u64) -> String {
        let out = self.wait_or_kill(secs);
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(
            out.status.success(),
            "server exited unsuccessfully\nstdout: {stdout}\nstderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        stdout
    }
}

/// Return the kernel-assigned port from the server's `listening <ip>:<port>`
/// first line. Read byte-at-a-time so nothing past the newline is consumed,
/// and the pipe is handed back so `wait_with_output` still gets the rest. The
/// reader thread plus receive timeout turns a wedged server into a failure
/// instead of a hung suite.
pub fn read_announced_port(child: &mut Child) -> u16 {
    fn die(child: &mut Child, msg: String) -> ! {
        child.kill().ok();
        let mut stderr = Vec::new();
        if let Some(mut e) = child.stderr.take() {
            e.read_to_end(&mut stderr).ok();
        }
        panic!("{msg}\nserver stderr: {}", String::from_utf8_lossy(&stderr));
    }

    let mut stdout = child.stdout.take().expect("server stdout is piped");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            match stdout.read(&mut byte) {
                Ok(n) if n > 0 && byte[0] != b'\n' => line.push(byte[0]),
                _ => break,
            }
        }
        let _ = tx.send((stdout, line));
    });
    let Ok((stdout, line)) = rx.recv_timeout(Duration::from_secs(30)) else {
        die(child, "server announced no port within 30s".to_string());
    };
    child.stdout = Some(stdout);

    let line = String::from_utf8_lossy(&line).into_owned();
    match line
        .strip_prefix("listening ")
        .and_then(|addr| addr.rsplit(':').next())
        .and_then(|p| p.parse().ok())
    {
        Some(port) => port,
        None => die(
            child,
            format!("server did not announce its port; first line: {line:?}"),
        ),
    }
}

/// Connect to the port a spawned server announced. One attempt suffices: the
/// listener is bound before the announcement, so the kernel completes the
/// handshake from the backlog even before the server calls accept.
pub fn connect(port: u16) -> TcpStream {
    let stream = TcpStream::connect(("127.0.0.1", port))
        .unwrap_or_else(|e| panic!("connect to announced port {port}: {e}"));
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream
}

/// Write `src` to `server.sca` in `proj`, spawn `al run` on it, and wait for
/// the announced port. `src` must bind `127.0.0.1:0` and print
/// `listening <ip>:<port>` as its first line.
pub fn spawn_al_server(proj: &Project, src: &str) -> AlServer {
    let prog = proj.dir.join("server.sca");
    std::fs::write(&prog, src).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_scarlet"))
        .arg("run")
        .arg(&prog)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn al server");

    let port = read_announced_port(&mut child);
    AlServer {
        child: Some(child),
        port,
    }
}

/// How many connections [`full_accept_queue`] will open before concluding this
/// platform answers whatever the accept queue holds. Darwin stops after one,
/// measured; the rest is headroom for platforms that admit more.
const MAX_FILL_ATTEMPTS: usize = 16;

/// What an overflow connect to a `listen(1)` socket meant.
#[derive(Debug, PartialEq, Eq)]
enum Overflow {
    /// SYN dropped; the accept queue is full and a later connect can hang.
    DroppedSyn,
    /// Kernel RST'd (ECONNRESET). The connect was answered, so the
    /// deadline-vs-kernel-floor tests have nothing to measure.
    Reset,
    Unexpected,
}

fn classify_overflow(err: &std::io::Error) -> Overflow {
    match err.kind() {
        ErrorKind::TimedOut | ErrorKind::WouldBlock => Overflow::DroppedSyn,
        ErrorKind::ConnectionReset => Overflow::Reset,
        _ => Overflow::Unexpected,
    }
}

/// A listener that never accepts, filled until the platform stops answering.
///
/// This is the peer an unbounded connect has no answer for: once the accept
/// queue is full the SYN is dropped rather than refused, so the client waits
/// with no answer and no error ever coming back. std's `TcpListener` always
/// listens with a backlog of 128, which is why this goes through `socket2`.
///
/// How many connections a `listen(1)` socket admits before that happens is
/// platform-specific — one on Darwin, measured; more than one on Linux, where
/// CI watched the connect complete — so the fill stops on the observation that
/// a connect no longer completes, never on a count. Assuming a count is what
/// made this test fail on Linux CI: the queue was still short, the connect
/// under test completed, and nothing about the deadline was witnessed.
///
/// Returns `None` when overflow is RST rather than a dropped SYN. A RST is
/// an answer, so the bounded-connect tests cannot witness a deadline.
///
/// The listener is returned because dropping it closes the port outright. The
/// fillers come with it because whether a closed-but-still-queued connection
/// keeps its slot is platform-specific — on Darwin it does, measured — and
/// holding them for the life of the test removes the question.
pub fn full_accept_queue() -> Option<(socket2::Socket, Vec<std::net::TcpStream>, u16)> {
    let addr: std::net::SocketAddr = "127.0.0.1:0".parse().expect("addr");
    let listener = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::STREAM,
        Some(socket2::Protocol::TCP),
    )
    .expect("socket");
    listener.bind(&addr.into()).expect("bind");
    listener.listen(1).expect("listen");
    let port = listener
        .local_addr()
        .expect("local_addr")
        .as_socket_ipv4()
        .expect("v4")
        .port();

    // A loopback connect the listener queues completes in about a millisecond,
    // so a filler that reaches this bound is a SYN that went unanswered.
    let fill_timeout = Duration::from_millis(300);
    let target = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let mut fillers = Vec::new();
    for _ in 0..MAX_FILL_ATTEMPTS {
        match std::net::TcpStream::connect_timeout(&target, fill_timeout) {
            Ok(filler) => fillers.push(filler),
            Err(e) => match classify_overflow(&e) {
                Overflow::DroppedSyn => return Some((listener, fillers, port)),
                Overflow::Reset => {
                    eprintln!(
                        "unmeasurable: kernel RST (ECONNRESET) filling a listen(1) queue \
                         instead of a dropped SYN; no unanswered connect to bound"
                    );
                    return None;
                }
                Overflow::Unexpected => {
                    panic!("filling the accept queue failed with {e:?}, not a dropped SYN")
                }
            },
        }
    }
    panic!(
        "{MAX_FILL_ATTEMPTS} connections to a listen(1) socket all completed: this platform \
         does not drop the SYN when the accept queue is full, so there is no way here to \
         make a connect hang"
    );
}

#[test]
fn classify_overflow_treats_rst_as_unmeasurable() {
    let rst_kind = std::io::Error::new(ErrorKind::ConnectionReset, "reset");
    assert_eq!(classify_overflow(&rst_kind), Overflow::Reset);
    assert_eq!(
        classify_overflow(&std::io::Error::new(ErrorKind::TimedOut, "t")),
        Overflow::DroppedSyn
    );
    assert_eq!(
        classify_overflow(&std::io::Error::new(ErrorKind::WouldBlock, "w")),
        Overflow::DroppedSyn
    );
    assert_eq!(
        classify_overflow(&std::io::Error::other("x")),
        Overflow::Unexpected
    );
}

// Darwin only: 54 is ECONNRESET there and nowhere else that CI runs — on Linux
// errno 54 is EXFULL, so this pin went red on every Linux job. The kind-level
// test above is the cross-platform half; this one freezes the exact number the
// macOS 27 kernel answered with, so it is scoped to the platform that produced it.
#[cfg(target_os = "macos")]
#[test]
fn classify_overflow_maps_econnreset_54() {
    // The macOS 27 panic spelled `Os { code: 54, kind: ConnectionReset }`.
    let rst = std::io::Error::from_raw_os_error(54);
    assert_eq!(rst.kind(), ErrorKind::ConnectionReset);
    assert_eq!(classify_overflow(&rst), Overflow::Reset);
}

/// The deadline the bounded connect tests are given, and the floor the kernel
/// enforces on the same call without one. Measured on aarch64-apple-darwin: a
/// loopback connect to a full accept queue fails by itself with ETIMEDOUT
/// after 7.836 s. Linux's floor is its SYN retransmission schedule and is far
/// longer — not measured here — so this is the tighter of the two and the
/// bounds that rest on it hold on both.
///
/// The gap between these two is the whole of what those tests can see. An
/// earlier version asserted only the `TimedOut` arm and the exit code, and
/// passed identically with the deadline made inert — both arms end in
/// `TimedOut`, so neither witnessed which clock produced it.
pub const CONNECT_DEADLINE_MS: u64 = 200;
pub const KERNEL_LOOPBACK_FLOOR_MS: u64 = 7_836;
