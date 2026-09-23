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
    pub(crate) span: Span,
}

/// A number as written, `_` separators included: `value` is what the
/// formatter prints back; [`NumberLiteral::digits`] is what everything that
/// interprets the number reads. Hex (`0x`/`0X`) and binary (`0b`/`0B`)
/// prefixes stay in both — [`NumberLiteral::as_int`] is the i64 parse.
#[derive(Debug, Clone)]
pub struct NumberLiteral {
    pub value: String,
    pub span: Span,
}

impl NumberLiteral {
    pub fn digits(&self) -> std::borrow::Cow<'_, str> {
        if self.value.contains('_') {
            std::borrow::Cow::Owned(self.value.replace('_', ""))
        } else {
            std::borrow::Cow::Borrowed(&self.value)
        }
    }

    /// Signed i64 value of this literal. Hex and binary are magnitudes in
    /// that radix, not two's-complement bit patterns: `0xFF` is 255, and
    /// `0x8000000000000000` is out of range (same as decimal
    /// `9223372036854775808`). A leading `-` is accepted so a negative
    /// pattern literal parses — including `-9223372036854775808` /
    /// `-0x8000000000000000`, which are i64::MIN. `None` is overflow or
    /// a non-integer spelling.
    pub fn as_int(&self) -> Option<i64> {
        parse_int_literal(&self.digits())
    }
}

/// `digits()` of an integer literal: optional leading `-`, then decimal, or
/// `0x`/`0X` + hex, or `0b`/`0B` + binary. Underscores are already gone.
fn parse_int_literal(s: &str) -> Option<i64> {
    let (sign, rest) = match s.strip_prefix('-') {
        Some(r) => ("-", r),
        None => ("", s),
    };
    if let Some(digits) = rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X")) {
        return int_from_radix(sign, digits, 16);
    }
    if let Some(digits) = rest.strip_prefix("0b").or_else(|| rest.strip_prefix("0B")) {
        return int_from_radix(sign, digits, 2);
    }
    s.parse().ok()
}

fn int_from_radix(sign: &str, digits: &str, radix: u32) -> Option<i64> {
    if digits.is_empty() {
        return None;
    }
    // `from_str_radix` accepts a leading sign, so the i64::MIN hex/bin
    // spellings parse the same way decimal `-9223372036854775808` does.
    let mut signed = String::with_capacity(sign.len() + digits.len());
    signed.push_str(sign);
    signed.push_str(digits);
    i64::from_str_radix(&signed, radix).ok()
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
    pub(crate) args: Vec<Identifier>,
    pub span: Span,
}

impl Attribute {
    /// `args` is `pub(crate)` because only the parser builds one; a consumer
    /// outside this crate (`@exhaustive`'s arity check, in `scarlet_core`)
    /// only ever needs to read it back.
    pub fn args(&self) -> &[Identifier] {
        &self.args
    }
}

#[derive(Debug, Clone)]
pub enum TypeKind {
    NamedType(NamedType),
    FunctionType(FunctionType),
    TupleType(TupleType),
}

#[derive(Debug, Clone)]
pub struct NamedType {
    /// Module qualifier of a `module.Type` reference; `None` for a bare name.
    pub qualifier: Option<Identifier>,
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
    pub(crate) fn symbol(self) -> &'static str {
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
    pub(crate) fn symbol(self) -> &'static str {
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
    pub(crate) span: Span,
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
    pub(crate) span: Span,
}

/// `Ctor(p1, ..) = expr` at statement position. Lowered to a one-arm match
/// that must pass exhaustiveness, so only single-constructor types qualify.
#[derive(Debug, Clone)]
pub struct CtorDestructuringBinding {
    /// The module the constructor is reached through: `http` in
    /// `http.Fixed(n, body) = e`.
    pub(crate) qualifier: Option<Identifier>,
    pub(crate) name: Identifier,
    pub args: Vec<PatternArg>,
    pub(crate) rest: bool,
    /// Span of just the `Ctor(args)` head, not the `= init` tail.
    pub(crate) pattern_span: Span,
    pub init: Expression,
    pub span: Span,
}

impl CtorDestructuringBinding {
    /// Re-materialise the constructor pattern for consumers of the general
    /// [`Pattern`] shape.
    pub fn as_pattern(&self) -> Pattern {
        Pattern::Constructor {
            qualifier: self.qualifier.clone(),
            name: self.name.clone(),
            args: self.args.clone(),
            rest: self.rest,
            span: self.pattern_span,
        }
    }
}

/// `a, b <- call(args)`: backpassing. Sugar for appending the rest of the
/// enclosing block to the call as a trailing `fn(a, b) { ... }` argument;
/// rewritten away by [`crate::desugar`] before type checking. `call` is
/// always a [`FunctionCallExpression`] — the parser rejects anything else.
/// A `_` binder is an [`Identifier`] named `_`, matching how a discarded
/// function parameter is spelled.
#[derive(Debug, Clone)]
pub struct BackpassBinding {
    pub binders: Vec<Identifier>,
    pub call: Expression,
    pub(crate) span: Span,
}

