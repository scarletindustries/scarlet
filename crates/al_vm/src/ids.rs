//! Runtime identities the front end mints at compile time and the VM carries
//! opaquely: [`TypeId`] in a tagged value's header, and [`FuncIdx`] naming a
//! slot in [`Program::functions`](crate::bytecode::Program).

use std::fmt;

use crate::newtype_index;

/// Nominal identity of a user-declared type, allocated once per declaration by
/// the front end (`TypeEnv::register_type_head`). `repr(transparent)` keeps the
/// runtime encoding a single `i32` word in an enum value's header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(transparent)]
pub struct TypeId(pub i32);

impl TypeId {
    /// Sentinel meaning "no nominal type"; real ids start at 1. Deliberately
    /// not `Default`, so a derived `Default` cannot manufacture it.
    pub const NONE: TypeId = TypeId(0);
}

impl fmt::Display for TypeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

newtype_index!(
    /// Index into `Program.functions`. Same numbering as the front end's
    /// `CoreProgram.fns` / `TypedProgram::fns` in `al_core`; mint one only
    /// where the `Function` slot it names is reserved.
    pub struct FuncIdx("fn#")
);

impl FuncIdx {
    /// The `i32` immediate `Op::CallKnown` and `Op::MakeClosure` carry.
    #[inline]
    pub fn to_operand(self) -> i32 {
        self.0 as i32
    }
}
