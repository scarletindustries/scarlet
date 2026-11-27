use super::*;

#[track_caller]
fn fmt(src: &str) -> String {
    match format(src) {
        FormatResult::Formatted { output } => output,
        FormatResult::ParseFailed { errors } => panic!("parse failed: {errors:?}"),
        FormatResult::CommentsLost { comment } => {
            panic!("formatter lost the comment `{comment}` in:\n{src}")
        }
        FormatResult::OutputInvalid(detail) => {
            panic!("formatter produced invalid output ({detail}) for:\n{src}")
        }
    }
}

/// A constructor destructured through its module keeps its module: a
/// formatter that dropped it would name a constructor not in scope.
#[test]
fn a_qualified_ctor_binding_keeps_its_module() {
    let src = "pub fn main() {\n\theaders::Header(name, value) = h\n\tname\n}\n";
    let out = fmt(src);
    assert!(out.contains("headers::Header(name, value) = h"), "{out}");
    assert_round_trips(&out);
}

#[test]
fn a_label_alone_in_a_pattern_stays_alone() {
    let src = "pub fn main() {\n\tUser(name:, age: a, ..) = u\n\tname\n}\n";
    let out = fmt(src);
    assert!(out.contains("User(name:, age: a, ..) = u"), "{out}");
    assert_round_trips(&out);
}

#[test]
fn a_long_backpass_pattern_wraps_and_re_parses() {
    let src = "fn f(r Result((Int, Int), Nil)) Result(Int, Nil) {\n\t(aaaaaaaaaaaaaaaaaaaaaa, bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb) <- result::then(result::map_err(r, fn(e) e))\n\tOk(aaaaaaaaaaaaaaaaaaaaaa + bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb)\n}\n";
    assert_round_trips(&fmt(src));
}

#[test]
fn a_short_pipeline_stays_on_one_line() {
    let src = "fn f(xs Array(Int)) Int {\n\tn = xs |> array::map(inc) |> array::length\n\tn\n}\n";
    assert_eq!(fmt(src), src);
}

/// A pipeline breaks at every `|>` or at none, and its stages line up
/// under its first value. After `=`, the whole chain moves down a level.
#[test]
fn a_long_pipeline_breaks_at_every_stage() {
    let stage = "array::filter(fn(word) string::length(word) > 0)";
    let src = format!(
        "fn f(line String) Array(String) {{\n\twords = line |> string::trim |> string::split(\" \") |> {stage}\n\tline |> string::trim |> string::split(\" \") |> {stage} |> array::reverse\n}}\n"
    );
    let out = fmt(&src);
    assert_eq!(
        out,
        format!(
            "fn f(line String) Array(String) {{\n\twords =\n\t\tline\n\t\t|> string::trim\n\t\t|> string::split(\" \")\n\t\t|> {stage}\n\tline\n\t|> string::trim\n\t|> string::split(\" \")\n\t|> {stage}\n\t|> array::reverse\n}}\n"
        )
    );
    assert_round_trips(&out);
}

#[test]
fn a_long_pipeline_after_a_label_moves_down_a_level() {
    let head = "socket::read_until(sock, buffer_size, deadline_in_milliseconds)";
    let src = format!(
        "fn f(sock Int) Reader {{\n\tr = Reader(read: {head} |> result::map_err(Transport), write: w)\n\tr\n}}\n"
    );
    let out = fmt(&src);
    assert_eq!(
        out,
        format!(
            "fn f(sock Int) Reader {{\n\tr = Reader(\n\t\tread:\n\t\t\t{head}\n\t\t\t|> result::map_err(Transport),\n\t\twrite: w,\n\t)\n\tr\n}}\n"
        )
    );
    assert_round_trips(&out);
}

/// A pipeline that doesn't start its line, like one before `or` or in a
/// lambda body, indents its `|>` lines so they don't sit under the binding.
#[test]
fn a_pipeline_mid_line_indents_its_stages() {
    let src = "fn f() Int {\n\tport = { os::get_env(\"PORT_NUMBER_FOR_THIS_SERVER\") or \"8080\" } |> binary::from_string |> binary::parse_int(binary::Dec) or 8080\n\tport\n}\n";
    let out = fmt(src);
    assert_eq!(
        out,
        "fn f() Int {\n\tport = { os::get_env(\"PORT_NUMBER_FOR_THIS_SERVER\") or \"8080\" }\n\t\t|> binary::from_string\n\t\t|> binary::parse_int(binary::Dec) or 8080\n\tport\n}\n"
    );
    assert_round_trips(&out);
}

#[track_caller]
fn assert_round_trips(out: &str) {
    match format(out) {
        FormatResult::Formatted { output } => {
            assert_eq!(out, output, "formatter not idempotent:\n{out}")
        }
        FormatResult::ParseFailed { .. } => {
            panic!("formatted output does not re-parse:\n{out}")
        }
        FormatResult::CommentsLost { comment } => {
            panic!("re-format lost the comment `{comment}`:\n{out}")
        }
        FormatResult::OutputInvalid(detail) => {
            panic!("re-format produced invalid output ({detail}):\n{out}")
        }
    }
}

/// Every `.sca` file in the repo, formatted at widths so narrow that layout
/// breaks every line it can. Each must come out as a valid program that
/// formats again. (It need not be a fixed point: at a width this narrow,
/// list items measure differently once their list is broken.)
#[test]
fn every_line_break_is_safe() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    let mut dirs = vec![
        root.join("examples"),
        root.join("crates/scarlet/tests/programs"),
        root.join("crates/scarlet_core/src/std"),
    ];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().and_then(|s| s.to_str()) == Some("sca") {
                files.push(path);
            }
        }
    }
    assert!(files.len() > 50, "found only {} files", files.len());
    for path in files {
        let name = path.display();
        let src = std::fs::read_to_string(&path).unwrap();
        for width in [0, 20, 40, 60] {
            let once = match format_at(&src, width) {
                FormatResult::Formatted { output } => output,
                FormatResult::ParseFailed { .. } => break,
                FormatResult::CommentsLost { comment } => {
                    panic!("{name} at width {width} lost the comment `{comment}`")
                }
                FormatResult::OutputInvalid(detail) => {
                    panic!("{name} at width {width}: {detail}")
                }
            };
            match format_at(&once, width) {
                FormatResult::Formatted { .. } => {}
                FormatResult::ParseFailed { errors } => {
                    panic!("{name} at width {width} does not re-parse: {errors:?}\n{once}")
                }
                FormatResult::CommentsLost { comment } => {
                    panic!("{name} at width {width} then lost the comment `{comment}`")
                }
                FormatResult::OutputInvalid(detail) => {
                    panic!("{name} at width {width} then: {detail}")
                }
            }
        }
    }
}

