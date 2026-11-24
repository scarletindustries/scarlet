use super::*;
use crate::{parser, scanner};

fn parse_and_desugar(src: &str) -> BlockExpression {
    let mut sc = scanner::new_scanner(src.to_string());
    let r = parser::new_parser(&mut sc).parse_program();
    assert!(
        r.diagnostics.is_empty(),
        "parse errors: {:?}",
        r.diagnostics
    );
    let mut block = r.ast;
    desugar_program(&mut block);
    block
}

fn contains_backpass_body(body: &[Node]) -> bool {
    body.iter().any(is_backpass)
}

/// The trailing lambda appended to `call`, if the node is a call.
fn trailing_lambda(node: &Node) -> &FunctionExpression {
    let Node::Expression(Expression::FunctionCallExpression(fc)) = node else {
        panic!("expected a call node, got {node:?}");
    };
    let Some(CallArg::Positional(Expression::FunctionExpression(fe))) = fc.arguments.last() else {
        panic!("expected a trailing lambda, got {:?}", fc.arguments.last());
    };
    fe
}

#[test]
fn single_backpass_becomes_trailing_lambda() {
    let block = parse_and_desugar("fn f() {\n\tx <- g(1)\n\tx\n}\n");
    let Node::Statement(s) = &block.body[0] else {
        panic!("expected fn decl");
    };
    let Statement::Declaration { decl, .. } = s.as_ref() else {
        panic!("expected declaration");
    };
    let Declaration::Function(fd) = decl.as_ref() else {
        panic!("expected function");
    };
    let FnBody::Block(Expression::BlockExpression(body)) = &fd.body else {
        panic!("expected block body");
    };
    assert_eq!(
        body.body.len(),
        1,
        "backpass collapsed the block to the call"
    );
    let fe = trailing_lambda(&body.body[0]);
    assert_eq!(fe.params.len(), 1);
    assert_eq!(fe.params[0].identifier.name, "x");
    let Expression::BlockExpression(rest) = fe.body.as_ref() else {
        panic!("lambda body should be a block");
    };
    assert_eq!(rest.body.len(), 1);
    assert!(!contains_backpass_body(&rest.body));
}

#[test]
fn two_backpasses_nest_in_order() {
    let block = parse_and_desugar("fn f() {\n\ta <- g(1)\n\tb <- h(a)\n\ta + b\n}\n");
    let Node::Statement(s) = &block.body[0] else {
        panic!("expected fn decl");
    };
    let Statement::Declaration { decl, .. } = s.as_ref() else {
        panic!("expected declaration");
    };
    let Declaration::Function(fd) = decl.as_ref() else {
        panic!("expected function");
    };
    let FnBody::Block(Expression::BlockExpression(body)) = &fd.body else {
        panic!("expected block body");
    };
    // Outer: g(1, fn(a) { h(a, fn(b) { a + b }) }).
    let outer = trailing_lambda(&body.body[0]);
    assert_eq!(outer.params[0].identifier.name, "a");
    let Expression::BlockExpression(inner_body) = outer.body.as_ref() else {
        panic!("lambda body should be a block");
    };
    assert_eq!(inner_body.body.len(), 1);
    let inner = trailing_lambda(&inner_body.body[0]);
    assert_eq!(inner.params[0].identifier.name, "b");
}

/// `fn f() { <body> }`'s body block, for a program whose first node is
/// exactly that one function declaration.
fn fn_body(block: &BlockExpression) -> &BlockExpression {
    let Node::Statement(s) = &block.body[0] else {
        panic!("expected fn decl");
    };
    let Statement::Declaration { decl, .. } = s.as_ref() else {
        panic!("expected declaration");
    };
    let Declaration::Function(fd) = decl.as_ref() else {
        panic!("expected function");
    };
    let FnBody::Block(Expression::BlockExpression(body)) = &fd.body else {
        panic!("expected block body");
    };
    body
}

fn ident_name(e: &Expression) -> &str {
    let Expression::Identifier(id) = e else {
        panic!("expected identifier, got {e:?}");
    };
    &id.name
}

#[test]
fn pipe_into_call_prepends_argument() {
    let block = parse_and_desugar("fn f() {\n\ta |> g(x)\n}\n");
    let body = fn_body(&block);
    let Node::Expression(Expression::FunctionCallExpression(fc)) = &body.body[0] else {
        panic!("expected a call, got {:?}", body.body[0]);
    };
    assert_eq!(ident_name(&fc.callee), "g");
    assert_eq!(fc.arguments.len(), 2, "piped value did not prepend");
    let CallArg::Positional(first) = &fc.arguments[0] else {
        panic!("expected the piped value as a positional argument");
    };
    assert_eq!(ident_name(first), "a");
    let CallArg::Positional(second) = &fc.arguments[1] else {
        panic!("expected g's own argument second");
    };
    assert_eq!(ident_name(second), "x");
}

#[test]
fn pipe_into_identifier_wraps_a_call() {
    // No parens on the right: `right` is a plain function value, called
    // with `left` as its only argument.
    let block = parse_and_desugar("fn f() {\n\ta |> g\n}\n");
    let body = fn_body(&block);
    let Node::Expression(Expression::FunctionCallExpression(fc)) = &body.body[0] else {
        panic!("expected a call, got {:?}", body.body[0]);
    };
    assert_eq!(ident_name(&fc.callee), "g");
    assert_eq!(fc.arguments.len(), 1);
    let CallArg::Positional(arg) = &fc.arguments[0] else {
        panic!("expected a positional argument");
    };
    assert_eq!(ident_name(arg), "a");
}

#[test]
fn pipe_chain_desugars_inside_out() {
    // a |> f |> g becomes g(f(a)): each stage wraps the one before it.
    let block = parse_and_desugar("fn f() {\n\ta |> f |> g\n}\n");
    let body = fn_body(&block);
    let Node::Expression(Expression::FunctionCallExpression(outer)) = &body.body[0] else {
        panic!("expected a call, got {:?}", body.body[0]);
    };
    assert_eq!(ident_name(&outer.callee), "g");
    assert_eq!(outer.arguments.len(), 1);
    let CallArg::Positional(Expression::FunctionCallExpression(inner)) = &outer.arguments[0] else {
        panic!(
            "expected g's argument to be the inner call, got {:?}",
            outer.arguments[0]
        );
    };
    assert_eq!(ident_name(&inner.callee), "f");
    assert_eq!(inner.arguments.len(), 1);
    let CallArg::Positional(innermost) = &inner.arguments[0] else {
        panic!("expected a positional argument");
    };
    assert_eq!(ident_name(innermost), "a");
}
