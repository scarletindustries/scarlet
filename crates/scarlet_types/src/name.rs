//! Canonical identities for the things a module defines. Every map in the
//! middle of the compiler keys on one of these, never on a bare name, because
//! two modules can each define a `Parsed`.

use std::fmt;
use std::rc::Rc;

pub use scarlet_syntax::module_path::ModuleKey;

/// A name as written in source: a type, value, constructor, field or label.
pub type Name = Rc<str>;

/// A nominal type's identity: the module that declares it and its name there.
/// A user's `type Parsed` is not `scarlet/http/h1.Parsed`.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct TypeName(Rc<(ModuleKey, Name)>);

impl TypeName {
    pub fn new(module: ModuleKey, name: Name) -> Self {
        TypeName(Rc::new((module, name)))
    }

    pub fn module(&self) -> &ModuleKey {
        &self.0.0
    }

    pub fn name(&self) -> &Name {
        &self.0.1
    }
}

impl fmt::Debug for TypeName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.module(), self.name())
    }
}

/// A module-level value's identity: a function, const or builtin.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ValueName {
    pub module: ModuleKey,
    pub name: Name,
}

impl fmt::Debug for ValueName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.module, self.name)
    }
}

/// A constructor's identity: its type and its index among that type's
/// variants, in declaration order.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct CtorName {
    pub type_name: TypeName,
    pub variant: u16,
}
