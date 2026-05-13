use crate::ast;

pub fn print_expr(expr: &ast::Expression) -> String {
    print_expression(expr, 0)
}

fn indent(level: usize) -> String {
    "\t".repeat(level)
}

fn print_node_at_level(node: &ast::Node, level: usize) -> String {
    match node {
        ast::Node::Statement(s) => print_statement(s, level),
        ast::Node::Expression(e) => print_expression(e, level),
    }
}

fn print_statement(stmt: &ast::Statement, level: usize) -> String {
    match stmt {
        ast::Statement::VariableBinding(s) => {
            format!(
                "{} = {}",
                s.identifier.name,
                print_expression(&s.init, level)
            )
        }
        ast::Statement::ConstBinding(s) => {
            format!(
                "const {} = {}",
                s.identifier.name,
                print_expression(&s.init, level)
            )
        }
        ast::Statement::TypePatternBinding(s) => {
            format!(
                "{} = {}",
                print_type(&s.typ),
                print_expression(&s.init, level)
            )
        }
        ast::Statement::TupleDestructuringBinding(s) => {
            let mut out = String::from("(");
            for (i, pattern) in s.patterns.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&print_expression(pattern, level));
            }
            out.push_str(") = ");
            out.push_str(&print_expression(&s.init, level));
            out
        }
        ast::Statement::FunctionDeclaration(s) => print_function(
            level,
            &s.identifier,
            &s.params,
            &s.body,
            &s.return_type,
            &s.error_type,
        ),
        ast::Statement::StructDeclaration(s) => {
            let mut out = format!("struct {}", s.identifier.name);
            if !s.type_params.is_empty() {
                out.push('(');
                for (i, tp) in s.type_params.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    out.push_str(&tp.name);
                }
                out.push(')');
            }
            out.push_str(" {\n");
            for field in &s.fields {
                out.push_str(&indent(level + 1));
                out.push_str(&field.identifier.name);
                out.push(' ');
                out.push_str(&print_type(&field.typ));
                if let Some(init) = &field.init {
                    out.push_str(" = ");
                    out.push_str(&print_expression(init, level + 1));
                }
                out.push_str(",\n");
            }
            out.push_str(&indent(level));
            out.push('}');
            out
        }
        ast::Statement::EnumDeclaration(s) => {
            let mut out = format!("enum {}", s.identifier.name);
            if !s.type_params.is_empty() {
                out.push('(');
                for (i, tp) in s.type_params.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    out.push_str(&tp.name);
                }
                out.push(')');
            }
            out.push_str(" {\n");
            for variant in &s.variants {
                out.push_str(&indent(level + 1));
                out.push_str(&variant.identifier.name);
                if !variant.payload.is_empty() {
                    out.push('(');
                    for (i, payload) in variant.payload.iter().enumerate() {
                        if i > 0 {
                            out.push_str(", ");
                        }
                        out.push_str(&print_type(payload));
                    }
                    out.push(')');
                }
                out.push_str(",\n");
            }
            out.push_str(&indent(level));
            out.push('}');
            out
        }
        ast::Statement::ImportDeclaration(s) => {
            let mut out = format!("from '{}' import ", s.path);
            for (i, spec) in s.specifiers.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&spec.identifier.name);
            }
            out
        }
        ast::Statement::ExportDeclaration(s) => {
            format!("export {}", print_statement(&s.declaration, level))
        }
    }
}

fn print_function(
    level: usize,
    identifier: &ast::Identifier,
    params: &[ast::FunctionParameter],
    body: &ast::Expression,
    return_type: &Option<ast::TypeIdentifier>,
    error_type: &Option<ast::TypeIdentifier>,
) -> String {
    let mut s = format!("fn {}(", identifier.name);
    for (i, param) in params.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        s.push_str(&param.identifier.name);
        if let Some(typ) = &param.typ {
            s.push(' ');
            s.push_str(&print_type(typ));
        }
    }
    s.push(')');
    if let Some(ret) = return_type {
        s.push(' ');
        s.push_str(&print_type(ret));
    }
    if let Some(err) = error_type {
        s.push('!');
        s.push_str(&print_type(err));
    }
    s.push(' ');
    s.push_str(&print_expression(body, level));
    s
}

