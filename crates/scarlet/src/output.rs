//! Where a running program's output goes.

use std::io::{self, BufWriter, IsTerminal, StdoutLock, Write};

/// Standard output, buffered the way C's stdio does it. A terminal sees every
/// write as soon as it is made, so a line drawn with `print` or a prompt
/// shows at once. A pipe or a file is fully buffered, since nobody watches it
/// live and one write per line is much slower.
pub enum ProgramOut {
    Terminal(StdoutLock<'static>),
    Buffered(BufWriter<StdoutLock<'static>>),
}

impl ProgramOut {
    pub fn stdout() -> Self {
        let out = io::stdout();
        if out.is_terminal() {
            ProgramOut::Terminal(out.lock())
        } else {
            ProgramOut::Buffered(BufWriter::new(out.lock()))
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
