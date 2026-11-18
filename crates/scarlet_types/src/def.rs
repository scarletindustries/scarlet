//! What a module says a type is.

use crate::name::{Name, TypeName};
use crate::ty::{Type, TypeVar};
use crate::variance::Variance;
use scarlet_syntax::span::Span;

/// A `type` declaration, as an importer sees it. Field and alias types refer
/// to the parameters as [`Type::Var`]s, numbered from 0 in the order the
/// parameters are declared.
#[derive(Debug, Clone, PartialEq)]
pub struct TypeDef {
    pub name: TypeName,
    pub params: Vec<TypeVar>,
    pub body: TypeBody,
    pub public: bool,
    pub doc: Option<String>,
    /// The declaring name's span in its module.
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TypeBody {
    Variants {
        variants: Vec<VariantDef>,
        /// Whether importers can see the constructors. An opaque type's
        /// constructors exist, but only its own module can name them.
        constructors_public: bool,
        /// How each parameter is used, in `params` order, worked out from
        /// the fields.
        variance: Vec<Variance>,
    },
    /// The target, with the alias's parameters as variables. An alias never
    /// appears in a checked type, so it has no variance of its own.
    Alias(Type),
    /// A type the host backs, with no constructors at all: `Int`, a socket.
    /// Its parameters are invariant unless the declaration says `@covariant`,
    /// since nothing in Scarlet shows which way the host moves its values.
    External { variance: Vec<Variance> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct VariantDef {
    pub name: Name,
    pub fields: Vec<FieldDef>,
    pub doc: Option<String>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FieldDef {
    pub label: Name,
    pub ty: Type,
    pub span: Span,
}

impl TypeDef {
    pub fn variants(&self) -> Option<&[VariantDef]> {
        match &self.body {
            TypeBody::Variants { variants, .. } => Some(variants),
            TypeBody::Alias(_) | TypeBody::External { .. } => None,
        }
    }

    /// How parameter `param` is used. An alias, or a parameter the type
    /// doesn't have, is taken as invariant, which never generalises wrongly.
    pub fn variance(&self, param: usize) -> Variance {
        match &self.body {
            TypeBody::Variants { variance, .. } | TypeBody::External { variance } => {
                variance.get(param).copied().unwrap_or(Variance::Invariant)
            }
            TypeBody::Alias(_) => Variance::Invariant,
        }
    }

    /// The field types of variant `index`, with `args` put in for the type's
    /// parameters.
    pub fn field_types(&self, index: usize, args: &[Type]) -> Option<Vec<Type>> {
        let variant = self.variants()?.get(index)?;
        Some(
            variant
                .fields
                .iter()
                .map(|f| self.instantiate(&f.ty, args))
                .collect(),
        )
    }

    /// `ty`, written in terms of this type's parameters, with `args` put in
    /// for them.
    pub fn instantiate(&self, ty: &Type, args: &[Type]) -> Type {
        ty.substitute(&|v| {
            self.params
                .iter()
                .position(|p| p == v)
                .and_then(|i| args.get(i).cloned())
        })
    }
}
