module ast

pub struct BasicASTNode {
}

pub struct Program {
	BasicASTNode
pub mut:
	body []ASTNode
}

pub struct VariableDeclaration {
	BasicASTNode
	ident string
	init Expression
}

pub struct FunctionDeclaration {
	BasicASTNode
	ident string
	args []FunctionArgument
	body []ASTNode
}

pub struct StructDeclaration {
	BasicASTNode
	ident string
	body []StructMemberDeclaration
}

pub struct StructMemberDeclaration {
	BasicASTNode
	ident    string
	typ     ?Identifier
	default ?Expression
}

pub type Declaration = FunctionDeclaration | VariableDeclaration

pub struct FunctionArgument {
	BasicASTNode
	ident string
	typ  ?Identifier
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

pub type Literal = BooleanLiteral | IntLiteral | StringLiteral

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

// implement statements

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
	body      []ASTNode
}

pub struct ConstStatement {
	BasicASTNode
pub mut:
	ident string
	init Expression
}

pub struct ImportStatement {
	BasicASTNode
pub mut:
	path string
	declarations []Identifier
}

pub type Statement = ReturnStatement | IfStatement | ForStatement | ImportStatement | ConstStatement

pub type ASTNode = Statement | Declaration | Expression | Identifier | Program

