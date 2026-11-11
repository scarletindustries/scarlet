use std::fmt;

use crate::ast::ImportPath;
use crate::diagnostic::{DiagnosticCode, Problem};
use crate::scanner::ScanError;
use crate::token::Kind;

/// Why the parser could not read a stretch of source. The text a user reads
/// is made only when a diagnostic is built, by `Display`.
#[derive(Debug, Clone)]
pub enum ParseError {
    NestingTooDeep,
    ExpectedToken {
        expected: Kind,
        got: Kind,
    },
    Expected {
        what: Expected,
        got: Kind,
    },
    Unexpected(Kind),
    UnexpectedInPattern(Kind),
    UnexpectedInInterpolation(Kind),
    ScannerError,
    CommaBetweenLines(LineItem),
    SameLine(LineItem),
    FieldsWithoutCommas,
    CommaBetweenArms,
    ImportAfterDeclaration,
    RelativeImportWithoutSlash,
    /// `import m.{x}`: what to write instead.
    ImportList {
        path: ImportPath,
        qualifier: String,
        item: String,
    },
    /// `http.Method` where only `http::Method` can be meant.
    DotBeforeModuleMember {
        module: String,
        member: String,
    },
    QualifierNotModule,
    Decrement,
    SpreadWithoutValue,
    SegmentSpec {
        got: Kind,
    },
    EmptyTuple,
    SingleElementParens,
    IfWithoutBody,
    IfWithoutElse,
    FunctionBodyNotBlock,
    ElsePattern,
    TuplePatternTooShort,
    TupleTypeTooShort,
    RangeBoundNotNumber,
    RestNotLast,
    LowercaseConstructor {
        name: String,
    },
    LowercaseTypeName {
        name: String,
    },
    VmWithoutReturnType,
    VmWithBody,
    VmArity,
    OpaqueAlias,
    OpaqueWithoutConstructors,
    OpaqueNotType,
    EmptyTypeDefinition,
    UnlabeledField,
    AttributesOnConst,
    ExpectedDeclaration {
        after: DeclarationLead,
        got: Kind,
    },
    BackpassOr,
    BackpassNotCall,
    BackpassAtTopLevel,
}

/// What a parser step wanted to read when it found something else.
#[derive(Debug, Clone, Copy)]
pub enum Expected {
    OrReceiver,
    ArgumentLabel,
    Identifier,
    BitWidth,
    Pattern,
    ConstructorName,
    Number,
    NumberAfterMinus,
    String,
    StringPart,
    SpreadBinding,
    PatternLabel,
    FunctionName,
    ParameterName,
    TypeName,
    MemberTypeName,
    TypeParameterName,
    DeclaredTypeName,
    FieldLabel,
    ConstName,
    AttributeName,
    AttributeArgument,
    ImportedModule,
    ModuleSegment,
    Alias,
    ModuleMember,
    TupleIndex,
    PropertyName,
}

/// The items of a type body, which go one per line.
#[derive(Debug, Clone, Copy)]
pub enum LineItem {
    Field,
    Constructor,
    Statement,
    Arm,
    Declaration,
}

