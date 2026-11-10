//! Typed indices ([`Idx`]) and the vector that only they can subscript
//! ([`TiVec`]). They keep the compiler's many `u32` index spaces, such as local
//! ids, constant slots and function indices, from mixing.

use std::fmt;
use std::hash::Hash;
use std::marker::PhantomData;
use std::ops::{Index, IndexMut};

/// A dense index into a [`TiVec`]. `Idx::from_usize(i).index() == i` must hold
/// for every `i` a program can allocate.
pub trait Idx: Copy + Eq + Ord + Hash + fmt::Debug {
    fn from_usize(i: usize) -> Self;
    fn index(self) -> usize;
}

/// A `Vec<T>` that only `I` can subscript.
///
/// The `fn(I) -> I` phantom neither owns nor borrows an `I`, so auto traits
/// like `Send` come from `T` alone.
pub struct TiVec<I: Idx, T> {
    raw: Vec<T>,
    _idx: PhantomData<fn(I) -> I>,
}

// There's no `is_empty` because nothing in the workspace needs it, and hawk
// fails on unused public items. Clippy would otherwise insist it comes with
// `len`.
#[allow(clippy::len_without_is_empty)]
impl<I: Idx, T> TiVec<I, T> {
    pub fn new() -> Self {
        TiVec {
            raw: Vec::new(),
            _idx: PhantomData,
        }
    }

    /// The index [`Self::push`] would return. Make new indices here, not from
    /// `len() as u32` at a call site that may not own the whole table.
    pub fn next_idx(&self) -> I {
        I::from_usize(self.raw.len())
    }

    /// Append `v` and return the index it landed at.
    pub fn push(&mut self, v: T) -> I {
        let i = self.next_idx();
        self.raw.push(v);
        i
    }

    /// The element at `i`, or `None` past the end.
    pub fn get(&self, i: I) -> Option<&T> {
        self.raw.get(i.index())
    }

    #[cfg(test)]
    fn as_slice(&self) -> &[T] {
        &self.raw
    }

    /// Drop every element at `len` and after. Does nothing when
    /// `len >= self.len()`.
    #[cfg(test)]
    fn truncate(&mut self, len: usize) {
        self.raw.truncate(len);
    }
}

/// Collecting gives the `n`th element index `n`.
impl<I: Idx, T> FromIterator<T> for TiVec<I, T> {
    fn from_iter<It: IntoIterator<Item = T>>(iter: It) -> Self {
        TiVec {
            raw: iter.into_iter().collect(),
            _idx: PhantomData,
        }
    }
}

impl<I: Idx, T> Default for TiVec<I, T> {
    fn default() -> Self {
        TiVec::new()
    }
}

impl<I: Idx, T: Clone> Clone for TiVec<I, T> {
    fn clone(&self) -> Self {
        TiVec {
            raw: self.raw.clone(),
            _idx: PhantomData,
        }
    }
}

impl<I: Idx, T: fmt::Debug> fmt::Debug for TiVec<I, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.raw, f)
    }
}

impl<I: Idx, T> Index<I> for TiVec<I, T> {
    type Output = T;
    fn index(&self, i: I) -> &T {
        &self.raw[i.index()]
    }
}

impl<I: Idx, T> IndexMut<I> for TiVec<I, T> {
    fn index_mut(&mut self, i: I) -> &mut T {
        &mut self.raw[i.index()]
    }
}

impl<I: Idx, T> IntoIterator for TiVec<I, T> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;
    fn into_iter(self) -> Self::IntoIter {
        self.raw.into_iter()
    }
}

impl<'a, I: Idx, T> IntoIterator for &'a TiVec<I, T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;
    fn into_iter(self) -> Self::IntoIter {
        self.raw.iter()
    }
}

/// Declare a `pub struct Name(pub u32)` dense index with its [`Idx`] impl and
/// a `Display` of `"<prefix><n>"`.
#[macro_export]
macro_rules! newtype_index {
    ($(#[$m:meta])* $vis:vis struct $name:ident($prefix:literal)) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        $vis struct $name(pub u32);

        impl $crate::tivec::Idx for $name {
            #[inline]
            fn from_usize(i: usize) -> Self {
                assert!(i <= u32::MAX as usize, concat!(stringify!($name), " overflow"));
                $name(i as u32)
            }
            #[inline]
            fn index(self) -> usize {
                self.0 as usize
            }
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                write!(f, concat!($prefix, "{}"), self.0)
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    newtype_index!(struct A("a"));
    newtype_index!(struct B("b"));

    #[test]
    fn push_returns_the_index_it_filled() {
        let mut v: TiVec<A, &str> = TiVec::new();
        assert_eq!(v.next_idx(), A(0));
        let a = v.push("zero");
        let b = v.push("one");
        assert_eq!((a, b), (A(0), A(1)));
        assert_eq!(v[a], "zero");
        assert_eq!(v[b], "one");
        assert_eq!(v.next_idx(), A(2));
    }

    #[test]
    fn truncate_drops_the_suffix() {
        let mut v: TiVec<A, &str> = TiVec::new();
        v.push("a");
        v.push("b");
        v.push("c");
        v.truncate(1);
        assert_eq!(v.as_slice(), &["a"]);
        assert_eq!(v.next_idx(), A(1));
        v.truncate(1);
        assert_eq!(v.as_slice(), &["a"]);
        v.truncate(0);
        assert_eq!(v.as_slice(), &[] as &[&str]);
        assert_eq!(v.next_idx(), A(0));
    }

    /// `B` can't subscript a `TiVec<A, _>`. Rust's type checker enforces that,
    /// so this only checks the two spaces are distinct types with one
    /// representation.
    #[test]
    fn distinct_index_spaces_do_not_unify() {
        let a = A(7);
        let b = B(7);
        assert_eq!(a.index(), b.index());
        assert_eq!(format!("{a} {b}"), "a7 b7");
        // `let _: A = b;` doesn't compile, and that's the guard.
    }
}