#[test]
fn idempotent_on_examples() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for dir in [
        root.join("examples"),
        root.join("crates/scarlet/tests/programs"),
    ] {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|s| s.to_str()) != Some("sca") {
                continue;
            }
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let src = std::fs::read_to_string(&path).unwrap();
            let once = match format(&src) {
                FormatResult::Formatted { output } => output,
                // Unparseable source is outside the formatter's remit.
                FormatResult::ParseFailed { .. } => continue,
                FormatResult::CommentsLost { comment } => {
                    panic!("formatting {name} lost the comment `{comment}`")
                }
                FormatResult::OutputInvalid(detail) => {
                    panic!("formatting {name} produced invalid output: {detail}")
                }
            };
            match format(&once) {
                FormatResult::Formatted { output } => {
                    assert_eq!(once, output, "formatter not idempotent for {name}")
                }
                FormatResult::ParseFailed { .. } => {
                    panic!("formatted output of {name} does not re-parse:\n{once}")
                }
                FormatResult::CommentsLost { comment } => {
                    panic!("re-formatting {name} lost the comment `{comment}`")
                }
                FormatResult::OutputInvalid(detail) => {
                    panic!("re-formatting {name} produced invalid output: {detail}")
                }
            }
        }
    }
}

#[test]
fn simple_cases() {
    #[rustfmt::skip]
        let cases: &[(&str, &str, &str)] = &[
            ("fn_body_always_breaks",
             "fn f(x Int) Int { if x > 0 { 1 } else { 2 } }\n",
             "fn f(x Int) Int {\n\tif x > 0 { 1 } else { 2 }\n}\n"),
            ("trivial_if_stays_inline_in_broken_body",
             "fn max(a Int, b Int) Int { if a > b { a } else { b } }\n",
             "fn max(a Int, b Int) Int {\n\tif a > b { a } else { b }\n}\n"),
            ("non_trivial_if_branch_breaks",
             "fn fu(id Int) Option { if id == 0 { None } else { Some(id) } }\n",
             "fn fu(id Int) Option {\n\tif id == 0 {\n\t\tNone\n\t} else {\n\t\tSome(id)\n\t}\n}\n"),
            ("if_chain_breaks_even_when_branches_trivial",
             "fn c(n Int) String { if n < 0 { \"neg\" } else if n == 0 { \"zero\" } else { \"pos\" } }\n",
             "fn c(n Int) String {\n\tif n < 0 {\n\t\t\"neg\"\n\t} else if n == 0 {\n\t\t\"zero\"\n\t} else {\n\t\t\"pos\"\n\t}\n}\n"),
            ("fn_body_braced",
             "fn perimeter(a Int, b Int, c Int) Int { a + b + c }\n",
             "fn perimeter(a Int, b Int, c Int) Int {\n\ta + b + c\n}\n"),
            ("type_shorthand_always_breaks",
             "type Point { Point(x Int, y Int) }\n",
             "type Point {\n\tx Int\n\ty Int\n}\n"),
            ("type_single_nullary_ctor_explicit",
             "type Nil { Nil }\n",
             "type Nil {\n\tNil\n}\n"),
            ("lambda_braceless",
             "f = fn(x Int) { x * 2 }\n",
             "f = fn(x Int) x * 2\n"),
            ("pub_preserves_blank_line",
             "pub fn a() Int { 1 }\n\npub fn b() Int { 2 }\n",
             "pub fn a() Int {\n\t1\n}\n\npub fn b() Int {\n\t2\n}\n"),
            ("type_multi_variant_explicit",
             "type Maybe(a) {\n\tJust(value A)\n\tNothing\n}\n",
             "type Maybe(a) {\n\tJust(value A)\n\tNothing\n}\n"),
            ("single_quotes_become_double",
             "x = 'hello'\n",
             "x = \"hello\"\n"),
            ("double_quotes_stay",
             "x = \"it's \\\"ok\\\"\"\n",
             "x = \"it's \\\"ok\\\"\"\n"),
            ("an_escaped_apostrophe_needs_no_escape",
             "x = 'it\\'s ' + \"it\\'s\"\n",
             "x = \"it's \" + \"it's\"\n"),
            ("a_double_quote_is_escaped",
             "x = 'say \"hi\"'\n",
             "x = \"say \\\"hi\\\"\"\n"),
            ("vm_attribute_on_own_line",
             "@vm(tcp_listen) pub fn listen(p Int) Result(Server, String)\n",
             "@vm(tcp_listen)\npub fn listen(p Int) Result(Server, String)\n"),
            ("external_type_body_less",
             "pub type Socket\n",
             "pub type Socket\n"),
            ("opaque_type_shorthand_round_trips",
             "pub opaque type Id {\n\tn Int\n}\n",
             "pub opaque type Id {\n\tn Int\n}\n"),
            ("opaque_type_variants_round_trips",
             "pub opaque type Maybe {\n\tYes\n\tNo\n}\n",
             "pub opaque type Maybe {\n\tYes\n\tNo\n}\n"),
            ("match_arms_newline_separated_no_commas",
             "match x { Ok(a) -> a\n Err(e) -> e }\n",
             "match x {\n\tOk(a) -> a\n\tErr(e) -> e\n}\n"),
            ("blank_line_preserved",
             "fn a() Int { 1 }\n\nfn b() Int { 2 }\n",
             "fn a() Int {\n\t1\n}\n\nfn b() Int {\n\t2\n}\n"),
            ("excess_blank_lines_collapse",
             "fn a() Int { 1 }\n\n\n\n\nfn b() Int { 2 }\n",
             "fn a() Int {\n\t1\n}\n\nfn b() Int {\n\t2\n}\n"),
            ("comment_only_body_has_no_leading_blank",
             "fn test() {    \n\t//\n}\n",
             "fn test() {\n\t//\n}\n"),
            ("short_or_pattern_stays_flat",
             "fn f(x Int) Int {\n\tmatch x {\n\t\t1 | 2 | 3 -> 10\n\t\t_ -> 0\n\t}\n}\n",
             "fn f(x Int) Int {\n\tmatch x {\n\t\t1 | 2 | 3 -> 10\n\t\t_ -> 0\n\t}\n}\n"),
            ("leading_pipe_is_normalized",
             "fn f(x Int) Int {\n\tmatch x {\n\t\t| 1\n\t\t| 2 -> 10\n\t\t_ -> 0\n\t}\n}\n",
             "fn f(x Int) Int {\n\tmatch x {\n\t\t1 | 2 -> 10\n\t\t_ -> 0\n\t}\n}\n"),
            ("wide_or_pattern_breaks_at_pipes",
             "fn f(e NetError) Nil {\n\tmatch e {\n\t\tErr(NotConnected) | Err(TimedOut) | Err(ConnectionRefused) | Err(BrokenPipe) | Err(AddrInUse) | Err(AddrNotAvailable) -> Nil\n\t\t_ -> Nil\n\t}\n}\n",
             "fn f(e NetError) Nil {\n\tmatch e {\n\t\tErr(NotConnected)\n\t\t| Err(TimedOut)\n\t\t| Err(ConnectionRefused)\n\t\t| Err(BrokenPipe)\n\t\t| Err(AddrInUse)\n\t\t| Err(AddrNotAvailable) -> Nil\n\t\t_ -> Nil\n\t}\n}\n"),
            ("backpass_single_binder",
             "fn f() Int {\n\tx <- g(1)\n\tx\n}\n",
             "fn f() Int {\n\tx <- g(1)\n\tx\n}\n"),
            ("backpass_binder_list_normalizes_spacing",
             "fn f() Int {\n\ta,b<-g(1,2)\n\ta + b\n}\n",
             "fn f() Int {\n\ta, b <- g(1, 2)\n\ta + b\n}\n"),
            ("backpass_discard_binder",
             "fn f() Int {\n\t_ <- g(1)\n\t0\n}\n",
             "fn f() Int {\n\t_ <- g(1)\n\t0\n}\n"),
            ("backpass_pattern_binders",
             "fn f() Int {\n\tNil <- g(1)\n\t(a,b) <- h(2)\n\ta + b\n}\n",
             "fn f() Int {\n\tNil <- g(1)\n\t(a, b) <- h(2)\n\ta + b\n}\n"),
            ("comment_preserved_above_fn",
             "// hello\nfn a() Int { 1 }\n",
             "// hello\nfn a() Int {\n\t1\n}\n"),
            ("multiline_block_comment_preserved_verbatim",
             "/* a\n   b */\nfn a() Int { 1 }\n",
             "/* a\n   b */\nfn a() Int {\n\t1\n}\n"),
        ];
    for (name, src, expected) in cases {
        let out = fmt(src);
        assert_eq!(out, *expected, "case `{name}`");
        assert_round_trips(&out);
    }
}

