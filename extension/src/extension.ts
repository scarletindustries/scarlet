import { spawn } from "node:child_process";
import * as vscode from "vscode";
import {
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
} from "vscode-languageclient/node";

let client: LanguageClient | undefined;

export function activate(context: vscode.ExtensionContext) {
  const config = vscode.workspace.getConfiguration("al");
  const binaryPath = config.get("binaryPath", "al");

   const serverOptions: ServerOptions = {
    command: binaryPath,
    args: ["lsp"],
  };

  const clientOptions: LanguageClientOptions = {
    documentSelector: [{ scheme: "file", language: "al" }],
  };

  client = new LanguageClient(
    "al-lsp",
    "AL Language Server",
    serverOptions,
    clientOptions
  );

  client.start();

   const formatter = vscode.languages.registerDocumentFormattingEditProvider(
    "al",
    {
      async provideDocumentFormattingEdits(
        document: vscode.TextDocument
      ): Promise<vscode.TextEdit[]> {
        try {
          const formatted = await formatWithStdin(
            binaryPath,
            document.getText()
          );

          const fullRange = new vscode.Range(
            document.positionAt(0),
            document.positionAt(document.getText().length)
          );

          return [vscode.TextEdit.replace(fullRange, formatted)];
        } catch (error) {
          const message =
            error instanceof Error ? error.message : String(error);
          vscode.window.showErrorMessage(`AL format failed: ${message}`);
          return [];
        }
      },
    }
  );

  context.subscriptions.push(formatter);
}

function formatWithStdin(binaryPath: string, content: string): Promise<string> {
  return new Promise((resolve, reject) => {
    const proc = spawn(binaryPath, ["fmt", "--stdin"]);

    let stdout = "";
    let stderr = "";

    proc.stdout.on("data", (data) => {
      stdout += data.toString();
    });

    proc.stderr.on("data", (data) => {
      stderr += data.toString();
    });

    proc.on("error", (err) => {
      reject(err);
    });

    proc.on("close", (code) => {
      if (code === 0) {
        resolve(stdout);
      } else {
        reject(new Error(stderr || `Process exited with code ${code}`));
      }
    });

    proc.stdin.write(content);
    proc.stdin.end();
  });
}

export async function deactivate() {
  if (client) {
    await client.stop();
  }
}
