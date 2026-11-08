//! `scarlet lint` — the `.sca` half of the illegal-state sweep.
//!
//! Mordant's `bool_cluster` reads Rust and reports a struct whose `n` `bool`
//! fields admit `2^n` states when fewer are legal. Nothing read the Scarlet
//! side, so a type declared in `.sca` was invisible to it — the first sweep's
//! one genuine hit was the then-current `http/h1.sca` `HeadFlags` (three
//! Bools; T-310 later collapsed it). This reports the same shape: a
//! constructor carrying several `Bool` fields.
//!
//! It once reported a second shape, a catch-all arm over a locally declared
//! sum type. The compiler now refuses that outright (a match that names some
//! variants of a value names them all), so the census has nothing left to find
//! there and no longer looks.
//!
//! This is a census, not a lint: it prints what the corpus contains and exits 0
//! either way. Every finding is a candidate for reading, not a defect. It is
//! deliberately not a CI gate and not a default-on compiler warning: the first
//! sweep read 3 constructors out of 469 declarations across five repos, and
//! only one was a genuine illegal state (`HeadFlags`); the other two are
//! decoder test fixtures whose width is the property under test. Re-measured on
//! this repo alone, it is 2 findings, both those fixtures in
//! `tests/programs/decoders.sca`, so a default-on warning's precision here
//! would be 0/2.
//!
//! It parses with the compiler's own scanner and parser rather than matching
//! text, because the text-matching version of this question has been wrong here
//! before: a `type` inside a comment or a string literal is not a declaration,
//! and a field list spans lines. The scanner resolves both by construction —
//! comments become trivia and string bodies become one token.
//!
//! A type can be declared inside a function body, so the walk descends every
//! expression the AST can nest, and every run compares the declarations it
//! inspected against the `type` keywords the scanner produced. A file where the
//! two disagree is reported as uninspected, never silently counted as clean.
//!
//! # What it cannot see
//!
//! - **`Bool` is matched by name**, so a locally declared `type Bool` would be
//!   counted as the prelude's and a module-qualified `m::Bool` would not be
//!   counted at all.
//! - **Whether the states are actually illegal.** That is the reading step. The
//!   `opaque` column is reported because it bounds who can build a bad value,
//!   but it does not exclude the shape: an invariant held by a private
//!   constructor is still held by a convention.
//! - **Layout fixed from outside.** `bool_cluster` skips a `repr` struct because
//!   something outside Rust dictates its states. A Scarlet type crossing the VM
//!   ABI is in that position and carries no syntactic mark, so it cannot be
//!   filtered here and must be triaged by reading.
//! - **Prose-carried invariants.** An invariant stated in a comment and carried
//!   by no type has no syntactic mark. A word scan of `.sca` comments hits
//!   module-behaviour prose (`never modified`, `always close`) at the same rate
//!   as a real invariant, and the measured instances (T-286, T-309) live next to
//!   expressions, not on type declarations. Not instrumented; re-read by hand
//!   when the comment is next to a type.

use std::path::{Path, PathBuf};

use crate::ast;
use crate::module::SOURCE_EXTENSION;
use crate::parser::new_parser;
use crate::scanner::new_scanner;
use crate::token::{Keyword, Kind};

/// Mordant's `FlagCluster::FLOOR`. Held equal to it deliberately: `HeadFlags`
/// and the `ConnTokens` it faces across the ABI are one boundary in two
/// languages, and a census that answered at a different threshold could not be
/// compared with the Rust side's.
pub const DEFAULT_MIN_BOOLS: usize = 2;

/// The census types are opaque: their fields are private because [`report`] is
/// the only reader, so a caller outside this module takes the rendered text
/// rather than picking the census apart.
pub struct Finding {
    path: PathBuf,
    line: i32,
    type_name: String,
    /// The constructor carrying the bools. Equal to `type_name` for the
    /// single-constructor types that are the Rust `struct` analogue.
    ctor_name: String,
    opaque: bool,
    bools: Vec<String>,
}

