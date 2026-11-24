use super::{OFF, ON, Palette};

/// The two palettes, for tests that must not depend on whether the test
/// binary's stdout happens to be a terminal.
impl Palette {
    pub(crate) const fn ansi_for_test() -> Self {
        ON
    }

    pub(crate) const fn plain_for_test() -> Self {
        OFF
    }
}
