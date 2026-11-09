<img width="96" src="logo.png" />

# Scarlet for Zed

[Scarlet](https://scarlet.industries) language support for Zed.

- Registers the Scarlet language for `.sca` files, with comments, brackets and tab indentation.
- Syntax highlighting, brackets, indentation and the outline from the tree-sitter grammar in `editors/tree-sitter-scarlet`.
- Diagnostics, completions and formatting from the Scarlet language server (`scarlet lsp`).

Zed fetches the grammar from the repository at the `rev` pinned in `extension.toml`, so a grammar change reaches Zed only once that `rev` moves.

## Requirements

The extension runs the `scarlet` binary from `PATH`. If the binary is somewhere else, set its path in Zed's settings:

```json
{
  "lsp": {
    "scarlet": {
      "binary": {
        "path": "/path/to/scarlet"
      }
    }
  }
}
```

## Development

Run `zed: install dev extension` and pick `editors/zed`. Zed compiles the extension to WebAssembly, so you need a Rust toolchain with the `wasm32-wasip2` target.