#[test]
fn long_if_breaks() {
    let src = "fn f(a Int, b Int, c Int) String { if !g(a, b, c) { \"Invalid\" } else if a == b && b == c { \"Equilateral\" } else if a == b || b == c || a == c { \"Isosceles\" } else { \"Scalene\" } }\n";
    let out = fmt(src);
    assert!(
        out.contains("\t} else if a == b && b == c {\n\t\t\"Equilateral\"\n"),
        "got:\n{out}"
    );
    assert!(
        out.contains("\t} else {\n\t\t\"Scalene\"\n\t}\n"),
        "got:\n{out}"
    );
}

#[test]
fn an_if_keeps_the_form_it_was_written_in() {
    for src in [
        "x = if a then 1 else 2\n",
        "x = if a { 1 } else { 2 }\n",
        "x = if a then f(1) else if b then g(2) else h(3)\n",
        "fn f() {\n\tif is_admin(user) {\n\t\tthing()\n\t} else {\n\t\tother_thing()\n\t}\n}\n",
        "fn f() {\n\tif is_admin(user) {\n\t\tthing()\n\t} else not_found()\n}\n",
        "x = if a then 1 else {\n\ty = 2\n\ty\n}\n",
        "x = if a then 1 else if b {\n\t2\n} else 3\n",
    ] {
        assert_eq!(fmt(src), src);
    }
}

#[test]
fn a_comment_above_a_bare_branch_indents_it() {
    let src = "x = if a then\n\t// one\n\t1\n\telse\n\t\t// two\n\t\t2\n";
    assert_eq!(fmt("x = if a then\n// one\n1 else // two\n2\n"), src);
    assert_round_trips(src);
    let src = "fn f() {\n\tif a {\n\t\t1\n\t} else\n\t\t// two\n\t\tf(2)\n}\n";
    assert_eq!(fmt(src), src);
}

#[test]
fn then_before_a_block_is_the_block() {
    assert_eq!(
        fmt("x = if a then { 1 } else { 2 }\n"),
        "x = if a { 1 } else { 2 }\n"
    );
}

#[test]
fn a_long_then_chain_puts_each_else_on_its_own_line() {
    let long = "a".repeat(40);
    let src = format!("x = if {long} then f(1) else if {long} then g(2) else h(3)\n");
    let out = fmt(&src);
    assert_eq!(
        out,
        format!("x = if {long} then f(1)\n\telse if {long} then g(2)\n\telse h(3)\n")
    );
    assert_round_trips(&out);
}

#[test]
fn a_final_list_hugs_the_parens() {
    let src = "x = Object([(\"resource\", Str(resource_uri)), (\"authorization_servers\", List([Str(auth_server)])), (\"scopes\", texts(scopes))])\n";
    let out = fmt(src);
    assert_eq!(
        out,
        "x = Object([\n\t(\"resource\", Str(resource_uri)),\n\t(\"authorization_servers\", List([Str(auth_server)])),\n\t(\"scopes\", texts(scopes)),\n])\n"
    );
    assert_round_trips(&out);
    assert_eq!(
        fmt("x = Object([(\"a\", 1)])\n"),
        "x = Object([(\"a\", 1)])\n"
    );
    let out = fmt(&format!(
        "x = f(\"{}\", [{}])\n",
        "a".repeat(40),
        "b, ".repeat(30)
    ));
    assert!(
        out.starts_with(&format!("x = f(\"{}\", [\n\tb,\n", "a".repeat(40))),
        "{out}"
    );
    assert_round_trips(&out);
}

/// A lambda's parameters never go one per line: that buries the `fn`
/// under its own arguments and reads as a call rather than a function.
#[test]
fn a_lambdas_parameters_stay_on_one_line() {
    let src = "x = array.fold(\n\tcode_points,\n\tzero,\n\tfn(acc, cp) result.then(\n\t\tacc,\n\t\tfn(bytes) result.map(encode_scalar_value(cp), fn(next) binary.append(bytes, next)),\n\t),\n)\n";
    assert_eq!(fmt(src), src);
    assert_round_trips(src);
    // A named function still breaks its parameters when the line is long.
    let long = "a".repeat(40);
    let out = fmt(&format!("fn f({long} Int, {long} Int) Int {{\n\t1\n}}\n"));
    assert_eq!(
        out,
        format!("fn f(\n\t{long} Int,\n\t{long} Int,\n) Int {{\n\t1\n}}\n")
    );
}

