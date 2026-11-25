//! What each of a program's constructors is to the VM: a value that is all in
//! one word, or a cell with a class of its own.
//!
//! The program says how many fields each constructor has
//! ([`Program::types`](scarlet_ir::core_ir::Program::types)). Every mention of
//! one is checked against that as the program loads, and so is every
//! constructor the VM builds on its own ([`Built`]). So a cell always has as
//! many fields as its class says.

use std::collections::BTreeMap;

use scarlet_ir::TypeId;
use scarlet_ir::core_ir::{Naming, Rename, VariantRef};

use crate::heap::{Ctor, Ctors, Numbering, TooManyCtors};
use crate::show::Types;
use crate::stop::{Broken, Stop};
use crate::value::{Tag, Value};

/// What a constructor is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Form {
    /// One with no fields. It needs no cell.
    Plain(VariantRef),
    Fields {
        ctor: Ctor,
        fields: usize,
    },
}

impl Form {
    /// The top half of every value of the constructor.
    pub(super) fn tag(self) -> Tag {
        match self {
            Form::Plain(v) => Value::nullary(v).tag(),
            Form::Fields { ctor, .. } => Tag::of_cell(ctor.variant()),
        }
    }

    fn fields(self) -> usize {
        match self {
            Form::Plain(_) => 0,
            Form::Fields { fields, .. } => fields,
        }
    }

    /// The fault of using constructor `v`, whose form this is, as one with
    /// `got` fields.
    fn unlike(self, v: VariantRef, got: usize) -> Broken {
        Broken::CtorFields {
            ctor: v,
            got,
            want: self.fields(),
        }
    }
}

/// Every constructor of every type the program names.
#[derive(Debug, Default)]
pub(super) struct Forms {
    /// Indexed by [`VariantRef::variant_idx`].
    types: BTreeMap<TypeId, Vec<Form>>,
}

impl Forms {
    /// The forms of the constructors in `types`, and the table of those that
    /// have cells.
    pub(super) fn new(types: &Types) -> Result<(Forms, Ctors), Stop> {
        let mut numbering = Numbering::default();
        let mut forms = Forms::default();
        for (type_id, names) in types {
            let mut of_type = Vec::with_capacity(names.variants.len());
            for (variant_idx, variant) in (0..=u16::MAX).zip(&names.variants) {
                let variant_ref = VariantRef {
                    type_id: *type_id,
                    variant_idx,
                };
                of_type.push(match variant.fields.len() {
                    0 => Form::Plain(variant_ref),
                    fields => Form::Fields {
                        ctor: numbering.add(variant_ref, fields)?,
                        fields,
                    },
                });
            }
            forms.types.insert(*type_id, of_type);
        }
        Ok((forms, numbering.finish()))
    }

    /// The form of constructor `v`, which something uses as one with `fields`
    /// fields.
    pub(super) fn of(&self, v: VariantRef, fields: usize) -> Result<Form, Broken> {
        let form = self.get(v)?;
        if form.fields() == fields {
            Ok(form)
        } else {
            Err(form.unlike(v, fields))
        }
    }

    pub(super) fn get(&self, v: VariantRef) -> Result<Form, Broken> {
        let of_type = self.types.get(&v.type_id);
        let form = of_type.and_then(|forms| forms.get(usize::from(v.variant_idx)));
        form.copied().ok_or(Broken::NoCtor(v))
    }
}

/// The constructors the VM builds on its own, as it builds them: one with no
/// fields is its value, and one with fields is what its cells are made with.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Built;

impl Naming for Built {
    type Plain = Value;
    type Fields<const N: usize> = CtorOf<N>;
}

/// A constructor the program lists with `N` fields, for the VM to build with
/// that many. Only the loader makes one, where it has read the list.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CtorOf<const N: usize>(Ctor);

impl<const N: usize> CtorOf<N> {
    pub(crate) fn ctor(self) -> Ctor {
        self.0
    }
}

impl Rename for &Forms {
    type To = Built;
    type Error = Broken;

    fn plain(&mut self, v: VariantRef) -> Result<Value, Broken> {
        self.of(v, 0).map(|_| Value::nullary(v))
    }

    fn fields<const N: usize>(&mut self, v: VariantRef) -> Result<CtorOf<N>, Broken> {
        match self.get(v)? {
            Form::Fields { ctor, fields } if fields == N => Ok(CtorOf(ctor)),
            form @ (Form::Plain(_) | Form::Fields { .. }) => Err(form.unlike(v, N)),
        }
    }
}

impl From<TooManyCtors> for Stop {
    fn from(_: TooManyCtors) -> Stop {
        Stop::TooManyConstructors
    }
}

#[cfg(test)]
mod tests;
