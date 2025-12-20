module ast

import compiler.token

// Span represents a source location (line and column)
pub struct Span {
pub:
	line   int
	column int
}

// Literals

pub struct StringLiteral {
pub mut:
	value string
	span  Span
}

// Interpolated string: 'Hello, $name!' or 'Result: ${a + b}'
// Parts alternate between string literals and expressions
pub struct InterpolatedString {
pub:
	parts []Expression // StringLiteral or other expressions
}

pub struct NumberLiteral {
pub mut:
	value string
	span  Span
}

pub struct BooleanLiteral {
pub:
	value bool
	span  Span
}

pub struct NoneExpression {}

// ErrorNode represents a malformed expression that couldn't be parsed
pub struct ErrorNode {
pub:
	message string
}

// Identifiers

pub struct Identifier {
pub mut:
	name string
	span Span
}

pub struct TypeIdentifier {
pub:
	is_array    bool
	is_option   bool
	is_function bool
pub mut:
	identifier  Identifier
	param_types []TypeIdentifier // for function types: fn(Int, String) -> these
	return_type ?&TypeIdentifier // for function types: fn(...) Int
	error_type  ?&TypeIdentifier // for function types: fn(...) Int!Error
}

// Operators

pub struct Operator {
pub mut:
	kind token.Kind
}

// Variable binding (x = expr)

pub struct VariableBinding {
pub mut:
	identifier Identifier
	typ        ?TypeIdentifier
	init       Expression
	span       Span
}

pub struct ConstBinding {
pub mut:
	identifier Identifier
	typ        ?TypeIdentifier
	init       Expression
	span       Span
}

// Functions

pub struct FunctionParameter {
pub mut:
	identifier Identifier
	typ        ?TypeIdentifier
}

pub struct FunctionExpression {
pub mut:
	identifier  ?Identifier
	return_type ?TypeIdentifier
	error_type  ?TypeIdentifier
	params      []FunctionParameter
	body        Expression
}

// Control flow

pub struct IfExpression {
pub:
	condition Expression
	body      Expression
	span      Span
pub mut:
	else_body ?Expression
}

pub struct MatchArm {
pub:
	pattern Expression
	body    Expression
}

pub struct WildcardPattern {}

pub struct MatchExpression {
pub:
	subject Expression
	arms    []MatchArm
}

pub struct OrExpression {
pub mut:
	expression Expression
	receiver   ?Identifier
	body       Expression
}

// Error handling

pub struct ErrorExpression {
pub:
	expression Expression
}

pub struct PropagateExpression {
pub:
	expression Expression
}

// Binary and unary operations

pub struct BinaryExpression {
pub mut:
	left  Expression
	right Expression
	op    Operator
	span  Span
}

pub struct UnaryExpression {
pub mut:
	expression Expression
	op         Operator
}

pub struct PostfixExpression {
pub mut:
	expression Expression
	op         Operator
}

// Data structures

pub struct ArrayExpression {
pub:
	elements []Expression
	span     Span
}

pub struct ArrayIndexExpression {
pub:
	expression Expression
	index      Expression
	span       Span
}

pub struct RangeExpression {
pub:
	start Expression
	end   Expression
}

// Struct definition

pub struct StructField {
pub mut:
	identifier Identifier
	typ        TypeIdentifier
	init       ?Expression
}

pub struct StructExpression {
pub mut:
	identifier Identifier
	fields     []StructField
}

// Enum definition

pub struct EnumVariant {
pub mut:
	identifier Identifier
	payload    ?TypeIdentifier
}

pub struct EnumExpression {
pub mut:
	identifier Identifier
	variants   []EnumVariant
}

// Struct instantiation

pub struct StructInitField {
pub mut:
	identifier Identifier
	init       Expression
}

pub struct StructInitExpression {
pub mut:
	identifier Identifier
	fields     []StructInitField
}

// Property and method access

pub struct PropertyAccessExpression {
pub:
	left  Expression
	right Expression
}

pub struct FunctionCallExpression {
pub:
	identifier Identifier
	arguments  []Expression
	span       Span
}

// Block (list of expressions, returns last)

pub struct BlockExpression {
pub mut:
	body []Expression
}

// Assert

pub struct AssertExpression {
pub:
	expression Expression
	message    Expression
}

// Imports and exports (top-level only)

pub struct ImportSpecifier {
pub:
	identifier Identifier
}

pub struct ImportDeclaration {
pub mut:
	path       string
	specifiers []ImportSpecifier
}

pub struct ExportExpression {
pub mut:
	expression Expression
}

pub type Expression = ArrayExpression
	| ArrayIndexExpression
	| AssertExpression
	| BinaryExpression
	| BlockExpression
	| BooleanLiteral
	| ConstBinding
	| EnumExpression
	| ErrorExpression
	| ErrorNode
	| ExportExpression
	| FunctionCallExpression
	| FunctionExpression
	| Identifier
	| IfExpression
	| ImportDeclaration
	| InterpolatedString
	| MatchExpression
	| NoneExpression
	| NumberLiteral
	| OrExpression
	| PostfixExpression
	| PropertyAccessExpression
	| PropagateExpression
	| RangeExpression
	| StringLiteral
	| StructExpression
	| StructInitExpression
	| TypeIdentifier
	| UnaryExpression
	| VariableBinding
	| WildcardPattern
