module typed_ast

import token
import type_def { Type }
import span { Span }

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
	value string
	span  Span @[required]
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
	name string
	span Span @[required]
}

pub struct TypeIdentifier {
pub:
	is_array     bool
	is_option    bool
	is_function  bool
	identifier   Identifier
	element_type ?&TypeIdentifier // for array types: []T has element_type = T
	param_types  []TypeIdentifier
	return_type  ?&TypeIdentifier
	error_type   ?&TypeIdentifier
	span         Span @[required]
}

pub struct Operator {
pub:
	kind token.Kind
}

pub struct VariableBinding {
pub:
	identifier Identifier
	typ        ?TypeIdentifier
	init       Expression
	span       Span @[required]
}

pub struct ConstBinding {
pub:
	identifier Identifier
	typ        ?TypeIdentifier
	init       Expression
	span       Span @[required]
}

pub struct TypePatternBinding {
pub:
	typ  TypeIdentifier
	init Expression
	span Span @[required]
}

pub struct FunctionParameter {
pub:
	identifier Identifier
	typ        ?TypeIdentifier
}

pub struct FunctionDeclaration {
pub:
	identifier  Identifier
	return_type ?TypeIdentifier
	error_type  ?TypeIdentifier
	params      []FunctionParameter
	body        Expression
	span        Span @[required]
}

pub struct FunctionExpression {
pub:
	return_type ?TypeIdentifier
	error_type  ?TypeIdentifier
	params      []FunctionParameter
	body        Expression
	span        Span @[required]
}

pub struct IfExpression {
pub:
	condition Expression
	body      Expression
	span      Span @[required]
	else_body ?Expression
}

pub struct MatchArm {
pub:
	pattern Expression
	body    Expression
}

pub struct WildcardPattern {
pub:
	span Span @[required]
}

pub struct OrPattern {
pub:
	patterns []Expression
	span     Span @[required]
}

pub struct MatchExpression {
pub:
	subject Expression
	arms    []MatchArm
	span    Span @[required]
}

pub struct OrExpression {
pub:
	expression    Expression
	receiver      ?Identifier
	body          Expression
	resolved_type Type
	span          Span @[required]
}

pub struct ErrorExpression {
pub:
	expression Expression
	span       Span @[required]
}

pub struct PropagateNoneExpression {
pub:
	expression    Expression
	resolved_type Type
	span          Span @[required]
}

pub struct BinaryExpression {
pub:
	left  Expression
	right Expression
	op    Operator
	span  Span @[required]
}

pub struct UnaryExpression {
pub:
	expression Expression
	op         Operator
	span       Span @[required]
}

pub struct ArrayExpression {
pub:
	elements []Expression
	span     Span @[required]
}

pub struct ArrayIndexExpression {
pub:
	expression Expression
	index      Expression
	span       Span @[required]
}

pub struct RangeExpression {
pub:
	start Expression
	end   Expression
	span  Span @[required]
}

pub struct StructField {
pub:
	identifier Identifier
	typ        TypeIdentifier
	init       ?Expression
}

pub struct StructExpression {
pub:
	identifier Identifier
	fields     []StructField
	span       Span @[required]
}

pub struct EnumVariant {
pub:
	identifier Identifier
	payload    []TypeIdentifier
}

pub struct EnumExpression {
pub:
	identifier Identifier
	variants   []EnumVariant
	span       Span @[required]
}

pub struct StructInitField {
pub:
	identifier Identifier
	init       Expression
}

pub struct StructInitExpression {
pub:
	identifier Identifier
	fields     []StructInitField
	span       Span @[required]
}

pub struct PropertyAccessExpression {
pub:
	left  Expression
	right Expression
	span  Span @[required]
}

pub struct FunctionCallExpression {
pub:
	identifier Identifier
	arguments  []Expression
	span       Span @[required]
}

pub struct BlockExpression {
pub:
	body []Expression
	span Span @[required]
}

pub struct AssertExpression {
pub:
	expression Expression
	message    Expression
	span       Span @[required]
}

pub struct ImportSpecifier {
pub:
	identifier Identifier
}

pub struct ImportDeclaration {
pub:
	path       string
	specifiers []ImportSpecifier
	span       Span @[required]
}

pub struct ExportExpression {
pub:
	expression Expression
	span       Span @[required]
}

pub struct SpreadExpression {
pub:
	expression ?Expression
	span       Span @[required]
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
	| FunctionDeclaration
	| FunctionExpression
	| Identifier
	| IfExpression
	| ImportDeclaration
	| InterpolatedString
	| MatchExpression
	| NoneExpression
	| NumberLiteral
	| OrExpression
	| OrPattern
	| PropertyAccessExpression
	| PropagateNoneExpression
	| RangeExpression
	| SpreadExpression
	| StringLiteral
	| StructExpression
	| StructInitExpression
	| TypeIdentifier
	| TypePatternBinding
	| UnaryExpression
	| VariableBinding
	| WildcardPattern
