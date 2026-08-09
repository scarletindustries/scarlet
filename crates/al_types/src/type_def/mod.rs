use indexmap::IndexMap;
use std::fmt;

pub use al_vm::TypeId;

/// Names of the prelude types that are not `Type::Named`. The only prelude
/// name strings outside `bytecode::prelude_bindings`.
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

/// A labelled field of a constructor variant, already substituted. The
/// unsubstituted template form is `environment::VariantField`.
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
    /// A user-defined nominal type. `variants` has field types already
    /// substituted for `type_args`, so consumers need no environment lookup.
    /// A struct is the single-variant case whose ctor name is the type name.
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
