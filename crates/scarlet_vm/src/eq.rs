//! `==`, which is structural equality. It works as the old VM's
//! `values_equal` did (that lived in `bytecode/value.rs`, which is now gone).
//!
//! Two values are equal when they hold the same thing: the same number,
//! string, Bool or `Nil`; the same constructor with equal fields; tuples or
//! arrays with equal elements, in order; the same function over equal
//! captures; maps with the same keys, each bound to equal values. A range is
//! an array of its Ints, so `0..3 == [0, 1, 2]`.
//!
//! The walk keeps a list of pairs still to compare instead of recursing, so
//! two very long lists compare without overflowing the stack. It stops at the
//! first pair that differs.

use crate::array::{self, Seq};
use crate::binary;
use crate::heap::{Cell, Heap, Kind};
use crate::map;
use crate::value::{Value, View};

/// Whether `a == b`.
pub(crate) fn equal(heap: &Heap, a: Value, b: Value) -> bool {
    // Empty until a pair holds other values, so comparing two that hold none,
    // like two Ints or two strings, does not allocate.
    let mut todo = Vec::new();
    if !pair(heap, a, b, &mut todo) {
        return false;
    }
    while let Some((a, b)) = todo.pop() {
        if !pair(heap, a, b, &mut todo) {
            return false;
        }
    }
    true
}

/// Compare one pair. Decide it now, or queue the values it holds.
fn pair(heap: &Heap, a: Value, b: Value, todo: &mut Vec<(Value, Value)>) -> bool {
    // Every value but a Float has only one form, so the same word means the
    // same value. A Float compares as a number, so its two zeros are equal.
    if a.bits() == b.bits() {
        return true;
    }
    match (a.view(), b.view()) {
        (View::Float(x), View::Float(y)) => x == y,
        (View::Cell(x), View::Cell(y)) => cells(heap, x, y, todo),
        // Immediates, small Ints and functions with no captures are equal
        // only when they are the same word, which the check above decided.
        // A small Int never equals a big one: a number has one form. Neither
        // does the empty array equal a cell, because no array's cell is empty.
        (
            View::Float(_)
            | View::Int(_)
            | View::Nil
            | View::Bool(_)
            | View::EmptyArray
            | View::Func(_)
            | View::Nullary(_)
            | View::Cell(_),
            _,
        ) => false,
    }
}

fn cells(heap: &Heap, x: Cell, y: Cell, todo: &mut Vec<(Value, Value)>) -> bool {
    let kx = heap.kind(x);
    if matches!(kx, Some(Kind::ArrayFlat | Kind::ArrayRoot | Kind::Range)) {
        return arrays(heap, Value::cell(x), Value::cell(y), todo);
    }
    // A slice equals a binary holding the same bits.
    if let (Some(a), Some(b)) = (binary::bits(heap, x), binary::bits(heap, y)) {
        return binary::equal(heap, a, b);
    }
    let (Some(kx), Some(ky)) = (kx, heap.kind(y)) else {
        return false;
    };
    if kx != ky {
        return false;
    }
    match kx {
        Kind::String => {
            let mut text = Vec::with_capacity(heap.string_len(x));
            heap.read_string(x, &mut text);
            heap.string_is(y, &text)
        }
        Kind::BigInt => heap.read_big_int(x) == heap.read_big_int(y),
        Kind::Ctor => {
            heap.ctor_of(x) == heap.ctor_of(y)
                && queue(todo, heap.values(x).collect(), heap.values(y).collect())
        }
        // A closure's first value is its function.
        Kind::Closure | Kind::Tuple => {
            queue(todo, heap.values(x).collect(), heap.values(y).collect())
        }
        // A handle is the same one only when the host's number is.
        Kind::Handle => heap.data(x) == heap.data(y),
        Kind::Map | Kind::MapNode | Kind::MapCollision => match map::equal_parts(heap, x, y) {
            Some(pairs) => {
                todo.extend(pairs.into_iter().rev());
                true
            }
            None => false,
        },
        // Arrays and binaries were compared above; a tree's inner nodes are
        // never values.
        Kind::ArrayFlat
        | Kind::ArrayRoot
        | Kind::ArrayBranch
        | Kind::Range
        | Kind::Binary
        | Kind::BinarySlice => false,
    }
}

/// Two arrays, each in any form. Two values that are not both arrays differ.
fn arrays(heap: &Heap, a: Value, b: Value, todo: &mut Vec<(Value, Value)>) -> bool {
    let (Some(a), Some(b)) = (array::seq(heap, a), array::seq(heap, b)) else {
        return false;
    };
    if a.len(heap) != b.len(heap) {
        return false;
    }
    match (a, b) {
        // They are the same length, so both are empty.
        (Seq::Empty, _) | (_, Seq::Empty) => true,
        (Seq::Range { start: x, .. }, Seq::Range { start: y, .. }) => x == y,
        (Seq::Flat(f), Seq::Range { start, end }) | (Seq::Range { start, end }, Seq::Flat(f)) => {
            are_ints(heap, f.items(heap), start..end)
        }
        (Seq::Tree(t), Seq::Range { start, end }) | (Seq::Range { start, end }, Seq::Tree(t)) => {
            are_ints(heap, array::elements_of_tree(heap, t), start..end)
        }
        (Seq::Flat(x), Seq::Flat(y)) => {
            todo.extend(x.items(heap).zip(y.items(heap)).rev());
            true
        }
        (Seq::Flat(f), Seq::Tree(t)) => {
            let all = f.items(heap).collect();
            queue(todo, all, array::elements_of_tree(heap, t))
        }
        (Seq::Tree(t), Seq::Flat(f)) => {
            let all = f.items(heap).collect();
            queue(todo, array::elements_of_tree(heap, t), all)
        }
        (Seq::Tree(x), Seq::Tree(y)) => {
            let (x, y) = (
                array::elements_of_tree(heap, x),
                array::elements_of_tree(heap, y),
            );
            queue(todo, x, y)
        }
    }
}

/// Whether `items`, of which there are as many, are `ints`.
fn are_ints(
    heap: &Heap,
    items: impl IntoIterator<Item = Value>,
    ints: std::ops::Range<i64>,
) -> bool {
    let mut pairs = items.into_iter().zip(ints);
    pairs.all(|(v, n)| is_int(heap, v, i128::from(n)))
}

/// Whether `v` is the Int `n`, small or big.
fn is_int(heap: &Heap, v: Value, n: i128) -> bool {
    match v.view() {
        View::Int(m) => i128::from(m) == n,
        View::Cell(cell) if heap.is(cell, Kind::BigInt) => heap.read_big_int(cell) == n.into(),
        View::Float(_)
        | View::Nil
        | View::Bool(_)
        | View::EmptyArray
        | View::Func(_)
        | View::Cell(_)
        | View::Nullary(_) => false,
    }
}

/// Queue `a` and `b` in pairs with the first pair on top. Returns false if
/// they differ in length.
fn queue(todo: &mut Vec<(Value, Value)>, a: Vec<Value>, b: Vec<Value>) -> bool {
    if a.len() != b.len() {
        return false;
    }
    todo.extend(a.into_iter().zip(b).rev());
    true
}

#[cfg(test)]
mod tests;
