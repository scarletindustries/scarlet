use indexmap::IndexMap;
use std::fmt;

/// Nominal identity of a user-declared type, allocated once per declaration by
/// [`TypeEnv::register_type_head`](crate::types::TypeEnv::register_type_head).
/// A newtype so a var-id, `StrId`, ctor-index, or slot number cannot be
/// silently passed where a nominal type id is expected — every such site is now
/// a compile error. `repr(transparent)` keeps the runtime encoding a single
/// `i32` word (the VM stores it in an enum value's header).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(transparent)]
pub struct TypeId(pub i32);

impl TypeId {
    /// Sentinel meaning "no nominal type". Never a real id: allocation starts
    /// at 1. Used for pre-prelude placeholders and "not a Con" fallbacks.
    /// Deliberately NOT `Default`: a derived `Default` on a struct embedding a
    /// `TypeId` would silently manufacture this sentinel. (A `NonZeroI32`
    /// niche — making `Option<TypeId>` free and the sentinel unrepresentable —
    /// is the eventual replacement, deferred as too broad for now.)
    pub const NONE: TypeId = TypeId(0);
}

impl fmt::Display for TypeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Names of the *structural* prelude types — those whose `Type` is not
/// `Named` (because exhaustiveness/array-pattern handling needs the dedicated
/// shape). These four are the only prelude name strings outside
/// `bytecode::prelude_bindings`.
pub mod prim_names {
    pub const INT: &str = "Int";
    pub const FLOAT: &str = "Float";
    pub const STRING: &str = "String";
    pub const ARRAY: &str = "Array";
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrimitiveKind {
    Int,
    Float,
    String,
}

impl PrimitiveKind {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Int => prim_names::INT,
            Self::Float => prim_names::FLOAT,
            Self::String => prim_names::STRING,
        }
    }
}

/// A labelled field of a constructor variant in the substituted
/// `Type::Named` form consumed by exhaustiveness/field-access. The template
/// form stored in `TypeInfo` is `environment::VariantField`, not this.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldDef {
    pub label: String,
    pub ty: Type,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Type {
    Primitive {
        kind: PrimitiveKind,
    },
    Array {
        element: Box<Type>,
    },
    Function {
        params: Vec<Type>,
        ret: Box<Type>,
    },
    /// A user-defined nominal type. `variants` carries the constructor set with
    /// field types already substituted for `type_args`, so consumers like
    /// exhaustiveness checking and field access need no further environment
    /// lookups. A "struct" is the single-variant case whose constructor name
    /// equals the type name.
    Named {
        id: TypeId,
        name: String,
        type_args: Vec<Type>,
        variants: IndexMap<String, Vec<FieldDef>>,
    },
    Var {
        name: String,
    },
    Tuple {
        elements: Vec<Type>,
    },
}

pub fn t_int() -> Type {
    Type::Primitive {
        kind: PrimitiveKind::Int,
    }
}

pub fn t_float() -> Type {
    Type::Primitive {
        kind: PrimitiveKind::Float,
    }
}

pub fn t_string() -> Type {
    Type::Primitive {
        kind: PrimitiveKind::String,
    }
}

pub fn t_var(name: impl Into<String>) -> Type {
    Type::Var { name: name.into() }
}

pub fn t_array(element: Type) -> Type {
    Type::Array {
        element: Box::new(element),
    }
}

pub fn t_tuple(elements: Vec<Type>) -> Type {
    Type::Tuple { elements }
}

pub fn t_named(
    id: TypeId,
    name: impl Into<String>,
    type_args: Vec<Type>,
    variants: IndexMap<String, Vec<FieldDef>>,
) -> Type {
    Type::Named {
        id,
        name: name.into(),
        type_args,
        variants,
    }
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Type::Primitive { kind } => f.write_str(kind.name()),
            Type::Array { element } => write!(f, "Array({})", element),
            Type::Function { params, ret } => {
                let params: Vec<String> = params.iter().map(|p| p.to_string()).collect();
                write!(f, "fn({}) {}", params.join(", "), ret)
            }
            Type::Named {
                name, type_args, ..
            } => {
                if type_args.is_empty() {
                    write!(f, "{}", name)
                } else {
                    let args: Vec<String> = type_args.iter().map(|a| a.to_string()).collect();
                    write!(f, "{}({})", name, args.join(", "))
                }
            }
            Type::Var { name } => write!(f, "{}", name),
            Type::Tuple { elements } => {
                let elems: Vec<String> = elements.iter().map(|e| e.to_string()).collect();
                write!(f, "({})", elems.join(", "))
            }
        }
    }
}
