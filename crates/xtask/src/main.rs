//! Workspace tooling. `cargo xtask gen-editor-syntax` writes the lexical layer of both editor
//! grammars from the compiler's own token tables. With `--check` it writes nothing and fails if
//! either file is stale.
//!
//! - It writes all of `scarlet.tmLanguage.json` at the repo root. The VSCode extension links to
//!   it with a symlink.
//! - It writes `editors/tree-sitter-scarlet/lexical.js`, which the tree-sitter grammar reads.
//!   That grammar's structural rules are hand-written in `grammar.js`.
//!
//! Keywords come from `Keyword::ALL`, operator spellings from `Kind::fixed_spelling_kinds` and
//! string escapes from `scanner::ESCAPES`, so the editors can't drift from the scanner. Because
//! the matches below are exhaustive, a new keyword or token kind has to be classified here
//! before the workspace compiles again. `cargo test -p xtask` fails until the checked-in files
//! are regenerated.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use scarlet_syntax::scanner::ESCAPES;
use scarlet_syntax::token::{Keyword, Kind, THEN};
use serde_json::{Value, json};

/// The contextual identifiers a `<< expr:spec >>` segment accepts as its spec. They aren't
/// keywords. `parse_bin_size_spec` in `parser/mod.rs` matches them by text, and its error
/// message lists them.
const BIN_SPEC_SIZED: &[&str] = &["size", "bytes"];
const BIN_SPEC_BARE: &[&str] = &["binary", "utf8"];

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("gen-editor-syntax") => {
            let check = args.iter().any(|a| a == "--check");
            match run(check) {
                Ok(()) => ExitCode::SUCCESS,
                Err(message) => {
                    eprintln!("{message}");
                    ExitCode::FAILURE
                }
            }
        }
        _ => {
            eprintln!("usage: cargo xtask gen-editor-syntax [--check]");
            ExitCode::FAILURE
        }
    }
}

enum GenError {
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    Stale(Vec<String>),
}

impl std::fmt::Display for GenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GenError::Io { path, source } => write!(f, "{}: {source}", path.display()),
            GenError::Stale(paths) => write!(
                f,
                "these generated files are stale, so run `cargo xtask gen-editor-syntax`:\n  {}",
                paths.join("\n  ")
            ),
        }
    }
}

fn run(check: bool) -> Result<(), GenError> {
    let mut stale = Vec::new();
    for (path, contents) in outputs() {
        if check {
            let on_disk = std::fs::read_to_string(&path).unwrap_or_default();
            if on_disk != contents {
                stale.push(path.display().to_string());
            }
        } else {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| GenError::Io {
                    path: parent.to_path_buf(),
                    source: e,
                })?;
            }
            std::fs::write(&path, contents).map_err(|e| GenError::Io {
                path: path.clone(),
                source: e,
            })?;
            println!("wrote {}", path.display());
        }
    }
    if stale.is_empty() {
        Ok(())
    } else {
        Err(GenError::Stale(stale))
    }
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("xtask lives at <root>/crates/xtask")
        .to_path_buf()
}

fn outputs() -> Vec<(PathBuf, String)> {
    let root = repo_root();
    vec![
        (root.join("scarlet.tmLanguage.json"), tm_language()),
        (
            root.join("editors/tree-sitter-scarlet/lexical.js"),
            lexical_js(),
        ),
    ]
}

// ---------------------------------------------------------------------------
// Token classification
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum KeywordGroup {
    Control,
    Import,
    Modifier,
    Declaration,
}

fn keyword_group(kw: Keyword) -> KeywordGroup {
    match kw {
        Keyword::If | Keyword::Else | Keyword::Match | Keyword::Or | Keyword::In => {
            KeywordGroup::Control
        }
        Keyword::Import | Keyword::As => KeywordGroup::Import,
        Keyword::Pub | Keyword::Opaque => KeywordGroup::Modifier,
        Keyword::Fn | Keyword::Type | Keyword::Const => KeywordGroup::Declaration,
    }
}

