//! A stack-resident scratch buffer for assembling a replacement node's slots
//! before handing them to an arena builder.
//!
//! Path copying in [`super::seq`] and [`super::hamt`] rebuilds one node image
//! at a time, and every image has a statically known maximum width. `Buf<N>`
//! holds it inline, so rebuilding never touches the host allocator.
//!
//! It owns its `Value`s: `extend` increfs and the buffer's drop decrefs,
//! balanced against a builder's `store_child`.

use super::value::Value;
use std::mem::MaybeUninit;

/// Inline buffer of at most `N` owned `Value`s.
pub(super) struct Buf<const N: usize> {
    items: [MaybeUninit<Value>; N],
    len: usize,
}

impl<const N: usize> Buf<N> {
    #[inline]
    pub(super) fn new() -> Buf<N> {
        Buf {
            items: [const { MaybeUninit::uninit() }; N],
            len: 0,
        }
    }

    #[inline]
    pub(super) fn push(&mut self, v: Value) {
        debug_assert!(self.len < N);
        self.items[self.len].write(v);
        self.len += 1;
    }

    #[inline]
    pub(super) fn extend(&mut self, vs: &[Value]) {
        debug_assert!(self.len + vs.len() <= N);
        for v in vs {
            self.items[self.len].write(v.clone());
            self.len += 1;
        }
    }
}

#[allow(unsafe_code)]
impl<const N: usize> Drop for Buf<N> {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: exactly the first `len` slots were initialized by
        // `push`/`extend`, and each is dropped once here.
        for slot in &mut self.items[..self.len] {
            unsafe { slot.assume_init_drop() };
        }
    }
}

#[allow(unsafe_code)]
impl<const N: usize> std::ops::Deref for Buf<N> {
    type Target = [Value];
    #[inline]
    fn deref(&self) -> &[Value] {
        // SAFETY: the first `len` slots are initialized `Value`s and
        // `MaybeUninit<Value>` has the same layout as `Value`.
        unsafe { std::slice::from_raw_parts(self.items.as_ptr().cast::<Value>(), self.len) }
    }
}

#[allow(unsafe_code)]
impl<const N: usize> std::ops::DerefMut for Buf<N> {
    #[inline]
    fn deref_mut(&mut self) -> &mut [Value] {
        // SAFETY: as for `Deref`; unique borrow of `self` gives unique access.
        unsafe { std::slice::from_raw_parts_mut(self.items.as_mut_ptr().cast::<Value>(), self.len) }
    }
}
