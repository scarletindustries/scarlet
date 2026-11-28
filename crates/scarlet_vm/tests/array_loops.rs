//! When the loops of `scarlet/array` give up what they have passed: the host is told of
//! each handle that nothing holds any more at the next call that asks it for something, so
//! a callback that asks finds out.

use scarlet_vm::{
    Audit, Deadline, Handle, Host, IoError, IpAddr, NetError, NoRandomness, OutputClosed, Received,
    SocketAddr, TlsError,
};

/// A world whose listeners are numbers, that writes down what it is asked.
#[derive(Default)]
struct Book {
    listeners: u64,
    lines: Vec<String>,
}

impl Host for Book {
    fn argv(&self) -> &[String] {
        &[]
    }
    fn env(&self) -> &[(String, String)] {
        &[]
    }
    fn print(&mut self, text: &[u8]) -> Result<(), OutputClosed> {
        let text = String::from_utf8_lossy(text);
        self.lines.push(text.trim_end().to_owned());
        Ok(())
    }
    fn monotonic_ms(&self) -> u128 {
        0
    }
    fn epoch_ms(&self) -> i128 {
        0
    }
    fn sleep_ms(&mut self, _: u64) {}
    fn random_bytes(&mut self, _: &mut [u8]) -> Result<(), NoRandomness> {
        Err(NoRandomness)
    }
    fn terminal_size(&self) -> Option<(u16, u16)> {
        None
    }
    fn read_file(&mut self, _: &[u8]) -> Result<Vec<u8>, IoError> {
        Err(IoError::NotFound)
    }
    fn write_file(&mut self, _: &[u8], _: &[u8]) -> Result<(), IoError> {
        Err(IoError::NotFound)
    }
    fn resolve(&mut self, _: &str, _: Option<Deadline>) -> Result<IpAddr, NetError> {
        Ok([127, 0, 0, 1].into())
    }
    fn listen(&mut self, _: SocketAddr) -> Result<Handle, NetError> {
        let h = Handle(self.listeners);
        self.listeners += 1;
        Ok(h)
    }
    fn accept(&mut self, _: Handle) -> Result<Option<(Handle, SocketAddr)>, NetError> {
        Err(NetError::NotConnected)
    }
    fn local_addr(&mut self, h: Handle) -> Result<SocketAddr, NetError> {
        self.lines.push(format!("at {}", h.0));
        let port = u16::try_from(h.0).expect("few");
        Ok(([127, 0, 0, 1], port).into())
    }
    fn connect(&mut self, _: SocketAddr, _: Option<Deadline>) -> Result<Handle, NetError> {
        Err(NetError::ConnectionRefused)
    }
    fn read(&mut self, _: Handle, _: usize, _: Option<Deadline>) -> Result<Received, NetError> {
        Err(NetError::NotConnected)
    }
    fn write(&mut self, _: Handle, _: &[&[u8]]) -> Result<(), NetError> {
        Err(NetError::NotConnected)
    }
    fn close(&mut self, _: Handle) -> Result<(), NetError> {
        Ok(())
    }
    fn tls_handshake(
        &mut self,
        _: Handle,
        _: &str,
        _: Option<Deadline>,
    ) -> Result<Handle, TlsError> {
        Err(TlsError::HandshakeFailed)
    }
    fn tls_read(&mut self, _: Handle, _: usize, _: Option<Deadline>) -> Result<Received, TlsError> {
        Err(TlsError::HandshakeFailed)
    }
    fn tls_write(&mut self, _: Handle, _: &[u8]) -> Result<(), TlsError> {
        Err(TlsError::HandshakeFailed)
    }
    fn tls_close(&mut self, _: Handle) -> Result<(), TlsError> {
        Ok(())
    }
    fn release(&mut self, h: Handle) {
        self.lines.push(format!("closed {}", h.0));
    }
}

/// What the world writes down when the array `ss` of `n` listeners, which nothing else
/// holds, is put through `with`. A callback there asks for the number of its listener.
fn book(n: usize, with: &str) -> Vec<String> {
    let src = format!(
        "import scarlet/array\n\
         import scarlet/net\n\
         fn open(n Int, acc Array(net::Server)) Array(net::Server) {{\n\
         \tif n == 0 {{\n\t\tacc\n\t}} else {{\n\
         \t\tmatch net::listen(\"127.0.0.1\", 0) {{\n\
         \t\t\tOk(s) -> open(n - 1, [..acc, s])\n\
         \t\t\tErr(_) -> acc\n\t\t}}\n\t}}\n}}\n\
         fn number(s net::Server) Int {{\n\
         \tmatch net::local_addr(s) {{\n\
         \t\tOk(a) -> a.port\n\
         \t\tErr(_) -> -1\n\t}}\n}}\n\
         pub fn main() {{\n\
         \tss = open({n}, [])\n\
         \tprintln({with})\n}}\n"
    );
    let result = scarlet_core::compile::compile_text(&src, None);
    assert!(result.success(), "{src}\n{:?}", result.diagnostics());
    let program = result.into_runnable().expect("a clean compile is runnable");
    let mut host = Book::default();
    assert_eq!(scarlet_vm::run(&program, &mut host, Audit::AtEnd), Ok(()));
    host.lines
}

