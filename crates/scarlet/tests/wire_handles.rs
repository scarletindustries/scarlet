//! Handles across the wire, end to end through `al run`. A `Pid`, a
//! `Subject`, a `Connection`, a listener and a port's stream encode as an
//! identity — run, kind, number — and decode, in the run that minted them, to
//! the same handle.
//!
//! Every test here does something through the DECODED handle that only the
//! original could do: a monitor placed through a decoded pid fires when that
//! process ends, a message sent through a decoded subject arrives in the
//! original's mailbox, bytes written through a decoded socket reach the peer.
//! `==` is asserted too, but a decoder that rebuilt a plausible-looking
//! different handle would pass `==` against nothing and these against less.

mod common;

use std::io::Read;

use common::net::spawn_al_server;
use common::{Project, run_outputs};

/// `process.self()` and a spawned worker's pid both round trip to `==`, and
/// a monitor placed through the decoded pid reports the worker's end with
/// the right pid and `Normal`. A decoded pid naming some other number would
/// report `NoProcess` at once instead.
#[test]
#[ignore = "needs the VM"]
fn a_pid_round_trips_and_a_monitor_placed_through_the_copy_fires() {
    run_outputs(
        "import scarlet/process\n\
         import scarlet/process.{Down, Normal}\n\
         import scarlet/wire\n\
         type Msg {\n\
         \tReady(gate process.Subject(Nil))\n\
         \tEnded(down Down)\n\
         }\n\
         pub fn main() {\n\
         \tme = process.self()\n\
         \tprintln(match wire.decode(wire.encode(me)) {\n\
         \t\tOk(p) -> p == me\n\
         \t\tErr(_) -> False\n\
         \t})\n\
         \tinbox = process.subject()\n\
         \tworker = process.spawn(fn() {\n\
         \t\tgate = process.subject()\n\
         \t\tprocess.send(inbox, Ready(gate))\n\
         \t\tprocess.receive(gate)\n\
         \t})\n\
         \tmatch wire.decode(wire.encode(worker)) {\n\
         \t\tOk(pid) -> {\n\
         \t\t\tprintln(pid == worker)\n\
         \t\t\t_ = process.monitor(pid, inbox, fn(d) Ended(d))\n\
         \t\t\tmatch process.receive(inbox) {\n\
         \t\t\t\tReady(gate) -> process.send(gate, Nil)\n\
         \t\t\t\tEnded(_) -> println('ended before it was told to')\n\
         \t\t\t}\n\
         \t\t\tmatch process.receive(inbox) {\n\
         \t\t\t\tEnded(down) -> {\n\
         \t\t\t\t\tprintln(down.pid == worker)\n\
         \t\t\t\t\tprintln(down.reason == Normal)\n\
         \t\t\t\t}\n\
         \t\t\t\tReady(_) -> println('a second gate')\n\
         \t\t\t}\n\
         \t\t}\n\
         \t\tErr(_) -> println('refused')\n\
         \t}\n\
         }\n",
        "True\nTrue\nTrue\nTrue\n",
    );
}

/// A message sent through the decoded subject arrives at the original's
/// owner. The original is the owning form and the copy is not; they are one
/// mailbox and compare equal.
#[test]
#[ignore = "needs the VM"]
fn a_subject_round_trips_and_a_message_sent_through_the_copy_arrives() {
    run_outputs(
        "import scarlet/process\n\
         import scarlet/wire\n\
         pub fn main() {\n\
         \ts = process.subject()\n\
         \tmatch wire.decode(wire.encode(s)) {\n\
         \t\tOk(copy) -> {\n\
         \t\t\tprintln(copy == s)\n\
         \t\t\tprocess.send(copy, 'through the copy')\n\
         \t\t\tprintln(process.receive(s) == 'through the copy')\n\
         \t\t}\n\
         \t\tErr(_) -> println('refused')\n\
         \t}\n\
         }\n",
        "True\nTrue\n",
    );
}

/// A `Subject` three levels down a public record crosses with the record,
/// and the identity at the bottom is still the mailbox it was.
#[test]
#[ignore = "needs the VM"]
fn a_subject_three_levels_down_round_trips() {
    run_outputs(
        "import scarlet/process\n\
         import scarlet/wire\n\
         pub type Inner {\n\
         \tInner(reply process.Subject(String))\n\
         }\n\
         pub type Middle {\n\
         \tMiddle(inner Inner)\n\
         }\n\
         pub type Outer {\n\
         \tOuter(mid Middle)\n\
         }\n\
         fn reply_of(o Outer) process.Subject(String) {\n\
         \to.mid.inner.reply\n\
         }\n\
         pub fn main() {\n\
         \treply = process.subject()\n\
         \tmatch wire.decode(wire.encode(Outer(Middle(Inner(reply))))) {\n\
         \t\tOk(back) -> {\n\
         \t\t\tprocess.send(reply_of(back), 'three levels down')\n\
         \t\t\tprintln(process.receive(reply))\n\
         \t\t}\n\
         \t\tErr(_) -> println('refused')\n\
         \t}\n\
         }\n",
        "three levels down\n",
    );
}