/// A file the walk could not account for in full, and why. Kept apart from the
/// findings because "nothing found here" and "not looked at properly here"
/// print the same empty list otherwise.
pub struct Uninspected {
    path: PathBuf,
    reason: String,
}

#[derive(Default)]
pub struct Census {
    findings: Vec<Finding>,
    uninspected: Vec<Uninspected>,
    files: usize,
    type_decls: usize,
}

/// Roots are taken as given, so the census can be pointed at the other `.sca`
/// corpora (`madder`, `website`) without the tool knowing they exist. The
/// driver defaults them to the working directory: as an xtask this defaulted to
/// `CARGO_MANIFEST_DIR`'s repo root, which in a released binary would name the
/// build machine's checkout rather than the caller's.
pub fn run(roots: &[PathBuf], min_bools: usize) -> Census {
    let mut census = Census::default();
    let mut paths = Vec::new();
    for root in roots {
        collect_sources(root, &mut paths);
    }
    paths.sort();
    for path in paths {
        census.files += 1;
        match std::fs::read_to_string(&path) {
            Ok(source) => inspect(&path, &source, min_bools, &mut census),
            Err(e) => census.uninspected.push(Uninspected {
                path,
                reason: format!("unreadable: {e}"),
            }),
        }
    }
    census
}

fn collect_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    if dir.is_file() {
        if dir.extension().is_some_and(|e| e == SOURCE_EXTENSION) {
            out.push(dir.to_path_buf());
        }
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            // Build output and vendored dependencies are not the corpus, and
            // `target` holds copies of the stdlib that would double every count.
            if matches!(name.as_ref(), "target" | "node_modules" | ".git") {
                continue;
            }
            collect_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == SOURCE_EXTENSION) {
            out.push(path);
        }
    }
}

fn inspect(path: &Path, source: &str, min_bools: usize, census: &mut Census) {
    let (tokens, _) = new_scanner(source).scan_all();
    // `type` is reserved and every one opens a declaration, so this is the
    // number the walk below must account for, nested ones included.
    let type_keywords = tokens
        .iter()
        .filter(|t| t.kind == Kind::Keyword(Keyword::Type))
        .count();

    let mut scanner = new_scanner(source);
    let parsed = new_parser(&mut scanner).parse_program();
    if !parsed.diagnostics.is_empty() {
        census.uninspected.push(Uninspected {
            path: path.to_path_buf(),
            reason: format!("{} parse diagnostic(s)", parsed.diagnostics.len()),
        });
        return;
    }

    let mut types = Vec::new();
    walk_nodes(&parsed.ast.body, &mut types);

    if types.len() != type_keywords {
        census.uninspected.push(Uninspected {
            path: path.to_path_buf(),
            reason: format!(
                "walk reached {} type declaration(s), scanner found {type_keywords} `type` keyword(s)",
                types.len(),
            ),
        });
        return;
    }

    census.type_decls += types.len();

    for type_decl in &types {
        let ast::TypeBody::Variants { ctors, opaque } = &type_decl.body else {
            continue;
        };
        for ctor in ctors {
            let bools: Vec<String> = ctor
                .fields
                .iter()
                .filter(|f| is_prelude_bool(&f.typ))
                .map(|f| f.label.name.clone())
                .collect();
            if bools.len() < min_bools {
                continue;
            }
            census.findings.push(Finding {
                path: path.to_path_buf(),
                line: ctor.identifier.span.start_line + 1,
                type_name: type_decl.identifier.name.clone(),
                ctor_name: ctor.identifier.name.clone(),
                opaque: *opaque,
                bools,
            });
        }
    }
}

fn walk_nodes<'a>(nodes: &'a [ast::Node], types: &mut Vec<&'a ast::TypeDeclaration>) {
    for node in nodes {
        walk_node(node, types);
    }
}

