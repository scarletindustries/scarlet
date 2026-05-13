use std::collections::HashMap;

use crate::ast;
use crate::diagnostic::{Diagnostic, Severity};
use crate::parser;
use crate::scanner;
use crate::span::Span;
use crate::token::{self, Kind, Trivia, TriviaKind};

pub struct FormatResult {
    pub output: String,
    pub has_errors: bool,
    pub diagnostics: Vec<Diagnostic>,
}

pub fn format_with_debug(input: &str, debug: bool) -> FormatResult {
    let mut s = scanner::new_scanner(input.to_string());
    let scanned_tokens = s.scan_all();
    let scanner_diagnostics = s.get_diagnostics();

    if debug {
        for tok in &scanned_tokens {
            eprintln!(
                "Token: {:?} \"{}\" trivia: {}",
                tok.kind,
                tok.literal.as_deref().unwrap_or(""),
                tok.leading_trivia.len()
            );
            for t in &tok.leading_trivia {
                eprintln!("  Trivia: {:?} \"{}\"", t.kind, t.text.replace('\n', "\\n"));
            }
        }
    }

    let mut trivia_map: HashMap<String, Vec<Trivia>> = HashMap::new();
    let mut eof_trivia: Vec<Trivia> = Vec::new();

    for tok in &scanned_tokens {
        if !tok.leading_trivia.is_empty() {
            let key = format!("{}:{}", tok.line, tok.column);
            trivia_map.insert(key, tok.leading_trivia.clone());
        }
        if tok.kind == Kind::Eof {
            eof_trivia = tok.leading_trivia.clone();
        }
    }

    let mut f = Formatter {
        tokens: scanned_tokens.clone(),
        trivia_map,
        output: String::with_capacity(input.len()),
        indent: 0,
        at_line_start: true,
    };

    let mut p = parser::new_parser_from_tokens(scanned_tokens, scanner_diagnostics);
    let result = p.parse_program();

    let errors: Vec<Diagnostic> = result
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .cloned()
        .collect();

    if !errors.is_empty() {
        return FormatResult {
            output: input.to_string(),
            has_errors: true,
            diagnostics: errors,
        };
    }

    f.format_block(&result.ast, true);
    f.emit_trivia(&eof_trivia);

    let mut final_out = f.output;
    while final_out.ends_with('\n') {
        final_out.pop();
    }
    final_out.push('\n');

    FormatResult {
        output: final_out,
        has_errors: false,
        diagnostics: result.diagnostics,
    }
}

struct Formatter {
    #[allow(dead_code)]
    tokens: Vec<token::Token>,
    trivia_map: HashMap<String, Vec<Trivia>>,
    output: String,
    indent: i32,
    at_line_start: bool,
}

impl Formatter {
    fn emit(&mut self, s: &str) {
        self.output.push_str(s);
        if !s.is_empty() {
            self.at_line_start = s.as_bytes()[s.len() - 1] == b'\n';
        }
    }

    fn emit_indent(&mut self) {
        for _ in 0..self.indent {
            self.output.push('\t');
        }
        self.at_line_start = false;
    }

    fn emit_trivia_for_span(&mut self, s: Span) {
        let key = format!("{}:{}", s.start_line, s.start_column);
        if let Some(trivia) = self.trivia_map.get(&key).cloned() {
            self.emit_trivia(&trivia);
        }
    }

    fn emit_trivia_at_span_end(&mut self, s: Span) {
        let key = format!("{}:{}", s.end_line, s.end_column - 1);
        if let Some(trivia) = self.trivia_map.get(&key).cloned() {
            self.emit_trivia(&trivia);
        }
    }

    fn emit_trivia(&mut self, trivia: &[Trivia]) {
        let mut consecutive_newlines = 0;

        for t in trivia {
            match t.kind {
                TriviaKind::Newline => {
                    consecutive_newlines += 1;
                }
                TriviaKind::LineComment | TriviaKind::BlockComment | TriviaKind::DocComment => {
                    if consecutive_newlines > 1 {
                        for _ in 0..consecutive_newlines - 1 {
                            self.emit("\n");
                        }
                    }
                    if !self.at_line_start {
                        self.emit("\n");
                    }
                    self.emit_indent();
                    let text = t.text.clone();
                    self.emit(&text);
                    self.emit("\n");
                    consecutive_newlines = 0;
                }
                TriviaKind::Whitespace => {}
            }
        }

        if consecutive_newlines > 1 {
            for _ in 0..consecutive_newlines - 1 {
                self.emit("\n");
            }
        }
    }

