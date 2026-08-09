use crate::span::Span;

/// Delegating `span()` / `span_mut()` for an enum whose variants each wrap a
/// single struct with a `span: Span` field.
macro_rules! impl_span {
    ($enum:ident: $($variant:ident),+ $(,)?) => {
        impl $enum {
            pub fn span(&self) -> Span {
                match self {
                    $($enum::$variant(x) => x.span,)+
                }
            }

            pub fn span_mut(&mut self) -> &mut Span {
                match self {
                    $($enum::$variant(x) => &mut x.span,)+
                }
            }
        }
    };
}

#[derive(Debug, Clone)]
pub struct StringLiteral {
    pub value: String,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum InterpPart {
    Literal(StringLiteral),
    Expr(Box<Expression>),
}

#[derive(Debug, Clone)]
pub struct InterpolatedString {
    pub parts: Vec<InterpPart>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct NumberLiteral {
    pub value: String,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ErrorNode {
    pub message: String,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Identifier {
    pub name: String,
    pub span: Span,
}

/// `@name` or `@name(arg, ...)` ahead of a declaration. Args are bare
/// identifiers, never expressions, so the parser never has to guess whether
/// `@x(a + b)` is an attribute or an error.
#[derive(Debug, Clone)]
pub struct Attribute {
    pub name: Identifier,
    pub args: Vec<Identifier>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum TypeKind {
    NamedType(NamedType),
    FunctionType(FunctionType),
    TupleType(TupleType),
}

#[derive(Debug, Clone)]
pub struct NamedType {
    pub identifier: Identifier,
    pub type_args: Vec<TypeIdentifier>,
}

#[derive(Debug, Clone)]
pub struct FunctionType {
    pub params: Vec<TypeIdentifier>,
    pub return_type: Option<Box<TypeIdentifier>>,
}

#[derive(Debug, Clone)]
pub struct TupleType {
    pub elements: Vec<TypeIdentifier>,
}

#[derive(Debug, Clone)]
pub struct TypeIdentifier {
    pub kind: TypeKind,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

impl BinaryOp {
    pub fn symbol(self) -> &'static str {
        match self {
            Self::Add => "+",
            Self::Sub => "-",
            Self::Mul => "*",
            Self::Div => "/",
            Self::Mod => "%",
            Self::Eq => "==",
            Self::Ne => "!=",
            Self::Lt => "<",
            Self::Le => "<=",
            Self::Gt => ">",
            Self::Ge => ">=",
            Self::And => "&&",
            Self::Or => "||",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Not,
    Neg,
}

impl UnaryOp {
    pub fn symbol(self) -> &'static str {
        match self {
            Self::Not => "!",
            Self::Neg => "-",
        }
    }
}

#[derive(Debug, Clone)]
pub struct VariableBinding {
    pub doc: Option<String>,
    pub identifier: Identifier,
    pub typ: Option<TypeIdentifier>,
    pub init: Expression,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ConstBinding {
    pub doc: Option<String>,
    pub identifier: Identifier,
    pub typ: Option<TypeIdentifier>,
    pub init: Expression,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct TupleDestructuringBinding {
    pub patterns: Vec<Pattern>,
    pub init: Expression,
    pub span: Span,
}

/// `UpperIdent = expr` at statement position: assert `expr` has the named type
/// and discard the value. The name must resolve to a 0-arg type, not a
/// constructor.
#[derive(Debug, Clone)]
pub struct TypedDiscard {
    pub ty_name: Identifier,
    pub init: Expression,
    pub span: Span,
}

/// `Ctor(p1, ..) = expr` at statement position. An irrefutable single-arm
/// destructure; the compiler lowers it to a one-arm match and exhaustiveness
/// must accept it (so only single-constructor types qualify).
#[derive(Debug, Clone)]
pub struct CtorDestructuringBinding {
    pub name: Identifier,
    pub args: Vec<PatternArg>,
    pub rest: bool,
    /// Span of just the `Ctor(args)` head, not the `= init` tail — this is what
    /// diagnostics against the reconstructed pattern should point at.
    pub pattern_span: Span,
    pub init: Expression,
    pub span: Span,
}

impl CtorDestructuringBinding {
    /// Re-materialise the constructor pattern for consumers that operate on
    /// the general [`Pattern`] shape (typing, exhaustiveness, codegen).
    pub fn as_pattern(&self) -> Pattern {
        Pattern::Constructor {
            qualifier: None,
            name: self.name.clone(),
            args: self.args.clone(),
            rest: self.rest,
            span: self.pattern_span,
        }
    }
}

#[derive(Debug, Clone)]
pub struct FunctionParameter {
    pub identifier: Identifier,
    pub typ: Option<TypeIdentifier>,
}

/// A function declaration's body. Encoding "normal fn" vs "`@vm` intrinsic" in
/// the type (instead of `Option<Expression>` + a separate `@vm` predicate)
/// makes the desync class impossible: the parser is the single authority, a
/// `@vm` fn always carries its op, and a body-less normal fn is
/// unrepresentable. (Previously the parser keyed body-presence on the `@vm`
/// attribute *name* while the siphon keyed removal on the `@vm` *arg*; a bare
/// `@vm` diverged the two and panicked an `unreachable!()`.)
#[derive(Debug, Clone)]
pub enum FnBody {
    /// A normal function with an AL expression body.
    Block(Expression),
    /// A `@vm(op)` intrinsic: no AL body; carries the VM op identifier.
    Vm(Identifier),
}

#[derive(Debug, Clone)]
pub struct FunctionDeclaration {
    pub doc: Option<String>,
    pub attributes: Vec<Attribute>,
    pub identifier: Identifier,
    pub return_type: Option<TypeIdentifier>,
    pub params: Vec<FunctionParameter>,
    pub body: FnBody,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct TypeDeclaration {
    pub doc: Option<String>,
    pub attributes: Vec<Attribute>,
    pub identifier: Identifier,
    pub type_params: Vec<Identifier>,
    pub body: TypeBody,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum Declaration {
    Const(ConstBinding),
    Function(FunctionDeclaration),
    Type(TypeDeclaration),
}

impl_span!(Declaration: Const, Function, Type);

#[derive(Debug, Clone)]
pub enum TypeBody {
    /// `type Name { Ctor ... }`. `opaque` lives here so that opaque-on-alias
    /// and opaque-on-external are unrepresentable in the AST.
    Variants {
        ctors: Vec<Constructor>,
        opaque: bool,
    },
    Alias(TypeIdentifier),
    /// `pub type Name` with no body — a host-backed handle.
    External,
}

#[derive(Debug, Clone)]
pub struct Constructor {
    pub doc: Option<String>,
    pub identifier: Identifier,
    pub fields: Vec<ConstructorField>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ConstructorField {
    pub label: Identifier,
    pub typ: TypeIdentifier,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ImportItem {
    pub name: Identifier,
    pub alias: Option<Identifier>,
}

/// A relative marker segment of an import path: the `.` / `..` in
/// `import ../lib/util`. A distinct type — not the literal strings "." /
/// ".." inside the name list — so the resolver can never confuse a marker
/// with a module name (or vice versa: a module literally named `.` cannot
/// be smuggled in as a marker).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelSeg {
    /// `.` — the importing file's own directory.
    CurrentDir,
    /// `..` — one directory up.
    ParentDir,
}

impl std::fmt::Display for RelSeg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            RelSeg::CurrentDir => ".",
            RelSeg::ParentDir => "..",
        })
    }
}

/// A module path as written in an `import`: relative markers first, module
/// names after. The split makes "markers lead, names follow" structurally
/// true — `import a/../b` is unrepresentable, not merely rejected.
#[derive(Debug, Clone)]
pub struct ImportPath {
    /// Leading `.` / `..` segments; empty for stdlib (`al/...`) and bare paths.
    pub leading: Vec<RelSeg>,
    /// The module-name segments (`al`/`string` in `import al/string`).
    pub names: Vec<String>,
}

impl ImportPath {
    /// Wrap an already-canonical, non-relative path (a stdlib `al/...` path
    /// or a resolved file identity) as an import with no relative markers.
    pub fn canonical(names: Vec<String>) -> Self {
        ImportPath {
            leading: Vec::new(),
            names,
        }
    }

    pub fn is_relative(&self) -> bool {
        !self.leading.is_empty()
    }
}

/// The path as the user wrote it, `/`-joined (e.g. `../lib/util`).
impl std::fmt::Display for ImportPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut sep = "";
        for seg in &self.leading {
            write!(f, "{sep}{seg}")?;
            sep = "/";
        }
        for name in &self.names {
            write!(f, "{sep}{name}")?;
            sep = "/";
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct ImportDeclaration {
    pub path: ImportPath,
    pub alias: Option<Identifier>,
    pub items: Vec<ImportItem>,
    /// Span of the final module-name path segment (e.g. `string` in `import al/string`).
    pub path_span: Span,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum Statement {
    Declaration {
        decl: Box<Declaration>,
        public: bool,
    },
    ImportDeclaration(ImportDeclaration),
    TupleDestructuringBinding(TupleDestructuringBinding),
    TypedDiscard(TypedDiscard),
    CtorDestructuringBinding(CtorDestructuringBinding),
    VariableBinding(VariableBinding),
}

impl Statement {
    pub fn span(&self) -> Span {
        match self {
            Statement::Declaration { decl, .. } => decl.span(),
            Statement::ImportDeclaration(x) => x.span,
            Statement::TupleDestructuringBinding(x) => x.span,
            Statement::TypedDiscard(x) => x.span,
            Statement::CtorDestructuringBinding(x) => x.span,
            Statement::VariableBinding(x) => x.span,
        }
    }
}

#[derive(Debug, Clone)]
pub struct FunctionExpression {
    pub return_type: Option<TypeIdentifier>,
    pub params: Vec<FunctionParameter>,
    pub body: Box<Expression>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct IfExpression {
    pub condition: Box<Expression>,
    pub body: Box<Expression>,
    pub span: Span,
    pub else_body: Box<Expression>,
}

#[derive(Debug, Clone)]
pub struct MatchArm {
    pub pattern: Pattern,
    pub guard: Option<Expression>,
    pub body: Expression,
}

#[derive(Debug, Clone)]
pub struct MatchExpression {
    pub subject: Box<Expression>,
    pub arms: Vec<MatchArm>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct OrExpression {
    pub expression: Box<Expression>,
    pub receiver: Option<Identifier>,
    pub body: Box<Expression>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct BinaryExpression {
    pub left: Box<Expression>,
    pub right: Box<Expression>,
    pub op: BinaryOp,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct UnaryExpression {
    pub expression: Box<Expression>,
    pub op: UnaryOp,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ArrayExpression {
    pub elements: Vec<ArrayElement>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct TupleExpression {
    pub elements: Vec<Expression>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ArrayIndexExpression {
    pub expression: Box<Expression>,
    pub index: Box<Expression>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct RangeExpression {
    pub start: Box<Expression>,
    pub end: Box<Expression>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum PropertyKey {
    Field(Identifier),
    TupleIndex(NumberLiteral),
}

impl PropertyKey {
    pub fn span(&self) -> Span {
        match self {
            PropertyKey::Field(id) => id.span,
            PropertyKey::TupleIndex(n) => n.span,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PropertyAccessExpression {
    pub left: Box<Expression>,
    pub right: PropertyKey,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct FunctionCallExpression {
    pub callee: Box<Expression>,
    pub arguments: Vec<CallArg>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum CallArg {
    Positional(Expression),
    Labeled {
        label: Identifier,
        value: Expression,
    },
    Spread(Expression),
}

impl CallArg {
    pub fn span(&self) -> Span {
        match self {
            CallArg::Positional(e) => e.span(),
            CallArg::Labeled { label, value } => label.span.union(&value.span()),
            CallArg::Spread(e) => e.span(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct BlockExpression {
    pub body: Vec<Node>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct SpreadElement {
    pub expression: Expression,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum ArrayElement {
    Expression(Expression),
    SpreadElement(SpreadElement),
}

/// The parsed `:spec` suffix of a `<<>>` segment: how the value is encoded
/// into / decoded from the binary, and how wide it is. An `Int` width is
/// always in bits, a `Binary` size always in bytes, and `Utf8` never carries
/// a size — no other combination is expressible.
#[derive(Debug, Clone)]
pub enum BinSpec {
    /// `:N` / `:size(expr)` — an integer of `bits` bits; `None` means the
    /// default width (8, supplied downstream).
    Int { bits: Option<Expression> },
    /// `:bytes(expr)` — a sub-binary of `bytes` bytes; `:binary` (`None`)
    /// consumes the remainder.
    Binary { bytes: Option<Expression> },
    /// `:utf8`, or the default for a bare string segment.
    Utf8,
}

impl BinSpec {
    /// The runtime size expression, if the spec carries one
    /// (`:N` / `:size(expr)` / `:bytes(expr)`).
    pub fn size_expr(&self) -> Option<&Expression> {
        match self {
            BinSpec::Int { bits } => bits.as_ref(),
            BinSpec::Binary { bytes } => bytes.as_ref(),
            BinSpec::Utf8 => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct BinSegment {
    pub value: Expression,
    pub spec: BinSpec,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct BinaryLiteral {
    pub segments: Vec<BinSegment>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum Pattern {
    Wildcard {
        span: Span,
    },
    Var {
        name: Identifier,
    },
    Constructor {
        /// `io` in `io.NotFound(path)` — the module qualifier the constructor
        /// was reached through. `None` for a constructor in scope, whether
        /// declared locally or brought in by `import al/io.{NotFound}`; both
        /// spellings denote the same constructor.
        qualifier: Option<Identifier>,
        name: Identifier,
        args: Vec<PatternArg>,
        rest: bool,
        span: Span,
    },
    Tuple {
        elements: Vec<Pattern>,
        span: Span,
    },
    Array {
        elements: Vec<ArrayPatternElement>,
        span: Span,
    },
    Binary {
        segments: Vec<BinSegmentPat>,
        rest: Option<BinaryPatternRest>,
        span: Span,
    },
    Literal(PatternLiteral),
    /// `p | q | ..` — at least two alternatives: `first` and the non-empty
    /// tail the parser accumulated after the first `|`.
    Or {
        first: Box<Pattern>,
        rest: Vec<Pattern>,
        span: Span,
    },
    Range {
        start: NumberLiteral,
        end: NumberLiteral,
        span: Span,
    },
}

#[derive(Debug, Clone)]
pub enum PatternLiteral {
    Number(NumberLiteral),
    String(StringLiteral),
}

#[derive(Debug, Clone)]
pub enum PatternArg {
    Positional(Pattern),
    Labeled { label: Identifier, pattern: Pattern },
}

#[derive(Debug, Clone)]
pub enum ArrayPatternElement {
    Pattern(Pattern),
    Spread {
        binding: Option<Identifier>,
        span: Span,
    },
}

#[derive(Debug, Clone)]
pub struct BinSegmentPat {
    pub value: Pattern,
    pub spec: BinSpec,
    pub span: Span,
}

impl BinSegmentPat {
    /// The string of a `<<'literal'>>` (Utf8 string-literal) pattern segment.
    pub fn utf8_literal(&self) -> Option<&str> {
        if !matches!(self.spec, BinSpec::Utf8) {
            return None;
        }
        match &self.value {
            Pattern::Literal(PatternLiteral::String(s)) => Some(&s.value),
            _ => None,
        }
    }
}

/// Trailing `..rest` in a `<<>>` pattern, capturing the remaining bytes.
#[derive(Debug, Clone)]
pub struct BinaryPatternRest {
    pub binding: Option<Identifier>,
    pub span: Span,
}

/// How [`Pattern::for_each_binder`] treats the alternatives of an or-pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrAlternatives {
    /// Walk every alternative. Scope analysis needs this: alternatives past
    /// the first can still contain segment size expressions that reference
    /// outer names.
    All,
    /// Walk only the first alternative. Typing enforces that all
    /// alternatives bind the identical set, so codegen can take the first
    /// as canonical.
    First,
}

/// A binding-relevant site found by [`Pattern::for_each_binder`].
pub enum PatternBinder<'a> {
    /// An identifier the pattern binds: a `Var`, an array spread binding,
    /// or a binary `..rest` binding.
    Name(&'a Identifier),
    /// A binary segment size expression. Not a binder itself, but it can
    /// reference outer names, so scope analysis must walk it.
    SizeExpr(&'a Expression),
}

impl Pattern {
    pub fn span(&self) -> Span {
        match self {
            Pattern::Wildcard { span } => *span,
            Pattern::Var { name } => name.span,
            Pattern::Constructor { span, .. } => *span,
            Pattern::Tuple { span, .. } => *span,
            Pattern::Array { span, .. } => *span,
            Pattern::Binary { span, .. } => *span,
            Pattern::Literal(PatternLiteral::Number(n)) => n.span,
            Pattern::Literal(PatternLiteral::String(s)) => s.span,
            Pattern::Or { span, .. } => *span,
            Pattern::Range { span, .. } => *span,
        }
    }

    /// Walk every binding site in this pattern in source order, calling `f`
    /// with each bound identifier and each binary segment size expression.
    /// Constructor names are not reported — they are uppercase and can never
    /// be binders.
    pub fn for_each_binder<'a>(
        &'a self,
        or_mode: OrAlternatives,
        f: &mut dyn FnMut(PatternBinder<'a>),
    ) {
        match self {
            Pattern::Wildcard { .. } | Pattern::Literal(_) | Pattern::Range { .. } => {}
            Pattern::Var { name } => f(PatternBinder::Name(name)),
            Pattern::Constructor { args, .. } => {
                for arg in args {
                    let p = match arg {
                        PatternArg::Positional(p) => p,
                        PatternArg::Labeled { pattern, .. } => pattern,
                    };
                    p.for_each_binder(or_mode, f);
                }
            }
            Pattern::Tuple { elements, .. } => {
                for p in elements {
                    p.for_each_binder(or_mode, f);
                }
            }
            Pattern::Array { elements, .. } => {
                for el in elements {
                    match el {
                        ArrayPatternElement::Pattern(p) => p.for_each_binder(or_mode, f),
                        ArrayPatternElement::Spread { binding, .. } => {
                            if let Some(id) = binding {
                                f(PatternBinder::Name(id));
                            }
                        }
                    }
                }
            }
            Pattern::Binary { segments, rest, .. } => {
                for seg in segments {
                    seg.value.for_each_binder(or_mode, f);
                    if let Some(sz) = seg.spec.size_expr() {
                        f(PatternBinder::SizeExpr(sz));
                    }
                }
                if let Some(r) = rest
                    && let Some(id) = &r.binding
                {
                    f(PatternBinder::Name(id));
                }
            }
            Pattern::Or { first, rest, .. } => {
                first.for_each_binder(or_mode, f);
                if or_mode == OrAlternatives::All {
                    for p in rest {
                        p.for_each_binder(or_mode, f);
                    }
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
pub enum Expression {
    ArrayExpression(ArrayExpression),
    ArrayIndexExpression(ArrayIndexExpression),
    BinaryExpression(BinaryExpression),
    BinaryLiteral(BinaryLiteral),
    BlockExpression(BlockExpression),
    ErrorNode(ErrorNode),
    FunctionCallExpression(FunctionCallExpression),
    FunctionExpression(FunctionExpression),
    Identifier(Identifier),
    IfExpression(IfExpression),
    InterpolatedString(InterpolatedString),
    MatchExpression(MatchExpression),
    NumberLiteral(NumberLiteral),
    OrExpression(OrExpression),
    PropertyAccessExpression(PropertyAccessExpression),
    RangeExpression(RangeExpression),
    StringLiteral(StringLiteral),
    TupleExpression(TupleExpression),
    UnaryExpression(UnaryExpression),
}

impl_span!(
    Expression: ArrayExpression, ArrayIndexExpression, BinaryExpression, BinaryLiteral,
    BlockExpression, ErrorNode, FunctionCallExpression, FunctionExpression, Identifier,
    IfExpression, InterpolatedString, MatchExpression, NumberLiteral, OrExpression,
    PropertyAccessExpression, RangeExpression, StringLiteral, TupleExpression, UnaryExpression,
);

#[derive(Debug, Clone)]
pub enum Node {
    Statement(Box<Statement>),
    Expression(Expression),
}

impl Node {
    pub fn span(&self) -> Span {
        match self {
            Node::Statement(s) => s.span(),
            Node::Expression(e) => e.span(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{parser, scanner};

    fn span(sl: i32, sc: i32, el: i32, ec: i32) -> Span {
        Span {
            start_line: sl,
            start_column: sc,
            end_line: el,
            end_column: ec,
        }
    }

    // `Statement::span` delegates to each statement kind, including the
    // tuple-destructuring and typed-discard forms. Parse a program containing
    // every statement shape and assert each reports a well-formed span.
    #[test]
    fn statement_span_covers_all_kinds() {
        let src = "import al/array\n\
                   x = 5\n\
                   fn f() { 0 }\n\
                   pub fn g() { 1 }\n\
                   (a, b) = (1, 2)\n\
                   Some(x) = Some(1)\n\
                   Nil = println('hi')\n";
        let mut sc = scanner::new_scanner(src.to_string());
        let p = parser::new_parser(&mut sc);
        let r = p.parse_program();
        assert!(
            r.diagnostics.is_empty(),
            "parse errors: {:?}",
            r.diagnostics
        );

        let mut saw_tuple = false;
        let mut saw_typed_discard = false;
        let mut count = 0;
        for node in &r.ast.body {
            if let Node::Statement(s) = node {
                let sp = s.span();
                assert!(
                    sp.end_line > sp.start_line
                        || (sp.end_line == sp.start_line && sp.end_column >= sp.start_column),
                    "degenerate span for {s:?}: {sp:?}"
                );
                count += 1;
                match s.as_ref() {
                    Statement::TupleDestructuringBinding(_) => saw_tuple = true,
                    Statement::TypedDiscard(_) => saw_typed_discard = true,
                    _ => {}
                }
            }
        }
        assert!(count >= 6, "expected every statement kind, saw {count}");
        assert!(saw_tuple, "no tuple-destructuring statement parsed");
        assert!(saw_typed_discard, "no typed-discard statement parsed");
    }

    // `CallArg::span` delegates to the inner expression for positional/spread
    // args and stretches from the label start to the value end for labeled args.
    // Each branch also drives `Expression::span` on the `ErrorNode` payload.
    #[test]
    fn call_arg_and_error_node_spans() {
        let err = |s| {
            Expression::ErrorNode(ErrorNode {
                message: "e".to_string(),
                span: s,
            })
        };

        // Expression::span on an ErrorNode is its own span.
        let enode = span(0, 0, 0, 3);
        assert_eq!(err(enode).span(), enode);

        // Positional / Spread args delegate to the inner expression.
        assert_eq!(
            CallArg::Positional(err(span(1, 0, 1, 4))).span(),
            span(1, 0, 1, 4)
        );
        assert_eq!(
            CallArg::Spread(err(span(2, 1, 2, 7))).span(),
            span(2, 1, 2, 7)
        );

        // Labeled spans from the label's start to the value's end.
        let labeled = CallArg::Labeled {
            label: Identifier {
                name: "k".to_string(),
                span: span(3, 2, 3, 3),
            },
            value: err(span(3, 5, 3, 11)),
        };
        assert_eq!(labeled.span(), span(3, 2, 3, 11));
    }
}
