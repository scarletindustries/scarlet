//! Prelude bootstrap: load `src/std/scarlet.sca` through the ordinary module
//! pipeline so its names are visible in every Scarlet program without an import.
//! Nothing here redefines a prelude type in Rust.
//!
//! Runs once per compiler, before any user code. It ends in
//! `PreludeBindings::capture`, a strict typed snapshot of every prelude name
//! the compiler relies on, so a drifted `scarlet.sca` fails here instead of
//! surfacing as a confused unify error downstream.

use super::PreludeBindings;
use super::compiler::Compiler;
use crate::module;
use crate::span::Span;
use crate::types::ValueKind;

impl Compiler {
    pub(crate) fn register_prelude(&mut self) {
        // `analyse_module` defines the value schemes into a scope it pops, so
        // pull them back out of the recorded interface into the root scope.
        // Type heads need no such rescue: they land in the flat env.type_info.
        let at = Span::DUMMY;
        let path = crate::ast::ImportPath::canonical(module::scarlet_prelude());
        // The prelude's residue IS the ambient namespace ("Type heads need no
        // rescue" above depends on it), so its compile must not be
        // frame-scoped like an ordinary module's.
        let loaded = self.with_retained_namespaces(|c| c.load_module(&path, at));
        let Some((_, key)) = loaded else {
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

        // Every identity check in the compiler compares against these refs
        // rather than matching strings.
        match PreludeBindings::capture(&self.env) {
            Ok(b) => {
                self.prelude = b;
                self.engine.set_prim_ids(self.prelude.prim_ids());
            }
            Err(e) => self.error(e.to_string(), at),
        }
    }
}
