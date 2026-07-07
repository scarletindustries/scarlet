//! Shared harness for tests that spawn an `al` subprocess as a TCP server and
//! drive it from a Rust client. The server binds `127.0.0.1:0` and prints
//! `listening <ip>:<port>` as its first line; [`spawn_al_server`] parses that
//! announcement, so the client can only ever reach the server's own listener.

use std::io::Read;
use std::net::TcpStream;
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

use super::{Project, wait_or_kill};

/// A spawned `al` server subprocess plus the port it announced. The `Drop`
/// impl kills and reaps the child, so a test that panics between spawn and
/// teardown never leaks a forever-looping server process. Consuming methods
/// (`shutdown_clean`, `wait_or_kill`) `take` the child first so `Drop` is a
/// no-op after them.
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

    /// Tear down a forever-looping server: drop the client connection, kill the
    /// child, and assert it neither reported a serve failure on stdout nor
    /// panicked on stderr.
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

    /// Bounded wait asserting a clean exit; dumps both streams on failure and
    /// returns captured stdout.
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

/// Read the spawned server's first stdout line — the `listening <ip>:<port>`
/// announcement it prints once `net.listen` has bound port 0 — and return the
/// kernel-assigned port. The line is read byte-at-a-time so nothing past the
/// newline is consumed, and the pipe is handed back to `child` so
/// `wait_with_output` still collects the rest of stdout. A reader thread plus
/// a receive timeout turns a server that wedges before announcing into a test
/// failure instead of a hung suite.
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
        // The receiver may have timed out and gone; nothing to do then.
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

/// Connect to the port a spawned server announced. The listener is bound
/// before the announcement is printed, so a single attempt succeeds — the
/// kernel completes the handshake from the backlog even before the server
/// calls accept. The 10s read timeout makes a wedged server fail the test
/// instead of hanging it.
pub fn connect(port: u16) -> TcpStream {
    let stream = TcpStream::connect(("127.0.0.1", port))
        .unwrap_or_else(|e| panic!("connect to announced port {port}: {e}"));
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream
}

/// Write `src` to `server.al` in `proj`, spawn `al run` on it with piped
/// output, and wait for it to announce its port. Every server source binds
/// `127.0.0.1:0` and prints `listening <ip>:<port>` (via `net.local_addr`) as
/// its first line; parsing that announcement before connecting means the
/// client can only ever reach the server's own listener — there is no
/// reserved-port handoff for a concurrent test to race. Call
/// [`AlServer::connect`] to open a client stream.
pub fn spawn_al_server(proj: &Project, src: &str) -> AlServer {
    let prog = proj.dir.join("server.al");
    std::fs::write(&prog, src).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_al"))
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