fn keyword_alternation(group: KeywordGroup) -> String {
    Keyword::ALL
        .into_iter()
        .filter(|kw| keyword_group(*kw) == group)
        .map(Keyword::text)
        .collect::<Vec<_>>()
        .join("|")
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OpGroup {
    /// `++`, `--`, `;` and `?`. The scanner makes tokens of them but no parser rule takes
    /// them, so they're always a parse error.
    Invalid,
    Arrow,
    Backpass,
    Range,
    Comparison,
    Logical,
    Pipe,
    PatternAlternation,
    Arithmetic,
    Assignment,
    Attribute,
}

enum TokenClass {
    Keyword,
    Operator(OpGroup),
    /// `<<` and `>>`, which the `#binary` region rule handles.
    BinaryDelimiter,
    Punctuation(&'static str),
}

fn classify(kind: &Kind) -> TokenClass {
    match kind {
        // These carry a payload or mark a position, so they aren't in the fixed-spelling list.
        Kind::Eof
        | Kind::LineEnd
        | Kind::Error(_)
        | Kind::Identifier(_)
        | Kind::LiteralNumber(_)
        | Kind::LiteralString(_)
        | Kind::InterpStringStart
        | Kind::InterpStringPart(_)
        | Kind::InterpStringEnd => unreachable!("{kind:?} has no fixed spelling"),
        Kind::Keyword(_) => TokenClass::Keyword,
        Kind::PuncArrow => TokenClass::Operator(OpGroup::Arrow),
        Kind::PuncBackArrow => TokenClass::Operator(OpGroup::Backpass),
        Kind::PuncDotdot => TokenClass::Operator(OpGroup::Range),
        Kind::PuncEqualsComparator
        | Kind::PuncNotEqual
        | Kind::PuncLte
        | Kind::PuncGte
        | Kind::PuncLt
        | Kind::PuncGt => TokenClass::Operator(OpGroup::Comparison),
        Kind::LogicalAnd | Kind::LogicalOr | Kind::PuncExclamationMark => {
            TokenClass::Operator(OpGroup::Logical)
        }
        Kind::BitwiseOr => TokenClass::Operator(OpGroup::PatternAlternation),
        Kind::PuncPipe => TokenClass::Operator(OpGroup::Pipe),
        Kind::PuncPlus | Kind::PuncMinus | Kind::PuncMul | Kind::PuncDiv | Kind::PuncMod => {
            TokenClass::Operator(OpGroup::Arithmetic)
        }
        Kind::PuncEquals => TokenClass::Operator(OpGroup::Assignment),
        Kind::PuncAt => TokenClass::Operator(OpGroup::Attribute),
        Kind::BinOpen | Kind::BinClose => TokenClass::BinaryDelimiter,
        Kind::PuncPlusplus
        | Kind::PuncMinusminus
        | Kind::PuncSemicolon
        | Kind::PuncQuestionMark => TokenClass::Operator(OpGroup::Invalid),
        Kind::PuncComma => TokenClass::Punctuation("punctuation.separator.comma.sca"),
        Kind::PuncColon => TokenClass::Punctuation("punctuation.separator.colon.sca"),
        Kind::PuncDot => TokenClass::Punctuation("punctuation.accessor.sca"),
        Kind::PuncColonColon => TokenClass::Punctuation("punctuation.accessor.module.sca"),
        Kind::PuncOpenParen => TokenClass::Punctuation("punctuation.section.parens.begin.sca"),
        Kind::PuncCloseParen => TokenClass::Punctuation("punctuation.section.parens.end.sca"),
        Kind::PuncOpenBrace => TokenClass::Punctuation("punctuation.section.braces.begin.sca"),
        Kind::PuncCloseBrace => TokenClass::Punctuation("punctuation.section.braces.end.sca"),
        Kind::PuncOpenBracket => TokenClass::Punctuation("punctuation.section.brackets.begin.sca"),
        Kind::PuncCloseBracket => TokenClass::Punctuation("punctuation.section.brackets.end.sca"),
    }
}

fn op_group_scope(group: OpGroup) -> &'static str {
    match group {
        OpGroup::Invalid => "invalid.illegal.unsupported-operator.sca",
        OpGroup::Arrow => "keyword.operator.arrow.sca",
        OpGroup::Backpass => "keyword.operator.backpass.sca",
        // The scope name is historical. `..` is both the range and the spread operator.
        OpGroup::Range => "keyword.operator.spread.sca",
        OpGroup::Comparison => "keyword.operator.comparison.sca",
        OpGroup::Logical => "keyword.operator.logical.sca",
        OpGroup::Pipe => "keyword.operator.pipe.sca",
        OpGroup::PatternAlternation => "keyword.operator.pattern.sca",
        OpGroup::Arithmetic => "keyword.operator.arithmetic.sca",
        OpGroup::Assignment => "keyword.operator.assignment.sca",
        OpGroup::Attribute => "keyword.operator.attribute.sca",
    }
}

fn regex_escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if "\\^$.|?*+()[]{}/".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Builds one alternation regex per operator group, longest spelling first, so `<=` wins over
/// `<` in the same rule. The order of the groups matters for the same reason. `Invalid` comes
/// before `Arithmetic` so `++` wins over `+`. `Logical` and `Pipe` come before
/// `PatternAlternation` so `||` and `|>` win over `|`. `Comparison` comes before `Assignment`
/// so `==` wins over `=`.
fn operator_rules() -> Vec<Value> {
    const ORDER: &[OpGroup] = &[
        OpGroup::Invalid,
        OpGroup::Arrow,
        OpGroup::Backpass,
        OpGroup::Range,
        OpGroup::Comparison,
        OpGroup::Logical,
        OpGroup::Pipe,
        OpGroup::PatternAlternation,
        OpGroup::Arithmetic,
        OpGroup::Assignment,
        OpGroup::Attribute,
    ];
    let kinds = Kind::fixed_spelling_kinds();
    ORDER
        .iter()
        .map(|&group| {
            let mut spellings: Vec<String> = kinds
                .iter()
                .filter(|k| matches!(classify(k), TokenClass::Operator(g) if g == group))
                .map(Kind::to_string)
                .collect();
            spellings.sort_by(|a, b| b.len().cmp(&a.len()).then(a.cmp(b)));
            let alternation = spellings
                .iter()
                .map(|s| regex_escape(s))
                .collect::<Vec<_>>()
                .join("|");
            json!({
                "name": op_group_scope(group),
                "match": format!("({alternation})"),
            })
        })
        .collect()
}

fn punctuation_rules() -> Vec<Value> {
    Kind::fixed_spelling_kinds()
        .iter()
        .filter_map(|kind| match classify(kind) {
            TokenClass::Punctuation(scope) => Some(json!({
                "name": scope,
                "match": regex_escape(&kind.to_string()),
            })),
            // Other sections handle these. The keyword alternations take keywords,
            // `operator_rules` takes operators and the `#binary` region rule takes `<<` and
            // `>>`. A new class has to pick its section.
            TokenClass::Keyword | TokenClass::Operator(_) | TokenClass::BinaryDelimiter => None,
        })
        .collect()
}

/// The regex character class of every character allowed after `\` in a string, taken
/// straight from the scanner's escape table.
fn escape_char_class() -> String {
    let mut class = String::new();
    for &(source, _) in ESCAPES {
        match source {
            b'\\' => class.push_str("\\\\"),
            b']' => class.push_str("\\]"),
            c => class.push(c as char),
        }
    }
    class
}

// ---------------------------------------------------------------------------
// tmLanguage (VSCode)
// ---------------------------------------------------------------------------

fn tm_language() -> String {
    let k = Keyword::text;
    let escape_rule = json!({
        "name": "constant.character.escape.sca",
        "match": format!("\\\\[{}]", escape_char_class()),
    });
    let bad_escape_rule = json!({
        "name": "invalid.illegal.escape.sca",
        "match": "\\\\.",
    });
    let interpolation_region = json!({
        "name": "meta.interpolation.sca",
        "begin": "\\$\\{",
        "end": "\\}",
        "beginCaptures": { "0": { "name": "punctuation.section.interpolation.begin.sca" } },
        "endCaptures": { "0": { "name": "punctuation.section.interpolation.end.sca" } },
        "patterns": [{ "include": "#interpolation-expression" }]
    });
    let interpolation_shorthand = json!({
        "name": "variable.other.interpolation.sca",
        "match": "\\$[a-zA-Z_][a-zA-Z0-9_]*",
    });
    let string_body = |quote: &str| -> Value {
        json!({
            "name": format!("string.quoted.{}.sca", if quote == "'" { "single" } else { "double" }),
            "begin": quote,
            "end": format!("{quote}|$"),
            "patterns": [
                escape_rule,
                bad_escape_rule,
                interpolation_region,
                interpolation_shorthand,
            ]
        })
    };

    let sized_specs = BIN_SPEC_SIZED.join("|");
    let bare_specs = BIN_SPEC_BARE.join("|");

    let grammar = json!({
        "name": "Scarlet",
        "scopeName": "source.sca",
        "patterns": [
            { "include": "#comments" },
            { "include": "#attributes" },
            { "include": "#strings" },
            { "include": "#numbers" },
            { "include": "#binary" },
            { "include": "#import-statement" },
            { "include": "#type-definition" },
            { "include": "#function-definition" },
            { "include": "#const-definition" },
            { "include": "#variable-with-type" },
            { "include": "#backpass-binding" },
            { "include": "#keywords" },
            { "include": "#constants" },
            { "include": "#module-qualified-access" },
            { "include": "#constructor-call" },
            { "include": "#function-call" },
            { "include": "#labeled-argument" },
            { "include": "#types" },
            { "include": "#operators" },
            { "include": "#punctuation" }
        ],
        "repository": {
            "attributes": {
                "patterns": [
                    {
                        "name": "meta.attribute.sca",
                        "match": "(@)([a-zA-Z_][a-zA-Z0-9_]*)(\\()?([a-zA-Z_][a-zA-Z0-9_]*)?(\\))?",
                        "captures": {
                            "1": { "name": "punctuation.definition.attribute.sca" },
                            "2": { "name": "entity.name.function.decorator.sca" },
                            "3": { "name": "punctuation.brackets.round.sca" },
                            "4": { "name": "string.unquoted.attribute.sca" },
                            "5": { "name": "punctuation.brackets.round.sca" }
                        }
                    }
                ]
            },
            "comments": {
                "patterns": [
                    {
                        "name": "comment.line.double-slash.sca",
                        "match": "//.*$"
                    },
                    {
                        "comment": "`/**` opens a doc comment, but `/**/` does not.",
                        "name": "comment.block.documentation.sca",
                        "begin": "/\\*\\*(?!/)",
                        "end": "\\*/"
                    },
                    {
                        "name": "comment.block.sca",
                        "begin": "/\\*",
                        "end": "\\*/"
                    }
                ]
            },
            "strings": {
                "patterns": [string_body("'"), string_body("\"")]
            },
            "numbers": {
                "comment": "Mirrors Scanner::scan_number. Hex (`0x`) and binary (`0b`) come first so they aren't split into `0` and an identifier. `_` separates digits only when it sits between two digits of that radix.",
                "patterns": [
                    {
                        "name": "constant.numeric.hex.sca",
                        "match": "\\b0[xX][0-9A-Fa-f]+(?:_[0-9A-Fa-f]+)*\\b"
                    },
                    {
                        "name": "constant.numeric.binary.sca",
                        "match": "\\b0[bB][01]+(?:_[01]+)*\\b"
                    },
                    {
                        "name": "constant.numeric.float.sca",
                        "match": "\\b\\d+(?:_\\d+)*\\.\\d+(?:_\\d+)*\\b"
                    },
                    {
                        "name": "constant.numeric.integer.sca",
                        "match": "\\b\\d+(?:_\\d+)*\\b"
                    }
                ]
            },
            "binary": {
                "comment": "A bit-string literal or pattern, like <<expr:spec, ..rest>>. The spec names are contextual identifiers, not keywords.",
                "begin": "<<",
                "end": ">>",
                "beginCaptures": { "0": { "name": "punctuation.definition.binary.begin.sca" } },
                "endCaptures": { "0": { "name": "punctuation.definition.binary.end.sca" } },
                "patterns": [
                    { "include": "#comments" },
                    { "include": "#strings" },
                    { "include": "#numbers" },
                    {
                        "match": format!("(:)\\s*({sized_specs})(?=\\s*\\()"),
                        "captures": {
                            "1": { "name": "punctuation.separator.colon.sca" },
                            "2": { "name": "storage.type.binary-spec.sca" }
                        }
                    },
                    {
                        "match": format!("(:)\\s*({bare_specs})\\b"),
                        "captures": {
                            "1": { "name": "punctuation.separator.colon.sca" },
                            "2": { "name": "storage.type.binary-spec.sca" }
                        }
                    },
                    {
                        "name": "keyword.operator.spread.sca",
                        "match": "\\.\\."
                    },
                    { "include": "#constants" },
                    { "include": "#module-qualified-access" },
                    { "include": "#constructor-call" },
                    { "include": "#function-call" },
                    { "include": "#types" },
                    { "include": "#operators" },
                    {
                        "name": "variable.other.sca",
                        "match": "\\b[a-z_][a-zA-Z0-9_]*\\b"
                    },
                    { "include": "#punctuation" }
                ]
            },
            "interpolation-expression": {
                "patterns": [
                    { "include": "#strings" },
                    { "include": "#numbers" },
                    { "include": "#binary" },
                    { "include": "#keywords" },
                    { "include": "#constants" },
                    { "include": "#module-qualified-access" },
                    { "include": "#constructor-call" },
                    { "include": "#function-call" },
                    { "include": "#labeled-argument" },
                    { "include": "#types" },
                    { "include": "#operators" },
                    {
                        "name": "variable.other.sca",
                        "match": "\\b[a-z_][a-zA-Z0-9_]*\\b"
                    },
                    { "include": "#punctuation" }
                ]
            },
            "import-statement": {
                "begin": format!("\\b({})\\b", k(Keyword::Import)),
                "beginCaptures": {
                    "1": { "name": "keyword.control.import.sca" }
                },
                "end": "(?=$|\\n)",
                "patterns": [
                    {
                        "name": "keyword.control.import.sca",
                        "match": format!("\\b({})\\b", k(Keyword::As))
                    },
                    {
                        "name": "punctuation.separator.path.sca",
                        "match": "/"
                    },
                    {
                        "name": "entity.name.type.sca",
                        "match": "\\b[A-Z][a-zA-Z0-9_]*\\b"
                    },
                    {
                        "name": "entity.name.namespace.sca",
                        "match": "\\b[a-z_][a-zA-Z0-9_]*\\b"
                    },
                    {
                        "name": "punctuation.accessor.sca",
                        "match": "\\."
                    },
                    { "include": "#punctuation" }
                ]
            },
            "type-definition": {
                "patterns": [
                    {
                        "comment": "type alias: type Name(params) = Type",
                        "match": format!(
                            "\\b({pub}\\s+)?({opaque}\\s+)?({type_})\\s+([A-Z][a-zA-Z0-9_]*)(?:\\s*\\(([^)]*)\\))?\\s*(=)",
                            pub = k(Keyword::Pub), opaque = k(Keyword::Opaque), type_ = k(Keyword::Type)
                        ),
                        "captures": {
                            "1": { "name": "storage.modifier.visibility.sca" },
                            "2": { "name": "storage.modifier.visibility.sca" },
                            "3": { "name": "keyword.other.type.sca" },
                            "4": { "name": "entity.name.type.sca" },
                            "5": {
                                "patterns": [
                                    {
                                        "name": "variable.other.type.sca",
                                        "match": "\\b[a-z_][a-zA-Z0-9_]*\\b"
                                    }
                                ]
                            },
                            "6": { "name": "keyword.operator.assignment.sca" }
                        }
                    },
                    {
                        "comment": "external type: type Name(params) with no body, ends at EOL",
                        "match": format!(
                            "\\b({pub}\\s+)?({opaque}\\s+)?({type_})\\s+([A-Z][a-zA-Z0-9_]*)(?:\\s*\\(([^)]*)\\))?\\s*$",
                            pub = k(Keyword::Pub), opaque = k(Keyword::Opaque), type_ = k(Keyword::Type)
                        ),
                        "captures": {
                            "1": { "name": "storage.modifier.visibility.sca" },
                            "2": { "name": "storage.modifier.visibility.sca" },
                            "3": { "name": "keyword.other.type.sca" },
                            "4": { "name": "entity.name.type.sca" },
                            "5": {
                                "patterns": [
                                    {
                                        "name": "variable.other.type.sca",
                                        "match": "\\b[a-z_][a-zA-Z0-9_]*\\b"
                                    }
                                ]
                            }
                        }
                    },
                    {
                        "comment": "type definition: type Name(params) { ... }",
                        "begin": format!(
                            "\\b({pub}\\s+)?({opaque}\\s+)?({type_})\\s+([A-Z][a-zA-Z0-9_]*)(?:\\s*(\\())?",
                            pub = k(Keyword::Pub), opaque = k(Keyword::Opaque), type_ = k(Keyword::Type)
                        ),
                        "beginCaptures": {
                            "1": { "name": "storage.modifier.visibility.sca" },
                            "2": { "name": "storage.modifier.visibility.sca" },
                            "3": { "name": "keyword.other.type.sca" },
                            "4": { "name": "entity.name.type.sca" },
                            "5": { "name": "punctuation.definition.typeparameters.begin.sca" }
                        },
                        "end": "\\}",
                        "endCaptures": {
                            "0": { "name": "punctuation.definition.type.end.sca" }
                        },
                        "patterns": [
                            { "include": "#comments" },
                            { "include": "#generic-params" },
                            { "include": "#type-body" }
                        ]
                    }
                ]
            },
            "generic-params": {
                "begin": "(?<=\\()",
                "end": "\\)",
                "endCaptures": {
                    "0": { "name": "punctuation.definition.typeparameters.end.sca" }
                },
                "patterns": [
                    {
                        "name": "variable.other.type.sca",
                        "match": "\\b[a-z_][a-zA-Z0-9_]*\\b"
                    },
                    {
                        "name": "punctuation.separator.comma.sca",
                        "match": ","
                    }
                ]
            },
            "type-body": {
                "begin": "\\{",
                "beginCaptures": {
                    "0": { "name": "punctuation.definition.type.begin.sca" }
                },
                "end": "(?=\\})",
                "patterns": [
                    { "include": "#comments" },
                    {
                        "comment": "Constructor with payload: Ctor(label Type, ...)",
                        "begin": "\\b([A-Z][a-zA-Z0-9_]*)\\s*(\\()",
                        "beginCaptures": {
                            "1": { "name": "entity.name.function.constructor.sca" },
                            "2": { "name": "punctuation.definition.parameters.begin.sca" }
                        },
                        "end": "\\)",
                        "endCaptures": {
                            "0": { "name": "punctuation.definition.parameters.end.sca" }
                        },
                        "patterns": [{ "include": "#field-decl" }]
                    },
                    {
                        "comment": "Nullary constructor",
                        "name": "entity.name.function.constructor.sca",
                        "match": "\\b[A-Z][a-zA-Z0-9_]*\\b"
                    },
                    { "include": "#field-decl" }
                ]
            },
            "field-decl": {
                "patterns": [
                    {
                        "comment": "Field label + concrete type",
                        "match": "\\b([a-z_][a-zA-Z0-9_]*)\\s+([A-Z][a-zA-Z0-9_]*)\\b",
                        "captures": {
                            "1": { "name": "variable.other.member.sca" },
                            "2": { "name": "entity.name.type.sca" }
                        }
                    },
                    {
                        "comment": "Field label + type parameter",
                        "match": "\\b([a-z_][a-zA-Z0-9_]*)\\s+([a-z_][a-zA-Z0-9_]*)\\b",
                        "captures": {
                            "1": { "name": "variable.other.member.sca" },
                            "2": { "name": "variable.other.type.sca" }
                        }
                    },
                    {
                        "name": "entity.name.type.sca",
                        "match": "\\b[A-Z][a-zA-Z0-9_]*\\b"
                    },
                    {
                        "name": "punctuation.separator.comma.sca",
                        "match": ","
                    },
                    { "include": "#punctuation" }
                ]
            },
            "function-definition": {
                "begin": format!(
                    "\\b({pub}\\s+)?({fn_})\\s+([a-z_][a-zA-Z0-9_]*)",
                    pub = k(Keyword::Pub), fn_ = k(Keyword::Fn)
                ),
                "beginCaptures": {
                    "1": { "name": "storage.modifier.visibility.sca" },
                    "2": { "name": "keyword.other.fn.sca" },
                    "3": { "name": "entity.name.function.sca" }
                },
                "end": "(?=\\{|\\n|$)",
                "patterns": [
                    { "include": "#comments" },
                    { "include": "#function-params" },
                    { "include": "#type-args" },
                    {
                        "name": "keyword.other.fn.sca",
                        "match": format!("\\b{}\\b", k(Keyword::Fn))
                    },
                    {
                        "name": "entity.name.type.sca",
                        "match": "\\b[A-Z][a-zA-Z0-9_]*\\b"
                    },
                    {
                        "name": "variable.other.type.sca",
                        "match": "\\b[a-z_][a-zA-Z0-9_]*\\b"
                    }
                ]
            },
            "function-params": {
                "comment": "The parameter list opens right after the function name (\\G) and closes at the matching paren. #type-args takes nested type-argument parens, so the inner ) of Option(a) can't end the list early.",
                "begin": "\\G\\s*(\\()",
                "beginCaptures": {
                    "1": { "name": "punctuation.definition.parameters.begin.sca" }
                },
                "end": "\\)",
                "endCaptures": {
                    "0": { "name": "punctuation.definition.parameters.end.sca" }
                },
                "patterns": [
                    { "include": "#comments" },
                    { "include": "#type-args" },
                    {
                        "match": "\\b([a-z_][a-zA-Z0-9_]*)\\s+([A-Z][a-zA-Z0-9_]*)\\b",
                        "captures": {
                            "1": { "name": "variable.parameter.sca" },
                            "2": { "name": "entity.name.type.sca" }
                        }
                    },
                    {
                        "match": format!("\\b([a-z_][a-zA-Z0-9_]*)\\s+({})\\b", k(Keyword::Fn)),
                        "captures": {
                            "1": { "name": "variable.parameter.sca" },
                            "2": { "name": "keyword.other.fn.sca" }
                        }
                    },
                    {
                        "match": "\\b([a-z_][a-zA-Z0-9_]*)\\s+([a-z_][a-zA-Z0-9_]*)\\b",
                        "captures": {
                            "1": { "name": "variable.parameter.sca" },
                            "2": { "name": "variable.other.type.sca" }
                        }
                    },
                    {
                        "name": "variable.parameter.sca",
                        "match": "\\b[a-z_][a-zA-Z0-9_]*\\b"
                    },
                    {
                        "name": "punctuation.separator.comma.sca",
                        "match": ","
                    }
                ]
            },
            "type-args": {
                "comment": "Balanced ( ... ) for a parameterised type or a fn type. It includes itself, so Option(List(a)) and Result(a, e) nest correctly.",
                "begin": "\\(",
                "beginCaptures": {
                    "0": { "name": "punctuation.definition.typeparameters.begin.sca" }
                },
                "end": "\\)",
                "endCaptures": {
                    "0": { "name": "punctuation.definition.typeparameters.end.sca" }
                },
                "patterns": [
                    { "include": "#comments" },
                    {
                        "name": "keyword.other.fn.sca",
                        "match": format!("\\b{}\\b", k(Keyword::Fn))
                    },
                    {
                        "name": "entity.name.type.sca",
                        "match": "\\b[A-Z][a-zA-Z0-9_]*\\b"
                    },
                    { "include": "#type-args" },
                    {
                        "name": "variable.other.type.sca",
                        "match": "\\b[a-z_][a-zA-Z0-9_]*\\b"
                    },
                    {
                        "name": "punctuation.separator.comma.sca",
                        "match": ","
                    }
                ]
            },
            "const-definition": {
                "comment": "The declared name gets the same scope as a fn name, so themes colour every top-level value declaration the same way.",
                "match": format!(
                    "\\b({pub}\\s+)?({const_})\\s+([a-zA-Z_][a-zA-Z0-9_]*)\\s*(?:([A-Z][a-zA-Z0-9_]*)\\s*)?(=)",
                    pub = k(Keyword::Pub), const_ = k(Keyword::Const)
                ),
                "captures": {
                    "1": { "name": "storage.modifier.visibility.sca" },
                    "2": { "name": "keyword.other.const.sca" },
                    "3": { "name": "entity.name.function.sca" },
                    "4": { "name": "entity.name.type.sca" },
                    "5": { "name": "keyword.operator.assignment.sca" }
                }
            },
            "variable-with-type": {
                "comment": "Variable declaration with type annotation: name Type = ...",
                "match": "\\b([a-z_][a-zA-Z0-9_]*)\\s+([A-Z][a-zA-Z0-9_]*)\\s*(=)",
                "captures": {
                    "1": { "name": "variable.other.sca" },
                    "2": { "name": "entity.name.type.sca" },
                    "3": { "name": "keyword.operator.assignment.sca" }
                }
            },
            "backpass-binding": {
                "comment": "Backpass statement: binder(s) <- call(...)",
                "match": "\\b([a-z_][a-zA-Z0-9_]*(?:\\s*,\\s*[a-z_][a-zA-Z0-9_]*)*)\\s*(<-)",
                "captures": {
                    "1": {
                        "patterns": [
                            {
                                "name": "variable.other.sca",
                                "match": "\\b[a-z_][a-zA-Z0-9_]*\\b"
                            }
                        ]
                    },
                    "2": { "name": "keyword.operator.backpass.sca" }
                }
            },
            "keywords": {
                "patterns": [
                    {
                        "name": "keyword.control.sca",
                        "match": format!("\\b({})\\b", keyword_alternation(KeywordGroup::Control))
                    },
                    {
                        "comment": "`then` is contextual. It's a keyword after an if condition and a name after a dot.",
                        "name": "keyword.control.sca",
                        "match": format!("(?<!\\.)\\b{THEN}\\b")
                    },
                    {
                        "name": "keyword.control.import.sca",
                        "match": format!("\\b({})\\b", keyword_alternation(KeywordGroup::Import))
                    },
                    {
                        "name": "storage.modifier.visibility.sca",
                        "match": format!("\\b({})\\b", keyword_alternation(KeywordGroup::Modifier))
                    },
                    {
                        "name": "keyword.other.sca",
                        "match": format!("\\b({})\\b", keyword_alternation(KeywordGroup::Declaration))
                    }
                ]
            },
            "constants": {
                "comment": "Prelude constructors that themes expect to see as language constants.",
                "patterns": [
                    {
                        "name": "constant.language.boolean.sca",
                        "match": "\\b(True|False)\\b"
                    },
                    {
                        "name": "constant.language.nil.sca",
                        "match": "\\bNil\\b"
                    }
                ]
            },
            "module-qualified-access": {
                "comment": "module::name access",
                "match": "\\b([a-z_][a-zA-Z0-9_]*)(::)(?=[A-Za-z_])",
                "captures": {
                    "1": { "name": "entity.name.namespace.sca" },
                    "2": { "name": "punctuation.accessor.module.sca" }
                }
            },
            "constructor-call": {
                "comment": "Constructor call or pattern: an uppercase name followed by (",
                "match": "\\b([A-Z][a-zA-Z0-9_]*)(?=\\s*\\()",
                "captures": {
                    "1": { "name": "entity.name.function.constructor.sca" }
                }
            },
            "function-call": {
                "comment": "Function calls",
                "match": "\\b([a-z_][a-zA-Z0-9_]*)(?=\\s*\\()",
                "captures": {
                    "1": { "name": "entity.name.function.sca" }
                }
            },
            "labeled-argument": {
                "comment": "Labelled call argument: label: value",
                "match": "\\b([a-z_][a-zA-Z0-9_]*)\\s*(:)(?!:)",
                "captures": {
                    "1": { "name": "variable.parameter.label.sca" },
                    "2": { "name": "punctuation.separator.colon.sca" }
                }
            },
            "types": {
                "patterns": [
                    {
                        "comment": "Standalone type name",
                        "name": "entity.name.type.sca",
                        "match": "\\b[A-Z][a-zA-Z0-9_]*\\b"
                    }
                ]
            },
            "operators": {
                "patterns": operator_rules()
            },
            "punctuation": {
                "patterns": punctuation_rules()
            }
        }
    });

    let mut out = serde_json::to_string_pretty(&grammar).expect("grammar serialises");
    out.push('\n');
    out
}

// ---------------------------------------------------------------------------
// lexical.js (tree-sitter)
// ---------------------------------------------------------------------------

fn lexical_js() -> String {
    let keywords = Keyword::ALL
        .into_iter()
        .map(|kw| format!("'{}'", kw.text()))
        .collect::<Vec<_>>()
        .join(", ");
    let sized = BIN_SPEC_SIZED
        .iter()
        .map(|s| format!("'{s}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let bare = BIN_SPEC_BARE
        .iter()
        .map(|s| format!("'{s}'"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "\
// GENERATED FILE - do not edit. Regenerate with: cargo xtask gen-editor-syntax
// These tables come from crates/scarlet_syntax (token/keywords.rs, token/kind.rs and the
// scanner's ESCAPES). grammar.js holds the hand-written structural rules and reads these
// tables, so the lexical layer can't drift from the compiler.
'use strict';

module.exports = {{
  // Keyword::ALL. Every one is reserved, so none of them can parse as an identifier.
  keywords: [{keywords}],
  // token::is_name_start / is_name_continue.
  identifier: /[A-Za-z_][A-Za-z0-9_]*/,
  // Scanner::scan_number: hex (`0x`), binary (`0b`), or decimal with an optional
  // fraction. A `_` only counts between two digits of that radix, and a `_` or `.`
  // with no digit after it ends the token. Hex and binary come first so `0xFF`
  // isn't read as decimal `0`.
  number: /(0[xX][0-9A-Fa-f]+(_[0-9A-Fa-f]+)*|0[bB][01]+(_[01]+)*|\\d+(_\\d+)*(\\.\\d+(_\\d+)*)?)/,
  // scanner::ESCAPES. Anything else after a backslash is an error.
  escape: /\\\\[{escape_class}]/,
  // The contextual identifiers in << >> segment specs (parse_bin_size_spec).
  binSpecSized: [{sized}],
  binSpecBare: [{bare}],
  // token::THEN is a keyword only after an `if` condition (parse_if_expression),
  // so it isn't in `keywords`.
  then: '{THEN}',
}};
",
        escape_class = escape_char_class(),
    )
}

#[cfg(test)]
mod tests {
    use scarlet_syntax::scanner::new_scanner;
    use scarlet_syntax::token::Kind;

    use super::outputs;

    /// Number literals are the one part of the lexical layer that's a hand-copied regex
    /// instead of coming from a table. A change to `Scanner::scan_number` regenerates
    /// byte-identical files, so `generated_files_are_up_to_date` can't see the regexes drift.
    /// This test pins the shape they encode instead. When it fails, the scanner has moved, and
    /// `lexical_js`'s `number` and `tm_language`'s `numbers` patterns must move with it.
    ///
    /// We added it after `_` digit separators reached the scanner but neither regex, and the
    /// tree-sitter corpus job went red on two examples.
    #[test]
    fn number_regexes_still_describe_the_scanner() {
        // Each case is a source and the LiteralNumber texts it must yield, in order.
        let cases: &[(&str, &[&str])] = &[
            ("1", &["1"]),
            ("1_000_000", &["1_000_000"]),
            ("1.5", &["1.5"]),
            ("1_0.0_1", &["1_0.0_1"]),
            // A `_` or a `.` with no digit after it ends the token, which is
            // why neither regex folds `_` into the digit class.
            ("1_", &["1"]),
            ("1.", &["1"]),
            // The scanner mustn't fuse a range like `0..10` into one token.
            ("0..10", &["0", "10"]),
            ("0xFF", &["0xFF"]),
            ("0Xff", &["0Xff"]),
            ("0b1010", &["0b1010"]),
            ("0B11", &["0B11"]),
            ("0xDE_AD", &["0xDE_AD"]),
            ("0b1111_0000", &["0b1111_0000"]),
            ("0x10..0x20", &["0x10", "0x20"]),
            // A prefix with no digits is an Error token, not a LiteralNumber.
            ("0x", &[]),
            ("0b", &[]),
        ];
        for (source, want) in cases {
            let (tokens, _) = new_scanner(*source).scan_all();
            let got: Vec<String> = tokens
                .iter()
                .filter_map(|t| match &t.kind {
                    Kind::LiteralNumber(text) => Some(text.to_string()),
                    _ => None,
                })
                .collect();
            assert_eq!(
                got, *want,
                "Scanner::scan_number changed on {source:?}. Update the `number` \
                 regex in lexical_js and the `numbers` patterns in tm_language, \
                 then regenerate with `cargo xtask gen-editor-syntax`"
            );
        }
    }

    /// Regenerating must be a no-op against the checked-in files, so CI fails
    /// when the scanner's token tables change without a regeneration.
    #[test]
    fn generated_files_are_up_to_date() {
        for (path, contents) in outputs() {
            let on_disk = std::fs::read_to_string(&path).unwrap_or_default();
            assert_eq!(
                on_disk,
                contents,
                "{} is stale. Run `cargo xtask gen-editor-syntax`",
                path.display()
            );
        }
    }
}