    fn format_block(&mut self, block: &ast::BlockExpression, is_top_level: bool) {
        for (i, node) in block.body.iter().enumerate() {
            let node_span = ast::node_span(node);
            if node_span.start_line > 0 {
                self.emit_trivia_for_span(node_span);
            }

            if !self.at_line_start && i > 0 {
                self.emit("\n");
            }

            if self.at_line_start && !is_top_level {
                self.emit_indent();
            } else if self.at_line_start && is_top_level && i > 0 {
                // intentionally empty
            }

            self.format_node(node);

            if !self.at_line_start {
                self.emit("\n");
            }
        }
    }

    fn format_node(&mut self, node: &ast::Node) {
        match node {
            ast::Node::Statement(s) => self.format_statement(s),
            ast::Node::Expression(e) => self.format_expr(e),
        }
    }

    fn format_statement(&mut self, stmt: &ast::Statement) {
        match stmt {
            ast::Statement::VariableBinding(s) => {
                self.emit(&s.identifier.name);
                if let Some(typ) = &s.typ {
                    self.emit(" ");
                    self.format_type(typ);
                }
                self.emit(" = ");
                self.format_expr(&s.init);
            }
            ast::Statement::ConstBinding(s) => {
                self.emit("const ");
                self.emit(&s.identifier.name);
                if let Some(typ) = &s.typ {
                    self.emit(" ");
                    self.format_type(typ);
                }
                self.emit(" = ");
                self.format_expr(&s.init);
            }
            ast::Statement::TypePatternBinding(s) => {
                self.format_type(&s.typ);
                self.emit(" = ");
                self.format_expr(&s.init);
            }
            ast::Statement::TupleDestructuringBinding(s) => {
                self.emit("(");
                for (i, pattern) in s.patterns.iter().enumerate() {
                    if i > 0 {
                        self.emit(", ");
                    }
                    self.format_expr(pattern);
                }
                self.emit(") = ");
                self.format_expr(&s.init);
            }
            ast::Statement::FunctionDeclaration(func) => {
                self.format_function_declaration(func);
            }
            ast::Statement::StructDeclaration(s) => {
                self.emit("struct ");
                self.emit(&s.identifier.name);
                if !s.type_params.is_empty() {
                    self.emit("(");
                    for (i, tp) in s.type_params.iter().enumerate() {
                        if i > 0 {
                            self.emit(", ");
                        }
                        self.emit(&tp.name);
                    }
                    self.emit(")");
                }
                self.emit(" {\n");
                self.indent += 1;
                for field in &s.fields {
                    self.emit_trivia_for_span(field.identifier.span);
                    self.emit_indent();
                    self.emit(&field.identifier.name);
                    self.emit(" ");
                    self.format_type(&field.typ);
                    if let Some(init) = &field.init {
                        self.emit(" = ");
                        self.format_expr(init);
                    }
                    self.emit("\n");
                }
                self.indent -= 1;
                self.emit_indent();
                self.emit("}");
            }
            ast::Statement::EnumDeclaration(s) => {
                self.emit("enum ");
                self.emit(&s.identifier.name);
                if !s.type_params.is_empty() {
                    self.emit("(");
                    for (i, tp) in s.type_params.iter().enumerate() {
                        if i > 0 {
                            self.emit(", ");
                        }
                        self.emit(&tp.name);
                    }
                    self.emit(")");
                }
                self.emit(" {\n");
                self.indent += 1;
                for variant in &s.variants {
                    self.emit_trivia_for_span(variant.identifier.span);
                    self.emit_indent();
                    self.emit(&variant.identifier.name);
                    if !variant.payload.is_empty() {
                        self.emit("(");
                        for (i, p) in variant.payload.iter().enumerate() {
                            if i > 0 {
                                self.emit(", ");
                            }
                            self.format_type(p);
                        }
                        self.emit(")");
                    }
                    self.emit("\n");
                }
                self.indent -= 1;
                self.emit_indent();
                self.emit("}");
            }
            ast::Statement::ImportDeclaration(s) => {
                self.emit("from '");
                self.emit(&s.path);
                self.emit("' import ");
                for (i, spec) in s.specifiers.iter().enumerate() {
                    if i > 0 {
                        self.emit(", ");
                    }
                    self.emit(&spec.identifier.name);
                }
            }
            ast::Statement::ExportDeclaration(s) => {
                self.emit("export ");
                self.format_statement(&s.declaration);
            }
        }
    }

