//! Supervision cases that write to stderr or decide the exit status: crash
//! restarts, budgets giving up, what that does to the process that declared
//! the tree, and the creator rule. The stderr-quiet behaviour is
//! `tests/programs/supervisors.scrl`.

mod common;

use std::time::Instant;

use common::{AlOutput, Project};

fn run(tag: &str, src: &str) -> AlOutput {
    Project::new(tag).run(src)
}

/// A counter worker whose `Boom` message crashes it, plus a client that
/// retries across the restart gap.
const COUNTER: &str = r#"import scarlet/process
import scarlet/process.{Subject, OneForOne}

type Msg {
	Ping(reply Subject(Int))
	Boom
}

fn serve(inbox Subject(Msg)) Nil {
	count(inbox, 0)
}

fn count(inbox Subject(Msg), n Int) Nil {
	match process.receive(inbox) {
		Ping(reply) -> {
			process.send(reply, n)
			count(inbox, n + 1)
		}
		Boom -> {
			xs = [n]
			_ = xs[0..9]
			Nil
		}
	}
}

fn ping(c Subject(Msg)) Int {
	reply = process.subject()
	process.send(c, Ping(reply))
	match process.receive_within(reply, 20) {
		Ok(n) -> n
		Err(Nil) -> ping(c)
	}
}

// Crash it `n` times, pausing so each crash lands on a fresh incarnation
// rather than being dropped with the previous one's backlog.
fn crash(c Subject(Msg), n Int) Nil {
	if n > 0 {
		process.send(c, Boom)
		process.sleep(15)
		crash(c, n - 1)
	} else {
		Nil
	}
}
"#;

#[test]
#[ignore = "needs the VM"]
fn a_crashing_worker_is_restarted_and_its_crash_reported() {
    let out = run(
        "crash_restart",
        &format!(
            "{COUNTER}
pub fn main() {{
	app <- process.root(OneForOne(restarts: 3, within_ms: 5000))
	c = process.permanent(app, serve)
	println('${{ping(c)}} ${{ping(c)}}')
	crash(c, 1)
	println('after: ${{ping(c)}}')
	println('restarts: ${{process.info(process.supervised(c)).restarts}}')
	// Exhaust the budget so the program ends: three restarts are allowed,
	// the fourth crash in the window gives up and main — parked in
	// `root` — is killed.
	crash(c, 4)
	println('not reached')
}}
"
        ),
    );
    assert!(
        !out.success,
        "exhaustion must fail the run:\n{}",
        out.stderr
    );
    assert_eq!(out.stdout, "0 1\nafter: 0\nrestarts: 1\n");
    assert_eq!(
        out.stderr.matches("[0..9]").count(),
        5,
        "one report per crash:\n{}",
        out.stderr
    );
    assert!(
        out.stderr
            .contains("supervisor gave up: more than 3 restarts in 5000ms"),
        "{}",
        out.stderr
    );
    assert!(
        out.stderr.contains("main process was killed"),
        "{}",
        out.stderr
    );
}

/// `process.supervisor` (the value form) outlives a main that returns; if it
/// then gives up, nothing can be killed, so the failure becomes the exit
/// status once the program winds down.
#[test]
#[ignore = "needs the VM"]
fn a_tree_outliving_its_declarer_still_fails_the_run_when_it_gives_up() {
    let started = Instant::now();
    let out = run(
        "orphan_giveup",
        &format!(
            "{COUNTER}
pub fn main() {{
	app = process.supervisor(OneForOne(restarts: 1, within_ms: 5000))
	c = process.permanent(app, serve)
	_ = process.spawn_unlinked(fn() crash(c, 2))
	println('main returning')
}}
"
        ),
    );
    assert!(!out.success, "--- stderr ---\n{}", out.stderr);
    assert_eq!(out.stdout, "main returning\n");
    assert!(out.stderr.contains("supervisor gave up"), "{}", out.stderr);
    assert!(
        out.stderr.contains("a supervisor gave up"),
        "exit reason:\n{}",
        out.stderr
    );
    assert!(
        started.elapsed().as_secs() < 10,
        "the orphaned tree must be freed so the program can end"
    );
}

