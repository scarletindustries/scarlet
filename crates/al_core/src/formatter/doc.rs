use std::collections::VecDeque;

const TAB_WIDTH: isize = 4;

#[derive(Debug, Clone)]
pub enum Doc {
    Nil,
    Text(String),
    /// Soft break. Flat → `unbroken`; broken → `broken` then newline+indent.
    Break {
        broken: &'static str,
        unbroken: &'static str,
    },
    /// `n` hard newlines. Forces every enclosing group to break.
    HardLine(usize),
    /// Increase the indent (in tab stops) for the wrapped doc.
    Nest(isize, Box<Doc>),
    /// Try to fit on one line; if that exceeds the width budget, render with
    /// every contained `Break` broken.
    Group(Box<Doc>),
    Concat(Vec<Doc>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Flat,
    Broken,
}

pub fn nil() -> Doc {
    Doc::Nil
}

pub fn text(s: impl Into<String>) -> Doc {
    let s = s.into();
    if s.is_empty() { Doc::Nil } else { Doc::Text(s) }
}

/// Soft break: " " when flat, newline when broken.
pub fn line() -> Doc {
    Doc::Break {
        broken: "",
        unbroken: " ",
    }
}

/// Soft break: "" when flat, newline when broken.
fn line0() -> Doc {
    Doc::Break {
        broken: "",
        unbroken: "",
    }
}

/// Soft break that emits `broken` (e.g. ",") before the newline when broken,
/// and `unbroken` when flat.
fn break_(broken: &'static str, unbroken: &'static str) -> Doc {
    Doc::Break { broken, unbroken }
}

pub fn hardline() -> Doc {
    Doc::HardLine(1)
}

pub fn hardlines(n: usize) -> Doc {
    if n == 0 { Doc::Nil } else { Doc::HardLine(n) }
}

pub fn nest(tabs: isize, d: Doc) -> Doc {
    if tabs == 0 {
        return d;
    }
    Doc::Nest(tabs, Box::new(d))
}

pub fn group(d: Doc) -> Doc {
    match d {
        Doc::Group(_) | Doc::Text(_) | Doc::Nil => d,
        _ => Doc::Group(Box::new(d)),
    }
}

pub fn concat(ds: Vec<Doc>) -> Doc {
    let mut out: Vec<Doc> = Vec::with_capacity(ds.len());
    for d in ds {
        match d {
            Doc::Nil => {}
            Doc::Concat(inner) => out.extend(inner),
            other => out.push(other),
        }
    }
    match out.len() {
        0 => Doc::Nil,
        1 => out.into_iter().next().unwrap_or(Doc::Nil),
        _ => Doc::Concat(out),
    }
}

#[macro_export]
macro_rules! d {
    () => { $crate::formatter::doc::nil() };
    ($($x:expr),+ $(,)?) => {
        $crate::formatter::doc::concat(vec![$($x),+])
    };
}

pub fn join(items: Vec<Doc>, sep: Doc) -> Doc {
    if items.is_empty() {
        return nil();
    }
    let mut out = Vec::with_capacity(items.len() * 2 - 1);
    for (i, it) in items.into_iter().enumerate() {
        if i > 0 {
            out.push(sep.clone());
        }
        out.push(it);
    }
    concat(out)
}

/// `open i0, i1, ... close` on one line, or one item per line indented one tab
/// with a trailing comma.
pub fn delimited(open: &str, items: Vec<Doc>, close: &str) -> Doc {
    if items.is_empty() {
        return text(format!("{open}{close}"));
    }
    let body = join(items, d![text(","), line()]);
    group(d![
        text(open),
        nest(1, d![line0(), body]),
        break_(",", ""),
        text(close),
    ])
}

/// Like `delimited`, but emits no trailing comma when broken across lines.
/// For comma-separated groups whose final element is a `..` rest marker: the
/// parser rejects a comma after `..` (rest must be last), so a wrapped pattern
/// must end `..\n<close>` rather than `..,\n<close>`.
pub fn delimited_no_trailing(open: &str, items: Vec<Doc>, close: &str) -> Doc {
    if items.is_empty() {
        return text(format!("{open}{close}"));
    }
    let body = join(items, d![text(","), line()]);
    group(d![
        text(open),
        nest(1, d![line0(), body]),
        line0(),
        text(close),
    ])
}

/// Like `delimited`, but items are separated by a single space when flat and by
/// newlines when broken, with no commas and no trailing separator. For the
/// constructs whose separator punctuation was removed from the grammar
/// (constructor fields, type args/params, fn-type params, import items,
/// attribute args) — their items are self-delimiting, so the comma was noise.
pub fn delimited_ws(open: &str, items: Vec<Doc>, close: &str) -> Doc {
    if items.is_empty() {
        return text(format!("{open}{close}"));
    }
    let body = join(items, line());
    group(d![
        text(open),
        nest(1, d![line0(), body]),
        line0(),
        text(close),
    ])
}

/// `{ body }` on one line, or broken across lines with the body indented.
pub fn block(body: Doc) -> Doc {
    if matches!(body, Doc::Nil) {
        return text("{}");
    }
    group(d![text("{"), nest(1, d![line(), body]), line(), text("}"),])
}

/// `{ body }` always broken across lines with the body indented.
pub fn hard_braces(body: Doc) -> Doc {
    d![
        text("{"),
        nest(1, d![hardline(), body]),
        hardline(),
        text("}")
    ]
}

pub fn layout(doc: &Doc, max_width: isize) -> String {
    let mut out = String::new();
    let mut col: isize = 0;
    let mut work: VecDeque<(isize, Mode, &Doc)> = VecDeque::new();
    work.push_back((0, Mode::Broken, doc));

    while let Some((indent, mode, d)) = work.pop_front() {
        match d {
            Doc::Nil => {}
            Doc::Text(s) => {
                out.push_str(s);
                col += str_width(s);
            }
            Doc::Break { broken, unbroken } => match mode {
                Mode::Flat => {
                    out.push_str(unbroken);
                    col += unbroken.len() as isize;
                }
                Mode::Broken => {
                    out.push_str(broken);
                    emit_newline(&mut out, indent);
                    col = indent * TAB_WIDTH;
                }
            },
            Doc::HardLine(n) => {
                for _ in 1..*n {
                    out.push('\n');
                }
                emit_newline(&mut out, indent);
                col = indent * TAB_WIDTH;
            }
            Doc::Nest(i, inner) => {
                work.push_front((indent + i, mode, inner));
            }
            Doc::Group(inner) => {
                // Flat propagation (Lindig "Strictly Pretty"): if we are already
                // inside a group rendered Flat, the enclosing group's `fits`
                // probe already walked this whole subtree as Flat and proved it
                // fits. Flat is irreversible, so re-probing here is guaranteed to
                // return true — skip it. This collapses the all-fits case from
                // O(D^2) (one `fits` per nested group, each rescanning its
                // descendants) to O(D).
                let m = if mode == Mode::Flat {
                    Mode::Flat
                } else {
                    let mut q = VecDeque::new();
                    q.push_back((indent, Mode::Flat, inner.as_ref()));
                    if fits(max_width - col, q) {
                        Mode::Flat
                    } else {
                        Mode::Broken
                    }
                };
                work.push_front((indent, m, inner));
            }
            Doc::Concat(ds) => {
                for d in ds.iter().rev() {
                    work.push_front((indent, mode, d));
                }
            }
        }
    }
    out
}

fn emit_newline(out: &mut String, indent: isize) {
    out.push('\n');
    for _ in 0..indent {
        out.push('\t');
    }
}

fn str_width(s: &str) -> isize {
    let mut w = 0isize;
    for c in s.chars() {
        w += if c == '\t' { TAB_WIDTH } else { 1 };
    }
    w
}

fn fits(mut remaining: isize, mut work: VecDeque<(isize, Mode, &Doc)>) -> bool {
    while let Some((indent, mode, d)) = work.pop_front() {
        if remaining < 0 {
            return false;
        }
        match d {
            Doc::Nil => {}
            Doc::Text(s) => remaining -= str_width(s),
            Doc::Break { unbroken, .. } => match mode {
                Mode::Flat => remaining -= unbroken.len() as isize,
                Mode::Broken => return true,
            },
            Doc::HardLine(_) => return false,
            Doc::Nest(i, inner) => work.push_front((indent + i, mode, inner)),
            Doc::Group(inner) => work.push_front((indent, Mode::Flat, inner)),
            Doc::Concat(ds) => {
                for d in ds.iter().rev() {
                    work.push_front((indent, mode, d));
                }
            }
        }
    }
    remaining >= 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_fits_flat() {
        let d = group(d![text("a"), line(), text("b"), line(), text("c")]);
        assert_eq!(layout(&d, 10), "a b c");
    }

    #[test]
    fn group_breaks() {
        let d = group(d![text("aaaa"), line(), text("bbbb"), line(), text("cccc")]);
        assert_eq!(layout(&d, 10), "aaaa\nbbbb\ncccc");
    }

    #[test]
    fn nesting() {
        let d = group(d![
            text("["),
            nest(
                1,
                d![line0(), text("aaaaa"), text(","), line(), text("bbbbb")]
            ),
            break_(",", ""),
            text("]"),
        ]);
        assert_eq!(layout(&d, 80), "[aaaaa, bbbbb]");
        assert_eq!(layout(&d, 10), "[\n\taaaaa,\n\tbbbbb,\n]");
    }

    #[test]
    fn hardline_forces_break() {
        let d = group(d![text("a"), hardline(), text("b")]);
        assert_eq!(layout(&d, 80), "a\nb");
    }

    #[test]
    fn delimited_helper() {
        let d = delimited("(", vec![text("one"), text("two"), text("three")], ")");
        assert_eq!(layout(&d, 80), "(one, two, three)");
        assert_eq!(layout(&d, 10), "(\n\tone,\n\ttwo,\n\tthree,\n)");
    }

    #[test]
    fn block_helper() {
        let d = block(text("body"));
        assert_eq!(layout(&d, 80), "{ body }");
        assert_eq!(layout(&d, 5), "{\n\tbody\n}");
    }
}
