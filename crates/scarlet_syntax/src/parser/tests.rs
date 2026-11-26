use super::*;
use crate::diagnostic;
use crate::scanner::new_scanner;

fn parse(src: &str) -> ParseResult {
    let mut s = new_scanner(src.to_string());
    new_parser(&mut s).parse_program()
}

fn assert_no_errors(src: &str) -> ParseResult {
    let result = parse(src);
    let errors: Vec<_> = result
        .diagnostics
        .iter()
        .filter(|d| d.severity() == diagnostic::Severity::Error)
        .collect();
    assert!(
        errors.is_empty(),
        "expected no parse errors for:\n{}\ngot: {:#?}",
        src,
        errors
    );
    result
}

/// The `doc` of the program's first declaration.
fn first_decl_doc(r: &ParseResult) -> Option<String> {
    match r.ast.body.first().expect("a first declaration") {
        ast::Node::Statement(s) => match s.as_ref() {
            ast::Statement::Declaration { decl, .. } => match decl.as_ref() {
                ast::Declaration::Function(f) => f.doc.clone(),
                ast::Declaration::Const(c) => c.doc.clone(),
                ast::Declaration::Type(t) => t.doc.clone(),
            },
            other => panic!("expected a declaration, got {other:?}"),
        },
        other => panic!("expected a statement, got {other:?}"),
    }
}

#[test]
fn doc_comment_at_line_zero_is_the_module_doc() {
    let r = assert_no_errors("/** Module prose. */\npub fn f() Int { 1 }\n");
    assert_eq!(r.doc.as_deref(), Some("/** Module prose. */"));
    assert_eq!(
        first_decl_doc(&r),
        None,
        "the line-0 doc belongs to the module, not to `f`"
    );
}

#[test]
fn doc_comment_below_line_zero_attaches_to_its_declaration() {
    let r = assert_no_errors("\n/** Docs f. */\npub fn f() Int { 1 }\n");
    assert_eq!(r.doc, None, "a doc on line 1 is not the module doc");
    assert_eq!(first_decl_doc(&r).as_deref(), Some("/** Docs f. */"));
}

#[test]
fn line_comment_before_a_doc_comment_leaves_no_module_doc() {
    // The `//` forces a newline, so the `/** */` no longer begins on line
    // 0 and stays the declaration's doc.
    let r = assert_no_errors("// header\n/** Docs f. */\npub fn f() Int { 1 }\n");
    assert_eq!(r.doc, None);
    assert_eq!(first_decl_doc(&r).as_deref(), Some("/** Docs f. */"));
}

#[test]
fn file_without_a_module_doc_has_none() {
    let r = assert_no_errors("pub fn f() Int { 1 }\n");
    assert_eq!(r.doc, None);
    assert_eq!(first_decl_doc(&r), None);
}

#[test]
fn module_doc_and_first_declaration_doc_coexist() {
    let r = assert_no_errors("/** Module. */\n/** Docs f. */\npub fn f() Int { 1 }\n");
    assert_eq!(r.doc.as_deref(), Some("/** Module. */"));
    assert_eq!(first_decl_doc(&r).as_deref(), Some("/** Docs f. */"));
}

#[test]
fn unterminated_line_zero_doc_is_not_the_module_doc() {
    // The scanner swallows the rest of the file into the comment; treating
    // that as a module doc would put the whole source on the hover card.
    let r = parse("/** Module prose\npub fn f() Int { 1 }\n");
    assert_eq!(r.doc, None);
}

#[test]
fn module_doc_survives_an_import_only_prefix() {
    let r = assert_no_errors("/** Module. */\nimport scarlet/string\npub fn f() Int { 1 }\n");
    assert_eq!(r.doc.as_deref(), Some("/** Module. */"));
}

fn assert_has_error(src: &str, snippet: &str) {
    let result = parse(src);
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.severity() == diagnostic::Severity::Error && d.message().contains(snippet)),
        "expected error containing '{}' for:\n{}\ngot: {:#?}",
        snippet,
        src,
        result.diagnostics
    );
}

fn assert_single_error(src: &str, snippet: &str) {
    let result = parse(src);
    let errors: Vec<_> = result
        .diagnostics
        .iter()
        .filter(|d| d.severity() == diagnostic::Severity::Error)
        .collect();
    assert_eq!(
        errors.len(),
        1,
        "expected exactly one error for:\n{src}\ngot: {errors:#?}"
    );
    assert!(
        errors[0].message().contains(snippet),
        "expected sole error to contain '{snippet}', got: {:?}",
        errors[0].message()
    );
}

