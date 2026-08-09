//! Typed indices ([`Idx`]) and the vector that only they can subscript
//! ([`TiVec`]). Keeps the compiler's many `u32` operand spaces (local ids,
//! constant slots, function indices, code addresses) from mixing.

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

/// `Vec<T>` that can only be subscripted by `I`.
///
/// The `fn(I) -> I` phantom neither owns nor borrows an `I`, so auto-traits
/// come from `T` alone.
pub struct TiVec<I: Idx, T> {
    raw: Vec<T>,
    _idx: PhantomData<fn(I) -> I>,
}

impl<I: Idx, T> TiVec<I, T> {
    pub fn new() -> Self {
        TiVec {
            raw: Vec::new(),
            _idx: PhantomData,
        }
    }

    pub fn len(&self) -> usize {
        self.raw.len()
    }

    pub fn is_empty(&self) -> bool {
        self.raw.is_empty()
    }

    /// The index [`Self::push`] would return. Mint indices here, not from
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

    pub fn get(&self, i: I) -> Option<&T> {
        self.raw.get(i.index())
    }

    pub fn get_mut(&mut self, i: I) -> Option<&mut T> {
        self.raw.get_mut(i.index())
    }

    /// True when `i` names an element of this vector.
    fn contains_idx(&self, i: I) -> bool {
        i.index() < self.raw.len()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, T> {
        self.raw.iter()
    }

    pub fn as_slice(&self) -> &[T] {
        &self.raw
    }

    /// The elements at `start..`, for consumers that walk a suffix.
    pub fn tail_from(&self, start: I) -> &[T] {
        &self.raw[start.index()..]
    }

    /// Drop the typed-index wrapper, for consumers that want a plain `Vec`.
    pub fn into_vec(self) -> Vec<T> {
        self.raw
    }

    /// Grow with `fill` until `i` is a valid index. No-op when it already is.
    pub fn resize_at_least(&mut self, i: I, fill: T)
    where
        T: Clone,
    {
        if !self.contains_idx(i) {
            self.raw.resize(i.index() + 1, fill);
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
    fn resize_at_least_grows_only_when_the_index_is_out_of_range() {
        let mut v: TiVec<A, u8> = TiVec::new();
        assert!(!v.contains_idx(A(2)));
        v.resize_at_least(A(2), 9);
        assert!(v.contains_idx(A(2)));
        assert_eq!(v.as_slice(), &[9, 9, 9]);
        v[A(0)] = 1;
        v.resize_at_least(A(1), 9);
        assert_eq!(v.as_slice(), &[1, 9, 9]);
    }

    /// `B` cannot subscript a `TiVec<A, _>`. The type checker enforces that, so
    /// this only pins the two spaces as distinct types with one representation.
    #[test]
    fn distinct_index_spaces_do_not_unify() {
        let a = A(7);
        let b = B(7);
        assert_eq!(a.index(), b.index());
        assert_eq!(format!("{a} {b}"), "a7 b7");
        // `let _: A = b;` does not compile — that is the guard.
    }
}
