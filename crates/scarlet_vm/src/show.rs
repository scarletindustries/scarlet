//! How `println` and `${x}` show a value: the way it would be written.
//!
//! A constructor shows as `Some(1)`, and a record, a constructor named after
//! its own type, as `Point{ x: 1, y: 2 }`. One whose fields are all small
//! stays on one line; any other is laid out one field per line, indented:
//!
//! ```text
//! Seg {
//!   a: Point{ x: 1, y: 2 },
//!   b: Point{ x: 3, y: 4 }
//! }
//! ```
//!
//! The layout is the old VM's (`inspect.rs`), so a program prints what it
//! always printed. The walk is a list of pieces still to write rather than
//! recursion, so a value nested a million deep shows without overflowing the
//! stack.

use std::collections::BTreeMap;

use scarlet_ir::TypeId;
use scarlet_ir::core_ir::{FuncIdx, TypeNames, VariantNames, VariantRef};

use crate::Stop;
use crate::array::{self, Seq};
use crate::code::Code;
use crate::heap::{Cell, Heap, Kind};
use crate::value::{Value, View};

/// Every type's names, by its id: [`scarlet_ir::core_ir::Program::types`].
pub(crate) type Types = BTreeMap<TypeId, TypeNames>;

/// A string this long or longer is not small: a constructor holding it takes
/// a line per field.
const SMALL_STRING: usize = 20;

/// The widest a tuple of small values may be and still stay on one line.
const LINE: usize = 80;

enum Piece<'t> {
    Value(Value, Layout),
    Text(&'t str),
    /// A line break, then this many levels of indent.
    Line(usize),
}

#[derive(Clone, Copy)]
enum Layout {
    /// All on one line.
    Flat,
    /// Its fields may take a line each, one level past this indent.
    Open(usize),
}

/// Append `v`, shown, to `out`.
pub(crate) fn show(heap: &Heap, code: &Code, v: Value, out: &mut Vec<u8>) -> Result<(), Stop> {
    let mut todo = vec![Piece::Value(v, Layout::Open(0))];
    while let Some(piece) = todo.pop() {
        match piece {
            Piece::Text(s) => out.extend_from_slice(s.as_bytes()),
            Piece::Line(n) => line(out, n),
            Piece::Value(v, layout) => value(heap, code, v, layout, out, &mut todo)?,
        }
    }
    Ok(())
}

fn value<'t>(
    heap: &Heap,
    code: &'t Code,
    v: Value,
    layout: Layout,
    out: &mut Vec<u8>,
    todo: &mut Vec<Piece<'t>>,
) -> Result<(), Stop> {
    match v.view() {
        View::Int(n) => out.extend_from_slice(n.to_string().as_bytes()),
        View::Nil => out.extend_from_slice(b"Nil"),
        View::Bool(true) => out.extend_from_slice(b"True"),
        View::Bool(false) => out.extend_from_slice(b"False"),
        View::Nullary(variant) => match names(&code.types, variant) {
            Some((_, names)) => out.extend_from_slice(names.name.as_bytes()),
            None => unnamed(variant, out),
        },
        View::Cell(cell) => match heap.kind(cell) {
            Some(Kind::String) => heap.read_string(cell, out),
            Some(Kind::BigInt) => {
                out.extend_from_slice(heap.read_big_int(cell).to_string().as_bytes());
            }
            Some(Kind::Ctor) => ctor(heap, &code.types, cell, layout, out, todo),
            Some(Kind::Closure) => function(code, heap.closure_func(cell), out),
            Some(Kind::Tuple) => tuple(heap, code, cell, layout, out, todo)?,
            Some(Kind::ArrayRoot) => array(heap, code, cell, layout, out, todo)?,
            Some(Kind::Range) => range(heap, cell, layout, out)?,
            Some(Kind::ArrayLeaf | Kind::ArrayBranch) => {
                return Err(Stop::BadProgram(
                    "a piece of an array's tree held as a value".into(),
                ));
            }
            None => return Err(Stop::NotBuiltYet("printing this value".into())),
        },
        View::Func(f) => function(code, f, out),
        View::Float(f) => out.extend_from_slice(crate::float::text(f).as_bytes()),
    }
    Ok(())
}

/// A function shows as its name, `<fn#serve>`, whatever it captured.
fn function(code: &Code, f: FuncIdx, out: &mut Vec<u8>) {
    out.extend_from_slice(b"<fn#");
    match code.names.get(f) {
        Some(name) => out.extend_from_slice(name.as_bytes()),
        None => out.extend_from_slice(f.0.to_string().as_bytes()),
    }
    out.push(b'>');
}

