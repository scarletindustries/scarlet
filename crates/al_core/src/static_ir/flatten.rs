//! Flatten a `PrecompileOutput` into the pool-of-arrays form that build.rs
//! emits as Rust source. With every type-side struct now `Copy` and backed by
//! `InferEngine` side-pools, this is a wholesale snapshot of those pools plus
//! per-module export tables and the program; nothing is restructured.
//!
//! `panic!` is the correct failure mode here: this code only executes inside
//! `build.rs`, where a panic surfaces as a `cargo build` error with the message
//! attached. The crate-wide `clippy::panic` deny is for the runtime path.
#![allow(clippy::panic)]

use indexmap::IndexMap;

use super::*;
use crate::bytecode::{PreludeBindings, Value, ValueView};
use crate::module::ModuleInterface;
use crate::precompile::PrecompileOutput;
use crate::type_def::TypeId;
use crate::types::InferEngine;

/// Owned counterpart of `StaticStdlib` — `Vec`s (and owned values) instead of
/// `&'static` borrows, field for field. Only [`flatten`] constructs one, fully.
#[derive(Debug)]
pub struct FlatPools {
    pub str_pool: Vec<String>,
    pub str_slice_pool: Vec<StrIdx>,
    pub byte_pool: Vec<u8>,

    pub nodes: Vec<TypeNode>,
    pub children: Vec<Ty>,
    pub quants: Vec<QuantVar>,
    pub str_slices: Vec<StrId>,
    pub type_params: Vec<TypeParam>,
    pub variant_fields: Vec<VariantField>,
    pub variants: Vec<Variant>,

    pub schemes: Vec<Scheme>,
    pub typeinfos: Vec<TypeInfo>,

    pub stypeexport_pool: Vec<STypeExport>,
    pub sexport_pool: Vec<SExport>,
    pub modules: Vec<(String, SModule)>,
    pub typeinfo_by_name: Vec<(String, TypeInfoIdx)>,

    pub code: Vec<Instruction>,
    pub functions: Vec<SFunction>,
    pub constants: Vec<SConst>,

    pub prelude: PreludeBindings,
    pub reserved: Vec<String>,
    pub next_type_id: TypeId,
    pub local_count: i32,

    str_intern: IndexMap<String, StrIdx>,
}

/// Slice whose `len` is the pool-length delta since `start` — structurally in
/// agreement with what the push loop actually appended, whatever that loop
/// skipped or filtered.
fn slice_since<T>(pool_len_after: usize, start: u32) -> Slice<T> {
    Slice::new(start, pool_len_after as u32 - start)
}

impl FlatPools {
    fn intern(&mut self, s: &str) -> StrIdx {
        if let Some(&i) = self.str_intern.get(s) {
            return i;
        }
        let i = StrIdx(self.str_pool.len() as u32);
        self.str_pool.push(s.to_string());
        self.str_intern.insert(s.to_string(), i);
        i
    }

    fn push_str_slice<I: IntoIterator<Item = String>>(&mut self, it: I) -> Slice<StrIdx> {
        let start = self.str_slice_pool.len() as u32;
        for s in it {
            let i = self.intern(&s);
            self.str_slice_pool.push(i);
        }
        slice_since(self.str_slice_pool.len(), start)
    }

    fn push_bytes(&mut self, bytes: &[u8]) -> Slice<u8> {
        let start = self.byte_pool.len() as u32;
        self.byte_pool.extend_from_slice(bytes);
        slice_since(self.byte_pool.len(), start)
    }

    fn push_scheme(&mut self, s: Scheme) -> SchemeIdx {
        // `s.def` (the declaring `DefinitionLocation`) is preserved: its
        // `module` `ArenaSlice` indexes the static string-slice pool, which
        // `seed_static` memcpies back as the live arena's prefix, so the
        // location stays valid after hydration. This is what lets cross-module
        // goto-def / find-references land inside a precompiled `al/*` module
        // (`synth_refs_from_interface` reads it back).
        let i = SchemeIdx(self.schemes.len() as u32);
        self.schemes.push(s);
        i
    }

