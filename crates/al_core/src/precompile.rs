//! Build-time stdlib precompilation. `precompile_stdlib` runs the full
//! parse/analyse/codegen pipeline over `src/std/**` once at `cargo build` time
//! (via `crates/al/build.rs`); `static_ir::flatten` then lowers the result to
//! pool-of-array form which build.rs emits as Rust source. At runtime
//! `Compiler::seed_static` consults that generated `&'static StaticStdlib` —
//! nothing is parsed or deserialized.
//!
//! Because this runs inside `build.rs`, any error in `al.al` or a stdlib
//! module surfaces as a *cargo build* failure with the diagnostic text — the
//! `PreludeBindings::capture` shape assertions become a compile-time contract.

use std::collections::HashSet;

use indexmap::IndexMap;

use crate::bytecode::{PreludeBindings, Program, compiler::Compiler, new_compiler};
use crate::diagnostic::{self, has_errors};
use crate::module::{self, ModuleInterface, stdlib};
use crate::span::Span;
use crate::types::{InferEngine, TypeBody, TypeInfo};

/// Everything `precompile_stdlib` extracts. Consumed only by build.rs.
#[derive(Debug, Clone)]
pub struct PrecompileOutput {
    pub blob: PrecompiledBlob,
    pub prelude: PreludeBindings,
    pub reserved: HashSet<String>,
    pub next_type_id: crate::type_def::TypeId,
}

/// The variable-size data: module interfaces, the global type table, and the
/// stdlib bytecode/functions/constants. Flattened by `static_ir::flatten`.
#[derive(Debug, Clone)]
pub struct PrecompiledBlob {
    pub interfaces: IndexMap<String, ModuleInterface>,
    pub type_info: IndexMap<String, TypeInfo>,
    pub program: Program,
    pub local_count: i32,
}

/// Compile the entire embedded stdlib (prelude + every `al/*.al` module) and
/// snapshot the resulting state. Returns the `InferEngine` alongside so
/// `flatten` can copy its `nodes`/`children`/`strings` arenas wholesale —
/// every `Ty` in the output indexes into that arena.
pub fn precompile_stdlib() -> Result<(PrecompileOutput, InferEngine), String> {
    let mut c = new_compiler(None, false);

    c.register_prelude();
    bail_on_errors(&c, "prelude")?;

    let at = Span::DUMMY;
    for path in stdlib::all_modules() {
        c.load_module(&path, at);
        bail_on_errors(&c, &module::path_key(&path))?;
    }

    let mut interfaces: IndexMap<String, ModuleInterface> =
        c.take_module_table().into_iter().collect();
    let mut type_info = c.take_type_info();

    // Close every TypeInfo body so its `Var(param_id)` refs become `Bound(idx)`.
    // This mints new arena nodes, so it must happen on the same engine whose
    // arena `flatten` is about to snapshot. `iface.types` entries alias the
    // same engine pools (they were copied from `env.type_info`), so closing
    // via `type_info` covers Custom bodies (their fields live in the shared
    // engine.variant_fields arena); an Alias body's target is stored on the
    // TypeInfo struct itself, so each interface copy is closed separately
    // below.
    let (program, mut engine) = c.into_parts();
    for ti in type_info.values_mut() {
        close_type_info(&mut engine, ti);
    }
    for iface in interfaces.values_mut() {
        for ti in iface.types.values_mut() {
            if let TypeBody::Alias { .. } = ti.body {
                close_type_info(&mut engine, ti);
            }
        }
    }

    let out = PrecompileOutput {
        prelude: program.1.clone(),
        reserved: program.2.clone(),
        next_type_id: program.3,
        blob: PrecompiledBlob {
            interfaces,
            type_info,
            local_count: program.4,
            program: program.0,
        },
    };
    Ok((out, engine))
}

fn close_type_info(engine: &mut InferEngine, ti: &mut TypeInfo) {
    match ti.body {
        TypeBody::Custom { variants } => {
            // The fields live in `engine.variant_fields`; rewrite their `ty`
            // in place. Distinct types occupy disjoint slices.
            for v in variants.range() {
                let fields = engine.variants[v].fields;
                for f in fields.range() {
                    let ty = engine.variant_fields[f].ty;
                    engine.variant_fields[f].ty = engine.close_body(ty, ti.type_params);
                }
            }
        }
        TypeBody::Alias { target } => {
            ti.body = TypeBody::Alias {
                target: engine.close_body(target, ti.type_params),
            };
        }
        TypeBody::Unresolved | TypeBody::External => {}
    }
}