/// Every program in the repo's two .sca corpora must parse cleanly. The
/// test globs the folders rather than listing files, so curating the corpus
/// can't break it.
#[test]
fn parses_every_corpus_program() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut seen = 0;
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
            // a.sca and b.sca are the LSP import demo's scratch pair, and may be mid-edit.
            if name == "a.sca" || name == "b.sca" {
                continue;
            }
            let src = std::fs::read_to_string(&path).unwrap();
            let r = parse(&src);
            let errors: Vec<_> = r
                .diagnostics
                .iter()
                .filter(|d| d.severity() == diagnostic::Severity::Error)
                .collect();
            assert!(errors.is_empty(), "{name} failed to parse: {errors:#?}");
            assert!(!r.ast.body.is_empty(), "{name} parsed to an empty program");
            seen += 1;
        }
    }
    assert!(
        seen > 20,
        "corpus glob found only {seen} programs — wrong dir?"
    );
}

#[test]
fn test_literals() {
    assert_no_errors("123");
    assert_no_errors("'hello'");
    assert_no_errors("true");
    assert_no_errors("false");
}

#[test]
fn test_binary_precedence() {
    let result = parse("1 + 2 * 3");
    assert!(result.diagnostics.is_empty());
    // 1 + (2 * 3)
    if let ast::Node::Expression(ast::Expression::BinaryExpression(b)) = &result.ast.body[0] {
        assert_eq!(b.op, ast::BinaryOp::Add);
        assert!(matches!(*b.right, ast::Expression::BinaryExpression(_)));
    } else {
        panic!("expected binary expression");
    }
}

#[test]
fn test_range_precedence() {
    // -5..5 is RangeExpression(Unary(-5), 5)
    let result = parse("-5..5");
    assert!(result.diagnostics.is_empty());
    if let ast::Node::Expression(ast::Expression::RangeExpression(r)) = &result.ast.body[0] {
        assert!(matches!(*r.start, ast::Expression::UnaryExpression(_)));
        assert!(matches!(*r.end, ast::Expression::NumberLiteral(_)));
    } else {
        panic!("expected range expression, got {:#?}", result.ast.body[0]);
    }
    // 1+2..3+4 is (1+2)..(3+4)
    let result = parse("1+2..3+4");
    if let ast::Node::Expression(ast::Expression::RangeExpression(r)) = &result.ast.body[0] {
        assert!(matches!(*r.start, ast::Expression::BinaryExpression(_)));
        assert!(matches!(*r.end, ast::Expression::BinaryExpression(_)));
    } else {
        panic!("expected range expression");
    }
}

#[test]
fn test_pipe_precedence() {
    // `a |> f(x)`: the call already carries an argument.
    let result = parse("a |> f(x)");
    assert!(result.diagnostics.is_empty());
    if let ast::Node::Expression(ast::Expression::PipeExpression(p)) = &result.ast.body[0] {
        assert!(matches!(*p.left, ast::Expression::Identifier(_)));
        assert!(matches!(
            *p.right,
            ast::Expression::FunctionCallExpression(_)
        ));
    } else {
        panic!("expected pipe expression, got {:#?}", result.ast.body[0]);
    }

    // Left-associative chain: (a |> f) |> g.
    let result = parse("a |> f |> g");
    assert!(result.diagnostics.is_empty());
    if let ast::Node::Expression(ast::Expression::PipeExpression(outer)) = &result.ast.body[0] {
        assert!(matches!(*outer.left, ast::Expression::PipeExpression(_)));
        assert!(matches!(*outer.right, ast::Expression::Identifier(_)));
    } else {
        panic!("expected pipe expression, got {:#?}", result.ast.body[0]);
    }

    // Tighter than `or`: the whole pipeline is `or`'s left operand, not
    // just its last stage.
    let result = parse("a |> f() or e -> g(e)");
    assert!(result.diagnostics.is_empty());
    if let ast::Node::Expression(ast::Expression::OrExpression(o)) = &result.ast.body[0] {
        assert!(matches!(*o.expression, ast::Expression::PipeExpression(_)));
    } else {
        panic!("expected or expression, got {:#?}", result.ast.body[0]);
    }
}

#[test]
fn test_variable_binding() {
    assert_no_errors("x = 5");
    assert_no_errors("x Int = 5");
    assert_no_errors("const PI = 3.14");
}

