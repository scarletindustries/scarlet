// Exercises the VM's I/O opcodes (`Op::FileWrite` / `Op::FileRead` and the TCP
// ops). File and network I/O is always enabled, so these run plain `al run`
// through `common::run_al`; the TCP echo test spawns the `al` binary directly
// because the server must run concurrently with the Rust client driving it.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

mod common;
use common::Project;
use common::net::spawn_al_server;

/// AL source that binds `127.0.0.1:0`, prints the `listening <addr>` line
/// `spawn_al_server` waits for, then runs `body` with `server` in scope.
/// `extra_imports` are added to the base `al/net`+`address`+`result` set.
fn listening_src(extra_imports: &str, body: &str) -> String {
    format!(
        "import al/net
import al/net/address
import al/result
{extra_imports}
match net.listen('127.0.0.1', 0) {{
	Ok(server) -> {{
		println('listening ${{result.map(net.local_addr(server), address.to_string) or '?'}}')
{body}
	}}
	Err(e) -> println('listen-failed: ${{e}}')
}}
"
    )
}

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

    let out = proj.run(&src);

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

/// Full TCP lifecycle through the VM: `net.listen` -> `net.local_addr` ->
/// `net.accept` -> `sock.peer` -> `net.read` -> `net.write` -> `net.close`.
/// The `al` program is an echo server; a Rust client drives it and asserts the
/// bytes round-trip and the server reached its `close` arm (printing `served`).
/// `net.local_addr` is proven by the harness itself: the client connects to
/// the port the announcement line reported. This is the only test that
/// exercises the socket opcodes' success paths.
#[test]
fn tcp_echo_server_roundtrip() {
    let proj = Project::new("io_tcp");
    let src = listening_src(
        "import al/net/socket.{Data, Closed}\nimport al/binary",
        r#"match net.accept(server) {
	Ok(sock) -> {
		println('peer ${address.to_string(sock.peer)}')
		match socket.read(sock, 4096) {
			Ok(Data(data)) -> match socket.write(sock, data) {
				Ok(_) -> match socket.close(sock) {
					Ok(_) -> match net.close(server) {
						Ok(_) -> println('served')
						Err(e) -> println('close-server-failed: ${e}')
					}
					Err(e) -> println('close-failed: ${e}')
				}
				Err(e) -> println('write-failed: ${e}')
			}
			Ok(Closed) -> println('peer-closed')
			Err(e) -> println('read-failed: ${e}')
		}
	}
	Err(e) -> println('accept-failed: ${e}')
}"#,
    );

    // The server blocks on accept(); spawn it and drive it from the client.
    let srv = spawn_al_server(&proj, &src);
    let mut stream = srv.connect();
    stream.write_all(b"ping-over-tcp").expect("client write");
    // Server echoes the bytes then closes the socket, so read_to_end sees the
    // echo followed by EOF.
    let mut got = Vec::new();
    stream.read_to_end(&mut got).expect("client read");
    assert_eq!(
        &got, b"ping-over-tcp",
        "echoed bytes must match what was sent"
    );

    // The announcement line was consumed by the harness; what remains starts
    // at the peer line.
    let stdout = srv.wait_ok(30);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 2, "expected peer/served, got: {stdout:?}");
    // peer_addr is the Rust client's loopback connection; port is ephemeral.
    assert!(
        lines[0].starts_with("peer 127.0.0.1:"),
        "peer_addr line mismatch: {:?}",
        lines[0]
    );
    // `served` proves every arm down to net.close returned Ok.
    assert_eq!(
        lines[1], "served",
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
    let proj = Project::new("io_connect");
    let src = r#"import al/experiments/scheduler
import al/net
import al/net/socket
import al/binary

match net.listen('127.0.0.1', 0) {
	Ok(server) -> match net.local_addr(server) {
		Ok(addr) -> {
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

			match net.connect('127.0.0.1', addr.port) {
				Ok(conn) -> {
					socket.write(conn, binary.from_string('hello')) or Nil
					socket.write(conn, binary.from_string(' world')) or Nil
					match socket.read_exact(conn, 11) {
						Ok(data) -> match binary.to_string(data) {
							Ok(text) -> println('echoed: ${text}')
							Err(_) -> println('not utf8')
						}
						Err(e) -> println('client read failed: ${e}')
					}
					socket.close(conn) or Nil
				}
				Err(e) -> println('connect failed: ${e}')
			}
		}
		Err(e) -> println('local_addr failed: ${e}')
	}
	Err(e) -> println('listen failed: ${e}')
}
"#;
    let out = proj.run(src);
    assert!(
        out.success,
        "program failed; stdout: {} stderr: {}",
        out.stdout, out.stderr
    );
    assert_eq!(
        out.stdout, "echoed: hello world\n",
        "stderr: {}",
        out.stderr
    );
}

