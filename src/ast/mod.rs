use crate::span::Span;
use crate::token;

// ============================================================================
// Literals and Basic Nodes
// ============================================================================

#[derive(Debug, Clone)]
pub struct StringLiteral {
    pub value: String,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct InterpolatedString {
    pub parts: Vec<Expression>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct NumberLiteral {
    pub value: String,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct BooleanLiteral {
    pub value: bool,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct NoneExpression {
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

#[derive(Debug, Clone)]
pub enum TypeKind {
    NamedType(NamedType),
    ArrayType(ArrayType),
    FunctionType(FunctionType),
    OptionType(OptionType),
}

#[derive(Debug, Clone)]
pub struct NamedType {
    pub identifier: Identifier,
    pub type_args: Vec<TypeIdentifier>,
}

#[derive(Debug, Clone)]
pub struct ArrayType {
    pub element: Box<TypeIdentifier>,
}

#[derive(Debug, Clone)]
pub struct FunctionType {
    pub params: Vec<TypeIdentifier>,
    pub return_type: Option<Box<TypeIdentifier>>,
    pub error_type: Option<Box<TypeIdentifier>>,
}

#[derive(Debug, Clone)]
pub struct OptionType {
    pub inner: Box<TypeIdentifier>,
}

#[derive(Debug, Clone)]
pub struct TypeIdentifier {
    pub kind: TypeKind,
    pub span: Span,
}

#[derive(Debug, Clone, Copy)]
pub struct Operator {
    pub kind: token::Kind,
}

// ============================================================================
// Statements (do not produce values)
// ============================================================================

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
pub struct TypePatternBinding {
    pub typ: TypeIdentifier,
    pub init: Expression,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct TupleDestructuringBinding {
    pub patterns: Vec<Expression>,
    pub init: Expression,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct FunctionParameter {
    pub identifier: Identifier,
    pub typ: Option<TypeIdentifier>,
}

#[derive(Debug, Clone)]
pub struct FunctionDeclaration {
    pub doc: Option<String>,
    pub identifier: Identifier,
    pub return_type: Option<TypeIdentifier>,
    pub error_type: Option<TypeIdentifier>,
    pub params: Vec<FunctionParameter>,
    pub body: Expression,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct StructField {
    pub doc: Option<String>,
    pub identifier: Identifier,
    pub typ: TypeIdentifier,
    pub init: Option<Expression>,
}

#[derive(Debug, Clone)]
pub struct StructDeclaration {
    pub doc: Option<String>,
    pub identifier: Identifier,
    pub type_params: Vec<Identifier>,
    pub fields: Vec<StructField>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct EnumVariant {
    pub doc: Option<String>,
    pub identifier: Identifier,
    pub payload: Vec<TypeIdentifier>,
}

#[derive(Debug, Clone)]
pub struct EnumDeclaration {
    pub doc: Option<String>,
    pub identifier: Identifier,
    pub type_params: Vec<Identifier>,
    pub variants: Vec<EnumVariant>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ImportSpecifier {
    pub identifier: Identifier,
}

#[derive(Debug, Clone)]
pub struct ImportDeclaration {
    pub path: String,
    pub specifiers: Vec<ImportSpecifier>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ExportDeclaration {
    pub declaration: Box<Statement>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum Statement {
    ConstBinding(ConstBinding),
    EnumDeclaration(EnumDeclaration),
    ExportDeclaration(ExportDeclaration),
    FunctionDeclaration(FunctionDeclaration),
    ImportDeclaration(ImportDeclaration),
    StructDeclaration(StructDeclaration),
    TupleDestructuringBinding(TupleDestructuringBinding),
    TypePatternBinding(TypePatternBinding),
    VariableBinding(VariableBinding),
}

impl Statement {
    pub fn span(&self) -> Span {
        match self {
            Statement::ConstBinding(x) => x.span,
            Statement::EnumDeclaration(x) => x.span,
            Statement::ExportDeclaration(x) => x.span,
            Statement::FunctionDeclaration(x) => x.span,
            Statement::ImportDeclaration(x) => x.span,
            Statement::StructDeclaration(x) => x.span,
            Statement::TupleDestructuringBinding(x) => x.span,
            Statement::TypePatternBinding(x) => x.span,
            Statement::VariableBinding(x) => x.span,
        }
    }
}

// ============================================================================
// Expressions (produce values)
// ============================================================================

#[derive(Debug, Clone)]
pub struct FunctionExpression {
    pub return_type: Option<TypeIdentifier>,
    pub error_type: Option<TypeIdentifier>,
    pub params: Vec<FunctionParameter>,
    pub body: Box<Expression>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct IfExpression {
    pub condition: Box<Expression>,
    pub body: Box<Expression>,
    pub span: Span,
    pub else_body: Option<Box<Expression>>,
}

#[derive(Debug, Clone)]
pub struct MatchArm {
    pub pattern: Expression,
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
pub struct ErrorExpression {
    pub expression: Box<Expression>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct BinaryExpression {
    pub left: Box<Expression>,
    pub right: Box<Expression>,
    pub op: Operator,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct UnaryExpression {
    pub expression: Box<Expression>,
    pub op: Operator,
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
pub struct StructInitExpression {
    pub identifier: Identifier,
    pub type_args: Vec<TypeIdentifier>,
    pub fields: Vec<StructInitField>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct StructInitField {
    pub identifier: Identifier,
    pub init: Expression,
}

#[derive(Debug, Clone)]
pub struct PropertyAccessExpression {
    pub left: Box<Expression>,
    pub right: Box<Expression>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct FunctionCallExpression {
    pub identifier: Identifier,
    pub arguments: Vec<Expression>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct BlockExpression {
    pub body: Vec<Node>,
    pub span: Span,
}

// ============================================================================
// Patterns (used in match arms)
// ============================================================================

#[derive(Debug, Clone)]
pub struct WildcardPattern {
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct OrPattern {
    pub patterns: Vec<Expression>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct SpreadElement {
    pub expression: Option<Expression>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum ArrayElement {
    Expression(Expression),
    SpreadElement(SpreadElement),
}

// ============================================================================
// Sum Types
// ============================================================================

#[derive(Debug, Clone)]
pub enum Expression {
    ArrayExpression(ArrayExpression),
    ArrayIndexExpression(ArrayIndexExpression),
    BinaryExpression(BinaryExpression),
    BlockExpression(BlockExpression),
    BooleanLiteral(BooleanLiteral),
    ErrorExpression(ErrorExpression),
    ErrorNode(ErrorNode),
    FunctionCallExpression(FunctionCallExpression),
    FunctionExpression(FunctionExpression),
    Identifier(Identifier),
    IfExpression(IfExpression),
    InterpolatedString(InterpolatedString),
    MatchExpression(MatchExpression),
    NoneExpression(NoneExpression),
    NumberLiteral(NumberLiteral),
    OrExpression(OrExpression),
    OrPattern(OrPattern),
    PropertyAccessExpression(PropertyAccessExpression),
    RangeExpression(RangeExpression),
    StringLiteral(StringLiteral),
    StructInitExpression(StructInitExpression),
    TupleExpression(TupleExpression),
    TypeIdentifier(TypeIdentifier),
    UnaryExpression(UnaryExpression),
    WildcardPattern(WildcardPattern),
}

impl Expression {
    pub fn span(&self) -> Span {
        match self {
            Expression::ArrayExpression(x) => x.span,
            Expression::ArrayIndexExpression(x) => x.span,
            Expression::BinaryExpression(x) => x.span,
            Expression::BlockExpression(x) => x.span,
            Expression::BooleanLiteral(x) => x.span,
            Expression::ErrorExpression(x) => x.span,
            Expression::ErrorNode(x) => x.span,
            Expression::FunctionCallExpression(x) => x.span,
            Expression::FunctionExpression(x) => x.span,
            Expression::Identifier(x) => x.span,
            Expression::IfExpression(x) => x.span,
            Expression::InterpolatedString(x) => x.span,
            Expression::MatchExpression(x) => x.span,
            Expression::NoneExpression(x) => x.span,
            Expression::NumberLiteral(x) => x.span,
            Expression::OrExpression(x) => x.span,
            Expression::OrPattern(x) => x.span,
            Expression::PropertyAccessExpression(x) => x.span,
            Expression::RangeExpression(x) => x.span,
            Expression::StringLiteral(x) => x.span,
            Expression::StructInitExpression(x) => x.span,
            Expression::TupleExpression(x) => x.span,
            Expression::TypeIdentifier(x) => x.span,
            Expression::UnaryExpression(x) => x.span,
            Expression::WildcardPattern(x) => x.span,
        }
    }
}

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

pub fn node_span(node: &Node) -> Span {
    node.span()
}
