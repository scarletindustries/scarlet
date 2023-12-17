module ast

pub struct BasicASTNode {
}

pub struct Program {
	BasicASTNode
pub mut:
	body []ASTNode
}

pub struct StringLiteral {
	BasicASTNode
pub mut:
	value string
}

pub struct NumberLiteral {
	BasicASTNode
pub mut:
	value string
}

pub struct Identifier {
	BasicASTNode
	name string
}

pub struct BinaryExpression {
	BasicASTNode
	left  Expression
	right Expression
	op    string // + - * / % < > <= >= == != etc
}

pub struct ConstStatement {
	BasicASTNode
pub mut:
	identifier Identifier
	init       Expression
}

pub struct ImportSpecifier {
	BasicASTNode
	identifier Identifier
}

pub struct ImportDeclaration {
	BasicASTNode
pub mut:
	path       string
	specifiers []ImportSpecifier
}

pub struct ExportStatement {
	BasicASTNode
pub mut:
	identifier  Identifier
	declaration Statement
}

pub struct StructStatement {
pub mut:
	identifier Identifier
	fields     []StructField
}

pub struct StructField {
	BasicASTNode
pub mut:
	identifier Identifier
	typ        Identifier
	init	   ?Expression
}

pub type Expression = StringLiteral | NumberLiteral

pub type Statement = ConstStatement
	| ExportStatement
	| ImportDeclaration
	| StructField
	| StructStatement

pub type ASTNode = Expression | Statement
