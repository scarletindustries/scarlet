//! Build-time stdlib precompilation. `crates/al/build.rs` runs
//! `precompile_stdlib` over `src/std/**`, `static_ir::flatten` lowers the
//! result to pool-of-array form, and build.rs emits that as Rust source. At
//! runtime `Compiler::seed_static` reads the generated `&'static
//! StaticStdlib` — nothing is parsed or deserialized. Any stdlib error is a
//! `cargo build` failure.
//!
//! Every index the blob carries is frozen into `.rodata` and can never be
//! re-minted, so it may only point into an arena the blob also carries. Only
//! the `InferEngine` pools qualify: `flatten` copies them wholesale and
//! `seed_static` memcpys them back as the live engine's prefix. A
//! compile-local arena must never appear. The post-inference `ResolvedPool`
//! stays out because the stdlib is frozen after `emit`, not before `lower`, so
//! no stdlib body is ever re-lowered from the blob. Pinned by
//! `blob_freezes_exactly_the_engine_pools` and
//! `precompile_output_carries_no_resolved_pool` below.
//!
//! Determinism is a correctness property. Two builds of the same source must
//! mint identical indices or `cargo build` is irreproducible, and silently so
//! — each blob is internally consistent. `precompile_stdlib_is_deterministic`
//! runs the pipeline twice. Jump operands are function-relative, so `code` is
//! copied verbatim and nothing here rewrites an operand.

use std::collections::BTreeSet;

use indexmap::IndexMap;

use crate::bytecode::{
    PreludeBindings, Program,
    compiler::{Compiler, CompilerParts},
    new_compiler,
};
use crate::diagnostic::{self, has_errors};
use crate::module::{ModuleInterface, ModuleKey, stdlib};
use crate::span::Span;
use crate::types::{InferEngine, TypeBody, TypeInfo};

/// Everything `precompile_stdlib` extracts. Consumed only by build.rs.
#[derive(Debug, Clone)]
pub struct PrecompileOutput {
    pub blob: PrecompiledBlob,
    pub prelude: PreludeBindings,
    /// `BTreeSet` so `flatten` copies it out sorted; `is_reserved` binary-searches.
    pub reserved: BTreeSet<String>,
    pub next_type_id: crate::type_def::TypeId,
}

/// The variable-size data, flattened by `static_ir::flatten`.
#[derive(Debug, Clone)]
pub struct PrecompiledBlob {
    pub interfaces: IndexMap<String, ModuleInterface>,
    pub type_info: IndexMap<String, TypeInfo>,
    pub program: Program,
    pub local_count: i32,
}

/// Compile the whole embedded stdlib and snapshot the result. The
/// `InferEngine` comes back too: every `Ty` in the output indexes its arenas,
/// which `flatten` copies wholesale.
pub fn precompile_stdlib() -> Result<(PrecompileOutput, InferEngine), String> {
    let mut c = new_compiler(None, false);

    c.register_prelude();
    bail_on_errors(&c, "prelude")?;

    let at = Span::DUMMY;
    for path in stdlib::all_modules() {
        c.load_module(&crate::ast::ImportPath::canonical(path.clone()), at);
        bail_on_errors(&c, ModuleKey::for_stdlib(&path).as_str())?;
    }

    let mut interfaces: IndexMap<String, ModuleInterface> =
        c.take_module_table().into_iter().collect();
    let mut type_info = c.take_type_info();

    // Close every TypeInfo body so `Var(param_id)` becomes `Bound(idx)`. This
    // mints arena nodes, so it must run on the engine `flatten` will snapshot.
    // Custom bodies are shared through `engine.variant_fields`, but an Alias
    // target lives on the `TypeInfo` itself, so each interface copy needs its
    // own pass.
    let (parts, mut engine) = c.into_parts();
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

    let CompilerParts {
        program,
        prelude,
        reserved,
        next_type_id,
        local_count,
    } = parts;
    let out = PrecompileOutput {
        prelude,
        reserved,
        next_type_id,
        blob: PrecompiledBlob {
            interfaces,
            type_info,
            local_count,
            program,
        },
    };
    Ok((out, engine))
}

