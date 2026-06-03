//! Static-stdlib representation. The live `Scheme`/`TypeInfo`/`ValueKind`/
//! `TypeBody`/`Variant`/`VariantField`/`TypeParam`/`QuantVar` are all `Copy`
//! and const-constructible (every variable-length field is an `ArenaSlice`
//! into an `InferEngine` pool), so the precompiled stdlib emits them directly
//! as `&'static [_]` — there is no S-prefixed mirror of the type IR.
//!
//! What remains here is the runtime-value side (`SFunction`/`SConst`, whose
//! live counterparts carry `Rc`s), the per-module export tables, and the
//! `StaticStdlib` handle that bundles every pool slice for `seed_static`.

pub mod flatten;

use std::sync::Arc;

use crate::bytecode::{Function, Instruction, PreludeBindings, Value};
use crate::frozen::FrozenBuilder;
use crate::module::{ExportedValue, ModuleInterface};
use crate::types::{
    QuantVar, Scheme, StrId, Ty, TypeInfo, TypeNode, TypeParam, Variant, VariantField,
};

/// Static descriptor for one stdlib constructor (type id, type/variant name,
/// field labels), emitted by build.rs as `stdlib::<module>::<CTOR>` consts so a
/// rename in the AL source surfaces as a Rust compile error at the VM usage
/// site rather than silently constructing a mismatched value.
#[derive(Debug)]
pub struct VariantTemplate {
    pub type_id: i32,
    pub type_name: &'static str,
    pub variant_name: &'static str,
    pub labels: &'static [&'static str],
}

/// Half-open `[start, start+len)` index into a sibling pool. Distinct from
/// `ArenaSlice` only in width (`u32` len vs `u16`) since string-slice pools
/// can exceed 65 535 entries while type-argument lists cannot.
#[derive(Debug, Clone, Copy)]
pub struct Slice {
    pub start: u32,
    pub len: u32,
}

impl Slice {
    pub const EMPTY: Slice = Slice { start: 0, len: 0 };
    #[inline]
    pub fn range(self) -> std::ops::Range<usize> {
        self.start as usize..(self.start + self.len) as usize
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SExport {
    pub name: u32,
    pub scheme: u32,
    pub local_slot: i32,
}

#[derive(Debug, Clone, Copy)]
pub struct STypeExport {
    pub name: u32,
    pub info: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct SModule {
    pub types: Slice,
    pub values: Slice,
    pub private_names: Slice,
    pub path: Slice,
}

#[derive(Debug, Clone, Copy)]
pub struct SFunction {
    pub name: u32,
    pub arity: i32,
    pub locals: i32,
    pub capture_count: i32,
    pub code_start: i32,
    pub code_len: i32,
}

#[derive(Debug, Clone, Copy)]
pub enum SConst {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(u32),
    StrArray(Slice),
    /// A binary literal: `bit_len` logical bits whose `bit_len.div_ceil(8)`
    /// bytes live in `byte_pool` at the given slice (bit offset 0).
    Binary(Slice, u64),
}

/// Single handle bundling every static pool.
#[derive(Debug, Clone, Copy)]
pub struct StaticStdlib {
    pub str_pool: &'static [&'static str],
    pub str_slice_pool: &'static [u32],
    pub byte_pool: &'static [u8],

    /// The type arena and side-pools — the same `Copy` types the live engine
    /// uses, so `seed_static` is a memcpy and consumers read them directly.
    pub nodes: &'static [TypeNode],
    pub children: &'static [Ty],
    pub quants: &'static [QuantVar],
    pub str_slices: &'static [StrId],
    pub type_params: &'static [TypeParam],
    pub variant_fields: &'static [VariantField],
    pub variants: &'static [Variant],

    pub schemes: &'static [Scheme],
    pub typeinfos: &'static [TypeInfo],

    pub stypeexport_pool: &'static [STypeExport],
    pub sexport_pool: &'static [SExport],
    pub modules: &'static [(&'static str, SModule)],
    pub typeinfo_by_name: &'static [(&'static str, u32)],

    pub code: &'static [Instruction],
    pub functions: &'static [SFunction],
    pub constants: &'static [SConst],

    pub prelude: &'static PreludeBindings,
    pub reserved: &'static [&'static str],
    pub next_type_id: i32,
    pub local_count: i32,
}

impl StaticStdlib {
    #[inline]
    fn s(&self, i: u32) -> String {
        self.str_pool[i as usize].to_string()
    }
    #[inline]
    fn modpath(&self, sl: Slice) -> Vec<String> {
        self.str_slice_pool[sl.range()]
            .iter()
            .map(|&i| self.s(i))
            .collect()
    }

    pub fn hydrate_module(&self, m: &SModule) -> ModuleInterface {
        let mut iface = ModuleInterface::new(self.modpath(m.path));
        for te in &self.stypeexport_pool[m.types.range()] {
            iface
                .types
                .insert(self.s(te.name), self.typeinfos[te.info as usize]);
        }
        for e in &self.sexport_pool[m.values.range()] {
            iface.values.insert(
                self.s(e.name),
                ExportedValue {
                    scheme: self.schemes[e.scheme as usize],
                    local_slot: (e.local_slot != i32::MIN).then_some(e.local_slot),
                },
            );
        }
        for &i in &self.str_slice_pool[m.private_names.range()] {
            iface.private_names.insert(self.s(i));
        }
        iface
    }

    pub fn lookup_module(&self, key: &str) -> Option<ModuleInterface> {
        self.modules
            .binary_search_by_key(&key, |(k, _)| k)
            .ok()
            .map(|i| self.hydrate_module(&self.modules[i].1))
    }

    /// Hydrate the precompiled stdlib's code/functions/constants into live
    /// form. Program literals/constants are built through the explicit
    /// `frozen` builder: string constants and label
    /// lists intern to the frozen area's canonical allocations, shared with
    /// everything else the same program compiles.
    pub fn hydrate_program(
        &self,
        frozen: &mut FrozenBuilder,
    ) -> (Vec<Instruction>, Vec<Function>, Vec<Value>) {
        let code = self.code.to_vec();
        let functions: Vec<Function> = self
            .functions
            .iter()
            .map(|f| Function {
                name: Arc::from(self.str_pool[f.name as usize]),
                arity: f.arity,
                locals: f.locals,
                capture_count: f.capture_count,
                code_start: f.code_start,
                code_len: f.code_len,
            })
            .collect();
        let constants: Vec<Value> = self
            .constants
            .iter()
            .map(|c| match *c {
                SConst::Int(n) => frozen.int(n),
                SConst::Float(f) => frozen.float(f),
                SConst::Bool(b) => frozen.bool(b),
                SConst::Str(i) => frozen.str(self.str_pool[i as usize]),
                SConst::StrArray(sl) => {
                    let strs: Vec<&str> = self.str_slice_pool[sl.range()]
                        .iter()
                        .map(|&i| self.str_pool[i as usize])
                        .collect();
                    frozen.str_array(&strs)
                }
                SConst::Binary(sl, bit_len) => {
                    frozen.binary_bits(self.byte_pool[sl.range()].to_vec(), bit_len)
                }
            })
            .collect();
        (code, functions, constants)
    }
}

pub use crate::bytecode::{Instruction as SInstruction, Op as SOp};