    fn format_array_element(&mut self, elem: &ast::ArrayElement) {
        match elem {
            ast::ArrayElement::SpreadElement(se) => {
                self.emit("..");
                if let Some(inner) = &se.expression {
                    self.format_expr(inner);
                }
            }
            ast::ArrayElement::Expression(e) => {
                self.format_expr(e);
            }
        }
    }

    fn format_expr(&mut self, expr: &ast::Expression) {
        match expr {
            ast::Expression::StringLiteral(e) => {
                self.emit(&format!("'{}'", escape_string(&e.value)));
            }
            ast::Expression::InterpolatedString(e) => {
                let mut s = String::from("'");
                for part in &e.parts {
                    match part {
                        ast::Expression::StringLiteral(sl) => {
                            s.push_str(&escape_string(&sl.value));
                        }
                        ast::Expression::Identifier(id) => {
                            s.push_str("${");
                            s.push_str(&id.name);
                            s.push('}');
                        }
                        other => {
                            s.push_str("${");
                            s.push_str(&self.format_expr_to_string(other));
                            s.push('}');
                        }
                    }
                }
                s.push('\'');
                self.emit(&s);
            }
            ast::Expression::NumberLiteral(e) => {
                self.emit(&e.value);
            }
            ast::Expression::BooleanLiteral(e) => {
                self.emit(if e.value { "true" } else { "false" });
            }
            ast::Expression::NoneExpression(_) => {
                self.emit("none");
            }
            ast::Expression::Identifier(e) => {
                self.emit(&e.name);
            }
            ast::Expression::TypeIdentifier(e) => {
                self.format_type(e);
            }
            ast::Expression::BinaryExpression(e) => {
                self.format_expr(&e.left);
                self.emit(&format!(" {} ", e.op.kind));
                self.format_expr(&e.right);
            }
            ast::Expression::UnaryExpression(e) => {
                let op = match e.op.kind {
                    Kind::PuncExclamationMark => "!",
                    Kind::PuncMinus => "-",
                    _ => "?",
                };
                self.emit(op);
                self.format_expr(&e.expression);
            }
            ast::Expression::BlockExpression(e) => {
                self.format_block_expr(e);
            }
            ast::Expression::IfExpression(e) => {
                self.emit("if ");
                self.format_expr(&e.condition);
                self.emit(" ");
                self.format_block_inline(&e.body);
                if let Some(else_body) = &e.else_body {
                    self.emit(" else ");
                    if matches!(else_body.as_ref(), ast::Expression::IfExpression(_)) {
                        self.format_expr(else_body);
                    } else {
                        self.format_block_inline(else_body);
                    }
                }
            }
            ast::Expression::MatchExpression(e) => {
                self.emit("match ");
                self.format_expr(&e.subject);
                self.emit(" {\n");
                self.indent += 1;
                for arm in &e.arms {
                    self.emit_indent();
                    self.format_expr(&arm.pattern);
                    self.emit(" -> ");
                    self.format_expr(&arm.body);
                    self.emit(",\n");
                }
                if e.span.end_line > 0 {
                    self.emit_trivia_at_span_end(e.span);
                }
                self.indent -= 1;
                self.emit_indent();
                self.emit("}");
            }
            ast::Expression::OrExpression(e) => {
                self.format_expr(&e.expression);
                self.emit(" or ");
                if let Some(receiver) = &e.receiver {
                    self.emit(&receiver.name);
                    self.emit(" -> ");
                }
                self.format_expr(&e.body);
            }
            ast::Expression::ErrorExpression(e) => {
                self.emit("error ");
                self.format_expr(&e.expression);
            }
            ast::Expression::FunctionExpression(func) => {
                self.format_function_expression(func);
            }
            ast::Expression::FunctionCallExpression(e) => {
                self.emit(&e.identifier.name);
                self.emit("(");
                for (i, arg) in e.arguments.iter().enumerate() {
                    if i > 0 {
                        self.emit(", ");
                    }
                    self.format_expr(arg);
                }
                self.emit(")");
            }
            ast::Expression::PropertyAccessExpression(e) => {
                self.format_expr(&e.left);
                self.emit(".");
                self.format_expr(&e.right);
            }
            ast::Expression::ArrayExpression(e) => {
                self.emit("[");
                for (i, elem) in e.elements.iter().enumerate() {
                    if i > 0 {
                        self.emit(", ");
                    }
                    self.format_array_element(elem);
                }
                self.emit("]");
            }
            ast::Expression::TupleExpression(e) => {
                self.emit("(");
                for (i, elem) in e.elements.iter().enumerate() {
                    if i > 0 {
                        self.emit(", ");
                    }
                    self.format_expr(elem);
                }
                self.emit(")");
            }
            ast::Expression::ArrayIndexExpression(e) => {
                self.format_expr(&e.expression);
                self.emit("[");
                self.format_expr(&e.index);
                self.emit("]");
            }
            ast::Expression::RangeExpression(e) => {
                self.format_expr(&e.start);
                self.emit("..");
                self.format_expr(&e.end);
            }
            ast::Expression::StructInitExpression(e) => {
                self.emit(&e.identifier.name);
                if !e.type_args.is_empty() {
                    self.emit("(");
                    for (i, ta) in e.type_args.iter().enumerate() {
                        if i > 0 {
                            self.emit(", ");
                        }
                        self.format_type(ta);
                    }
                    self.emit(")");
                }
                self.emit("{");

                if e.fields.len() <= 2 && self.is_short_struct_init(e) {
                    self.emit(" ");
                    for (i, field) in e.fields.iter().enumerate() {
                        if i > 0 {
                            self.emit(", ");
                        }
                        self.emit(&field.identifier.name);
                        self.emit(": ");
                        self.format_expr(&field.init);
                    }
                    self.emit(" }");
                } else {
                    self.emit("\n");
                    self.indent += 1;
                    for field in &e.fields {
                        self.emit_indent();
                        self.emit(&field.identifier.name);
                        self.emit(": ");
                        self.format_expr(&field.init);
                        self.emit(",\n");
                    }
                    self.indent -= 1;
                    self.emit_indent();
                    self.emit("}");
                }
            }
            ast::Expression::WildcardPattern(_) => {
                self.emit("else");
            }
            ast::Expression::OrPattern(e) => {
                for (i, pattern) in e.patterns.iter().enumerate() {
                    if i > 0 {
                        self.emit(" | ");
                    }
                    self.format_expr(pattern);
                }
            }
            ast::Expression::ErrorNode(e) => {
                self.emit(&format!("/* error: {} */", e.message));
            }
        }
    }

