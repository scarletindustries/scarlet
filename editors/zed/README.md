<img width="96" src="logo.png" />

# Scarlet for Zed

[Scarlet](https://scarlet.industries) language support for Zed.

- Registers the Scarlet language for `.sca` files: comments, brackets, tab indentation.
- Diagnostics, completions, and formatting from the Scarlet language server (`scarlet lsp`).

Zed syntax highlighting requires a tree-sitter grammar. Scarlet has none. Language server features do not require one.

## Requirements

The extension runs the `scarlet` binary from `PATH`. Set the path in Zed settings when the binary is elsewhere:

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

Run `zed: install dev extension` and select `editors/zed`. Zed compiles the extension to WebAssembly. The build requires a Rust toolchain with the `wasm32-wasip2` target.
