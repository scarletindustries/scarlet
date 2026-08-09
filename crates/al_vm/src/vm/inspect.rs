//! Value rendering: the `inspect` form every `Print`/`ToString` and the
//! CLI/REPL result line share, plus the type names error messages use.
//!
//! `inspect` renders a value the way the user would write it: records as
//! `Name{field: v}`, variants as `Name(v)`, strings quoted, floats with
//! a trailing `.0` so they never read as ints. It is layout-aware —
//! short simple arrays stay flat, long ones wrap six to a line, nested
//! aggregates expand one element per line — and the unit tests in
//! [`super`] lock that shape.
//!
//! Everything here is read-only over the value graph: no allocation in
//! any arena, just one host `String`. The whole rendering streams into a
//! single output buffer — leaves, separators, and indentation are written
//! in place, so a nested value costs O(total bytes) instead of re-copying
//! every child rendering once per ancestor level. Closure names resolve
//! through the program (`functions[func_idx].name`), which is why
//! `inspect` takes the [`Program`] and why it stays `pub`: the CLI and
//! REPL print final results with it after `run()` returns.

use std::fmt::Write;

use crate::bytecode::{MapBacking, Program, Value, ValueView, hamt};

use super::{binary, str_ref};

pub(super) fn value_type_name(v: &Value) -> String {
    match v.kind() {
        ValueView::Int(_) => "Int".to_string(),
        ValueView::Float(_) => "Float".to_string(),
        ValueView::Bool(_) => "Bool".to_string(),
        ValueView::Str(_) => "String".to_string(),
        ValueView::Array(_) | ValueView::Range(..) => "Array".to_string(),
        ValueView::Binary(_) => "Binary".to_string(),
        ValueView::Tuple(_) => "Tuple".to_string(),
        ValueView::Closure(_) => "Function".to_string(),
        ValueView::Enum(e) => e.enum_name().to_string(),
        ValueView::Socket(_) => "Socket".to_string(),
        ValueView::Nil => "Nil".to_string(),
        ValueView::Map(_) => "Map".to_string(),
    }
}

/// Render `v` for `println`/`string.inspect`. A closure stores only its
/// `func_idx` + captures, so its name is resolved here through
/// `program.functions[func_idx]`.
pub fn inspect(v: &Value, program: &Program) -> String {
    let mut out = String::new();
    inspect_impl(v, program, Some(0), &mut out);
    out
}

fn is_simple_value(v: &Value) -> bool {
    match v.kind() {
        ValueView::Int(_) | ValueView::Float(_) | ValueView::Bool(_) => true,
        ValueView::Closure(_) => true,
        ValueView::Str(s) => s.len() < 20,
        ValueView::Binary(b) => b.bit_len().div_ceil(8) as usize <= 8,
        ValueView::Enum(e) => e.payload().is_empty(),
        _ => false,
    }
}

/// A constructor named after its own type that carries field labels is the
/// record-shorthand form (`type T { a Int  b Bool }` → `T{ a: .., b: .. }`).
/// Sum-type variants and prelude constructors (`Some`, `Ok`, `Err`, …) have a
/// distinct variant name and keep the positional `Variant(..)` rendering.
fn is_record(e: &crate::bytecode::EnumRef<'_>) -> bool {
    !e.field_labels().is_empty()
        && e.field_labels().len() == e.payload().len()
        && e.enum_name() == e.variant_name()
}

pub(super) fn f64_str(f: f64) -> String {
    let mut s = String::new();
    write_f64(&mut s, f);
    s
}

/// Match V's f64.str() — always include a decimal point for finite values.
fn write_f64(out: &mut String, f: f64) {
    let start = out.len();
    let _ = write!(out, "{}", f);
    if f.is_finite()
        && !out[start..]
            .bytes()
            .any(|b| matches!(b, b'.' | b'e' | b'E'))
    {
        out.push_str(".0");
    }
}

