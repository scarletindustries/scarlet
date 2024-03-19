module ast

import lib.compiler.token

pub struct StringLiteral {
pub mut:
	value string
}

pub struct NumberLiteral {
pub mut:
	value string
}

pub struct BooleanLiteral {
pub:
	value bool
}

pub struct Identifier {
pub mut:
	name string
}

pub struct Operator {
pub mut:
	kind token.Kind
}

pub struct VariableDeclaration {
pub mut:
	identifier Identifier
	init       Expression
}

pub struct StructInitialisationField {
pub mut:
	identifier Identifier
	init       Expression
}

pub struct StructInitialisation {
pub mut:
	identifier Identifier
	fields     []StructInitialisationField
}

pub struct BinaryExpression {
pub mut:
	left  Expression
	right Expression
	op    Operator // + - * / % < > <= >= == != etc
}

pub struct ConstStatement {
pub mut:
	identifier Identifier
	init       Expression
}

pub struct ThrowStatement {
pub:
	expression Expression
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

pub struct ExportStatement {
pub mut:
	declaration Statement
}

pub struct StructDeclarationStatement {
pub mut:
	identifier Identifier
	fields     []StructField
}

pub struct StructField {
pub mut:
	identifier Identifier
	typ        Identifier
	init       ?Expression
}

pub struct NoneExpression {}

pub struct FunctionStatement {
pub mut:
	identifier       Identifier
	return_type      ?Identifier
	is_return_option bool
	throw_type       ?Identifier
	params           []FunctionParameter
	body             []Statement
}

pub struct FunctionParameter {
pub mut:
	identifier Identifier
	typ        ?Identifier
}

pub struct FunctionCallExpression {
pub:
	identifier           Identifier
	arguments            []Expression
	has_exclamation_mark bool
}

pub struct PropertyAccessExpression {
pub:
	expression Expression
	identifier Identifier
}

pub struct ReturnStatement {
pub:
	expression ?Expression
}

pub struct IfStatement {
pub:
	condition Expression
	body      []Statement
pub mut:
	else_statement ?Statement
}

pub struct OrStatement {
pub mut:
	body       []Statement
	expression Expression
	receiver   ?Identifier
}

pub struct UnaryExpression {
pub mut:
	expression Expression
	op         Operator
}

pub struct ForStatement {
pub:
	body []Statement
}

pub struct PostfixExpression {
pub mut:
	expression Expression
	op         Operator
}

pub struct ForInStatement {
pub:
	body       []Statement
	identifier Identifier
	expression Expression
}

pub struct AssignmentStatement {
pub mut:
	identifier Identifier
	expression Expression
}

pub struct DeclarationStatement {
pub mut:
	identifier Identifier
	expression Expression
}

pub struct BlockExpression {
pub mut:
	body []Statement
}

pub struct ContinueStatement {}

pub struct BreakStatement {}

pub struct RangeExpression {
pub:
	start Expression
	end   Expression
}

pub struct ArrayExpression {
pub:
	elements []Expression
}

pub struct AssertStatement {
pub:
	expression Expression
	message    Expression
}

pub type Expression = ArrayExpression
	| BinaryExpression
	| BlockExpression
	| BooleanLiteral
	| FunctionCallExpression
	| Identifier
	| NoneExpression
	| NumberLiteral
	| PostfixExpression
	| PropertyAccessExpression
	| RangeExpression
	| StringLiteral
	| StructInitialisation
	| StructInitialisationField
	| UnaryExpression

pub type Statement = AssertStatement
	| AssignmentStatement
	| BreakStatement
	| ConstStatement
	| ContinueStatement
	| ExportStatement
	| Expression
	| ForInStatement
	| ForStatement
	| FunctionParameter
	| FunctionStatement
	| IfStatement
	| ImportDeclaration
	| OrStatement
	| ReturnStatement
	| StructDeclarationStatement
	| StructField
	| ThrowStatement
	| DeclarationStatement