/// `socket.read_within` against a peer that connects but never sends must hit
/// its deadline and take the `Err(timeout)` arm rather than blocking forever.
/// The Rust client stays connected and silent for the whole window, so the only
/// thing that can wake the parked read is the deadline timer.
#[test]
fn tcp_read_within_times_out() {
    let proj = Project::new("io_read_within_timeout");
    let src = listening_src(
        "import al/net/socket",
        r#"match net.accept(server) {
	Ok(sock) -> match socket.read_within(sock, 4096, 150) {
		Ok(_) -> println('unexpected-data')
		Err(e) -> println('timed-out: ${e}')
	}
	Err(e) -> println('accept-failed: ${e}')
}"#,
    );

    // The client connects (so `accept` returns) but never writes. Holding the
    // stream open keeps the socket alive-but-silent across the read window.
    let srv = spawn_al_server(&proj, &src);
    let _stream = srv.connect();
    let stdout = srv.wait_ok(30);
    assert!(
        stdout.contains("timed-out:"),
        "read_within must take its Err(timeout) arm; got stdout: {stdout:?}"
    );
    assert!(
        !stdout.contains("unexpected-data"),
        "read_within must not return Ok when no bytes ever arrive: {stdout:?}"
    );
}

/// `socket.read_within` returns `Ok` with the bytes when data arrives before the
/// deadline: the readable wake fires and the op re-runs to a successful read.
/// The generous timeout makes the success path independent of scheduler timing.
#[test]
fn tcp_read_within_returns_data() {
    let proj = Project::new("io_read_within_data");
    let src = listening_src(
        "import al/net/socket.{Data, Closed}\nimport al/binary",
        r#"match net.accept(server) {
	Ok(sock) -> match socket.read_within(sock, 4096, 5000) {
		Ok(Data(data)) -> match binary.to_string(data) {
			Ok(text) -> println('got: ${text}')
			Err(_) -> println('not-utf8')
		}
		Ok(Closed) -> println('peer-closed')
		Err(e) -> println('read-failed: ${e}')
	}
	Err(e) -> println('accept-failed: ${e}')
}"#,
    );

    let srv = spawn_al_server(&proj, &src);
    let mut stream = srv.connect();
    stream.write_all(b"hello").expect("client write");
    let stdout = srv.wait_ok(30);
    drop(stream);
    assert!(
        stdout.contains("got: hello"),
        "read_within must return the bytes that arrived; got stdout: {stdout:?}"
    );
}

/// Writing a *bit-unaligned* binary (`<<1:4>>` is 4 bits) to a file is rejected
/// inside the write opcode and surfaces as the typed `IoError::UnalignedBinary`
/// variant (matched directly here) — the file is never created.
#[test]
fn file_write_unaligned_binary_errors() {
    let proj = Project::new("io_unaligned");
    let data = proj.dir.join("out.bin");
    let src = r#"import al/io.{UnalignedBinary}
match io.write_file('__PATH__', <<1:4>>) {
	Err(UnalignedBinary) -> println('rejected')
	Err(_) -> println('wrong-error')
	Ok(_) -> println('wrote')
}
"#
    .replace("__PATH__", &data.display().to_string());

    let out = proj.run(&src);

    assert!(out.success, "program should run, stderr: {}", out.stderr);
    assert_eq!(
        out.stdout, "rejected\n",
        "expected the typed UnalignedBinary rejection, got: {:?}",
        out.stdout
    );
    assert!(
        !data.exists(),
        "no file should be created for a rejected unaligned write"
    );
}

