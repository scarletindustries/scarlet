use std::io::IsTerminal;

/// Whether to emit ANSI color for `s`. `NO_COLOR` beats `CLICOLOR_FORCE`, which
/// beats the terminal check.
fn color_enabled(s: &impl IsTerminal) -> bool {
    if std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) {
        return false;
    }
    if std::env::var_os("CLICOLOR_FORCE").is_some_and(|v| !v.is_empty() && v != "0") {
        return true;
    }
    s.is_terminal()
}

/// Resolved ANSI palette. Every field is an escape sequence or the empty
/// string, so call sites interpolate without checking.
#[derive(Clone, Copy)]
pub struct Palette {
    pub reset: &'static str,
    pub bold: &'static str,
    pub dim: &'static str,
    pub red: &'static str,
    pub cyan: &'static str,
    pub blue: &'static str,
    pub error: &'static str,
    link_open: &'static str,
    link_close: &'static str,
    enabled: bool,
}

const ON: Palette = Palette {
    reset: "\x1b[0m",
    bold: "\x1b[1m",
    dim: "\x1b[2m",
    red: "\x1b[31m",
    cyan: "\x1b[36m",
    blue: "\x1b[34m",
    error: "\x1b[1;31m",
    link_open: "\x1b[4;36m",
    link_close: "\x1b[0m",
    enabled: true,
};

const OFF: Palette = Palette {
    reset: "",
    bold: "",
    dim: "",
    red: "",
    cyan: "",
    blue: "",
    error: "",
    link_open: "",
    link_close: "",
    enabled: false,
};

const OSC8_OPEN: &str = "\x1b]8;;";
const OSC8_CLOSE: &str = "\x07";

impl Palette {
    pub fn for_stream(s: &impl IsTerminal) -> Self {
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

    /// Render `text` as an OSC 8 terminal hyperlink to `url`. Returns `text`
    /// bare when color is off.
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
