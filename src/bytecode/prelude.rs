use super::compiler::Compiler;
use crate::types::{InferType, Scheme};

// The prelude defines types and functions that are automatically available
// in every AL program without requiring an import.
//
// Primitive types (Int, Float, String, Bool, None) are built into the
// type system and are always available.
//
// Functions:
//   println(a) None     print any value to stdout
//   inspect(a) String   convert any value to its string representation

impl Compiler {
    pub(super) fn register_prelude(&mut self) {
        let a = self.engine.fresh_var();
        let n = self.engine.icon_none();
        let s = self.engine.icon_string();
        let a_id = match &a {
            InferType::IVar { id } => *id,
            _ => unreachable!(),
        };

        self.env.define(
            "println",
            Scheme {
                quantified: vec![a_id],
                ty: InferType::IFun {
                    params: vec![a.clone()],
                    ret: Box::new(n),
                    err: None,
                },
                def: None,
            },
        );

        self.env.define(
            "inspect",
            Scheme {
                quantified: vec![a_id],
                ty: InferType::IFun {
                    params: vec![a],
                    ret: Box::new(s),
                    err: None,
                },
                def: None,
            },
        );
    }
}