fn print_type(t: &ast::TypeIdentifier) -> String {
    match &t.kind {
        ast::TypeKind::OptionType(ot) => {
            format!("?{}", print_type(&ot.inner))
        }
        ast::TypeKind::ArrayType(at) => {
            format!("[]{}", print_type(&at.element))
        }
        ast::TypeKind::FunctionType(ft) => {
            let mut s = String::from("fn(");
            for (i, p) in ft.params.iter().enumerate() {
                if i > 0 {
                    s.push_str(", ");
                }
                s.push_str(&print_type(p));
            }
            s.push(')');
            if let Some(ret) = &ft.return_type {
                s.push(' ');
                s.push_str(&print_type(ret));
            }
            if let Some(err) = &ft.error_type {
                s.push('!');
                s.push_str(&print_type(err));
            }
            s
        }
        ast::TypeKind::NamedType(nt) => {
            let mut s = nt.identifier.name.clone();
            if !nt.type_args.is_empty() {
                s.push('(');
                for (i, ta) in nt.type_args.iter().enumerate() {
                    if i > 0 {
                        s.push_str(", ");
                    }
                    s.push_str(&print_type(ta));
                }
                s.push(')');
            }
            s
        }
    }
}

fn print_array_element(elem: &ast::ArrayElement, level: usize) -> String {
    match elem {
        ast::ArrayElement::SpreadElement(se) => {
            if let Some(inner) = &se.expression {
                format!("..{}", print_expression(inner, level))
            } else {
                String::from("..")
            }
        }
        ast::ArrayElement::Expression(e) => print_expression(e, level),
    }
}