fn push_indent(out: &mut String, n: usize) {
    for _ in 0..n {
        out.push_str("  ");
    }
}

fn join_inline(out: &mut String, xs: &[Value], program: &Program) {
    for (i, v) in xs.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        inspect_impl(v, program, None, out);
    }
}

/// Body of an expanded container: one line per element, indented one level
/// past `n`, closing delimiter back at `n`. The caller has already written
/// the opening delimiter.
fn block_body<T>(
    out: &mut String,
    close: char,
    n: usize,
    items: impl Iterator<Item = T>,
    mut write_line: impl FnMut(T, &mut String),
) {
    out.push('\n');
    for (i, item) in items.enumerate() {
        if i > 0 {
            out.push_str(",\n");
        }
        push_indent(out, n + 1);
        write_line(item, out);
    }
    out.push('\n');
    push_indent(out, n);
    out.push(close);
}

/// Array-of-leaves layout: flat `[..]` when it fits in 80 columns, otherwise
/// wrapped six elements per line at indent depth `n`. Streams the flat
/// attempt into `out` and bails as soon as it exceeds the budget, then
/// truncates and re-renders wrapped — each leaf is written at most twice.
/// `make_iter` is called once per pass so the wrapped retry restarts from
/// the front without the caller having to buffer elements.
fn wrap_six<T, I: Iterator<Item = T>>(
    out: &mut String,
    n: usize,
    mut make_iter: impl FnMut() -> I,
    mut write_elem: impl FnMut(T, &mut String),
) {
    let start = out.len();
    out.push('[');
    let mut fits = true;
    for (i, item) in make_iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        write_elem(item, out);
        if out.len() - start > 80 {
            fits = false;
            break;
        }
    }
    if fits {
        out.push(']');
        if out.len() - start <= 80 {
            return;
        }
    }
    out.truncate(start);
    out.push_str("[\n");
    push_indent(out, n + 1);
    for (i, item) in make_iter().enumerate() {
        if i > 0 {
            if i % 6 == 0 {
                out.push_str(", \n");
                push_indent(out, n + 1);
            } else {
                out.push_str(", ");
            }
        }
        write_elem(item, out);
    }
    out.push('\n');
    push_indent(out, n);
    out.push(']');
}