/// What came before a declaration keyword that was not there.
#[derive(Debug, Clone, Copy)]
pub enum DeclarationLead {
    Pub,
    Attributes,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::NestingTooDeep => f.write_str(
                "This expression or type is nested too deeply. Pull part of it out into a \
                 named value or type",
            ),
            ParseError::ExpectedToken { expected, got } => {
                write!(f, "Expected '{expected}', but found '{got}'")
            }
            ParseError::Expected { what, got } => write!(f, "{what}, but found '{got}'"),
            ParseError::Unexpected(got) => write!(f, "Expected an expression, but found '{got}'"),
            ParseError::UnexpectedInPattern(got) => {
                write!(f, "Expected a pattern, but found '{got}'")
            }
            ParseError::UnexpectedInInterpolation(got) => {
                write!(f, "Unexpected '{got}' inside an interpolated string")
            }
            ParseError::ScannerError => f.write_str("This text can't be read"),
            ParseError::CommaBetweenLines(item) => write!(
                f,
                "Put each {item} on its own line, without a `,` between them"
            ),
            ParseError::SameLine(item) => write!(f, "Put each {item} on its own line"),
            ParseError::FieldsWithoutCommas => {
                f.write_str("Separate fields with commas, like `Name(a Int, b String)`")
            }
            ParseError::CommaBetweenArms => {
                f.write_str("Match arms go on separate lines, without a `,` between them")
            }
            ParseError::ImportAfterDeclaration => {
                f.write_str("Put imports at the top of the file, before any other declaration")
            }
            ParseError::RelativeImportWithoutSlash => {
                f.write_str("Expected `/` after `.` or `..` in an import path")
            }
            ParseError::ImportList {
                path,
                qualifier,
                item,
            } => write!(
                f,
                "Import the module and name it at the use: `import {path}`, then `{qualifier}::{item}`"
            ),
            ParseError::DotBeforeModuleMember { module, member } => write!(
                f,
                "Reach a module's type or constructor with `::`, like `{module}::{member}`"
            ),
            ParseError::QualifierNotModule => {
                f.write_str("Only an imported module's name can come before `::`")
            }
            ParseError::Decrement => f.write_str(
                "Scarlet has no `--` operator, because values never change. Write \
                 `x = x - 1` to shadow `x` with a new value instead",
            ),
            ParseError::SpreadWithoutValue => {
                f.write_str("Expected a value after `..` in an array, like `[..xs]`")
            }
            ParseError::SegmentSpec { got } => write!(
                f,
                "Expected a segment size after `:`, which is a number, `size(..)`, `bytes(..)`, \
                 `binary` or `utf8`, but found '{got}'"
            ),
            ParseError::EmptyTuple => f.write_str(
                "A tuple needs at least two elements. Write `Nil` for no value, or `{ expr }` \
                 to group an expression",
            ),
            ParseError::SingleElementParens => f.write_str(
                "Parentheses can't hold just one expression. Write `{ expr }` to group it, or \
                 add another element to make a tuple",
            ),
            ParseError::IfWithoutBody => {
                f.write_str("An `if` needs `then` or a block `{ ... }` after its condition")
            }
            ParseError::IfWithoutElse => f.write_str("An `if` needs an `else` branch"),
            ParseError::FunctionBodyNotBlock => {
                f.write_str("A function body must be a block `{ ... }`")
            }
            ParseError::ElsePattern => f.write_str(
                "`else` is not a pattern. Use `_` or a name for the arm that matches everything \
                 else",
            ),
            ParseError::TuplePatternTooShort => {
                f.write_str("A tuple pattern needs at least two elements")
            }
            ParseError::TupleTypeTooShort => {
                f.write_str("A tuple type needs at least two elements")
            }
            ParseError::RangeBoundNotNumber => {
                f.write_str("Both ends of a range pattern must be number literals, like `1..10`")
            }
            ParseError::RestNotLast => {
                f.write_str("`..` must be the last segment in a `<<>>` pattern")
            }
            ParseError::LowercaseConstructor { name } => write!(
                f,
                "Constructor name '{name}' must start with an uppercase letter"
            ),
            ParseError::LowercaseTypeName { name } => {
                write!(f, "Type name '{name}' must start with an uppercase letter")
            }
            ParseError::VmWithoutReturnType => {
                f.write_str("A @vm function must declare its return type")
            }
            ParseError::VmWithBody => f.write_str("A @vm function can't have a body"),
            ParseError::VmArity => {
                f.write_str("@vm takes exactly one argument, the VM op key, like @vm(add)")
            }
            ParseError::OpaqueAlias => f.write_str("A type alias can't be `opaque`"),
            ParseError::OpaqueWithoutConstructors => {
                f.write_str("An `opaque` type must have constructors to hide")
            }
            ParseError::OpaqueNotType => f.write_str("Only a `type` declaration can be `opaque`"),
            ParseError::EmptyTypeDefinition => {
                f.write_str("A type definition needs at least one constructor")
            }
            ParseError::UnlabeledField => f.write_str(
                "Each constructor field needs a label. Write `label Type`, like `name String`",
            ),
            ParseError::AttributesOnConst => {
                f.write_str("A `const` declaration can't have attributes")
            }
            ParseError::ExpectedDeclaration { after, got } => write!(
                f,
                "Expected `fn`, `type` or `const` after {after}, but found '{got}'"
            ),
            ParseError::BackpassOr => f.write_str("A backpass statement can't have `or`"),
            ParseError::BackpassNotCall => {
                f.write_str("A backpass needs a function call on the right of `<-`")
            }
            ParseError::BackpassAtTopLevel => f.write_str(
                "Backpassing is not allowed at the top level of a module. Use it inside a \
                 function or a block",
            ),
        }
    }
}

