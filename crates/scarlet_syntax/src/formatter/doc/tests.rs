use super::*;

fn layout(doc: &Doc, max_width: isize) -> String {
    layout_keeping_flat(doc, max_width, &KeepFlat::These(BTreeSet::new())).text
}

fn parens(open: &'static str, arg: &'static str) -> Doc {
    group(d![
        text(open),
        nest(1, d![line0(), text(arg)]),
        trailing_comma(),
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
        trailing_comma(),
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
        trailing_comma(),
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
