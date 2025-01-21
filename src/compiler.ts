import { JSGenerator } from "./generator/js";
import { Parser } from "./parser/parser";
import { Scanner } from "./scanner/scanner";

export class Compiler {
  compile(source: string): string {
    // Create scanner and tokenize input
    const scanner = new Scanner(source);
    const tokens = scanner.scanAll();

    // Parse tokens into AST
    const parser = new Parser(tokens, source);
    const ast = parser.parseProgram();

    // Generate JavaScript code
    const generator = new JSGenerator();
    return generator.generateRoot(ast);
  }
}
