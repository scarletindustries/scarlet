//! What a program sees of the world outside it: its arguments, its
//! environment and its clock. The driver makes one and hands it to
//! [`crate::run`], so a test can hand in a world of its own.

use std::time::Instant;

/// What one run sees of the world.
pub struct Host {
    /// `os.argv`: the path of the entrypoint that was run, then the arguments
    /// after it.
    argv: Vec<String>,
    /// `os.env`: each variable whose name and value are both UTF-8, in the
    /// order the OS lists them.
    env: Vec<(String, String)>,
    /// What `time.monotonic` counts from.
    started: Instant,
}

impl Host {
    pub fn new(argv: Vec<String>, env: Vec<(String, String)>) -> Host {
        Host {
            argv,
            env,
            started: Instant::now(),
        }
    }

    /// The world of this OS process, with its environment as it is now and
    /// the given `argv`.
    pub fn of_this_process(argv: Vec<String>) -> Host {
        let env = std::env::vars_os()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
            .collect();
        Host::new(argv, env)
    }

    pub(crate) fn argv(&self) -> &[String] {
        &self.argv
    }

    pub(crate) fn env(&self) -> &[(String, String)] {
        &self.env
    }

    /// Standard output's terminal size as (columns, rows), or `None` when it
    /// is not a terminal or reports no size.
    pub(crate) fn terminal_size(&self) -> Option<(u16, u16)> {
        let size = rustix::termios::tcgetwinsize(std::io::stdout()).ok()?;
        (size.ws_col > 0 && size.ws_row > 0).then_some((size.ws_col, size.ws_row))
    }

    /// Wait `ms` milliseconds. There is no scheduler yet, so this blocks the
    /// thread, and with it the one process there is.
    pub(crate) fn sleep_ms(&self, ms: u64) {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }

    /// Milliseconds since the run began, on a clock that only goes forward.
    pub(crate) fn monotonic_ms(&self) -> u128 {
        self.started.elapsed().as_millis()
    }
}
