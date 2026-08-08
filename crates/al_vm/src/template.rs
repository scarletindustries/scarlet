//! Descriptors for the enum values the VM constructs on its own — `Ok`/`Err`
//! results from I/O ops, `NetError` variants, HTTP protocol values, and the
//! like. A [`VariantTemplate`] is pure data (a nominal id plus names and
//! labels); the front end supplies one per variant it wants the runtime to be
//! able to build, so the VM never hard-codes a language's stdlib.

use crate::TypeId;

/// Static descriptor for one constructor the runtime may instantiate (type
/// id, type/variant name, field labels). AL's build.rs emits these as
/// `stdlib::<module>::<CTOR>` consts so a rename in the AL source surfaces as
/// a Rust compile error at the VM usage site rather than silently
/// constructing a mismatched value.
#[derive(Debug)]
pub struct VariantTemplate {
    pub type_id: TypeId,
    pub variant_idx: u16,
    pub type_name: &'static str,
    pub variant_name: &'static str,
    pub labels: &'static [&'static str],
}
