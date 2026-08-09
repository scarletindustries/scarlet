//! AL's embedding of the language-agnostic VM (`al_vm::vm`). The stdlib
//! constructor bindings ride inside the compiled `Program`
//! (`templates`/`abi`, bound by `al_core`'s `bind_abi`), so nothing here
//! supplements them.

pub use al_vm::vm::*;
