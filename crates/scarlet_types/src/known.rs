//! The prelude types the compiler itself depends on, by identity. The
//! prelude, `scarlet.sca`, declares them like any other type. These are just
//! their canonical names, so nothing compares a type by its bare name.

use crate::name::{ModuleKey, Name, TypeName};
use crate::ty::Prim;

/// The key of the prelude module, `scarlet`.
pub fn prelude_key() -> ModuleKey {
    ModuleKey::for_stdlib(&scarlet_syntax::module_path::scarlet_prelude())
}

#[derive(Debug, Clone)]
pub struct Known {
    pub int: TypeName,
    pub float: TypeName,
    pub string: TypeName,
    pub array: TypeName,
    pub binary: TypeName,
    pub bool: TypeName,
    pub nil: TypeName,
    pub option: TypeName,
    pub result: TypeName,
}

impl Known {
    pub fn new() -> Self {
        let key = prelude_key();
        let n = |s: &str| TypeName::new(key.clone(), Name::from(s));
        Known {
            int: n("Int"),
            float: n("Float"),
            string: n("String"),
            array: n("Array"),
            binary: n("Binary"),
            bool: n("Bool"),
            nil: n("Nil"),
            option: n("Option"),
            result: n("Result"),
        }
    }

    pub fn prim_of(&self, name: &TypeName) -> Option<Prim> {
        if *name == self.int {
            Some(Prim::Int)
        } else if *name == self.float {
            Some(Prim::Float)
        } else if *name == self.string {
            Some(Prim::String)
        } else {
            None
        }
    }
}

impl Default for Known {
    fn default() -> Self {
        Known::new()
    }
}