/// Render a value into `out`. `indent = None` forces a single-line rendering;
/// `Some(n)` pretty-prints, expanding containers across lines at indent depth
/// `n` when their children aren't all simple leaves.
fn inspect_impl(v: &Value, program: &Program, indent: Option<usize>, out: &mut String) {
    match v.kind() {
        ValueView::Int(i) => {
            let _ = write!(out, "{}", i);
        }
        ValueView::Float(f) => write_f64(out, f),
        ValueView::Bool(b) => out.push_str(if b { "True" } else { "False" }),
        ValueView::Str(s) => out.push_str(s),
        ValueView::Binary(b) => out.push_str(&binary::inspect(&b.to_aligned_vec(), b.bit_len())),
        ValueView::Closure(c) => {
            let _ = write!(
                out,
                "<fn#{}>",
                program.functions[c.func_idx() as usize].name
            );
        }
        ValueView::Socket(s) => {
            let kind = if s.is_listener { "listener" } else { "socket" };
            let _ = write!(out, "<{}#{}>", kind, s.id);
        }
        ValueView::Range(a, z) => {
            // Render like the materialized array, without building one in
            // any arena (printing must not allocate heap values). Each
            // element streams straight into `out` — no per-element String.
            let count = (z as i128 - a as i128).clamp(0, usize::MAX as i128) as usize;
            match indent {
                None => {
                    out.push('[');
                    for i in 0..count {
                        if i > 0 {
                            out.push_str(", ");
                        }
                        let _ = write!(out, "{}", a + i as i64);
                    }
                    out.push(']');
                }
                Some(_) if count == 0 => out.push_str("[]"),
                Some(n) => wrap_six(
                    out,
                    n,
                    || 0..count,
                    |i, out| {
                        let _ = write!(out, "{}", a + i as i64);
                    },
                ),
            }
        }
        ValueView::Nil => out.push_str("Nil"),
        ValueView::Map(m) => match m.backing() {
            // A live view of the host environment: render an opaque marker
            // rather than dumping (and materializing) every variable.
            MapBacking::Env => out.push_str("<map env>"),
            // An in-memory map renders its entries as `{k: v, …}`.
            MapBacking::Hamt => {
                let entries = hamt::collect_entries(v);
                out.push('{');
                for (i, (k, val)) in entries.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    inspect_impl(k, program, None, out);
                    out.push_str(": ");
                    inspect_impl(val, program, None, out);
                }
                out.push('}');
            }
        },
        ValueView::Enum(e) if e.payload().is_empty() => out.push_str(e.variant_name()),
        ValueView::Enum(e) if is_record(&e) => {
            let payload = e.payload();
            let labels = e.field_labels();
            match indent {
                Some(n) if !payload.iter().all(is_simple_value) => {
                    out.push_str(e.variant_name());
                    out.push_str(" {");
                    block_body(out, '}', n, labels.iter().zip(payload), |(l, v), out| {
                        out.push_str(str_ref(l));
                        out.push_str(": ");
                        inspect_impl(v, program, Some(n + 1), out);
                    });
                }
                _ => {
                    out.push_str(e.variant_name());
                    out.push_str("{ ");
                    for (i, (l, v)) in labels.iter().zip(payload).enumerate() {
                        if i > 0 {
                            out.push_str(", ");
                        }
                        out.push_str(str_ref(l));
                        out.push_str(": ");
                        inspect_impl(v, program, None, out);
                    }
                    out.push_str(" }");
                }
            }
        }
        ValueView::Enum(e) => {
            let payload = e.payload();
            match indent {
                Some(n) if !payload.iter().all(is_simple_value) => {
                    out.push_str(e.variant_name());
                    out.push('(');
                    block_body(out, ')', n, payload.iter(), |v, out| {
                        inspect_impl(v, program, Some(n + 1), out);
                    });
                }
                _ => {
                    out.push_str(e.variant_name());
                    out.push('(');
                    join_inline(out, payload, program);
                    out.push(')');
                }
            }
        }
        ValueView::Tuple(t) => match indent {
            None => {
                out.push('(');
                join_inline(out, t, program);
                out.push(')');
            }
            Some(_) if t.is_empty() => out.push_str("()"),
            Some(n) => {
                if t.iter().all(is_simple_value) {
                    let start = out.len();
                    out.push('(');
                    join_inline(out, t, program);
                    out.push(')');
                    if out.len() - start <= 80 {
                        return;
                    }
                    out.truncate(start);
                }
                out.push('(');
                block_body(out, ')', n, t.iter(), |v, out| {
                    inspect_impl(v, program, Some(n + 1), out);
                });
            }
        },
        ValueView::Array(arr) => match indent {
            // Stream elements straight from the persistent tree; the layout
            // helpers consume iterators, so nothing is collected into host
            // memory even for the wrap-six retry (it just restarts a fresh
            // walk).
            None => {
                out.push('[');
                for (i, v) in arr.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    inspect_impl(&v, program, None, out);
                }
                out.push(']');
            }
            Some(_) if arr.is_empty() => out.push_str("[]"),
            Some(n) if arr.iter().all(|v| is_simple_value(&v)) => {
                wrap_six(
                    out,
                    n,
                    || arr.iter(),
                    |v, out| inspect_impl(&v, program, None, out),
                );
            }
            Some(n) => {
                out.push('[');
                block_body(out, ']', n, arr.iter(), |v, out| {
                    inspect_impl(&v, program, Some(n + 1), out);
                });
            }
        },
    }
}
