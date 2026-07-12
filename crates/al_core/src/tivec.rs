//! Typed indices ([`Idx`]) and the index-typed vector that consumes them
//! ([`TiVec`]).
//!
//! The compiler juggles half a dozen `u32`-shaped operand spaces — local ids,
//! constant-pool slots, function indices, code addresses — and every one of
//! them used to be a bare integer. Nothing stopped a `ConstId` being passed
//! where a `FuncIdx` was wanted, or a jump operand being used to index
//! `program.functions`. `TiVec<I, T>` closes that hole from the container
//! side: the *only* way to subscript one is with its own index type, so a
//! mismatch is a type error rather than a wrong-but-plausible instruction.
//!
//! [`TiVec::push`] returns the index it just filled, and [`TiVec::next_idx`]
//! names the index a push *would* fill. Together they replace the
//! `let i = v.len() as I; v.push(x)` idiom that silently breaks whenever a
//! caller hands over a vector numbered from somewhere other than zero.

use std::fmt;
use std::hash::Hash;
use std::iter::FromIterator;
use std::marker::PhantomData;
use std::ops::{Index, IndexMut};

/// A dense index into a [`TiVec`]. Implementors are newtypes over an unsigned
/// integer; the round-trip `Idx::from_usize(i).index() == i` must hold for
/// every `i` a program can actually allocate.
pub trait Idx: Copy + Eq + Ord + Hash + fmt::Debug {
    fn from_usize(i: usize) -> Self;
    fn index(self) -> usize;
}

/// `Vec<T>` that can only be subscripted by `I`.
///
/// The `PhantomData<fn(I) -> I>` makes `TiVec` invariant in `I` — irrelevant
/// for the concrete newtype indices used here — while neither borrowing nor
/// owning an `I`, so `TiVec` inherits `Send`/`Sync`/auto-traits from `T`
/// alone.
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

    /// The index [`Self::push`] would return. Minting an index this way — not
    /// from `len() as u32` at a call site that may not own the whole table — is
    /// what keeps producer and consumer numbering in step.
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

    /// The elements at `start..`, as a plain read-only slice — for consumers
    /// that walk a suffix (e.g. everything appended past a base index).
    /// Returning `&[T]` erases the typed index, but a shared slice can
    /// neither grow the table nor replace an entry, so no index is minted or
    /// misdirected through it.
    pub fn tail_from(&self, start: I) -> &[T] {
        &self.raw[start.index()..]
    }

    /// Drop the typed-index wrapper. Used at the boundaries where a plain
    /// `Vec` is what the consumer (the VM's `Program`, a test) wants.
    pub fn into_vec(self) -> Vec<T> {
        self.raw
    }

    /// Grow with `fill` until `i` is a valid index. No-op when it already is.
    /// The grow-on-demand idiom for a sparse dense-index table, hoisted out of
    /// its call sites so the `+ 1` is written once.
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

impl<I: Idx, T> From<Vec<T>> for TiVec<I, T> {
    fn from(raw: Vec<T>) -> Self {
        TiVec {
            raw,
            _idx: PhantomData,
        }
    }
}

impl<I: Idx, T> FromIterator<T> for TiVec<I, T> {
    fn from_iter<It: IntoIterator<Item = T>>(it: It) -> Self {
        TiVec {
            raw: it.into_iter().collect(),
            _idx: PhantomData,
        }
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
///
/// A macro rather than four hand-written copies because the *whole point* of
/// these types is that they behave identically and are not interchangeable: a
/// hand-written `index()` that forgot a cast would reintroduce exactly the
/// class of bug the newtype exists to remove.
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

    /// The point of the whole module: `B` cannot subscript a `TiVec<A, _>`.
    /// Enforced by the type checker, so all this test can do is pin the two
    /// index spaces as distinct types with the same representation.
    #[test]
    fn distinct_index_spaces_do_not_unify() {
        let a = A(7);
        let b = B(7);
        assert_eq!(a.index(), b.index());
        assert_eq!(format!("{a} {b}"), "a7 b7");
        // `let _: A = b;` does not compile — that is the guard.
    }
}
