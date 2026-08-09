use std::borrow::Cow;
use std::collections::VecDeque;

use unicode_width::UnicodeWidthChar;

const TAB_WIDTH: isize = 4;

/// A layout document. The representation is private because the engine relies
/// on an invariant only these constructors keep: a `Group`'s subtree contains a
/// hard newline iff its `Breaks` is `Always` or `Hugging`.
#[derive(Debug, Clone)]
pub struct Doc(DocInner);

#[derive(Debug, Clone)]
enum DocInner {
    Nil,
    /// Literal text, guaranteed newline-free: `text()` splits multi-line
    /// strings into one `Text` per line joined by `RawNewline`s. `width` is the
    /// display-column count, precomputed so `fits` probes stay O(1) per visit.
    Text {
        s: Cow<'static, str>,
        width: isize,
    },
    /// A newline embedded in literal text (a multi-line `/* … */` comment).
    /// Unlike `HardLine` it emits no indent, because the text's continuation
    /// lines must render verbatim.
    RawNewline,
    /// Soft break. Flat → `unbroken`; broken → `broken` then newline+indent.
    Break {
        broken: &'static str,
        unbroken: &'static str,
        width: isize,
    },
    /// `n` hard newlines. Forces every enclosing group to break.
    HardLine(usize),
    /// Increase the indent (in tab stops) for the wrapped doc.
    Nest(isize, Box<Doc>),
    /// Like `Nest`, but the indent applies only when the enclosing group is
    /// broken, so a hugged item's block nests from the group's base indent.
    NestIfBroken(isize, Box<Doc>),
    /// Try to fit on one line; if that exceeds the width budget, render with
    /// every contained `Break` broken.
    Group {
        doc: Box<Doc>,
        breaks: Breaks,
    },
    /// A hugged trailing item: the block-shaped last element of a delimited
    /// list (`f(a, fn() { … })`). Width probes treat its hard newlines as the
    /// natural end of the line rather than as proof that the enclosing content
    /// cannot render flat. It always renders broken.
    Hug(Box<Doc>),
    Concat(Vec<Doc>),
}

impl Doc {
    pub fn is_nil(&self) -> bool {
        matches!(self.0, DocInner::Nil)
    }
}

/// How willing a group is to be the point where its line breaks. `fits`
/// consults this when a group appears in the content trailing the group being
/// probed. Private: `Always` is valid only for a subtree with a hard newline
/// and `Hugging` only for `delimited_hug`'s shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Breaks {
    /// Contains a hard newline: never renders flat, so the line provably ends
    /// inside it.
    Always,
    /// Block-shaped (`{ … }`): a natural end for the line, so width probes of
    /// earlier content assume the line ends at its first break.
    Willingly,
    /// List-shaped (`(a, b, c)`): breaks only when its own contents do not fit.
    /// Width probes count it at full flat width, so earlier content breaks
    /// first to make room for it.
    Reluctantly,
    /// List-shaped with a hugged, hard-breaking final item
    /// (`f(a, fn() { … })`): ends the line like `Always`, but goes
    /// one-item-per-line only when its head does not fit.
    Hugging,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Flat,
    Broken,
}

pub fn nil() -> Doc {
    Doc(DocInner::Nil)
}

pub fn text(s: impl Into<Cow<'static, str>>) -> Doc {
    let s = s.into();
    if s.is_empty() {
        return nil();
    }
    if !s.contains('\n') {
        return single_line_text(s);
    }
    // One Text per line joined by RawNewlines, so column accounting resumes
    // from the last line and enclosing groups see the hard break.
    let mut parts: Vec<Doc> = Vec::new();
    match s {
        Cow::Borrowed(s) => {
            for (i, line) in s.split('\n').enumerate() {
                if i > 0 {
                    parts.push(Doc(DocInner::RawNewline));
                }
                if !line.is_empty() {
                    parts.push(single_line_text(Cow::Borrowed(line)));
                }
            }
        }
        Cow::Owned(s) => {
            for (i, line) in s.split('\n').enumerate() {
                if i > 0 {
                    parts.push(Doc(DocInner::RawNewline));
                }
                if !line.is_empty() {
                    parts.push(single_line_text(Cow::Owned(line.to_owned())));
                }
            }
        }
    }
    concat(parts)
}