    fn push_typeinfo(&mut self, ti: TypeInfo) -> TypeInfoIdx {
        let i = TypeInfoIdx(self.typeinfos.len() as u32);
        self.typeinfos.push(ti);
        i
    }

    fn flatten_module(&mut self, key: &str, iface: &ModuleInterface) {
        let path = self.push_str_slice(iface.path.iter().cloned());
        let t_start = self.stypeexport_pool.len() as u32;
        for (n, ti) in &iface.types {
            let info = self.push_typeinfo(*ti);
            let name = self.intern(n);
            self.stypeexport_pool.push(STypeExport { name, info });
        }
        let types = slice_since(self.stypeexport_pool.len(), t_start);
        let v_start = self.sexport_pool.len() as u32;
        for (n, ev) in &iface.values {
            let scheme = self.push_scheme(ev.scheme);
            let name = self.intern(n);
            let param_names = self.push_str_slice(ev.param_names.iter().cloned());
            let doc = ev.doc.as_deref().map(|d| self.intern(d));
            self.sexport_pool.push(SExport {
                name,
                scheme,
                local_slot: ev.local_slot,
                param_names,
                doc,
            });
        }
        let values = slice_since(self.sexport_pool.len(), v_start);
        // `private_names` is a `BTreeSet`: iteration is sorted by type, so
        // interning in that order is reproducible build to build.
        let private_names = self.push_str_slice(iface.private_names.iter().cloned());
        let doc = iface.doc.as_deref().map(|d| self.intern(d));
        self.modules.push((
            key.to_string(),
            SModule {
                types,
                values,
                private_names,
                path,
                doc,
            },
        ));
    }

    fn flatten_const(&mut self, v: &Value) -> SConst {
        match v.kind() {
            ValueView::Int(n) => SConst::Int(n),
            ValueView::Float(f) => SConst::Float(f),
            ValueView::Bool(b) => SConst::Bool(b),
            ValueView::Str(s) => SConst::Str(self.intern(s)),
            ValueView::Array(a) => {
                let strs: Vec<String> = a
                    .iter()
                    .map(|v| match v.as_str() {
                        Some(s) => s.to_string(),
                        None => panic!(
                            "flatten: stdlib constant array contains a non-string element \
                             ({v:?}); extend SConst if this is intentional"
                        ),
                    })
                    .collect();
                SConst::StrArray(self.push_str_slice(strs))
            }
            ValueView::Binary(b) => {
                let sl = self.push_bytes(&b.to_aligned_vec());
                SConst::Binary(sl, b.bit_len())
            }
            _ => panic!(
                "flatten: stdlib constant pool contains a non-scalar value ({v:?}); \
                 extend SConst if this is intentional"
            ),
        }
    }
}