const SIZES: [usize; 6] = [1, 3, 32, 33, 70, 1057];

/// The lines of a loop over listeners `0..n` that gives each up before it looks at the next.
fn passed(n: usize) -> Vec<String> {
    let mut want = Vec::new();
    for i in 0..n {
        if i > 0 {
            want.push(format!("closed {}", i - 1));
        }
        want.push(format!("at {i}"));
    }
    want
}

/// That what was looked at and closed until something was printed is `want`, and that by the
/// end each of `n` was closed once.
fn is_book(lines: &[String], n: usize, want: &[String], what: &str) {
    assert_eq!(&lines[..want.len()], want, "{what}");
    let mut closed: Vec<usize> = lines
        .iter()
        .filter_map(|l| l.strip_prefix("closed ")?.parse().ok())
        .collect();
    closed.sort_unstable();
    assert!(closed.into_iter().eq(0..n), "{what}");
}

/// Each element is given up once its callback has ended, before the next begins.
#[test]
fn a_loop_gives_up_what_it_has_passed() {
    for n in SIZES {
        for with in [
            "array::length(array::map(ss, fn(s) number(s)))",
            "array::length(array::filter(ss, fn(s) number(s) < 0))",
            "array::find(ss, fn(s) number(s) < 0) == None",
            "array::any(ss, fn(s) number(s) < 0)",
            "array::all(ss, fn(s) number(s) >= 0)",
            "array::fold(ss, 0, fn(t, s) t + number(s))",
            "array::each(ss, fn(s) if number(s) < 0 { Nil } else { Nil })",
        ] {
            let mut want = passed(n);
            let shown = match with {
                w if w.contains("map") => n.to_string(),
                w if w.contains("filter") => "0".to_owned(),
                w if w.contains("any") => "False".to_owned(),
                w if w.contains("fold") => (n * (n - 1) / 2).to_string(),
                w if w.contains("each") => "Nil".to_owned(),
                _ => "True".to_owned(),
            };
            want.push(shown);
            is_book(&book(n, with), n, &want, with);
        }
    }
}

/// What a loop gives back is kept, and what stops early asks for no more.
#[test]
fn a_loop_keeps_what_it_gives_back_and_stops_when_it_may() {
    for n in SIZES {
        let all: Vec<String> = (0..n).map(|i| format!("at {i}")).collect();
        let kept = "array::length(array::filter(ss, fn(s) number(s) >= 0))";
        is_book(&book(n, kept), n, &all, kept);
        let kept = "array::length(array::map(ss, fn(s) (number(s), s)))";
        is_book(&book(n, kept), n, &all, kept);
        let last = n - 1;
        for with in [
            format!("array::find(ss, fn(s) number(s) == {last}) == None"),
            format!("array::any(ss, fn(s) number(s) == {last})"),
            format!("array::all(ss, fn(s) number(s) != {last})"),
        ] {
            is_book(&book(n, &with), n, &passed(n), &with);
        }
        for with in [
            "array::find(ss, fn(s) number(s) == 0) == None",
            "array::any(ss, fn(s) number(s) == 0)",
            "array::all(ss, fn(s) number(s) != 0)",
        ] {
            let lines = book(n, with);
            is_book(&lines, n, &passed(1), with);
            let asked = lines.iter().filter(|l| l.starts_with("at")).count();
            assert_eq!(asked, 1, "{with}");
        }
        let back = "array::fold(array::reverse(ss), 0, fn(t, s) t + number(s))";
        let mut want = Vec::new();
        for i in (0..n).rev() {
            if i + 1 < n {
                want.push(format!("closed {}", i + 1));
            }
            want.push(format!("at {i}"));
        }
        is_book(&book(n, back), n, &want, back);
    }
}

/// A search with no callback has given the array up by the time it says what it found.
#[test]
fn a_search_has_given_its_array_up_when_it_ends() {
    for n in SIZES {
        let with = "{\n\
                    \t\tother = open(1, [])\n\
                    \t\tfound = match other[0] {\n\
                    \t\t\tSome(o) -> array::contains(ss, o)\n\
                    \t\t\tNone -> True\n\t\t}\n\
                    \t\t\"${found} ${array::map(other, fn(s) number(s))}\"\n\t}";
        let lines = book(n, with);
        let at = lines.iter().position(|l| *l == format!("at {n}"));
        let mut before = lines[..at.expect("asked")].to_vec();
        before.sort();
        let mut want: Vec<String> = (0..n).map(|i| format!("closed {i}")).collect();
        want.sort();
        assert_eq!(before, want);
        assert!(lines.contains(&format!("False [{n}]")), "{lines:?}");
    }
}
