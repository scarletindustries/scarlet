// Exercises the VM's I/O opcodes (`Op::FileWrite` / `Op::FileRead` and the TCP
// ops). File and network I/O is always enabled, so these run plain `al run`
// through `common::run_al`; the TCP echo test spawns the `al` binary directly
// because the server must run concurrently with the Rust client driving it.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
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
fn connect_retry(port: u16) -> TcpStream {
    for _ in 0..200 {
        if let Ok(s) = TcpStream::connect(("127.0.0.1", port)) {
            return s;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("server on port {port} never accepted a connection");
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
"#
    .replace("__PORT__", &port.to_string());
    let prog = proj.dir.join("server.al");
    std::fs::write(&prog, src).unwrap();

    // The server blocks on accept(); spawn it and drive it from the client.
    let child = Command::new(env!("CARGO_BIN_EXE_al"))
        .arg("run")
        .arg(&prog)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn al server");

    let mut stream = connect_retry(port);
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
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