    fn format_function_declaration(&mut self, func: &ast::FunctionDeclaration) {
        self.emit("fn ");
        self.emit(&func.identifier.name);
        self.emit("(");
        for (i, param) in func.params.iter().enumerate() {
            if i > 0 {
                self.emit(", ");
            }
            self.emit(&param.identifier.name);
            if let Some(typ) = &param.typ {
                self.emit(" ");
                self.format_type(typ);
            }
        }
        self.emit(")");
        if let Some(ret) = &func.return_type {
            self.emit(" ");
            self.format_type(ret);
        }
        if let Some(err) = &func.error_type {
            if func.return_type.is_none() {
                self.emit(" ");
            }
            self.emit("!");
            self.format_type(err);
        }
        self.format_function_body(&func.body);
    }

    fn format_function_expression(&mut self, func: &ast::FunctionExpression) {
        self.emit("fn(");
        for (i, param) in func.params.iter().enumerate() {
            if i > 0 {
                self.emit(", ");
            }
            self.emit(&param.identifier.name);
            if let Some(typ) = &param.typ {
                self.emit(" ");
                self.format_type(typ);
            }
        }
        self.emit(")");
        if let Some(ret) = &func.return_type {
            self.emit(" ");
            self.format_type(ret);
        }
        if let Some(err) = &func.error_type {
            self.emit("!");
            self.format_type(err);
        }
        self.format_function_body(&func.body);
    }

    fn format_function_body(&mut self, body: &ast::Expression) {
        self.emit(" ");
        if let ast::Expression::BlockExpression(b) = body {
            self.format_block_expr(b);
        } else {
            self.format_expr(body);
        }
    }

    fn format_type(&mut self, typ: &ast::TypeIdentifier) {
        match &typ.kind {
            ast::TypeKind::OptionType(ot) => {
                self.emit("?");
                self.format_type(&ot.inner);
            }
            ast::TypeKind::ArrayType(at) => {
                self.emit("[]");
                self.format_type(&at.element);
            }
            ast::TypeKind::FunctionType(ft) => {
                self.emit("fn(");
                for (i, p) in ft.params.iter().enumerate() {
                    if i > 0 {
                        self.emit(", ");
                    }
                    self.format_type(p);
                }
                self.emit(")");
                if let Some(ret) = &ft.return_type {
                    self.emit(" ");
                    self.format_type(ret);
                }
                if let Some(err) = &ft.error_type {
                    self.emit("!");
                    self.format_type(err);
                }
            }
            ast::TypeKind::NamedType(nt) => {
                self.emit(&nt.identifier.name);
                if !nt.type_args.is_empty() {
                    self.emit("(");
                    for (i, ta) in nt.type_args.iter().enumerate() {
                        if i > 0 {
                            self.emit(", ");
                        }
                        self.format_type(ta);
                    }
                    self.emit(")");
                }
            }
        }
    }