/// Reading a missing path surfaces the typed `IoError::NotFound(path)`, matched
/// directly here — the offending path is carried in the variant, not buried in
/// a string. (Proves both the typing and the "carry arguments" goal.)
#[test]
fn file_read_missing_path_errors() {
    let proj = Project::new("io_missing");
    let missing = proj.dir.join("does_not_exist.txt");
    let src = r#"import al/io.{NotFound}
match io.read_text('__PATH__') {
	Ok(_) -> println('read-ok')
	Err(NotFound(p)) -> println('missing: ${p}')
	Err(_) -> println('wrong-error')
}
"#
    .replace("__PATH__", &missing.display().to_string());

    let out = proj.run(&src);

    assert!(out.success, "program should run, stderr: {}", out.stderr);
    assert_eq!(
        out.stdout,
        format!("missing: {}\n", missing.display()),
        "expected NotFound carrying the path, stderr: {}",
        out.stderr
    );
}

/// Connecting to a port with no listener surfaces the typed
/// `NetError::ConnectionRefused`, matched directly (not a string compare). This
/// also exercises the non-blocking-connect completion path (`finish_connect`).
#[test]
fn connect_refused_is_typed() {
    // Reserve an ephemeral localhost port and release it, leaving a port with
    // no listener. Tests that spawn a server must never do this: in the window
    // between release and the server's re-bind the kernel can hand the port to
    // a concurrent test, and a client connecting then reaches the wrong
    // listener. Spawned servers bind port 0 and announce the kernel-assigned
    // port instead — see `common::net::spawn_al_server`.
    let port = TcpListener::bind("127.0.0.1:0")
        .expect("reserve a free port")
        .local_addr()
        .expect("local_addr")
        .port();
    let proj = Project::new("net_refused");
    let src = r#"import al/net
import al/net/error.{ConnectionRefused}
match net.connect('127.0.0.1', __PORT__) {
	Err(ConnectionRefused) -> println('refused')
	Err(_) -> println('other-error')
	Ok(_) -> println('connected')
}
"#
    .replace("__PORT__", &port.to_string());

    let out = proj.run(&src);

    assert!(out.success, "program should run, stderr: {}", out.stderr);
    assert_eq!(
        out.stdout, "refused\n",
        "expected typed ConnectionRefused, got: {:?} (stderr: {})",
        out.stdout, out.stderr
    );
}

/// `io.read_file` offloads to the blocking thread pool instead of running on
/// a scheduler thread: several spawned processes plus main all read the same
/// file concurrently and every read completes correctly (exercises offload →
/// worker → completion → resume). Without the pool these reads would run
/// inline on the scheduler thread, stalling that core for the duration of
/// the syscall.
#[test]
fn file_read_offloads_to_blocking_pool() {
    let proj = Project::new("io_pool");
    let data = proj.dir.join("data.txt");
    std::fs::write(&data, "payload").unwrap();
    let src = r#"import al/io
import al/experiments/scheduler

fn reader() fn() Nil {
	fn() match io.read_text('__PATH__') {
		Ok(_) -> println('ok')
		Err(_) -> println('err')
	}
}

scheduler.spawn(reader())
scheduler.spawn(reader())
scheduler.spawn(reader())
match io.read_text('__PATH__') {
	Ok(_) -> println('ok')
	Err(_) -> println('err')
}
"#
    .replace("__PATH__", &data.display().to_string());

    let out = proj.run(&src);

    assert!(out.success, "program should run, stderr: {}", out.stderr);
    let oks = out.stdout.lines().filter(|&l| l == "ok").count();
    assert_eq!(
        oks, 4,
        "all four reads should complete via the pool; got: {:?}",
        out.stdout
    );
}

