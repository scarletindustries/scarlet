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

use std::marker::PhantomData;
use std::sync::Arc;

use crate::bytecode::{Function, Instruction, PreludeBindings, Value};
use crate::frozen::FrozenBuilder;
use crate::module::{ExportedValue, ModuleInterface};
use crate::type_def::TypeId;
// Re-exported for the generated stdlib file: build.rs emits `SExport`
// literals whose `local_slot` field Debug-prints as `Some(GlobalSlot(n))`,
// and the generated code glob-imports this module.
pub use crate::typed_ir::GlobalSlot;
use crate::types::{
    QuantVar, Scheme, StrId, Ty, TypeInfo, TypeNode, TypeParam, Variant, VariantField,
};

// `VariantTemplate` — the static descriptor for one stdlib constructor,
// emitted by build.rs as `stdlib::<module>::<CTOR>` consts — lives in `al_vm`
// (the VM instantiates them) and is re-exported here where flattening
// produces them.
pub use al_vm::VariantTemplate;

/// Index into [`StaticStdlib::str_pool`]. Distinct from the engine-side
/// `StrId`: `str_pool` extends `engine.strings` past its verbatim prefix with
/// names interned during flattening, so a `StrIdx` is not always a valid
/// `StrId`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StrIdx(pub u32);

/// Index into [`StaticStdlib::schemes`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SchemeIdx(pub u32);

/// Index into [`StaticStdlib::typeinfos`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TypeInfoIdx(pub u32);

/// Half-open `[start, start+len)` index into the sibling pool whose element
/// type is `T`, so a slice can only index the pool it was minted against.
/// Distinct from `ArenaSlice` only in width (`u32` len vs `u16`) since
/// string-slice pools can exceed 65 535 entries while type-argument lists
/// cannot.
#[derive(Debug)]
pub struct Slice<T> {
    pub start: u32,
    pub len: u32,
    _marker: PhantomData<T>,
}

// Manual impls: a derive would bound `T: Clone`/`T: Copy`, which the
// `PhantomData` doesn't require.
impl<T> Clone for Slice<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for Slice<T> {}

impl<T> Slice<T> {
    #[inline]
    pub const fn new(start: u32, len: u32) -> Self {
        Slice {
            start,
            len,
            _marker: PhantomData,
        }
    }

    #[inline]
    pub fn range(self) -> std::ops::Range<usize> {
        self.start as usize..self.start as usize + self.len as usize
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SExport {
    pub name: StrIdx,
    pub scheme: SchemeIdx,
    pub local_slot: Option<GlobalSlot>,
    /// `str_slice_pool` range of `str_pool` indices: the function's parameter
    /// names, in order.
    pub param_names: Slice<StrIdx>,
    /// `str_pool` index of the declaration's doc comment, if it has one.
    pub doc: Option<StrIdx>,
}

#[derive(Debug, Clone, Copy)]
pub struct STypeExport {
    pub name: StrIdx,
    pub info: TypeInfoIdx,
}

#[derive(Debug, Clone, Copy)]
pub struct SModule {
    pub types: Slice<STypeExport>,
    pub values: Slice<SExport>,
    pub private_names: Slice<StrIdx>,
    pub path: Slice<StrIdx>,
    /// `str_pool` index of the module-level doc comment (the `/** */` block
    /// at the top of the file), if there is one. Declaration docs travel on
    /// [`SExport::doc`].
    pub doc: Option<StrIdx>,
}

#[derive(Debug, Clone, Copy)]
pub struct SFunction {
    pub name: StrIdx,
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
    Str(StrIdx),
    StrArray(Slice<StrIdx>),
    /// A binary literal: `bit_len` logical bits whose `bit_len.div_ceil(8)`
    /// bytes live in `byte_pool` at the given slice (bit offset 0).
    Binary(Slice<u8>, u64),
}

/// Single handle bundling every static pool.
#[derive(Debug, Clone, Copy)]
pub struct StaticStdlib {
    pub str_pool: &'static [&'static str],
    pub str_slice_pool: &'static [StrIdx],
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
    pub typeinfo_by_name: &'static [(&'static str, TypeInfoIdx)],

    pub code: &'static [Instruction],
    pub functions: &'static [SFunction],
    pub constants: &'static [SConst],

    pub prelude: &'static PreludeBindings,
    pub reserved: &'static [&'static str],
    pub next_type_id: TypeId,
    pub local_count: i32,
}

impl StaticStdlib {
    #[inline]
    fn s(&self, i: StrIdx) -> String {
        self.str_pool[i.0 as usize].to_string()
    }
    #[inline]
    fn modpath(&self, sl: Slice<StrIdx>) -> Vec<String> {
        self.str_slice_pool[sl.range()]
            .iter()
            .map(|&i| self.s(i))
            .collect()
    }

    fn hydrate_module(&self, m: &SModule) -> ModuleInterface {
        let mut iface = ModuleInterface::new(self.modpath(m.path));
        for te in &self.stypeexport_pool[m.types.range()] {
            iface
                .types
                .insert(self.s(te.name), self.typeinfos[te.info.0 as usize]);
        }
        for e in &self.sexport_pool[m.values.range()] {
            iface.values.insert(
                self.s(e.name),
                ExportedValue {
                    scheme: self.schemes[e.scheme.0 as usize],
                    local_slot: e.local_slot,
                    param_names: self.str_slice_pool[e.param_names.range()]
                        .iter()
                        .map(|&i| self.s(i))
                        .collect(),
                    doc: e.doc.map(|i| self.s(i)),
                },
            );
        }
        for &i in &self.str_slice_pool[m.private_names.range()] {
            iface.private_names.insert(self.s(i));
        }
        iface.doc = m.doc.map(|i| self.s(i));
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
                name: Arc::from(self.str_pool[f.name.0 as usize]),
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
            .map(|c| {
                match *c {
                    SConst::Int(n) => frozen.int(n),
                    SConst::Float(f) => frozen.float(f),
                    SConst::Bool(b) => frozen.bool(b),
                    SConst::Str(i) => frozen.str(self.str_pool[i.0 as usize]),
                    SConst::StrArray(sl) => {
                        let strs: Vec<&str> = self.str_slice_pool[sl.range()]
                            .iter()
                            .map(|&i| self.str_pool[i.0 as usize])
                            .collect();
                        frozen.str_array(&strs)
                    }
                    SConst::Binary(sl, bit_len) => {
                        frozen.binary_bits(self.byte_pool[sl.range()].to_vec(), bit_len)
                    }
                }
                .into_value()
            })
            .collect();
        (code, functions, constants)
    }
}
