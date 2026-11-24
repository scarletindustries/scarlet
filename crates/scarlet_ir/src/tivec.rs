//! Typed indices ([`Idx`]) and the vector that only they can subscript
//! ([`TiVec`]). They keep the compiler's many `u32` index spaces, such as local
//! ids, constant slots and function indices, from mixing.

use std::fmt;
use std::hash::Hash;
use std::marker::PhantomData;

/// A dense index into a [`TiVec`]. `Idx::from_usize(i)?.index() == i` must
/// hold whenever `from_usize` succeeds.
pub trait Idx: Copy + Eq + Ord + Hash + fmt::Debug {
    /// Why an index can't be made: [`TableFull`] for one narrower than
    /// `usize`, and [`std::convert::Infallible`] for one as wide, since no
    /// `Vec` is longer than `usize::MAX`.
    type Full: fmt::Debug;
    fn from_usize(i: usize) -> Result<Self, Self::Full>;
    fn index(self) -> usize;
}

/// A table already holds as many entries as its index type can name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableFull;

impl fmt::Display for TableFull {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a table holds as many entries as its 32-bit ids can name")
    }
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
    pub fn next_idx(&self) -> Result<I, I::Full> {
        I::from_usize(self.raw.len())
    }

    /// Append `v` and return the index it landed at. When the index type
    /// can't name another entry, `v` isn't added.
    pub fn push(&mut self, v: T) -> Result<I, I::Full> {
        let i = self.next_idx()?;
        self.raw.push(v);
        Ok(i)
    }

    /// The element at `i`, or `None` past the end.
    pub fn get(&self, i: I) -> Option<&T> {
        self.raw.get(i.index())
    }

    /// The element at `i` for writing, or `None` past the end.
    pub fn get_mut(&mut self, i: I) -> Option<&mut T> {
        self.raw.get_mut(i.index())
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
            type Full = $crate::tivec::TableFull;
            #[inline]
            fn from_usize(i: usize) -> Result<Self, $crate::tivec::TableFull> {
                u32::try_from(i).map($name).map_err(|_| $crate::tivec::TableFull)
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
mod tests;
