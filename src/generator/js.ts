import type {
  ArrayExpression,
  ArrayIndexExpression,
  AssertStatement,
  BinaryExpression,
  BlockExpression,
  BooleanLiteral,
  BreakStatement,
  ConstStatement,
  ContinueStatement,
  DeclarationStatement,
  EnumDeclaration,
  ExportStatement,
  Expression,
  ForInStatement,
  ForStatement,
  FunctionCall,
  FunctionStatement,
  Identifier,
  IfStatement,
  ImportDeclaration,
  MatchExpression,
  NoneExpression,
  NumberLiteral,
  OrExpression,
  PropertyAccess,
  RangeExpression,
  ReturnStatement,
  Statement,
  StringLiteral,
  StructDeclaration,
  StructInitialization,
  ThrowStatement,
  TypeIdentifier,
  UnaryExpression,
} from "../ast/nodes";

export class JSGenerator {
  private indentLevel: number = 0;
  private readonly indent = "  ";

  private getIndent(): string {
    return this.indent.repeat(this.indentLevel);
  }

  generateRoot(statements: Statement[]): string {
    return `(() => {
  const println = console.log;

${this.generateStatements(statements)}
})();`;
  }

  private generateStatements(statements: Statement[]): string {
    return statements.map((stmt) => this.generateStatement(stmt)).join("\n");
  }

  private generateStatement(statement: Statement): string {
    switch (statement.type) {
      case "FunctionStatement":
        return this.generateFunctionStatement(statement);
      case "ReturnStatement":
        return this.generateReturnStatement(statement);
      case "ConstStatement":
        return this.generateConstStatement(statement);
      case "StructDeclaration":
        return this.generateStructDeclaration(statement);
      case "ImportDeclaration":
        return this.generateImportDeclaration(statement);
      case "ExportStatement":
        return this.generateExportStatement(statement);
      case "IfStatement":
        return this.generateIfStatement(statement);
      case "ForStatement":
        return this.generateForStatement(statement);
      case "ForInStatement":
        return this.generateForInStatement(statement);
      case "ThrowStatement":
        return this.generateThrowStatement(statement);
      case "AssertStatement":
        return this.generateAssertStatement(statement);
      case "DeclarationStatement":
        return this.generateDeclarationStatement(statement);
      case "BreakStatement":
        return this.generateBreakStatement(statement);
      case "ContinueStatement":
        return this.generateContinueStatement(statement);
      case "EnumDeclaration":
        return this.generateEnumDeclaration(statement);
      case "ExpressionStatement":
        return this.generateExpression(statement.expression) + ";";
      default:
        throw new Error("Unsupported statement type: " + statement.type);
    }
  }

  private generateFunctionStatement(statement: FunctionStatement): string {
    const { identifier, params, body, returnType, throwType } = statement;
    const jsDoc = this.generateJSDoc(returnType, throwType);
    const functionBody = this.generateStatements(body);

    return `${jsDoc}function ${identifier.name}(${params
      .map((p) => p.identifier.name)
      .join(", ")}) {
${this.indent}${functionBody}
}`;
  }

  private generateJSDoc(
    returnType?: TypeIdentifier,
    throwType?: TypeIdentifier
  ): string {
    const parts: string[] = [];

    if (returnType) {
      let type = returnType.identifier.name;
      if (returnType.isArray) {
        type = `Array<${type}>`;
      }
      if (returnType.isOption) {
        type = `${type} | null`;
      }
      parts.push(`@returns {${type}}`);
    }

    if (throwType) {
      parts.push(`@throws {${throwType.identifier.name}}`);
    }

    if (parts.length === 0) return "";
    return `/**\n * ${parts.join("\n * ")}\n */\n`;
  }

  private generateReturnStatement(statement: ReturnStatement): string {
    const { expression } = statement;
    return `${this.getIndent()}return${
      expression ? " " + this.generateExpression(expression) : ""
    };`;
  }

  private generateConstStatement(statement: ConstStatement): string {
    const { identifier, init } = statement;
    return `${this.getIndent()}const ${
      identifier.name
    } = ${this.generateExpression(init)};`;
  }

