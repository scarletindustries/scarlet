import { execFile } from "node:child_process";
import { promisify } from "node:util";
import * as vscode from "vscode";

const execFileAsync = promisify(execFile);

export function activate(context: vscode.ExtensionContext) {
  const formatter = vscode.languages.registerDocumentFormattingEditProvider(
    "al",
    {
      async provideDocumentFormattingEdits(
        document: vscode.TextDocument
      ): Promise<vscode.TextEdit[]> {
        const config = vscode.workspace.getConfiguration("al");
        const binaryPath = config.get("binaryPath", "al");

        try {
          const { stdout } = await execFileAsync(binaryPath, [
            "fmt",
            "--stdout",
            document.fileName,
          ]);

          const fullRange = new vscode.Range(
            document.positionAt(0),
            document.positionAt(document.getText().length)
          );

          return [vscode.TextEdit.replace(fullRange, stdout)];
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

export function deactivate() {}
