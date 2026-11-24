use std::env;
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

/// A JetBrains IDE, identified by the URL scheme its protocol handler
/// registers (`idea://`, `pycharm://`, ...).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JetbrainsIde {
    scheme: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Editor {
    Vscode,
    VscodeInsiders,
    Vscodium,
    Cursor,
    Sublime,
    Atom,
    Jetbrains(JetbrainsIde),
    Zed,
}

const fn jetbrains(scheme: &'static str) -> Editor {
    Editor::Jetbrains(JetbrainsIde { scheme })
}

/// Exact lowercase executable basename → editor. Matching must stay exact:
/// `encoder`, `barcode` and `xcode` all contain "code".
const EDITORS_BY_BASENAME: &[(&str, Editor)] = &[
    ("code", Editor::Vscode),
    ("code-insiders", Editor::VscodeInsiders),
    ("code - insiders", Editor::VscodeInsiders),
    ("codium", Editor::Vscodium),
    ("vscodium", Editor::Vscodium),
    ("cursor", Editor::Cursor),
    ("subl", Editor::Sublime),
    ("sublime_text", Editor::Sublime),
    ("sublime text", Editor::Sublime),
    ("atom", Editor::Atom),
    ("zed", Editor::Zed),
    ("idea", jetbrains("idea")),
    ("intellij", jetbrains("idea")),
    ("webstorm", jetbrains("webstorm")),
    ("phpstorm", jetbrains("phpstorm")),
    ("pycharm", jetbrains("pycharm")),
    ("rubymine", jetbrains("rubymine")),
    ("goland", jetbrains("goland")),
    ("rider", jetbrains("rider")),
    ("clion", jetbrains("clion")),
];

fn lookup_basename(base: &str) -> Option<Editor> {
    EDITORS_BY_BASENAME
        .iter()
        .find(|(name, _)| *name == base)
        .map(|(_, e)| *e)
}

fn match_basename(cmd: &str) -> Option<Editor> {
    let base = Path::new(cmd).file_name()?.to_str()?.to_lowercase();
    lookup_basename(&base)
}

/// Match a $VISUAL/$EDITOR value, which may carry a path and/or flags. The
/// whole value is retried as a fallback because a flag-free path can itself
/// contain spaces (`/Applications/Sublime Text.app/.../bin/subl`).
fn match_env_var(value: &str) -> Option<Editor> {
    let value = value.trim();
    let cmd = if let Some(rest) = value.strip_prefix('"') {
        rest.split('"').next()
    } else if let Some(rest) = value.strip_prefix('\'') {
        rest.split('\'').next()
    } else {
        value.split_whitespace().next()
    };
    cmd.and_then(match_basename)
        .or_else(|| match_basename(value))
}

/// Match a `ps -o comm=` line by its exact lowercase basename. Do not split on
/// whitespace: comm values take no arguments but may contain spaces
/// (`sublime text`, `code - insiders`).
fn match_process_name(comm: &str) -> Option<Editor> {
    match_basename(comm.trim())
}

/// Detects the user's editor once per process, so `ps` runs at most once.
pub fn detect_editor() -> Option<Editor> {
    static DETECTED: OnceLock<Option<Editor>> = OnceLock::new();
    *DETECTED.get_or_init(detect_editor_uncached)
}

fn detect_editor_uncached() -> Option<Editor> {
    for env_var in ["VISUAL", "EDITOR"] {
        if let Ok(editor) = env::var(env_var)
            && let Some(detected) = match_env_var(&editor)
        {
            return Some(detected);
        }
    }

    let ps_args: &[&str] = if cfg!(target_os = "macos") {
        &["x", "-o", "comm="]
    } else if cfg!(target_os = "linux") {
        &["x", "--no-heading", "-o", "comm"]
    } else {
        return None;
    };

    Command::new("ps")
        .args(ps_args)
        .output()
        .ok()
        .filter(|r| r.status.success())
        .and_then(|r| {
            String::from_utf8_lossy(&r.stdout)
                .lines()
                .find_map(match_process_name)
        })
}

/// Encode everything but `/`, which must survive so the editor's URL handler
/// still sees the path segments.
const PATH_ENCODE: &AsciiSet = &NON_ALPHANUMERIC.remove(b'/');

pub fn build_editor_url(editor: Editor, abs_path: &str, line: i32, col: usize) -> String {
    let path = utf8_percent_encode(abs_path, PATH_ENCODE);
    match editor {
        Editor::Vscode => format!("vscode://file{path}:{line}:{col}"),
        Editor::VscodeInsiders => format!("vscode-insiders://file{path}:{line}:{col}"),
        Editor::Vscodium => format!("vscodium://file{path}:{line}:{col}"),
        Editor::Cursor => format!("cursor://file{path}:{line}:{col}"),
        Editor::Sublime => format!("subl://open?url=file://{path}&line={line}&column={col}"),
        Editor::Atom => {
            format!("atom://core/open/file?filename={path}&line={line}&column={col}")
        }
        Editor::Jetbrains(ide) => {
            format!("{}://open?file={path}&line={line}&column={col}", ide.scheme)
        }
        Editor::Zed => format!("zed://file{path}:{line}:{col}"),
    }
}

#[cfg(test)]
mod tests;
