import type {
  EnumVariant,
  Expression,
  FunctionParameter,
  Identifier,
  ImportSpecifier,
  MatchCase,
  MatchExpression,
  MatchPattern,
  PropertyAccess,
  Statement,
  StructField,
  TypeIdentifier,
} from "../ast/nodes";
import type { Token } from "../token/types";
import { TokenKind, tokenKindToString } from "../token/types";

export class Parser {
  private index: number = 0;
  private currentToken: Token;

  constructor(private tokens: Token[], private src: string) {
    this.currentToken = tokens[0];
  }

  private error(message: string): never {
    const line = this.currentToken.line;
    const column = this.currentToken.column;

    const split = this.src.split("\n");

    const theTwoLinesAbove =
      line > 2 ? split.slice(line - 3, line - 1).join("\n") : "";
    const theTwoLinesBelow =
      line < split.length - 1 ? split.slice(line, line + 2).join("\n") : "";

    const lineContent = split[line - 1];

    const pointer = " ".repeat(column - 1) + "^";

    const content = `\n${theTwoLinesAbove}\n${lineContent}\n${pointer}\n${theTwoLinesBelow}\n`;

    throw new Error(
      `${message} at line ${line}, column ${column}\n` + `${content}`
    );
  }

  private eat(kind: TokenKind): Token {
    if (this.currentToken.kind !== kind) {
      this.error(
        `Expected \`${tokenKindToString(kind)}\` but got \`${tokenKindToString(
          this.currentToken.kind
        )}\``
      );
    }

    const token = this.currentToken;
    this.index++;
    this.currentToken = this.tokens[this.index];
    return token;
  }

  private peek(): Token {
    return this.tokens[this.index + 1];
  }

  private isAtEnd(): boolean {
    return this.currentToken.kind === TokenKind.EOF;
  }

  private parseIdentifier(): Identifier {
    const token = this.eat(TokenKind.IDENTIFIER);
    if (!token.literal) this.error("Expected identifier");
    return {
      type: "Identifier",
      name: token.literal,
    };
  }

  private parseTypeIdentifier(): TypeIdentifier {
    let isArray = false;
    const identifier = this.parseIdentifier();

    if (this.currentToken.kind === TokenKind.PUNC_OPEN_BRACKET) {
      this.eat(TokenKind.PUNC_OPEN_BRACKET);
      this.eat(TokenKind.PUNC_CLOSE_BRACKET);
      isArray = true;
    }

    return {
      type: "TypeIdentifier",
      identifier,
      isArray,
      isOption: false, // TODO: Implement option types
    };
  }

  private parseFunctionParameter(): FunctionParameter {
    const identifier = this.parseIdentifier();
    let typeAnnotation;

    if (this.currentToken.kind === TokenKind.IDENTIFIER) {
      typeAnnotation = this.parseTypeIdentifier();
    }

    return {
      type: "FunctionParameter",
      identifier,
      typeAnnotation,
    };
  }

  private parseStructField(): StructField {
    const identifier = this.parseIdentifier();
    this.eat(TokenKind.PUNC_COLON);
    const typeAnnotation = this.parseTypeIdentifier();

    let init;
    if (this.currentToken.kind === TokenKind.PUNC_EQUALS) {
      this.eat(TokenKind.PUNC_EQUALS);
      init = this.parseExpression();
    }

    return {
      type: "StructField",
      identifier,
      typeAnnotation,
      init,
    };
  }

  private parseImportSpecifier(): ImportSpecifier {
    const identifier = this.parseIdentifier();
    return {
      type: "ImportSpecifier",
      identifier,
    };
  }

