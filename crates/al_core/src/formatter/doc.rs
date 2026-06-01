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
    /// Like `Nest`, but the indent applies only when the enclosing group is
    /// broken. A flat (hugging) rendering keeps the wrapped doc at the current
    /// indent so a hugged item's own block nests from the group's base indent
    /// rather than one level deeper.
    NestIfBroken(isize, Box<Doc>),
    /// Try to fit on one line; if that exceeds the width budget, render with
    /// every contained `Break` broken.
    Group {
        doc: Box<Doc>,
        breaks: Breaks,
    },
    /// A hugged trailing item: the block-shaped last element of a delimited
    /// list (`f(a, fn() { … })`). Width probes treat its hard newlines as the
    /// natural end of the line — the way trailing broken content is treated —
    /// rather than as proof that the enclosing content cannot render flat. It
    /// always renders broken, so groups inside it re-probe their own widths.
    Hug(Box<Doc>),
    Concat(Vec<Doc>),
}

/// How willing a group is to be the point where its line breaks. This is what
/// `fits` consults when a group appears in the content trailing the group
/// being probed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Breaks {
    /// Contains a hard newline: never renders flat, so the line provably ends
    /// inside it.
    Always,
    /// Block-shaped (`{ … }`): a natural end for the line. Width probes of
    /// earlier content assume the line ends at its first break rather than
    /// breaking that earlier content.
    Willingly,
    /// List-shaped (`(a, b, c)`): breaks only when its own contents do not
    /// fit. Width probes of earlier content count it at full flat width, so
    /// the earlier content breaks first to make room for it.
    Reluctantly,
    /// List-shaped with a hugged, hard-breaking final item
    /// (`f(a, fn() { … })`): the line provably ends inside it like `Always`,
    /// but it breaks one-item-per-line only when its head — everything up to
    /// the final item's first newline — does not fit. Otherwise the final item
    /// hugs the delimiters and absorbs the break.
    Hugging,
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

/// Indent that applies only in the broken layout. A hugging (flat) rendering
/// keeps the contents at the current indent.
fn nest_if_broken(tabs: isize, d: Doc) -> Doc {
    if tabs == 0 {
        return d;
    }
    Doc::NestIfBroken(tabs, Box::new(d))
}

/// Mark `d` as a hugged trailing item; see `Doc::Hug`.
fn hug(d: Doc) -> Doc {
    Doc::Hug(Box::new(d))
}

pub fn group(d: Doc) -> Doc {
    group_as(Breaks::Reluctantly, d)
}

fn group_as(breaks: Breaks, d: Doc) -> Doc {
    match d {
        Doc::Group { .. } | Doc::Text(_) | Doc::Nil => d,
        _ => {
            let breaks = if contains_hardline(&d) {
                Breaks::Always
            } else {
                breaks
            };
            Doc::Group {
                doc: Box::new(d),
                breaks,
            }
        }
    }
}

