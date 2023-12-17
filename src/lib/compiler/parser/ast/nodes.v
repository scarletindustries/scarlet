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

pub struct BooleanLiteral {
	BasicASTNode
	value bool
}

pub struct IntLiteral {
	BasicASTNode
	value int
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

pub type Expression = BinaryExpression | BooleanLiteral | IntLiteral | StringLiteral

pub struct ReturnStatement {
	BasicASTNode
	value ?Expression
}

pub struct IfStatement {
	BasicASTNode
	condition Expression
	body      []ASTNode
}

pub struct ForStatement {
	BasicASTNode
	body []ASTNode
}

pub struct ConstStatement {
	BasicASTNode
pub mut:
	ident string
	init  Expression
}

pub struct ImportSpecifier {
	BasicASTNode
	ident Identifier
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
	ident       string
	declaration Statement
}

pub type Statement = ConstStatement
	| ExportStatement
	| ForStatement
	| IfStatement
	| ImportDeclaration
	| ReturnStatement

pub type ASTNode = Expression | Statement