fn single_line_text(s: Cow<'static, str>) -> Doc {
    debug_assert!(!s.contains('\n'));
    let width = str_width(&s);
    Doc(DocInner::Text { s, width })
}

/// Soft break: " " when flat, newline when broken.
pub fn line() -> Doc {
    break_("", " ")
}

/// Soft break: "" when flat, newline when broken.
fn line0() -> Doc {
    break_("", "")
}

/// Soft break that emits `broken` (e.g. ",") before the newline when broken,
/// and `unbroken` when flat.
fn break_(broken: &'static str, unbroken: &'static str) -> Doc {
    Doc(DocInner::Break {
        broken,
        unbroken,
        width: str_width(unbroken),
    })
}

pub fn hardline() -> Doc {
    Doc(DocInner::HardLine(1))
}

pub fn hardlines(n: usize) -> Doc {
    if n == 0 {
        nil()
    } else {
        Doc(DocInner::HardLine(n))
    }
}

pub fn nest(tabs: isize, d: Doc) -> Doc {
    if tabs == 0 {
        return d;
    }
    Doc(DocInner::Nest(tabs, Box::new(d)))
}

/// Indent that applies only in the broken layout.
fn nest_if_broken(tabs: isize, d: Doc) -> Doc {
    if tabs == 0 {
        return d;
    }
    Doc(DocInner::NestIfBroken(tabs, Box::new(d)))
}

/// Mark `d` as a hugged trailing item; see `DocInner::Hug`.
fn hug(d: Doc) -> Doc {
    Doc(DocInner::Hug(Box::new(d)))
}

pub fn group(d: Doc) -> Doc {
    // Idempotent: an already-grouped doc keeps its own Breaks.
    if matches!(d.0, DocInner::Group { .. }) {
        return d;
    }
    group_as(Breaks::Reluctantly, d)
}

/// Group that breaks willingly: block-shaped (`{ … }`), a natural end for the
/// line. See `Breaks::Willingly`.
pub fn group_willing(d: Doc) -> Doc {
    group_as(Breaks::Willingly, d)
}

fn group_as(breaks: Breaks, d: Doc) -> Doc {
    match d.0 {
        DocInner::Text { .. } | DocInner::Nil => d,
        // Rebuilding a hugging group with the caller's `breaks` would land on
        // `Always` and silently turn the hug into per-item breaks.
        DocInner::Group {
            breaks: Breaks::Hugging,
            ..
        } => d,
        DocInner::Group { doc, .. } => group_as(breaks, *doc),
        inner => {
            let d = Doc(inner);
            let breaks = if contains_hardline(&d) {
                Breaks::Always
            } else {
                breaks
            };
            Doc(DocInner::Group {
                doc: Box::new(d),
                breaks,
            })
        }
    }
}

/// Whether the doc, as the trailing element of a line, provides the line's
/// natural end. Content before such a doc never breaks on its behalf.
pub fn ends_line(d: &Doc) -> bool {
    matches!(
        d.0,
        DocInner::Group {
            breaks: Breaks::Willingly,
            ..
        }
    ) || contains_hardline(d)
}

/// Whether the doc contains a hard newline. Nested groups answer from their
/// cached `Breaks` flag rather than being walked again.
fn contains_hardline(d: &Doc) -> bool {
    match &d.0 {
        DocInner::Nil | DocInner::Text { .. } | DocInner::Break { .. } => false,
        DocInner::HardLine(_) | DocInner::RawNewline => true,
        DocInner::Nest(_, inner) | DocInner::NestIfBroken(_, inner) => contains_hardline(inner),
        // A hugged item's hard newlines are real for every group except the
        // hugging group itself, so enclosing groups still break around it.
        DocInner::Hug(inner) => contains_hardline(inner),
        DocInner::Group { breaks, .. } => matches!(breaks, Breaks::Always | Breaks::Hugging),
        DocInner::Concat(ds) => ds.iter().any(contains_hardline),
    }
}

