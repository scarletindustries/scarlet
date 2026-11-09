//! `scarlet --version` must name the compiler build: a canary stamp (when CI
//! rewrote VERSION) or `+git.<sha>` (for a source build). A bare `0.0.1` is the
//! T-530 hole.

use std::process::Command;

#[test]
fn version_names_the_compiler() {
    let out = Command::new(env!("CARGO_BIN_EXE_scarlet"))
        .env("NO_COLOR", "1")
        .arg("--version")
        .output()
        .expect("run scarlet --version");
    assert!(
        out.status.success(),
        "scarlet --version rc={:?} stderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    let stamp = text
        .split_whitespace()
        .nth(1)
        .unwrap_or("")
        .trim_end_matches(['\r', '\n']);
    assert!(
        stamp.contains("-canary.") || stamp.contains("+git."),
        "T-530: --version must name a canary or commit, got {stamp:?} from {text:?}"
    );
    if let Some(git) = stamp.split("+git.").nth(1) {
        let sha = git.split('.').next().unwrap_or(git);
        let head = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8(o.stdout).ok());
        if let Some(head) = head {
            let head = head.trim();
            assert!(
                head.starts_with(sha),
                "embedded {sha:?} is not a prefix of HEAD {head:?}"
            );
        }
    }
}