/// Write the constructor's name and opening bracket now, and queue the rest:
/// each field, the separators between them, and the closing bracket. The
/// queue is a stack, so it is pushed last piece first.
fn ctor<'t>(
    heap: &Heap,
    types: &'t Types,
    cell: Cell,
    layout: Layout,
    out: &mut Vec<u8>,
    todo: &mut Vec<Piece<'t>>,
) {
    let variant = heap.variant(cell);
    let Some((ty, names)) = names(types, variant) else {
        return unnamed(variant, out);
    };
    let fields: Vec<Value> = heap.fields(cell).collect();
    let record =
        !names.fields.is_empty() && names.fields.len() == fields.len() && ty.name == names.name;
    let label = |i: usize| names.fields.get(i).map_or("", String::as_str);
    out.extend_from_slice(names.name.as_bytes());
    match layout {
        Layout::Open(n) if !fields.iter().all(|f| small(heap, *f)) => {
            let (open, close) = if record { (" {", "}") } else { ("(", ")") };
            out.extend_from_slice(open.as_bytes());
            todo.push(Piece::Text(close));
            todo.push(Piece::Line(n));
            for (i, f) in fields.iter().enumerate().rev() {
                todo.push(Piece::Value(*f, Layout::Open(n + 1)));
                if record {
                    todo.push(Piece::Text(": "));
                    todo.push(Piece::Text(label(i)));
                }
                todo.push(Piece::Line(n + 1));
                if i > 0 {
                    todo.push(Piece::Text(","));
                }
            }
        }
        Layout::Open(_) | Layout::Flat => {
            let (open, close) = if record { ("{ ", " }") } else { ("(", ")") };
            out.extend_from_slice(open.as_bytes());
            todo.push(Piece::Text(close));
            for (i, f) in fields.iter().enumerate().rev() {
                todo.push(Piece::Value(*f, Layout::Flat));
                if record {
                    todo.push(Piece::Text(": "));
                    todo.push(Piece::Text(label(i)));
                }
                if i > 0 {
                    todo.push(Piece::Text(", "));
                }
            }
        }
    }
}

/// A tuple: `(1, 'a')` on one line when every element is small and the line
/// fits, and one element per line otherwise.
fn tuple<'t>(
    heap: &Heap,
    code: &'t Code,
    cell: Cell,
    layout: Layout,
    out: &mut Vec<u8>,
    todo: &mut Vec<Piece<'t>>,
) -> Result<(), Stop> {
    let elements: Vec<Value> = heap.elements(cell).collect();
    let n = match layout {
        Layout::Flat => None,
        Layout::Open(_) if elements.is_empty() => None,
        Layout::Open(n) if elements.iter().all(|e| small(heap, *e)) => {
            // A small value holds no other value, so showing it here takes a
            // bounded amount of work and no recursion.
            let start = out.len();
            out.push(b'(');
            for (i, e) in elements.iter().enumerate() {
                if i > 0 {
                    out.extend_from_slice(b", ");
                }
                show(heap, code, *e, out)?;
            }
            out.push(b')');
            if out.len() - start <= LINE {
                return Ok(());
            }
            out.truncate(start);
            Some(n)
        }
        Layout::Open(n) => Some(n),
    };
    out.push(b'(');
    todo.push(Piece::Text(")"));
    match n {
        Some(n) => {
            todo.push(Piece::Line(n));
            for (i, e) in elements.iter().enumerate().rev() {
                todo.push(Piece::Value(*e, Layout::Open(n + 1)));
                todo.push(Piece::Line(n + 1));
                if i > 0 {
                    todo.push(Piece::Text(","));
                }
            }
        }
        None => {
            for (i, e) in elements.iter().enumerate().rev() {
                todo.push(Piece::Value(*e, Layout::Flat));
                if i > 0 {
                    todo.push(Piece::Text(", "));
                }
            }
        }
    }
    Ok(())
}

/// An array: `[1, 2, 3]` when it fits on a line, six to a line when it holds
/// only small values and does not fit, and one element per line otherwise.
fn array<'t>(
    heap: &Heap,
    code: &'t Code,
    cell: Cell,
    layout: Layout,
    out: &mut Vec<u8>,
    todo: &mut Vec<Piece<'t>>,
) -> Result<(), Stop> {
    let elements = array::elements(heap, cell);
    match layout {
        Layout::Open(_) if elements.is_empty() => out.extend_from_slice(b"[]"),
        Layout::Open(n) if elements.iter().all(|e| small(heap, *e)) => {
            six_to_a_line(elements.len(), n, out, |i, out| match elements.get(i) {
                Some(e) => show(heap, code, *e, out),
                None => Ok(()),
            })?;
        }
        Layout::Open(n) => {
            out.push(b'[');
            todo.push(Piece::Text("]"));
            todo.push(Piece::Line(n));
            for (i, e) in elements.iter().enumerate().rev() {
                todo.push(Piece::Value(*e, Layout::Open(n + 1)));
                todo.push(Piece::Line(n + 1));
                if i > 0 {
                    todo.push(Piece::Text(","));
                }
            }
        }
        Layout::Flat => {
            out.push(b'[');
            todo.push(Piece::Text("]"));
            for (i, e) in elements.iter().enumerate().rev() {
                todo.push(Piece::Value(*e, Layout::Flat));
                if i > 0 {
                    todo.push(Piece::Text(", "));
                }
            }
        }
    }
    Ok(())
}

