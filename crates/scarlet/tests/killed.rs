//! What a program printed before it was killed has reached its reader.

mod common;

use std::io::Read;
use std::process::Stdio;
use std::sync::mpsc::channel;
use std::time::Duration;

use common::{ChildGuard, Project, scarlet_run};

/// The program never waits and never ends, so nothing but time writes out what it printed.
#[test]
fn a_killed_program_keeps_what_it_printed() {
    let proj = Project::new("killed");
    proj.write(
        "main.sca",
        r#"fn spin(n Int) Int {
	spin(n + 1)
}

pub fn main() {
	println("one")
	println("two")
	_ = spin(0)
	Nil
}
"#,
    );
    let mut child = scarlet_run(&proj.dir.join("main.sca"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn scarlet");
    let mut out = child.stdout.take().expect("its output is piped");
    let guard = ChildGuard::new(child);
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let mut buf = [0; 8];
        tx.send(out.read_exact(&mut buf).map(|()| buf))
    });
    let seen = rx.recv_timeout(Duration::from_secs(60));
    drop(guard);
    let seen = seen.expect("it printed in time").expect("it is read");
    assert_eq!(&seen, b"one\ntwo\n");
}