  private generateStructDeclaration(statement: StructDeclaration): string {
    const { identifier, fields } = statement;
    const fieldInits = fields
      .map((field) => {
        const defaultValue = field.init
          ? ` = ${this.generateExpression(field.init)}`
          : "";
        return `${this.indent}${field.identifier.name}${defaultValue}`;
      })
      .join("\n");

    return `class ${identifier.name} {
${fieldInits}

${this.indent}constructor(init) {
${this.indent}${this.indent}${fields
      .map((f) => `this.${f.identifier.name} = init.${f.identifier.name}`)
      .join(";\n" + this.indent.repeat(2))};
${this.indent}}
}`;
  }

  private generateImportDeclaration(statement: ImportDeclaration): string {
    const { specifiers, path } = statement;
    return `import { ${specifiers
      .map((s) => s.identifier.name)
      .join(", ")} } from '${path}';`;
  }

  private generateExportStatement(statement: ExportStatement): string {
    return `export ${this.generateStatement(statement.declaration)}`;
  }

  private generateIfStatement(statement: IfStatement): string {
    const { condition, then, else: elseBody } = statement;
    let code = `if (${this.generateExpression(condition)}) {\n`;
    code += this.generateStatements(then) + "\n}";

    if (elseBody) {
      code += " else {\n";
      code += this.generateStatements(elseBody) + "\n}";
    }

    return code;
  }

  private generateForStatement(statement: ForStatement): string {
    return `for (;;) {\n${this.generateStatements(statement.body)}\n}`;
  }

  private generateForInStatement(statement: ForInStatement): string {
    const { identifier, iterator, body } = statement;
    return `for (const ${identifier.name} of ${this.generateExpression(
      iterator
    )}) {
${this.generateStatements(body)}
}`;
  }

  private generateThrowStatement(statement: ThrowStatement): string {
    return `throw ${this.generateExpression(statement.expression)};`;
  }

  private generateAssertStatement(statement: AssertStatement): string {
    const { expression, message } = statement;
    return `if (!${this.generateExpression(expression)}) {
${this.indent}throw new Error(${this.generateExpression(message)});
}`;
  }

  private generateDeclarationStatement(
    statement: DeclarationStatement
  ): string {
    const { identifier, init } = statement;
    return `${this.getIndent()}let ${
      identifier.name
    } = ${this.generateExpression(init)};`;
  }

  private generateBreakStatement(_: BreakStatement): string {
    return "break;";
  }

  private generateContinueStatement(_: ContinueStatement): string {
    return "continue;";
  }

  private generateEnumDeclaration(statement: EnumDeclaration): string {
    const { identifier, variants } = statement;
    const enumName = identifier.name;

    // Generate variant classes
    const variantClasses = variants
      .map((variant) => {
        const className = `${enumName}_${variant.name.name}`;
        const constructor = variant.payload
          ? `constructor(value) { this.value = value; }`
          : "constructor() {}";

        return `class ${className} {
  ${constructor}
}`;
      })
      .join("\n\n");

    // Generate static factory methods
    const factoryMethods = variants
      .map((variant) => {
        const methodName = variant.name.name;
        const className = `${enumName}_${methodName}`;
        return variant.payload
          ? `static ${methodName}(value) { return new ${className}(value); }`
          : `static ${methodName} = new ${className}();`;
      })
      .join("\n  ");

    return `${variantClasses}

class ${enumName} {
  ${factoryMethods}
}`;
  }

  private generateExpression(expression: Expression): string {
    switch (expression.type) {
      case "StringLiteral":
        return this.generateStringLiteral(expression);
      case "NumberLiteral":
        return this.generateNumberLiteral(expression);
      case "BooleanLiteral":
        return this.generateBooleanLiteral(expression);
      case "NoneExpression":
        return this.generateNoneExpression(expression);
      case "Identifier":
        return this.generateIdentifier(expression);
      case "BinaryExpression":
        return this.generateBinaryExpression(expression);
      case "UnaryExpression":
        return this.generateUnaryExpression(expression);
      case "BlockExpression":
        return this.generateBlockExpression(expression);
      case "FunctionCall":
        return this.generateFunctionCall(expression);
      case "PropertyAccess":
        return this.generatePropertyAccess(expression);
      case "StructInitialization":
        return this.generateStructInitialization(expression);
      case "ArrayExpression":
        return this.generateArrayExpression(expression);
      case "ArrayIndexExpression":
        return this.generateArrayIndexExpression(expression);
      case "RangeExpression":
        return this.generateRangeExpression(expression);
      case "TypeIdentifier":
        return this.generateTypeIdentifier(expression);
      case "MatchExpression":
        return this.generateMatchExpression(expression);
      case "OrExpression":
        return this.generateOrExpression(expression);
      default:
        throw new Error("Unsupported expression type: " + expression.type);
    }
  }

