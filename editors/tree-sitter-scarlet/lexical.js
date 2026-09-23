// GENERATED FILE - do not edit. Regenerate with: cargo xtask gen-editor-syntax
// Source of truth: crates/scarlet_syntax (token/keywords.rs, token/kind.rs,
// scanner ESCAPES). grammar.js holds the hand-written structural rules and
// consumes these tables so the lexical layer cannot drift from the compiler.
'use strict';

module.exports = {
  // Keyword::ALL. Every one is reserved: none may parse as an identifier.
  keywords: ['fn', 'import', 'type', 'in', 'match', 'const', 'if', 'else', 'or', 'pub', 'opaque', 'as'],
  // token::is_name_start / is_name_continue.
  identifier: /[A-Za-z_][A-Za-z0-9_]*/,
  // Scanner::scan_number: hex (`0x`), binary (`0b`), or decimal with an
  // optional fraction. `_` sits between two digits of that radix. A `_` or
  // `.` with no digit after it ends the token. Hex/bin are listed first so
  // `0xFF` is not consumed as decimal `0`.
  number: /(0[xX][0-9A-Fa-f]+(_[0-9A-Fa-f]+)*|0[bB][01]+(_[01]+)*|\d+(_\d+)*(\.\d+(_\d+)*)?)/,
  // scanner::ESCAPES; anything else after a backslash is an error.
  escape: /\\[ntr0"'\\$]/,
  // Contextual identifiers in << >> segment specs (parse_bin_spec).
  binSpecSized: ['size', 'bytes'],
  binSpecBare: ['binary', 'utf8'],
  // token::THEN: a keyword only after an `if` condition
  // (parse_if_expression), so it is not in `keywords`.
  then: 'then',
};
