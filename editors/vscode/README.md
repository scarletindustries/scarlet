<img width="96" src="images/icon.png" />

# Scarlet for Visual Studio Code

[Scarlet](https://scarlet.industries) language support for Visual Studio Code.

- Syntax highlighting for `.sca` files.
- Diagnostics and completions from the Scarlet language server (`scarlet lsp`).
- Document formatting through `scarlet fmt --stdin`.

## Requirements

The extension runs the `scarlet` binary from `PATH`. If the binary is somewhere else, set `scarlet.binaryPath`:

```json
{
  "scarlet.binaryPath": "/path/to/scarlet"
}
```

## Development

We use Bun for packages and scripts.

```sh
bun install
bun run build      # typecheck and bundle to out/extension.js
bun run package    # produce a .vsix
```