  private parseExpression(isMatchPattern: boolean = false): Expression {
    // Handle match expressions directly
    if ((this.currentToken.kind as TokenKind) === TokenKind.KW_MATCH) {
      return this.parseMatchExpression();
    }

    // Parse other expressions
    let expr = isMatchPattern
      ? this.parsePrimaryExpression(true)
      : this.parseAssignmentExpression();

    // If we're in a match pattern, we need to handle property access here
    if (isMatchPattern) {
      while ((this.currentToken.kind as TokenKind) === TokenKind.PUNC_DOT) {
        this.eat(TokenKind.PUNC_DOT);
        if ((this.currentToken.kind as TokenKind) !== TokenKind.IDENTIFIER) {
          this.error("Expected identifier after dot in match pattern");
        }
        const right = this.parseIdentifier();
        expr = {
          type: "PropertyAccess",
          left: expr,
          right,
        };
      }
    }

    if (
      !isMatchPattern &&
      (this.currentToken.kind as TokenKind) === TokenKind.KW_OR
    ) {
      this.eat(TokenKind.KW_OR);
      let errorBinding;

      if ((this.currentToken.kind as TokenKind) === TokenKind.IDENTIFIER) {
        errorBinding = this.parseIdentifier();
      }

      this.eat(TokenKind.PUNC_OPEN_BRACE);
      const body: Statement[] = [];
      while (
        (this.currentToken.kind as TokenKind) !== TokenKind.PUNC_CLOSE_BRACE &&
        !this.isAtEnd()
      ) {
        body.push(this.parseStatement());
      }
      this.eat(TokenKind.PUNC_CLOSE_BRACE);

      return {
        type: "OrExpression",
        expression: expr,
        errorBinding,
        handler: {
          type: "BlockExpression",
          body,
        },
      };
    }

    return expr;
  }

  private parseAssignmentExpression(): Expression {
    let expr = this.parseLogicalExpression();

    if (this.currentToken.kind === TokenKind.PUNC_EQUALS) {
      this.eat(TokenKind.PUNC_EQUALS);
      const right = this.parseAssignmentExpression();
      return {
        type: "BinaryExpression",
        operator: "=",
        left: expr,
        right,
      };
    }

    return expr;
  }

  private parseLogicalExpression(): Expression {
    let expr = this.parseComparisonExpression();

    while (
      this.currentToken.kind === TokenKind.PUNC_AND ||
      this.currentToken.kind === TokenKind.PUNC_OR
    ) {
      const operator =
        this.currentToken.kind === TokenKind.PUNC_AND ? "&&" : "||";
      this.eat(this.currentToken.kind);
      const right = this.parseComparisonExpression();
      expr = {
        type: "BinaryExpression",
        operator,
        left: expr,
        right,
      };
    }

    return expr;
  }

  private parseComparisonExpression(): Expression {
    let expr = this.parseAdditiveExpression();

    while (
      this.currentToken.kind === TokenKind.PUNC_EQUALS_EQUALS ||
      this.currentToken.kind === TokenKind.PUNC_BANG_EQUALS ||
      this.currentToken.kind === TokenKind.PUNC_GREATER ||
      this.currentToken.kind === TokenKind.PUNC_GREATER_EQUALS ||
      this.currentToken.kind === TokenKind.PUNC_LESS ||
      this.currentToken.kind === TokenKind.PUNC_LESS_EQUALS
    ) {
      const operator = (() => {
        switch (this.currentToken.kind) {
          case TokenKind.PUNC_EQUALS_EQUALS:
            return "===";
          case TokenKind.PUNC_BANG_EQUALS:
            return "!==";
          case TokenKind.PUNC_GREATER:
            return ">";
          case TokenKind.PUNC_GREATER_EQUALS:
            return ">=";
          case TokenKind.PUNC_LESS:
            return "<";
          case TokenKind.PUNC_LESS_EQUALS:
            return "<=";
          default:
            return "===";
        }
      })();

      this.eat(this.currentToken.kind);
      const right = this.parseAdditiveExpression();
      expr = {
        type: "BinaryExpression",
        operator,
        left: expr,
        right,
      };
    }

    return expr;
  }

  private parseAdditiveExpression(): Expression {
    let expr = this.parseMultiplicativeExpression();

    while (
      this.currentToken.kind === TokenKind.PUNC_PLUS ||
      this.currentToken.kind === TokenKind.PUNC_MINUS
    ) {
      const operator =
        this.currentToken.kind === TokenKind.PUNC_PLUS ? "+" : "-";
      this.eat(this.currentToken.kind);
      const right = this.parseMultiplicativeExpression();
      expr = {
        type: "BinaryExpression",
        operator,
        left: expr,
        right,
      };
    }

    return expr;
  }