fn binding(src: &str) -> ast::Binding {
    let r = parse(src);
    assert!(r.diagnostics.is_empty(), "{:#?}", r.diagnostics);
    let ast::Node::Statement(s) = &r.ast.body[0] else {
        panic!("expected statement, got {:#?}", r.ast.body[0])
    };
    let ast::Statement::Binding(b) = s.as_ref() else {
        panic!("expected a binding, got {:#?}", s)
    };
    (**b).clone()
}

#[test]
fn every_binding_is_a_pattern() {
    let b = binding("x Int = 5");
    assert_eq!(b.name().map(|n| n.name.as_str()), Some("x"));
    assert!(b.typ.is_some());

    let b = binding("_ Nil = println('x')");
    assert_eq!(b.name().map(|n| n.name.as_str()), Some("_"));
    assert!(b.typ.is_some());

    assert!(matches!(
        binding("(a, b) = t").pattern,
        ast::Pattern::Tuple { .. }
    ));
    assert!(matches!(
        binding("Nil = println('x')").pattern,
        ast::Pattern::Constructor { .. }
    ));

    let b = binding("Some(x) = Some(1)");
    let ast::Pattern::Constructor { name, args, .. } = &b.pattern else {
        panic!("expected a constructor pattern, got {:#?}", b.pattern)
    };
    assert_eq!((name.name.as_str(), args.len()), ("Some", 1));

    assert_no_errors("Point(x, y) = origin");
    assert_no_errors("Wrapper(a, b, c) = make()");

    // Through its module, as a constructor has to be written when it is
    // not imported by name.
    let b = binding("headers::Header(name, value) = h");
    let ast::Pattern::Constructor {
        qualifier,
        name,
        args,
        ..
    } = &b.pattern
    else {
        panic!("expected a constructor pattern, got {:#?}", b.pattern)
    };
    assert_eq!(qualifier.as_ref().map(|q| q.name.as_str()), Some("headers"));
    assert_eq!((name.name.as_str(), args.len()), ("Header", 2));
    // With no `=` after it, the same shape is a call.
    let r = parse("http::Fixed(1, b)");
    assert!(r.diagnostics.is_empty(), "{:#?}", r.diagnostics);
    assert!(matches!(r.ast.body[0], ast::Node::Expression(_)));
}

#[test]
fn module_access_is_its_own_node() {
    let r = parse("c::f(1)");
    assert!(r.diagnostics.is_empty(), "{:#?}", r.diagnostics);
    let ast::Node::Expression(ast::Expression::FunctionCallExpression(call)) = &r.ast.body[0]
    else {
        panic!("expected a call, got {:#?}", r.ast.body[0])
    };
    let ast::Expression::ModuleAccessExpression(ma) = call.callee.as_ref() else {
        panic!("expected a module access, got {:#?}", call.callee)
    };
    assert_eq!(
        (ma.module.name.as_str(), ma.member.name.as_str()),
        ("c", "f")
    );

    // `.` after a module member is an ordinary field read on its value.
    let r = parse("c::origin.x");
    assert!(r.diagnostics.is_empty(), "{:#?}", r.diagnostics);
    assert!(matches!(
        &r.ast.body[0],
        ast::Node::Expression(ast::Expression::PropertyAccessExpression(pa))
            if matches!(pa.left.as_ref(), ast::Expression::ModuleAccessExpression(_))
    ));
}

#[test]
fn only_a_name_comes_before_colons() {
    assert_has_error(
        "a.b::c",
        "Only an imported module's name can come before `::`",
    );
    assert_has_error(
        "a::b::c",
        "Only an imported module's name can come before `::`",
    );
    assert_has_error(
        "f()::c",
        "Only an imported module's name can come before `::`",
    );
    assert_has_error("x = c::", "Expected a module member after `::`");
}

#[test]
fn test_uppercase_ident_still_expression() {
    // No `=` ahead → constructor expression, not a statement.
    let r = parse("Some(1)");
    assert!(r.diagnostics.is_empty(), "{:#?}", r.diagnostics);
    assert!(matches!(r.ast.body[0], ast::Node::Expression(_)));

    // `==` is comparison, not binding.
    let r = parse("Nil == x");
    assert!(r.diagnostics.is_empty(), "{:#?}", r.diagnostics);
    assert!(matches!(r.ast.body[0], ast::Node::Expression(_)));
}

/// The first node of a source whose sole top-level node is a block.
fn first_block_node(r: &ParseResult) -> &ast::Node {
    let ast::Node::Expression(ast::Expression::BlockExpression(b)) = &r.ast.body[0] else {
        panic!("expected a top-level block, got {:#?}", r.ast.body[0]);
    };
    b.body.first().expect("a non-empty block")
}