pub fn flatten(out: &PrecompileOutput, engine: &InferEngine) -> FlatPools {
    // The engine's pools are the basis of ours; seed them first so every
    // `StrId`/`ArenaSlice` embedded in the snapshot stays valid. Additional
    // strings (function/module names) intern after.
    // str_pool prefix must equal engine.strings verbatim so embedded StrId
    // indices stay valid; direct copy makes that structural instead of relying
    // on engine.strings being dupe-free.
    let mut p = FlatPools {
        str_pool: engine.strings.iter().cloned().collect(),
        str_slice_pool: Vec::new(),
        byte_pool: Vec::new(),
        nodes: engine.nodes.clone(),
        children: engine.children.clone(),
        quants: engine.quants.clone(),
        str_slices: engine.str_slices.clone(),
        type_params: engine.type_params.clone(),
        variant_fields: engine.variant_fields.clone(),
        variants: engine.variants.clone(),
        schemes: Vec::new(),
        typeinfos: Vec::new(),
        stypeexport_pool: Vec::new(),
        sexport_pool: Vec::new(),
        modules: Vec::new(),
        typeinfo_by_name: Vec::new(),
        code: Vec::new(),
        functions: Vec::new(),
        constants: Vec::new(),
        prelude: out.prelude.clone(),
        // `reserved` is a `BTreeSet`, so this comes out sorted (build.rs
        // relies on binary_search over the emitted slice).
        reserved: out.reserved.iter().cloned().collect(),
        next_type_id: out.next_type_id,
        local_count: out.blob.local_count,
        str_intern: IndexMap::new(),
    };
    for (i, s) in engine.strings.iter().enumerate() {
        p.str_intern.entry(s.clone()).or_insert(StrIdx(i as u32));
    }

    let mut keys: Vec<&String> = out.blob.interfaces.keys().collect();
    keys.sort();
    for k in keys {
        p.flatten_module(k, &out.blob.interfaces[k]);
    }

    let mut ti_names: Vec<&String> = out.blob.type_info.keys().collect();
    ti_names.sort();
    for n in ti_names {
        let idx = p.push_typeinfo(out.blob.type_info[n]);
        p.typeinfo_by_name.push((n.clone(), idx));
    }

    p.code = out.blob.program.code.clone();
    for f in &out.blob.program.functions {
        let name = p.intern(&f.name);
        p.functions.push(SFunction {
            name,
            arity: f.arity,
            locals: f.locals,
            capture_count: f.capture_count,
            code_start: f.code_start,
            code_len: f.code_len,
        });
    }
    let consts = out.blob.program.constants.clone();
    for v in &consts {
        let c = p.flatten_const(v);
        p.constants.push(c);
    }

    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::precompile::precompile_stdlib;

    // `flatten` lowers a `PrecompileOutput` to the pool-of-arrays form build.rs
    // serialises; like `precompile_stdlib` it only ever runs at build time, so
    // this is the sole non-build exercise of it. Assert it is *lossless*:
    // every program field reconstructs exactly, and the type-arena pools are
    // copied wholesale from the engine so embedded `Ty`/`StrId` stay valid.
    #[test]
    fn flatten_is_lossless_over_the_precompiled_stdlib() {
        let (out, engine) = precompile_stdlib().expect("stdlib must precompile");
        let p = flatten(&out, &engine);
        let prog = &out.blob.program;

        // The type arena and side-pools are snapshotted verbatim — any `Ty`
        // index in a flattened `Scheme`/`TypeInfo` still points at the same
        // node after hydration only if these are byte-for-byte copies.
        assert_eq!(p.nodes.len(), engine.nodes.len(), "nodes not copied 1:1");
        assert_eq!(p.children.len(), engine.children.len());
        assert_eq!(p.variants.len(), engine.variants.len());
        assert_eq!(p.variant_fields.len(), engine.variant_fields.len());
        // The string pool is seeded with the engine's strings as its prefix so
        // every pre-existing `StrId` keeps its meaning; later names append.
        assert!(p.str_pool.len() >= engine.strings.len());
        assert!(
            p.str_pool
                .iter()
                .take(engine.strings.len())
                .eq(engine.strings.iter())
        );

        // Functions round-trip: count, interned name, and every scalar field.
        assert_eq!(p.functions.len(), prog.functions.len());
        for (sf, f) in p.functions.iter().zip(&prog.functions) {
            assert_eq!(
                p.str_pool[sf.name.0 as usize].as_str(),
                &*f.name,
                "function name lost in interning"
            );
            assert_eq!(sf.arity, f.arity);
            assert_eq!(sf.locals, f.locals);
            assert_eq!(sf.capture_count, f.capture_count);
            assert_eq!(sf.code_start, f.code_start);
            assert_eq!(sf.code_len, f.code_len);
        }

        // Code round-trips instruction-for-instruction (Instruction isn't
        // PartialEq, so compare each decoded field).
        assert_eq!(p.code.len(), prog.code.len());
        for (a, b) in p.code.iter().zip(&prog.code) {
            assert_eq!(a.op, b.op);
            assert_eq!(a.a, b.a);
            assert_eq!(a.b, b.b);
            assert_eq!(a.operand, b.operand);
        }

        // Constants round-trip: each flattened `SConst` decodes back to the
        // original scalar / string / string-array value.
        assert_eq!(p.constants.len(), prog.constants.len());
        for (sc, v) in p.constants.iter().zip(&prog.constants) {
            match (sc, v.kind()) {
                (SConst::Int(n), ValueView::Int(m)) => assert_eq!(*n, m),
                (SConst::Float(f), ValueView::Float(g)) => assert_eq!(*f, g),
                (SConst::Bool(b), ValueView::Bool(c)) => assert_eq!(*b, c),
                (SConst::Str(i), ValueView::Str(s)) => {
                    assert_eq!(p.str_pool[i.0 as usize].as_str(), s);
                }
                (SConst::StrArray(sl), ValueView::Array(arr)) => {
                    let strs: Vec<&str> = p.str_slice_pool[sl.range()]
                        .iter()
                        .map(|&i| p.str_pool[i.0 as usize].as_str())
                        .collect();
                    let want: Vec<String> = arr
                        .iter()
                        .map(|e| e.as_str().expect("label is string").to_string())
                        .collect();
                    assert_eq!(strs, want);
                }
                (SConst::Binary(sl, bit_len), ValueView::Binary(b)) => {
                    assert_eq!(*bit_len, b.bit_len());
                    assert_eq!(&p.byte_pool[sl.range()], b.to_aligned_vec().as_slice());
                }
                (other, _) => panic!("constant kind mismatch for {other:?}"),
            }
        }

        // Every interface became a flattened module, keyed identically and
        // emitted in sorted order (build.rs relies on binary_search on it).
        let keys: Vec<&String> = p.modules.iter().map(|(k, _)| k).collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted, "modules must be emitted sorted");
        for k in out.blob.interfaces.keys() {
            assert!(
                p.modules.iter().any(|(mk, _)| mk == k),
                "module '{k}' missing from flattened pools"
            );
        }

        // The module doc is what hover shows for `al/scheduler`, which is
        // never recompiled; declaration docs travel on `SExport::doc`.
        for (k, iface) in &out.blob.interfaces {
            let (_, m) = p.modules.iter().find(|(mk, _)| mk == k).unwrap();
            let flat = m.doc.map(|i| p.str_pool[i.0 as usize].as_str());
            assert_eq!(flat, iface.doc.as_deref(), "module doc lost for '{k}'");
        }
        let (_, sched) = p
            .modules
            .iter()
            .find(|(k, _)| k == "al/scheduler")
            .expect("al/scheduler is precompiled");
        let doc = p.str_pool[sched.doc.expect("al/scheduler has a module doc").0 as usize].as_str();
        assert!(doc.contains("Lightweight processes."));

        // The standalone type-info table covers every entry by name.
        assert_eq!(p.typeinfo_by_name.len(), out.blob.type_info.len());

        // `reserved` (also build.rs-only) is the reserved names in sorted
        // order, losslessly.
        let mut sorted = p.reserved.clone();
        sorted.sort();
        assert_eq!(p.reserved, sorted, "reserved names must come out sorted");
        assert_eq!(p.reserved.len(), out.reserved.len());
        assert!(p.reserved.iter().any(|r| r == "Some"));

        // The prelude bindings, next type id, and stdlib local count travel
        // with the pools — `FlatPools` mirrors every `StaticStdlib` field.
        assert_eq!(p.next_type_id, out.next_type_id);
        assert_eq!(p.local_count, out.blob.local_count);
        for ((n, a), (m, b)) in p.prelude.type_fields().zip(out.prelude.type_fields()) {
            assert_eq!(n, m);
            assert_eq!(a.id, b.id, "prelude type '{n}' drifted in flatten");
        }
        for ((n, a), (m, b)) in p.prelude.ctor_fields().zip(out.prelude.ctor_fields()) {
            assert_eq!(n, m);
            assert_eq!(
                (a.type_id, a.variant_idx, a.arity),
                (b.type_id, b.variant_idx, b.arity),
                "prelude ctor '{n}' drifted in flatten"
            );
        }
    }
}