impl fmt::Display for Expected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Expected::OrReceiver => "Expected a name for the error after `or`",
            Expected::ArgumentLabel => "Expected an argument label",
            Expected::Identifier => "Expected a name",
            Expected::BitWidth => "Expected a bit width",
            Expected::Pattern => "Expected a pattern",
            Expected::ConstructorName => "Expected a constructor name",
            Expected::Number => "Expected a number",
            Expected::NumberAfterMinus => "Expected a number after `-`",
            Expected::String => "Expected a string",
            Expected::StringPart => "Expected the text of a string",
            Expected::SpreadBinding => "Expected a name after `..`",
            Expected::PatternLabel => "Expected a field label",
            Expected::FunctionName => "Expected a function name",
            Expected::ParameterName => "Expected a parameter name",
            Expected::TypeName => "Expected a type name",
            Expected::MemberTypeName => "Expected a type name after `::`",
            Expected::TypeParameterName => "Expected a type parameter name",
            Expected::DeclaredTypeName => "Expected a type name after `type`",
            Expected::FieldLabel => "Expected a field label",
            Expected::ConstName => "Expected a constant name",
            Expected::AttributeName => "Expected an attribute name after `@`",
            Expected::AttributeArgument => "Expected an attribute argument",
            Expected::ImportedModule => "Expected a module name after `import`",
            Expected::ModuleSegment => "Expected a module name after `/`",
            Expected::Alias => "Expected a name after `as`",
            Expected::ModuleMember => "Expected a module member after `::`",
            Expected::TupleIndex => "Expected a tuple index",
            Expected::PropertyName => "Expected a field name after `.`",
        })
    }
}

impl fmt::Display for LineItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            LineItem::Field => "field",
            LineItem::Constructor => "constructor",
            LineItem::Statement => "statement",
            LineItem::Arm => "match arm",
            LineItem::Declaration => "declaration",
        })
    }
}

impl fmt::Display for DeclarationLead {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            DeclarationLead::Pub => "`pub`",
            DeclarationLead::Attributes => "attributes",
        })
    }
}

/// What reading a source file can find wrong with it: the scanner's errors
/// and the parser's.
#[derive(Debug, Clone)]
pub enum SyntaxError {
    Scan(ScanError),
    /// `at_end` says the parser ran out of input partway through, so a reader
    /// that can ask for more input should.
    Parse {
        error: ParseError,
        at_end: bool,
    },
}

impl Problem for SyntaxError {
    fn code(&self) -> DiagnosticCode {
        match self {
            SyntaxError::Scan(e) => e.code(),
            SyntaxError::Parse { at_end: true, .. } => DiagnosticCode::UnexpectedEof,
            SyntaxError::Parse { at_end: false, .. } => DiagnosticCode::ParseError,
        }
    }
}

impl fmt::Display for SyntaxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SyntaxError::Scan(e) => e.fmt(f),
            SyntaxError::Parse { error, .. } => error.fmt(f),
        }
    }
}
