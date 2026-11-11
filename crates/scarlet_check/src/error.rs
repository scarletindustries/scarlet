//! What the checker can find wrong with a module. Each failure is its own
//! variant, carrying what it is about. Its text is made by `Display`, only
//! when someone reads it.

use std::fmt;

use scarlet_syntax::diagnostic::{DiagnosticCode, Problem, Severity};
use scarlet_syntax::module_path::ModulePath;
use scarlet_syntax::parser::ParseError;
use scarlet_types::name::Name;
use scarlet_types::ty::{Type, show_together};

/// A declaration that only module scope can hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NestedDecl {
    Function,
    Type,
    Const,
}

/// Where a constructor's fields are given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldSite {
    Call,
    Pattern,
}

/// Something the parser should have refused, found by the checker.
#[derive(Debug, Clone)]
pub enum Unreadable {
    Backpass,
    Pipe,
    /// A stretch the parser could not read, and why.
    Node(ParseError),
}

#[derive(Debug, Clone)]
pub enum CheckError {
    // --- Imports and modules ---
    /// An import's qualifier already names another import, written as
    /// `written`.
    DuplicateQualifier {
        qualifier: Name,
        written: String,
    },
    /// `name::` names no import. `stdlib` lists the stdlib modules of that
    /// name, which an import would bring in.
    UnknownModule {
        name: Name,
        stdlib: Vec<ModulePath>,
    },
    /// `q::Name` in a pattern, where `q` names no import.
    UnknownQualifier(Name),
    /// A module, `module` as its import wrote it, used as a value. `example`
    /// is one of its members to suggest.
    ModuleNotValue {
        name: Name,
        module: String,
        example: Option<Name>,
    },
    /// `m.field`, where `m` is a module.
    ModuleField {
        module: Name,
        field: Name,
    },
    PrivateMember {
        module: String,
        member: Name,
    },
    NoMember {
        module: String,
        member: Name,
    },

    // --- Declarations ---
    UnknownBuiltin(Name),
    /// A statement at module scope in a library.
    StatementInLibrary,
    /// A statement at module scope in a program's entry.
    StatementInEntry,
    MainNotPublic,
    MainTakesParameters,
    VmOutsideStdlib,
    VmOnNonFunction,
    UnknownAttribute(Name),
    AlreadyDefined(Name),
    /// Where the name the error before this one says is defined twice was
    /// defined first.
    FirstDefinition,
    RedefinesPrelude(Name),
    /// A declaration below module scope.
    NestedDeclaration(NestedDecl),
    Unreadable(Unreadable),

    // --- Types and annotations ---
    DuplicateTypeParameter(Name),
    /// A function type in a type definition, with no return type.
    FunctionTypeNeedsReturn,
    TypeVariableWithArguments(Name),
    UnknownTypeVariable(Name),
    UnknownQualifiedType {
        qualifier: Name,
        name: Name,
    },
    /// A bare type name nothing defines. `exported_by` lists the imports
    /// that export a type of that name, in order.
    UnknownType {
        name: Name,
        exported_by: Vec<Name>,
    },
    TypeArity {
        qualifier: Option<Name>,
        name: Name,
        expected: usize,
        given: usize,
    },
    RecursiveAlias(Name),
    /// A public value whose type still holds a variable nothing generalised.
    NotFullyKnown {
        name: Name,
        ty: Type,
    },

    // --- Names and values ---
    /// A local whose name starts with `_` is read.
    ReadsDiscarded(Name),
    /// A name nothing in scope binds. `ctor_in` lists the imports with a
    /// constructor of that name, in order; `suggestion` is the closest name
    /// in scope.
    UnknownIdentifier {
        name: Name,
        ctor_in: Vec<Name>,
        suggestion: Option<Name>,
    },
    InvalidNumber(String),
    IntOutOfRange(String),
    TupleIndexNotInt(String),

    // --- Calls and constructors ---
    LabelNeedsKnownCallee,
    /// A constructor call with more than one `..base`.
    TwoSpreadsInCall,
    SpreadOutsideConstructor,
    NoSuchParameter {
        label: Name,
        available: Vec<Name>,
    },
    ParameterTwice(Name),
    TooManyFields {
        ctor: Name,
        arity: usize,
    },
    NoSuchField {
        ctor: Name,
        label: Name,
        available: Vec<Name>,
    },
    FieldTwice(Name),
    /// A construction or pattern leaves fields out. A pattern can say so
    /// with `..`.
    MissingFields {
        ctor: Name,
        fields: Vec<Name>,
        site: FieldSite,
    },
    WrongArgumentCount {
        expected: usize,
        given: usize,
    },
    NotCallable(Type),
    SpreadCannotFill {
        label: Name,
        variant: Name,
        ty: Name,
    },

