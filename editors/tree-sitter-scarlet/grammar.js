/// <reference types="tree-sitter-cli/dsl" />
// @ts-check

// Structural rules transcribed from crates/scarlet_syntax/src/parser; the
// lexical layer (keywords, escapes, literal shapes) is generated into
// lexical.js by `cargo xtask gen-editor-syntax`. `bun run corpus` checks the
// grammar against every .scrl file in the repo.

'use strict';

const lex = require('./lexical');

const KEYWORDS = new Set(lex.keywords);

// A keyword renamed in the compiler breaks generation here instead of
// silently drifting.
function kw(text) {
  if (!KEYWORDS.has(text)) throw new Error(`not a Scarlet keyword: ${text}`);
  return text;
}

function commaSep1(rule) {
  return seq(rule, repeat(seq(',', rule)), optional(','));
}

function commaSep(rule) {
  return optional(commaSep1(rule));
}

// parse_field_list separates fields with commas or newlines; whitespace-blind,
// the comma is simply optional.
function fieldSep1(rule) {
  return seq(rule, repeat(seq(optional(','), rule)), optional(','));
}

module.exports = grammar({
  name: 'scarlet',

  word: ($) => $.identifier,

  extras: ($) => [/[ \t\r\n]/, $.line_comment, $.block_comment, $.doc_comment],

  externals: ($) => [$._minus_line_start],

  conflicts: ($) => [
    // Statement forms that only the reference parser's same-line rules
    // separate from expressions: `x Foo = e` vs `x` then `Foo = e`, and
    // `Foo(x) = e` vs the call `Foo(x)`.
    [$.binding, $._primary_expression],
    [$.ctor_binding, $._primary_expression],
    // Pattern/expression twins: a GLR fork inside `(...)`, `[...]`, or
    // `<<...>>` stays alive until the `=` (or its absence) after the closing
    // delimiter decides destructuring vs literal.
    [$._pattern_atom, $._primary_expression],
    [$._primary_expression, $.ctor_pattern],
    [$._primary_expression, $._number_pattern],
    [$._primary_expression, $.negative_number],
    [$._primary_expression, $.rest_pattern],
    [$.array_expression, $.array_pattern],
    [$.binary_literal, $.binary_pattern],
    [$.binary_segment, $.binary_pattern_segment],
  ],

  rules: {
    program: ($) => repeat($._node),

    _node: ($) =>
      choice(
        $.import_declaration,
        $.function_declaration,
        $.type_declaration,
        $.const_declaration,
        $.binding,
        $.tuple_binding,
        $.typed_discard,
        $.ctor_binding,
        $.backpass,
        $._expression,
      ),

    // Declarations -----------------------------------------------------------

    attribute: ($) =>
      seq(
        '@',
        field('name', $.identifier),
        optional(seq(token.immediate('('), commaSep($.identifier), ')')),
      ),

    import_declaration: ($) =>
      seq(
        kw('import'),
        repeat($.relative_segment),
        field('module', $.identifier),
        repeat(seq('/', field('module', $.identifier))),
        optional(seq(kw('as'), field('alias', $.identifier))),
        optional($.import_items),
      ),

    relative_segment: () => seq(choice('.', '..'), '/'),

    import_items: ($) => seq('.', '{', commaSep1($.import_item), '}'),

    import_item: ($) =>
      seq(
        field('name', choice($.identifier, $.type_identifier)),
        optional(seq(kw('as'), field('alias', choice($.identifier, $.type_identifier)))),
      ),

    function_declaration: ($) =>
      prec.right(seq(
        repeat($.attribute),
        optional($.visibility_modifier),
        kw('fn'),
        field('name', $.identifier),
        field('parameters', $.parameters),
        optional(field('return_type', $._type)),
        // Absent on `@vm(...)` declarations.
        optional(field('body', $.block)),
      )),

    visibility_modifier: () => kw('pub'),

    parameters: ($) => seq('(', commaSep($.parameter), ')'),

    parameter: ($) =>
      seq(field('name', $.identifier), optional(field('type', $._type))),

    type_declaration: ($) =>
      prec.right(seq(
        repeat($.attribute),
        optional($.visibility_modifier),
        optional($.opaque_modifier),
        kw('type'),
        field('name', $.type_identifier),
        optional(field('parameters', $.type_parameters)),
        // Absent entirely on an external/opaque handle type.
        optional(choice(seq('=', field('alias', $._type)), field('body', $.type_body))),
      )),

    opaque_modifier: () => kw('opaque'),

    type_parameters: ($) =>
      seq(token.immediate('('), commaSep1($.identifier), ')'),

    type_body: ($) =>
      seq(
        '{',
        choice(
          // `type Point { x Int  y Int }`: single-constructor shorthand,
          // selected by a lowercase first token.
          fieldSep1($.constructor_field),
          repeat1($.constructor),
        ),
        '}',
      ),

    constructor: ($) =>
      seq(
        field('name', $.type_identifier),
        optional(seq(token.immediate('('), fieldSep1($.constructor_field), ')')),
      ),

    constructor_field: ($) =>
      seq(field('name', $.identifier), field('type', $._type)),

    const_declaration: ($) =>
      seq(
        optional($.visibility_modifier),
        kw('const'),
        field('name', choice($.identifier, $.type_identifier)),
        optional(field('type', $._type)),
        '=',
        field('value', $._expression),
      ),

    // Types ------------------------------------------------------------------

    _type: ($) => choice($.function_type, $.tuple_type, $.named_type, $.type_variable),

    function_type: ($) =>
      prec.right(
        seq(kw('fn'), '(', commaSep($._type), ')', optional(field('return_type', $._type))),
      ),

    tuple_type: ($) => seq('(', $._type, ',', commaSep1($._type), ')'),

    named_type: ($) =>
      seq(
        optional(seq(field('module', $.identifier), '.')),
        field('name', $.type_identifier),
        optional(seq(token.immediate('('), commaSep1($._type), ')')),
      ),

    // A lowercase name in type position is a free type variable.
    type_variable: ($) => $.identifier,

    // Statements -------------------------------------------------------------

    binding: ($) =>
      seq(
        field('name', $.identifier),
        optional(field('type', $._type)),
        '=',
        field('value', $._expression),
      ),

    tuple_binding: ($) =>
      seq('(', commaSep1($._pattern), ')', '=', field('value', $._expression)),

    // `Nil = println('x')`: assert-and-discard of a zero-arg type.
    typed_discard: ($) =>
      seq(field('type', $.type_identifier), '=', field('value', $._expression)),

    // `Stat(files, ..) = walk(root)`: single-arm match sugar, and
    // `io.Stat(files, ..) = walk(root)` for a constructor reached through its
    // module.
    ctor_binding: ($) =>
      seq(
        optional(seq(field('module', $.identifier), '.')),
        field('constructor', $.type_identifier),
        token.immediate('('),
        optional($.pattern_arguments),
        ')',
        '=',
        field('value', $._expression),
      ),

    // `a, b <- call(args)` desugars to `call(args, fn(a, b) { rest })`.
    backpass: ($) =>
      seq(
        field('binder', $.identifier),
        repeat(seq(',', field('binder', $.identifier))),
        '<-',
        field('call', $.call_expression),
      ),

    // Expressions ------------------------------------------------------------
    // Precedence ladder from the parser's PRECEDENCE table, loosest first:
    // or < |> < || < && < == != < comparisons < .. < + - < * / % < unary <
    // postfix.

    _expression: ($) =>
      choice(
        $.or_expression,
        $.pipe_expression,
        $.binary_expression,
        $.range_expression,
        $.unary_expression,
        $._postfix_expression,
      ),

    or_expression: ($) =>
      prec.right(
        1,
        seq(
          field('left', $._expression),
          kw('or'),
          optional(seq(field('binder', $.identifier), '->')),
          field('right', $._expression),
        ),
      ),

    // `left |> right`: sugar for calling `right` with `left` prepended as its
    // first argument (crate::desugar rewrites it before type checking).
    pipe_expression: ($) =>
      prec.left(
        2,
        seq(field('left', $._expression), '|>', field('right', $._expression)),
      ),

    binary_expression: ($) => {
      const table = [
        [3, '||'],
        [4, '&&'],
        [5, choice('==', '!=')],
        [6, choice('<', '>', '<=', '>=')],
        [8, choice('+', '-')],
        [9, choice('*', '/', '%')],
      ];
      return choice(
        ...table.map(([precedence, operator]) =>
          prec.left(
            precedence,
            seq(
              field('left', $._expression),
              field('operator', operator),
              field('right', $._expression),
            ),
          ),
        ),
      );
    },

    // Endpoints are additive-precedence (`-5..5` is `(-5)..(5)`); no chaining.
    range_expression: ($) =>
      prec.left(
        7,
        seq(field('start', $._expression), '..', field('end', $._expression)),
      ),

    // `_minus_line_start` is the parser's fresh-line-minus rule: it starts a
    // new statement or arm, so unary and negative-literal rules accept it but
    // binary subtraction does not.
    unary_expression: ($) =>
      prec(
        10,
        seq(
          field('operator', choice('!', '-', alias($._minus_line_start, '-'))),
          field('operand', $._expression),
        ),
      ),

    _postfix_expression: ($) =>
      choice(
        $.call_expression,
        $.index_expression,
        $.field_expression,
        $._primary_expression,
      ),

    // token.immediate: a `(` or `[` on a fresh line starts a new expression
    // instead of chaining (parser "P7").
    call_expression: ($) =>
      prec(
        11,
        seq(
          field('function', $._postfix_expression),
          token.immediate('('),
          optional($.arguments),
          ')',
        ),
      ),

    arguments: ($) => commaSep1($._argument),

    _argument: ($) => choice($.spread_argument, $.labeled_argument, $._expression),

    spread_argument: ($) => seq('..', $._expression),

    // A bare `label:` is punning sugar for `label: label` (crate::parser
    // desugars it before anything downstream sees the argument).
    labeled_argument: ($) =>
      prec(1, seq(field('label', $.identifier), ':', optional(field('value', $._expression)))),

    index_expression: ($) =>
      prec(
        10,
        seq(
          field('value', $._postfix_expression),
          token.immediate('['),
          field('index', $._expression),
          ']',
        ),
      ),

    field_expression: ($) =>
      prec(
        10,
        seq(
          field('value', $._postfix_expression),
          '.',
          // A number field is a tuple index; `x.0.1` arrives as the single
          // number token `0.1`, exactly as the scanner fuses it.
          field('field', choice($.identifier, $.type_identifier, $.number)),
        ),
      ),

    _primary_expression: ($) =>
      choice(
        $.number,
        $.string,
        $.identifier,
        $.type_identifier,
        $.tuple_expression,
        $.block,
        $.array_expression,
        $.binary_literal,
        $.if_expression,
        $.match_expression,
        $.function_expression,
      ),

    // 2+ elements; `(x)` grouping does not exist (blocks group instead).
    tuple_expression: ($) =>
      seq('(', $._expression, ',', commaSep1($._expression), ')'),

    block: ($) => seq('{', repeat($._node), '}'),

    array_expression: ($) =>
      seq('[', commaSep(choice($.spread_argument, $._expression)), ']'),

    binary_literal: ($) => seq('<<', commaSep($.binary_segment), '>>'),

    binary_segment: ($) =>
      seq(
        field('value', choice($._expression, $.rest_pattern)),
        optional(seq(':', field('spec', $.binary_spec))),
      ),

    // Contextual identifiers, not keywords (parse_bin_spec matches by text).
    binary_spec: ($) =>
      choice(
        $.number,
        seq(
          field('name', alias(choice(...lex.binSpecSized), $.spec_identifier)),
          '(',
          $._expression,
          ')',
        ),
        field('name', alias(choice(...lex.binSpecBare), $.spec_identifier)),
      ),

    // `if c then a else b`, or a block after the condition. Either branch
    // runs on as far as an expression can, as parse_expression does.
    if_expression: ($) =>
      prec.right(
        seq(
          kw('if'),
          field('condition', $._expression),
          choice(
            seq(lex.then, field('consequence', $._expression)),
            field('consequence', $.block),
          ),
          kw('else'),
          field('alternative', $._expression),
        ),
      ),

    match_expression: ($) =>
      seq(kw('match'), field('value', $._expression), '{', repeat($.match_arm), '}'),

    match_arm: ($) =>
      seq(
        optional('|'),
        field('pattern', $._pattern),
        optional(seq(kw('if'), field('guard', $._expression))),
        '->',
        field('value', $._expression),
      ),

    // A lambda body is a single expression; a block when it needs statements.
    function_expression: ($) =>
      prec.right(
        seq(kw('fn'), field('parameters', $.parameters), field('body', $._expression)),
      ),

    // Patterns ---------------------------------------------------------------

    _pattern: ($) => choice($.or_pattern, $.range_pattern, $._pattern_atom),

    or_pattern: ($) => prec.left(seq($._pattern, '|', $._pattern)),

    // Both bounds must be number literals; end-exclusive like ranges.
    range_pattern: ($) => seq($._number_pattern, '..', $._number_pattern),

    _number_pattern: ($) => choice($.number, $.negative_number),

    negative_number: ($) =>
      seq(choice('-', alias($._minus_line_start, '-')), $.number),

    _pattern_atom: ($) =>
      choice(
        $.identifier,
        $.ctor_pattern,
        $.number,
        $.negative_number,
        $.string,
        $.tuple_pattern,
        $.array_pattern,
        $.binary_pattern,
      ),

    ctor_pattern: ($) =>
      seq(
        optional(seq(field('module', $.identifier), '.')),
        field('name', $.type_identifier),
        optional(seq(token.immediate('('), optional($.pattern_arguments), ')')),
      ),

    pattern_arguments: ($) => commaSep1($._pattern_argument),

    _pattern_argument: ($) => choice($.labeled_pattern, $._pattern, $.rest_pattern),

    labeled_pattern: ($) =>
      prec(1, seq(field('label', $.identifier), ':', field('pattern', $._pattern))),

    // `..` / `..rest`: remaining fields, elements, or bytes.
    rest_pattern: ($) => prec.right(seq('..', optional(field('binder', $.identifier)))),

    tuple_pattern: ($) => seq('(', $._pattern, ',', commaSep1($._pattern), ')'),

    array_pattern: ($) =>
      seq('[', commaSep(choice($.rest_pattern, $._pattern)), ']'),

    binary_pattern: ($) => seq('<<', commaSep($.binary_pattern_segment), '>>'),

    binary_pattern_segment: ($) =>
      seq(
        field('value', choice($._pattern_atom, $.rest_pattern)),
        optional(seq(':', field('spec', $.binary_spec))),
      ),

    // Strings ----------------------------------------------------------------
    // Single-line; both quotes; `${expr}` and `$name` interpolation.
    //
    // `\u{HEX*}` is hand-written here rather than generated: `lex.escape` is
    // scanner::ESCAPES, one source byte to one denoted byte, and this one
    // denotes up to 4. As permissive on digit count as the scanner itself —
    // `scan_unicode_escape` recovers a bad count or an out-of-range value
    // with a diagnostic, not a lex error, so the shape still parses. Aliased
    // to `escape_sequence` inline, the same way `string_content` below is,
    // so `@string.escape` highlighting covers it with no wrapper node.

    string: ($) =>
      choice(
        seq(
          "'",
          repeat(
            choice(
              $.escape_sequence,
              alias(token.immediate(seq('\\u{', /[0-9A-Fa-f]*/, '}')), $.escape_sequence),
              $.interpolation,
              $.short_interpolation,
              alias(token.immediate(prec(1, /[^'\\$\n]+/)), $.string_content),
              alias(token.immediate('$'), $.string_content),
            ),
          ),
          "'",
        ),
        seq(
          '"',
          repeat(
            choice(
              $.escape_sequence,
              alias(token.immediate(seq('\\u{', /[0-9A-Fa-f]*/, '}')), $.escape_sequence),
              $.interpolation,
              $.short_interpolation,
              alias(token.immediate(prec(1, /[^"\\$\n]+/)), $.string_content),
              alias(token.immediate('$'), $.string_content),
            ),
          ),
          '"',
        ),
      ),

    escape_sequence: () => token.immediate(lex.escape),

    interpolation: ($) => seq(token.immediate('${'), $._expression, '}'),

    short_interpolation: ($) => token.immediate(seq('$', lex.identifier)),

    // Terminals --------------------------------------------------------------
    // identifier and type_identifier are disjoint by first-letter case,
    // modelling token::is_type_name — the language's only case rule.

    identifier: () => /[a-z_][A-Za-z0-9_]*/,

    type_identifier: () => /[A-Z][A-Za-z0-9_]*/,

    number: () => token(lex.number),

    line_comment: () => token(seq('//', /[^\n\r]*/)),

    // `/**` opens a doc comment, but `/**/` does not. Non-nesting.
    doc_comment: () => token(prec(1, /\/\*\*[^*]([^*]|\*+[^*/])*\*+\//)),

    block_comment: () => token(/\/\*([^*]|\*+[^*/])*\*+\//),
  },
});