#[test]
fn a_final_binary_literal_hugs_the_parens() {
    let src = "x = Ok(<<\n\t224 + int.bitwise_shift_right(code_point, 12):8,\n\t128 + int.bitwise_and(int.bitwise_shift_right(code_point, 6), 63):8,\n\t128 + int.bitwise_and(code_point, 63):8,\n>>)\n";
    assert_eq!(fmt(src), src);
    assert_round_trips(src);
}

/// A call that hugs may itself be hugged, so the parens close together
/// rather than each on its own line.
#[test]
fn a_final_call_that_hugs_is_hugged_too() {
    let src = "x = Timer(waiting: spawn_unlinked(fn() {\n\tsleep(delay_ms)\n\tsend(subject, message)\n}))\n";
    assert_eq!(fmt(src), src);
    assert_round_trips(src);
    // Not the last argument, so the fallback still breaks one per line.
    let out = fmt("x = f(g(fn() {\n\tone()\n\ttwo()\n}), 2)\n");
    assert_eq!(
        out,
        "x = f(\n\tg(fn() {\n\t\tone()\n\t\ttwo()\n\t}),\n\t2,\n)\n"
    );
    assert_round_trips(&out);
}

#[test]
fn a_list_does_not_hug_when_the_head_does_not_fit() {
    let head = "a".repeat(100);
    let out = fmt(&format!("x = f({head}, [1, 2])\n"));
    assert_eq!(out, format!("x = f(\n\t{head},\n\t[1, 2],\n)\n"));
}

#[test]
fn a_list_does_not_hug_after_another_list() {
    let items = "item, ".repeat(12);
    let out = fmt(&format!("x = f([{items}], [{items}])\n"));
    assert!(out.starts_with("x = f(\n\t[item, "), "{out}");
    assert_round_trips(&out);
}

#[test]
fn a_call_around_a_hugging_call_breaks_as_it_does_for_a_lambda() {
    let out = fmt(&format!(
        "x = Ok(Object([(\"{}\", 1), (\"b\", 2)]))\n",
        "a".repeat(80)
    ));
    assert!(out.starts_with("x = Ok(\n\tObject([\n\t\t(\""), "{out}");
    assert_round_trips(&out);
}

#[test]
fn an_operator_chain_breaks_every_operand() {
    let src = "fn f(c Int) Bool {\n\tunreserved = { c >= 48 && c <= 57 } || { c >= 65 && c <= 90 } || { c >= 97 && c <= 122 } || c == 45 || c == 46 || c == 95 || c == 126\n\tunreserved\n}\n";
    let out = fmt(src);
    assert_eq!(
        out,
        "fn f(c Int) Bool {\n\tunreserved = { c >= 48 && c <= 57 } ||\n\t\t{ c >= 65 && c <= 90 } ||\n\t\t{ c >= 97 && c <= 122 } ||\n\t\tc == 45 ||\n\t\tc == 46 ||\n\t\tc == 95 ||\n\t\tc == 126\n\tunreserved\n}\n"
    );
    assert_round_trips(&out);
    // `+` and `-` are one level, so they chain together; `*` binds
    // tighter and stays whole.
    let long = "a".repeat(30);
    let out = fmt(&format!("x = {long} - {long} * 2 + {long} - 1\n"));
    assert_eq!(
        out,
        format!("x = {long} -\n\t{long} * 2 +\n\t{long} -\n\t1\n")
    );
    assert_round_trips(&out);
    assert_eq!(fmt("x = a || b && c\n"), "x = a || b && c\n");
}

#[test]
fn match_wildcard_arms_format_as_written() {
    let out = fmt("fn f(x Int) String { match x { 1 -> \"one\"\n _ -> \"other\" } }\n");
    assert!(out.contains("_ -> \"other\"\n"), "got: {out}");
    let out2 = fmt("fn g(o Option(Int)) String { match o { Some(_) -> \"y\"\n _ -> \"n\" } }\n");
    assert!(out2.contains("Some(_) -> \"y\""), "nested _: {out2}");
    assert!(out2.contains("_ -> \"n\"\n"), "top _: {out2}");
}

#[test]
fn or_handler_overflow_breaks_subject_args() {
    // When `subject(...) or e -> handler(...)` overflows, the subject's
    // arguments break and the handler stays intact, not the reverse.
    let out = fmt(
        "http.serve(\"0.0.0.0\", 8080, fn(_req) http.text(\"Hello from scarlet/http!\")) or e -> println(\"serve failed: ${e}\")\n",
    );
    assert_eq!(
        out,
        "http.serve(\n\t\"0.0.0.0\",\n\t8080,\n\tfn(_req) http.text(\"Hello from scarlet/http!\"),\n) or e -> println(\"serve failed: ${e}\")\n"
    );
}

#[test]
fn or_block_handler_keeps_subject_flat() {
    // A `{ … }` handler is the natural break point, so the subject stays
    // flat and the block body breaks.
    let src = "http.serve(\"0.0.0.0\", 8080, fn(_req) http.text(\"Hello from scarlet/http!\")) or e -> {\n\tprintln(\"serve failed: ${e}\")\n}\n";
    assert_eq!(fmt(src), src);
}

#[test]
fn long_call_breaks_per_arg() {
    let out = fmt(
        "x = MakeThing(first_label: some_function_call(a, b, c), second_label: another_one(d, e, f), third_label: yet_more(g, h, i))\n",
    );
    assert!(out.contains("MakeThing(\n\tfirst_label: "), "got:\n{out}");
    assert!(out.contains(",\n\tthird_label: "), "got:\n{out}");
    assert!(out.contains(",\n)\n"), "got:\n{out}");
}

#[test]
fn block_lambda_arg_hugs_call_parens() {
    // A lone multi-statement lambda hugs the parentheses.
    let src = "scheduler.spawn(fn() {\n\tprintln(\"Hello!\")\n\tprintln(\"Hello!\")\n})\n";
    assert_eq!(fmt(src), src);
}

#[test]
fn match_arg_hugs_call_parens() {
    let src = "println(match 10 {\n\t0 -> \"zero\"\n\t_ -> \"else\"\n})\n";
    assert_eq!(fmt(src), src);
}

#[test]
fn block_lambda_after_leading_args_hugs() {
    // Leading arguments stay on the call head line; the lambda hugs.
    let src = "scheduler.run_after(1000, fn() {\n\tprintln(\"a\")\n\tprintln(\"b\")\n})\n";
    assert_eq!(fmt(src), src);
}