fn walk_node<'a>(node: &'a ast::Node, types: &mut Vec<&'a ast::TypeDeclaration>) {
    match node {
        ast::Node::Statement(statement) => walk_statement(statement, types),
        ast::Node::Expression(expr) => walk_expr(expr, types),
    }
}

fn walk_statement<'a>(statement: &'a ast::Statement, types: &mut Vec<&'a ast::TypeDeclaration>) {
    match statement {
        ast::Statement::Declaration { decl, .. } => walk_decl(decl, types),
        ast::Statement::ImportDeclaration(_) => {}
        ast::Statement::Binding(b) => {
            walk_pattern(&b.pattern, types);
            walk_expr(&b.init, types);
        }
        ast::Statement::Backpass(b) => walk_expr(&b.call, types),
    }
}

fn walk_decl<'a>(decl: &'a ast::Declaration, types: &mut Vec<&'a ast::TypeDeclaration>) {
    match decl {
        ast::Declaration::Const(c) => walk_expr(&c.init, types),
        ast::Declaration::Function(f) => match &f.body {
            ast::FnBody::Block(body) => walk_expr(body, types),
            ast::FnBody::Vm(_) => {}
        },
        ast::Declaration::Type(t) => types.push(t),
    }
}

fn walk_expr<'a>(expr: &'a ast::Expression, types: &mut Vec<&'a ast::TypeDeclaration>) {
    match expr {
        ast::Expression::ArrayExpression(a) => {
            for el in &a.elements {
                match el {
                    ast::ArrayElement::Expression(e)
                    | ast::ArrayElement::SpreadElement(ast::SpreadElement {
                        expression: e, ..
                    }) => {
                        walk_expr(e, types);
                    }
                }
            }
        }
        ast::Expression::ArrayIndexExpression(a) => {
            walk_expr(&a.expression, types);
            walk_expr(&a.index, types);
        }
        ast::Expression::BinaryExpression(b) => {
            walk_expr(&b.left, types);
            walk_expr(&b.right, types);
        }
        ast::Expression::BinaryLiteral(b) => {
            for seg in &b.segments {
                walk_expr(&seg.value, types);
                if let Some(size) = seg.spec.size_expr() {
                    walk_expr(size, types);
                }
            }
        }
        ast::Expression::BlockExpression(b) => walk_nodes(&b.body, types),
        ast::Expression::ErrorNode(_) => {}
        ast::Expression::FunctionCallExpression(f) => {
            walk_expr(&f.callee, types);
            for arg in &f.arguments {
                match arg {
                    ast::CallArg::Positional(e) | ast::CallArg::Spread(e) => {
                        walk_expr(e, types);
                    }
                    ast::CallArg::Labeled { value, .. } => walk_expr(value, types),
                }
            }
        }
        ast::Expression::FunctionExpression(f) => walk_expr(&f.body, types),
        ast::Expression::Identifier(_) => {}
        ast::Expression::IfExpression(i) => {
            walk_expr(&i.condition, types);
            walk_expr(&i.body, types);
            walk_expr(&i.else_body, types);
        }
        ast::Expression::InterpolatedString(s) => {
            for part in &s.parts {
                if let ast::InterpPart::Expr(e) = part {
                    walk_expr(e, types);
                }
            }
        }
        ast::Expression::MatchExpression(m) => {
            walk_expr(&m.subject, types);
            for arm in &m.arms {
                walk_pattern(&arm.pattern, types);
                if let Some(guard) = &arm.guard {
                    walk_expr(guard, types);
                }
                walk_expr(&arm.body, types);
            }
        }
        ast::Expression::NumberLiteral(_) | ast::Expression::ModuleAccessExpression(_) => {}
        ast::Expression::OrExpression(o) => {
            walk_expr(&o.expression, types);
            walk_expr(&o.body, types);
        }
        ast::Expression::PipeExpression(p) => {
            walk_expr(&p.left, types);
            walk_expr(&p.right, types);
        }
        ast::Expression::PropertyAccessExpression(p) => walk_expr(&p.left, types),
        ast::Expression::RangeExpression(r) => {
            walk_expr(&r.start, types);
            walk_expr(&r.end, types);
        }
        ast::Expression::StringLiteral(_) => {}
        ast::Expression::TupleExpression(t) => {
            for e in &t.elements {
                walk_expr(e, types);
            }
        }
        ast::Expression::UnaryExpression(u) => walk_expr(&u.expression, types),
    }
}