/// A main that returns leaves its tree running: the worker is still
/// restarted afterwards, and the program keeps going on the tree's account.
#[test]
#[ignore = "needs the VM"]
fn a_returned_main_leaves_its_tree_supervised() {
    let out = run(
        "orphan_restart",
        &format!(
            "{COUNTER}
pub fn main() {{
	app = process.supervisor(OneForOne(restarts: 5, within_ms: 5000))
	c = process.permanent(app, serve)
	_ = process.spawn_unlinked(fn() {{
		crash(c, 1)
		println('after main returned: ${{ping(c)}}')
		// Now end the program: exhaust the budget deliberately.
		crash(c, 6)
	}})
}}
"
        ),
    );
    assert!(!out.success);
    assert_eq!(out.stdout, "after main returned: 0\n");
}

#[test]
#[ignore = "needs the VM"]
fn a_nested_supervisor_that_gives_up_is_restarted_by_its_parent() {
    let out = run(
        "nested_giveup",
        &format!(
            "{COUNTER}
pub fn main() {{
	app <- process.root(OneForOne(restarts: 2, within_ms: 5000))
	inner = process.supervisor_in(app, OneForOne(restarts: 0, within_ms: 5000))
	c = process.permanent(inner, serve)
	crash(c, 1)
	println('served again by the restarted subtree: ${{ping(c)}}')
	println('inner restarts charged to app: ${{process.info(process.supervised(c)).restarts}}')
	crash(c, 3)
	println('not reached')
}}
"
        ),
    );
    assert!(!out.success);
    assert_eq!(
        out.stdout,
        "served again by the restarted subtree: 0\ninner restarts charged to app: 1\n"
    );
    // inner gives up on every crash (budget 0); app absorbs two of those,
    // the third exhausts it too.
    assert!(
        out.stderr
            .matches("supervisor gave up: more than 0 restarts")
            .count()
            >= 3,
        "{}",
        out.stderr
    );
    assert!(
        out.stderr
            .contains("more than 2 restarts in 5000ms (last: a child supervisor gave up)"),
        "{}",
        out.stderr
    );
}

/// A watch on a supervisor hears it give up, and then hears it removed when
/// the failure reaches (and kills) the process that declared it.
#[test]
#[ignore = "needs the VM"]
fn a_watch_on_a_supervisor_reports_its_give_up() {
    let out = run(
        "watch_giveup",
        &format!(
            "{COUNTER}
fn describe(e process.Exit) String {{
	what = match e.ended {{
		process.GaveUp(restarts, within_ms) -> 'gave up (${{restarts}} in ${{within_ms}}ms)'
		process.Exited(process.Killed) -> 'removed'
		process.Exited(_) -> 'exited'
	}}
	after = match e.status {{
		process.Restarting -> 'restarting'
		process.Gone -> 'gone'
		process.Running -> 'running'
	}}
	'${{what}}, ${{after}}'
}}

pub fn main() {{
	events = process.subject()
	// The declaring process is a helper, so that main survives to read the
	// events after the helper has been killed by the escalation.
	declared = process.subject()
	_ = process.spawn_unlinked(fn() {{
		inner = process.supervisor(OneForOne(restarts: 1, within_ms: 5000))
		c = process.permanent(inner, serve)
		process.send(declared, (inner.supervised, c))
		process.receive(process.subject())
	}})
	(place, c) = process.receive(declared)
	_ = process.watch(place, events, fn(e) e)
	crash(c, 2)
	println(describe(process.receive(events)))
	println(describe(process.receive(events)))
}}
"
        ),
    );
    assert!(out.success, "--- stderr ---\n{}", out.stderr);
    assert_eq!(
        out.stdout, "gave up (1 in 5000ms), restarting\nremoved, gone\n",
        "--- stderr ---\n{}",
        out.stderr
    );
}

#[test]
#[ignore = "needs the VM"]
fn only_the_creator_may_declare_into_a_supervisor() {
    let out = run(
        "creator_rule",
        r#"import scarlet/process
import scarlet/process.{OneForOne, Crashed, Supervision}

fn kill_repeatedly(sup process.Supervised, n Int) Nil {
	if n > 0 {
		match process.children(sup) {
			[worker, ..] -> match process.info(worker).pid {
				Some(pid) -> process.kill(pid)
				None -> Nil
			}
			[] -> Nil
		}
		process.sleep(10)
		kill_repeatedly(sup, n - 1)
	} else {
		Nil
	}
}

pub fn main() {
	app = process.supervisor(OneForOne(restarts: 1, within_ms: 1000))
	downs = process.subject()
	intruder = process.spawn_unlinked(fn() {
		_ = process.permanent(app, fn(inbox) process.receive(inbox))
		println('not reached')
	})
	_ = process.monitor(intruder, downs, fn(d) d)
	match process.receive(downs).reason {
		Crashed(Supervision(why)) -> println('refused: ${why}')
		_ -> println('unexpected')
	}
	// The creator itself may, of course.
	_ = process.permanent(app, fn(inbox) process.receive(inbox))
	println('declared: ${process.count(app.supervised)}')
	// End the program: the worker above lives under a supervisor owned by main,
	// which is returning, so kill it through an exhausted budget instead.
	_ = process.spawn_unlinked(fn() kill_repeatedly(app.supervised, 3))
}
"#,
    );
    assert_eq!(
        out.stdout,
        "refused: only the process that created a supervisor or factory may declare into it\ndeclared: 1\n",
        "--- stderr ---\n{}",
        out.stderr
    );
    assert!(out.stderr.contains("supervision:"), "{}", out.stderr);
}

