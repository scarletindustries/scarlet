module ast

import lib.compiler.token

pub struct Block {
pub mut:
	body []ASTNode
}

pub struct StringLiteral {
pub mut:
	value string
}

pub struct NumberLiteral {
pub mut:
	value string
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
	init      Expression
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

pub struct StructStatement {
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

pub struct FunctionStatement {
pub mut:
	identifier  Identifier
	return_type ?Identifier
	throw_type  ?Identifier
	params      []FunctionParameter
	body        []Statement
}

pub struct FunctionParameter {
pub mut:
	identifier Identifier
	typ        ?Identifier
}

pub struct FunctionCallExpression {
	identifier Identifier
	arguments  []Expression
}

pub struct PropertyAccessExpression {
pub:
	expression Expression
	identifier Identifier
}

pub struct ReturnStatement {
pub:
	expression Expression
}

pub type Expression = FunctionCallExpression
	| Identifier
	| NumberLiteral
	| PropertyAccessExpression
	| StringLiteral
	| BinaryExpression | StructInitialisation | StructInitialisationField

pub type Statement = ConstStatement
	| ExportStatement
	| Expression
	| FunctionParameter
	| FunctionStatement
	| ImportDeclaration
	| ReturnStatement
	| StructField
	| StructStatement | BinaryExpression | ThrowStatement

pub type ASTNode = Expression | Statement
