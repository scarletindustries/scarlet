module ast

import compiler.token
import compiler.type_def { Type }

pub struct Span {
pub:
	line   int
	column int
}

pub struct StringLiteral {
pub mut:
	value string
	span  Span
}

pub struct InterpolatedString {
pub:
	parts []Expression
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

pub struct ErrorNode {
pub:
	message string
}

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
	param_types []TypeIdentifier
	return_type ?&TypeIdentifier
	error_type  ?&TypeIdentifier
}

pub struct Operator {
pub mut:
	kind token.Kind
}

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

pub struct IfExpression {
pub mut:
	condition Expression
	body      Expression
	span      Span
	else_body ?Expression
}

pub struct MatchArm {
pub mut:
	pattern Expression
	body    Expression
}

pub struct WildcardPattern {}

pub struct MatchExpression {
pub mut:
	subject Expression
	arms    []MatchArm
}

pub struct OrExpression {
pub mut:
	expression    Expression
	receiver      ?Identifier
	body          Expression
	resolved_type ?Type
}

pub struct ErrorExpression {
pub mut:
	expression Expression
}

pub struct PropagateExpression {
pub mut:
	expression    Expression
	resolved_type ?Type
}

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

pub struct ArrayExpression {
pub mut:
	elements []Expression
	span     Span
}

pub struct ArrayIndexExpression {
pub mut:
	expression Expression
	index      Expression
	span       Span
}

pub struct RangeExpression {
pub mut:
	start Expression
	end   Expression
}

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

pub struct PropertyAccessExpression {
pub mut:
	left  Expression
	right Expression
}

pub struct FunctionCallExpression {
pub mut:
	identifier Identifier
	arguments  []Expression
	span       Span
}

pub struct BlockExpression {
pub mut:
	body []Expression
}

pub struct AssertExpression {
pub mut:
	expression Expression
	message    Expression
}

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
