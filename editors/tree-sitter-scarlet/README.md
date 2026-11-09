# tree-sitter-scarlet

The tree-sitter grammar for Scarlet. Zed builds it from this directory. The
editor queries live in `editors/zed/languages/scarlet/`.

## Layout

- `grammar.js` holds the structural rules, copied from the reference parser
  in `crates/scarlet_syntax/src/parser`.
- `lexical.js` is generated. `cargo xtask gen-editor-syntax` writes it from
  the compiler's token tables, so don't edit it.
- `src/scanner.c` is the external scanner. It makes one token: a `-` at the
  start of a line, which the LR grammar can't express.
- `src/parser.c`, `src/grammar.json` and `src/node-types.json` are generated
  by `tree-sitter generate`. Anything that uses the grammar builds from these
  files, so they're committed.

## Development

```sh
bun install
bun run generate
bun run corpus
```

`bun run generate` rebuilds `src/` after you change `grammar.js`. `bun run
corpus` parses every `.sca` file in the repository and fails on any error
node. CI runs both. `cargo test -p xtask` fails when `lexical.js` or the
tmLanguage is out of date, so the grammar can't fall behind the compiler
without the build failing.
