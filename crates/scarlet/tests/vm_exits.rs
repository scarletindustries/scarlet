//! Fault isolation: a crash ends the process that raised it and nothing
//! else, is reported to stderr, is observable through monitors as a typed
//! reason, and spreads over links — up to and including main, which is what
//! turns an uncontained crash into a failing exit status. The crash-free
//! half of this (kill, links) is `tests/programs/exits.scrl`.

mod common;

use std::time::Instant;

use common::{AlOutput, Project};

fn run(tag: &str, src: &str) -> AlOutput {
    Project::new(tag).run(src)
}

/// The program failed, and said why on stderr.
fn assert_failed_with(out: &AlOutput, needle: &str) {
    assert!(
        !out.success,
        "expected a failing exit status\n--- stdout ---\n{}\n--- stderr ---\n{}",
        out.stdout, out.stderr
    );
    assert!(
        out.stderr.contains(needle),
        "stderr should mention {needle:?}:\n{}",
        out.stderr
    );
}

/// An unlinked child's crash is reported, seen by its monitor as the typed
/// reason, and does not touch the process that spawned it.
#[test]
#[ignore = "needs the VM"]
fn an_unlinked_crash_is_contained_and_typed() {
    let out = run(
        "contained",
        r#"import scarlet/process
import scarlet/process.{Crashed, SliceOutOfBounds}

fn third(xs Array(Int)) Int {
	match xs[2] {
		Some(x) -> x
		None -> -1
	}
}

pub fn main() {
	downs = process.subject()
	xs = [1, 2, 3]
	worker = process.spawn_unlinked(fn() {
		// An out-of-range slice is the simplest way to crash on purpose.
		_ = xs[0..9]
		Nil
	})
	_ = process.monitor(worker, downs, fn(d) d)
	match process.receive(downs).reason {
		Crashed(SliceOutOfBounds(from, to, length)) -> println('crashed: slice ${from}..${to} of ${length}')
		Crashed(_) -> println('crashed some other way')
		_ -> println('did not crash')
	}
	println('spawner still running: ${third(xs)}')
}
"#,
    );
    assert!(
        out.success,
        "the spawner must survive an unlinked child's crash\n--- stdout ---\n{}\n--- stderr ---\n{}",
        out.stdout, out.stderr
    );
    assert_eq!(
        out.stdout,
        "crashed: slice 0..9 of 3\nspawner still running: 3\n"
    );
    assert!(
        out.stderr.contains("crashed:") && out.stderr.contains("[0..9]"),
        "the crash must be reported on stderr as it happens:\n{}",
        out.stderr
    );
}

/// A run that stops, here by asking for more heap than there is, fails the
/// run and is reported once. Code has no other way to stop a run: failure is a
/// value, and only a limit of the machine is not.
#[test]
fn a_main_crash_fails_the_run_and_is_reported_once() {
    let out = run(
        "main_crash",
        r#"import scarlet/int

pub fn main() {
	println('before')
	_ = int.bitwise_shift_left(1, int.max_value)
	println('after')
}
"#,
    );
    assert_failed_with(&out, "ran out of heap");
    assert_eq!(out.stdout, "before\n");
    assert_eq!(
        out.stderr.matches("ran out of heap").count(),
        1,
        "reported exactly once:\n{}",
        out.stderr
    );
}

/// A crash in a linked child kills main: the program fails, promptly, even
/// though main itself was blocked for ever and never misbehaved.
#[test]
#[ignore = "needs the VM"]
fn a_linked_childs_crash_kills_main() {
    let started = Instant::now();
    let out = run(
        "linked_crash",
        r#"import scarlet/process

pub fn main() {
	xs = [1, 2, 3]
	_ = process.spawn(fn() {
		_ = xs[2..8]
		Nil
	})
	// Blocked for ever: only the link can end this process.
	_ = process.receive(process.subject())
	println('unreachable')
}
"#,
    );
    assert_failed_with(&out, "[2..8]");
    assert_failed_with(&out, "main process was killed");
    assert_eq!(out.stdout, "");
    assert!(
        started.elapsed().as_secs() < 10,
        "the link must end main promptly, not leave it blocked"
    );
}

