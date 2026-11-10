//! What one module shows the modules that import it. It is a plain value with
//! no reference into the check that made it, so it can be cached, compared and
//! handed to any later check.

use std::collections::BTreeSet;

use indexmap::IndexMap;

use crate::def::TypeDef;
use crate::name::{CtorName, ModuleKey, Name};
use crate::ty::Scheme;
use scarlet_ir::intrinsic::Intrinsic;
use scarlet_syntax::span::Span;

#[derive(Debug, Clone, PartialEq)]
pub struct Interface {
    pub key: ModuleKey,
    /// Every type the module declares, public or not, because a public value
    /// can mention a private type. `TypeDef::public` says which can be named.
    pub types: IndexMap<Name, TypeDef>,
    /// The public values, constructors included.
    pub values: IndexMap<Name, ValueDef>,
    /// Names the module declares but doesn't export, so a lookup can say
    /// "private" instead of "not found".
    pub private_names: BTreeSet<Name>,
    /// The module's leading `/** */` comment.
    pub doc: Option<String>,
}

impl Interface {
    pub fn new(key: ModuleKey) -> Self {
        Interface {
            key,
            types: IndexMap::new(),
            values: IndexMap::new(),
            private_names: BTreeSet::new(),
            doc: None,
        }
    }

    /// A type an importer can name.
    pub fn public_type(&self, name: &str) -> Option<&TypeDef> {
        self.types.get(name).filter(|t| t.public)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ValueDef {
    pub scheme: Scheme,
    pub kind: ValueKind,
    pub doc: Option<String>,
    /// The declaring name's span in its module.
    pub span: Span,
}

/// What a module-level name is.
#[derive(Debug, Clone, PartialEq)]
pub enum ValueKind {
    /// A `fn` declaration, with its parameter names in order. A call may use
    /// them as labels.
    Fn { params: Vec<Name> },
    /// A `@vm` function: the VM runs it directly.
    Builtin {
        intrinsic: Intrinsic,
        params: Vec<Name>,
    },
    Const,
    Constructor {
        ctor: CtorName,
        /// The declared name of the variant, whatever name the use spelled.
        name: Name,
        labels: Vec<Name>,
    },
}

impl ValueKind {
    /// The names a call can label its arguments with, in parameter order.
    pub fn labels(&self) -> Option<&[Name]> {
        match self {
            ValueKind::Fn { params } | ValueKind::Builtin { params, .. } => Some(params),
            ValueKind::Constructor { labels, .. } => Some(labels),
            ValueKind::Const => None,
        }
    }
}