  private parseMultiplicativeExpression(): Expression {
    let expr = this.parseUnaryExpression();

    while (
      this.currentToken.kind === TokenKind.PUNC_STAR ||
      this.currentToken.kind === TokenKind.PUNC_SLASH ||
      this.currentToken.kind === TokenKind.PUNC_PERCENT
    ) {
      const operator =
        this.currentToken.kind === TokenKind.PUNC_STAR
          ? "*"
          : this.currentToken.kind === TokenKind.PUNC_SLASH
          ? "/"
          : "%";
      this.eat(this.currentToken.kind);
      const right = this.parseUnaryExpression();
      expr = {
        type: "BinaryExpression",
        operator,
        left: expr,
        right,
      };
    }

    return expr;
  }

  private parseUnaryExpression(): Expression {
    if (
      this.currentToken.kind === TokenKind.PUNC_BANG ||
      this.currentToken.kind === TokenKind.PUNC_MINUS
    ) {
      const operator =
        this.currentToken.kind === TokenKind.PUNC_BANG ? "!" : "-";
      this.eat(this.currentToken.kind);
      const expression = this.parseUnaryExpression();
      return {
        type: "UnaryExpression",
        operator,
        expression,
      };
    }

    return this.parsePrimaryExpression(false);
  }

  private parsePrimaryExpression(isMatchPattern: boolean): Expression {
    let expr: Expression;

    switch (this.currentToken.kind) {
      case TokenKind.LITERAL_STRING:
        expr = this.parseStringLiteral();
        break;
      case TokenKind.LITERAL_NUMBER:
        expr = this.parseNumberLiteral();
        break;
      case TokenKind.KW_TRUE:
      case TokenKind.KW_FALSE:
        expr = this.parseBooleanLiteral();
        break;
      case TokenKind.KW_NONE:
        expr = this.parseNoneLiteral();
        break;
      case TokenKind.PUNC_OPEN_BRACKET:
        expr = this.parseArrayExpression();
        break;
      case TokenKind.KW_MATCH:
        expr = this.parseMatchExpression();
        break;
      case TokenKind.IDENTIFIER:
        expr = this.parseIdentifier();
        break;
      default:
        this.error(`Unexpected token ${this.currentToken.kind}`);
    }

    // Parse property access, function calls, and array indexing
    while (true) {
      const kind = this.currentToken.kind as TokenKind;

      if (kind === TokenKind.PUNC_DOT && !isMatchPattern) {
        this.eat(TokenKind.PUNC_DOT);
        const property = this.parseIdentifier();
        expr = {
          type: "PropertyAccess",
          left: expr,
          right: property,
        };
      } else if (!isMatchPattern && kind === TokenKind.PUNC_OPEN_PAREN) {
        this.eat(TokenKind.PUNC_OPEN_PAREN);
        const args: Expression[] = [];
        if (
          (this.currentToken.kind as TokenKind) !== TokenKind.PUNC_CLOSE_PAREN
        ) {
          do {
            args.push(this.parseExpression());
          } while (
            (this.currentToken.kind as TokenKind) === TokenKind.PUNC_COMMA &&
            this.eat(TokenKind.PUNC_COMMA)
          );
        }
        this.eat(TokenKind.PUNC_CLOSE_PAREN);
        expr = {
          type: "FunctionCall",
          identifier: expr,
          arguments: args,
        };
      } else if (!isMatchPattern && kind === TokenKind.PUNC_OPEN_BRACKET) {
        this.eat(TokenKind.PUNC_OPEN_BRACKET);
        const index = this.parseExpression();
        this.eat(TokenKind.PUNC_CLOSE_BRACKET);
        expr = {
          type: "ArrayIndexExpression",
          identifier: expr,
          index,
        };
      } else if (
        !isMatchPattern &&
        kind === TokenKind.PUNC_OPEN_BRACE &&
        expr.type === "Identifier"
      ) {
        expr = this.parseStructInitialization(expr);
      } else if (!isMatchPattern && kind === TokenKind.PUNC_DOTDOT) {
        this.eat(TokenKind.PUNC_DOTDOT);
        const end = this.parseExpression();
        expr = {
          type: "RangeExpression",
          start: expr,
          end,
        };
      } else {
        break;
      }
    }

    return expr;
  }

