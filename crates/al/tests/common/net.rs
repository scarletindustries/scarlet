//! Harness for tests that spawn an `al` subprocess as a TCP server and drive
//! it from a Rust client. The server binds `127.0.0.1:0` and prints
//! `listening <ip>:<port>` first; parsing that announcement means the client
//! can only ever reach the server's own listener.

use std::io::Read;
use std::net::TcpStream;
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

use super::{Project, wait_or_kill};

/// A spawned `al` server subprocess plus the port it announced. `Drop` kills
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

/// Write `src` to `server.al` in `proj`, spawn `al run` on it, and wait for
/// the announced port. `src` must bind `127.0.0.1:0` and print
/// `listening <ip>:<port>` as its first line.
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