    // --- Patterns ---
    RangeBoundNotInt(String),
    UnboundInSomeAlternative(Name),
    BoundTwice(Name),
    NotInFirstAlternative(Name),
    ConstructorTakesNoArguments {
        ctor: Name,
        given: usize,
    },
    TwoSpreads,
    SpreadNotLast,
    PatternNotConstructor(Name),
    PatternIsType(Name),
    /// A bare constructor in a pattern that only imports have, under the
    /// qualifiers `qualifiers`.
    UnknownConstructorElsewhere {
        name: Name,
        qualifiers: Vec<Name>,
    },
    UnknownConstructor(Name),
    NoConstructorInModule {
        module: String,
        name: Name,
    },
    PrivateConstructor {
        module: String,
        name: Name,
    },
    QualifiedNotConstructor {
        module: String,
        name: Name,
    },

    // --- Inference ---
    Mismatch {
        expected: Type,
        given: Type,
        /// The parameter counts, when both are functions of different
        /// arities.
        arity: Option<(usize, usize)>,
    },
    InfiniteType,
    TupleIndexOutOfRange {
        index: u32,
        len: usize,
    },
    NotATuple {
        index: u32,
    },
    /// Resolution orders the module so this can't happen. If it does, it is
    /// a bug in the checker.
    ReadBeforeTyped,
    FieldOfUnknownType(Name),
    NoFieldOnType {
        ty: Type,
        label: Name,
    },
    NoFieldOnNamed {
        ty: Name,
        label: Name,
    },
    NoAccessibleFields(Name),
    FieldNotOnEveryVariant {
        label: Name,
        variant: Name,
        ty: Name,
    },
    OrNeedsOptionOrResult(Type),
    OrOptionReceiver,

    // --- After inference ---
    UnusedValue(Type),
    /// `missing` is the patterns the arms leave out, written as patterns.
    NotExhaustive {
        missing: String,
    },
    Unreachable,
    /// A catch-all arm over named variants, which it stands for.
    CatchAll(Vec<String>),
    /// A binding's pattern misses `missing`, written as a pattern.
    BindingMisses {
        missing: String,
    },
    OrNilReceiver,
    OrDropsError(Type),
    OrDiscardsReceiver(Name),
    NilDiscard(Name),
    Unused(Name),
}

impl Problem for CheckError {
    fn code(&self) -> DiagnosticCode {
        use CheckError as E;
        match self {
            E::DuplicateQualifier { .. } | E::UnknownModule { .. } => DiagnosticCode::ModuleError,
            E::Unreadable(_) => DiagnosticCode::ParseError,
            E::Unused(_) => DiagnosticCode::UnusedBinding,
            E::FirstDefinition => DiagnosticCode::RelatedLocation,
            _ => DiagnosticCode::TypeError,
        }
    }

    fn severity(&self) -> Severity {
        match self {
            CheckError::FirstDefinition => Severity::Hint,
            _ => Severity::Error,
        }
    }
}

/// `items` joined with " or ".
fn either(items: impl Iterator<Item = String>) -> String {
    items.collect::<Vec<_>>().join(" or ")
}

