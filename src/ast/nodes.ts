export interface Identifier {
  type: "Identifier";
  name: string;
}

export interface TypeIdentifier {
  type: "TypeIdentifier";
  identifier: Identifier;
  isArray: boolean;
  isOption: boolean;
}

export interface StringLiteral {
  type: "StringLiteral";
  value: string;
}

export interface NumberLiteral {
  type: "NumberLiteral";
  value: string;
}

export interface BooleanLiteral {
  type: "BooleanLiteral";
  value: boolean;
}

export interface NoneExpression {
  type: "NoneExpression";
}

export interface BinaryExpression {
  type: "BinaryExpression";
  left: Expression;
  right: Expression;
  operator: string;
}

export interface UnaryExpression {
  type: "UnaryExpression";
  operator: string;
  expression: Expression;
}

export interface FunctionParameter {
  type: "FunctionParameter";
  identifier: Identifier;
  typeAnnotation?: TypeIdentifier;
}

export interface FunctionStatement {
  type: "FunctionStatement";
  identifier: Identifier;
  params: FunctionParameter[];
  body: Statement[];
  returnType?: TypeIdentifier;
  throwType?: TypeIdentifier;
}

export interface ReturnStatement {
  type: "ReturnStatement";
  expression?: Expression;
}

export interface ConstStatement {
  type: "ConstStatement";
  identifier: Identifier;
  init: Expression;
}

export interface StructField {
  type: "StructField";
  identifier: Identifier;
  typeAnnotation: TypeIdentifier;
  init?: Expression;
}

export interface StructDeclaration {
  type: "StructDeclaration";
  identifier: Identifier;
  fields: StructField[];
}

export interface StructInitialization {
  type: "StructInitialization";
  identifier: Identifier;
  fields: { identifier: Identifier; init: Expression }[];
}

export interface ImportSpecifier {
  type: "ImportSpecifier";
  identifier: Identifier;
}

export interface ImportDeclaration {
  type: "ImportDeclaration";
  path: string;
  specifiers: ImportSpecifier[];
}

export interface ExportStatement {
  type: "ExportStatement";
  declaration: Statement;
}

export interface BlockExpression {
  type: "BlockExpression";
  body: Statement[];
}

export interface IfStatement {
  type: "IfStatement";
  condition: Expression;
  body: Statement[];
  elseBody?: Statement[];
}

export interface ForStatement {
  type: "ForStatement";
  body: Statement[];
}

export interface ForInStatement {
  type: "ForInStatement";
  identifier: Identifier;
  expression: Expression;
  body: Statement[];
}

export interface ThrowStatement {
  type: "ThrowStatement";
  expression: Expression;
}

export interface AssertStatement {
  type: "AssertStatement";
  expression: Expression;
  message: Expression;
}

export interface FunctionCall {
  type: "FunctionCall";
  identifier: Expression;
  arguments: Expression[];
}

export interface PropertyAccess {
  type: "PropertyAccess";
  left: Expression;
  right: Identifier;
}

export interface ArrayExpression {
  type: "ArrayExpression";
  elements: Expression[];
}

export interface ArrayIndexExpression {
  type: "ArrayIndexExpression";
  identifier: Expression;
  index: Expression;
}

export interface RangeExpression {
  type: "RangeExpression";
  start: Expression;
  end: Expression;
}

export interface DeclarationStatement {
  type: "DeclarationStatement";
  identifier: Identifier;
  init: Expression;
}

export interface EnumVariant {
  type: "EnumVariant";
  name: Identifier;
  payload?: TypeIdentifier;
}

export interface EnumDeclaration {
  type: "EnumDeclaration";
  identifier: Identifier;
  variants: EnumVariant[];
}

export interface MatchPattern {
  type: "MatchPattern";
  enumPath: (Identifier | PropertyAccess)[];
  binding?: Identifier;
}

export interface MatchCase {
  type: "MatchCase";
  pattern: MatchPattern;
  body: Expression;
}

export interface MatchExpression {
  type: "MatchExpression";
  expression: Expression;
  cases: MatchCase[];
}

export interface BreakStatement {
  type: "BreakStatement";
}

export interface ContinueStatement {
  type: "ContinueStatement";
}

export interface OrExpression {
  type: "OrExpression";
  expression: Expression;
  errorBinding?: Identifier;
  handler: BlockExpression;
}

export type Expression =
  | StringLiteral
  | NumberLiteral
  | BooleanLiteral
  | NoneExpression
  | Identifier
  | BinaryExpression
  | UnaryExpression
  | BlockExpression
  | FunctionCall
  | PropertyAccess
  | StructInitialization
  | ArrayExpression
  | ArrayIndexExpression
  | RangeExpression
  | TypeIdentifier
  | MatchExpression
  | OrExpression;

export type Statement =
  | FunctionStatement
  | ReturnStatement
  | ConstStatement
  | StructDeclaration
  | ImportDeclaration
  | ExportStatement
  | IfStatement
  | ForStatement
  | ForInStatement
  | ThrowStatement
  | AssertStatement
  | DeclarationStatement
  | BreakStatement
  | ContinueStatement
  | EnumDeclaration
  | Expression;