#[track_caller]
fn assert_backpass(node: &ast::Node, binders: &[&str]) {
    let ast::Node::Statement(s) = node else {
        panic!("expected statement, got {node:#?}");
    };
    let ast::Statement::Backpass(bp) = s.as_ref() else {
        panic!("expected Backpass, got {s:#?}");
    };
    let names: Vec<&str> = bp
        .binders
        .iter()
        .map(|b| match b {
            ast::Pattern::Var { name } => name.name.as_str(),
            other => panic!("expected a plain binder, got {other:#?}"),
        })
        .collect();
    assert_eq!(names, binders);
    assert!(
        matches!(bp.call, ast::Expression::FunctionCallExpression(_)),
        "RHS must be a call, got {:#?}",
        bp.call
    );
}

#[test]
fn test_backpass_single_binder() {
    let r = assert_no_errors("{\nx <- g(1)\nx\n}");
    assert_backpass(first_block_node(&r), &["x"]);
}

#[test]
fn test_backpass_multiple_binders() {
    let r = assert_no_errors("{\na, b <- g(1, 2)\na + b\n}");
    assert_backpass(first_block_node(&r), &["a", "b"]);
}

#[test]
fn test_backpass_discard_binder() {
    let r = assert_no_errors("{\n_ <- g(1)\n0\n}");
    assert_backpass(first_block_node(&r), &["_"]);
}

#[test]
fn test_backpass_without_spaces_is_still_backpass() {
    // `a<-g(1)` lexes as ident `<-` call, not `a < -g(1)`.
    let r = assert_no_errors("{\na<-g(1)\na\n}");
    assert_backpass(first_block_node(&r), &["a"]);
}

#[test]
fn a_backpass_pattern_can_wrap_inside_brackets() {
    let r = assert_no_errors("{\n(\n\ta,\n\tb,\n) <- h(2)\na + b\n}");
    let ast::Node::Statement(s) = first_block_node(&r) else {
        panic!("expected a statement");
    };
    assert!(matches!(s.as_ref(), ast::Statement::Backpass(_)), "{s:#?}");
}

/// The left of `<-` is a pattern, like the left of `=`.
#[test]
fn a_backpass_binds_patterns() {
    let r = assert_no_errors("{\nNil <- g(1)\n(a, b) <- h(2)\na + b\n}");
    let binders = |node: &ast::Node| match node {
        ast::Node::Statement(s) => match s.as_ref() {
            ast::Statement::Backpass(bp) => bp.binders.clone(),
            other => panic!("expected Backpass, got {other:#?}"),
        },
        other => panic!("expected statement, got {other:#?}"),
    };
    let ast::Node::Expression(ast::Expression::BlockExpression(block)) = &r.ast.body[0] else {
        panic!("expected a top-level block, got {:#?}", r.ast.body[0])
    };
    assert!(matches!(
        binders(&block.body[0])[..],
        [ast::Pattern::Constructor { .. }]
    ));
    assert!(matches!(
        binders(&block.body[1])[..],
        [ast::Pattern::Tuple { .. }]
    ));
}

#[test]
fn a_label_alone_binds_the_field_to_its_name() {
    let b = binding("User(name:, age: a, ..) = u");
    let ast::Pattern::Constructor { args, rest, .. } = &b.pattern else {
        panic!("expected a constructor pattern, got {:#?}", b.pattern)
    };
    assert!(*rest);
    let [
        ast::PatternArg::Labeled {
            pattern: ast::Pattern::Var { name: n },
            punned: true,
            ..
        },
        ast::PatternArg::Labeled {
            pattern: ast::Pattern::Var { name: a },
            punned: false,
            ..
        },
    ] = args.as_slice()
    else {
        panic!("unexpected args {args:#?}")
    };
    assert_eq!((n.name.as_str(), a.name.as_str()), ("name", "a"));
}

#[test]
fn test_spaced_less_than_minus_is_still_comparison() {
    let r = assert_no_errors("{\nx = a < -b\nx\n}");
    assert!(matches!(
        first_block_node(&r),
        ast::Node::Statement(s) if matches!(s.as_ref(), ast::Statement::Binding(_))
    ));
}

