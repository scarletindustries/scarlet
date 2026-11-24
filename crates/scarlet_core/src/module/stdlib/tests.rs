use super::STD;
use crate::formatter::{FormatResult, format};

/// People read the stdlib to learn how Scarlet is written, so it stays
/// exactly as `scarlet fmt` writes it.
#[test]
fn the_stdlib_is_formatted() {
    let unformatted: Vec<String> = STD
        .find("**/*.sca")
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.as_file())
        .filter(|file| {
            let src = file.contents_utf8().unwrap_or_default();
            !matches!(format(src), FormatResult::Formatted { output } if output == src)
        })
        .map(|file| file.path().display().to_string())
        .collect();
    assert!(
        unformatted.is_empty(),
        "not formatted; run `scarlet fmt crates/scarlet_core/src/std`: {unformatted:?}"
    );
}
