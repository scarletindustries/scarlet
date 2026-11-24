//! `scarlet lint` is the `.sca` half of the illegal-state sweep.
//!
//! Mordant's `bool_cluster` reads Rust and reports a struct whose `n` `bool`
//! fields allow `2^n` states when fewer are legal. Nothing read the Scarlet
//! side, so it could not see a type declared in `.sca`. The first sweep's only
//! real hit was `HeadFlags` in `http/h1.sca` as it was then, with three Bools
//! (T-310 later collapsed it). This command reports the same shape: a
//! constructor that carries several `Bool` fields.
//!
//! It used to report a second shape too, a catch-all arm over a sum type
//! declared in the same module. The compiler now refuses that outright (once a
//! match names some variants of a value it must name them all), so the census
//! no longer looks for it.
//!
//! This is a census, not a lint. It prints what the corpus contains and exits 0
//! either way, and every finding is something to read, not a defect. It is
//! deliberately not a CI gate and not a default-on compiler warning. The first
//! sweep found 3 constructors out of 469 declarations across five repos, and
//! only one of them (`HeadFlags`) was a real illegal state. The other two are
//! decoder test fixtures whose width is the property under test. Measured again
//! on this repo alone, it finds 2, both of them those fixtures in
//! `tests/programs/decoders.sca`, so a default-on warning would be right 0
//! times out of 2 here.
//!
//! It parses with the compiler's own scanner and parser instead of matching
//! text, because matching text has given the wrong answer here before. A `type`
//! inside a comment or a string literal is not a declaration, and a field list
//! can span lines. The scanner gets both right by construction, because
//! comments become trivia and a string's body becomes one token.
//!
//! A type can be declared inside a function body, so the walk goes into every
//! expression the AST can nest. Every run also checks the declarations it
//! inspected against the `type` keywords the scanner produced. If the two
//! disagree, the file is reported as not inspected, never quietly counted as
//! clean.
//!
//! # What it cannot see
//!
//! - **It matches `Bool` by name.** So a locally declared `type Bool` would be
//!   counted as the prelude's, and a module-qualified `m::Bool` would not be
//!   counted at all.
//! - **It can't tell whether the states are really illegal.** That is what the
//!   reading step is for. It reports the `opaque` column because that limits
//!   who can build a bad value, but it doesn't rule the shape out: an invariant
//!   kept by a private constructor is still only kept by convention.
//! - **It can't see layout fixed from outside.** `bool_cluster` skips a `repr`
//!   struct because something outside Rust decides its states. A Scarlet type
//!   that crosses the VM ABI is in the same position but has no syntactic mark,
//!   so it can't be filtered out here and has to be triaged by reading.
//! - **It can't see invariants that only live in prose.** An invariant stated
//!   in a comment and carried by no type has no syntactic mark. A word scan of
//!   `.sca` comments matches prose about module behaviour (`never modified`,
//!   `always close`) as often as a real invariant, and the cases we measured
//!   (T-286, T-309) sit next to expressions, not type declarations. Nothing
//!   checks for these, so read the comment by hand when it is next to a type.

use std::path::{Path, PathBuf};

use crate::ast;
use crate::module::SOURCE_EXTENSION;
use crate::parser::new_parser;
use crate::scanner::new_scanner;
use crate::token::{Keyword, Kind};

/// This is kept equal to Mordant's `FlagCluster::FLOOR` on purpose. `HeadFlags`
/// and the `ConnTokens` it faces across the ABI are one boundary in two
/// languages, and a census with a different threshold could not be compared
/// with the Rust side's.
pub const DEFAULT_MIN_BOOLS: usize = 2;

/// The census types are opaque. Their fields are private because [`report`] is
/// the only thing that reads them, so a caller outside this module gets the
/// rendered text instead of picking the census apart.
pub struct Finding {
    path: PathBuf,
    line: i32,
    type_name: String,
    /// The constructor that carries the bools. It equals `type_name` for a
    /// single-constructor type, which is what a Rust `struct` would be.
    ctor_name: String,
    opaque: bool,
    bools: Vec<String>,
}

/// A file the walk could not account for in full, and why. We keep these apart
/// from the findings because otherwise "nothing found here" and "not looked at
/// properly here" would print the same empty list.
pub struct Uninspected {
    path: PathBuf,
    reason: Skipped,
}

/// Why the walk could not account for a file.
enum Skipped {
    Unreadable(std::io::Error),
    /// The parser reported this many diagnostics, so the tree is partial.
    ParseDiagnostics(usize),
    /// The walk reached a different number of type declarations than the
    /// scanner found `type` keywords.
    WalkMismatch {
        walked: usize,
        keywords: usize,
    },
}

impl std::fmt::Display for Skipped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Skipped::Unreadable(e) => write!(f, "unreadable: {e}"),
            Skipped::ParseDiagnostics(n) => write!(f, "{n} parse diagnostic(s)"),
            Skipped::WalkMismatch { walked, keywords } => write!(
                f,
                "walk reached {walked} type declaration(s), scanner found {keywords} `type` keyword(s)"
            ),
        }
    }
}

#[derive(Default)]
pub struct Census {
    findings: Vec<Finding>,
    uninspected: Vec<Uninspected>,
    files: usize,
    type_decls: usize,
}

/// The roots are used as given, so you can point the census at the other `.sca`
/// corpora (`madder`, `website`) without the tool knowing they exist. The
/// driver defaults them to the working directory. As an xtask it defaulted to
/// the repo root from `CARGO_MANIFEST_DIR`, which in a released binary would
/// name the build machine's checkout, not the caller's.
pub fn run(roots: &[PathBuf], min_bools: usize) -> Census {
    let mut census = Census::default();
    let mut paths = Vec::new();
    for root in roots {
        collect_sources(root, &mut paths);
    }
    paths.sort();
    for path in paths {
        census.files = census.files.saturating_add(1);
        match std::fs::read_to_string(&path) {
            Ok(source) => inspect(&path, &source, min_bools, &mut census),
            Err(e) => census.uninspected.push(Uninspected {
                path,
                reason: Skipped::Unreadable(e),
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
            reason: Skipped::ParseDiagnostics(parsed.diagnostics.len()),
        });
        return;
    }

    let mut types = Vec::new();
    walk_nodes(&parsed.ast.body, &mut types);

    if types.len() != type_keywords {
        census.uninspected.push(Uninspected {
            path: path.to_path_buf(),
            reason: Skipped::WalkMismatch {
                walked: types.len(),
                keywords: type_keywords,
            },
        });
        return;
    }

    census.type_decls = census.type_decls.saturating_add(types.len());

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
                line: ctor.identifier.span.start_line.saturating_add(1),
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

/// `2^n` as a number, or written as a power when it does not fit. This matches
/// `bool_cluster`'s message, so the two sides print numbers you can compare.
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
mod tests;