/// `http.serve` is a subtree: a handler crash closes one connection, the
/// service reports its open connections, and the acceptors are supervised
/// children of the service.
#[test]
#[ignore = "needs the VM"]
fn a_service_is_a_supervised_subtree() {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    let proj = Project::new("service_tree");
    let src = r#"import scarlet/net
import scarlet/net/address
import scarlet/net/socket
import scarlet/net/socket.{Data, Closed}
import scarlet/process
import scarlet/process.{FactoryOf, Worker, Transient}
import scarlet/array

fn first(xs Array(String)) String {
	match xs {
		[x, ..] -> x
		[] -> 'none'
	}
}

// `stop` is how the test tells the server it is done: main then reports on
// the tree and closes the listener, so the program ends on its own and the
// report is complete whatever the machine is doing.
fn handle(sock socket.Socket, stop process.Subject(Nil)) Nil {
	match socket.read(sock, 64) {
		Ok(Data(b)) -> {
			if b == <<'crash'>> {
				xs = [1]
				_ = xs[0..2]
				Nil
			} else if b == <<'quit'>> {
				process.send(stop, Nil)
			} else {
				socket.write(sock, <<'ok'>>) or Nil
			}
		}
		Ok(Closed) -> Nil
		Err(_) -> Nil
	}
}

pub fn main() {
	match net.listen('127.0.0.1', 0) {
		Ok(server) -> match net.local_addr(server) {
			Ok(addr) -> {
				println('listening ${address.to_string(addr)}')
				stop = process.subject()
				service = net.serve_on(server, fn(sock) handle(sock, stop))
				process.receive(stop)
				kinds = array.map(process.children(service.supervised), fn(c) match process.info(c).kind {
					FactoryOf(_) -> 'factory'
					Worker(Transient) -> 'acceptor'
					_ -> 'other'
				})
				println('first child: ${first(kinds)}')
				println('acceptors: ${array.length(array.filter(kinds, fn(k) k == 'acceptor'))}')
				net.close(server) or Nil
			}
			Err(_) -> println('no addr')
		}
		Err(_) -> println('no listen')
	}
}
"#;
    let srv = common::net::spawn_al_server(&proj, src);

    let mut crasher = TcpStream::connect(("127.0.0.1", srv.port)).expect("connect");
    crasher.write_all(b"crash").expect("write");
    crasher
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    let mut buf = Vec::new();
    let closed = crasher.read_to_end(&mut buf);
    assert!(
        matches!(closed, Ok(0)),
        "a crashed handler's connection must close: {closed:?} / {buf:?}"
    );

    let mut client = TcpStream::connect(("127.0.0.1", srv.port)).expect("connect 2");
    client.write_all(b"hello").expect("write 2");
    client
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    let mut reply = [0u8; 2];
    client.read_exact(&mut reply).expect("reply");
    assert_eq!(&reply, b"ok");

    let mut quitter = TcpStream::connect(("127.0.0.1", srv.port)).expect("connect 3");
    quitter.write_all(b"quit").expect("write 3");
    drop(quitter);
    let out = srv.wait_or_kill(20);
    assert!(
        out.status.success(),
        "closing the listener ends the acceptors, and with them the program: {:?}",
        out.status
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stdout.contains("first child: factory"),
        "the connection factory is declared first (stopped last):\n{stdout}"
    );
    let acceptors: usize = stdout
        .lines()
        .find_map(|l| l.strip_prefix("acceptors: "))
        .and_then(|n| n.parse().ok())
        .expect("acceptor count line");
    assert!(acceptors >= 1, "{stdout}");
    assert!(
        stderr.contains("[0..2]"),
        "the handler crash is reported:\n{stderr}"
    );
    assert!(
        !stderr.contains("gave up"),
        "a handler crash must not count against the acceptors:\n{stderr}"
    );
}
