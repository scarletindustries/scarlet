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
        let at = Span::DUMMY;
        let path = crate::ast::ImportPath::canonical(module::scarlet_prelude());
        // The prelude's type entries, by name, are the namespace every program
        // sees for types. So unlike an ordinary module, its compile must not
        // undo them. Nothing imports the prelude, so we load it as though the
        // entry did.
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
        let mut values = indexmap::IndexMap::new();
        for (name, ev) in &iface.values {
            if matches!(ev.scheme.kind, ValueKind::Constructor { .. }) {
                self.reserved.insert(name.clone());
            }
            values.insert(self.engine.intern(name), ev.scheme);
        }
        self.prelude_values = values;

        // Every identity check in the compiler compares against these refs
        // rather than matching strings.
        let engine = &self.engine;
        let prelude_values = &self.prelude_values;
        let value = |name: &str| {
            engine
                .lookup_str(name)
                .and_then(|id| prelude_values.get(&id))
                .copied()
        };
        match PreludeBindings::capture(&self.env, value) {
            Ok(b) => {
                self.prelude = b;
                self.engine.set_prim_ids(self.prelude.prim_ids());
            }
            Err(e) => self.error(e.to_string(), at),
        }
    }
}
