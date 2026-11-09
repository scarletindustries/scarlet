//! Loads the prelude, `src/std/scarlet.sca`, through the ordinary module
//! pipeline so every Scarlet program can use its names without an import.
//! Nothing here redefines a prelude type in Rust.
//!
//! This runs once per compiler, before any user code. It ends in
//! `PreludeBindings::capture`, which takes a strict, typed snapshot of every
//! prelude name the compiler relies on. So if `scarlet.sca` drifts, it fails
//! here and not as a confusing unify error later on.

use super::PreludeBindings;
use super::compiler::Compiler;
use crate::module;
use crate::span::Span;
use crate::types::ValueKind;

impl Compiler {
    pub(crate) fn register_prelude(&mut self) {
        // `analyse_module` defines the value schemes into a scope it pops, so
        // pull them back out of the recorded interface into the root scope.
        // Type heads don't need this, because they land in the flat `env.type_info`.
        let at = Span::DUMMY;
        let path = crate::ast::ImportPath::canonical(module::scarlet_prelude());
        // What the prelude leaves behind is the namespace every program sees,
        // and the note above about type heads depends on that. So unlike an
        // ordinary module, its compile must not be scoped to a frame.
        // Nothing imports the prelude, so we load it as though the entry did.
        let entry = module::ModuleKey::main();
        let importer = super::compiler::Importer {
            dir: None,
            key: &entry,
        };
        let loaded = self.with_retained_namespaces(|c| c.load_module(&path, at, &importer));
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
