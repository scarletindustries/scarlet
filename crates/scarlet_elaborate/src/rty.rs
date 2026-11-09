//! Resolved types. The pool itself is part of the contract between the
//! compiler and the VM, so it lives in `scarlet_ir`. What's left here belongs
//! to the compiler alone.

pub use scarlet_ir::rty::{RSlice, RTy, ResolvedNode, ResolvedPool};

/// How many arguments a function type, constructor, or eta wrapper takes.
///
/// It's a newtype because it gets compared against other bare counts, and a
/// payload of the wrong width silently corrupts a heap value instead of
/// crashing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Arity(pub u16);

impl Arity {
    /// The arity of a parameter or field list.
    #[inline]
    #[allow(clippy::expect_used)] // earlier stages keep arities far below u16::MAX
    pub(crate) fn of<T>(items: &[T]) -> Self {
        Arity(u16::try_from(items.len()).expect("constructor arity exceeds u16"))
    }
}

impl std::fmt::Display for Arity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::Arity;

    #[test]
    fn arity_counts_the_items() {
        assert_eq!(Arity::of(&[1, 2]), Arity(2));
        assert_eq!(Arity::of::<u8>(&[]), Arity(0));
    }
}