fn walk_pattern<'a>(pattern: &'a ast::Pattern, types: &mut Vec<&'a ast::TypeDeclaration>) {
    match pattern {
        ast::Pattern::Var { .. } | ast::Pattern::Literal(_) | ast::Pattern::Range { .. } => {}
        ast::Pattern::Constructor { args, .. } => {
            for arg in args {
                let p = match arg {
                    ast::PatternArg::Positional(p)
                    | ast::PatternArg::Labeled { pattern: p, .. } => p,
                };
                walk_pattern(p, types);
            }
        }
        ast::Pattern::Tuple { elements, .. } => {
            for p in elements {
                walk_pattern(p, types);
            }
        }
        ast::Pattern::Array { elements, .. } => {
            for el in elements {
                if let ast::ArrayPatternElement::Pattern(p) = el {
                    walk_pattern(p, types);
                }
            }
        }
        ast::Pattern::Binary { segments, .. } => {
            for seg in segments {
                walk_pattern(&seg.value, types);
                if let Some(size) = seg.spec.size_expr() {
                    walk_expr(size, types);
                }
            }
        }
        ast::Pattern::Or { first, rest, .. } => {
            walk_pattern(first, types);
            for p in rest {
                walk_pattern(p, types);
            }
        }
    }
}

fn is_prelude_bool(typ: &ast::TypeIdentifier) -> bool {
    match &typ.kind {
        ast::TypeKind::NamedType(named) => {
            named.qualifier.is_none()
                && named.identifier.name == "Bool"
                && named.type_args.is_empty()
        }
        ast::TypeKind::FunctionType(_) | ast::TypeKind::TupleType(_) => false,
    }
}

/// `2^n`, or the unevaluated power when it does not fit — the same contract as
/// `bool_cluster`'s message, so the two sides print comparable numbers.
pub(crate) fn states(n: usize) -> String {
    match u32::try_from(n).ok().and_then(|n| 2u64.checked_pow(n)) {
        Some(n) => n.to_string(),
        None => format!("2^{n}"),
    }
}

