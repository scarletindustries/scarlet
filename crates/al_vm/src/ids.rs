//! Runtime identities shared between the VM and any front end: the nominal
//! [`TypeId`] a tagged value carries in its header, and the [`FuncIdx`]
//! naming a slot in [`Program::functions`](crate::bytecode::Program).
//! Both are minted by the front end at compile time and carried opaquely by
//! the VM — they live here because the VM stores and compares them.

use std::fmt;

use crate::newtype_index;

/// Nominal identity of a user-declared type, allocated once per declaration
/// by the front end (AL's `TypeEnv::register_type_head`). A newtype so a
/// var-id, string id, ctor-index, or slot number cannot be silently passed
/// where a nominal type id is expected — every such site is now a compile
/// error. `repr(transparent)` keeps the runtime encoding a single `i32` word
/// (the VM stores it in an enum value's header).
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

newtype_index!(
    /// Index into `Program.functions` — one numbering shared with the front
    /// end's function tables (`CoreProgram.fns` / `TypedProgram::fns` in
    /// `al_core`). Minted only where the `Function` slot it names is
    /// reserved; everything downstream carries it opaquely until
    /// [`FuncIdx::to_operand`] writes it into a `CallKnown`/`MakeClosure`
    /// immediate.
    pub struct FuncIdx("fn#")
);

impl FuncIdx {
    /// The call/closure-operand encoding: the `i32` immediate `Op::CallKnown`
    /// and `Op::MakeClosure` carry.
    #[inline]
    pub fn to_operand(self) -> i32 {
        self.0 as i32
    }
}