#[derive(Debug, Clone)]
pub struct FunctionParameter {
    pub identifier: Identifier,
    pub typ: Option<TypeIdentifier>,
}

/// A function declaration's body. Not `Option<Expression>` plus a separate
/// `@vm` predicate: the two can desync, and this makes a `@vm` fn without its
/// op, or a body-less normal fn, unrepresentable.
#[derive(Debug, Clone)]
pub enum FnBody {
    Block(Expression),
    /// A `@vm(op)` intrinsic: no Scarlet body, carries the VM op identifier.
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
    /// `type Name { Ctor ... }`. `opaque` lives here so opaque-on-alias and
    /// opaque-on-external are unrepresentable.
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
    pub(crate) span: Span,
}

#[derive(Debug, Clone)]
pub struct ConstructorField {
    pub label: Identifier,
    pub typ: TypeIdentifier,
    pub(crate) span: Span,
}

#[derive(Debug, Clone)]
pub struct ImportItem {
    pub name: Identifier,
    pub alias: Option<Identifier>,
}

/// The `.` / `..` in `import ../lib/util`. A distinct type, not a "." string
/// in the name list, so a marker and a module name can never be confused.
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

/// A module path as written in an `import`. The split makes "markers lead,
/// names follow" structural: `import a/../b` is unrepresentable.
#[derive(Debug, Clone)]
pub struct ImportPath {
    /// Leading `.` / `..` segments; empty for stdlib (`scarlet/...`) and bare paths.
    pub leading: Vec<RelSeg>,
    /// The module-name segments (`scarlet`/`string` in `import scarlet/string`).
    pub names: Vec<String>,
}

impl ImportPath {
    /// Wrap an already-canonical, non-relative path as an import.
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
    /// Span of the final module-name path segment (e.g. `string` in `import scarlet/string`).
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
    Backpass(BackpassBinding),
}

