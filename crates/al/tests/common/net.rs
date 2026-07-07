//! Shared harness for tests that spawn an `al` subprocess as a TCP server and
//! drive it from a Rust client. The server binds `127.0.0.1:0` and prints
//! `listening <ip>:<port>` as its first line; [`spawn_al_server`] parses that
//! announcement, so the client can only ever reach the server's own listener.

use std::io::Read;
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use super::Project;

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
/// output, and connect a client once the server announces its port. Every
/// server source binds `127.0.0.1:0` and prints `listening <ip>:<port>` (via
/// `net.local_addr`) as its first line; parsing that announcement before
/// connecting means the client can only ever reach the server's own listener —
/// there is no reserved-port handoff for a concurrent test to race. Returns
/// the port too, for tests that open further connections.
pub fn spawn_al_server(proj: &Project, src: &str) -> (Child, TcpStream, u16) {
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
    (child, connect(port), port)
}

/// Tear down a `spawn_al_server` pair: drop the client connection, kill the
/// (forever-looping) server, and assert it never reported a serve failure.
pub fn shutdown_clean(mut child: Child, stream: TcpStream) {
    drop(stream);
    child.kill().ok();
    let out = child.wait_with_output().expect("await server shutdown");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("serve failed"),
        "server reported a failure: {stdout}"
    );
}

/// Wait up to `secs` for a self-terminating `child` to exit, then collect its
/// output. Unlike `shutdown_clean` (which kills a forever-looping server),
/// callers here expect the child to finish on its own; the bound just
/// guarantees a wedge fails the test instead of hanging the suite. Callers
/// pass a bound far above the happy-path runtime: under a full parallel
/// `cargo test` the spawned VM competes with every other test binary for CPU,
/// and a tight bound turns load into a spurious kill on an otherwise correct
/// run.
pub fn wait_or_kill(mut child: Child, secs: u64) -> std::process::Output {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => break,
        }
    }
    // No-op if it already exited; forces the issue if it overran the deadline.
    child.kill().ok();
    child.wait_with_output().expect("collect child output")
}
