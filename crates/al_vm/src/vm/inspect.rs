//! Value rendering: the `inspect` form `Print`/`ToString` and the CLI/REPL
//! result line share, plus the type names error messages use.
//!
//! `inspect` renders a value the way the user would write it, and is
//! layout-aware: short simple arrays stay flat, long ones wrap six to a line,
//! nested aggregates expand one element per line. The unit tests in [`super`]
//! lock that shape.
//!
//! Nothing here allocates in an arena — the whole rendering streams into one
//! host `String`. `inspect` takes the [`Program`] because a closure stores only
//! its `func_idx` and its name has to be looked up there.

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

/// Render `v` for `println`/`string.inspect`.
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
/// record-shorthand form (`T{ a: .., b: .. }`). Sum-type variants have a
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

/// Body of an expanded container: one line per element indented past `n`, then
/// the closing delimiter at `n`. The caller writes the opening delimiter.
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
/// six elements per line at indent depth `n`. The flat attempt streams into
/// `out` and bails at the budget, so each leaf is written at most twice.
/// `make_iter` is called once per pass so the retry needs no buffering.
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

/// Render a value into `out`. `indent = None` forces one line; `Some(n)`
/// expands containers across lines when their children are not all leaves.
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
            // Render like the materialized array without building one:
            // printing must not allocate heap values.
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
            // A live view of the host environment: an opaque marker, rather
            // than materializing every variable.
            MapBacking::Env => out.push_str("<map env>"),
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
            // The layout helpers take iterators, so elements stream straight
            // from the persistent tree even on the wrap-six retry.
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