/// A range, `0..3`, shows as the array of its elements: `[0, 1, 2]`. Each
/// element is written straight from the range's start, so none is built.
fn range(heap: &Heap, cell: Cell, layout: Layout, out: &mut Vec<u8>) -> Result<(), Stop> {
    let Some(Seq::Range { start, end }) = array::seq(heap, cell) else {
        return Err(Stop::BadProgram("a range cell that is not a range".into()));
    };
    let count = array::range_len(start, end);
    let item = |i: usize, out: &mut Vec<u8>| {
        let n = i128::from(start) + i as i128;
        out.extend_from_slice(n.to_string().as_bytes());
        Ok(())
    };
    let count = usize::try_from(count).unwrap_or(usize::MAX);
    match layout {
        Layout::Open(_) if count == 0 => {
            out.extend_from_slice(b"[]");
            Ok(())
        }
        Layout::Open(n) => six_to_a_line(count, n, out, item),
        Layout::Flat => {
            out.push(b'[');
            let r = (0..count).try_for_each(|i| {
                if i > 0 {
                    out.extend_from_slice(b", ");
                }
                item(i, out)
            });
            out.push(b']');
            r
        }
    }
}

/// `count` small elements, each written by `item`, on one line when they fit
/// in `LINE` columns, and six to a line, indented one past `n`, when they do
/// not. The one-line try stops as soon as it is too wide, so no element is
/// written more than twice.
fn six_to_a_line(
    count: usize,
    n: usize,
    out: &mut Vec<u8>,
    mut item: impl FnMut(usize, &mut Vec<u8>) -> Result<(), Stop>,
) -> Result<(), Stop> {
    let start = out.len();
    out.push(b'[');
    let mut fits = true;
    for i in 0..count {
        if i > 0 {
            out.extend_from_slice(b", ");
        }
        item(i, out)?;
        if out.len() - start > LINE {
            fits = false;
            break;
        }
    }
    if fits {
        out.push(b']');
        if out.len() - start <= LINE {
            return Ok(());
        }
    }
    out.truncate(start);
    out.push(b'[');
    line(out, n + 1);
    for i in 0..count {
        if i > 0 {
            out.extend_from_slice(b", ");
            if i % 6 == 0 {
                line(out, n + 1);
            }
        }
        item(i, out)?;
    }
    line(out, n);
    out.push(b']');
    Ok(())
}

/// A line break, then `n` levels of indent.
fn line(out: &mut Vec<u8>, n: usize) {
    out.push(b'\n');
    for _ in 0..n {
        out.extend_from_slice(b"  ");
    }
}

/// Whether `v` is small enough that a constructor holding only such values
/// stays on one line.
fn small(heap: &Heap, v: Value) -> bool {
    match v.view() {
        View::Int(_)
        | View::Float(_)
        | View::Nil
        | View::Bool(_)
        | View::Func(_)
        | View::Nullary(_) => true,
        View::Cell(cell) => match heap.kind(cell) {
            Some(Kind::String) => heap.string_len(cell) < SMALL_STRING,
            Some(Kind::BigInt | Kind::Closure) => true,
            Some(
                Kind::Ctor
                | Kind::Tuple
                | Kind::ArrayRoot
                | Kind::ArrayLeaf
                | Kind::ArrayBranch
                | Kind::Range,
            )
            | None => false,
        },
    }
}

fn names(types: &Types, v: VariantRef) -> Option<(&TypeNames, &VariantNames)> {
    let ty = types.get(&v.type_id)?;
    Some((ty, ty.variants.get(usize::from(v.variant_idx))?))
}

/// A constructor the program has no names for, which only a compiler that
/// broke its promise (`Program::types`) can give. Shown by number, so a
/// wrong answer is visible rather than a stop.
fn unnamed(v: VariantRef, out: &mut Vec<u8>) {
    out.extend_from_slice(format!("<ctor {}.{}>", v.type_id.0, v.variant_idx).as_bytes());
}
