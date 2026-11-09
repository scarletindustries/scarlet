/// <reference types="tree-sitter-cli/dsl" />
// @ts-check

// The structural rules are copied from crates/scarlet_syntax/src/parser. The
// lexical layer (keywords, escapes and the shapes of literals) is generated
// into lexical.js by `cargo xtask gen-editor-syntax`. `bun run corpus` checks
// the grammar against every .sca file in the repo.

'use strict';

const lex = require('./lexical');

const KEYWORDS = new Set(lex.keywords);

// If the compiler renames a keyword, generation breaks here rather than the
// grammar quietly falling out of step.
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

// parse_field_list separates fields with commas or newlines. This grammar
// ignores whitespace, so here the comma is just optional.
function fieldSep1(rule) {
  return seq(rule, repeat(seq(optional(','), rule)), optional(','));
}

module.exports = grammar({
  name: 'scarlet',

  word: ($) => $.identifier,

  extras: ($) => [/[ \t\r\n]/, $.line_comment, $.block_comment, $.doc_comment],

  externals: ($) => [$._minus_line_start],

  conflicts: ($) => [
    // Statements that only the reference parser's same-line rules tell apart
    // from expressions: `x Foo = e` against `x` then `Foo = e`, and
    // `Foo(x) = e` against the call `Foo(x)`.
    [$.binding, $._primary_expression],
    // Patterns and expressions that look the same: a GLR fork inside `(...)`,
    // `[...]` or `<<...>>` stays alive until the `=` after the closing
    // delimiter, or the lack of one, picks destructuring or a literal.
    [$._pattern_atom, $._primary_expression],
    [$._primary_expression, $.ctor_pattern],
    [$.module_expression, $.ctor_pattern],
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
      ),

    relative_segment: () => seq(choice('.', '..'), '/'),

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
        // Left out entirely on an external or opaque handle type.
        optional(choice(seq('=', field('alias', $._type)), field('body', $.type_body))),
      )),

    opaque_modifier: () => kw('opaque'),

    type_parameters: ($) =>
      seq(token.immediate('('), commaSep1($.identifier), ')'),

    type_body: ($) =>
      seq(
        '{',
        choice(
          // `type Point { x Int  y Int }` is the shorthand for a type with
          // one constructor, picked when the first token is lowercase.
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
        optional(seq(field('module', $.identifier), '::')),
        field('name', $.type_identifier),
        optional(seq(token.immediate('('), commaSep1($._type), ')')),
      ),

    // A lowercase name in type position is a free type variable.
    type_variable: ($) => $.identifier,

    // Statements -------------------------------------------------------------

    // `pattern [Type] = value`: every binding. `x Int = 5` binds a name;
    // `(a, b) = t` and `io::Stat(files, ..) = walk(root)` take a value apart.
    binding: ($) =>
      seq(
        field('pattern', choice($.identifier, $.ctor_pattern, $.tuple_pattern)),
        optional(field('type', $._type)),
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

    // `left |> right` calls `right` with `left` as its first argument
    // (scarlet_syntax's desugar rewrites it before type checking).
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

    // Each end binds like `+` (`-5..5` is `(-5)..(5)`), and ranges don't chain.
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

    // A bare `label:` is short for `label: label` (the parser expands it
    // before anything downstream sees the argument).
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

    // `c::member`: a value, function or constructor of an imported module.
    module_expression: ($) =>
      seq(
        field('module', $.identifier),
        '::',
        field('member', choice($.identifier, $.type_identifier)),
      ),

    _primary_expression: ($) =>
      choice(
        $.module_expression,
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

    // Two or more elements. There's no `(x)` grouping, because blocks group.
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

    // Contextual identifiers, not keywords (parse_bin_size_spec matches by text).
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
    // reaches as far as an expression can, as in parse_expression.
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

    // A lambda body is one expression, or a block when it needs statements.
    function_expression: ($) =>
      prec.right(
        seq(kw('fn'), field('parameters', $.parameters), field('body', $._expression)),
      ),

    // Patterns ---------------------------------------------------------------

    _pattern: ($) => choice($.or_pattern, $.range_pattern, $._pattern_atom),

    or_pattern: ($) => prec.left(seq($._pattern, '|', $._pattern)),

    // Both bounds must be number literals, and the end is excluded, as in ranges.
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
        optional(seq(field('module', $.identifier), '::')),
        field('name', $.type_identifier),
        optional(seq(token.immediate('('), optional($.pattern_arguments), ')')),
      ),

    pattern_arguments: ($) => commaSep1($._pattern_argument),

    _pattern_argument: ($) => choice($.labeled_pattern, $._pattern, $.rest_pattern),

    labeled_pattern: ($) =>
      // `name:` alone binds the field to `name`.
      prec(1, seq(field('label', $.identifier), ':', optional(field('pattern', $._pattern)))),

    // `..` or `..rest` takes the remaining fields, elements or bytes.
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
    // Strings fit on one line, take either quote, and interpolate `${expr}`
    // and `$name`.
    //
    // `\u{HEX*}` is written by hand rather than generated, because
    // `lex.escape` is scanner::ESCAPES, where one source byte stands for one
    // byte, and this escape stands for up to 4. It allows any digit count, as
    // the scanner does: `scan_unicode_escape` reports a bad count or value as
    // a diagnostic, not a lex error. It's aliased to `escape_sequence` inline,
    // like `string_content` below, so `@string.escape` highlighting covers it.

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
    // identifier and type_identifier differ by the case of the first letter,
    // as in token::is_type_name. That's the only case rule in the language.

    identifier: () => /[a-z_][A-Za-z0-9_]*/,

    type_identifier: () => /[A-Z][A-Za-z0-9_]*/,

    number: () => token(lex.number),

    line_comment: () => token(seq('//', /[^\n\r]*/)),

    // `/**` opens a doc comment, but `/**/` does not. Comments don't nest.
    doc_comment: () => token(prec(1, /\/\*\*[^*]([^*]|\*+[^*/])*\*+\//)),

    block_comment: () => token(/\/\*([^*]|\*+[^*/])*\*+\//),
  },
});