  private parseStructInitialization(identifier: Identifier): Expression {
    this.eat(TokenKind.PUNC_OPEN_BRACE);
    const fields: { identifier: Identifier; init: Expression }[] = [];

    while (
      (this.currentToken.kind as TokenKind) !== TokenKind.PUNC_CLOSE_BRACE
    ) {
      const name = this.parseIdentifier();
      this.eat(TokenKind.PUNC_COLON);
      const init = this.parseExpression();

      fields.push({
        identifier: name,
        init,
      });

      if ((this.currentToken.kind as TokenKind) === TokenKind.PUNC_COMMA) {
        this.eat(TokenKind.PUNC_COMMA);
      }
    }

    this.eat(TokenKind.PUNC_CLOSE_BRACE);
    return {
      type: "StructInitialization",
      identifier,
      fields,
    };
  }

  private parseStringLiteral(): Expression {
    const token = this.eat(TokenKind.LITERAL_STRING);
    if (!token.literal) this.error("Expected string literal");
    return {
      type: "StringLiteral",
      value: token.literal,
    };
  }

  private parseNumberLiteral(): Expression {
    const token = this.eat(TokenKind.LITERAL_NUMBER);
    if (!token.literal) this.error("Expected number literal");
    return {
      type: "NumberLiteral",
      value: token.literal,
    };
  }

  private parseBooleanLiteral(): Expression {
    const isTrue = this.currentToken.kind === TokenKind.KW_TRUE;
    this.eat(this.currentToken.kind);
    return {
      type: "BooleanLiteral",
      value: isTrue,
    };
  }

  private parseNoneLiteral(): Expression {
    this.eat(TokenKind.KW_NONE);
    return {
      type: "NoneExpression",
    };
  }

  private parseArrayExpression(): Expression {
    this.eat(TokenKind.PUNC_OPEN_BRACKET);
    const elements: Expression[] = [];
    while (this.currentToken.kind !== TokenKind.PUNC_CLOSE_BRACKET) {
      elements.push(this.parseExpression());
      if (this.currentToken.kind === TokenKind.PUNC_COMMA) {
        this.eat(TokenKind.PUNC_COMMA);
      }
    }
    this.eat(TokenKind.PUNC_CLOSE_BRACKET);
    return {
      type: "ArrayExpression",
      elements,
    };
  }

  private parseStatement(): Statement {
    // Save the current state in case we need to backtrack
    // Save the current state in case we need to backtrack
    const savedIndex = this.index;
    const savedToken = this.currentToken;

    try {
      switch (this.currentToken.kind) {
        case TokenKind.KW_FUNCTION:
          return this.parseFunctionStatement();
        case TokenKind.KW_RETURN:
          return this.parseReturnStatement();
        case TokenKind.KW_CONST:
          return this.parseConstStatement();
        case TokenKind.KW_STRUCT:
          return this.parseStructDeclaration();
        case TokenKind.KW_IMPORT:
        case TokenKind.KW_FROM:
          return this.parseImportDeclaration();
        case TokenKind.KW_EXPORT:
          return this.parseExportStatement();
        case TokenKind.KW_IF:
          return this.parseIfStatement();
        case TokenKind.KW_FOR:
          return this.parseForStatement();
        case TokenKind.KW_THROW:
          return this.parseThrowStatement();
        case TokenKind.KW_ASSERT:
          return this.parseAssertStatement();
        case TokenKind.KW_BREAK:
          return this.parseBreakStatement();
        case TokenKind.KW_CONTINUE:
          return this.parseContinueStatement();
        case TokenKind.KW_ENUM:
          return this.parseEnumDeclaration();
        case TokenKind.KW_MATCH:
          return this.parseMatchExpression();
        case TokenKind.IDENTIFIER: {
          const identifier = this.parseIdentifier();

          if (
            (this.currentToken.kind as TokenKind) ===
            TokenKind.PUNC_COLON_EQUALS
          ) {
            this.eat(TokenKind.PUNC_COLON_EQUALS);
            const init = this.parseExpression();
            return {
              type: "DeclarationStatement",
              identifier,
              init,
            };
          }

          // If it's not a declaration, backtrack and parse as expression
          this.index = savedIndex;
          this.currentToken = savedToken;
          return this.parseExpression();
        }
        default:
          return this.parseExpression();
      }
    } catch (e) {
      // If parsing fails, restore the state and rethrow
      this.index = savedIndex;
      this.currentToken = savedToken;
      throw e;
    }
  }

