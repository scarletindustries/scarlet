// Exercises the VM's I/O opcodes (`Op::FileWrite` / `Op::FileRead` and the TCP
// ops). File and network I/O is always enabled, so these run plain `al run`
// through `common::run_al`; the TCP echo test spawns the `al` binary directly
// because the server must run concurrently with the Rust client driving it.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

mod common;
use common::{Project, run_al};

/// `io.write_text` then `io.read_text` round-trips the content: stdout echoes
/// what was read back, and the bytes are genuinely on disk.
#[test]
fn file_write_then_read_roundtrips() {
    let proj = Project::new("io_roundtrip");
    let data = proj.dir.join("out.txt");
    let src = r#"import al/io

path = '__PATH__'
match io.write_text(path, 'hello-io') {
	Err(e) -> println('WRITE-FAILED: ${e}')
	Ok(_) -> match io.read_text(path) {
		Ok(s) -> println('roundtrip: ${s}')
		Err(e) -> println('READ-FAILED: ${e}')
	}
}
"#
    .replace("__PATH__", &data.display().to_string());
    let prog = proj.dir.join("prog.al");
    std::fs::write(&prog, src).unwrap();

    let out = run_al("run", &prog);

    assert!(
        out.success,
        "roundtrip should succeed\nstdout: {}\nstderr: {}",
        out.stdout, out.stderr
    );
    // The Ok arm ran with the value read back through Op::FileRead.
    assert_eq!(
        out.stdout, "roundtrip: hello-io\n",
        "wrong roundtrip stdout (stderr: {})",
        out.stderr
    );
    // Op::FileWrite actually persisted the bytes; not just an in-memory echo.
    assert_eq!(
        std::fs::read_to_string(&data).unwrap(),
        "hello-io",
        "file on disk has wrong content"
    );
}

/// Reserve an ephemeral localhost port, then release it so the spawned `al`
/// server can bind it. A small TOCTOU window exists but is acceptable for a
/// test; the server reports `listen-failed` (not a hang) if it loses the race.
fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").expect("reserve a free port");
    l.local_addr().expect("local_addr").port()
}

