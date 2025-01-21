import type {
  BlockExpression,
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

/**
 * Main Parser class.
 * Provides methods to parse tokens into an AST (Program, Statements, Expressions, etc.).
 */
export class Parser {
  private tokens: Token[];
  private index: number;
  private current: Token;
  private src: string; // For error-reporting context

  constructor(tokens: Token[], source: string) {
    this.tokens = tokens;
    this.src = source;
    this.index = 0;
    this.current = this.tokens[this.index];
  }

  /**
   * Entry point: parse the entire program into a root AST node (Program).
   */
  public parseProgram(): Statement[] {
    const statements: Statement[] = [];

    while (!this.isAtEnd()) {
      statements.push(this.parseStatement());
    }

    return statements;
  }

  /**
   * Consumes a token of a specific kind. If the token doesn't match, throws an error.
   */
  private eat(kind: TokenKind, message?: string): Token {
    if (this.current.kind !== kind) {
      this.error(
        message ||
          `Expected '${tokenKindToString(kind)}' but got '${tokenKindToString(
            this.current.kind
          )}'`
      );
    }
    const previous = this.current;
    this.advance();
    return previous;
  }

  /**
   * Moves to the next token and updates this.current.
   */
  private advance(): void {
    if (!this.isAtEnd()) {
      this.index++;
      this.current = this.tokens[this.index];
    }
  }

  /**
   * Optional consumption of a token if it matches.
   * Returns true if a token was consumed, false otherwise.
   */
  private match(kind: TokenKind): boolean {
    if (this.current.kind === kind) {
      this.advance();
      return true;
    }
    return false;
  }

  /**
   * Checks if we've reached the end of the tokens (EOF).
   */
  private isAtEnd(): boolean {
    return this.current.kind === TokenKind.EOF;
  }

  /**
   * Throws a parse error with helpful context (line, column, snippet).
   */
  private error(message: string): never {
    const { line, column } = this.current;
    const lines = this.src.split("\n");

    const snippetAbove = line > 1 ? lines[line - 2] : "";
    const snippet = lines[line - 1] || "";
    const snippetBelow = line < lines.length ? lines[line] : "";

    const pointer = " ".repeat(column - 1) + "^";

    const combined = [snippetAbove, snippet, pointer, snippetBelow]
      .filter(Boolean)
      .join("\n");

    throw new Error(
      `${message}\nAt line ${line}, column ${column}:\n${combined}`
    );
  }

  /**
   * Peek at the next token without consuming it.
   */
  private peek(offset = 1): Token {
    const idx = this.index + offset;
    if (idx >= this.tokens.length) return this.tokens[this.tokens.length - 1];
    return this.tokens[idx];
  }

  private debugLog(msg: string) {
    console.debug(
      `[Parser Debug] index=${this.index}, current=${tokenKindToString(
        this.current.kind
      )} => ${msg}`
    );
  }

  /**
   * Recursive descent parse for statements.
   * This might dispatch to many different parse functions
   * depending on the token kind (function, const, struct, etc.).
   */
  private parseStatement(): Statement {
    this.debugLog("parseStatement() entered");
    switch (this.current.kind) {
      case TokenKind.KW_FUNCTION:
        return this.parseFunctionStatement();

      case TokenKind.KW_STRUCT:
        return this.parseStructDeclaration();

      case TokenKind.KW_ENUM:
        return this.parseEnumDeclaration();

      case TokenKind.KW_RETURN:
        return this.parseReturnStatement();

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

      case TokenKind.KW_MATCH:
        this.debugLog(
          "Detected 'match' keyword – going to parse as an expression"
        );
        return this.parseExpression() as Statement;

      case TokenKind.KW_EXPORT:
        return this.parseExportStatement();

      case TokenKind.KW_CONST:
        return this.parseConstStatement();

      case TokenKind.KW_IMPORT:
      case TokenKind.KW_FROM:
        return this.parseImportStatement();

      // If we get an identifier, check if it might be a declaration like:
      //   myVar := expression
      case TokenKind.IDENTIFIER:
        return this.parsePossibleDeclarationOrExpression();

      default:
        return this.parseExpressionStatement();
    }
  }

  /**
   * If an identifier is encountered, it might be a variable declaration, or
   * it might be a simple expression. Peek ahead to decide which path to take.
   */
  private parsePossibleDeclarationOrExpression(): Statement {
    const savedIndex = this.index;
    const savedToken = this.current;

    const id = this.parseIdentifier();

    // In some languages, we might see "identifier :=" or "identifier ="
    // We'll check for a special token, e.g. PUNC_COLON_EQUALS or something similar.
    if (this.match(TokenKind.PUNC_COLON_EQUALS)) {
      // It's a short-hand declaration
      const init = this.parseExpression();
      return {
        type: "DeclarationStatement",
        identifier: id,
        init,
      };
    }

    // Not a short-hand declaration, revert and parse as expression
    this.index = savedIndex;
    this.current = savedToken;
    const expr = this.parseExpression();

    return {
      type: "ExpressionStatement",
      expression: expr,
    };
  }

  /**
   * A fallback when we suspect the token starts an expression rather than specialized statement keywords.
   */
  private parseExpressionStatement(): Statement {
    const expr = this.parseExpression();
    return {
      type: "ExpressionStatement",
      expression: expr,
    };
  }

  //---------------------------------------------------------------------------
  // Declarations & Definitions
  //---------------------------------------------------------------------------

  private parseFunctionStatement(): Statement {
    this.eat(TokenKind.KW_FUNCTION);
    const identifier = this.parseIdentifier();

    this.eat(TokenKind.PUNC_OPEN_PAREN);
    const params = this.parseFunctionParameters();
    this.eat(TokenKind.PUNC_CLOSE_PAREN);

    // Optionally parse return type
    let returnType: TypeIdentifier | undefined;
    let throwType: TypeIdentifier | undefined;

    if (this.current.kind === TokenKind.IDENTIFIER) {
      returnType = this.parseTypeIdentifier();
    }

    // If there's a comma, we parse the throw type
    if (this.match(TokenKind.PUNC_COMMA)) {
      throwType = this.parseTypeIdentifier();
    }

    // Finally, parse the function body (block)
    const body = this.parseBlockStatement();

    return {
      type: "FunctionStatement",
      identifier,
      params,
      body: [body],
      returnType,
      throwType,
    };
  }

  private parseFunctionParameters(): FunctionParameter[] {
    const params: FunctionParameter[] = [];

    if (this.current.kind !== TokenKind.PUNC_CLOSE_PAREN) {
      do {
        params.push(this.parseFunctionParameter());
      } while (this.match(TokenKind.PUNC_COMMA));
    }

    return params;
  }

  private parseFunctionParameter(): FunctionParameter {
    const identifier = this.parseIdentifier();

    // Optional type
    let typeAnnotation: TypeIdentifier | undefined;
    if (this.current.kind === TokenKind.IDENTIFIER) {
      typeAnnotation = this.parseTypeIdentifier();
    }

    return {
      type: "FunctionParameter",
      identifier,
      typeAnnotation,
    };
  }

  private parseStructDeclaration(): Statement {
    this.eat(TokenKind.KW_STRUCT);
    const identifier = this.parseIdentifier();

    this.eat(TokenKind.PUNC_OPEN_BRACE);
    const fields: StructField[] = [];
    while (
      this.current.kind !== TokenKind.PUNC_CLOSE_BRACE &&
      !this.isAtEnd()
    ) {
      fields.push(this.parseStructField());
      this.match(TokenKind.PUNC_COMMA); // optional comma
    }
    this.eat(TokenKind.PUNC_CLOSE_BRACE);

    return {
      type: "StructDeclaration",
      identifier,
      fields,
    };
  }

  private parseStructField(): StructField {
    const identifier = this.parseIdentifier();
    this.eat(TokenKind.PUNC_COLON);
    const typeAnnotation = this.parseTypeIdentifier();

    let init: Expression | undefined;
    if (this.match(TokenKind.PUNC_EQUALS)) {
      init = this.parseExpression();
    }

    return {
      type: "StructField",
      identifier,
      typeAnnotation,
      init,
    };
  }

  private parseEnumDeclaration(): Statement {
    this.eat(TokenKind.KW_ENUM);
    const identifier = this.parseIdentifier();

    this.eat(TokenKind.PUNC_OPEN_BRACE);
    const variants: EnumVariant[] = [];
    while (
      this.current.kind !== TokenKind.PUNC_CLOSE_BRACE &&
      !this.isAtEnd()
    ) {
      variants.push(this.parseEnumVariant());
      this.match(TokenKind.PUNC_COMMA); // optional comma
    }
    this.eat(TokenKind.PUNC_CLOSE_BRACE);

    return {
      type: "EnumDeclaration",
      identifier,
      variants,
    };
  }

  private parseEnumVariant(): EnumVariant {
    const name = this.parseIdentifier();
    let payload: TypeIdentifier | undefined = undefined;

    if (this.match(TokenKind.PUNC_OPEN_PAREN)) {
      payload = this.parseTypeIdentifier();
      this.eat(TokenKind.PUNC_CLOSE_PAREN);
    }

    return {
      type: "EnumVariant",
      name,
      payload,
    };
  }

  //---------------------------------------------------------------------------
  // Other Statements
  //---------------------------------------------------------------------------

  private parseReturnStatement(): Statement {
    this.eat(TokenKind.KW_RETURN);
    // Possibly parse an expression if not semicolon (or some other pattern)
    if (
      this.current.kind !== TokenKind.PUNC_SEMICOLON &&
      this.current.kind !== TokenKind.EOF &&
      this.current.kind !== TokenKind.PUNC_CLOSE_BRACE
    ) {
      const expression = this.parseExpression();
      return { type: "ReturnStatement", expression };
    }
    return { type: "ReturnStatement" };
  }

  private parseIfStatement(): Statement {
    this.eat(TokenKind.KW_IF);
    const condition = this.parseExpression();
    const thenBlock = this.parseBlockStatementOrSingle();

    let elseBlock: BlockExpression | undefined;
    if (this.match(TokenKind.KW_ELSE)) {
      elseBlock = this.parseBlockStatementOrSingle();
    }

    return {
      type: "IfStatement",
      condition,
      then: [thenBlock],
      else: elseBlock && [elseBlock],
    };
  }

  private parseForStatement(): Statement {
    this.eat(TokenKind.KW_FOR);
    // Distinguish between for-in or standard for:
    // for i in something { ... }
    // for { ... } (infinite loop)
    // Up to you to implement the grammar.

    if (this.peek().kind === TokenKind.KW_IN) {
      // for i in expression
      const identifier = this.parseIdentifier();
      this.eat(TokenKind.KW_IN);
      const iterator = this.parseExpression();
      const body = this.parseBlockStatement();
      return {
        type: "ForInStatement",
        identifier,
        iterator,
        body: [body],
      };
    } else {
      // for { ... }
      const body = this.parseBlockStatement();
      return {
        type: "ForStatement",
        body: [body],
      };
    }
  }

  private parseThrowStatement(): Statement {
    this.eat(TokenKind.KW_THROW);
    const expression = this.parseExpression();
    return { type: "ThrowStatement", expression };
  }

  private parseAssertStatement(): Statement {
    this.eat(TokenKind.KW_ASSERT);
    const expression = this.parseExpression();
    this.eat(TokenKind.PUNC_COMMA);
    const message = this.parseExpression();
    return { type: "AssertStatement", expression, message };
  }

  private parseBreakStatement(): Statement {
    this.eat(TokenKind.KW_BREAK);
    return { type: "BreakStatement" };
  }

  private parseContinueStatement(): Statement {
    this.eat(TokenKind.KW_CONTINUE);
    return { type: "ContinueStatement" };
  }

  private parseExportStatement(): Statement {
    this.eat(TokenKind.KW_EXPORT);
    // Usually, the syntax might be "export struct X {...}" or "export fn ..."
    // So we just parse the next statement and wrap it in an export node
    const declaration = this.parseStatement();
    return {
      type: "ExportStatement",
      declaration,
    };
  }

  private parseConstStatement(): Statement {
    this.eat(TokenKind.KW_CONST);
    const id = this.parseIdentifier();
    this.eat(TokenKind.PUNC_EQUALS);
    const init = this.parseExpression();
    return {
      type: "ConstStatement",
      identifier: id,
      init,
    };
  }

  private parseImportStatement(): Statement {
    // Could handle both "from '...' import ..." and "import x from '...'".
    // Because your language allows both forms, let's unify them.
    if (this.match(TokenKind.KW_FROM)) {
      const pathToken = this.eat(TokenKind.LITERAL_STRING);
      this.eat(TokenKind.KW_IMPORT);
      const specifiers = this.parseImportSpecifiers();
      return {
        type: "ImportDeclaration",
        specifiers,
        path: pathToken.literal || "",
      };
    } else {
      this.eat(TokenKind.KW_IMPORT);
      const specifiers = this.parseImportSpecifiers();
      this.eat(TokenKind.KW_FROM);
      const pathToken = this.eat(TokenKind.LITERAL_STRING);
      return {
        type: "ImportDeclaration",
        specifiers,
        path: pathToken.literal || "",
      };
    }
  }

  private parseImportSpecifiers(): ImportSpecifier[] {
    const specifiers: ImportSpecifier[] = [];
    do {
      specifiers.push(this.parseImportSpecifier());
    } while (this.match(TokenKind.PUNC_COMMA));
    return specifiers;
  }

  private parseImportSpecifier(): ImportSpecifier {
    const id = this.parseIdentifier();
    return {
      type: "ImportSpecifier",
      identifier: id,
    };
  }

  //---------------------------------------------------------------------------
  // Expressions
  //---------------------------------------------------------------------------

  /**
   * Handles "expr or err { block }" or "expr or expr"
   */
  private parseExpression(): Expression {
    const primary = this.parseAssignmentExpression();

    // If next token is KW_OR, parse the rest
    if (this.match(TokenKind.KW_OR)) {
      // We can parse "identifier" if it's the error binding or check for block/open brace
      let errorBinding: Identifier | undefined;
      if (this.current.kind === TokenKind.IDENTIFIER) {
        errorBinding = this.parseIdentifier();
      }

      // If we find an open brace, parse a block
      if (this.current.kind === TokenKind.PUNC_OPEN_BRACE) {
        const handlerBlock = this.parseBlockStatement();
        return {
          type: "OrExpression",
          expression: primary,
          errorBinding,
          handler: {
            type: "BlockExpression",
            body: handlerBlock.body,
          },
        };
      } else {
        // Otherwise, parse an expression fallback
        const fallbackExpr = this.parseExpression();
        return {
          type: "OrExpressionFallback",
          expression: primary,
          fallback: fallbackExpr,
        };
      }
    }

    return primary;
  }

  /**
   * Assignment expression: a = b, etc.
   */
  private parseAssignmentExpression(): Expression {
    const left = this.parseLogicalExpression();

    if (this.match(TokenKind.PUNC_EQUALS)) {
      // For now, treat it as a binary expression "="
      // or a specialized AST node "AssignmentExpression" if you prefer.
      const right = this.parseAssignmentExpression();
      return {
        type: "BinaryExpression",
        operator: "=",
        left,
        right,
      };
    }

    return left;
  }

  /**
   * Logical expressions: &&, ||
   */
  private parseLogicalExpression(): Expression {
    let expr = this.parseComparisonExpression();

    while (
      this.current.kind === TokenKind.PUNC_AND ||
      this.current.kind === TokenKind.PUNC_OR
    ) {
      const operator = this.current.kind === TokenKind.PUNC_AND ? "&&" : "||";
      this.advance();
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

  /**
   * Comparison expressions: ==, !=, <, <=, >, >=
   */
  private parseComparisonExpression(): Expression {
    let expr = this.parseAdditiveExpression();

    while (
      this.current.kind === TokenKind.PUNC_EQUALS_EQUALS ||
      this.current.kind === TokenKind.PUNC_BANG_EQUALS ||
      this.current.kind === TokenKind.PUNC_GREATER ||
      this.current.kind === TokenKind.PUNC_GREATER_EQUALS ||
      this.current.kind === TokenKind.PUNC_LESS ||
      this.current.kind === TokenKind.PUNC_LESS_EQUALS
    ) {
      const operator = (() => {
        switch (this.current.kind) {
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

      this.advance();
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

  /**
   * Additive expressions: +, -
   */
  private parseAdditiveExpression(): Expression {
    let expr = this.parseMultiplicativeExpression();

    while (
      this.current.kind === TokenKind.PUNC_PLUS ||
      this.current.kind === TokenKind.PUNC_MINUS
    ) {
      const operator = this.current.kind === TokenKind.PUNC_PLUS ? "+" : "-";
      this.advance();
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

  /**
   * Multiplicative expressions: *, /, %
   */
  private parseMultiplicativeExpression(): Expression {
    let expr = this.parseUnaryExpression();

    while (
      this.current.kind === TokenKind.PUNC_STAR ||
      this.current.kind === TokenKind.PUNC_SLASH ||
      this.current.kind === TokenKind.PUNC_PERCENT
    ) {
      const operator =
        this.current.kind === TokenKind.PUNC_STAR
          ? "*"
          : this.current.kind === TokenKind.PUNC_SLASH
          ? "/"
          : "%";
      this.advance();
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

  /**
   * Unary expressions: !, -
   */
  private parseUnaryExpression(): Expression {
    if (
      this.current.kind === TokenKind.PUNC_BANG ||
      this.current.kind === TokenKind.PUNC_MINUS
    ) {
      const operator = this.current.kind === TokenKind.PUNC_BANG ? "!" : "-";
      this.advance();
      const right = this.parseUnaryExpression();
      return {
        type: "UnaryExpression",
        operator,
        expression: right,
      };
    }

    return this.parsePrimaryExpression();
  }

  private parsePrimaryExpression(): Expression {
    this.debugLog(
      "parsePrimaryExpression() start: current token = " +
        tokenKindToString(this.current.kind)
    );

    let expr: Expression;

    // If current token = "match", parse match expression right away
    if (this.current.kind === TokenKind.KW_MATCH) {
      this.debugLog(
        "parsePrimaryExpression() found `match` -> parseMatchExpression()"
      );
      return this.parseMatchExpression();
    }

    // Otherwise, handle typical literal or identifier
    switch (this.current.kind) {
      case TokenKind.LITERAL_STRING:
        this.debugLog("parsePrimaryExpression() -> LITERAL_STRING");
        expr = this.parseStringLiteral();
        break;
      case TokenKind.LITERAL_NUMBER:
        this.debugLog("parsePrimaryExpression() -> LITERAL_NUMBER");
        expr = this.parseNumberLiteral();
        break;
      case TokenKind.KW_TRUE:
      case TokenKind.KW_FALSE:
        this.debugLog("parsePrimaryExpression() -> BooleanLiteral");
        expr = this.parseBooleanLiteral();
        break;
      case TokenKind.KW_NONE:
        this.debugLog("parsePrimaryExpression() -> NoneLiteral");
        expr = this.parseNoneLiteral();
        break;
      case TokenKind.PUNC_OPEN_BRACKET:
        this.debugLog("parsePrimaryExpression() -> parseArrayExpression()");
        expr = this.parseArrayExpression();
        break;
      case TokenKind.IDENTIFIER:
        this.debugLog("parsePrimaryExpression() -> parseIdentifier()");
        expr = this.parseIdentifier();
        break;
      default:
        this.error(
          `Unexpected token ${tokenKindToString(
            this.current.kind
          )} in parsePrimaryExpression()`
        );
    }

    // Check for trailing calls, property access, struct initialization, etc.
    while (true) {
      this.debugLog(
        "parsePrimaryExpression() [loop], looking at token = " +
          tokenKindToString(this.current.kind)
      );

      const currentKind = this.current.kind as TokenKind; // the `as TokenKind` is needed here because the methods above mutate `this.current` but TypeScript thinks it's still constrained

      if (currentKind === TokenKind.PUNC_DOT) {
        // property access
        this.debugLog("parsePrimaryExpression() -> property access");
        this.advance();
        const right = this.parseIdentifier();
        expr = {
          type: "PropertyAccess",
          left: expr,
          right,
        };
      } else if (currentKind === TokenKind.PUNC_OPEN_PAREN) {
        // function call
        this.debugLog("parsePrimaryExpression() -> parseFunctionCall()");
        expr = this.parseFunctionCall(expr);
      } else if (currentKind === TokenKind.PUNC_OPEN_BRACKET) {
        // array indexing
        this.debugLog("parsePrimaryExpression() -> parseArrayIndex()");
        expr = this.parseArrayIndex(expr);
      } else if (
        currentKind === TokenKind.PUNC_OPEN_BRACE &&
        expr.type === "Identifier"
      ) {
        // Peek ahead to decide if it's actually a struct initialization or something else (like match arms).
        if (this.looksLikeStructInitialization()) {
          this.debugLog(
            `parsePrimaryExpression() -> parseStructInitialization(${expr.name})`
          );
          expr = this.parseStructInitialization(expr as Identifier);
        } else {
          // Not a struct literal, so break and let the calling function handle the brace (e.g. match arms).
          break;
        }
      } else if (currentKind === TokenKind.PUNC_DOTDOT) {
        this.debugLog("parsePrimaryExpression() -> parse RangeExpression `..`");
        this.advance();
        const end = this.parseExpression();
        expr = {
          type: "RangeExpression",
          start: expr,
          end,
        };
      } else {
        // No more trailing tokens that belong to this expression
        break;
      }
    }

    this.debugLog(
      "parsePrimaryExpression() end, returning expr of type " + expr.type
    );
    return expr;
  }

  /**
   * Peek ahead to see if this '{' is likely a struct literal vs. a match block.
   * We consider it a struct only if:
   *   - The next token is '}' (empty struct), or
   *   - The next token is an IDENTIFIER and the following token is ':'
   * Otherwise, we assume it's something else (like match arms).
   */
  private looksLikeStructInitialization(): boolean {
    const nextToken = this.peek();
    if (!nextToken) return false; // no token => definitely not a struct

    // 1) "{}" for an empty struct
    if (nextToken.kind === TokenKind.PUNC_CLOSE_BRACE) {
      return true;
    }

    // 2) "identifier :" form
    if (nextToken.kind === TokenKind.IDENTIFIER) {
      const afterNext = this.peek(1);
      if (afterNext && afterNext.kind === TokenKind.PUNC_COLON) {
        return true;
      }
    }

    // If it doesn't look like a struct, let higher-level code (e.g. match) handle it
    return false;
  }

  private parseFunctionCall(callee: Expression): Expression {
    // e.g. myFunc(expr1, expr2, ...)
    this.eat(TokenKind.PUNC_OPEN_PAREN);
    const args: Expression[] = [];
    if (this.current.kind !== TokenKind.PUNC_CLOSE_PAREN) {
      do {
        args.push(this.parseExpression());
      } while (this.match(TokenKind.PUNC_COMMA));
    }
    this.eat(TokenKind.PUNC_CLOSE_PAREN);
    return {
      type: "FunctionCall",
      callee,
      arguments: args,
    };
  }

  private parseArrayIndex(arrayExpr: Expression): Expression {
    this.eat(TokenKind.PUNC_OPEN_BRACKET);
    const index = this.parseExpression();
    this.eat(TokenKind.PUNC_CLOSE_BRACKET);
    return {
      type: "ArrayIndexExpression",
      array: arrayExpr,
      index,
    };
  }

  private parseStructInitialization(id: Identifier): Expression {
    this.debugLog(`parseStructInitialization() for identifier '${id.name}'`);
    this.eat(TokenKind.PUNC_OPEN_BRACE);
    const fields: { identifier: Identifier; init: Expression }[] = [];

    while (
      this.current.kind !== TokenKind.PUNC_CLOSE_BRACE &&
      !this.isAtEnd()
    ) {
      this.debugLog(
        `parseStructInitialization() inside loop - current token = ${tokenKindToString(
          this.current.kind
        )}`
      );

      // This is where "Expected ':' but got ..." might be triggered if the parser
      // thinks it's reading a struct field but sees the wrong token.
      const fieldName = this.parseIdentifier();
      this.eat(TokenKind.PUNC_COLON);
      const init = this.parseExpression();
      fields.push({ identifier: fieldName, init });
      this.match(TokenKind.PUNC_COMMA);
    }

    this.eat(TokenKind.PUNC_CLOSE_BRACE);
    this.debugLog(
      `parseStructInitialization() done, created a StructInitialization for '${id.name}'`
    );
    return {
      type: "StructInitialization",
      identifier: id,
      fields,
    };
  }

  //---------------------------------------------------------------------------
  // Match Expression
  //---------------------------------------------------------------------------

  private parseMatchExpression(): MatchExpression {
    this.debugLog("parseMatchExpression() entered");
    this.eat(TokenKind.KW_MATCH);
    const expression = this.parseExpression();

    this.eat(TokenKind.PUNC_OPEN_BRACE);
    const cases: MatchCase[] = [];

    while (
      this.current.kind !== TokenKind.PUNC_CLOSE_BRACE &&
      !this.isAtEnd()
    ) {
      this.debugLog("parseMatchExpression() -> parsing one match case...");
      cases.push(this.parseMatchCase());
      this.match(TokenKind.PUNC_COMMA);
    }

    this.eat(TokenKind.PUNC_CLOSE_BRACE);

    return {
      type: "MatchExpression",
      expression,
      cases,
    };
  }

  private parseMatchCase(): MatchCase {
    this.debugLog("parseMatchCase() entered");
    const pattern = this.parseEnumPattern();
    this.debugLog(
      "after parseEnumPattern(), next token = " +
        tokenKindToString(this.current.kind)
    );

    if (!this.match(TokenKind.PUNC_ARROW)) {
      this.error("Expected '=>' in match case");
    }

    this.debugLog("matched =>, now parsing the body of the case");
    let body: Expression;
    if (this.current.kind === TokenKind.KW_MATCH) {
      body = this.parseMatchExpression();
    } else {
      body = this.parseExpression();
    }

    return {
      type: "MatchCase",
      pattern,
      body,
    };
  }

  /**
   * Parse something like:
   *   MyEnum.A
   *   MyEnum.B
   *   MyEnum.C(subBinding)
   *
   * We do NOT treat "(subBinding)" as a function call here!
   */
  private parseEnumPattern(): MatchPattern {
    this.debugLog("parseEnumPattern() entered");
    if (this.current.kind !== TokenKind.IDENTIFIER) {
      this.error("Expected identifier at start of match variant pattern");
    }

    let patternNode: Identifier | PropertyAccess = this.parseIdentifier();

    while (this.match(TokenKind.PUNC_DOT)) {
      const right = this.parseIdentifier();
      patternNode = {
        type: "PropertyAccess",
        left: patternNode,
        right,
      };
    }

    // Optionally something like (binding)
    let binding: Identifier | undefined;
    if (this.match(TokenKind.PUNC_OPEN_PAREN)) {
      if (this.current.kind !== TokenKind.IDENTIFIER) {
        this.error(
          "Expected identifier in match pattern binding (e.g. MyEnum.C(sub))"
        );
      }
      binding = this.parseIdentifier();
      this.eat(TokenKind.PUNC_CLOSE_PAREN);
    }

    this.debugLog("parseEnumPattern() complete, returning pattern");
    return {
      type: "MatchPattern",
      enumPath: [patternNode],
      binding,
    };
  }

  //---------------------------------------------------------------------------
  // Literal Parsing
  //---------------------------------------------------------------------------

  private parseStringLiteral(): Expression {
    const token = this.eat(TokenKind.LITERAL_STRING);
    if (!token.literal) {
      this.error("String token missing literal value");
    }
    return {
      type: "StringLiteral",
      value: token.literal,
    };
  }

  private parseNumberLiteral(): Expression {
    const token = this.eat(TokenKind.LITERAL_NUMBER);
    if (!token.literal) {
      this.error("Number token missing literal value");
    }
    return {
      type: "NumberLiteral",
      value: token.literal,
    };
  }

  private parseBooleanLiteral(): Expression {
    const isTrue = this.current.kind === TokenKind.KW_TRUE;
    this.advance();
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

    while (
      this.current.kind !== TokenKind.PUNC_CLOSE_BRACKET &&
      !this.isAtEnd()
    ) {
      elements.push(this.parseExpression());
      this.match(TokenKind.PUNC_COMMA);
    }

    this.eat(TokenKind.PUNC_CLOSE_BRACKET);
    return {
      type: "ArrayExpression",
      elements,
    };
  }

  //---------------------------------------------------------------------------
  // Helpers
  //---------------------------------------------------------------------------

  private parseTypeIdentifier(): TypeIdentifier {
    // e.g. MyType, or MyType[], or maybe ?MyType for optional.
    const identifier = this.parseIdentifier();
    let isArray = false;
    if (this.match(TokenKind.PUNC_OPEN_BRACKET)) {
      this.eat(TokenKind.PUNC_CLOSE_BRACKET);
      isArray = true;
    }
    // For now, ignore option (?) logic or do so if needed in your language.
    return {
      type: "TypeIdentifier",
      identifier,
      isArray,
      isOption: false,
    };
  }

  private parseIdentifier(): Identifier {
    if (this.current.kind !== TokenKind.IDENTIFIER) {
      this.error(
        `Expected identifier, got ${tokenKindToString(this.current.kind)}`
      );
    }
    const token = this.current;
    this.advance();
    return {
      type: "Identifier",
      name: token.literal || "",
    };
  }

  /**
   * Parses a block statement: { statement... }
   */
  private parseBlockStatement(): BlockExpression {
    this.eat(TokenKind.PUNC_OPEN_BRACE);
    const statements: Statement[] = [];
    while (
      !this.isAtEnd() &&
      this.current.kind !== TokenKind.PUNC_CLOSE_BRACE
    ) {
      statements.push(this.parseStatement());
    }
    this.eat(TokenKind.PUNC_CLOSE_BRACE);

    return {
      type: "BlockExpression",
      body: statements,
    };
  }

  private parseBlockStatementOrSingle(): BlockExpression {
    // If next token is a brace, parse a full block. Otherwise parse a single statement and wrap it.
    if (this.current.kind === TokenKind.PUNC_OPEN_BRACE) {
      return this.parseBlockStatement();
    } else {
      const single = this.parseStatement();
      return {
        type: "BlockExpression",
        body: [single],
      };
    }
  }
}