  private parseFunctionStatement(): Statement {
    this.eat(TokenKind.KW_FUNCTION);
    const identifier = this.parseIdentifier();
    this.eat(TokenKind.PUNC_OPEN_PAREN);

    const params: FunctionParameter[] = [];
    if (this.currentToken.kind !== TokenKind.PUNC_CLOSE_PAREN) {
      do {
        params.push(this.parseFunctionParameter());
      } while (
        this.currentToken.kind === TokenKind.PUNC_COMMA &&
        this.eat(TokenKind.PUNC_COMMA)
      );
    }
    this.eat(TokenKind.PUNC_CLOSE_PAREN);

    let returnType;
    let throwType;

    const nextToken = this.currentToken;
    if (nextToken.kind === TokenKind.IDENTIFIER) {
      returnType = this.parseTypeIdentifier();
    }

    if (this.currentToken.kind === TokenKind.PUNC_COMMA) {
      this.eat(TokenKind.PUNC_COMMA);
      throwType = this.parseTypeIdentifier();
    }

    this.eat(TokenKind.PUNC_OPEN_BRACE);
    const body: Statement[] = [];
    while (
      this.currentToken.kind !== TokenKind.PUNC_CLOSE_BRACE &&
      !this.isAtEnd()
    ) {
      body.push(this.parseStatement());
    }
    this.eat(TokenKind.PUNC_CLOSE_BRACE);

    return {
      type: "FunctionStatement",
      identifier,
      params,
      body,
      returnType,
      throwType,
    };
  }

  private parseReturnStatement(): Statement {
    this.eat(TokenKind.KW_RETURN);
    let expression;
    if (this.currentToken.kind !== TokenKind.PUNC_SEMICOLON) {
      expression = this.parseExpression();
    }
    return {
      type: "ReturnStatement",
      expression,
    };
  }

  private parseConstStatement(): Statement {
    this.eat(TokenKind.KW_CONST);
    const identifier = this.parseIdentifier();
    this.eat(TokenKind.PUNC_EQUALS);
    const init = this.parseExpression();
    return {
      type: "ConstStatement",
      identifier,
      init,
    };
  }

  private parseStructDeclaration(): Statement {
    this.eat(TokenKind.KW_STRUCT);
    const identifier = this.parseIdentifier();
    this.eat(TokenKind.PUNC_OPEN_BRACE);

    const fields: StructField[] = [];
    while (this.currentToken.kind !== TokenKind.PUNC_CLOSE_BRACE) {
      fields.push(this.parseStructField());
      if (this.currentToken.kind === TokenKind.PUNC_COMMA) {
        this.eat(TokenKind.PUNC_COMMA);
      }
    }
    this.eat(TokenKind.PUNC_CLOSE_BRACE);

    return {
      type: "StructDeclaration",
      identifier,
      fields,
    };
  }

