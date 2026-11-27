use std::io::{self, Write};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use super::Timed;

/// Sends each write it is given.
struct Seen(Sender<Vec<u8>>);

impl Write for Seen {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let gone = |_| io::ErrorKind::BrokenPipe;
        self.0.send(buf.to_vec()).map_err(gone)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn timed() -> (Timed<Seen>, Receiver<Vec<u8>>) {
    let (tx, rx) = channel();
    (Timed::new(Seen(tx)), rx)
}

fn next(rx: &Receiver<Vec<u8>>) -> Vec<u8> {
    rx.recv_timeout(Duration::from_secs(20)).unwrap()
}

#[test]
fn what_is_written_comes_out_with_no_flush() {
    let (mut out, rx) = timed();
    for line in ["one", "two", "three"] {
        out.write_all(line.as_bytes()).unwrap();
        assert_eq!(next(&rx), line.as_bytes());
    }
}

#[test]
fn a_write_waits_for_the_buffer() {
    let (mut out, rx) = timed();
    for _ in 0..1000 {
        out.write_all(b"ab").unwrap();
    }
    let mut all = Vec::new();
    let mut writes = 0;
    while all.len() < 2000 {
        all.extend(next(&rx));
        writes += 1;
    }
    assert_eq!(all, b"ab".repeat(1000));
    assert!(writes < 500, "{writes} writes");
}

#[test]
fn a_flush_writes_at_once_and_only_once() {
    let (mut out, rx) = timed();
    out.write_all(b"now").unwrap();
    out.flush().unwrap();
    assert_eq!(rx.try_recv().unwrap(), b"now");
    out.write_all(b"later").unwrap();
    assert_eq!(next(&rx), b"later");
    assert!(rx.try_recv().is_err());
}

#[test]
fn a_reader_that_left_is_an_error() {
    let (mut out, rx) = timed();
    drop(rx);
    out.write_all(b"x").unwrap();
    assert_eq!(out.flush().unwrap_err().kind(), io::ErrorKind::BrokenPipe);
}