#[test]
fn test_backpass_rhs_must_be_a_call() {
    assert_has_error(
        "{\nx <- 5\nx\n}",
        "A backpass needs a function call on the right of `<-`",
    );
    assert_has_error(
        "{\nx <- y\nx\n}",
        "A backpass needs a function call on the right of `<-`",
    );
}

#[test]
fn test_backpass_rejects_or_suffix() {
    assert_has_error(
        "{\nx <- g(1) or 0\nx\n}",
        "A backpass statement can't have `or`",
    );
}

#[test]
fn test_backpass_may_end_its_block() {
    let r = assert_no_errors("{\n_ <- g(1)\n}");
    assert_backpass(first_block_node(&r), &["_"]);
}

#[test]
fn test_backpass_rejected_at_module_top_level() {
    assert_has_error(
        "x <- g(1)\nx\n",
        "Backpassing is not allowed at the top level of a module",
    );
}

#[test]
fn test_function_decl() {
    assert_no_errors("fn add(a Int, b Int) Int { a + b }");
    assert_no_errors("fn noop() { Nil }");
    assert_no_errors("fn id(x a) a { x }");
    assert_no_errors("fn pair() (Int, Int) { (1, 2) }");
}

#[test]
fn test_type_decl() {
    assert_no_errors("type Point { Point(x Int, y Int) }");
    assert_no_errors("type Box(t) { Box(value t) }");
    assert_no_errors("type Color {\n\tRed\n\n\tGreen\n\n\tBlue\n}");
    assert_no_errors("type Option(t) {\n\tSome(value t)\n\n\tNone\n}");
    assert_no_errors("type IntList = Array(Int)");
}

#[test]
fn test_type_decl_errors() {
    assert_has_error("type Foo {}", "at least one constructor");
    assert_has_error("type foo { Foo }", "uppercase");
    assert_has_error("type Foo { Foo(Int) }", "needs a label");
}

#[test]
fn test_type_decl_shorthand() {
    assert_no_errors("type User {\n\tname String\n\tage Int\n}");
    assert_no_errors("type Box(A) { value A }");
    assert_no_errors("type User {\n  name String\n  age Int\n}");
    assert_has_error("type Foo { bar }", "Expected a type name");
}

#[test]
fn test_if_expression() {
    assert_no_errors("if x > 0 { 1 } else { 2 }");
    assert_has_error("if x > 0 { 1 }", "needs an `else` branch");
}

#[test]
fn test_match_expression() {
    assert_no_errors("match x { 1 -> 'one'\n 2 -> 'two'\n _ -> 'other' }");
    assert_no_errors("match x { 1 | 2 -> 'low'\n _ -> 'high' }");
    assert_no_errors("match x { Some(v) -> v\n None -> 0 }");
    assert_no_errors("match p { Point(x: a, y: b) -> a + b }");
    assert_no_errors("match p { Point(x: a, ..) -> a }");
    assert_no_errors("match xs { [a, b, ..rest] -> a\n [] -> 0 }");
    assert_no_errors("match t { (a, b) -> a + b }");
    assert_no_errors("match n { 1..5 -> 'low'\n _ -> 'hi' }");
}

#[test]
fn test_array_and_tuple() {
    assert_no_errors("[1, 2, 3]");
    assert_no_errors("(1, 2, 3)");
    assert_no_errors("[..xs, 1, ..ys]");
    assert_has_error("[..xs, 1, ..]", "Expected a value after `..`");
    assert_has_error("()", "A tuple needs at least two elements");
    assert_has_error("(1)", "Parentheses can't hold just one expression");
}

#[test]
fn test_tuple_destructuring() {
    assert_no_errors("(a, b) = (1, 2)");
}

#[test]
fn a_line_starting_with_equals_continues_a_binding() {
    // No statement starts with `=`, so the line break before it is only
    // spacing.
    for src in ["{\nx\n= 1\nx\n}", "{\n(a, b)\n= (1, 2)\na\n}"] {
        let r = assert_no_errors(src);
        assert!(
            matches!(first_block_node(&r), ast::Node::Statement(_)),
            "{src}"
        );
    }
}

#[test]
fn statements_on_one_line_are_an_error() {
    assert_single_error("{\nx = 1 y = 2\ny\n}", "Put each statement on its own line");
    assert_single_error(
        "match x {\nA -> 1 B -> 2\n}",
        "Put each match arm on its own line",
    );
}

#[test]
fn a_line_break_inside_brackets_is_only_spacing() {
    // Each of these was a parse error, or two statements, when the parser
    // looked at line breaks itself.
    assert_no_errors("{\nf(\n1\n-2\n)\n}");
    assert_no_errors("{\nxs[\n0\n]\n}");
    assert_no_errors("{\n[\nf\n(1)\n]\n}");
}

