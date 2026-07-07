use std::env;
use std::path::Path;
use std::process::Command;

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Editor {
    Vscode,
    VscodeInsiders,
    Vscodium,
    Cursor,
    Sublime,
    Atom,
    Jetbrains,
    Zed,
}

// Lowercase substring patterns for $VISUAL/$EDITOR, which may contain a full
// path and/or flags (e.g. `code --wait`). Ordered most-specific-first so that
// e.g. "insiders" wins before bare "code".
const ENV_PATTERNS: &[(&str, Editor)] = &[
    ("insiders", Editor::VscodeInsiders),
    ("codium", Editor::Vscodium),
    ("code", Editor::Vscode),
    ("cursor", Editor::Cursor),
    ("subl", Editor::Sublime),
    ("atom", Editor::Atom),
    ("intellij", Editor::Jetbrains),
    ("idea", Editor::Jetbrains),
    ("webstorm", Editor::Jetbrains),
    ("phpstorm", Editor::Jetbrains),
    ("pycharm", Editor::Jetbrains),
    ("rubymine", Editor::Jetbrains),
    ("goland", Editor::Jetbrains),
    ("rider", Editor::Jetbrains),
    ("clion", Editor::Jetbrains),
    ("zed", Editor::Zed),
];

fn match_env_var(value: &str) -> Option<Editor> {
    let lower = value.to_lowercase();
    ENV_PATTERNS
        .iter()
        .find(|(pat, _)| lower.contains(pat))
        .map(|(_, e)| *e)
}

/// Match a `ps -o comm=` line by its exact lowercase basename against a fixed
/// allow-list. Substring matching here would false-positive on unrelated
/// processes (`encoder`, `barcode`, `xcode` all contain "code").
fn match_process_name(comm: &str) -> Option<Editor> {
    let base = Path::new(comm.trim()).file_name()?.to_str()?.to_lowercase();
    match base.as_str() {
        "code" => Some(Editor::Vscode),
        "code-insiders" | "code - insiders" => Some(Editor::VscodeInsiders),
        "codium" | "vscodium" => Some(Editor::Vscodium),
        "cursor" => Some(Editor::Cursor),
        "subl" | "sublime_text" | "sublime text" => Some(Editor::Sublime),
        "atom" => Some(Editor::Atom),
        "zed" => Some(Editor::Zed),
        "idea" | "intellij" | "webstorm" | "phpstorm" | "pycharm" | "rubymine" | "goland"
        | "rider" | "clion" => Some(Editor::Jetbrains),
        _ => None,
    }
}

pub fn detect_editor() -> Option<Editor> {
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
        Editor::Jetbrains => format!("idea://open?file={path}&line={line}&column={col}"),
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
    fn url_path_is_percent_encoded() {
        let url = build_editor_url(Editor::Vscode, "/Users/me/My Project/a#b.al", 3, 7);
        assert_eq!(url, "vscode://file/Users/me/My%20Project/a%23b%2Eal:3:7");
        let url = build_editor_url(Editor::Jetbrains, "/tmp/a b?.al", 1, 1);
        assert_eq!(url, "idea://open?file=/tmp/a%20b%3F%2Eal&line=1&column=1");
    }
}
