module ast

pub struct BasicASTNode {
pub:
	start int
	end   int
}

pub struct Program {
	BasicASTNode
	body []ASTNode
}

pub struct VariableDeclaration {
	BasicASTNode
	name string
	init Expression
}

pub struct FunctionDeclaration {
	BasicASTNode
	name string
	args []FunctionArgument
	body []ASTNode
}

pub struct StructDeclaration {
	BasicASTNode
	name string
	body []StructMemberDeclaration
}

pub struct StructMemberDeclaration {
	BasicASTNode
	name    string
	typ     ?Identifier
	default ?Expression
}

pub type Declaration = FunctionDeclaration | VariableDeclaration

pub struct FunctionArgument {
	BasicASTNode
	name string
	typ  ?Identifier
}

pub struct StringLiteral {
	BasicASTNode
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

pub type Expression = BinaryExpression | Literal

pub type ASTNode = Declaration | Expression | Identifier | Program