#[test]
fn test_postfix() {
    assert_no_errors("a.b.c");
    assert_no_errors("x = arr[0]");
    assert_no_errors("f(1, 2)");
    assert_no_errors("a.f(1).g");
    assert_no_errors("get_fn()(1, 2)");
}

#[test]
fn test_qualified_types() {
    assert_no_errors("type C = http::Method");
    assert_no_errors("type C = mod::Wrapper(Int)");
    assert_no_errors("fn f(m http::Method) http::Method { m }");
    assert_no_errors("fn g(xs fn(a) mod::Out) { xs }");
    assert_has_error(
        "type C = http.Method",
        "Reach a module's type or constructor with `::`, like `http::Method`",
    );
}

#[test]
fn test_call_args() {
    assert_no_errors("f(1, 2, 3)");
    assert_no_errors("f(a: 1, b: 2)");
    assert_no_errors("f(..xs)");
    assert_no_errors("Some(1)");
    assert_no_errors("Point(x: 1, y: 2)");
}

#[test]
fn test_import_pub() {
    assert_no_errors("import scarlet/json");
    assert_no_errors("import scarlet/json as j");
    assert_no_errors("import ./helper");
    assert_no_errors("import ../shared/auth");
    assert_no_errors("pub fn f() { 1 }");
    assert_no_errors("pub type S { S(a Int) }");
    assert_no_errors("pub const x = 1");

    let result = parse("fn f() { 1 }\nimport scarlet/json");
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.message().contains("at the top of the file"))
    );
}

/// There is no `import m.{x}`: the error says what to write instead, named
/// through the alias when there is one.
#[test]
fn a_named_import_says_to_qualify_the_use() {
    // One error, not a cascade: the list is skipped and the import kept.
    let error = |src: &str| {
        let result = parse(src);
        assert_eq!(result.diagnostics.len(), 1, "{:?}", result.diagnostics);
        result.diagnostics[0].message().clone()
    };
    let message = error("import scarlet/list.{fold, map as m}\nx = 1");
    assert!(message.contains("`import scarlet/list`"), "{message}");
    assert!(message.contains("`list::fold`"), "{message}");
    let message = error("import ./lib as l.{x}");
    assert!(message.contains("`import ./lib`"), "{message}");
    assert!(message.contains("`l::x`"), "{message}");
}

#[test]
fn test_error_recovery() {
    let result = parse("fn ??? bad");
    assert!(!result.diagnostics.is_empty());
}

#[test]
fn test_block_body_diagnostics() {
    // An `if` needs `then` or a block after its condition; a function
    // body must be a block.
    assert_has_error("x = if 1 < 2 5 else 6", "needs `then` or a block");
    assert_has_error("fn f() Int = 1", "A function body must be a block");
    // The well-formed shapes still parse cleanly.
    assert_no_errors("x = if 1 < 2 { 5 } else { 6 }");
    assert_no_errors("x = if 1 < 2 then 5 else 6");
    assert_no_errors("x = if 1 < 2 { 5 } else 6");
    assert_no_errors("x = if a then 1 else if b then 2 else 3");
    assert_no_errors("x = if a then { y = 1\n y } else f(2)");
    // `then` after a dot is still a name: `result.then` is a function.
    assert_no_errors("x = result.then(r, f)");
    assert_no_errors("fn f() Int { 1 }");
}

