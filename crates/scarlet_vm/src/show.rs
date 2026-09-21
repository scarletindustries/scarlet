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
use crate::code::Code;
use crate::heap::{Cell, Heap, Kind};
use crate::value::{Value, View};

/// Every type's names, by its id: [`scarlet_ir::core_ir::Program::types`].
pub(crate) type Types = BTreeMap<TypeId, TypeNames>;

/// A string this long or longer is not small: a constructor holding it takes
/// a line per field.
const SMALL_STRING: usize = 20;

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
            Piece::Line(n) => {
                out.push(b'\n');
                for _ in 0..n {
                    out.extend_from_slice(b"  ");
                }
            }
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
            None => return Err(Stop::NotBuiltYet("printing this value".into())),
        },
        View::Func(f) => function(code, f, out),
        View::Float(_) => return Err(Stop::NotBuiltYet("printing a Float".into())),
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
            Some(Kind::Ctor) | None => false,
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