fn bail_on_errors(c: &Compiler, label: &str) -> Result<(), String> {
    let diags = c.diagnostics();
    if has_errors(diags) {
        let rendered: Vec<String> = diags
            .iter()
            .filter(|d| d.severity == diagnostic::Severity::Error)
            .map(|d| {
                format!(
                    "{}:{}:{}: {}",
                    label,
                    d.span.start_line + 1,
                    d.span.start_column + 1,
                    d.message
                )
            })
            .collect();
        return Err(format!(
            "stdlib compile failed in '{label}':\n  {}",
            rendered.join("\n  ")
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // `precompile_stdlib` is the build.rs entry point: it parses, type-checks
    // and code-gens the entire embedded stdlib from scratch (no static
    // fallback). At runtime this never executes — the binary's `.rodata` is the
    // flattened result — so the only place it is exercised is here and in
    // `build.rs`. A regression in any stdlib module, the prelude, or the
    // `TypeBody`-closing pass would surface as an `Err` here.
    #[test]
    fn precompile_stdlib_succeeds_and_has_expected_shape() {
        let (out, engine) =
            precompile_stdlib().expect("stdlib must precompile without diagnostics");

        // Every embedded `al/*` module compiled into an interface, keyed by its
        // slash-joined path. `all_modules()` (module/stdlib.rs) discovers these.
        for key in [
            "al/array",
            "al/string",
            "al/option",
            "al/result",
            "al/int",
            "al/float",
            "al/decimal",
            "al/binary",
            "al/bool",
            "al/net",
            "al/io",
            "al/time",
            "al/experiments/scheduler",
            "al/http",
            "al/http/status",
            "al/http/headers",
            "al/http/body",
            "al/http/h1",
        ] {
            assert!(
                out.blob.interfaces.contains_key(key),
                "missing precompiled interface for '{key}'; got {:?}",
                out.blob.interfaces.keys().collect::<Vec<_>>()
            );
        }

        // al/array exports the pure-AL combinators as values.
        let array = &out.blob.interfaces["al/array"];
        for f in ["map", "filter", "fold", "reverse", "length", "contains"] {
            assert!(array.values.contains_key(f), "al/array should export '{f}'");
        }

        // The al/http stack compiled with its spec-fixed public surface. These
        // names are the contract the H1 server core is built on;
        // a renamed or dropped export would silently break the connection driver.
        let expected_exports: &[(&str, &[&str])] = &[
            (
                "al/http",
                &["serve", "text", "ok", "not_found", "with_header"],
            ),
            ("al/http/status", &["reason_phrase"]),
            (
                "al/http/headers",
                &["get", "has", "set", "append", "render"],
            ),
            (
                "al/http/body",
                &["empty", "from_binary", "pull", "drain", "collect"],
            ),
            (
                "al/http/h1",
                &["should_close", "want_100_continue", "serialize_head"],
            ),
        ];
        for (module, fns) in expected_exports {
            let iface = &out.blob.interfaces[*module];
            for f in *fns {
                assert!(
                    iface.values.contains_key(*f),
                    "{module} should export '{f}'; got {:?}",
                    iface.values.keys().collect::<Vec<_>>()
                );
            }
        }

        // The shared program carries real code: functions, instructions and a
        // constant pool. The entry index is in range.
        let prog = &out.blob.program;
        assert!(!prog.functions.is_empty(), "stdlib has no functions");
        assert!(!prog.code.is_empty(), "stdlib emitted no bytecode");
        assert!((prog.entry as usize) < prog.functions.len());

        // Prelude bindings resolved to distinct, real type ids and correctly
        // shaped constructors. These are the contract `PreludeBindings::capture`
        // enforces; assert it held end-to-end.
        let p = &out.prelude;
        assert_eq!(p.int.name, "Int");
        assert_eq!(p.option.name, "Option");
        assert_eq!(p.result.name, "Result");
        assert_ne!(p.option.id, p.result.id, "Option/Result share a type id");
        assert_ne!(p.option.id, p.nil.id, "Option/Nil share a type id");
        assert_eq!(p.some.arity, 1, "Some carries one payload");
        assert_eq!(p.none.arity, 0, "None is nullary");
        assert_eq!(p.ok.arity, 1);
        assert_eq!(p.err.arity, 1);
        assert_eq!(p.true_.arity, 0);
        assert_eq!(p.some.type_id, p.option.id, "Some belongs to Option");
        assert_eq!(p.ok.type_id, p.result.id, "Ok belongs to Result");
        assert_ne!(
            p.some.variant_idx, p.none.variant_idx,
            "Some/None are distinct variants"
        );

        // Type ids handed to the first user module must sit past every stdlib
        // id, so next_type_id is strictly positive.
        assert!(out.next_type_id.0 > 0, "next_type_id should be positive");

        // The prelude reserves its own constructor/type names so a user can't
        // shadow them.
        for name in ["True", "False", "Some", "None", "Ok", "Err"] {
            assert!(
                out.reserved.contains(name),
                "'{name}' should be a reserved prelude name"
            );
        }

        // The returned engine's arena is non-empty (every `Ty` in the snapshot
        // indexes into it) and the close-bodies pass minted nodes onto it.
        assert!(!engine.nodes.is_empty(), "engine arena is empty");
        assert!(
            !engine.strings.is_empty(),
            "engine interned no type-side strings"
        );
    }
}
