use std::env;
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

/// A JetBrains IDE, identified by the URL scheme its protocol handler
/// registers (`idea://`, `pycharm://`, ...). Captured at detection time so
/// links open in the product that is actually running.
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

/// Exact lowercase executable basename → editor, shared by both detectors.
/// Substring matching would false-positive on unrelated names (`encoder`,
/// `barcode`, `xcode` all contain "code").
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

/// Match a $VISUAL/$EDITOR value, which may contain a full path and/or flags.
/// Three forms, tried in order:
/// - quoted command with flags: `"/Applications/.../bin/code" -w` — the
///   quoted span is the command;
/// - unquoted command with flags: `/usr/local/bin/code --wait` — the first
///   whitespace token is the command;
/// - unquoted flag-free path containing spaces:
///   `/Applications/Sublime Text.app/.../bin/subl` — the whole value is the
///   command, matched as a fallback.
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

/// Match a `ps -o comm=` line by its exact lowercase basename. No whitespace
/// split here: comm values are argument-free but may themselves contain
/// spaces (`sublime text`, `code - insiders`).
fn match_process_name(comm: &str) -> Option<Editor> {
    match_basename(comm.trim())
}

/// Detect the user's editor once per process; `ps` is forked at most once.
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

/// Everything except `/` — over-encoding is harmless, but `/` must survive so
/// `scheme://file/abs/path` keeps its path segments and the editor's URL handler
/// sees the intended authority/path split.
const PATH_ENCODE: &AsciiSet = &NON_ALPHANUMERIC.remove(b'/');

pub fn build_editor_url(editor: Editor, abs_path: &str, line: i32, col: i32) -> String {
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
mod tests {
    use super::*;

    #[test]
    fn process_name_exact_match() {
        assert_eq!(match_process_name("code"), Some(Editor::Vscode));
        assert_eq!(
            match_process_name("/usr/local/bin/code"),
            Some(Editor::Vscode)
        );
        assert_eq!(match_process_name("Zed"), Some(Editor::Zed));
        // Substring false-positives that the old contains-based matcher hit:
        assert_eq!(match_process_name("encoder"), None);
        assert_eq!(match_process_name("barcode-scanner"), None);
        assert_eq!(match_process_name("Xcode"), None);
        assert_eq!(
            match_process_name("/Applications/Xcode.app/Contents/MacOS/Xcode"),
            None
        );
    }

    #[test]
    fn env_var_exact_basename_match() {
        assert_eq!(match_env_var("code"), Some(Editor::Vscode));
        assert_eq!(
            match_env_var("/usr/local/bin/code --wait"),
            Some(Editor::Vscode)
        );
        assert_eq!(match_env_var("PyCharm"), Some(jetbrains("pycharm")));
        // Unquoted flag-free macOS path containing spaces:
        assert_eq!(
            match_env_var("/Applications/Sublime Text.app/Contents/SharedSupport/bin/subl"),
            Some(Editor::Sublime)
        );
        // Quoted macOS path with flags:
        assert_eq!(
            match_env_var(
                "\"/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code\" -w"
            ),
            Some(Editor::Vscode)
        );
        assert_eq!(
            match_env_var("'/Applications/Sublime Text.app/Contents/SharedSupport/bin/subl' -w"),
            Some(Editor::Sublime)
        );
        // Substring false-positives that the old contains-based matcher hit:
        assert_eq!(match_env_var("encoder"), None);
        assert_eq!(match_env_var("/opt/xcode/bin/edit"), None);
        assert_eq!(match_env_var("nvim"), None);
    }

    #[test]
    fn url_path_is_percent_encoded() {
        let url = build_editor_url(Editor::Vscode, "/Users/me/My Project/a#b.al", 3, 7);
        assert_eq!(url, "vscode://file/Users/me/My%20Project/a%23b%2Eal:3:7");
        let url = build_editor_url(jetbrains("idea"), "/tmp/a b?.al", 1, 1);
        assert_eq!(url, "idea://open?file=/tmp/a%20b%3F%2Eal&line=1&column=1");
    }

    #[test]
    fn jetbrains_url_uses_detected_product_scheme() {
        let url = build_editor_url(jetbrains("pycharm"), "/tmp/a.al", 2, 3);
        assert_eq!(url, "pycharm://open?file=/tmp/a%2Eal&line=2&column=3");
        let url = build_editor_url(jetbrains("clion"), "/tmp/a.al", 2, 3);
        assert_eq!(url, "clion://open?file=/tmp/a%2Eal&line=2&column=3");
    }
}