pub fn concat(ds: Vec<Doc>) -> Doc {
    let mut out: Vec<Doc> = Vec::with_capacity(ds.len());
    for d in ds {
        match d.0 {
            DocInner::Nil => {}
            DocInner::Concat(inner) => out.extend(inner),
            other => out.push(Doc(other)),
        }
    }
    match out.len() {
        0 => nil(),
        1 => out.into_iter().next().unwrap_or_else(nil),
        _ => Doc(DocInner::Concat(out)),
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
    // Peel a Concat separator into its parts so each of the N-1 separators
    // pushes leaf parts directly instead of cloning a wrapper Vec that
    // `concat` would immediately flatten and drop.
    let sep_parts: &[Doc] = match &sep.0 {
        DocInner::Concat(v) => v,
        DocInner::Nil => &[],
        _ => std::slice::from_ref(&sep),
    };
    let mut out = Vec::with_capacity(items.len() + (items.len() - 1) * sep_parts.len());
    for (i, it) in items.into_iter().enumerate() {
        if i > 0 {
            out.extend_from_slice(sep_parts);
        }
        out.push(it);
    }
    concat(out)
}

/// Shared shape of the `delimited*` family: `items` joined by `sep`, with
/// `tail` between the final item and `close`.
fn delimited_with(
    open: &'static str,
    items: Vec<Doc>,
    close: &'static str,
    sep: Doc,
    tail: Doc,
) -> Doc {
    if items.is_empty() {
        return text(format!("{open}{close}"));
    }
    let body = join(items, sep);
    group(d![
        text(open),
        nest(1, d![line0(), body]),
        tail,
        text(close),
    ])
}

/// `open i0, i1, ... close` on one line, or one item per line indented one tab
/// with a trailing comma.
pub fn delimited(open: &'static str, items: Vec<Doc>, close: &'static str) -> Doc {
    delimited_with(open, items, close, d![text(","), line()], break_(",", ""))
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
/// line; otherwise this falls back to `delimited`. It is also exactly
/// `delimited` when the final item renders flat, or when an earlier item
/// hard-breaks and leaves nothing to hug onto.
pub fn delimited_hug(open: &'static str, mut items: Vec<Doc>, close: &'static str) -> Doc {
    let Some(last) = items.pop() else {
        return text(format!("{open}{close}"));
    };
    if !contains_hardline(&last) || items.iter().any(contains_hardline) {
        items.push(last);
        return delimited(open, items, close);
    }
    items.push(hug(last));
    let body = join(items, d![text(","), line()]);
    Doc(DocInner::Group {
        doc: Box::new(d![
            text(open),
            nest_if_broken(1, d![line0(), body]),
            break_(",", ""),
            text(close),
        ]),
        breaks: Breaks::Hugging,
    })
}

/// Like `delimited`, but emits no trailing comma when broken across lines. For
/// groups whose final element is a `..` rest marker: the parser rejects a comma
/// after `..`, so a wrapped pattern must end `..\n<close>`.
pub fn delimited_no_trailing(open: &'static str, items: Vec<Doc>, close: &'static str) -> Doc {
    delimited_with(open, items, close, d![text(","), line()], line0())
}

/// `{ body }` on one line, or broken across lines with the body indented.
pub fn block(body: Doc) -> Doc {
    if body.is_nil() {
        return text("{}");
    }
    group_as(
        Breaks::Willingly,
        d![text("{"), nest(1, d![line(), body]), line(), text("}")],
    )
}

/// Always broken one-per-line, with **no** separators — the newline is the
/// separator. Used for a constructor field list that is too long to read flat.
pub fn hard_list_bare(open: &'static str, items: Vec<Doc>, close: &'static str) -> Doc {
    if items.is_empty() {
        return d![text(open), text(close)];
    }
    let mut parts = Vec::new();
    for (i, it) in items.into_iter().enumerate() {
        if i > 0 {
            parts.push(hardline());
        }
        parts.push(it);
    }
    d![
        text(open),
        nest(1, d![hardline(), concat(parts)]),
        hardline(),
        text(close)
    ]
}

/// Comma-separated when the list fits on one line; one per line with **no**
/// commas when it breaks. The parser accepts either.
pub fn delimited_commas_when_flat(open: &'static str, items: Vec<Doc>, close: &'static str) -> Doc {
    delimited_with(open, items, close, break_("", ", "), line0())
}

/// A delimited list always broken one-per-line, with a trailing comma:
///
/// ```text
/// (
///     a Int,
///     b String,
/// )
/// ```
///
/// Used when the author already broke the list — width is not the only reason
/// to keep it broken.
pub fn hard_list(open: &'static str, items: Vec<Doc>, close: &'static str) -> Doc {
    if items.is_empty() {
        return d![text(open), text(close)];
    }
    let mut parts = Vec::new();
    for (i, it) in items.into_iter().enumerate() {
        if i > 0 {
            parts.push(hardline());
        }
        parts.push(it);
        parts.push(text(","));
    }
    d![
        text(open),
        nest(1, d![hardline(), concat(parts)]),
        hardline(),
        text(close)
    ]
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
    // Hoisted so every width probe reuses one allocation.
    let mut probe: Vec<(isize, Mode, &Doc)> = Vec::new();
    work.push_back((0, Mode::Broken, doc));

    while let Some((indent, mode, d)) = work.pop_front() {
        match &d.0 {
            DocInner::Nil => {}
            DocInner::Text { s, width } => {
                out.push_str(s);
                col += width;
            }
            DocInner::Break {
                broken,
                unbroken,
                width,
            } => match mode {
                Mode::Flat => {
                    out.push_str(unbroken);
                    col += width;
                }
                Mode::Broken => {
                    out.push_str(broken);
                    emit_newline(&mut out, indent);
                    col = indent * TAB_WIDTH;
                }
            },
            DocInner::HardLine(n) => {
                for _ in 1..*n {
                    out.push('\n');
                }
                emit_newline(&mut out, indent);
                col = indent * TAB_WIDTH;
            }
            DocInner::RawNewline => {
                // No indent: the continuation line renders verbatim.
                out.push('\n');
                col = 0;
            }
            DocInner::Nest(i, inner) => {
                work.push_front((indent + i, mode, inner));
            }
            DocInner::NestIfBroken(i, inner) => {
                let indent = if mode == Mode::Broken {
                    indent + i
                } else {
                    indent
                };
                work.push_front((indent, mode, inner));
            }
            DocInner::Hug(inner) => {
                // A hugged item always renders broken, and the groups inside it
                // must re-probe their own widths: the enclosing group's flat
                // probe stopped at the hug instead of walking its subtree.
                work.push_front((indent, Mode::Broken, inner));
            }
            DocInner::Group { doc: inner, breaks } => {
                let m = if *breaks == Breaks::Always {
                    // A hard newline inside makes flat rendering impossible.
                    Mode::Broken
                } else if mode == Mode::Flat {
                    // Flat propagation (Lindig, "Strictly Pretty"). The
                    // enclosing group's probe already proved this subtree fits
                    // flat, so re-probing would always say yes. Skipping it
                    // takes the all-fits case from O(D^2) to O(D).
                    Mode::Flat
                } else if fits(
                    max_width - col,
                    (indent, Mode::Flat, inner),
                    &work,
                    &mut probe,
                ) {
                    Mode::Flat
                } else {
                    Mode::Broken
                };
                work.push_front((indent, m, inner));
            }
            DocInner::Concat(ds) => {
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
    if s.is_ascii() {
        let tabs = s.bytes().filter(|&b| b == b'\t').count() as isize;
        return s.len() as isize + tabs * (TAB_WIDTH - 1);
    }
    let mut w = 0isize;
    for c in s.chars() {
        w += if c == '\t' {
            TAB_WIDTH
        } else {
            // Terminal columns: CJK/emoji count 2, combining marks 0.
            c.width().unwrap_or(0) as isize
        };
    }
    w
}

/// Whether a group rendered flat leaves the rest of its line within
/// `remaining` columns. `seed` is the group's contents, probed flat; `rest` is
/// the pending work after it, in its already-decided modes.
///
/// Including the trailing work is what lets an early group break for the line
/// as a whole: without it, `f(a, b) or e -> g(c)` keeps `f(a, b)` flat and
/// dumps the overflow onto `g`, the worst place on the line to break.
fn fits<'d>(
    mut remaining: isize,
    seed: (isize, Mode, &'d Doc),
    rest: &VecDeque<(isize, Mode, &'d Doc)>,
    probe: &mut Vec<(isize, Mode, &'d Doc)>,
) -> bool {
    probe.clear();
    probe.push(seed);
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
        match &d.0 {
            DocInner::Nil => {}
            DocInner::Text { width, .. } => remaining -= width,
            DocInner::Break { width, .. } => match mode {
                Mode::Flat => remaining -= width,
                Mode::Broken => return true,
            },
            DocInner::HardLine(_) | DocInner::RawNewline => match mode {
                // Inside flat content a hard newline cannot render flat; in the
                // trailing work it simply ends the line.
                Mode::Flat => return false,
                Mode::Broken => return true,
            },
            DocInner::Nest(i, inner) => probe.push((indent + i, mode, inner)),
            // Nesting never affects a width probe: probes stop at the first
            // newline.
            DocInner::NestIfBroken(_, inner) => probe.push((indent, mode, inner)),
            DocInner::Hug(inner) => probe.push((indent, Mode::Broken, inner)),
            DocInner::Group { doc, breaks } => {
                let m = match (mode, *breaks) {
                    (Mode::Flat, _) => Mode::Flat,
                    // A trailing group that breaks ends the line at its first
                    // break; a reluctant one is counted at full flat width so
                    // the probed group breaks instead of it.
                    (Mode::Broken, Breaks::Always | Breaks::Willingly | Breaks::Hugging) => {
                        Mode::Broken
                    }
                    (Mode::Broken, Breaks::Reluctantly) => Mode::Flat,
                };
                probe.push((indent, m, doc));
            }
            DocInner::Concat(ds) => {
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

    fn parens(open: &'static str, arg: &'static str) -> Doc {
        group(d![
            text(open),
            nest(1, d![line0(), text(arg)]),
            break_(",", ""),
            text(")"),
        ])
    }

    fn lambda() -> Doc {
        d![text("fn() "), hard_braces(text("body"))]
    }

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
    fn multiline_text_forces_group_break() {
        // A multi-line block comment is a hard break: the group can never
        // render flat, however generous the width.
        let d = group(d![text("/* a\n   b */"), line(), text("c")]);
        assert_eq!(layout(&d, 80), "/* a\n   b */\nc");
    }

    #[test]
    fn multiline_text_renders_verbatim_without_indent() {
        // Continuation lines render from column zero even under a nest.
        let d = d![
            text("head"),
            nest(
                2,
                d![hardline(), text("/* a\n   b */"), hardline(), text("c")]
            ),
        ];
        assert_eq!(layout(&d, 80), "head\n\t\t/* a\n   b */\n\t\tc");
    }

    #[test]
    fn col_resumes_after_multiline_text() {
        // The text overflows overall but its last line is short, so the group
        // after it fits on the resumed line and must stay flat.
        let doc = d![
            text("/* aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\nbb */ "),
            parens("f(", "ccc"),
        ];
        assert_eq!(
            layout(&doc, 20),
            "/* aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\nbb */ f(ccc)"
        );
    }

    #[test]
    fn width_probe_stops_at_trailing_multiline_text() {
        // A trailing multi-line comment ends the line at its first newline,
        // so the probe counts only its first line and the group stays flat.
        let doc = d![
            parens("f(", "aaaa"),
            text(" /* x\n   yyyyyyyyyyyyyyyyyyyyyyyy */"),
        ];
        assert_eq!(
            layout(&doc, 12),
            "f(aaaa) /* x\n   yyyyyyyyyyyyyyyyyyyyyyyy */"
        );
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
        // `(aaaa)` alone fits but the trailing text does not, so the group
        // breaks rather than overflow the line.
        let doc = || d![parens("(", "aaaa"), text(" tail")];
        assert_eq!(layout(&doc(), 11), "(aaaa) tail");
        assert_eq!(layout(&doc(), 9), "(\n\taaaa,\n) tail");
    }

    #[test]
    fn trailing_reluctant_group_breaks_earlier_group() {
        // When the pair overflows the first list breaks, not the second.
        let doc = d![parens("f(", "aaaa"), text(" + g"), parens("(", "bbbb")];
        assert_eq!(layout(&doc, 12), "f(\n\taaaa,\n) + g(bbbb)");
    }

    #[test]
    fn trailing_block_keeps_earlier_group_flat() {
        // A block after the args is a natural break point.
        let doc = || d![parens("f(", "aaaa"), text(" "), block(text("body"))];
        assert_eq!(layout(&doc(), 30), "f(aaaa) { body }");
        assert_eq!(layout(&doc(), 10), "f(aaaa) {\n\tbody\n}");
    }

    #[test]
    fn delimited_hug_lets_last_item_hug() {
        // A hard-breaking last item hugs the delimiters.
        let doc = d![
            text("f"),
            delimited_hug("(", vec![text("a"), lambda()], ")")
        ];
        assert_eq!(layout(&doc, 80), "f(a, fn() {\n\tbody\n})");
    }

    #[test]
    fn delimited_hug_breaks_per_item_when_head_overflows() {
        // Head line does not fit, so fall back to one item per line.
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
        // Nothing to hug: a flat last item gives plain `delimited` behaviour.
        let items = || vec![text("aaaa"), text("bbbb")];
        let hug_doc = delimited_hug("(", items(), ")");
        let plain_doc = delimited("(", items(), ")");
        assert_eq!(layout(&hug_doc, 80), layout(&plain_doc, 80));
        assert_eq!(layout(&hug_doc, 5), layout(&plain_doc, 5));
    }

    #[test]
    fn delimited_hug_falls_back_when_earlier_item_breaks() {
        // A hard-breaking earlier item leaves nothing for the last to hug.
        let block_item = || d![text("match x "), hard_braces(text("arm"))];
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
        // A hugging list always renders multi-line, so an enclosing group
        // must break around it.
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
        // Like a block, a hugging call is a natural end for the line.
        let doc = || {
            d![
                parens("f(", "aaaa"),
                text(" or g"),
                delimited_hug("(", vec![lambda()], ")"),
            ]
        };
        assert_eq!(layout(&doc(), 30), "f(aaaa) or g(fn() {\n\tbody\n})");
        // Too narrow for the hug head: the call breaks per item but still
        // ends the line, so the earlier group stays flat.
        assert_eq!(
            layout(&doc(), 14),
            "f(aaaa) or g(\n\tfn() {\n\t\tbody\n\t},\n)"
        );
    }

    #[test]
    fn regrouping_a_hugging_group_preserves_the_hug() {
        // A rebuild would land on `Always` and turn the hug into per-item
        // breaks.
        let hugging = || delimited_hug("(", vec![text("a"), lambda()], ")");
        let expected = layout(&d![text("f"), hugging()], 80);
        assert_eq!(expected, "f(a, fn() {\n\tbody\n})");
        assert_eq!(layout(&d![text("f"), group(hugging())], 80), expected);
        assert_eq!(
            layout(&d![text("f"), group_willing(hugging())], 80),
            expected
        );
    }
}
