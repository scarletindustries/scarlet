//! Prelude bootstrap: load `src/std/al.al` into a fresh compiler so its
//! names are visible in every AL program without an import.
//!
//! The prelude is real AL source — primitives, `Nil`/`Bool`/`Option`/
//! `Result`, `println`/`inspect` — declared via external/`@vm` and loaded
//! through the ordinary module pipeline, so the exact parse/analyse path
//! that handles user modules produces it; nothing here redefines a prelude
//! type in Rust.
//!
//! Runs exactly once per compiler, before any user code is seen. Its last
//! act is `PreludeBindings::capture` (see `prelude_bindings.rs`): a strict
//! typed snapshot of every prelude name the compiler relies on, so a
//! drifted `al.al` fails loudly here instead of surfacing as a confused
//! unify error somewhere downstream.

use super::PreludeBindings;
use super::compiler::Compiler;
use crate::module;
use crate::span::Span;
use crate::types::ValueKind;

impl Compiler {
    pub(crate) fn register_prelude(&mut self) {
        // 1. Load al.al via the module pipeline. analyse_module pushes/pops its
        //    own env scope, so while type heads land in the flat env.type_info,
        //    value schemes (Some/None/Ok/Err/Nil/True/False/println/...) are
        //    defined into a scope that is immediately popped. Pull them back
        //    from the recorded ModuleInterface into the root scope so they are
        //    visible everywhere without an explicit import.
        let at = Span::DUMMY;
        let path = crate::ast::ImportPath::canonical(module::al_prelude());
        let Some((_, key)) = self.load_module(&path, at) else {
            return;
        };
        #[allow(clippy::expect_used)]
        let iface = self
            .module_table
            .get(&key)
            .expect("load_module succeeded so prelude must be in module_table");
        for name in iface.types.keys() {
            self.reserved.insert(name.clone());
        }
        for (name, ev) in &iface.values {
            if matches!(ev.scheme.kind, ValueKind::Constructor { .. }) {
                self.reserved.insert(name.clone());
            }
            self.env.define(name, ev.scheme);
        }

        // 2. Capture strict bindings. After this, every identity check in the
        //    compiler compares against these refs — no string matching. A
        //    missing or mis-shaped name is a compile error here, not a silent
        //    mistype later.
        match PreludeBindings::capture(&self.env) {
            Ok(b) => {
                self.prelude = b;
                self.engine.set_prim_ids(self.prelude.prim_ids());
            }
            Err(e) => self.error(e.to_string(), at),
        }
    }
}