    fn format_block_expr(&mut self, block: &ast::BlockExpression) {
        let has_close_comment = self.has_comment_trivia_at_span_end(block.span);
        if block.body.is_empty() && !has_close_comment {
            self.emit("{}");
        } else if block.body.len() == 1
            && self.is_simple_node(&block.body[0])
            && !self.has_comment_trivia_node(&block.body[0])
            && !has_close_comment
        {
            self.emit("{ ");
            self.format_node(&block.body[0]);
            self.emit(" }");
        } else {
            self.emit("{\n");
            self.indent += 1;
            for node in &block.body {
                let node_span = ast::node_span(node);
                if node_span.start_line > 0 {
                    self.emit_trivia_for_span(node_span);
                }
                if self.at_line_start {
                    self.emit_indent();
                }
                self.format_node(node);
                self.emit("\n");
            }
            if block.span.end_line > 0 {
                self.emit_trivia_at_span_end(block.span);
            }
            self.indent -= 1;
            self.emit_indent();
            self.emit("}");
        }
    }

    fn format_block_inline(&mut self, expr: &ast::Expression) {
        if let ast::Expression::BlockExpression(b) = expr {
            self.format_block_expr(b);
        } else if self.is_simple_expr(expr) {
            self.format_expr(expr);
        } else {
            self.emit("{ ");
            self.format_expr(expr);
            self.emit(" }");
        }
    }

    fn is_simple_expr(&self, expr: &ast::Expression) -> bool {
        match expr {
            ast::Expression::Identifier(_)
            | ast::Expression::NumberLiteral(_)
            | ast::Expression::StringLiteral(_)
            | ast::Expression::BooleanLiteral(_)
            | ast::Expression::NoneExpression(_) => true,
            ast::Expression::BinaryExpression(e) => {
                self.is_simple_expr(&e.left) && self.is_simple_expr(&e.right)
            }
            ast::Expression::UnaryExpression(e) => self.is_simple_expr(&e.expression),
            _ => false,
        }
    }

    fn is_simple_node(&self, node: &ast::Node) -> bool {
        match node {
            ast::Node::Statement(_) => false,
            ast::Node::Expression(e) => self.is_simple_expr(e),
        }
    }

    fn has_comment_trivia_node(&self, node: &ast::Node) -> bool {
        let node_span = ast::node_span(node);
        self.has_comment_trivia_at_span(node_span)
    }

    fn has_comment_trivia_at_span(&self, s: Span) -> bool {
        if s.start_line > 0 {
            let key = format!("{}:{}", s.start_line, s.start_column);
            if let Some(trivia) = self.trivia_map.get(&key) {
                for t in trivia {
                    if matches!(
                        t.kind,
                        TriviaKind::LineComment | TriviaKind::BlockComment | TriviaKind::DocComment
                    ) {
                        return true;
                    }
                }
            }
        }
        false
    }

    fn has_comment_trivia_at_span_end(&self, s: Span) -> bool {
        if s.end_line > 0 {
            let key = format!("{}:{}", s.end_line, s.end_column - 1);
            if let Some(trivia) = self.trivia_map.get(&key) {
                for t in trivia {
                    if matches!(
                        t.kind,
                        TriviaKind::LineComment | TriviaKind::BlockComment | TriviaKind::DocComment
                    ) {
                        return true;
                    }
                }
            }
        }
        false
    }

    fn is_short_struct_init(&self, init: &ast::StructInitExpression) -> bool {
        for field in &init.fields {
            if !self.is_simple_expr(&field.init) {
                return false;
            }
        }
        true
    }

    fn format_expr_to_string(&self, expr: &ast::Expression) -> String {
        let mut temp = Formatter {
            tokens: self.tokens.clone(),
            trivia_map: self.trivia_map.clone(),
            output: String::with_capacity(64),
            indent: 0,
            at_line_start: true,
        };
        temp.format_expr(expr);
        temp.output
    }
}

fn escape_string(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for c in s.bytes() {
        match c {
            b'\n' => result.push_str("\\n"),
            b'\r' => result.push_str("\\r"),
            b'\t' => result.push_str("\\t"),
            b'\\' => result.push_str("\\\\"),
            b'\'' => result.push_str("\\'"),
            0 => result.push_str("\\0"),
            other => result.push(other as char),
        }
    }
    result
}