#[test]
fn hug_falls_back_when_head_does_not_fit() {
    // The head line is over the limit, so every argument gets its own.
    let out = fmt(&format!(
        "serve(\"{}\", \"{}\", fn(request) {{\n\thandle(request)\n\thandle(request)\n}})\n",
        "a".repeat(40),
        "b".repeat(40),
    ));
    assert_eq!(
        out,
        format!(
            "serve(\n\t\"{}\",\n\t\"{}\",\n\tfn(request) {{\n\t\thandle(request)\n\t\thandle(request)\n\t}},\n)\n",
            "a".repeat(40),
            "b".repeat(40),
        )
    );
}

#[test]
fn multiline_lambda_in_non_final_position_breaks_args() {
    // Only a final block-shaped argument hugs.
    let src = "f(fn() {\n\ta()\n\tb()\n}, other)\n";
    assert_eq!(
        fmt(src),
        "f(\n\tfn() {\n\t\ta()\n\t\tb()\n\t},\n\tother,\n)\n"
    );
}

#[test]
fn hugging_call_as_argument_breaks_outer_call() {
    // A hug never leaks upward: an outer call still breaks per-argument.
    let src = "outer(inner(fn() {\n\ta()\n\tb()\n}), other)\n";
    assert_eq!(
        fmt(src),
        "outer(\n\tinner(fn() {\n\t\ta()\n\t\tb()\n\t}),\n\tother,\n)\n"
    );
}

#[test]
fn hugged_lambda_body_still_wraps_wide_statements() {
    // Statements inside a hugged lambda still re-probe their own width.
    let a = "a".repeat(45);
    let b = "b".repeat(45);
    let src = format!("go(fn() {{\n\tfirst()\n\tprocess(\"{a}\", \"{b}\")\n}})\n");
    let expected =
        format!("go(fn() {{\n\tfirst()\n\tprocess(\n\t\t\"{a}\",\n\t\t\"{b}\",\n\t)\n}})\n");
    assert_eq!(fmt(&src), expected);
}

#[test]
fn hugged_call_chains_format_each_link() {
    // Each call in a member chain makes its own hug decision.
    let src = "list.map(fn(x) {\n\tlog(x)\n\tx * 2\n}).filter(fn(x) {\n\tlog(x)\n\tx > 0\n})\n";
    assert_eq!(fmt(src), src);
}

#[test]
fn binary_literal_round_trip() {
    let out = fmt("x = <<1, 2:4, n:size(w), s:utf8, body:bytes(len), tail:binary>>\n");
    assert_eq!(
        out,
        "x = <<1, 2:4, n:size(w), s:utf8, body:bytes(len), tail:binary>>\n"
    );
    assert_eq!(fmt("x = <<>>\n"), "x = <<>>\n");
}

#[test]
fn binary_pattern_round_trip() {
    let out = fmt("fn f(b Binary) Int { match b { <<a, b:4, _:4, ..rest>> -> a\n _ -> 0 } }\n");
    assert!(out.contains("<<a, b:4, _:4, ..rest>> -> a"), "got:\n{out}");
    let out2 = fmt("fn g(b Binary) Int { match b { <<x, ..>> -> x\n <<>> -> 0 } }\n");
    assert!(out2.contains("<<x, ..>> -> x"), "got:\n{out2}");
    assert!(out2.contains("<<>> -> 0"), "got:\n{out2}");
}

#[test]
fn constructor_pattern_rest_wraps_without_trailing_comma() {
    // A wrapped `..` rest marker must not emit `..,`: the parser rejects a
    // comma after `..`, so the output would not parse.
    let src = "result = match value {\n\tVeryLongConstructorNameHere(first_argument_name, \
                   second_argument_name, third_argument_name, fourth_argument_name, \
                   fifth_argument_name, ..) -> 1\n\t_ -> 0\n}\n";
    let out = fmt(src);
    assert!(
        out.contains("\t\t.."),
        "expected rest marker to wrap:\n{out}"
    );
    assert!(!out.contains("..,"), "trailing comma after rest:\n{out}");
    // The formatted output must re-parse and be idempotent (round-trip).
    assert_round_trips(&out);
}

#[test]
fn long_match_arm_breaks_after_arrow_not_inside_pattern() {
    // An overflowing arm wraps after the `->`, one indent deeper. The
    // pattern must stay intact.
    let src = "match x {\n\tSome(value) -> quite_long_function_name(value, another_argument, \
                   and_yet_another_long_argument_here)\n\t_ -> None\n}\n";
    let out = fmt(src);
    assert_eq!(
        out,
        "match x {\n\tSome(value) ->\n\t\tquite_long_function_name(value, another_argument, \
             and_yet_another_long_argument_here)\n\t_ -> None\n}\n"
    );
}

#[test]
fn long_guarded_match_arm_breaks_after_arrow() {
    // Pattern and guard share the head line; the body wraps after `->`.
    let src = "match x {\n\tSome(value) if value <= some_limit(value) - tolerance -> \
                   build_result(value, scale_factor, offset)\n\t_ -> None\n}\n";
    let out = fmt(src);
    assert_eq!(
        out,
        "match x {\n\tSome(value) if value <= some_limit(value) - tolerance ->\n\t\t\
             build_result(value, scale_factor, offset)\n\t_ -> None\n}\n"
    );
}

#[test]
fn match_arm_block_body_hugs_arrow() {
    // A body that ends the line itself keeps hugging the arrow.
    let src = "match x {\n\tSome(v) -> match v {\n\t\t0 -> \"zero\"\n\t\t_ -> \"more\"\n\t}\n\t_ -> \"none\"\n}\n";
    assert_eq!(fmt(src), src);
}

#[test]
fn if_chain_and_struct_call_break() {
    // Inlined rather than read from examples/: an over-long if-chain and
    // struct call must each break one clause or argument per line.
    let src = "type TriangleInfo {\n\tsides String\n\ttriangle_type String\n\tperimeter Int\n\tvalid Bool\n}\n\nfn classify_triangle(a, b, c) String {\n\tif a + b <= c {\n\t\t\"Invalid\"\n\t} else if a == b && b == c {\n\t\t\"Equilateral\"\n\t} else {\n\t\t\"Scalene\"\n\t}\n}\n\nfn analyze_triangle(a, b, c) {\n\tTriangleInfo(sides: \"${a}, ${b}, ${c}\", triangle_type: classify_triangle(a, b, c), perimeter: a + b + c, valid: a + b > c)\n}\n";
    let out = fmt(src);
    assert!(
        out.contains("\t} else if a == b && b == c {\n\t\t\"Equilateral\"\n"),
        "if-chain should break per-clause:\n{out}"
    );
    assert!(
        out.contains("TriangleInfo(\n\t\tsides: "),
        "struct call should break per-arg:\n{out}"
    );
}

/// Top-level items the source parses into, to prove the formatter does not
/// split or merge program structure.
fn top_level_items(src: &str) -> usize {
    let mut s = scanner::new_scanner(src.to_string());
    let (toks, diags) = s.scan_all();
    let p = parser::new_parser_from_tokens(toks, diags);
    p.parse_program().ast.body.len()
}