/// A crash in main kills the processes linked to it, so a program whose
/// main crashes does not linger on the strength of its workers.
#[test]
#[ignore = "needs the VM"]
fn a_main_crash_kills_linked_workers() {
    let started = Instant::now();
    let out = run(
        "main_crash_cascade",
        r#"import scarlet/process

pub fn main() {
	// Linked to main; would otherwise keep the program alive for a minute.
	_ = process.spawn(fn() process.sleep(60000))
	xs = [1]
	_ = xs[0..5]
}
"#,
    );
    assert_failed_with(&out, "[0..5]");
    assert!(
        started.elapsed().as_secs() < 20,
        "the linked worker must be killed when main crashes, not slept out"
    );
}

/// The boundary: a crash below an unlinked process stops there. main
/// survives a crash two levels down when the middle process was spawned
/// unlinked, and sees it only through its monitor on the middle process.
#[test]
#[ignore = "needs the VM"]
fn a_cascade_stops_at_an_unlinked_process() {
    let out = run(
        "boundary",
        r#"import scarlet/process
import scarlet/process.{Killed}

pub fn main() {
	downs = process.subject()
	xs = [1, 2]
	middle = process.spawn_unlinked(fn() {
		// Linked to middle: its crash kills middle, and stops there.
		_ = process.spawn(fn() {
			_ = xs[0..3]
			Nil
		})
		process.receive(process.subject())
	})
	_ = process.monitor(middle, downs, fn(d) d)
	match process.receive(downs).reason {
		Killed -> println('middle was killed by its child')
		_ -> println('unexpected reason')
	}
	println('main survived')
}
"#,
    );
    assert!(
        out.success,
        "--- stdout ---\n{}\n--- stderr ---\n{}",
        out.stdout, out.stderr
    );
    assert_eq!(
        out.stdout,
        "middle was killed by its child\nmain survived\n"
    );
    assert!(out.stderr.contains("[0..3]"), "{}", out.stderr);
}

/// `net.serve` starts handlers unlinked and gives them their connections, so
/// a handler that crashes closes its own connection and the server keeps
/// serving the next one.
#[test]
#[ignore = "needs the VM"]
fn a_crashing_connection_handler_does_not_stop_the_server() {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    let proj = Project::new("crashing_handler");
    let src = r#"import scarlet/net
import scarlet/net/address
import scarlet/net/socket
import scarlet/net/socket.{Data, Closed}
import scarlet/process

pub fn main() {
	xs = [1, 2, 3]
	match net.listen('127.0.0.1', 0) {
		Ok(server) -> match net.local_addr(server) {
			Ok(addr) -> {
				_ = net.serve_on(server, fn(sock) {
					match socket.read(sock, 16) {
						Ok(Data(b)) -> {
							if b == <<'crash'>> {
								_ = xs[0..9]
								Nil
							} else {
								socket.write(sock, <<'served'>>) or Nil
							}
						}
						Ok(Closed) -> Nil
						Err(_) -> Nil
					}
				})
				println('listening ${address.to_string(addr)}')
				// Keep serving until told to stop; the test drives connections.
				process.sleep(20000)
			}
			Err(_) -> println('no addr')
		}
		Err(_) -> println('no listen')
	}
}
"#;
    let srv = common::net::spawn_al_server(&proj, src);

    // First connection: make its handler crash. The connection must close
    // (the crashed handler owned it), rather than hang open.
    let mut crasher = TcpStream::connect(("127.0.0.1", srv.port)).expect("connect");
    crasher.write_all(b"crash").expect("write");
    crasher
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    let mut buf = Vec::new();
    let closed = crasher.read_to_end(&mut buf);
    assert!(
        matches!(closed, Ok(0)),
        "the crashed handler's connection must be closed by the runtime, got {closed:?} / {buf:?}"
    );

    // Second connection: still served.
    let mut client = TcpStream::connect(("127.0.0.1", srv.port)).expect("connect 2");
    client.write_all(b"hello").expect("write 2");
    client
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    let mut reply = String::new();
    client.read_to_string(&mut reply).expect("read reply");
    assert_eq!(reply, "served");
}
