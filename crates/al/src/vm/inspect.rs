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
//! any arena, just host `String`s. Closure names resolve through the
//! program (`functions[func_idx].name`), which is why `inspect` takes
//! the [`Program`] and why it stays `pub`: the CLI and REPL print final
//! results with it after `run()` returns.

use al_core::bytecode::{Program, Value, ValueView};

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
    }
}

/// Render `v` for `println`/`string.inspect`. A closure stores only its
/// `func_idx` + captures, so its name is resolved here through
/// `program.functions[func_idx]`.
pub fn inspect(v: &Value, program: &Program) -> String {
    inspect_impl(v, program, Some(0))
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
fn is_record(e: &al_core::bytecode::EnumRef<'_>) -> bool {
    !e.field_labels().is_empty()
        && e.field_labels().len() == e.payload().len()
        && e.enum_name() == e.variant_name()
}

pub(super) fn f64_str(f: f64) -> String {
    // Match V's f64.str() — always include a decimal point for finite values.
    let s = format!("{}", f);
    if f.is_finite() && !s.contains('.') && !s.contains('e') && !s.contains('E') {
        format!("{}.0", s)
    } else {
        s
    }
}

fn join_inline(xs: &[Value], program: &Program) -> String {
    xs.iter()
        .map(|v| inspect_impl(v, program, None))
        .collect::<Vec<_>>()
        .join(", ")
}

fn block(open: &str, close: char, lines: Vec<String>, n: usize) -> String {
    let inner = "  ".repeat(n + 1);
    let body = lines
        .into_iter()
        .map(|l| format!("{}{}", inner, l))
        .collect::<Vec<_>>()
        .join(",\n");
    format!("{}\n{}\n{}{}", open, body, "  ".repeat(n), close)
}

/// Render a value. `indent = None` forces a single-line rendering; `Some(n)`
/// pretty-prints, expanding containers across lines at indent depth `n` when
/// their children aren't all simple leaves.
fn inspect_impl(v: &Value, program: &Program, indent: Option<usize>) -> String {
    let pretty = |xs: &[Value], n: usize| -> Vec<String> {
        xs.iter()
            .map(|v| inspect_impl(v, program, Some(n + 1)))
            .collect()
    };
    match v.kind() {
        ValueView::Int(i) => i.to_string(),
        ValueView::Float(f) => f64_str(f),
        ValueView::Bool(b) => if b { "True" } else { "False" }.to_string(),
        ValueView::Str(s) => s.to_string(),
        ValueView::Binary(b) => binary::inspect(&b.to_aligned_vec(), b.bit_len()),
        ValueView::Closure(c) => {
            format!("<fn#{}>", program.functions[c.func_idx() as usize].name)
        }
        ValueView::Socket(s) => {
            let kind = if s.is_listener { "listener" } else { "socket" };
            format!("<{}#{}>", kind, s.id)
        }
        ValueView::Range(a, z) => {
            // Render like the materialized array, without building one in
            // any arena (printing must not allocate heap values).
            let elems: Vec<String> = (a..z).map(|i| i.to_string()).collect();
            match indent {
                None => format!("[{}]", elems.join(", ")),
                Some(_) if elems.is_empty() => "[]".into(),
                Some(n) => {
                    let flat = format!("[{}]", elems.join(", "));
                    if flat.len() <= 80 {
                        return flat;
                    }
                    let inner = "  ".repeat(n + 1);
                    let body = elems
                        .chunks(6)
                        .map(|chunk| chunk.join(", "))
                        .collect::<Vec<_>>()
                        .join(&format!(", \n{}", inner));
                    format!("[\n{}{}\n{}]", inner, body, "  ".repeat(n))
                }
            }
        }
        ValueView::Nil => "Nil".to_string(),
        ValueView::Enum(e) if e.payload().is_empty() => e.variant_name().to_string(),
        ValueView::Enum(e) if is_record(&e) => {
            let payload = e.payload();
            let fields = |mode: Option<usize>| -> Vec<String> {
                e.field_labels()
                    .iter()
                    .zip(payload)
                    .map(|(l, v)| format!("{}: {}", str_ref(l), inspect_impl(v, program, mode)))
                    .collect()
            };
            match indent {
                Some(n) if !payload.iter().all(is_simple_value) => block(
                    &format!("{} {{", e.variant_name()),
                    '}',
                    fields(Some(n + 1)),
                    n,
                ),
                _ => format!("{}{{ {} }}", e.variant_name(), fields(None).join(", ")),
            }
        }
        ValueView::Enum(e) => {
            let payload = e.payload();
            match indent {
                Some(n) if !payload.iter().all(is_simple_value) => block(
                    &format!("{}(", e.variant_name()),
                    ')',
                    pretty(payload, n),
                    n,
                ),
                _ => format!("{}({})", e.variant_name(), join_inline(payload, program)),
            }
        }
        ValueView::Tuple(t) => match indent {
            None => format!("({})", join_inline(t, program)),
            Some(_) if t.is_empty() => "()".into(),
            Some(n) => {
                if t.iter().all(is_simple_value) {
                    let flat = format!("({})", join_inline(t, program));
                    if flat.len() <= 80 {
                        return flat;
                    }
                }
                block("(", ')', pretty(t, n), n)
            }
        },
        ValueView::Array(arr) => {
            // The tree lacks slice APIs (chunks/&[Value]); materialise into
            // host memory once so the formatting body below stays
            // byte-identical to the old behaviour (this is a cold path).
            let arr: Vec<Value> = arr.iter().collect();
            let arr: &[Value] = &arr;
            match indent {
                None => format!("[{}]", join_inline(arr, program)),
                Some(_) if arr.is_empty() => "[]".into(),
                Some(n) if arr.iter().all(is_simple_value) => {
                    let flat = format!("[{}]", join_inline(arr, program));
                    if flat.len() <= 80 {
                        return flat;
                    }
                    // Long array of leaves: wrap 6-per-line rather than one-per-line.
                    let inner = "  ".repeat(n + 1);
                    let body = arr
                        .chunks(6)
                        .map(|chunk| join_inline(chunk, program))
                        .collect::<Vec<_>>()
                        .join(&format!(", \n{}", inner));
                    format!("[\n{}{}\n{}]", inner, body, "  ".repeat(n))
                }
                Some(n) => block("[", ']', pretty(arr, n), n),
            }
        }
    }
}