fn join(names: &[Name]) -> String {
    names
        .iter()
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

impl fmt::Display for CheckError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use CheckError as E;
        match self {
            E::DuplicateQualifier { qualifier, written } => write!(
                f,
                "'{qualifier}' already names the import of `{written}`. Give this one another name with `as`"
            ),
            E::UnknownModule { name, stdlib } if stdlib.is_empty() => write!(
                f,
                "Unknown module '{name}'. Import it before reaching into it with `::`"
            ),
            E::UnknownModule { name, stdlib } => write!(
                f,
                "Unknown module '{name}': it is the name of a stdlib module that is not imported. Add {}",
                either(stdlib.iter().map(|p| format!("`import {}`", p.join("/"))))
            ),
            E::UnknownQualifier(q) => {
                write!(f, "Unknown module qualifier '{q}'. Import the module first")
            }
            E::ModuleNotValue {
                name,
                module,
                example,
            } => {
                write!(
                    f,
                    "'{name}' is the module `{module}`, not a value. Use one of its members"
                )?;
                match example {
                    Some(member) => write!(f, ", like `{name}::{member}`"),
                    None => Ok(()),
                }
            }
            E::ModuleField { module, field } => write!(
                f,
                "'{module}' is a module, not a value. Reach its members with `::`, like `{module}::{field}`"
            ),
            E::PrivateMember { module, member } => {
                write!(f, "'{member}' is private in module '{module}'")
            }
            E::NoMember { module, member } => {
                write!(f, "Module '{module}' has no member '{member}'")
            }

            E::UnknownBuiltin(key) => write!(f, "unknown @vm builtin '{key}'"),
            E::StatementInLibrary => {
                write!(f, "Modules may only contain declarations at the top level")
            }
            E::StatementInEntry => write!(
                f,
                "Statements aren't allowed at module scope. Put a program's code inside `pub fn main()`"
            ),
            E::MainNotPublic => write!(
                f,
                "`main` must be public, because a program starts at `pub fn main()`"
            ),
            E::MainTakesParameters => write!(
                f,
                "`main` takes no parameters. Read the program's arguments with `os.argv()` instead"
            ),
            E::VmOutsideStdlib => write!(f, "'@vm' is only allowed in the standard library"),
            E::VmOnNonFunction => write!(f, "'@vm' may only be used on functions"),
            E::UnknownAttribute(a) => write!(f, "Unknown attribute '@{a}'"),
            E::AlreadyDefined(n) => write!(f, "'{n}' is already defined"),
            E::FirstDefinition => write!(f, "first definition was here"),
            E::RedefinesPrelude(n) => {
                write!(f, "'{n}' is defined in the prelude and cannot be redefined")
            }
            E::NestedDeclaration(kind) => {
                let kind = match kind {
                    NestedDecl::Function => "named function",
                    NestedDecl::Type => "type",
                    NestedDecl::Const => "const",
                };
                write!(f, "{kind} declarations are only allowed at the top level")
            }
            E::Unreadable(Unreadable::Backpass) => write!(f, "This backpass can't be read here"),
            E::Unreadable(Unreadable::Pipe) => write!(f, "This pipe can't be read here"),
            E::Unreadable(Unreadable::Node(e)) => e.fmt(f),

            E::DuplicateTypeParameter(p) => write!(f, "Duplicate type parameter '{p}'"),
            E::FunctionTypeNeedsReturn => write!(
                f,
                "A function type in a type definition must declare its return type, like `fn(Int) Int`"
            ),
            E::TypeVariableWithArguments(n) => {
                write!(f, "Type variable '{n}' cannot take arguments")
            }
            E::UnknownTypeVariable(n) => write!(f, "Unknown type variable '{n}'"),
            E::UnknownQualifiedType { qualifier, name } => write!(
                f,
                "Unknown type '{qualifier}::{name}'. Check that '{qualifier}' is imported and exports a type called '{name}'."
            ),
            E::UnknownType { name, exported_by } => match exported_by.as_slice() {
                [] => write!(f, "Unknown type '{name}'"),
                [one] => write!(f, "Unknown type '{name}'. Did you mean '{one}::{name}'?"),
                many => write!(
                    f,
                    "Unknown type '{name}'. Did you mean one of {}?",
                    many.iter()
                        .map(|q| format!("'{q}::{name}'"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            },
            E::TypeArity {
                qualifier,
                name,
                expected,
                given,
            } => {
                let shown = match qualifier {
                    Some(q) => format!("{q}::{name}"),
                    None => name.to_string(),
                };
                write!(
                    f,
                    "Type '{shown}' expects {expected} type argument{}, got {given}",
                    if *expected == 1 { "" } else { "s" }
                )
            }
            E::RecursiveAlias(n) => write!(
                f,
                "Recursive type alias '{n}'. An alias can't refer back to itself"
            ),
            E::NotFullyKnown { name, ty } => write!(
                f,
                "The type of '{name}' isn't fully known: '{ty}'. Write its type out, so every module that uses it agrees on it"
            ),

            E::ReadsDiscarded(name) => match name.trim_start_matches('_') {
                "" => write!(
                    f,
                    "'{name}' is a discard binding and can't be read. Give it a name to use its value"
                ),
                stripped => write!(
                    f,
                    "'{name}' starts with '_', which marks it unused, so it can't be read. Rename it to '{stripped}'"
                ),
            },
            E::UnknownIdentifier {
                name,
                ctor_in,
                suggestion,
            } => {
                if !ctor_in.is_empty() {
                    write!(
                        f,
                        "Unknown identifier '{name}'. A constructor of that name is {}",
                        either(ctor_in.iter().map(|q| format!("`{q}::{name}`")))
                    )
                } else if let Some(s) = suggestion {
                    write!(f, "Unknown identifier '{name}'. Did you mean '{s}'?")
                } else {
                    write!(f, "Unknown identifier '{name}'")
                }
            }
            E::InvalidNumber(v) => write!(f, "invalid number literal '{v}'"),
            E::IntOutOfRange(v) => write!(
                f,
                "integer literal '{v}' out of range for Int (must be between {} and {})",
                i64::MIN,
                i64::MAX
            ),
            E::TupleIndexNotInt(v) => write!(f, "Tuple index must be an integer, got '{v}'"),

            E::LabelNeedsKnownCallee => write!(
                f,
                "Labelled arguments need a callee whose parameter names are known: a constructor, or a function named directly"
            ),
            E::TwoSpreadsInCall => write!(f, "A constructor call can only have one spread"),
            E::SpreadOutsideConstructor => write!(
                f,
                "Spread arguments are only allowed in constructor record-update calls"
            ),
            E::NoSuchParameter { label, available } => write!(
                f,
                "This function has no parameter '{label}'. Available: {}",
                join(available)
            ),
            E::ParameterTwice(p) => write!(f, "Parameter '{p}' is specified more than once"),
            E::TooManyFields { ctor, arity } => write!(
                f,
                "Constructor '{ctor}' has {arity} field(s) but more were supplied"
            ),
            E::NoSuchField {
                ctor,
                label,
                available,
            } => write!(
                f,
                "Constructor '{ctor}' has no field '{label}'. Available: {}",
                join(available)
            ),
            E::FieldTwice(l) => write!(f, "Field '{l}' is specified more than once"),
            E::MissingFields { ctor, fields, site } => write!(
                f,
                "Constructor '{ctor}' is missing field(s): {}{}",
                join(fields),
                match site {
                    FieldSite::Call => "",
                    FieldSite::Pattern => ". Use '..' to ignore them",
                }
            ),
            E::WrongArgumentCount { expected, given } => {
                write!(f, "Expected {expected} argument(s), got {given}")
            }
            E::NotCallable(ty) => write!(
                f,
                "This value of type '{ty}' is not a function and cannot be called"
            ),
            E::SpreadCannotFill { label, variant, ty } => write!(
                f,
                "The spread cannot fill field '{label}', which {}; write '{label}:' in the call",
                MissingOn { variant, ty }
            ),

            E::RangeBoundNotInt(v) => {
                write!(f, "Range pattern bound must be an integer, got '{v}'")
            }
            E::UnboundInSomeAlternative(n) => write!(
                f,
                "Variable '{n}' must be bound in every alternative of this pattern"
            ),
            E::BoundTwice(n) => write!(f, "Variable '{n}' is bound more than once in this pattern"),
            E::NotInFirstAlternative(n) => write!(
                f,
                "Variable '{n}' is not bound in the first alternative of this pattern. Every alternative must bind the same names"
            ),
            E::ConstructorTakesNoArguments { ctor, given } => write!(
                f,
                "Constructor '{ctor}' takes no arguments but {given} were given"
            ),
            E::TwoSpreads => write!(f, "An array pattern can only have one spread"),
            E::SpreadNotLast => write!(f, "The spread in an array pattern must come last"),
            E::PatternNotConstructor(n) => write!(
                f,
                "'{n}' is not a constructor and cannot be used in a pattern"
            ),
            E::PatternIsType(n) => write!(
                f,
                "'{n}' is a type, not a constructor. To check a value's type and drop it, write `_ {n} = value`"
            ),
            E::UnknownConstructorElsewhere { name, qualifiers } => write!(
                f,
                "Unknown constructor '{name}' in pattern. It is {}",
                either(qualifiers.iter().map(|q| format!("`{q}::{name}`")))
            ),
            E::UnknownConstructor(n) => write!(f, "Unknown constructor '{n}' in pattern"),
            E::NoConstructorInModule { module, name } => {
                write!(f, "Module '{module}' has no constructor '{name}'")
            }
            E::PrivateConstructor { module, name } => {
                write!(f, "Constructor '{name}' is private in module '{module}'")
            }
            E::QualifiedNotConstructor { module, name } => write!(
                f,
                "'{module}::{name}' is not a constructor and cannot be used in a pattern"
            ),

            E::Mismatch {
                expected,
                given,
                arity,
            } => {
                let shown = show_together(&[expected, given]);
                write!(
                    f,
                    "Type mismatch: expected '{}', got '{}'",
                    shown[0], shown[1]
                )?;
                match arity {
                    Some((e, g)) => write!(
                        f,
                        "\nExpected a function that takes {e} argument(s), but got one that takes {g}."
                    ),
                    None => Ok(()),
                }
            }
            E::InfiniteType => write!(
                f,
                "Infinite type detected: this would need a type that contains itself, as `x(x)` does"
            ),
            E::TupleIndexOutOfRange { index, len } => write!(
                f,
                "Tuple index {index} is out of range, because the tuple has {len} elements"
            ),
            E::NotATuple { index } => {
                write!(
                    f,
                    "Can't use .{index} here, because the value isn't a tuple"
                )
            }
            E::ReadBeforeTyped => write!(
                f,
                "internal compiler error: this name was read before it had a type"
            ),
            E::FieldOfUnknownType(label) => write!(
                f,
                "Can't access field '{label}' because the value's type isn't known here. Add a type annotation"
            ),
            E::NoFieldOnType { ty, label } => write!(f, "Type '{ty}' has no field '{label}'"),
            E::NoFieldOnNamed { ty, label } => write!(f, "Type '{ty}' has no field '{label}'"),
            E::NoAccessibleFields(ty) => write!(f, "Type '{ty}' has no accessible fields"),
            E::FieldNotOnEveryVariant { label, variant, ty } => {
                write!(f, "Field '{label}' {}", MissingOn { variant, ty })
            }
            E::OrNeedsOptionOrResult(ty) => write!(
                f,
                "'or' requires the left side to be Option(_) or Result(_, _), got '{ty}'"
            ),
            E::OrOptionReceiver => write!(
                f,
                "'or' on an Option can't bind a value, because None carries nothing"
            ),

            E::UnusedValue(ty) => write!(
                f,
                "This expression's value of type '{ty}' isn't used. Assign it to a name, or write '_ = ...' to throw it away"
            ),
            E::NotExhaustive { missing } => {
                write!(f, "Match is not exhaustive. Add arms for: {missing}")
            }
            E::Unreachable => write!(
                f,
                "This pattern is unreachable, because an earlier pattern matches the same values"
            ),
            E::CatchAll(missing) => write!(
                f,
                "This pattern stands for {}. Name {} instead, like `{} ->`",
                missing.join(", "),
                if missing.len() == 1 { "it" } else { "them" },
                missing.join(" | ")
            ),
            E::BindingMisses { missing } => write!(
                f,
                "A binding's pattern must match every value, and this one misses {missing}. Use `match` to handle it"
            ),
            E::OrNilReceiver => write!(
                f,
                "'or' on a Result(_, Nil) can't bind a value, because the error carries nothing"
            ),
            E::OrDropsError(ty) => write!(
                f,
                "This 'or' throws away the Result's error, of type '{ty}'. Bind it with 'e -> ...' and handle it"
            ),
            E::OrDiscardsReceiver(n) => write!(
                f,
                "'{n}' throws away the error. Give the 'or' receiver a name and use it"
            ),
            E::NilDiscard(n) => write!(
                f,
                "'{n}' always matches Nil here, so write the explicit 'Nil' pattern instead"
            ),
            E::Unused(n) => write!(
                f,
                "'{n}' is never used. Start its name with '_' if that's on purpose"
            ),
        }
    }
}

/// The end of a sentence that names a field some variant of `ty` lacks.
struct MissingOn<'a> {
    variant: &'a Name,
    ty: &'a Name,
}

impl fmt::Display for MissingOn<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "is not present on every variant of '{}' (missing on '{}')",
            self.ty, self.variant
        )
    }
}