/// A listener and a `Socket` record, both through the wire: the connection
/// is accepted through the decoded listener, and the bytes the client reads
/// were written through the decoded socket. `Socket.peer` is an opaque
/// `SocketAddress` from another module, so this is the identity rule and the
/// opaque rule walking one record together.
#[test]
#[ignore = "needs the VM"]
fn a_listener_and_a_socket_record_round_trip_and_the_copies_are_used() {
    let proj = Project::new("wire_socket");
    let src = "import scarlet/binary
import scarlet/net
import scarlet/net/address
import scarlet/net/socket
import scarlet/result
import scarlet/wire

pub fn main() {
	match net.listen('127.0.0.1', 0) {
		Ok(server) -> {
			println('listening ${result.map(net.local_addr(server), address.to_string) or '?'}')
			match wire.decode(wire.encode(server)) {
				Ok(listener) -> {
					println(listener == server)
					match net.accept(listener) {
						Ok(Some(sock)) -> match wire.decode(wire.encode(sock)) {
							Ok(copy) -> {
								println(copy == sock)
								println(address.to_string(copy.peer) == address.to_string(sock.peer))
								match socket.write(copy, binary.from_string('through the copy')) {
									Ok(Nil) -> match socket.close(copy) {
										Ok(Nil) -> println('served')
										Err(e) -> println('close-failed: ${e}')
									}
									Err(e) -> println('write-failed: ${e}')
								}
							}
							Err(_) -> println('socket refused')
						}
						Ok(None) -> println('accept-closed')
						Err(e) -> println('accept-failed: ${e}')
					}
				}
				Err(_) -> println('listener refused')
			}
		}
		Err(e) -> println('listen-failed: ${e}')
	}
}
";
    let srv = spawn_al_server(&proj, src);
    let mut stream = srv.connect();
    let mut got = Vec::new();
    stream.read_to_end(&mut got).expect("client read");
    assert_eq!(
        &got, b"through the copy",
        "the bytes written through the decoded socket must reach the peer"
    );
    let stdout = srv.wait_ok(30);
    assert_eq!(stdout, "True\nTrue\nTrue\nserved\n");
}

/// `Port` is a record over a `Connection` and an `Int`, and needs no handle
/// mapping of its own. What it does need is the kind byte to be the value's:
/// its stream is `SocketKind::Port` at runtime under a field typed
/// `Connection`, and a decoder that rebuilt it as a connection would hand
/// `port.write` a handle the port table does not hold.
#[test]
#[ignore = "needs the VM"]
fn a_port_record_round_trips_and_the_copy_is_written_to() {
    run_outputs(
        "import scarlet/os/port\n\
         import scarlet/os/port.{Exited}\n\
         import scarlet/wire\n\
         pub fn main() {\n\
         \tmatch port.spawn('cat', []) {\n\
         \t\tOk(p) -> match wire.decode(wire.encode(p)) {\n\
         \t\t\tOk(copy) -> {\n\
         \t\t\t\tprintln(copy == p)\n\
         \t\t\t\tprintln(port.write(copy, <<'through the copy'>>) == Ok(Nil))\n\
         \t\t\t\tprintln(port.read_exact(p, 16) == Ok(<<'through the copy'>>))\n\
         \t\t\t\tprintln(port.close(copy) == Ok(Exited(0)))\n\
         \t\t\t}\n\
         \t\t\tErr(_) -> println('refused')\n\
         \t\t}\n\
         \t\tErr(_) -> println('spawn failed')\n\
         \t}\n\
         }\n",
        "True\nTrue\nTrue\nTrue\n",
    );
}

/// The bytes of a pid with its sixteen run-identity bytes zeroed are another
/// run's, and the refusal is the `OtherRun` constructor a program can match
/// on, carrying this run's identity first and the forged one second. The
/// header is eleven bytes and the run follows it, so bytes 11..27 are the
/// run.
#[test]
#[ignore = "needs the VM"]
fn a_handle_from_another_run_is_refused_with_other_run() {
    run_outputs(
        "import scarlet/binary\n\
         import scarlet/process\n\
         import scarlet/wire\n\
         import scarlet/wire.{OtherRun}\n\
         pub fn main() {\n\
         \tme = process.self()\n\
         \tbytes = wire.encode(me)\n\
         \tsize = binary.byte_size(bytes)\n\
         \thead = binary.slice_bytes(bytes, 0, 11) or <<>>\n\
         \ttail = binary.slice_bytes(bytes, 27, size - 27) or <<>>\n\
         \tzeros = <<0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0>>\n\
         \tmatch wire.decode(binary.concat([head, zeros, tail])) {\n\
         \t\tOk(p) -> println(p == me)\n\
         \t\tErr(OtherRun(mine, theirs)) -> {\n\
         \t\t\tprintln(binary.byte_size(mine) == 16)\n\
         \t\t\tprintln(theirs == zeros)\n\
         \t\t\tprintln(mine == theirs)\n\
         \t\t}\n\
         \t\tErr(_) -> println('some other refusal')\n\
         \t}\n\
         }\n",
        "True\nTrue\nFalse\n",
    );
}