  private parseImportDeclaration(): Statement {
    if (this.currentToken.kind === TokenKind.KW_FROM) {
      this.eat(TokenKind.KW_FROM);
      const pathToken = this.eat(TokenKind.LITERAL_STRING);
      if (!pathToken.literal)
        this.error("Expected string literal for import path");
      this.eat(TokenKind.KW_IMPORT);

      const specifiers: ImportSpecifier[] = [];
      do {
        specifiers.push(this.parseImportSpecifier());
        if ((this.currentToken.kind as TokenKind) === TokenKind.PUNC_COMMA) {
          this.eat(TokenKind.PUNC_COMMA);
        } else {
          break;
        }
      } while (true);

      return {
        type: "ImportDeclaration",
        specifiers,
        path: pathToken.literal,
      };
    } else {
      this.eat(TokenKind.KW_IMPORT);
      const specifiers: ImportSpecifier[] = [];

      while ((this.currentToken.kind as TokenKind) !== TokenKind.KW_FROM) {
        specifiers.push(this.parseImportSpecifier());
        if ((this.currentToken.kind as TokenKind) === TokenKind.PUNC_COMMA) {
          this.eat(TokenKind.PUNC_COMMA);
        }
      }

      this.eat(TokenKind.KW_FROM);
      const pathToken = this.eat(TokenKind.LITERAL_STRING);
      if (!pathToken.literal)
        this.error("Expected string literal for import path");

      return {
        type: "ImportDeclaration",
        specifiers,
        path: pathToken.literal,
      };
    }
  }

  private parseExportStatement(): Statement {
    this.eat(TokenKind.KW_EXPORT);
    return {
      type: "ExportStatement",
      declaration: this.parseStatement(),
    };
  }

  private parseIfStatement(): Statement {
    this.eat(TokenKind.KW_IF);
    const condition = this.parseExpression();
    this.eat(TokenKind.PUNC_OPEN_BRACE);

    const body: Statement[] = [];
    while (
      this.currentToken.kind !== TokenKind.PUNC_CLOSE_BRACE &&
      !this.isAtEnd()
    ) {
      body.push(this.parseStatement());
    }
    this.eat(TokenKind.PUNC_CLOSE_BRACE);

    let elseBody;
    const nextToken = this.currentToken;
    if (nextToken && nextToken.kind === TokenKind.KW_ELSE) {
      this.eat(TokenKind.KW_ELSE);
      this.eat(TokenKind.PUNC_OPEN_BRACE);
      elseBody = [];
      while (
        this.currentToken.kind !== TokenKind.PUNC_CLOSE_BRACE &&
        !this.isAtEnd()
      ) {
        elseBody.push(this.parseStatement());
      }
      this.eat(TokenKind.PUNC_CLOSE_BRACE);
    }

    return {
      type: "IfStatement",
      condition,
      body,
      elseBody,
    };
  }

  private parseForStatement(): Statement {
    this.eat(TokenKind.KW_FOR);

    // Check if it's a for-in statement
    if (this.peek().kind === TokenKind.KW_IN) {
      const identifier = this.parseIdentifier();
      this.eat(TokenKind.KW_IN);
      const expression = this.parseExpression();

      this.eat(TokenKind.PUNC_OPEN_BRACE);
      const body: Statement[] = [];
      while (this.currentToken.kind !== TokenKind.PUNC_CLOSE_BRACE) {
        body.push(this.parseStatement());
      }
      this.eat(TokenKind.PUNC_CLOSE_BRACE);

      return {
        type: "ForInStatement",
        identifier,
        expression,
        body,
      };
    }

    // Regular for statement
    this.eat(TokenKind.PUNC_OPEN_BRACE);
    const body: Statement[] = [];
    while (this.currentToken.kind !== TokenKind.PUNC_CLOSE_BRACE) {
      body.push(this.parseStatement());
    }
    this.eat(TokenKind.PUNC_CLOSE_BRACE);

    return {
      type: "ForStatement",
      body,
    };
  }

  private parseThrowStatement(): Statement {
    this.eat(TokenKind.KW_THROW);
    return {
      type: "ThrowStatement",
      expression: this.parseExpression(),
    };
  }

  private parseAssertStatement(): Statement {
    this.eat(TokenKind.KW_ASSERT);
    const expression = this.parseExpression();
    this.eat(TokenKind.PUNC_COMMA);
    const message = this.parseExpression();
    return {
      type: "AssertStatement",
      expression,
      message,
    };
  }