/// Whether the doc contains a hard newline. Nested groups answer from their
/// cached `Breaks` flag rather than being walked again.
fn contains_hardline(d: &Doc) -> bool {
    match d {
        Doc::Nil | Doc::Text(_) | Doc::Break { .. } => false,
        Doc::HardLine(_) => true,
        Doc::Nest(_, inner) | Doc::NestIfBroken(_, inner) => contains_hardline(inner),
        // A hugged item's hard newlines are real for every group except the
        // hugging group itself (which accounts for them via `Breaks::Hugging`),
        // so enclosing groups still break around it.
        Doc::Hug(inner) => contains_hardline(inner),
        Doc::Group { breaks, .. } => matches!(breaks, Breaks::Always | Breaks::Hugging),
        Doc::Concat(ds) => ds.iter().any(contains_hardline),
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

/// Like `delimited`, but a final item that provably renders across multiple
/// lines hugs the delimiters instead of forcing every item onto its own line:
///
/// ```text
/// f(a, fn() {
///     …
/// })
/// ```
///
/// The earlier items and the final item's first line must fit on the current
/// line; otherwise the whole list falls back to the one-item-per-line layout
/// of `delimited`. When the final item renders flat anyway (or an earlier item
/// hard-breaks, leaving nothing to hug onto), this is exactly `delimited`.
pub fn delimited_hug(open: &str, mut items: Vec<Doc>, close: &str) -> Doc {
    let Some(last) = items.pop() else {
        return text(format!("{open}{close}"));
    };
    if !contains_hardline(&last) || items.iter().any(contains_hardline) {
        items.push(last);
        return delimited(open, items, close);
    }
    let mut body: Vec<Doc> = Vec::new();
    for item in items {
        body.push(item);
        body.push(text(","));
        body.push(line());
    }
    body.push(hug(last));
    Doc::Group {
        doc: Box::new(d![
            text(open),
            nest_if_broken(1, d![line0(), concat(body)]),
            break_(",", ""),
            text(close),
        ]),
        breaks: Breaks::Hugging,
    }
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
    group_as(
        Breaks::Willingly,
        d![text("{"), nest(1, d![line(), body]), line(), text("}")],
    )
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
            Doc::NestIfBroken(i, inner) => {
                let indent = if mode == Mode::Broken {
                    indent + i
                } else {
                    indent
                };
                work.push_front((indent, mode, inner));
            }
            Doc::Hug(inner) => {
                // A hugged item always renders broken: its hard newlines are
                // real, and the groups inside it must re-probe their own widths
                // because the enclosing group's flat probe stopped at the hug
                // rather than walking its whole subtree.
                work.push_front((indent, Mode::Broken, inner));
            }
            Doc::Group { doc: inner, breaks } => {
                let m = if *breaks == Breaks::Always {
                    // A hard newline inside makes flat rendering impossible.
                    Mode::Broken
                } else if mode == Mode::Flat {
                    // Flat propagation (Lindig "Strictly Pretty"): if we are already
                    // inside a group rendered Flat, the enclosing group's `fits`
                    // probe already walked this whole subtree as Flat and proved it
                    // fits. Flat is irreversible, so re-probing here is guaranteed to
                    // return true — skip it. This collapses the all-fits case from
                    // O(D^2) (one `fits` per nested group, each rescanning its
                    // descendants) to O(D).
                    Mode::Flat
                } else if fits(max_width - col, (indent, Mode::Flat, inner), &work) {
                    Mode::Flat
                } else {
                    Mode::Broken
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

/// Whether a group rendered flat leaves the rest of its line within
/// `remaining` columns.
///
/// `seed` is the group's contents, probed in Flat mode. `rest` is the pending
/// work that follows the group, in its already-decided modes. Including the
/// trailing work is what lets an early group break for the line as a whole:
/// without it, `f(a, b) or e -> g(c)` keeps `f(a, b)` flat and dumps the
/// overflow onto `g`, the last (and worst) place on the line to break.
///
/// The probe succeeds as soon as the line provably ends — a broken-mode
/// `Break`, or a hard newline in the trailing work — and fails as soon as the
/// width is exhausted.
fn fits<'d>(
    mut remaining: isize,
    seed: (isize, Mode, &'d Doc),
    rest: &VecDeque<(isize, Mode, &'d Doc)>,
) -> bool {
    let mut probe: Vec<(isize, Mode, &'d Doc)> = vec![seed];
    let mut rest = rest.iter();
    loop {
        if remaining < 0 {
            return false;
        }
        let (indent, mode, d) = match probe.pop() {
            Some(entry) => entry,
            None => match rest.next() {
                Some(entry) => *entry,
                None => return true,
            },
        };
        match d {
            Doc::Nil => {}
            Doc::Text(s) => remaining -= str_width(s),
            Doc::Break { unbroken, .. } => match mode {
                Mode::Flat => remaining -= unbroken.len() as isize,
                // An already-broken break ends the line; everything before it fit.
                Mode::Broken => return true,
            },
            Doc::HardLine(_) => match mode {
                // Inside flat content a hard newline cannot render flat;
                // in the trailing work it simply ends the line.
                Mode::Flat => return false,
                Mode::Broken => return true,
            },
            Doc::Nest(i, inner) => probe.push((indent + i, mode, inner)),
            // Nesting never affects a width probe (probes stop at the first
            // newline); whether the conditional indent applies is resolved at
            // layout time.
            Doc::NestIfBroken(_, inner) => probe.push((indent, mode, inner)),
            // A hugged item always renders broken, so probe it the way trailing
            // broken content is probed: its first hard newline ends the line.
            Doc::Hug(inner) => probe.push((indent, Mode::Broken, inner)),
            Doc::Group { doc, breaks } => {
                let m = match (mode, *breaks) {
                    // Inside flat-probed content everything must stay flat.
                    (Mode::Flat, _) => Mode::Flat,
                    // A trailing group that is willing to break — or that
                    // provably breaks — ends the line at its first break; a
                    // reluctant one is counted at full flat width so the probed
                    // group breaks instead of it.
                    (Mode::Broken, Breaks::Always | Breaks::Willingly | Breaks::Hugging) => {
                        Mode::Broken
                    }
                    (Mode::Broken, Breaks::Reluctantly) => Mode::Flat,
                };
                probe.push((indent, m, doc));
            }
            Doc::Concat(ds) => {
                for d in ds.iter().rev() {
                    probe.push((indent, mode, d));
                }
            }
        }
    }
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

    #[test]
    fn trailing_text_breaks_group() {
        // `(aaaa)` alone fits, but the text after it on the same line does
        // not — the group must break rather than overflow the line.
        let doc = || {
            d![
                group(d![
                    text("("),
                    nest(1, d![line0(), text("aaaa")]),
                    break_(",", ""),
                    text(")"),
                ]),
                text(" tail"),
            ]
        };
        assert_eq!(layout(&doc(), 11), "(aaaa) tail");
        assert_eq!(layout(&doc(), 9), "(\n\taaaa,\n) tail");
    }

    #[test]
    fn trailing_reluctant_group_breaks_earlier_group() {
        // Two argument lists on one line: when the pair overflows, the first
        // one breaks and the second stays intact — not the other way around.
        let doc = d![
            group(d![
                text("f("),
                nest(1, d![line0(), text("aaaa")]),
                break_(",", ""),
                text(")"),
            ]),
            text(" + g"),
            group(d![
                text("("),
                nest(1, d![line0(), text("bbbb")]),
                break_(",", ""),
                text(")"),
            ]),
        ];
        assert_eq!(layout(&doc, 12), "f(\n\taaaa,\n) + g(bbbb)");
    }

    #[test]
    fn trailing_block_keeps_earlier_group_flat() {
        // A block after the args is a natural break point: the args stay
        // flat and the line ends at the block's `{`.
        let doc = || {
            d![
                group(d![
                    text("f("),
                    nest(1, d![line0(), text("aaaa")]),
                    break_(",", ""),
                    text(")"),
                ]),
                text(" "),
                block(text("body")),
            ]
        };
        assert_eq!(layout(&doc(), 30), "f(aaaa) { body }");
        assert_eq!(layout(&doc(), 10), "f(aaaa) {\n\tbody\n}");
    }

    #[test]
    fn delimited_hug_lets_last_item_hug() {
        // A hard-breaking last item hugs the delimiters: earlier items stay on
        // the head line and the closer follows the item's final line.
        let lambda = || d![text("fn() "), hard_braces(text("body"))];
        let doc = d![
            text("f"),
            delimited_hug("(", vec![text("a"), lambda()], ")")
        ];
        assert_eq!(layout(&doc, 80), "f(a, fn() {\n\tbody\n})");
    }

    #[test]
    fn delimited_hug_breaks_per_item_when_head_overflows() {
        // When the head line (everything up to the hugged item's `{`) does not
        // fit, fall back to the one-item-per-line layout with the hugged item
        // indented like any other.
        let lambda = || d![text("fn() "), hard_braces(text("body"))];
        let doc = d![
            text("f"),
            delimited_hug("(", vec![text("aaaaaaaaaa"), lambda()], ")")
        ];
        assert_eq!(
            layout(&doc, 16),
            "f(\n\taaaaaaaaaa,\n\tfn() {\n\t\tbody\n\t},\n)"
        );
    }

    #[test]
    fn delimited_hug_without_hard_break_matches_delimited() {
        // Nothing to hug: a flat-rendering last item gives the standard
        // delimited behaviour in both the fitting and overflowing cases.
        let items = || vec![text("aaaa"), text("bbbb")];
        let hug_doc = delimited_hug("(", items(), ")");
        let plain_doc = delimited("(", items(), ")");
        assert_eq!(layout(&hug_doc, 80), layout(&plain_doc, 80));
        assert_eq!(layout(&hug_doc, 5), layout(&plain_doc, 5));
    }

    #[test]
    fn delimited_hug_falls_back_when_earlier_item_breaks() {
        // A hard-breaking item before the last leaves nothing for the last to
        // hug onto: every item goes on its own line.
        let block_item = || d![text("match x "), hard_braces(text("arm"))];
        let lambda = || d![text("fn() "), hard_braces(text("body"))];
        let doc = d![
            text("f"),
            delimited_hug("(", vec![block_item(), lambda()], ")")
        ];
        assert_eq!(
            layout(&doc, 80),
            "f(\n\tmatch x {\n\t\tarm\n\t},\n\tfn() {\n\t\tbody\n\t},\n)"
        );
    }

    #[test]
    fn group_containing_hugging_group_breaks() {
        // A hugging list provably renders across multiple lines, so an
        // enclosing group must break around it rather than keep its own
        // breaks flat.
        let lambda = || d![text("fn() "), hard_braces(text("body"))];
        let call = || d![text("g"), delimited_hug("(", vec![lambda()], ")")];
        let outer = group(d![
            text("["),
            nest(1, d![line0(), call(), text(","), line(), text("x")]),
            break_(",", ""),
            text("]"),
        ]);
        assert_eq!(
            layout(&outer, 80),
            "[\n\tg(fn() {\n\t\tbody\n\t}),\n\tx,\n]"
        );
    }

    #[test]
    fn trailing_hugging_group_keeps_earlier_group_flat() {
        // Like a block, a hugging call after the args is a natural end for the
        // line: the earlier args stay flat whether the call hugs or breaks.
        let lambda = || d![text("fn() "), hard_braces(text("body"))];
        let doc = || {
            d![
                group(d![
                    text("f("),
                    nest(1, d![line0(), text("aaaa")]),
                    break_(",", ""),
                    text(")"),
                ]),
                text(" or g"),
                delimited_hug("(", vec![lambda()], ")"),
            ]
        };
        assert_eq!(layout(&doc(), 30), "f(aaaa) or g(fn() {\n\tbody\n})");
        // Too narrow for the hug head `g(fn() {`: the call breaks per-item,
        // but it still ends the line so the earlier group stays flat.
        assert_eq!(
            layout(&doc(), 14),
            "f(aaaa) or g(\n\tfn() {\n\t\tbody\n\t},\n)"
        );
    }
}
