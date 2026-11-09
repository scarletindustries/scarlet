use std::io::IsTerminal;

/// Whether to write ANSI colour to `s`. `NO_COLOR` wins over `CLICOLOR_FORCE`,
/// which wins over the terminal check.
fn color_enabled(s: &impl IsTerminal) -> bool {
    if std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) {
        return false;
    }
    if std::env::var_os("CLICOLOR_FORCE").is_some_and(|v| !v.is_empty() && v != "0") {
        return true;
    }
    s.is_terminal()
}

/// The ANSI palette to use. Every field is an escape sequence or an empty
/// string, so callers can interpolate it without checking.
#[derive(Clone, Copy)]
pub struct Palette {
    pub reset: &'static str,
    pub bold: &'static str,
    pub dim: &'static str,
    pub(crate) red: &'static str,
    pub(crate) green: &'static str,
    pub(crate) yellow: &'static str,
    pub(crate) magenta: &'static str,
    pub(crate) cyan: &'static str,
    pub(crate) blue: &'static str,
    pub error: &'static str,
    /// The brand colour (Scarlet, #AA091B), for the logo only and never for
    /// body text.
    pub scarlet: &'static str,
    link_open: &'static str,
    link_close: &'static str,
    enabled: bool,
}

const ON: Palette = Palette {
    reset: "\x1b[0m",
    bold: "\x1b[1m",
    dim: "\x1b[2m",
    red: "\x1b[31m",
    green: "\x1b[32m",
    yellow: "\x1b[33m",
    magenta: "\x1b[35m",
    cyan: "\x1b[36m",
    blue: "\x1b[34m",
    error: "\x1b[1;31m",
    scarlet: "\x1b[38;2;170;9;27m",
    link_open: "\x1b[4;36m",
    link_close: "\x1b[0m",
    enabled: true,
};

const OFF: Palette = Palette {
    reset: "",
    bold: "",
    dim: "",
    red: "",
    green: "",
    yellow: "",
    magenta: "",
    cyan: "",
    blue: "",
    error: "",
    scarlet: "",
    link_open: "",
    link_close: "",
    enabled: false,
};

const OSC8_OPEN: &str = "\x1b]8;;";
const OSC8_CLOSE: &str = "\x07";

impl Palette {
    fn for_stream(s: &impl IsTerminal) -> Self {
        if color_enabled(s) { ON } else { OFF }
    }

    pub fn for_stdout() -> Self {
        Self::for_stream(&std::io::stdout())
    }

    pub fn for_stderr() -> Self {
        Self::for_stream(&std::io::stderr())
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// The two palettes, for tests that must not depend on whether the test
    /// binary's stdout happens to be a terminal.
    #[cfg(test)]
    pub(crate) const fn ansi_for_test() -> Self {
        ON
    }

    #[cfg(test)]
    pub(crate) const fn plain_for_test() -> Self {
        OFF
    }

    /// Renders `text` as an OSC 8 terminal hyperlink to `url`, or returns
    /// `text` as it is when colour is off.
    pub fn hyperlink(&self, url: &str, text: &str) -> String {
        if self.enabled {
            format!(
                "{OSC8_OPEN}{url}{OSC8_CLOSE}{}{text}{}{OSC8_OPEN}{OSC8_CLOSE}",
                self.link_open, self.link_close
            )
        } else {
            text.to_string()
        }
    }
}
