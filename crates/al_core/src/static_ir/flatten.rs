//! Flatten a `PrecompileOutput` into the pool-of-arrays form that build.rs
//! emits as Rust source. With every type-side struct now `Copy` and backed by
//! `InferEngine` side-pools, this is a wholesale snapshot of those pools plus
//! per-module export tables and the program; nothing is restructured.
//!
//! `panic!` is the correct failure mode here: this code only executes inside
//! `build.rs`, where a panic surfaces as a `cargo build` error with the message
//! attached. The crate-wide `clippy::panic` deny is for the runtime path.
#![allow(clippy::panic)]

use std::collections::HashSet;

use indexmap::IndexMap;

use super::*;
use crate::bytecode::{Value, ValueView};
use crate::module::ModuleInterface;
use crate::precompile::PrecompileOutput;
use crate::types::InferEngine;

/// Owned counterpart of `StaticStdlib` — `Vec`s instead of `&'static [_]`.
#[derive(Debug, Default)]
pub struct FlatPools {
    pub str_pool: Vec<String>,
    pub str_slice_pool: Vec<u32>,
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
    pub typeinfo_by_name: Vec<(String, u32)>,

    pub code: Vec<Instruction>,
    pub functions: Vec<SFunction>,
    pub constants: Vec<SConst>,

    str_intern: IndexMap<String, u32>,
}

impl FlatPools {
    fn intern(&mut self, s: &str) -> u32 {
        if let Some(&i) = self.str_intern.get(s) {
            return i;
        }
        let i = self.str_pool.len() as u32;
        self.str_pool.push(s.to_string());
        self.str_intern.insert(s.to_string(), i);
        i
    }

    fn push_str_slice<I: IntoIterator<Item = String>>(&mut self, it: I) -> Slice {
        let start = self.str_slice_pool.len() as u32;
        for s in it {
            let i = self.intern(&s);
            self.str_slice_pool.push(i);
        }
        Slice {
            start,
            len: self.str_slice_pool.len() as u32 - start,
        }
    }

    fn push_bytes(&mut self, bytes: &[u8]) -> Slice {
        let start = self.byte_pool.len() as u32;
        self.byte_pool.extend_from_slice(bytes);
        Slice {
            start,
            len: self.byte_pool.len() as u32 - start,
        }
    }

    fn push_scheme(&mut self, s: Scheme) -> u32 {
        // `s.def` (the declaring `DefinitionLocation`) is preserved: its
        // `module` `ArenaSlice` indexes the static string-slice pool, which
        // `seed_static` memcpies back as the live arena's prefix, so the
        // location stays valid after hydration. This is what lets cross-module
        // goto-def / find-references land inside a precompiled `al/*` module
        // (`synth_refs_from_interface` reads it back).
        let i = self.schemes.len() as u32;
        self.schemes.push(s);
        i
    }

    fn push_typeinfo(&mut self, ti: TypeInfo) -> u32 {
        let i = self.typeinfos.len() as u32;
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
        let types = Slice {
            start: t_start,
            len: iface.types.len() as u32,
        };
        let v_start = self.sexport_pool.len() as u32;
        for (n, ev) in &iface.values {
            let scheme = self.push_scheme(ev.scheme);
            let name = self.intern(n);
            self.sexport_pool.push(SExport {
                name,
                scheme,
                local_slot: ev.local_slot.unwrap_or(i32::MIN),
            });
        }
        let values = Slice {
            start: v_start,
            len: iface.values.len() as u32,
        };
        let private_names = self.push_str_slice(iface.private_names.iter().cloned());
        self.modules.push((
            key.to_string(),
            SModule {
                types,
                values,
                private_names,
                path,
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
    let mut p = FlatPools::default();

    // The engine's pools are the basis of ours; seed them first so every
    // `StrId`/`ArenaSlice` embedded in the snapshot stays valid. Additional
    // strings (function/module names) intern after.
    for s in &engine.strings {
        p.intern(s);
    }
    p.nodes = engine.nodes.clone();
    p.children = engine.children.clone();
    p.quants = engine.quants.clone();
    p.str_slices = engine.str_slices.clone();
    p.type_params = engine.type_params.clone();
    p.variant_fields = engine.variant_fields.clone();
    p.variants = engine.variants.clone();

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

pub fn reserved_sorted(reserved: &HashSet<String>) -> Vec<String> {
    let mut v: Vec<_> = reserved.iter().cloned().collect();
    v.sort();
    v
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
        assert_eq!(&p.str_pool[..engine.strings.len()], &engine.strings[..]);

        // Functions round-trip: count, interned name, and every scalar field.
        assert_eq!(p.functions.len(), prog.functions.len());
        for (sf, f) in p.functions.iter().zip(&prog.functions) {
            assert_eq!(
                p.str_pool[sf.name as usize].as_str(),
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
                    assert_eq!(p.str_pool[*i as usize].as_str(), s);
                }
                (SConst::StrArray(sl), ValueView::Array(arr)) => {
                    let strs: Vec<&str> = p.str_slice_pool[sl.range()]
                        .iter()
                        .map(|&i| p.str_pool[i as usize].as_str())
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

        // The standalone type-info table covers every entry by name.
        assert_eq!(p.typeinfo_by_name.len(), out.blob.type_info.len());

        // `reserved_sorted` (also build.rs-only) yields the reserved names in
        // sorted order, losslessly.
        let reserved = reserved_sorted(&out.reserved);
        let mut sorted = reserved.clone();
        sorted.sort();
        assert_eq!(reserved, sorted, "reserved names must come out sorted");
        assert_eq!(reserved.len(), out.reserved.len());
        assert!(reserved.iter().any(|r| r == "Some"));
    }
}
