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
	identifier  Identifier
	return_type ?Identifier
	is_return_option bool
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
	body 	  []Statement
pub mut:
	else_statement ?Statement
}

pub struct OrStatement {
pub mut:
	body 	 []Statement
	receiver ?Identifier
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

pub struct ForInStatement {
	body []Statement
	identifier Identifier
	expression Expression
}

pub struct BlockExpression {
pub:
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
	elements []Expression
}

pub type Expression = FunctionCallExpression
	| StringLiteral
	| NumberLiteral
	| BooleanLiteral
	| Identifier
	| PropertyAccessExpression
	| BinaryExpression
	| StructInitialisation
	| StructInitialisationField
	| NoneExpression
	| UnaryExpression
	| RangeExpression
	| BlockExpression
	| ArrayExpression

pub type Statement = ConstStatement
	| ExportStatement
	| Expression
	| FunctionParameter
	| FunctionStatement
	| ImportDeclaration
	| ReturnStatement
	| StructField
	| StructDeclarationStatement
	| BinaryExpression
	| ThrowStatement
	| IfStatement
	| OrStatement
	| ForStatement
	| ForInStatement
	| ContinueStatement
	| BreakStatement

pub type ASTNode = Expression | Statement
