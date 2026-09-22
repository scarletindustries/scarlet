//! `==`: structural equality, as the old VM had it (`values_equal` in
//! `bytecode/value.rs` on master).
//!
//! Two values are equal when they hold the same thing: the same number,
//! string, Bool or `Nil`; the same constructor with equal fields; tuples or
//! arrays with equal elements, in order; the same function over equal
//! captures. A range is an array of its Ints, so `0..3 == [0, 1, 2]`.
//!
//! The walk is a list of pairs still to compare rather than recursion, so two
//! lists a million long compare without overflowing the stack. It stops at the
//! first pair that differs.

use crate::array::{self, Seq};
use crate::binary;
use crate::heap::{Cell, Heap, Kind};
use crate::value::{Value, View};

/// Whether `a == b`.
pub(crate) fn equal(heap: &Heap, a: Value, b: Value) -> bool {
    let mut todo = vec![(a, b)];
    while let Some((a, b)) = todo.pop() {
        if !pair(heap, a, b, &mut todo) {
            return false;
        }
    }
    true
}

/// Compare one pair: decide it now, or queue what it holds.
fn pair(heap: &Heap, a: Value, b: Value, todo: &mut Vec<(Value, Value)>) -> bool {
    // Every value but a Float has one form, so the same word is the same
    // value. A Float compares as a number, so its two zeros are equal.
    if a.bits() == b.bits() {
        return true;
    }
    match (a.view(), b.view()) {
        (View::Float(x), View::Float(y)) => x == y,
        (View::Cell(x), View::Cell(y)) => cells(heap, x, y, todo),
        // Immediates, small Ints and functions with no captures are equal
        // only when they are the same word, which the check above decided.
        // A small Int never equals a big one: a number has one form.
        (
            View::Float(_)
            | View::Int(_)
            | View::Nil
            | View::Bool(_)
            | View::Func(_)
            | View::Nullary(_)
            | View::Cell(_),
            _,
        ) => false,
    }
}

fn cells(heap: &Heap, x: Cell, y: Cell, todo: &mut Vec<(Value, Value)>) -> bool {
    if let (Some(a), Some(b)) = (array::seq(heap, x), array::seq(heap, y)) {
        return arrays(heap, a, b, todo);
    }
    // A slice equals a binary holding the same bits.
    if let (Some(a), Some(b)) = (binary::bits(heap, x), binary::bits(heap, y)) {
        return binary::equal(heap, a, b);
    }
    let (Some(kx), Some(ky)) = (heap.kind(x), heap.kind(y)) else {
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
            heap.variant(x) == heap.variant(y)
                && queue(todo, heap.fields(x).collect(), heap.fields(y).collect())
        }
        Kind::Closure => {
            heap.closure_func(x) == heap.closure_func(y)
                && queue(todo, captures(heap, x), captures(heap, y))
        }
        Kind::Tuple => queue(todo, heap.elements(x).collect(), heap.elements(y).collect()),
        // Arrays and binaries were compared above; a tree's inner nodes are
        // never values.
        Kind::ArrayRoot
        | Kind::ArrayLeaf
        | Kind::ArrayBranch
        | Kind::Range
        | Kind::Binary
        | Kind::BinarySlice => false,
    }
}

/// Two arrays, either of which may be a range.
fn arrays(heap: &Heap, a: Seq, b: Seq, todo: &mut Vec<(Value, Value)>) -> bool {
    if a.len(heap) != b.len(heap) {
        return false;
    }
    match (a, b) {
        (Seq::Range { start: x, .. }, Seq::Range { start: y, .. }) => x == y || a.len(heap) == 0,
        (Seq::Tree(t), Seq::Range { start, .. }) | (Seq::Range { start, .. }, Seq::Tree(t)) => {
            array::elements(heap, t)
                .into_iter()
                .zip(0i128..)
                .all(|(v, i)| is_int(heap, v, i128::from(start) + i))
        }
        (Seq::Tree(x), Seq::Tree(y)) => {
            queue(todo, array::elements(heap, x), array::elements(heap, y))
        }
    }
}

/// Whether `v` is the Int `n`, small or big.
fn is_int(heap: &Heap, v: Value, n: i128) -> bool {
    match v.view() {
        View::Int(m) => i128::from(m) == n,
        View::Cell(cell) if heap.kind(cell) == Some(Kind::BigInt) => {
            heap.read_big_int(cell) == n.into()
        }
        View::Float(_)
        | View::Nil
        | View::Bool(_)
        | View::Func(_)
        | View::Cell(_)
        | View::Nullary(_) => false,
    }
}

fn captures(heap: &Heap, closure: Cell) -> Vec<Value> {
    (0..).map_while(|i| heap.capture(closure, i)).collect()
}

/// Queue `a` and `b` pairwise, first pair on top, or say they differ in
/// length.
fn queue(todo: &mut Vec<(Value, Value)>, a: Vec<Value>, b: Vec<Value>) -> bool {
    if a.len() != b.len() {
        return false;
    }
    todo.extend(a.into_iter().zip(b).rev());
    true
}