impl Statement {
    fn span(&self) -> Span {
        match self {
            Statement::Declaration { decl, .. } => decl.span(),
            Statement::ImportDeclaration(x) => x.span,
            Statement::TupleDestructuringBinding(x) => x.span,
            Statement::TypedDiscard(x) => x.span,
            Statement::CtorDestructuringBinding(x) => x.span,
            Statement::VariableBinding(x) => x.span,
            Statement::Backpass(x) => x.span,
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
    pub(crate) span: Span,
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

/// `left |> right`: sugar for calling `right` with `left` as its first
/// argument. Kept as its own node (rather than desugared at parse time) so
/// the formatter renders the pipeline as written instead of the nested calls
/// it stands for; [`crate::desugar`] rewrites it into a
/// [`FunctionCallExpression`] before type checking, the same way it rewrites
/// [`BackpassBinding`].
#[derive(Debug, Clone)]
pub struct PipeExpression {
    pub left: Box<Expression>,
    pub right: Box<Expression>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct UnaryExpression {
    pub expression: Box<Expression>,
    pub op: UnaryOp,
    pub(crate) span: Span,
}

#[derive(Debug, Clone)]
pub struct ArrayExpression {
    pub elements: Vec<ArrayElement>,
    pub(crate) span: Span,
}

#[derive(Debug, Clone)]
pub struct TupleExpression {
    pub elements: Vec<Expression>,
    pub(crate) span: Span,
}

#[derive(Debug, Clone)]
pub struct ArrayIndexExpression {
    pub expression: Box<Expression>,
    pub index: Box<Expression>,
    pub(crate) span: Span,
}

#[derive(Debug, Clone)]
pub struct RangeExpression {
    pub start: Box<Expression>,
    pub end: Box<Expression>,
    pub(crate) span: Span,
}

#[derive(Debug, Clone)]
pub enum PropertyKey {
    Field(Identifier),
    TupleIndex(NumberLiteral),
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
        /// True for punning sugar (`label:` with nothing after it), which the
        /// parser desugars to `value: Identifier(label)` on the spot. Every
        /// later pass sees an ordinary labeled argument either way; this flag
        /// exists only so the formatter can render it back the way it was
        /// written instead of guessing from `value`'s shape.
        punned: bool,
    },
    Spread(Expression),
}

impl CallArg {
    pub(crate) fn span(&self) -> Span {
        match self {
            CallArg::Positional(e) => e.span(),
            CallArg::Labeled { label, value, .. } => label.span.union(&value.span()),
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
    pub(crate) span: Span,
}

#[derive(Debug, Clone)]
pub enum ArrayElement {
    Expression(Expression),
    SpreadElement(SpreadElement),
}

/// The parsed `:spec` suffix of a `<<>>` segment. An `Int` width is always in
/// bits, a `Binary` size always in bytes, and `Utf8` never carries a size.
#[derive(Debug, Clone)]
pub enum BinSpec {
    /// `:N` / `:size(expr)`. `None` means the default width of 8, supplied
    /// downstream.
    Int { bits: Option<Expression> },
    /// `:bytes(expr)`. `None` is `:binary`, which consumes the remainder.
    Binary { bytes: Option<Expression> },
    /// `:utf8`, or the default for a bare string segment.
    Utf8,
}

impl BinSpec {
    /// The runtime size expression, if the spec carries one.
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
    pub(crate) span: Span,
}

#[derive(Debug, Clone)]
pub struct BinaryLiteral {
    pub segments: Vec<BinSegment>,
    pub(crate) span: Span,
}

#[derive(Debug, Clone)]
pub enum Pattern {
    Var {
        name: Identifier,
    },
    Constructor {
        /// `io` in `io.NotFound(path)`. `None` when the constructor is in
        /// scope; both spellings denote the same constructor.
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
    /// `p | q | ..`. `rest` is non-empty: there are always two or more.
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
    /// Walk every alternative. Scope analysis needs this: later alternatives
    /// can hold segment size expressions referencing outer names.
    All,
    /// Walk only the first. Typing enforces that all alternatives bind the
    /// identical set, so the first is canonical for codegen.
    First,
}

/// A binding-relevant site found by [`Pattern::for_each_binder`].
pub enum PatternBinder<'a> {
    /// A `Var`, an array spread binding, or a binary `..rest` binding.
    Name(&'a Identifier),
    /// Not a binder, but it can reference outer names, so scope analysis must
    /// walk it.
    SizeExpr(&'a Expression),
}

impl Pattern {
    pub fn span(&self) -> Span {
        match self {
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

    /// Walk every binding site in source order. Constructor names are not
    /// reported: they are uppercase and can never be binders.
    pub fn for_each_binder<'a>(
        &'a self,
        or_mode: OrAlternatives,
        f: &mut dyn FnMut(PatternBinder<'a>),
    ) {
        match self {
            Pattern::Literal(_) | Pattern::Range { .. } => {}
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
    PipeExpression(PipeExpression),
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
    PipeExpression, PropertyAccessExpression, RangeExpression, StringLiteral, TupleExpression,
    UnaryExpression,
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

    #[test]
    fn statement_span_covers_all_kinds() {
        let src = "import scarlet/array\n\
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

    #[test]
    fn call_arg_and_error_node_spans() {
        let err = |s| {
            Expression::ErrorNode(ErrorNode {
                message: "e".to_string(),
                span: s,
            })
        };

        let enode = span(0, 0, 0, 3);
        assert_eq!(err(enode).span(), enode);

        assert_eq!(
            CallArg::Positional(err(span(1, 0, 1, 4))).span(),
            span(1, 0, 1, 4)
        );
        assert_eq!(
            CallArg::Spread(err(span(2, 1, 2, 7))).span(),
            span(2, 1, 2, 7)
        );

        // A labeled arg spans from the label's start to the value's end.
        let labeled = CallArg::Labeled {
            label: Identifier {
                name: "k".to_string(),
                span: span(3, 2, 3, 3),
            },
            value: err(span(3, 5, 3, 11)),
            punned: false,
        };
        assert_eq!(labeled.span(), span(3, 2, 3, 11));
    }

    fn num(value: &str) -> NumberLiteral {
        NumberLiteral {
            value: value.to_string(),
            span: span(0, 0, 0, value.len() as i32),
        }
    }

    #[test]
    fn as_int_parses_hex_bin_decimal_and_rejects_overflow() {
        assert_eq!(num("0xFF").as_int(), Some(255));
        assert_eq!(num("0xff").as_int(), Some(255));
        assert_eq!(num("0X10").as_int(), Some(16));
        assert_eq!(num("0b1010").as_int(), Some(10));
        assert_eq!(num("0B11").as_int(), Some(3));
        assert_eq!(num("0xDE_AD_BE_EF").as_int(), Some(3735928559));
        assert_eq!(num("-0xFF").as_int(), Some(-255));
        assert_eq!(num("255").as_int(), Some(255));
        assert_eq!(num("0x7FFFFFFFFFFFFFFF").as_int(), Some(i64::MAX));
        assert_eq!(num("-0x8000000000000000").as_int(), Some(i64::MIN));
        assert_eq!(num("-9223372036854775808").as_int(), Some(i64::MIN));
        // Magnitudes past i64::MAX overflow in every radix; hex is not bits.
        assert_eq!(num("0x8000000000000000").as_int(), None);
        assert_eq!(num("0xFFFFFFFFFFFFFFFF").as_int(), None);
        assert_eq!(num("9223372036854775808").as_int(), None);
        assert_eq!(num("1.5").as_int(), None);
    }
}