/// `net.resolve` runs `getaddrinfo` on the blocking pool and returns a typed
/// `IpAddress`. `localhost` always resolves on a loopback-capable host.
#[test]
fn dns_resolve_runs_on_pool() {
    let proj = Project::new("dns_resolve");
    let src = r#"import al/net
import al/experiments/scheduler

scheduler.spawn(fn() Nil)
match net.resolve('localhost') {
	Ok(_) -> println('resolved')
	Err(_) -> println('err')
}
"#;
    let out = proj.run(src);
    assert!(out.success, "program should run, stderr: {}", out.stderr);
    assert_eq!(
        out.stdout, "resolved\n",
        "localhost should resolve to a typed IpAddress; got: {:?}",
        out.stdout
    );
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
        .map(|v| {
            v.parse::<usize>()
                .expect("server sent non-numeric Content-Length")
        })
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

/// Read one response and assert it is a 200 whose body is exactly `expected`.
fn assert_200(stream: &mut TcpStream, buf: &mut Vec<u8>, expected: &[u8], ctx: &str) {
    let (status, _h, body) = read_one_response(stream, buf);
    assert!(
        status.starts_with("HTTP/1.1 200"),
        "{ctx}: status {status:?}"
    );
    assert_eq!(
        body,
        expected,
        "{ctx}: body {:?}",
        String::from_utf8_lossy(&body)
    );
}

/// End-to-end HTTP/1.1 through the pure-AL `al/http` server core: spawn an `al`
/// server bound to an ephemeral port, drive it with a raw TCP client, and
/// assert a well-formed 200 with a correct `Content-Length` and body. A second
/// request is pipelined into the *same* write so the connection driver must
/// answer it from its carried leftover bytes — exercising keep-alive and the
/// pipelining handoff without an intervening read.
#[test]
fn http_server_get_and_keepalive() {
    let proj = Project::new("io_http");
    let src = listening_src(
        "import al/http",
        "http.serve_on(server, fn(_req) http.text('Hello from al/http!'))",
    );

    // The server blocks in its accept loop; spawn it and drive it from a client.
    let srv = spawn_al_server(&proj, &src);
    let mut stream = srv.connect();

    // Two HTTP/1.1 GETs in a single write. Both land in one server-side read,
    // so the connection loop must parse the first, respond, then serve the
    // second from the carried leftover bytes (keep-alive + pipelining).
    let request = b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n";
    let mut pipelined = Vec::new();
    pipelined.extend_from_slice(request);
    pipelined.extend_from_slice(request);
    stream.write_all(&pipelined).expect("client write");

    let mut buf = Vec::new();
    for which in ["first", "second"] {
        assert_200(&mut stream, &mut buf, b"Hello from al/http!", which);
    }

    // The server loops forever; tear it down now that both requests answered.
    srv.shutdown_clean(stream);
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
    let proj = Project::new("io_http_chunked");
    let src = listening_src(
        "import al/http\nimport al/http/body\nimport al/http/headers\nimport al/binary",
        r#"http.serve_on(server, fn(req) {
	collected = body.collect(req.body, 1048576) or <<>>
	echoed = binary.to_string(collected) or '<not-utf8>'
	trailer = match headers.get(req.trailers, <<'x-checksum'>>) {
		Some(v) -> binary.to_string(v) or '?'
		None -> 'none'
	}
	http.text('body=[${echoed}] trailer=[${trailer}]')
})"#,
    );

    let srv = spawn_al_server(&proj, &src);
    let mut stream = srv.connect();

    // --- Exchange 1: chunked POST with a trailer ---------------------------
    // (Single-line byte string: Rust's `\` line continuation strips leading
    // whitespace, which would corrupt the " world" chunk's leading space.)
    let chunked_post: &[u8] = b"POST /upload HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\nX-Checksum: abc123\r\n\r\n";
    stream.write_all(chunked_post).expect("write chunked POST");

    let mut buf = Vec::new();
    assert_200(
        &mut stream,
        &mut buf,
        b"body=[hello world] trailer=[abc123]",
        "chunked POST",
    );

    // --- Exchange 2: the SAME connection must still be in frame ------------
    let get = b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n";
    stream.write_all(get).expect("write follow-up GET");
    assert_200(
        &mut stream,
        &mut buf,
        b"body=[] trailer=[none]",
        "follow-up GET (desync check)",
    );

    // --- Exchange 3: chunked POST + GET pipelined in ONE write -------------
    let mut pipelined = Vec::new();
    pipelined.extend_from_slice(
        b"POST /pipelined HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\n",
    );
    pipelined.extend_from_slice(get);
    stream.write_all(&pipelined).expect("write pipelined batch");

    assert_200(
        &mut stream,
        &mut buf,
        b"body=[abc] trailer=[none]",
        "pipelined chunked POST",
    );
    assert_200(
        &mut stream,
        &mut buf,
        b"body=[] trailer=[none]",
        "pipelined GET",
    );

    // --- Malformed chunked body is rejected with 400 -----------------------
    // (Fresh connection: the 400 closes the current one.)
    let mut bad_stream = srv.connect();
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
    srv.shutdown_clean(stream);
}