  private generateStringLiteral(literal: StringLiteral): string {
    return `'${literal.value.replace(/'/g, "\\'")}'`;
  }

  private generateNumberLiteral(literal: NumberLiteral): string {
    return literal.value;
  }

  private generateBooleanLiteral(literal: BooleanLiteral): string {
    return literal.value.toString();
  }

  private generateNoneExpression(_: NoneExpression): string {
    return "null";
  }

  private generateIdentifier(identifier: Identifier): string {
    return identifier.name;
  }

  private generateBinaryExpression(expression: BinaryExpression): string {
    return `(${this.generateExpression(expression.left)} ${
      expression.operator
    } ${this.generateExpression(expression.right)})`;
  }

  private generateUnaryExpression(expression: UnaryExpression): string {
    return `${expression.operator}${this.generateExpression(
      expression.expression
    )}`;
  }

  private generateBlockExpression(expression: BlockExpression): string {
    return `{\n${this.generateStatements(expression.body)}\n}`;
  }

  private generateFunctionCall(expression: FunctionCall): string {
    return `${this.generateExpression(expression.callee)}(${expression.arguments
      .map((arg) => this.generateExpression(arg))
      .join(", ")})`;
  }

  private generatePropertyAccess(expression: PropertyAccess): string {
    return `${this.generateExpression(
      expression.left
    )}.${this.generateExpression(expression.right)}`;
  }

  private generateStructInitialization(
    expression: StructInitialization
  ): string {
    const fields = expression.fields
      .map(
        (field) =>
          `${field.identifier.name}: ${this.generateExpression(field.init)}`
      )
      .join(",\n" + this.getIndent());

    return `new ${
      expression.identifier.name
    }({\n${this.getIndent()}${fields}\n})`;
  }

  private generateArrayExpression(expression: ArrayExpression): string {
    return `[${expression.elements
      .map((e) => this.generateExpression(e))
      .join(", ")}]`;
  }

  private generateArrayIndexExpression(
    expression: ArrayIndexExpression
  ): string {
    return `${this.generateExpression(
      expression.array
    )}[${this.generateExpression(expression.index)}]`;
  }

  private generateRangeExpression(expression: RangeExpression): string {
    return `Array.from({length: ${this.generateExpression(
      expression.end
    )} - ${this.generateExpression(
      expression.start
    )}}, (_, i) => ${this.generateExpression(expression.start)} + i)`;
  }

  private generateTypeIdentifier(identifier: TypeIdentifier): string {
    let type = identifier.identifier.name;
    if (identifier.isArray) {
      type = `Array<${type}>`;
    }
    if (identifier.isOption) {
      type = `${type} | null`;
    }
    return type;
  }

  private generateMatchExpression(expression: MatchExpression): string {
    const { expression: matchExpr, cases } = expression;
    const value = this.generateExpression(matchExpr);

    const caseClauses = cases
      .map(({ pattern, body }) => {
        const condition = pattern.enumPath
          .map((p) => this.generateExpression(p))
          .join(".");

        const binding = pattern.binding
          ? `const ${pattern.binding.name} = ${value}.value;`
          : "";

        return `if (${value} instanceof ${condition}) {
  ${binding}
  return ${this.generateExpression(body)};
}`;
      })
      .join(" else ");

    return `(() => {
  ${caseClauses}
  throw new Error('No match case found');
})()`;
  }

  private generateOrExpression(expression: OrExpression): string {
    const { expression: tryExpr, errorBinding, handler } = expression;
    const tryValue = this.generateExpression(tryExpr);
    const handlerBody = this.generateBlockExpression(handler);

    if (errorBinding) {
      return `(() => {
  try {
    return ${tryValue};
  } catch (${errorBinding.name}) {
    ${handlerBody}
  }
})()`;
    } else {
      return `(() => {
  try {
    return ${tryValue};
  } catch (e) {
    ${handlerBody}
  }
})()`;
    }
  }
}