fn print_expression(expr: &ast::Expression, level: usize) -> String {
    match expr {
        ast::Expression::StringLiteral(e) => {
            format!("'{}'", e.value)
        }
        ast::Expression::InterpolatedString(e) => {
            let mut s = String::from("'");
            for part in &e.parts {
                match part {
                    ast::Expression::StringLiteral(sl) => s.push_str(&sl.value),
                    ast::Expression::Identifier(id) => {
                        s.push('$');
                        s.push_str(&id.name);
                    }
                    other => {
                        s.push_str("${");
                        s.push_str(&print_expression(other, level));
                        s.push('}');
                    }
                }
            }
            s.push('\'');
            s
        }
        ast::Expression::NumberLiteral(e) => e.value.clone(),
        ast::Expression::BooleanLiteral(e) => {
            if e.value {
                String::from("true")
            } else {
                String::from("false")
            }
        }
        ast::Expression::NoneExpression(_) => String::from("none"),
        ast::Expression::Identifier(e) => e.name.clone(),
        ast::Expression::TypeIdentifier(e) => print_type(e),
        ast::Expression::BinaryExpression(e) => {
            format!(
                "{} {} {}",
                print_expression(&e.left, level),
                e.op.kind,
                print_expression(&e.right, level)
            )
        }
        ast::Expression::UnaryExpression(e) => {
            let op = match e.op.kind {
                crate::token::Kind::PuncExclamationMark => "!",
                _ => "?",
            };
            format!("{}{}", op, print_expression(&e.expression, level))
        }
        ast::Expression::BlockExpression(e) => {
            if e.body.is_empty() {
                String::from("{}")
            } else if e.body.len() == 1 {
                format!("{{ {} }}", print_node_at_level(&e.body[0], level))
            } else {
                let mut s = String::from("{\n");
                for n in &e.body {
                    s.push_str(&indent(level + 1));
                    s.push_str(&print_node_at_level(n, level + 1));
                    s.push('\n');
                }
                s.push_str(&indent(level));
                s.push('}');
                s
            }
        }
        ast::Expression::IfExpression(e) => {
            let mut s = format!(
                "if {} {}",
                print_expression(&e.condition, level),
                print_expression(&e.body, level)
            );
            if let Some(else_body) = &e.else_body {
                s.push_str(" else ");
                s.push_str(&print_expression(else_body, level));
            }
            s
        }
        ast::Expression::MatchExpression(e) => {
            let mut s = format!("match {} {{\n", print_expression(&e.subject, level));
            for arm in &e.arms {
                s.push_str(&indent(level + 1));
                s.push_str(&print_expression(&arm.pattern, level + 1));
                s.push_str(" -> ");
                s.push_str(&print_expression(&arm.body, level + 1));
                s.push_str(",\n");
            }
            s.push_str(&indent(level));
            s.push('}');
            s
        }
        ast::Expression::OrExpression(e) => {
            let mut s = format!("{} or ", print_expression(&e.expression, level));
            if let Some(receiver) = &e.receiver {
                s.push_str(&receiver.name);
                s.push_str(" -> ");
            }
            s.push_str(&print_expression(&e.body, level));
            s
        }
        ast::Expression::ErrorExpression(e) => {
            format!("error {}", print_expression(&e.expression, level))
        }
        ast::Expression::FunctionExpression(e) => {
            let mut s = String::from("fn(");
            for (i, param) in e.params.iter().enumerate() {
                if i > 0 {
                    s.push_str(", ");
                }
                s.push_str(&param.identifier.name);
                if let Some(typ) = &param.typ {
                    s.push(' ');
                    s.push_str(&print_type(typ));
                }
            }
            s.push(')');
            if let Some(ret) = &e.return_type {
                s.push(' ');
                s.push_str(&print_type(ret));
            }
            if let Some(err) = &e.error_type {
                s.push('!');
                s.push_str(&print_type(err));
            }
            s.push(' ');
            s.push_str(&print_expression(&e.body, level));
            s
        }
        ast::Expression::FunctionCallExpression(e) => {
            let mut s = format!("{}(", e.identifier.name);
            for (i, arg) in e.arguments.iter().enumerate() {
                if i > 0 {
                    s.push_str(", ");
                }
                s.push_str(&print_expression(arg, level));
            }
            s.push(')');
            s
        }
        ast::Expression::PropertyAccessExpression(e) => {
            format!(
                "{}.{}",
                print_expression(&e.left, level),
                print_expression(&e.right, level)
            )
        }
        ast::Expression::ArrayExpression(e) => {
            let mut s = String::from("[");
            for (i, elem) in e.elements.iter().enumerate() {
                if i > 0 {
                    s.push_str(", ");
                }
                s.push_str(&print_array_element(elem, level));
            }
            s.push(']');
            s
        }
        ast::Expression::TupleExpression(e) => {
            let mut s = String::from("(");
            for (i, elem) in e.elements.iter().enumerate() {
                if i > 0 {
                    s.push_str(", ");
                }
                s.push_str(&print_expression(elem, level));
            }
            if e.elements.len() == 1 {
                s.push(',');
            }
            s.push(')');
            s
        }
        ast::Expression::ArrayIndexExpression(e) => {
            format!(
                "{}[{}]",
                print_expression(&e.expression, level),
                print_expression(&e.index, level)
            )
        }
        ast::Expression::RangeExpression(e) => {
            format!(
                "{}..{}",
                print_expression(&e.start, level),
                print_expression(&e.end, level)
            )
        }
        ast::Expression::StructInitExpression(e) => {
            let mut s = e.identifier.name.clone();
            if !e.type_args.is_empty() {
                s.push('(');
                for (i, ta) in e.type_args.iter().enumerate() {
                    if i > 0 {
                        s.push_str(", ");
                    }
                    s.push_str(&print_type(ta));
                }
                s.push(')');
            }
            s.push_str("{\n");
            for field in &e.fields {
                s.push_str(&indent(level + 1));
                s.push_str(&field.identifier.name);
                s.push_str(": ");
                s.push_str(&print_expression(&field.init, level + 1));
                s.push_str(",\n");
            }
            s.push_str(&indent(level));
            s.push('}');
            s
        }
        ast::Expression::WildcardPattern(_) => String::from("else"),
        ast::Expression::OrPattern(e) => {
            let mut s = String::new();
            for (i, pattern) in e.patterns.iter().enumerate() {
                if i > 0 {
                    s.push_str(" | ");
                }
                s.push_str(&print_expression(pattern, level));
            }
            s
        }
        ast::Expression::ErrorNode(e) => {
            format!("/* could not parse: {} */", e.message)
        }
    }
}
