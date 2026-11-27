//! Where a running program's output goes.

use std::io::{self, BufWriter, IsTerminal, Stdout, StdoutLock, Write};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

/// Standard output, buffered the way C's stdio does it. A terminal sees every
/// write as soon as it is made, so a line drawn with `print` or a prompt
/// shows at once. A pipe or a file is buffered, since one write per line is
/// much slower.
pub enum ProgramOut {
    Terminal(StdoutLock<'static>),
    Buffered(Timed<Stdout>),
}

impl ProgramOut {
    pub fn stdout() -> Self {
        let out = io::stdout();
        if out.is_terminal() {
            ProgramOut::Terminal(out.lock())
        } else {
            ProgramOut::Buffered(Timed::new(out))
        }
    }
}

impl Write for ProgramOut {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            ProgramOut::Terminal(out) => {
                let n = out.write(buf)?;
                out.flush()?;
                Ok(n)
            }
            ProgramOut::Buffered(out) => out.write(buf),
        }
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        match self {
            ProgramOut::Terminal(out) => {
                out.write_all(buf)?;
                out.flush()
            }
            ProgramOut::Buffered(out) => out.write_all(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            ProgramOut::Terminal(out) => out.flush(),
            ProgramOut::Buffered(out) => out.flush(),
        }
    }
}

/// The longest that output waits in the buffer.
const LONGEST_WAIT: Duration = Duration::from_millis(100);

/// A buffer that a thread writes out [`LONGEST_WAIT`] after something is put
/// in it, so a program that is killed loses no more than that. It needs no
/// signal handler, and it works for a signal that nothing can catch.
pub struct Timed<W: Write + Send + 'static> {
    shared: Arc<Mutex<Shared<W>>>,
    flusher: Flusher,
}

struct Shared<W: Write> {
    out: BufWriter<W>,
    /// Whether the thread has been told of what is in `out`.
    due: bool,
}

/// The thread is started by the first write, and ends when its sender goes.
enum Flusher {
    NotStarted,
    Running(SyncSender<()>),
    /// The system gave no thread, so each write is written out as it is made.
    Refused,
}

impl<W: Write + Send + 'static> Timed<W> {
    fn new(out: W) -> Self {
        let out = BufWriter::new(out);
        Timed {
            shared: Arc::new(Mutex::new(Shared { out, due: false })),
            flusher: Flusher::NotStarted,
        }
    }

    /// Runs `write`, and sees to it that what it leaves in the buffer is written out.
    fn buffer<T>(
        &mut self,
        write: impl FnOnce(&mut BufWriter<W>) -> io::Result<T>,
    ) -> io::Result<T> {
        if let Flusher::NotStarted = self.flusher {
            self.flusher = start(&self.shared);
        }
        let mut shared = lock(&self.shared);
        let written = write(&mut shared.out)?;
        match &self.flusher {
            Flusher::Running(_) if shared.due => {}
            // The thread takes its message before it clears `due`, so there is room for this.
            Flusher::Running(wake) => match wake.try_send(()) {
                Ok(()) => shared.due = true,
                Err(_) => shared.out.flush()?,
            },
            Flusher::NotStarted | Flusher::Refused => shared.out.flush()?,
        }
        Ok(written)
    }
}

fn start<W: Write + Send + 'static>(shared: &Arc<Mutex<Shared<W>>>) -> Flusher {
    let shared = Arc::clone(shared);
    let (wake, woken) = sync_channel(1);
    let run = move || {
        while woken.recv().is_ok() {
            thread::sleep(LONGEST_WAIT);
            let mut shared = lock(&shared);
            shared.due = false;
            // What is not written stays in the buffer, and the program's next flush fails as
            // this one did.
            let _ = shared.out.flush();
        }
    };
    match thread::Builder::new().name("output".into()).spawn(run) {
        Ok(_) => Flusher::Running(wake),
        Err(_) => Flusher::Refused,
    }
}

fn lock<W: Write>(shared: &Mutex<Shared<W>>) -> MutexGuard<'_, Shared<W>> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

impl<W: Write + Send + 'static> Write for Timed<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buffer(|out| out.write(buf))
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.buffer(|out| out.write_all(buf))
    }

    fn flush(&mut self) -> io::Result<()> {
        lock(&self.shared).out.flush()
    }
}

#[cfg(test)]
mod tests;