#[test]
fn wrapped_subtraction_keeps_operator_trailing() {
    // A wrapping subtraction must keep the operator at the END of the left
    // operand's line. The parser reads a leading `-` as a fresh unary
    // expression, so a wrapped operator silently re-parses as two
    // statements.
    let src = format!("result = {} - {}\n", "a".repeat(50), "b".repeat(50));
    assert_eq!(top_level_items(&src), 1, "source should be one binding");

    let out = fmt(&src);
    assert!(
        out.contains('\n'),
        "expected the subtraction to wrap:\n{out}"
    );
    for line in out.lines() {
        assert!(
            !line.trim_start().starts_with('-'),
            "operator wrapped to the start of a continuation line; this \
                 re-parses as unary negation:\n{out}"
        );
    }

    // Must re-parse to one binding, not a binding plus a stray unary.
    assert_eq!(
        top_level_items(&out),
        1,
        "formatting split one subtraction into two top-level items:\n{out}"
    );
}

#[test]
fn nested_unary_minus_keeps_space() {
    // `- -x` must not print as `--x`: the scanner relexes `--` as the
    // rejected decrement token, corrupting a valid file in place.
    let out = fmt("x = - -y\n");
    assert_eq!(out, "x = - -y\n");
    assert!(
        !out.contains("--"),
        "nested unary minus collapsed into the `--` decrement token:\n{out}"
    );
    assert_round_trips(&out);
}

#[test]
fn deeply_nested_unary_minus_keeps_spaces() {
    let out = fmt("x = - - -y\n");
    assert_eq!(out, "x = - - -y\n");
    assert_round_trips(&out);
}

/// Dropping a pattern's module qualifier changes the program: `io::NotFound`
/// resolves through the module, `NotFound` needs an import.
#[test]
fn a_qualified_constructor_pattern_keeps_its_qualifier() {
    let src = "match e {\n\tio::NotFound(path) -> path\n\tio::Denied -> \"no\"\n\t_ -> \"\"\n}\n";
    let out = fmt(src);
    assert!(out.contains("io::NotFound(path)"), "{out}");
    assert!(out.contains("io::Denied"), "{out}");
    assert_round_trips(&out);
}

/// Regression: a comment between the fields of a single-constructor
/// shorthand used to be deleted outright.
#[test]
fn shorthand_fields_keep_their_comments() {
    let src = "type R {\n\ta Int\n\t// why b matters\n\tb String\n}\n";
    let out = fmt(src);
    assert!(out.contains("// why b matters"), "comment deleted:\n{out}");
    assert_round_trips(&out);
}

#[test]
fn call_argument_comments_survive() {
    let src = "f(\n\t// why a\n\ta,\n\tb,\n)\n";
    let out = fmt(src);
    assert!(out.contains("// why a"), "comment deleted:\n{out}");
    assert_round_trips(&out);
}

#[test]
fn labeled_and_final_call_argument_comments_survive() {
    let out = fmt("f(\n\ta,\n\t// why b\n\tlabel: b,\n)\n");
    assert!(out.contains("// why b"), "comment deleted:\n{out}");
    assert_round_trips(&out);
}

/// A punned argument keeps its shorthand. If the formatter expanded `now:`
/// to `now: now`, the shorthand would be gone the first time a file got
/// formatted.
#[test]
fn punned_call_argument_keeps_its_shorthand() {
    let out = fmt("f(now:, self:)\n");
    assert!(out.contains("f(now:, self:)"), "shorthand lost:\n{out}");
    assert_round_trips(&out);
}

/// The mirror of the test above: a labelled argument spelled out in full
/// is not collapsed into a pun just because its value happens to share
/// the label's name. `punned` is a fact the parser records at the site
/// that observed it, not one re-derived later by comparing spellings.
#[test]
fn spelled_out_argument_is_not_collapsed_into_a_pun() {
    let out = fmt("f(now: now)\n");
    assert!(
        out.contains("f(now: now)"),
        "expanded form was collapsed:\n{out}"
    );
    assert_round_trips(&out);
}

/// Punning composes with the other call-argument shapes: an ordinary
/// labelled argument and a record-update spread in the same call.
#[test]
fn punning_mixes_with_labeled_and_spread_arguments() {
    let out = fmt("Segment(..s, from:, label: \"up\")\n");
    assert!(
        out.contains("Segment(..s, from:, label: \"up\")"),
        "mixed call argument shapes not preserved:\n{out}"
    );
    assert_round_trips(&out);
}

#[test]
fn array_element_comments_survive() {
    let out = fmt("x = [\n\t// first\n\t1,\n\t2,\n]\n");
    assert!(out.contains("// first"), "comment deleted:\n{out}");
    assert_round_trips(&out);
}

#[test]
fn array_spread_comments_survive() {
    let out = fmt("x = [\n\t1,\n\t// the rest\n\t..rest,\n]\n");
    assert!(out.contains("// the rest"), "comment deleted:\n{out}");
    assert_round_trips(&out);
}

#[test]
fn call_spread_argument_comments_survive() {
    let out = fmt("f(\n\t// rest\n\t..args,\n)\n");
    assert!(out.contains("// rest"), "comment deleted:\n{out}");
    assert_round_trips(&out);
}

#[test]
fn tuple_element_comments_survive() {
    let out = fmt("x = (\n\t// left\n\t1,\n\t2,\n)\n");
    assert!(out.contains("// left"), "comment deleted:\n{out}");
    assert_round_trips(&out);
}

#[test]
fn binary_segment_comments_survive() {
    let out = fmt("x = <<\n\t// version byte\n\t1,\n\t2,\n>>\n");
    assert!(out.contains("// version byte"), "comment deleted:\n{out}");
    assert_round_trips(&out);
}

#[test]
fn if_condition_comments_survive() {
    let out = fmt("if // check sign\nx > 0 {\n\t1\n} else {\n\t2\n}\n");
    assert!(out.contains("// check sign"), "comment deleted:\n{out}");
    assert_round_trips(&out);
}

#[test]
fn else_if_gap_comments_survive() {
    // A comment between `else` and `if` attaches to the nested `if` token.
    let out = fmt("if a {\n\t1\n} else // why\nif b {\n\t2\n} else {\n\t3\n}\n");
    assert!(out.contains("// why"), "comment deleted:\n{out}");
    assert_round_trips(&out);
}

#[test]
fn else_brace_gap_comments_survive() {
    // A comment between `else` and its `{` attaches to the brace token.
    let out = fmt("if a {\n\t1\n} else // fallback\n{\n\t2\n}\n");
    assert!(out.contains("// fallback"), "comment deleted:\n{out}");
    assert_round_trips(&out);
}