#[test]
fn test_tuple_arity_and_pattern_diagnostics() {
    // A single type or pattern in parentheses is not a tuple.
    assert_has_error(
        "fn f(x (Int)) Int { 1 }",
        "A tuple type needs at least two elements",
    );
    assert_has_error(
        "match (1, 2) { (x) -> x }",
        "A tuple pattern needs at least two elements",
    );
    // A binary operator cannot begin a pattern.
    assert_has_error("match 1 { + -> 0 }", "Expected a pattern, but found '+'");
    // Recovery that synchronises to the arm's `->` must consume it and
    // parse the body, not loop re-erroring on the same token. A lowercase
    // member can't be a constructor, so that case still needs recovery.
    assert_no_errors("match x { net::V6(ip) -> 1 }");
    assert_single_error(
        "match x { net::v6(ip) -> 1 }",
        "Constructor name 'v6' must start with an uppercase letter",
    );
    assert_single_error(
        "match x { net.V6(ip) -> 1 }",
        "Reach a module's type or constructor with `::`, like `net::V6`",
    );
    assert_single_error(
        "match x { a if + -> 1 }",
        "Expected an expression, but found '+'",
    );
    assert_single_error("match x { -> 1 }", "Expected a pattern, but found '->'");
    // Constructor fields need commas. A line break in the brackets is only
    // spacing, so it can't separate them.
    assert_no_errors("type P {\n\tP(a Int, b Int)\n}\n");
    assert_no_errors("type P {\n\tP(\n\t\ta Int,\n\t\tb Int,\n\t)\n}\n");
    assert_has_error(
        "type P {\n\tP(a Int b Int)\n}\n",
        "Separate fields with commas",
    );
    assert_has_error(
        "type P {\n\tP(\n\t\ta Int\n\t\tb Int\n\t)\n}\n",
        "Separate fields with commas",
    );
    assert_no_errors("fn f(x (Int, Int)) Int { 1 }");
    assert_no_errors("match (1, 2) { (a, b) -> a }");
}

#[test]
fn test_declaration_guard_diagnostics() {
    // `@vm` ops are bodyless intrinsics.
    assert_has_error(
        "@vm(add)\nfn f(a Int) Int { a }",
        "A @vm function can't have a body",
    );
    // `@vm` carries exactly one arg, enforced at parse time so
    // `FnBody::Vm` always holds it.
    assert_has_error(
        "@vm\nfn f(a Int) Int",
        "@vm takes exactly one argument, the VM op key",
    );
    assert_has_error(
        "@vm(add, sub)\nfn f(a Int) Int",
        "@vm takes exactly one argument, the VM op key",
    );
    // `opaque` requires a body and only modifies `type`.
    assert_has_error(
        "pub opaque type T",
        "`opaque` type must have constructors to hide",
    );
    assert_has_error("pub opaque type T = Int", "A type alias can't be `opaque`");
    assert_has_error(
        "pub opaque fn f() Nil { Nil }",
        "Only a `type` declaration can be `opaque`",
    );
    // A `const` declaration can't have attributes.
    assert_has_error(
        "@vm(x)\nconst PI = 3",
        "A `const` declaration can't have attributes",
    );
    assert_has_error("pub x = 1", "after `pub`");
    // The message must not mention a `pub` the user never wrote.
    assert_has_error(
        "@vm(add)\njunk",
        "Expected `fn`, `type` or `const` after attributes",
    );
    assert_has_error(
        "import .foo",
        "Expected `/` after `.` or `..` in an import path",
    );
}

#[test]
fn test_decrement_operator_rejected() {
    assert_has_error("y = 5\nz = y--", "Scarlet has no `--` operator");
}

#[test]
fn test_is_type_name() {
    assert!(is_type_name("Int"));
    assert!(is_type_name("String"));
    assert!(!is_type_name("foo"));
    assert!(!is_type_name(""));
    assert!(!is_type_name("_Foo"));
}

#[test]
fn test_deep_nesting_does_not_overflow() {
    // Without the recursion guard each of these aborts the process
    // (SIGABRT), taking the whole test binary with it.
    let n = 5000;

    assert_has_error(&format!("x = {}true", "!".repeat(n)), "too deep");
    assert_has_error(&format!("{}1{}", "(".repeat(n), ")".repeat(n)), "too deep");
    assert_has_error(&format!("{}1{}", "[".repeat(n), "]".repeat(n)), "too deep");
    assert_has_error(
        &format!("type T = {}Int{}", "(Int, ".repeat(n), ")".repeat(n)),
        "too deep",
    );
    assert_has_error(
        &format!(
            "match x {{ {}y{} -> 1, _ -> 2 }}",
            "S(".repeat(n),
            ")".repeat(n)
        ),
        "too deep",
    );
}