  private parseBreakStatement(): Statement {
    this.eat(TokenKind.KW_BREAK);
    return { type: "BreakStatement" };
  }

  private parseContinueStatement(): Statement {
    this.eat(TokenKind.KW_CONTINUE);
    return { type: "ContinueStatement" };
  }

  private parseEnumDeclaration(): Statement {
    this.eat(TokenKind.KW_ENUM);
    const identifier = this.parseIdentifier();
    this.eat(TokenKind.PUNC_OPEN_BRACE);

    const variants: EnumVariant[] = [];
    while (
      (this.currentToken.kind as TokenKind) !== TokenKind.PUNC_CLOSE_BRACE
    ) {
      const name = this.parseIdentifier();
      let payload;

      if ((this.currentToken.kind as TokenKind) === TokenKind.PUNC_OPEN_PAREN) {
        this.eat(TokenKind.PUNC_OPEN_PAREN);
        payload = this.parseTypeIdentifier();
        this.eat(TokenKind.PUNC_CLOSE_PAREN);
      }

      variants.push({
        type: "EnumVariant",
        name,
        payload,
      });

      if ((this.currentToken.kind as TokenKind) === TokenKind.PUNC_COMMA) {
        this.eat(TokenKind.PUNC_COMMA);
      }
    }

    this.eat(TokenKind.PUNC_CLOSE_BRACE);
    return {
      type: "EnumDeclaration",
      identifier,
      variants,
    };
  }

  private parseMatchExpression(): MatchExpression {
    this.eat(TokenKind.KW_MATCH);
    const expression = this.parseExpression();
    this.eat(TokenKind.PUNC_OPEN_BRACE);

    const cases: MatchCase[] = [];
    while (
      (this.currentToken.kind as TokenKind) !== TokenKind.PUNC_CLOSE_BRACE
    ) {
      // Parse the pattern directly
      // Parse the pattern directly
      if ((this.currentToken.kind as TokenKind) !== TokenKind.IDENTIFIER) {
        this.error("Expected identifier at start of match pattern");
      }

      // Parse the first identifier (e.g. MyEnum)
      const pattern = this.parseExpression(true) as Identifier | PropertyAccess;

      let binding;
      if ((this.currentToken.kind as TokenKind) === TokenKind.PUNC_OPEN_PAREN) {
        this.eat(TokenKind.PUNC_OPEN_PAREN);
        if ((this.currentToken.kind as TokenKind) !== TokenKind.IDENTIFIER) {
          this.error("Expected identifier in binding");
        }
        binding = this.parseIdentifier();
        this.eat(TokenKind.PUNC_CLOSE_PAREN);
      }

      const matchPattern: MatchPattern = {
        type: "MatchPattern",
        enumPath: [pattern],
        binding,
      };

      if ((this.currentToken.kind as TokenKind) !== TokenKind.PUNC_ARROW) {
        throw new Error(
          `Expected PUNC_ARROW, got ${this.currentToken.kind} at line ${this.currentToken.line}, column ${this.currentToken.column}`
        );
      }
      this.eat(TokenKind.PUNC_ARROW);

      // Parse the body expression
      let body;
      if ((this.currentToken.kind as TokenKind) === TokenKind.KW_MATCH) {
        body = this.parseMatchExpression();
      } else {
        body = this.parseExpression();
      }

      cases.push({
        type: "MatchCase",
        pattern: matchPattern,
        body,
      });

      if ((this.currentToken.kind as TokenKind) === TokenKind.PUNC_COMMA) {
        this.eat(TokenKind.PUNC_COMMA);
      }
    }

    this.eat(TokenKind.PUNC_CLOSE_BRACE);

    const result: MatchExpression = {
      type: "MatchExpression",
      expression,
      cases,
    };

    return result;
  }

  parseProgram(): Statement[] {
    const statements: Statement[] = [];
    while (!this.isAtEnd()) {
      statements.push(this.parseStatement());
    }
    return statements;
  }
}