#[allow(clippy::panic)] // build-time only: freezing a placeholder body must fail the build
fn close_type_info(engine: &mut InferEngine, ti: &mut TypeInfo) {
    match ti.body {
        TypeBody::Custom { variants } => {
            // Distinct types occupy disjoint `variant_fields` slices, so
            // rewriting `ty` in place is safe.
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
        // Unresolved here means a stdlib type declaration never got a body,
        // and freezing would bake the placeholder into `.rodata`.
        TypeBody::External => {}
        TypeBody::Unresolved => panic!(
            "type body still unresolved at stdlib freeze: {}",
            engine.str(ti.name)
        ),
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

    // The only place `precompile_stdlib` runs outside build.rs, so this is
    // where a regression in a stdlib module, the prelude, or the
    // `TypeBody`-closing pass shows up.
    #[test]
    fn precompile_stdlib_succeeds_and_has_expected_shape() {
        let (out, engine) =
            precompile_stdlib().expect("stdlib must precompile without diagnostics");

        // Keyed by slash-joined path; `module/stdlib.rs` discovers these.
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
            "al/scheduler",
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

        // The contract the H1 server core is built on: renaming or dropping
        // one of these silently breaks the connection driver.
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

        let prog = &out.blob.program;
        assert!(!prog.functions.is_empty(), "stdlib has no functions");
        assert!(!prog.code.is_empty(), "stdlib emitted no bytecode");
        assert!((prog.entry as usize) < prog.functions.len());

        // The contract `PreludeBindings::capture` enforces, checked
        // end-to-end.
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

        // The first user module's ids must sit past every stdlib id.
        assert!(out.next_type_id.0 > 0, "next_type_id should be positive");

        // A user must not be able to shadow these.
        for name in ["True", "False", "Some", "None", "Ok", "Err"] {
            assert!(
                out.reserved.contains(name),
                "'{name}' should be a reserved prelude name"
            );
        }

        assert!(!engine.nodes.is_empty(), "engine arena is empty");
        assert!(
            !engine.strings.is_empty(),
            "engine interned no type-side strings"
        );
    }

    /// `EnginePoolWatermark` is destructured exhaustively on purpose: adding
    /// a pool to the engine stops this compiling until someone decides whether
    /// the blob must carry it, rather than silently shipping dangling
    /// indices.
    #[test]
    fn blob_freezes_exactly_the_engine_pools() {
        use crate::static_ir::flatten::flatten;
        use crate::types::EnginePoolWatermark;

        let (out, engine) = precompile_stdlib().expect("stdlib must precompile");
        let flat = flatten(&out, &engine);

        let EnginePoolWatermark {
            nodes,
            children,
            strings,
            quants,
            str_slices,
            type_params,
            variant_fields,
            variants,
        } = engine.pool_watermark();

        assert_eq!(flat.nodes.len(), nodes, "node arena not frozen verbatim");
        assert_eq!(flat.children.len(), children);
        assert_eq!(flat.quants.len(), quants);
        assert_eq!(flat.str_slices.len(), str_slices);
        assert_eq!(flat.type_params.len(), type_params);
        assert_eq!(flat.variant_fields.len(), variant_fields);
        assert_eq!(flat.variants.len(), variants);
        // `str_pool` is the one pool that grows past the engine's: only its
        // prefix may be indexed by a frozen `StrId`.
        assert!(
            flat.str_pool.len() >= strings,
            "str_pool must extend engine.strings, never truncate it"
        );

        // The blob ships bytecode, not IR, which is why no resolved-type
        // pool needs serialising.
        assert!(!flat.code.is_empty(), "stdlib froze no bytecode");
        assert!(!flat.functions.is_empty(), "stdlib froze no functions");
    }

    /// The other half of that guard: the artifact must not carry a
    /// `ResolvedPool`. Adding one stops this exhaustive destructuring from
    /// compiling, rather than quietly freezing indices into an arena
    /// `seed_static` never restores.
    #[test]
    fn precompile_output_carries_no_resolved_pool() {
        let (out, _engine) = precompile_stdlib().expect("stdlib must precompile");

        let PrecompileOutput {
            blob,
            prelude: _,
            reserved: _,
            next_type_id: _,
        } = out;
        let PrecompiledBlob {
            interfaces,
            type_info,
            program,
            local_count,
        } = blob;

        assert!(!interfaces.is_empty());
        assert!(!type_info.is_empty());
        assert!(!program.code.is_empty());
        assert!(local_count > 0, "stdlib init occupies entry-frame slots");
    }

    /// Two runs over identical source must agree exactly, or `cargo build` is
    /// irreproducible — silently, since each blob is internally consistent.
    #[test]
    fn precompile_stdlib_is_deterministic() {
        use crate::static_ir::flatten::flatten;

        let (a, ea) = precompile_stdlib().expect("stdlib must precompile");
        let (b, eb) = precompile_stdlib().expect("stdlib must precompile");
        let (fa, fb) = (flatten(&a, &ea), flatten(&b, &eb));

        let instrs = |f: &crate::static_ir::flatten::FlatPools| {
            f.code
                .iter()
                .map(|i| (i.op, i.operand, i.a, i.b))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            instrs(&fa),
            instrs(&fb),
            "stdlib bytecode is nondeterministic"
        );

        let fns = |f: &crate::static_ir::flatten::FlatPools| {
            f.functions
                .iter()
                .map(|s| {
                    (
                        s.name,
                        s.arity,
                        s.locals,
                        s.capture_count,
                        s.code_start,
                        s.code_len,
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(fns(&fa), fns(&fb), "function table is nondeterministic");

        // Frozen `Ty`s index the node arena by position, so a reordering
        // here is a silently-wrong blob.
        fn dbg<T: std::fmt::Debug>(v: &[T]) -> Vec<String> {
            v.iter().map(|x| format!("{x:?}")).collect()
        }
        assert_eq!(
            dbg(&fa.nodes),
            dbg(&fb.nodes),
            "type node arena is nondeterministic"
        );
        assert_eq!(fa.children, fb.children);
        assert_eq!(dbg(&fa.variants), dbg(&fb.variants));
        assert_eq!(dbg(&fa.variant_fields), dbg(&fb.variant_fields));
        assert_eq!(dbg(&fa.schemes), dbg(&fb.schemes));
        assert_eq!(dbg(&fa.typeinfos), dbg(&fb.typeinfos));

        assert_eq!(dbg(&fa.constants), dbg(&fb.constants));
        assert_eq!(fa.str_pool, fb.str_pool, "string pool is nondeterministic");
        assert_eq!(fa.str_slice_pool, fb.str_slice_pool);
        assert_eq!(fa.byte_pool, fb.byte_pool);
        assert_eq!(fa.reserved, fb.reserved);
        assert_eq!(fa.typeinfo_by_name, fb.typeinfo_by_name);

        let keys = |f: &crate::static_ir::flatten::FlatPools| {
            f.modules.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>()
        };
        assert_eq!(keys(&fa), keys(&fb), "module order is nondeterministic");
        assert_eq!(a.next_type_id, b.next_type_id);
        assert_eq!(a.blob.local_count, b.blob.local_count);
    }
}
