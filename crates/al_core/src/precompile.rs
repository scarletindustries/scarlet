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
//!
//! # What the blob may freeze
//!
//! Every index the blob carries is frozen into `.rodata` and can never be
//! re-minted, so it may only point into an arena the blob *also* carries.
//! Exactly one arena family qualifies: the `InferEngine` pools, enumerated by
//! [`crate::types::EnginePoolWatermark`] and copied wholesale by
//! `static_ir::flatten`. A stdlib `Scheme`/`TypeInfo` holds `Ty`s and
//! `ArenaSlice`s into them; `seed_static` memcpys them back as the live
//! engine's prefix, which is why those indices stay valid.
//!
//! A *compile-local* arena — one minted and dropped inside a single
//! `compile()` — must never appear here. The post-inference resolved-type pool
//! (`RTy`/`ResolvedPool`) is compile-local, and it stays out of the blob
//! because **the stdlib is frozen after `emit`, not before `lower`**:
//! `PrecompiledBlob::program` is finished bytecode, and `seed_static` installs
//! it directly. No stdlib body is ever re-lowered at runtime, so no stdlib
//! `RTy` ever needs to exist. `build.rs`'s dependency set is unaffected.
//! Pinned by `blob_freezes_exactly_the_engine_pools` below and by
//! `crates/al/tests/arena_rewind.rs::stdlib_bodies_are_never_relowered`.
//!
//! # Why the return type is `(PrecompileOutput, InferEngine)` and nothing else
//!
//! That pair *is* the decision. The elaborator mints a fresh `ResolvedPool`
//! per compile ([`crate::typed_ir::zonk::pool_for`], i.e.
//! `ResolvedPool::new(eng.prim_ids())`) and drops it once elaboration and emit
//! for that compile have finished; it is never a field of
//! [`crate::bytecode::compiler::Compiler`] and never reachable from
//! `PrecompileOutput`, so no `RTy` can be spelled in the generated `.rodata`.
//! (`bytecode::session` states the same invariant from the rewind side: there
//! is no pool on the `Compiler` to truncate a `ConstId`'s types against.)
//! `precompile_output_carries_no_resolved_pool` destructures both halves of
//! the artifact exhaustively — adding a field to carry the pool stops that
//! test compiling, which is the point.
//!
//! # Determinism is a correctness property, not a nicety
//!
//! Every index in the blob (a `Ty`, an `ArenaSlice`, a `code_start`, a
//! `func_idx`) is minted by one `precompile_stdlib()` run and then frozen. The
//! elaborator resolves types through hash-keyed memo tables; if any of them
//! leaked iteration order into arena allocation order, two builds of the same
//! source would produce two different — individually consistent, mutually
//! incompatible — blobs, and `cargo build` would be irreproducible.
//! `precompile_stdlib_is_deterministic` runs the whole pipeline twice and
//! compares the flattened artifacts.
//!
//! Jump operands are whatever `emit` produced; the blob copies `code` verbatim
//! and `seed_static` installs it as the program's prefix, so function-relative
//! operands survive the freeze without a relocation pass — nothing in this
//! module rewrites an operand.

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
    /// `BTreeSet` so iteration is sorted: `flatten` copies it verbatim into
    /// the reproducible stdlib blob, which `is_reserved` binary-searches.
    pub reserved: BTreeSet<String>,
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
    for path in stdlib::all_modules()? {
        c.load_module(&crate::ast::ImportPath::canonical(path.clone()), at);
        bail_on_errors(&c, ModuleKey::for_stdlib(&path).as_str())?;
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
        // External bodies have nothing to close; an Unresolved body reaching
        // the freeze means a stdlib type declaration never got its body filled
        // in — freezing it would bake the placeholder into `.rodata`.
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

    /// The frozen-blob seam. `flatten` snapshots the engine's arenas so that
    /// every `Ty`/`ArenaSlice` baked into `.rodata` still resolves once
    /// `seed_static` memcpys them back. `EnginePoolWatermark` is the canonical
    /// enumeration of those arenas — the same list `Compiler::reset_to` rewinds
    /// — so it is destructured exhaustively here: adding a pool to the engine
    /// fails to compile this test until someone decides whether the blob must
    /// carry it. That is the guard for the spec's assumption that the
    /// post-inference resolved-type pool is compile-local; were it ever hung
    /// off the engine, this line would force the question rather than silently
    /// shipping dangling `RTy`s.
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
        // `str_pool` is the one pool that *grows* past the engine's: function
        // and module names intern after the verbatim prefix. Only the prefix
        // may be indexed by a frozen `StrId`.
        assert!(
            flat.str_pool.len() >= strings,
            "str_pool must extend engine.strings, never truncate it"
        );

        // The blob is frozen *after* emit: it ships bytecode, not IR. This is
        // precisely why no resolved-type pool needs serialising — `lower` never
        // runs over a stdlib body at runtime.
        assert!(!flat.code.is_empty(), "stdlib froze no bytecode");
        assert!(!flat.functions.is_empty(), "stdlib froze no functions");
    }

    /// The other half of the same guard, on the other side of `flatten`.
    /// `PrecompileOutput` is everything build.rs is handed; it is destructured
    /// exhaustively here, as is the `PrecompiledBlob` inside it. A future
    /// `pool: ResolvedPool` field — the natural way to start serialising
    /// resolved types — stops this test compiling, forcing the author to
    /// confront the spec's third assumption rather than quietly freezing
    /// indices into an arena `seed_static` never restores.
    ///
    /// The decision this pins: **the artifact must NOT carry a
    /// `ResolvedPool`.** It is compile-local — the elaborator mints one per
    /// compile (`zonk::pool_for`) and drops it after emit; it is never a field
    /// of `Compiler` — and no stdlib body is re-lowered at runtime
    /// (`arena_rewind.rs`), so no stdlib `RTy` outlives the build.
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

    /// Every index the blob carries — `Ty`, `ArenaSlice`, `code_start`,
    /// `func_idx`, constant-pool slot — is minted by one `precompile_stdlib()`
    /// run and then frozen into `.rodata`. Two runs over identical source must
    /// therefore agree exactly: a hash-keyed memo in the elaborator that leaked
    /// iteration order into arena allocation order would make `cargo build`
    /// irreproducible, and would do it *silently* (each blob is internally
    /// consistent; only a rebuild disagrees).
    #[test]
    fn precompile_stdlib_is_deterministic() {
        use crate::static_ir::flatten::flatten;

        let (a, ea) = precompile_stdlib().expect("stdlib must precompile");
        let (b, eb) = precompile_stdlib().expect("stdlib must precompile");
        let (fa, fb) = (flatten(&a, &ea), flatten(&b, &eb));

        // Bytecode: opcode and all three operand fields, instruction for
        // instruction. This is what a relative-operand emitter must keep stable.
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

        // The type arena. `Ty`s frozen into stdlib `Scheme`s index it by
        // position, so a reordering here is a silently-wrong blob.
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
