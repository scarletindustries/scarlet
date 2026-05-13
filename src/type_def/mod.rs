use indexmap::IndexMap;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrimitiveKind {
    Int,
    Float,
    String,
    Bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Type {
    Primitive {
        kind: PrimitiveKind,
    },
    Array {
        element: Box<Type>,
    },
    Option {
        inner: Box<Type>,
    },
    Function {
        params: Vec<Type>,
        ret: Box<Type>,
        error_type: Option<Box<Type>>,
    },
    Struct {
        id: i32,
        name: String,
        type_params: Vec<String>,
        type_args: Vec<Type>,
        fields: IndexMap<String, Type>,
    },
    Enum {
        id: i32,
        name: String,
        type_params: Vec<String>,
        type_args: Vec<Type>,
        variants: IndexMap<String, Vec<Type>>,
    },
    None,
    Var {
        name: String,
    },
    Result {
        success: Box<Type>,
        error: Box<Type>,
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

pub fn t_bool() -> Type {
    Type::Primitive {
        kind: PrimitiveKind::Bool,
    }
}

pub fn t_none() -> Type {
    Type::None
}

pub fn t_var(name: impl Into<String>) -> Type {
    Type::Var { name: name.into() }
}

pub fn t_array(element: Type) -> Type {
    Type::Array {
        element: Box::new(element),
    }
}

pub fn t_option(inner: Type) -> Type {
    Type::Option {
        inner: Box::new(inner),
    }
}

pub fn t_tuple(elements: Vec<Type>) -> Type {
    Type::Tuple { elements }
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Type::Primitive { kind } => match kind {
                PrimitiveKind::Int => write!(f, "Int"),
                PrimitiveKind::Float => write!(f, "Float"),
                PrimitiveKind::String => write!(f, "String"),
                PrimitiveKind::Bool => write!(f, "Bool"),
            },
            Type::Array { element } => write!(f, "[]{}", element),
            Type::Option { inner } => write!(f, "?{}", inner),
            Type::Function {
                params,
                ret,
                error_type,
            } => {
                let params: Vec<String> = params.iter().map(|p| p.to_string()).collect();
                write!(f, "fn({}) {}", params.join(", "), ret)?;
                if let Some(err) = error_type {
                    write!(f, "!{}", err)?;
                }
                Ok(())
            }
            Type::Struct {
                name, type_args, ..
            } => {
                if !type_args.is_empty() {
                    let args: Vec<String> = type_args.iter().map(|a| a.to_string()).collect();
                    write!(f, "{}({})", name, args.join(", "))
                } else {
                    write!(f, "{}", name)
                }
            }
            Type::Enum {
                name, type_args, ..
            } => {
                if !type_args.is_empty() {
                    let args: Vec<String> = type_args.iter().map(|a| a.to_string()).collect();
                    write!(f, "{}({})", name, args.join(", "))
                } else {
                    write!(f, "{}", name)
                }
            }
            Type::None => write!(f, "none"),
            Type::Var { name } => write!(f, "{}", name),
            Type::Result { success, error } => write!(f, "{}!{}", success, error),
            Type::Tuple { elements } => {
                let elems: Vec<String> = elements.iter().map(|e| e.to_string()).collect();
                write!(f, "({})", elems.join(", "))
            }
        }
    }
}

pub fn type_to_string(t: &Type) -> String {
    t.to_string()
}