pub fn report(census: &Census, min_bools: usize) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "scarlet lint: constructors with >= {min_bools} Bool fields\n\n"
    ));
    for finding in &census.findings {
        let name = if finding.type_name == finding.ctor_name {
            finding.type_name.clone()
        } else {
            format!("{}::{}", finding.type_name, finding.ctor_name)
        };
        out.push_str(&format!(
            "{}:{} {name}{} — {} Bool fields, {} states: {}\n",
            finding.path.display(),
            finding.line,
            if finding.opaque { " (opaque)" } else { "" },
            finding.bools.len(),
            states(finding.bools.len()),
            finding.bools.join(", "),
        ));
    }
    if census.findings.is_empty() {
        out.push_str("(none)\n");
    }
    out.push_str(&format!(
        "\n{} finding(s) over {} type declaration(s) in {} file(s)\n",
        census.findings.len(),
        census.type_decls,
        census.files,
    ));

    out.push_str(&format!(
        "{} file(s) not inspected\n",
        census.uninspected.len()
    ));
    for skipped in &census.uninspected {
        out.push_str(&format!(
            "  {}: {}\n",
            skipped.path.display(),
            skipped.reason
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{Census, DEFAULT_MIN_BOOLS, inspect, states};

    fn census_of(source: &str) -> Census {
        let mut census = Census::default();
        inspect(
            Path::new("test.sca"),
            source,
            DEFAULT_MIN_BOOLS,
            &mut census,
        );
        census
    }

    #[test]
    fn states_is_two_to_the_n() {
        assert_eq!(states(2), "4");
        assert_eq!(states(3), "8");
        assert_eq!(states(64), "2^64");
    }

    /// The shape the census exists to find. The fixture keeps three Bools
    /// even though T-310 collapsed the `h1.sca` type that motivated it —
    /// this tests the finder, not the current spelling.
    #[test]
    fn finds_a_bool_cluster() {
        let census = census_of(
            "pub opaque type HeadFlags {\n\
             \tconn_close Bool\n\
             \tconn_keep_alive Bool\n\
             \texpect_100_continue Bool\n\
             }\n",
        );
        assert_eq!(census.uninspected.len(), 0);
        assert_eq!(census.findings.len(), 1);
        let finding = &census.findings[0];
        assert_eq!(finding.type_name, "HeadFlags");
        assert!(finding.opaque);
        assert_eq!(
            finding.bools,
            ["conn_close", "conn_keep_alive", "expect_100_continue"]
        );
    }

    /// One `Bool` is under the floor, and a second field of another type does
    /// not make a cluster — the count is of `Bool` fields, not of fields.
    #[test]
    fn one_bool_is_not_a_cluster() {
        let census = census_of("pub type Flags {\n\tclose Bool\n\tcount Int\n}\n");
        assert_eq!(census.findings.len(), 0);
        assert_eq!(census.type_decls, 1);
    }

    /// Bools in two different constructors of one sum type are never
    /// simultaneously inhabited, so they are not a cluster. The Rust analogue
    /// is that `bool_cluster` reads one struct, not an enum's whole variant set.
    #[test]
    fn bools_in_separate_constructors_do_not_combine() {
        let census = census_of("pub type Step {\n\tOpen(a Bool)\n\tShut(b Bool)\n}\n");
        assert_eq!(census.findings.len(), 0);
    }

    /// The defect a text search makes here: `type` and `Bool` inside a comment
    /// or a string are not a declaration. The scanner never emits them as
    /// tokens, so neither the finding count nor the coverage cross-check moves.
    #[test]
    fn comments_and_strings_are_not_declarations() {
        let census = census_of(
            "// pub type Ghost {\n\
             //\ta Bool\n\
             //\tb Bool\n\
             // }\n\
             pub const doc = \"pub type Ghost { a Bool b Bool }\"\n",
        );
        assert_eq!(census.findings.len(), 0);
        assert_eq!(census.type_decls, 0);
        assert_eq!(census.uninspected.len(), 0);
    }

    /// A declaration nested in a block body is inside the walk. The coverage
    /// cross-check must agree, and a nested bool cluster is a finding — the
    /// top-level-only walk used to report this file uninspected instead.
    #[test]
    fn a_nested_declaration_is_inspected() {
        let census = census_of(
            "pub fn make() {\n\
             \ttype Inner {\n\
             \t\tOne(a Bool, b Bool)\n\
             \t}\n\
             \t0\n\
             }\n",
        );
        assert_eq!(census.uninspected.len(), 0);
        assert_eq!(census.type_decls, 1);
        assert_eq!(census.findings.len(), 1);
        assert_eq!(census.findings[0].type_name, "Inner");
    }

    /// A file that does not parse yields no declarations, which is the same
    /// empty list a clean file yields. It must be counted as uninspected.
    #[test]
    fn a_file_that_does_not_parse_is_not_clean() {
        let census = census_of("pub type Broken {\n\ta Bool\n\tb Bool\n");
        assert_eq!(census.findings.len(), 0);
        assert_eq!(census.uninspected.len(), 1);
    }
}
