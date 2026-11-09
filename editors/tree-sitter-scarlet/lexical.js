// GENERATED FILE - do not edit. Regenerate with: cargo xtask gen-editor-syntax
// These tables come from crates/scarlet_syntax (token/keywords.rs, token/kind.rs and the
// scanner's ESCAPES). grammar.js holds the hand-written structural rules and reads these
// tables, so the lexical layer can't drift from the compiler.
'use strict';

module.exports = {
  // Keyword::ALL. Every one is reserved, so none of them can parse as an identifier.
  keywords: ['fn', 'import', 'type', 'in', 'match', 'const', 'if', 'else', 'or', 'pub', 'opaque', 'as'],
  // token::is_name_start / is_name_continue.
  identifier: /[A-Za-z_][A-Za-z0-9_]*/,
  // Scanner::scan_number: hex (`0x`), binary (`0b`), or decimal with an optional
  // fraction. A `_` only counts between two digits of that radix, and a `_` or `.`
  // with no digit after it ends the token. Hex and binary come first so `0xFF`
  // isn't read as decimal `0`.
  number: /(0[xX][0-9A-Fa-f]+(_[0-9A-Fa-f]+)*|0[bB][01]+(_[01]+)*|\d+(_\d+)*(\.\d+(_\d+)*)?)/,
  // scanner::ESCAPES. Anything else after a backslash is an error.
  escape: /\\[ntr0"'\\$]/,
  // The contextual identifiers in << >> segment specs (parse_bin_size_spec).
  binSpecSized: ['size', 'bytes'],
  binSpecBare: ['binary', 'utf8'],
  // token::THEN is a keyword only after an `if` condition (parse_if_expression),
  // so it isn't in `keywords`.
  then: 'then',
};