/// Connect to `127.0.0.1:port`, retrying until the server's `net.listen` has
/// bound (it is spawned concurrently). Fails the test if it never comes up.
/// The returned stream has a 10s read timeout so a wedged server fails the
/// test instead of hanging it.
fn connect_retry(port: u16) -> TcpStream {
    for _ in 0..200 {
        if let Ok(s) = TcpStream::connect(("127.0.0.1", port)) {
            s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            return s;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("server on port {port} never accepted a connection");
}

/// Write `src` (with `__PORT__` substituted) to `server.al` in `proj`, spawn
/// `al run` on it with piped output, and connect a client once the server is
/// listening. The server blocks in its accept loop; the caller drives it.
fn spawn_al_server(proj: &Project, src: &str, port: u16) -> (Child, TcpStream) {
    let prog = proj.dir.join("server.al");
    std::fs::write(&prog, src.replace("__PORT__", &port.to_string())).unwrap();

    let child = Command::new(env!("CARGO_BIN_EXE_al"))
        .arg("run")
        .arg(&prog)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn al server");

    (child, connect_retry(port))
}

/// Tear down a `spawn_al_server` pair: drop the client connection, kill the
/// (forever-looping) server, and assert it never reported a serve failure.
fn shutdown_clean(mut child: Child, stream: TcpStream) {
    drop(stream);
    child.kill().ok();
    let out = child.wait_with_output().expect("await server shutdown");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("serve failed"),
        "server reported a failure: {stdout}"
    );
}

/// Full TCP lifecycle through the VM: `net.listen` -> `net.local_addr` ->
/// `net.accept` -> `sock.peer` -> `net.read` -> `net.write` -> `net.close`.
/// The `al` program is an echo server; a Rust client drives it and asserts the
/// bytes round-trip and the server reached its `close` arm (printing `served`).
/// This is the only test that exercises the socket opcodes' success paths.
#[test]
fn tcp_echo_server_roundtrip() {
    let port = free_port();
    let proj = Project::new("io_tcp");
    let src = r#"import al/net
import al/net/socket
import al/net/address
import al/binary
import al/result

match net.listen('127.0.0.1', __PORT__) {
	Ok(server) -> {
		println('local ${result.map(net.local_addr(server), address.to_string) or '?'}')
		match net.accept(server) {
			Ok(sock) -> {
				println('peer ${address.to_string(sock.peer)}')
				match socket.read(sock, 4096) {
					Ok(data) -> match socket.write(sock, data) {
						Ok(_) -> match socket.close(sock) {
							Ok(_) -> match net.close(server) {
								Ok(_) -> println('served')
								Err(e) -> println('close-server-failed: ${e}')
							}
							Err(e) -> println('close-failed: ${e}')
						}
						Err(e) -> println('write-failed: ${e}')
					}
					Err(e) -> println('read-failed: ${e}')
				}
			}
			Err(e) -> println('accept-failed: ${e}')
		}
	}
	Err(e) -> println('listen-failed: ${e}')
}
"#;

    // The server blocks on accept(); spawn it and drive it from the client.
    let (child, mut stream) = spawn_al_server(&proj, src, port);
    stream.write_all(b"ping-over-tcp").expect("client write");
    // Server echoes the bytes then closes the socket, so read_to_end sees the
    // echo followed by EOF.
    let mut got = Vec::new();
    stream.read_to_end(&mut got).expect("client read");
    assert_eq!(
        &got, b"ping-over-tcp",
        "echoed bytes must match what was sent"
    );

    let out = child.wait_with_output().expect("await server");
    assert!(
        out.status.success(),
        "server failed; stdout: {} stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines.len(),
        3,
        "expected local/peer/served, got: {stdout:?}"
    );
    // local_addr must report the bound port; ip is whatever the VM binds to.
    assert!(
        lines[0].starts_with("local ") && lines[0].ends_with(&format!(":{port}")),
        "local_addr line mismatch: {:?}",
        lines[0]
    );
    // peer_addr is the Rust client's loopback connection; port is ephemeral.
    assert!(
        lines[1].starts_with("peer 127.0.0.1:"),
        "peer_addr line mismatch: {:?}",
        lines[1]
    );
    // `served` proves every arm down to net.close returned Ok.
    assert_eq!(
        lines[2], "served",
        "server did not complete the listen/accept/read/write/close chain"
    );
}

/// The client side of the API, exercised entirely from AL: a spawned process
/// serves one connection while the main process `net.connect`s to it, writes
/// in two pieces, and reads the echo back with `read_exact`. The server echoes
/// with a single vectored `write_parts`. Covers connect (non-blocking
/// completion), read_exact (cross-read accumulation), and write_parts.
#[test]
fn tcp_connect_and_vectored_echo() {
    let port = free_port();
    let proj = Project::new("io_connect");
    let src = r#"import al/experiments/scheduler
import al/net
import al/net/socket
import al/binary

match net.listen('127.0.0.1', __PORT__) {
	Ok(server) -> {
		scheduler.spawn(fn() {
			match net.accept(server) {
				Ok(sock) -> match socket.read_exact(sock, 5) {
					Ok(first) -> match socket.read_exact(sock, 6) {
						Ok(second) -> {
							socket.write_parts(sock, [first, second]) or Nil
							socket.close(sock) or Nil
						}
						Err(e) -> println('server read2 failed: ${e}')
					}
					Err(e) -> println('server read1 failed: ${e}')
				}
				Err(e) -> println('server accept failed: ${e}')
			}
		})

		match net.connect('127.0.0.1', __PORT__) {
			Ok(conn) -> {
				socket.write(conn, binary.from_string('hello')) or Nil
				socket.write(conn, binary.from_string(' world')) or Nil
				match socket.read_exact(conn, 11) {
					Ok(data) -> match binary.to_string(data) {
						Ok(text) -> println('echoed: ${text}')
						Err(_e) -> println('not utf8')
					}
					Err(e) -> println('client read failed: ${e}')
				}
				socket.close(conn) or Nil
			}
			Err(e) -> println('connect failed: ${e}')
		}
	}
	Err(e) -> println('listen failed: ${e}')
}
"#
    .replace("__PORT__", &port.to_string());
    let prog = proj.dir.join("connect.al");
    std::fs::write(&prog, src).unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_al"))
        .arg("run")
        .arg(&prog)
        .output()
        .expect("run al");

    assert!(
        out.status.success(),
        "program failed; stdout: {} stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "echoed: hello world\n",
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Writing a *bit-unaligned* binary (`<<1:4>>` is 4 bits) to a file is rejected
/// inside the write opcode (`reject_unaligned`) and surfaces as `Err` — the
/// file is never created.
#[test]
fn file_write_unaligned_binary_errors() {
    let proj = Project::new("io_unaligned");
    let data = proj.dir.join("out.bin");
    let src = r#"import al/io
match io.write_file('__PATH__', <<1:4>>) {
	Err(e) -> println('rejected: ${e}')
	Ok(_) -> println('wrote')
}
"#
    .replace("__PATH__", &data.display().to_string());
    let prog = proj.dir.join("prog.al");
    std::fs::write(&prog, src).unwrap();

    let out = run_al("run", &prog);

    assert!(out.success, "program should run, stderr: {}", out.stderr);
    assert!(
        out.stdout.starts_with("rejected:") && out.stdout.contains("non-byte-aligned"),
        "expected the unaligned-write rejection, got: {:?}",
        out.stdout
    );
    assert!(
        !data.exists(),
        "no file should be created for a rejected unaligned write"
    );
}

/// Reading a path that does not exist takes `push_io_result`'s `Err` branch
/// (the OS error is wrapped into a `Result.Err` string), not a panic or VM
/// abort.
#[test]
fn file_read_missing_path_errors() {
    let proj = Project::new("io_missing");
    let missing = proj.dir.join("does_not_exist.txt");
    let src = r#"import al/io
match io.read_text('__PATH__') {
	Ok(_) -> println('read-ok')
	Err(_) -> println('read-err')
}
"#
    .replace("__PATH__", &missing.display().to_string());
    let prog = proj.dir.join("prog.al");
    std::fs::write(&prog, src).unwrap();

    let out = run_al("run", &prog);

    assert!(out.success, "program should run, stderr: {}", out.stderr);
    assert_eq!(out.stdout, "read-err\n", "stderr: {}", out.stderr);
}

/// First byte offset of `needle` within `hay`, or `None`.
fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Read exactly one HTTP/1.1 response off `stream`, buffering through `buf`.
/// Accumulates until the CRLFCRLF head terminator, parses `Content-Length`,
/// then reads that many body bytes. Any bytes past this response (a pipelined
/// follow-up) are left in `buf` for the next call. Returns the status line, a
/// lowercased-name header map, and the body bytes.
fn read_one_response(
    stream: &mut TcpStream,
    buf: &mut Vec<u8>,
) -> (String, std::collections::HashMap<String, String>, Vec<u8>) {
    let mut tmp = [0u8; 4096];
    let header_end = loop {
        if let Some(pos) = find_subslice(buf, b"\r\n\r\n") {
            break pos + 4;
        }
        let n = stream
            .read(&mut tmp)
            .expect("read response head from server");
        assert!(
            n > 0,
            "server closed before a full response head; buffered: {:?}",
            String::from_utf8_lossy(&buf[..])
        );
        buf.extend_from_slice(&tmp[..n]);
    };

    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default().to_string();
    let mut headers = std::collections::HashMap::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }

    let content_length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let total = header_end + content_length;
    while buf.len() < total {
        let n = stream
            .read(&mut tmp)
            .expect("read response body from server");
        assert!(n > 0, "server closed before the full response body");
        buf.extend_from_slice(&tmp[..n]);
    }

    let body = buf[header_end..total].to_vec();
    buf.drain(..total);
    (status_line, headers, body)
}

/// End-to-end HTTP/1.1 through the pure-AL `al/http` server core: spawn an `al`
/// server bound to an ephemeral port, drive it with a raw TCP client, and
/// assert a well-formed 200 with a correct `Content-Length` and body. A second
/// request is pipelined into the *same* write so the connection driver must
/// answer it from its carried leftover bytes — exercising keep-alive and the
/// pipelining handoff without an intervening read.
#[test]
fn http_server_get_and_keepalive() {
    let port = free_port();
    let proj = Project::new("io_http");
    let src = r#"import al/http

match http.serve('127.0.0.1', __PORT__, fn(_req) http.text('Hello from al/http!')) {
	Ok(_) -> Nil
	Err(e) -> println('serve failed: ${e}')
}
"#;

    // The server blocks in its accept loop; spawn it and drive it from a client.
    let (child, mut stream) = spawn_al_server(&proj, src, port);

    // Two HTTP/1.1 GETs in a single write. Both land in one server-side read,
    // so the connection loop must parse the first, respond, then serve the
    // second from the carried leftover bytes (keep-alive + pipelining).
    let request = b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n";
    let mut pipelined = Vec::new();
    pipelined.extend_from_slice(request);
    pipelined.extend_from_slice(request);
    stream.write_all(&pipelined).expect("client write");

    let expected_body: &[u8] = b"Hello from al/http!";
    let mut buf = Vec::new();
    for which in ["first", "second"] {
        let (status_line, headers, body) = read_one_response(&mut stream, &mut buf);
        assert!(
            status_line.starts_with("HTTP/1.1 200"),
            "{which} response: status line was {status_line:?}"
        );
        let cl: usize = headers
            .get("content-length")
            .unwrap_or_else(|| panic!("{which} response: no Content-Length header ({headers:?})"))
            .parse()
            .expect("Content-Length must be numeric");
        assert_eq!(
            cl,
            expected_body.len(),
            "{which} response: Content-Length must equal the body length"
        );
        assert_eq!(
            body.as_slice(),
            expected_body,
            "{which} response: body bytes mismatch"
        );
    }

    // The server loops forever; tear it down now that both requests answered.
    shutdown_clean(child, stream);
}

/// End-to-end chunked transfer-encoding decode through the `al/http` server:
/// a Rust client POSTs a chunked-encoded body (two chunks + trailer) over a
/// kept-alive connection, the AL handler reads the decoded body and the
/// trailer, and a SECOND request on the same connection must also succeed —
/// proving the chunked framing consumed exactly the body + terminator and the
/// connection did not desync. A third exchange pipelines a chunked POST and a
/// GET into one write.
#[test]
fn http_server_chunked_post_roundtrip() {
    let port = free_port();
    let proj = Project::new("io_http_chunked");
    let src = r#"import al/http
import al/http/body
import al/http/headers
import al/binary

match http.serve('127.0.0.1', __PORT__, fn(req) {
	collected = body.collect(req.body, 1048576) or <<>>
	echoed = binary.to_string(collected) or '<not-utf8>'
	trailer = match headers.get(req.headers, <<'x-checksum'>>) {
		Some(v) -> binary.to_string(v) or '?'
		None -> 'none'
	}
	http.text('body=[${echoed}] trailer=[${trailer}]')
}) {
	Ok(_) -> Nil
	Err(e) -> println('serve failed: ${e}')
}
"#;

    let (child, mut stream) = spawn_al_server(&proj, src, port);

    // --- Exchange 1: chunked POST with a trailer ---------------------------
    // (Single-line byte string: Rust's `\` line continuation strips leading
    // whitespace, which would corrupt the " world" chunk's leading space.)
    let chunked_post: &[u8] = b"POST /upload HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\nX-Checksum: abc123\r\n\r\n";
    stream.write_all(chunked_post).expect("write chunked POST");

    let mut buf = Vec::new();
    let (status, _headers, body) = read_one_response(&mut stream, &mut buf);
    assert!(
        status.starts_with("HTTP/1.1 200"),
        "chunked POST status: {status:?}"
    );
    assert_eq!(
        body.as_slice(),
        b"body=[hello world] trailer=[abc123]",
        "decoded body / trailer mismatch: {:?}",
        String::from_utf8_lossy(&body)
    );

    // --- Exchange 2: the SAME connection must still be in frame ------------
    let get = b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n";
    stream.write_all(get).expect("write follow-up GET");
    let (status, _headers, body) = read_one_response(&mut stream, &mut buf);
    assert!(
        status.starts_with("HTTP/1.1 200"),
        "follow-up GET status: {status:?} (connection desynced after chunked body?)"
    );
    assert_eq!(
        body.as_slice(),
        b"body=[] trailer=[none]",
        "follow-up GET body mismatch"
    );

    // --- Exchange 3: chunked POST + GET pipelined in ONE write -------------
    let mut pipelined = Vec::new();
    pipelined.extend_from_slice(
        b"POST /pipelined HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\n",
    );
    pipelined.extend_from_slice(get);
    stream.write_all(&pipelined).expect("write pipelined batch");

    let (status, _headers, body) = read_one_response(&mut stream, &mut buf);
    assert!(status.starts_with("HTTP/1.1 200"), "pipelined POST status");
    assert_eq!(
        body.as_slice(),
        b"body=[abc] trailer=[none]",
        "pipelined chunked POST body mismatch"
    );
    let (status, _headers, body) = read_one_response(&mut stream, &mut buf);
    assert!(status.starts_with("HTTP/1.1 200"), "pipelined GET status");
    assert_eq!(
        body.as_slice(),
        b"body=[] trailer=[none]",
        "pipelined GET body mismatch"
    );

    // --- Malformed chunked body is rejected with 400 -----------------------
    // (Fresh connection: the 400 closes the current one.)
    let mut bad_stream = connect_retry(port);
    bad_stream
        .write_all(
            b"POST / HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\nhello\r\n0\r\n\r\n",
        )
        .expect("write malformed chunked POST");
    let mut bad_buf = Vec::new();
    let (status, _headers, _body) = read_one_response(&mut bad_stream, &mut bad_buf);
    assert!(
        status.starts_with("HTTP/1.1 400"),
        "malformed chunk size must be rejected with 400, got {status:?}"
    );

    drop(bad_stream);
    shutdown_clean(child, stream);
}