/// A comment with no emission site (before the `else` keyword, whose token
/// has no AST span) must make `format` refuse to produce output.
#[test]
fn unemittable_comment_is_a_noop_not_a_deletion() {
    let src = "if a {\n\t1\n}\n// stranded before else\nelse {\n\t2\n}\n";
    match format(src) {
        FormatResult::CommentsLost { comment } => {
            assert_eq!(comment, "// stranded before else")
        }
        // If a future change learns to emit this comment, it must actually
        // be present in the output.
        FormatResult::Formatted { output } => {
            assert!(output.contains("// stranded before else"), "{output}")
        }
        FormatResult::ParseFailed { errors } => panic!("parse failed: {errors:?}"),
        FormatResult::OutputInvalid(detail) => {
            panic!("formatter produced invalid output: {detail}")
        }
    }
}

/// A dropped comment whose text also appears in a string literal must still
/// be detected: the check compares trivia, never raw substrings.
#[test]
fn comment_loss_is_not_masked_by_string_literal() {
    let src = "s = \"// stranded before else\"\nif a {\n\t1\n}\n// stranded before else\nelse {\n\t2\n}\n";
    match format(src) {
        FormatResult::CommentsLost { comment } => {
            assert_eq!(comment, "// stranded before else")
        }
        // If a future change learns to emit this comment, it must appear
        // in addition to the string literal.
        FormatResult::Formatted { output } => {
            assert!(
                output.matches("// stranded before else").count() >= 2,
                "{output}"
            )
        }
        FormatResult::ParseFailed { errors } => panic!("parse failed: {errors:?}"),
        FormatResult::OutputInvalid(detail) => {
            panic!("formatter produced invalid output: {detail}")
        }
    }
}

/// A dropped comment that is a prefix of a surviving one must still be
/// detected: `// TODO` vs `// TODO: fix`.
#[test]
fn comment_loss_is_not_masked_by_longer_comment() {
    let src = "// stranded before else and more\nif a {\n\t1\n}\n// stranded before else\nelse {\n\t2\n}\n";
    match format(src) {
        FormatResult::CommentsLost { comment } => {
            assert_eq!(comment, "// stranded before else")
        }
        FormatResult::Formatted { output } => {
            assert!(output.contains("// stranded before else\n"), "{output}")
        }
        FormatResult::ParseFailed { errors } => panic!("parse failed: {errors:?}"),
        FormatResult::OutputInvalid(detail) => {
            panic!("formatter produced invalid output: {detail}")
        }
    }
}

#[test]
fn unary_minus_before_non_minus_stays_tight() {
    // Only the `--` collision needs a separating space.
    assert_eq!(fmt("x = -y\n"), "x = -y\n");
    assert_eq!(fmt("x = -f(y)\n"), "x = -f(y)\n");
    assert_eq!(fmt("x = -y.z\n"), "x = -y.z\n");
}

#[test]
fn double_not_stays_adjacent() {
    // `!!x` lexes as two tokens and parses fine, so it stays tight.
    let out = fmt("x = !!y\n");
    assert_eq!(out, "x = !!y\n");
    assert!(
        matches!(format(&out), FormatResult::Formatted { .. }),
        "formatted output does not re-parse:\n{out}"
    );
}

#[test]
fn interpolation_stays_flat_when_it_fits() {
    let out = fmt("x = \"value: ${a + b}\"\n");
    assert_eq!(out, "x = \"value: ${a + b}\"\n");
    assert_round_trips(&out);
}

#[test]
fn hard_breaking_match_inside_interpolation_gets_real_layout() {
    // A `match` inside `${…}` must lay out at the enclosing indent, not be
    // spliced in as text pre-rendered at indent 0.
    let src =
        "fn f(y Int) String {\n\t\"value: ${match y { 0 -> \"zero\"\n _ -> \"other\" }}\"\n}\n";
    let out = fmt(src);
    assert_eq!(
        out,
        "fn f(y Int) String {\n\t\"value: ${match y {\n\t\t0 -> \"zero\"\n\t\t_ -> \"other\"\n\t}}\"\n}\n"
    );
    assert_round_trips(&out);
}

#[test]
fn wide_interpolated_expression_breaks_at_enclosing_indent() {
    // The sub-expression joins the enclosing layout, so an overflow breaks
    // with real indentation instead of being measured at infinite width.
    let src = "fn f() String {\n\t\"r: ${wwwwwwwwwwwwwwwwwwwwwwwwwwwwww(aaaaaaaaaaaaaaaaaaaaaaaaaa, bbbbbbbbbbbbbbbbbbbbbbbbbb, cccccc)}\"\n}\n";
    let out = fmt(src);
    assert!(
        out.contains("(\n\t\taaaaaaaaaaaaaaaaaaaaaaaaaa,\n"),
        "expected the call to break per-argument at the enclosing indent, got:\n{out}"
    );
    assert_round_trips(&out);
}

#[test]
fn lambda_inside_interpolation_is_idempotent() {
    // A call inside `${…}` whose last argument is a lambda whose body is
    // itself an interpolated string. Pass 1 used to emit a broken `read(`
    // and a hard-broken `fn(\n d,\n)`; pass 2 then hugged the call because
    // those HardLines made the lambda hug-eligible. fmt(fmt(x)) != fmt(x).
    let src = "println(\"b.1 is null   ${read(sample, fn(d) \"${option.unwrap(option.map(option.then(json.field(d, \"b\"), fn(b) json.index(b, 1)), json.is_null), False)}\")}\")\n";
    let out = fmt(src);
    assert_round_trips(&out);
}

/// The T-155 family: a lambda as the last call argument whose body is a
/// bare wrapping expression. Same HardLine-then-hug flip as
/// `lambda_inside_interpolation_is_idempotent`, no string involved. A
/// lambda's parameters no longer wrap at all, so the flip has nowhere to
/// start; what is left to hold is that each still re-formats to itself.
fn assert_trailing_lambda_wraps_and_is_fixed(src: &str) {
    let out = fmt(src);
    assert!(!out.contains("fn(\n"), "lambda params wrapped:\n{out}");
    assert_round_trips(&out);
}

#[test]
fn trailing_lambda_with_wrapping_constructor_is_idempotent() {
    assert_trailing_lambda_wraps_and_is_fixed(
        "update_bucket(limiter, key, fn(held) Bucket(remaining: Known(0), reset_at: clock.now(), last_refill: clock.now(), capacity: held.capacity))\n",
    );
}

