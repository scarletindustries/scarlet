use super::*;

fn parts(src: &str) -> Parts {
    match parse(src) {
        Entry::Accepted(program) => split(src, &program),
        _ => panic!("did not parse: {src}"),
    }
}

#[test]
fn an_import_is_held_apart_from_what_follows_it() {
    let p = parts("import scarlet/string\nconst x = 1\nx + 1\n");
    assert_eq!(p.imports, "import scarlet/string\n");
    assert_eq!(p.definitions, "const x = 1\n");
    assert_eq!(p.expressions, "x + 1\n");
}

#[test]
fn a_multi_line_declaration_keeps_all_of_its_lines() {
    let p = parts("fn tri(n Int) Int {\n\tn * 3\n}\ntri(2)\n");
    assert_eq!(p.definitions, "fn tri(n Int) Int {\n\tn * 3\n}\n");
    assert_eq!(p.expressions, "tri(2)\n");
}

#[test]
fn a_doc_comment_travels_with_its_declaration() {
    let p = parts("/// doc\nfn f() Int { 1 }\n");
    assert_eq!(p.definitions, "/// doc\nfn f() Int { 1 }\n");
    assert!(p.expressions.is_empty());
}

#[test]
fn a_bare_expression_is_never_kept() {
    let p = parts("println('once')");
    assert!(p.definitions.is_empty() && p.imports.is_empty());
    assert_eq!(p.expressions, "println('once')\n");
}
