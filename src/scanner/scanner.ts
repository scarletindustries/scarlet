import type { Token } from "../token/types";
import { KEYWORDS, TokenKind } from "../token/types";

export class Scanner {
  private pos: number = 0;
  private line: number = 1;
  private column: number = 1;

  constructor(private input: string) {}

  private isAtEnd(): boolean {
    return this.pos >= this.input.length;
  }

  private peek(): string {
    if (this.isAtEnd()) return "\0";
    return this.input[this.pos];
  }

  private peekNext(): string {
    if (this.pos + 1 >= this.input.length) return "\0";
    return this.input[this.pos + 1];
  }

  private advance(): string {
    const char = this.input[this.pos++];
    if (char === "\n") {
      this.line++;
      this.column = 1;
    } else {
      this.column++;
    }
    return char;
  }

  private match(expected: string): boolean {
    if (this.isAtEnd() || this.input[this.pos] !== expected) return false;
    this.pos++;
    this.column++;
    return true;
  }

  private skipWhitespace() {
    while (!this.isAtEnd()) {
      const char = this.peek();
      switch (char) {
        case " ":
        case "\r":
        case "\t":
          this.advance();
          break;
        case "\n":
          this.advance();
          break;
        case "/":
          if (this.peekNext() === "/") {
            while (this.peek() !== "\n" && !this.isAtEnd()) this.advance();
          } else {
            return;
          }
          break;
        default:
          return;
      }
    }
  }

  private isDigit(char: string): boolean {
    return char >= "0" && char <= "9";
  }

  private isAlpha(char: string): boolean {
    return (
      (char >= "a" && char <= "z") ||
      (char >= "A" && char <= "Z") ||
      char === "_"
    );
  }

  private isAlphaNumeric(char: string): boolean {
    return this.isAlpha(char) || this.isDigit(char);
  }

  private scanNumber(): Token {
    const start = this.pos - 1;
    const startColumn = this.column - 1;

    while (this.isDigit(this.peek())) this.advance();

    // Look for a decimal point
    if (this.peek() === "." && this.isDigit(this.peekNext())) {
      this.advance(); // Consume the "."
      while (this.isDigit(this.peek())) this.advance();
    }

    return {
      kind: TokenKind.LITERAL_NUMBER,
      literal: this.input.substring(start, this.pos),
      line: this.line,
      column: startColumn,
    };
  }

  private scanString(): Token {
    const start = this.pos;
    const startColumn = this.column;

    while (this.peek() !== "'" && !this.isAtEnd()) {
      if (this.peek() === "\\" && this.peekNext() === "'") {
        this.advance(); // Consume the backslash
      }
      this.advance();
    }

    if (this.isAtEnd()) {
      throw new Error(`Unterminated string at line ${this.line}`);
    }

    this.advance(); // Closing quote

    return {
      kind: TokenKind.LITERAL_STRING,
      literal: this.input.substring(start, this.pos - 1),
      line: this.line,
      column: startColumn,
    };
  }

  private scanIdentifier(): Token {
    const start = this.pos - 1;
    const startColumn = this.column - 1;

    while (this.isAlphaNumeric(this.peek())) this.advance();

    const text = this.input.substring(start, this.pos);
    const kind = KEYWORDS[text] || TokenKind.IDENTIFIER;

    return {
      kind,
      literal: text,
      line: this.line,
      column: startColumn,
    };
  }

