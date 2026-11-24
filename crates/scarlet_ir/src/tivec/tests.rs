use super::*;

impl<I: Idx, T> TiVec<I, T> {
    fn as_slice(&self) -> &[T] {
        &self.raw
    }

    /// Drop every element at `len` and after. Does nothing when
    /// `len >= self.len()`.
    fn truncate(&mut self, len: usize) {
        self.raw.truncate(len);
    }
}

newtype_index!(struct A("a"));
newtype_index!(struct B("b"));

#[test]
fn push_returns_the_index_it_filled() {
    let mut v: TiVec<A, &str> = TiVec::new();
    assert_eq!(v.next_idx(), Ok(A(0)));
    let a = v.push("zero").unwrap();
    let b = v.push("one").unwrap();
    assert_eq!((a, b), (A(0), A(1)));
    assert_eq!(v.get(a), Some(&"zero"));
    assert_eq!(v.get(b), Some(&"one"));
    assert_eq!(v.next_idx(), Ok(A(2)));
}

#[test]
fn truncate_drops_the_suffix() {
    let mut v: TiVec<A, &str> = TiVec::new();
    v.push("a").unwrap();
    v.push("b").unwrap();
    v.push("c").unwrap();
    v.truncate(1);
    assert_eq!(v.as_slice(), &["a"]);
    assert_eq!(v.next_idx(), Ok(A(1)));
    v.truncate(1);
    assert_eq!(v.as_slice(), &["a"]);
    v.truncate(0);
    assert_eq!(v.as_slice(), &[] as &[&str]);
    assert_eq!(v.next_idx(), Ok(A(0)));
}

#[test]
fn an_index_past_u32_is_a_full_table() {
    assert_eq!(A::from_usize(u32::MAX as usize), Ok(A(u32::MAX)));
    assert_eq!(A::from_usize(u32::MAX as usize + 1), Err(TableFull));
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
