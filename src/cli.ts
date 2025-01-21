import { Command } from "commander";
import { readFileSync } from "fs";
import { Compiler } from "./compiler";

const program = new Command();

program.name("alc").description("al compiler and toolchain").version("0.0.1");

program
  .command("build")
  .description("Build and compile an entrypoint to your program")
  .argument("<entrypoint>", "The entrypoint file to compile")
  .action((entrypoint: string) => {
    try {
      const source = readFileSync(entrypoint, "utf-8");
      const compiler = new Compiler();
      const result = compiler.compile(source);
      console.log(result);
    } catch (error) {
      console.error("Error:", error instanceof Error ? error.message : error);
      process.exit(1);
    }
  });

program.parse();