  scanToken(): Token {
    this.skipWhitespace();

    if (this.isAtEnd()) {
      return { kind: TokenKind.EOF, line: this.line, column: this.column };
    }

    const char = this.advance();

    if (this.isDigit(char)) return this.scanNumber();
    if (this.isAlpha(char)) return this.scanIdentifier();

    switch (char) {
      case "'":
        return this.scanString();
      case "(":
        return {
          kind: TokenKind.PUNC_OPEN_PAREN,
          line: this.line,
          column: this.column - 1,
        };
      case ")":
        return {
          kind: TokenKind.PUNC_CLOSE_PAREN,
          line: this.line,
          column: this.column - 1,
        };
      case "{":
        return {
          kind: TokenKind.PUNC_OPEN_BRACE,
          line: this.line,
          column: this.column - 1,
        };
      case "}":
        return {
          kind: TokenKind.PUNC_CLOSE_BRACE,
          line: this.line,
          column: this.column - 1,
        };
      case "[":
        return {
          kind: TokenKind.PUNC_OPEN_BRACKET,
          line: this.line,
          column: this.column - 1,
        };
      case "]":
        return {
          kind: TokenKind.PUNC_CLOSE_BRACKET,
          line: this.line,
          column: this.column - 1,
        };
      case ",":
        return {
          kind: TokenKind.PUNC_COMMA,
          line: this.line,
          column: this.column - 1,
        };
      case ".":
        if (this.match(".")) {
          return {
            kind: TokenKind.PUNC_DOTDOT,
            line: this.line,
            column: this.column - 2,
          };
        }
        return {
          kind: TokenKind.PUNC_DOT,
          line: this.line,
          column: this.column - 1,
        };
      case ":":
        if (this.match("=")) {
          return {
            kind: TokenKind.PUNC_COLON_EQUALS,
            line: this.line,
            column: this.column - 2,
          };
        }
        return {
          kind: TokenKind.PUNC_COLON,
          line: this.line,
          column: this.column - 1,
        };
      case ";":
        return {
          kind: TokenKind.PUNC_SEMICOLON,
          line: this.line,
          column: this.column - 1,
        };
      case "=":
        if (this.match("=")) {
          return {
            kind: TokenKind.PUNC_EQUALS_EQUALS,
            line: this.line,
            column: this.column - 2,
          };
        }
        if (this.match(">")) {
          return {
            kind: TokenKind.PUNC_ARROW,
            line: this.line,
            column: this.column - 2,
          };
        }
        return {
          kind: TokenKind.PUNC_EQUALS,
          line: this.line,
          column: this.column - 1,
        };
      case "!":
        if (this.match("=")) {
          return {
            kind: TokenKind.PUNC_BANG_EQUALS,
            line: this.line,
            column: this.column - 2,
          };
        }
        return {
          kind: TokenKind.PUNC_BANG,
          line: this.line,
          column: this.column - 1,
        };
      case ">":
        if (this.match("=")) {
          return {
            kind: TokenKind.PUNC_GREATER_EQUALS,
            line: this.line,
            column: this.column - 2,
          };
        }
        return {
          kind: TokenKind.PUNC_GREATER,
          line: this.line,
          column: this.column - 1,
        };
      case "<":
        if (this.match("=")) {
          return {
            kind: TokenKind.PUNC_LESS_EQUALS,
            line: this.line,
            column: this.column - 2,
          };
        }
        return {
          kind: TokenKind.PUNC_LESS,
          line: this.line,
          column: this.column - 1,
        };
      case "+":
        return {
          kind: TokenKind.PUNC_PLUS,
          line: this.line,
          column: this.column - 1,
        };
      case "-":
        if (this.match(">")) {
          return {
            kind: TokenKind.PUNC_ARROW,
            line: this.line,
            column: this.column - 2,
          };
        }
        return {
          kind: TokenKind.PUNC_MINUS,
          line: this.line,
          column: this.column - 1,
        };
      case "*":
        return {
          kind: TokenKind.PUNC_STAR,
          line: this.line,
          column: this.column - 1,
        };
      case "/":
        return {
          kind: TokenKind.PUNC_SLASH,
          line: this.line,
          column: this.column - 1,
        };
      case "%":
        return {
          kind: TokenKind.PUNC_PERCENT,
          line: this.line,
          column: this.column - 1,
        };
      case "?":
        return {
          kind: TokenKind.PUNC_QUESTION,
          line: this.line,
          column: this.column - 1,
        };
      case "&":
        return {
          kind: TokenKind.PUNC_AND,
          line: this.line,
          column: this.column - 1,
        };
      case "|":
        return {
          kind: TokenKind.PUNC_PIPE,
          line: this.line,
          column: this.column - 1,
        };
    }

    throw new Error(
      `Unexpected character '${char}' at line ${this.line}, column ${this.column}`
    );
  }

  scanAll(): Token[] {
    const tokens: Token[] = [];
    while (!this.isAtEnd()) {
      tokens.push(this.scanToken());
    }
    tokens.push({ kind: TokenKind.EOF, line: this.line, column: this.column });
    return tokens;
  }
}
