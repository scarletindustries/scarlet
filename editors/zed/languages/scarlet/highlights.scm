; Base identifiers; later patterns override for specific roles.
(identifier) @variable
(type_identifier) @type

; Comments
(line_comment) @comment
(block_comment) @comment
(doc_comment) @comment.doc

; Literals
(number) @number
(string) @string
(string_content) @string
(escape_sequence) @string.escape
(short_interpolation) @variable.special
(interpolation
  "${" @punctuation.special
  "}" @punctuation.special)

; Keywords (Keyword::ALL minus `in`, which is reserved but consumed by no
; rule, so it has no token node), and `then`, a keyword only after an `if`
; condition
[
  "fn"
  "import"
  "type"
  "match"
  "const"
  "if"
  "else"
  "then"
  "or"
  "as"
] @keyword
(visibility_modifier) @keyword
(opaque_modifier) @keyword

; Prelude constructors that read as language constants
((type_identifier) @boolean
  (#match? @boolean "^(True|False)$"))
((type_identifier) @constant
  (#eq? @constant "Nil"))

; Types
(named_type
  name: (type_identifier) @type)
(type_variable
  (identifier) @type)
(binary_spec
  name: (spec_identifier) @type)

; Constructors
(constructor
  name: (type_identifier) @constructor)
(ctor_pattern
  name: (type_identifier) @constructor)
(ctor_binding
  constructor: (type_identifier) @constructor)
(call_expression
  function: (type_identifier) @constructor)
(call_expression
  function: (field_expression
    field: (type_identifier) @constructor))

; Modules
(import_declaration
  module: (identifier) @namespace)
(import_declaration
  alias: (identifier) @namespace)
(named_type
  module: (identifier) @namespace)
(ctor_pattern
  module: (identifier) @namespace)

; Record fields and labels
(constructor_field
  name: (identifier) @property)
(field_expression
  field: (identifier) @property)
(labeled_argument
  label: (identifier) @property)
(labeled_pattern
  label: (identifier) @property)

; Functions
(function_declaration
  name: (identifier) @function)
(parameter
  name: (identifier) @variable.parameter)
(call_expression
  function: (identifier) @function)
(call_expression
  function: (field_expression
    field: (identifier) @function))

; Attributes: @vm(...), @test, ...
(attribute
  "@" @attribute
  name: (identifier) @attribute)

; Operators
[
  "->"
  "<-"
  ".."
  "=="
  "!="
  "<"
  ">"
  "<="
  ">="
  "&&"
  "||"
  "!"
  "|"
  "+"
  "-"
  "*"
  "/"
  "%"
  "="
] @operator

; Punctuation
[
  "("
  ")"
  "["
  "]"
  "{"
  "}"
] @punctuation.bracket
[
  "<<"
  ">>"
] @punctuation.special
[
  ","
  ":"
  "."
] @punctuation.delimiter