#[test]
fn trailing_lambda_with_wrapping_or_chain_is_idempotent() {
    assert_trailing_lambda_wraps_and_is_fixed(
        "check(limiter, key, fn(held) held.remaining == 0 || held.reset_at < now || held.capacity < wanted || held.tokens < needed)\n",
    );
}

#[test]
fn trailing_lambda_with_wrapping_call_body_is_idempotent() {
    assert_trailing_lambda_wraps_and_is_fixed(
        "array.map(data, fn(opcode) expect.is_false(opcode == websocket.Opcode.Continuation || opcode == websocket.Opcode.Text || opcode == websocket.Opcode.Binary))\n",
    );
}

#[test]
fn hugged_trailing_lambda_form_is_idempotent() {
    // The second-pass form the bug used to emit, starting already hugged.
    let hugged = "update_bucket(limiter, key, fn(\n\theld,\n) Bucket(\n\tremaining: Known(0),\n\treset_at: clock.now(),\n\tlast_refill: clock.now(),\n\tcapacity: held.capacity,\n))\n";
    let out = fmt(hugged);
    assert_round_trips(&out);
}

#[test]
fn short_pipe_chain_stays_flat() {
    let out = fmt("fn f() {\n\ta |> f() |> g()\n}\n");
    assert_eq!(out, "fn f() {\n\ta |> f() |> g()\n}\n");
    assert_round_trips(&out);
}

#[test]
fn long_pipe_chain_breaks_one_stage_per_line() {
    // Written already flat; over MAX_WIDTH so the formatter must re-break
    // it rather than leave the pipeline exploding into nested calls
    // (which was the whole cost this operator exists to avoid).
    let out = fmt(
        "fn f() {\n\tsome_long_starting_value_here_indeed |> first_transform_step_function() |> second_transform_step_function() |> third_and_final_transform_step_function()\n}\n",
    );
    assert!(
        out.contains("\n\t|> second_transform_step_function()\n"),
        "expected a broken pipe chain, got:\n{out}"
    );
    assert_round_trips(&out);
}

/// `src` formats to `want`, which has no error and formats to itself.
#[track_caller]
fn rewrites(src: &str, want: &str) {
    let out = fmt(src);
    assert_eq!(out, want);
    assert_round_trips(&out);
    let mut scanner = scanner::new_scanner(out);
    let parsed = parser::new_parser(&mut scanner).parse_program();
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
}

#[test]
fn single_quoted_strings_become_double_quoted() {
    rewrites("x = 'a'\n", "x = \"a\"\n");
    rewrites("x = ''\n", "x = \"\"\n");
    rewrites("x = 'it\\'s'\n", "x = \"it's\"\n");
    rewrites("x = 'say \"hi\"'\n", "x = \"say \\\"hi\\\"\"\n");
    rewrites("x = 'a \\\"b\\\"'\n", "x = \"a \\\"b\\\"\"\n");
    rewrites("x = '\\\\\\n\\t\\$y'\n", "x = \"\\\\\\n\\t\\$y\"\n");
    rewrites("x = '\\\\\"'\n", "x = \"\\\\\\\"\"\n");
}

#[test]
fn a_double_quoted_string_needs_no_escaped_apostrophe() {
    rewrites("x = \"it\\'s\"\n", "x = \"it's\"\n");
    rewrites("x = \"it's\"\n", "x = \"it's\"\n");
}

#[test]
fn single_quoted_interpolations_become_double_quoted() {
    rewrites("x = 'a ${b} c'\n", "x = \"a ${b} c\"\n");
    rewrites("x = 'a $b'\n", "x = \"a ${b}\"\n");
    rewrites(
        "x = 'it\\'s \"${b}\" \\${c}'\n",
        "x = \"it's \\\"${b}\\\" \\${c}\"\n",
    );
    rewrites("x = 'a ${'b'} c'\n", "x = \"a ${\"b\"} c\"\n");
    rewrites("x = \"a ${'b'} c\"\n", "x = \"a ${\"b\"} c\"\n");
    rewrites("x = 'a ${\"b\"} c'\n", "x = \"a ${\"b\"} c\"\n");
    rewrites(
        "x = '${f('${'it\\'s'}', \"\\\"\")}'\n",
        "x = \"${f(\"${\"it's\"}\", \"\\\"\")}\"\n",
    );
}

#[test]
fn single_quoted_patterns_become_double_quoted() {
    rewrites(
        "fn f(x) {\n\tmatch x {\n\t\t'a' -> 1\n\t\t'\"' -> 2\n\t\t_ -> 3\n\t}\n}\n",
        "fn f(x) {\n\tmatch x {\n\t\t\"a\" -> 1\n\t\t\"\\\"\" -> 2\n\t\t_ -> 3\n\t}\n}\n",
    );
    rewrites(
        "fn f(x) {\n\tmatch x {\n\t\t<<'GET ', ..r>> -> r\n\t\tr -> r\n\t}\n}\n",
        "fn f(x) {\n\tmatch x {\n\t\t<<\"GET \", ..r>> -> r\n\t\tr -> r\n\t}\n}\n",
    );
}

#[test]
fn a_comment_keeps_its_quotes() {
    rewrites(
        "// don't 'quote' \"me\"\nx = 'a'\n// it's 'b'\n\n/** 'c' isn't */\nfn f() {\n\t'd'\n}\n",
        "// don't 'quote' \"me\"\nx = \"a\"\n// it's 'b'\n\n/** 'c' isn't */\nfn f() {\n\t\"d\"\n}\n",
    );
    rewrites("x = '// no'\n", "x = \"// no\"\n");
}

/// A string grows when its quotes need an escape, and then a line may break.
#[test]
fn a_line_made_too_long_by_escapes_breaks() {
    let word = "a".repeat(45);
    rewrites(
        &format!("x = f('{word}\"', '{word}')\n"),
        &format!("x = f(\n\t\"{word}\\\"\",\n\t\"{word}\",\n)\n"),
    );
}

/// Only a whole string in single quotes is the formatter's to mend.
#[test]
fn single_quotes_beside_another_error_are_refused() {
    for src in [
        "x = 'a'\ny = )\n",
        "x = 'a'\ny = 'b\n",
        "x = 'a'\ny = \"b\n",
        "x = 'a\\q'\n",
        "x = 'a ${b'\n",
        "x = don't\n",
    ] {
        match format(src) {
            FormatResult::ParseFailed { errors } => assert!(
                errors.iter().all(|d| !d.problem.is_single_quoted_string()),
                "{src}: {errors:?}"
            ),
            FormatResult::Formatted { output } => panic!("formatted {src} to {output}"),
            FormatResult::CommentsLost { .. } | FormatResult::OutputInvalid(_) => {
                panic!("a formatter bug on {src}")
            }
        }
    }
}
