//! HTTP client read deadline: a peer that never sends must time out, and a
//! peer that does send must still complete.
//!
//! The hang case is the point. `client.plain` now closes over `socket.read_until`,
//! so a silent accepted connection is `Transport(TimedOut)` rather than a park
//! that outlives the suite. The success case is the control: a red hang test
//! next to a green exchange means the deadline fired, not that the client
//! cannot talk at all.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

mod common;
use common::wait_or_kill;

fn spawn_silent_peer(hold: Duration) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    thread::spawn(move || {
        let Ok((stream, _)) = listener.accept() else {
            return;
        };
        thread::sleep(hold);
        drop(stream);
    });
    port
}

fn spawn_http_peer(reply: &'static [u8]) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let mut buf = [0u8; 1024];
        let _ = stream.read(&mut buf);
        let _ = stream.write_all(reply);
        let _ = stream.flush();
        drop(stream);
    });
    port
}

fn client_src(port: u16, deadline_ms: i64) -> String {
    format!(
        r#"import scarlet/http/client.{{Request, Transport}}
import scarlet/http/url
import scarlet/net
import scarlet/net/error.{{TimedOut}}
import scarlet/string
import scarlet/time

pub fn main() {{
	match net.connect('127.0.0.1', {port}) {{
		Ok(sock) -> match url.parse('http://127.0.0.1/') {{
			Err(e) -> println('url failed: ${{string.inspect(e)}}')
			Ok(u) -> {{
				io = client.plain(sock)
				req = Request(method: <<'GET'>>, url: u, headers: [], body: <<>>)
				match client.send_until(io, req, 1024, time.deadline_in_ms({deadline_ms})) {{
					Err(Transport(TimedOut)) -> println('http-timeout: Transport(TimedOut)')
					Ok(r) -> println('http-ok: ${{r.status}}')
					Err(e) -> println('other: ${{string.inspect(e)}}')
				}}
				shut = io.close
				shut() or Nil
			}}
		}}
		Err(e) -> println('connect failed: ${{e}}')
	}}
}}
"#
    )
}

fn run_bounded(tag: &str, src: &str, secs: u64) -> (Option<i32>, String) {
    let dir = std::env::temp_dir().join(format!(
        "scarlet_http_client_{tag}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let entry = dir.join("main.scrl");
    std::fs::write(&entry, src).expect("write program");
    std::fs::write(dir.join("package.scrl"), "name = 'http_client_test'\n").expect("write package");
    let out = wait_or_kill(
        Command::new(env!("CARGO_BIN_EXE_scarlet"))
            .arg("run")
            .arg(&entry)
            .current_dir(&dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("run scarlet"),
        secs,
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_dir_all(&dir);
    (out.status.code(), combined)
}

/// A peer that accepts and never sends must hit the client's read deadline as
/// `Transport(TimedOut)` rather than parking forever.
#[test]
#[ignore = "needs the VM"]
fn http_send_until_times_out_against_a_silent_peer() {
    let port = spawn_silent_peer(Duration::from_secs(30));
    let (code, out) = run_bounded("http_hang", &client_src(port, 200), 10);
    assert_eq!(
        code,
        Some(0),
        "http send_until must return, not hang, got:\n{out}"
    );
    assert!(
        out.contains("http-timeout: Transport(TimedOut)"),
        "http send_until must take its Err(Transport(TimedOut)) arm; got:\n{out}"
    );
    assert!(
        !out.contains("http-ok:"),
        "http send_until must not return Ok when no bytes ever arrive:\n{out}"
    );
}

/// Control: the same `send_until` path returns the response a peer that does
/// send. A red hang test next to this is a deadline, not a broken client.
#[test]
#[ignore = "needs the VM"]
fn http_send_until_returns_a_response() {
    let port = spawn_http_peer(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
    let (code, out) = run_bounded("http_ok", &client_src(port, 5000), 10);
    assert_eq!(
        code,
        Some(0),
        "http send_until control must return, got:\n{out}"
    );
    assert!(
        out.contains("http-ok: 200"),
        "http send_until must return the response that arrived; got:\n{out}"
    );
    assert!(
        !out.contains("http-timeout:"),
        "http send_until must not time out when the peer answered:\n{out}"
    );
}
