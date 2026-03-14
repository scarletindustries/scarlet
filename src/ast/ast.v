module ast

import token
import span { Span }

// ============================================================================
// Literals and Basic Nodes
// ============================================================================

pub struct StringLiteral {
pub:
	value string
	span  Span @[required]
}

pub struct InterpolatedString {
pub:
	parts []Expression
	span  Span @[required]
}

pub struct NumberLiteral {
pub:
	value string @[required]
	span  Span   @[required]
}

pub struct BooleanLiteral {
pub:
	value bool
	span  Span @[required]
}

pub struct NoneExpression {
pub:
	span Span @[required]
}

pub struct ErrorNode {
pub:
	message string
	span    Span @[required]
}

pub struct Identifier {
pub:
	name string @[required]
	span Span   @[required]
}

pub type TypeKind = NamedType | ArrayType | FunctionType | OptionType

pub struct NamedType {
pub:
	identifier Identifier @[required]
	type_args  []TypeIdentifier
}

pub struct ArrayType {
pub:
	element &TypeIdentifier @[required]
}

pub struct FunctionType {
pub:
	params      []TypeIdentifier
	return_type ?&TypeIdentifier
	error_type  ?&TypeIdentifier
}

pub struct OptionType {
pub:
	inner &TypeIdentifier @[required]
}

pub struct TypeIdentifier {
pub:
	kind TypeKind @[required]
	span Span     @[required]
}

pub struct Operator {
pub:
	kind token.Kind @[required]
}

// ============================================================================
// Statements (do not produce values)
// ============================================================================

pub struct VariableBinding {
pub:
	doc        ?string
	identifier Identifier @[required]
	typ        ?TypeIdentifier
	init       Expression @[required]
	span       Span       @[required]
}

pub struct ConstBinding {
pub:
	doc        ?string
	identifier Identifier @[required]
	typ        ?TypeIdentifier
	init       Expression @[required]
	span       Span       @[required]
}

pub struct TypePatternBinding {
pub:
	typ  TypeIdentifier @[required]
	init Expression     @[required]
	span Span           @[required]
}

pub struct TupleDestructuringBinding {
pub:
	patterns []Expression
	init     Expression @[required]
	span     Span       @[required]
}

pub struct FunctionParameter {
pub:
	identifier Identifier @[required]
	typ        ?TypeIdentifier
}

pub struct FunctionDeclaration {
pub:
	doc         ?string
	identifier  Identifier @[required]
	return_type ?TypeIdentifier
	error_type  ?TypeIdentifier
	params      []FunctionParameter
	body        Expression @[required]
	span        Span       @[required]
}

pub struct StructField {
pub:
	doc        ?string
	identifier Identifier     @[required]
	typ        TypeIdentifier @[required]
	init       ?Expression
}

pub struct StructDeclaration {
pub:
	doc         ?string
	identifier  Identifier @[required]
	type_params []Identifier
	fields      []StructField
	span        Span @[required]
}

pub struct EnumVariant {
pub:
	doc        ?string
	identifier Identifier @[required]
	payload    []TypeIdentifier
}

pub struct EnumDeclaration {
pub:
	doc         ?string
	identifier  Identifier @[required]
	type_params []Identifier
	variants    []EnumVariant
	span        Span @[required]
}

pub struct ImportSpecifier {
pub:
	identifier Identifier @[required]
}

pub struct ImportDeclaration {
pub:
	path       string @[required]
	specifiers []ImportSpecifier
	span       Span @[required]
}

pub struct ExportDeclaration {
pub:
	declaration Statement @[required]
	span        Span      @[required]
}

pub type Statement = ConstBinding
	| EnumDeclaration
	| ExportDeclaration
	| FunctionDeclaration
	| ImportDeclaration
	| StructDeclaration
	| TupleDestructuringBinding
	| TypePatternBinding
	| VariableBinding

// ============================================================================
// Expressions (produce values)
// ============================================================================

pub struct FunctionExpression {
pub:
	return_type ?TypeIdentifier
	error_type  ?TypeIdentifier
	params      []FunctionParameter
	body        Expression @[required]
	span        Span       @[required]
}

pub struct IfExpression {
pub:
	condition Expression @[required]
	body      Expression @[required]
	span      Span       @[required]
	else_body ?Expression
}

pub struct MatchArm {
pub:
	pattern Expression @[required]
	body    Expression @[required]
}

pub struct MatchExpression {
pub:
	subject Expression @[required]
	arms    []MatchArm
	span    Span @[required]
}

pub struct OrExpression {
pub:
	expression Expression @[required]
	receiver   ?Identifier
	body       Expression @[required]
	span       Span       @[required]
}

pub struct ErrorExpression {
pub:
	expression Expression @[required]
	span       Span       @[required]
}

pub struct BinaryExpression {
pub:
	left  Expression @[required]
	right Expression @[required]
	op    Operator   @[required]
	span  Span       @[required]
}

pub struct UnaryExpression {
pub:
	expression Expression @[required]
	op         Operator   @[required]
	span       Span       @[required]
}

pub struct ArrayExpression {
pub:
	elements []ArrayElement
	span     Span @[required]
}

pub struct TupleExpression {
pub:
	elements []Expression
	span     Span @[required]
}

pub struct ArrayIndexExpression {
pub:
	expression Expression @[required]
	index      Expression @[required]
	span       Span       @[required]
}

pub struct RangeExpression {
pub:
	start Expression @[required]
	end   Expression @[required]
	span  Span       @[required]
}

pub struct StructInitExpression {
pub:
	identifier Identifier @[required]
	type_args  []TypeIdentifier
	fields     []StructInitField
	span       Span @[required]
}

pub struct StructInitField {
pub:
	identifier Identifier @[required]
	init       Expression @[required]
}

pub struct PropertyAccessExpression {
pub:
	left  Expression @[required]
	right Expression @[required]
	span  Span       @[required]
}

pub struct FunctionCallExpression {
pub:
	identifier Identifier @[required]
	arguments  []Expression
	span       Span @[required]
}

pub struct BlockExpression {
pub:
	body []Node
	span Span @[required]
}

// ============================================================================
// Patterns (used in match arms)
// ============================================================================

pub struct WildcardPattern {
pub:
	span Span @[required]
}

pub struct OrPattern {
pub:
	patterns []Expression
	span     Span @[required]
}

pub struct SpreadElement {
pub:
	expression ?Expression
	span       Span @[required]
}

pub type ArrayElement = Expression | SpreadElement

// ============================================================================
// Sum Types
// ============================================================================

pub type Expression = ArrayExpression
	| ArrayIndexExpression
	| BinaryExpression
	| BlockExpression
	| BooleanLiteral
	| ErrorExpression
	| ErrorNode
	| FunctionCallExpression
	| FunctionExpression
	| Identifier
	| IfExpression
	| InterpolatedString
	| MatchExpression
	| NoneExpression
	| NumberLiteral
	| OrExpression
	| OrPattern
	| PropertyAccessExpression
	| RangeExpression
	| StringLiteral
	| StructInitExpression
	| TupleExpression
	| TypeIdentifier
	| UnaryExpression
	| WildcardPattern

pub type Node = Statement | Expression

pub fn node_span(node Node) Span {
	return match node {
		Statement { node.span }
		Expression { node.span }
	}
}