// Blocks take the most stack for each level, and a build with no optimisations needs
// more for them than a test's 2 MiB. A main thread has 8.
fn on_main_sized_stack(test: fn()) {
    std::thread::Builder::new()
        .stack_size(8 << 20)
        .spawn(test)
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn blocks_nested_too_deep_are_reported_once() {
    on_main_sized_stack(reported_once);
}

fn reported_once() {
    for n in [14, 15, 20, 64, 500] {
        for (open, item, close) in [
            ("x = {\nif n {\n", "y = 1\ny", "\n} else {\n2\n}\n}\nx"),
            ("if n {\n", "if a { b } else { c }\nz", "\n} else {\n2\n}"),
            ("f = fn() {\nx = {\n", "g(1)\nh { i: 2 }", "\n}\nx\n}\nf()"),
        ] {
            let n = n * 10;
            let src = format!(
                "fn f(n Bool) Int {{\n{}{item}{}\n}}\nfn after() Int {{\n1\n}}\n",
                open.repeat(n),
                close.repeat(n),
            );
            let result = parse(&src);
            let messages: Vec<_> = result.diagnostics.iter().map(|d| d.message()).collect();
            assert_eq!(messages.len(), 1, "{messages:#?}");
            assert!(messages[0].contains("too deep"), "{messages:#?}");
        }
    }
}

#[test]
fn blocks_nested_too_deep_and_not_closed_end() {
    on_main_sized_stack(not_closed);
}

fn not_closed() {
    for open in [
        "x = {\n",
        "if n {\n",
        "x = {\nif n {\n",
        "match n {\n_ -> {\n",
    ] {
        assert_has_error(
            &format!("fn f(n Bool) Int {{\n{}y", open.repeat(500)),
            "too deep",
        );
    }
}

#[test]
fn test_reasonable_nesting_ok() {
    // Realistic nesting must never trip the limit. The worst multiplier is
    // the array/paren path at ~3 guard hits per source level, so 30 source
    // levels is ~90, under MAX_PARSE_DEPTH.
    let n = 30;

    assert_no_errors(&format!("x = {}true", "!".repeat(n)));
    assert_no_errors(&format!("x = {}1{}", "[".repeat(n), "]".repeat(n)));
    assert_no_errors(&format!(
        "type T = {}Int{}",
        "(Int, ".repeat(n),
        ")".repeat(n)
    ));
}

#[test]
fn test_binary_literal_expr() {
    assert_no_errors("x = <<>>");
    assert_no_errors("x = <<1, 2, 3>>");
    assert_no_errors("x = <<x:4>>");
    assert_no_errors("x = <<1:4, 2:4>>");
    assert_no_errors("x = <<body:bytes(len)>>");
    assert_no_errors("x = <<n:size(w)>>");
    assert_no_errors("x = <<rest:binary>>");
    assert_no_errors("x = <<s:utf8>>");
    assert_no_errors("x = <<a + b, head:binary, 0>>");
}

#[test]
fn test_binary_literal_ast_shape() {
    let result = parse("x = <<1, n:4, b:bytes(len), r:binary, s:utf8>>");
    assert!(result.diagnostics.is_empty(), "{:#?}", result.diagnostics);
    let ast::Node::Statement(stmt) = &result.ast.body[0] else {
        panic!("expected statement")
    };
    let ast::Statement::Binding(vb) = stmt.as_ref() else {
        panic!("expected a binding")
    };
    let ast::Expression::BinaryLiteral(bl) = &vb.init else {
        panic!("expected BinaryLiteral, got {:?}", vb.init)
    };
    assert_eq!(bl.segments.len(), 5);
    assert!(matches!(
        bl.segments[0].spec,
        ast::BinSpec::Int { bits: None }
    ));
    assert!(matches!(
        bl.segments[1].spec,
        ast::BinSpec::Int { bits: Some(_) }
    ));
    assert!(matches!(
        bl.segments[2].spec,
        ast::BinSpec::Binary { bytes: Some(_) }
    ));
    assert!(matches!(
        bl.segments[3].spec,
        ast::BinSpec::Binary { bytes: None }
    ));
    assert!(matches!(bl.segments[4].spec, ast::BinSpec::Utf8));
}

#[test]
fn test_binary_literal_bad_spec() {
    assert_has_error("x = <<1:foo>>", "segment size after `:`");
    assert_has_error("x = <<1:>>", "segment size after `:`");
}

#[test]
fn test_binary_pattern() {
    assert_no_errors("match b { <<a, b>> -> a + b\n _ -> 0 }");
    assert_no_errors("match b { <<x:4, y:4>> -> x\n _ -> 0 }");
    assert_no_errors("match b { <<_:8, body:bytes(n), rest:binary>> -> body\n _ -> b }");
    assert_no_errors("match b { <<1, 2, ..rest>> -> rest\n _ -> b }");
    assert_no_errors("match b { <<1, 2, ..>> -> 0\n _ -> 0 }");
    assert_no_errors("match b { <<>> -> 0\n _ -> 1 }");
}

#[test]
fn test_binary_pattern_rest_must_be_last() {
    assert_has_error("match b { <<..r, 1>> -> r\n _ -> b }", "last segment");
}
