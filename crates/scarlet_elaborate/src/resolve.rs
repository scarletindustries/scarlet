//! What a resolved name means. This is the only place that builds a
//! [`ValueRef`] or a [`TypedCallee`].
//!
//! A top-level `fn` named as a **value** loads its entry-frame slot
//! ([`ValueRef::Global`]), even inside its own body. It is called directly,
//! with no closure of its own, so reading itself as a closure
//! ([`ValueRef::SelfClosure`]) is wrong there. The same name *called* inside
//! its own body is a self-call ([`TypedCallee::SelfRec`]). So a top-level fn
//! refers to itself in two different ways depending on where the name sits.
//!
//! [`Denotation`] makes the wrong pairing impossible to write. Its payload is
//! private, and the constructors for a top-level fn always pick
//! [`ValueRef::Global`] as the value load. [`Denotation::as_value`] and
//! [`Denotation::as_callee`] give back the half each position needs.
//!
//! [`Denotation::self_closure`] covers the *nested lambda*, whose frame is a
//! real closure frame, so reading itself as a closure is correct.

use scarlet_ir::core_ir::{FuncIdx, VariantRef};
use scarlet_types::intrinsic::Intrinsic;
use scarlet_types::types::ValueKind;

use super::{Arity, CaptureIdx, GlobalSlot, RTy, TypedCallee, TypedExpr, ValueRef};

/// How a statically-known function is *called*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FnTarget {
    /// A direct call to `func_idx`.
    Known(FuncIdx),
    /// A self-call to the function currently being elaborated.
    SelfRec,
}

/// What a resolved name denotes. It's opaque on purpose, as the module docs
/// explain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Denotation(Den);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Den {
    /// An ordinary runtime value. Calling it is a dynamic call.
    Value(ValueRef),
    /// A function known at compile time. `place` is how it loads as a value
    /// and `target` is how it is called. The two can't be swapped, so the
    /// constructors always choose them together.
    Fn { place: ValueRef, target: FnTarget },
    /// A data constructor.
    Ctor { variant: VariantRef, arity: Arity },
    /// A `@vm` builtin, where the call *is* the intrinsic.
    Builtin { intrinsic: Intrinsic },
}

/// A name in value position. Not every name is a load. A constructor builds a
/// value, and a constructor with fields or a builtin needs an eta wrapper: a
/// small function that takes the arguments and applies it, so it can be passed
/// around like any other function value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueForm {
    /// `TypedExpr::Var { place }`.
    Ref(ValueRef),
    /// A nullary constructor: `TypedExpr::Ctor { variant, args: [] }`.
    Ctor(VariantRef),
    /// A name that needs `fn(a0..aN-1) { <apply>(a0..aN-1) }`. That is a
    /// `TypedExpr::Closure` with no captures over a `TypedFn` the elaborator
    /// generates.
    Eta(EtaTarget),
}

/// What an eta wrapper's body applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EtaTarget {
    /// The arity comes from the declaration, not from the instantiated type.
    Ctor { variant: VariantRef, arity: Arity },
    /// The arity comes from the instantiated function type, through `FnRTy`.
    Builtin { intrinsic: Intrinsic },
}

/// A name in callee position.
#[derive(Debug, Clone, PartialEq)]
pub enum CallForm {
    Callee(TypedCallee),
    /// A constructor call, which becomes `TypedExpr::Ctor { variant, args }`
    /// and not a call at all. It carries nothing on purpose, so every path
    /// goes through the elaborator's `ctor_of` for label reordering and the
    /// arity check.
    Ctor,
}

impl Denotation {
    /// A module-scope value that is not a function known at compile time.
    pub fn global(slot: GlobalSlot) -> Self {
        Denotation(Den::Value(ValueRef::Global(slot)))
    }

    pub fn capture(idx: CaptureIdx) -> Self {
        Denotation(Den::Value(ValueRef::Capture(idx)))
    }

    /// A top-level `fn` other than the one being elaborated. It is called by
    /// index and loaded from its entry-frame slot.
    pub fn known_fn(slot: GlobalSlot, func_idx: FuncIdx) -> Self {
        Denotation(Den::Fn {
            place: ValueRef::Global(slot),
            target: FnTarget::Known(func_idx),
        })
    }

    /// The top-level `fn` currently being elaborated. Calling it is a
    /// self-call, and loading it reads its global slot. It never loads itself
    /// as a closure, because it runs without one.
    pub fn self_toplevel_fn(slot: GlobalSlot) -> Self {
        Denotation(Den::Fn {
            place: ValueRef::Global(slot),
            target: FnTarget::SelfRec,
        })
    }

    /// A nested lambda referring to itself. Its frame is a real closure frame,
    /// so loading itself as a closure is right.
    pub fn self_closure() -> Self {
        Denotation(Den::Fn {
            place: ValueRef::SelfClosure,
            target: FnTarget::SelfRec,
        })
    }

    /// Turn a module-scope value into the known function that `fn_at` says
    /// its slot holds. Every other denotation comes back unchanged.
    pub(crate) fn promoted(self, fn_at: impl FnOnce(GlobalSlot) -> Option<FuncIdx>) -> Self {
        let Den::Value(ValueRef::Global(slot)) = self.0 else {
            return self;
        };
        match fn_at(slot) {
            Some(func_idx) => Self::known_fn(slot, func_idx),
            None => self,
        }
    }

    fn ctor(variant: VariantRef, arity: Arity) -> Self {
        Denotation(Den::Ctor { variant, arity })
    }

    fn builtin(intrinsic: Intrinsic) -> Self {
        Denotation(Den::Builtin { intrinsic })
    }

    /// The denotation a name's [`ValueKind`] decides on its own, which is a
    /// data constructor or a `@vm` builtin.
    ///
    /// It returns `None` for [`ValueKind::Local`] and [`ValueKind::ModuleFn`],
    /// which name a *place* only the frame that bound them knows. Because this
    /// takes no place, a constructor's frame resolution can't leak into a
    /// [`ValueRef`].
    ///
    /// It takes no name either, for the same reason. Every field of the
    /// [`VariantRef`] comes from the declaration, so the name a use site wrote
    /// for the constructor can't reach codegen. The name used to be the one
    /// field read off the binding, which is how an aliased import put `G`
    /// where `Green` belonged.
    pub fn from_kind(kind: ValueKind) -> Option<Self> {
        match kind {
            ValueKind::Constructor {
                type_id,
                variant_idx,
                arity,
                ..
            } => Some(Denotation::ctor(
                VariantRef {
                    type_id,
                    variant_idx,
                },
                Arity(arity),
            )),
            ValueKind::Builtin { intrinsic, .. } => Some(Denotation::builtin(intrinsic)),
            ValueKind::Local | ValueKind::ModuleFn { .. } => None,
        }
    }

    /// The name in value position.
    pub(crate) fn as_value(&self) -> ValueForm {
        match self.0 {
            Den::Value(place) | Den::Fn { place, .. } => ValueForm::Ref(place),
            Den::Ctor {
                variant,
                arity: Arity(0),
            } => ValueForm::Ctor(variant),
            Den::Ctor { variant, arity } => ValueForm::Eta(EtaTarget::Ctor { variant, arity }),
            Den::Builtin { intrinsic } => ValueForm::Eta(EtaTarget::Builtin { intrinsic }),
        }
    }

    /// The name in callee position. `fn_ty` types the `TypedExpr::Var` a
    /// dynamic call evaluates, and is unused otherwise.
    pub(crate) fn as_callee(&self, fn_ty: RTy) -> CallForm {
        match self.0 {
            Den::Fn {
                target: FnTarget::Known(fi),
                ..
            } => CallForm::Callee(TypedCallee::Known(fi)),
            Den::Fn {
                target: FnTarget::SelfRec,
                ..
            } => CallForm::Callee(TypedCallee::SelfRec),
            Den::Value(place) => CallForm::Callee(TypedCallee::Dynamic(Box::new(TypedExpr::Var {
                ty: fn_ty,
                place,
            }))),
            Den::Ctor { .. } => CallForm::Ctor,
            Den::Builtin { intrinsic } => CallForm::Callee(TypedCallee::Builtin { intrinsic }),
        }
    }

    /// The variant and declared [`Arity`] this name constructs.
    pub(crate) fn as_ctor(&self) -> Option<(VariantRef, Arity)> {
        match self.0 {
            Den::Ctor { variant, arity } => Some((variant, arity)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scarlet_types::type_def::TypeId;
    use scarlet_types::types::{ArenaSlice, StrId};

    const TY: RTy = RTy(7);

    fn variant() -> VariantRef {
        VariantRef {
            type_id: TypeId(3),
            variant_idx: 1,
        }
    }

    /// A top-level fn loads its global slot as a value and is a self-call as a
    /// callee. Nothing can pair them the other way, because `Den` is private
    /// and both halves come from one constructor.
    #[test]
    fn a_toplevel_fn_loads_its_global_slot_and_calls_itself() {
        let d = Denotation::self_toplevel_fn(GlobalSlot(4));
        assert_eq!(
            d.as_value(),
            ValueForm::Ref(ValueRef::Global(GlobalSlot(4)))
        );
        assert_eq!(d.as_callee(TY), CallForm::Callee(TypedCallee::SelfRec));
    }

    /// A name that stands for a place gets no denotation from its `ValueKind`
    /// alone, because only the frame that bound it knows where it lives.
    #[test]
    fn the_kind_bridge_defers_places_to_the_frame() {
        assert_eq!(
            Denotation::from_kind(ValueKind::ModuleFn {
                param_labels: ArenaSlice::EMPTY
            }),
            None
        );
        assert_eq!(Denotation::from_kind(ValueKind::Local), None);
    }

    /// A nested lambda's frame is a real closure frame, so it can load itself.
    #[test]
    fn a_nested_lambda_loads_push_self() {
        let d = Denotation::self_closure();
        assert_eq!(d.as_value(), ValueForm::Ref(ValueRef::SelfClosure));
        assert_eq!(d.as_callee(TY), CallForm::Callee(TypedCallee::SelfRec));
    }

    #[test]
    fn a_known_toplevel_fn_calls_by_index_and_loads_by_slot() {
        let d = Denotation::known_fn(GlobalSlot(2), FuncIdx(9));
        assert_eq!(
            d.as_value(),
            ValueForm::Ref(ValueRef::Global(GlobalSlot(2)))
        );
        assert_eq!(
            d.as_callee(TY),
            CallForm::Callee(TypedCallee::Known(FuncIdx(9)))
        );
    }

    /// A module-scope *value* has no `func_idx`, so calling it is a dynamic
    /// call through its load.
    #[test]
    fn a_plain_global_calls_dynamically_through_its_load() {
        let d = Denotation::global(GlobalSlot(5));
        assert_eq!(
            d.as_callee(TY),
            CallForm::Callee(TypedCallee::Dynamic(Box::new(TypedExpr::Var {
                ty: TY,
                place: ValueRef::Global(GlobalSlot(5)),
            })))
        );
    }

    #[test]
    fn a_capture_loads_by_index() {
        let d = Denotation::capture(CaptureIdx(3));
        assert_eq!(
            d.as_value(),
            ValueForm::Ref(ValueRef::Capture(CaptureIdx(3)))
        );
    }

    #[test]
    fn a_nullary_ctor_is_a_construction_not_a_load() {
        let d = Denotation::ctor(variant(), Arity(0));
        assert_eq!(d.as_value(), ValueForm::Ctor(variant()));
        assert_eq!(d.as_callee(TY), CallForm::Ctor);
    }

    #[test]
    fn a_ctor_as_a_value_asks_for_an_eta_wrapper() {
        let d = Denotation::ctor(variant(), Arity(2));
        assert_eq!(
            d.as_value(),
            ValueForm::Eta(EtaTarget::Ctor {
                variant: variant(),
                arity: Arity(2)
            })
        );
        assert_eq!(d.as_callee(TY), CallForm::Ctor);
        assert_eq!(d.as_ctor().map(|(_, a)| a), Some(Arity(2)));
    }

    #[test]
    fn a_builtin_is_a_direct_call_when_called_and_a_wrapper_when_loaded() {
        let d = Denotation::builtin(Intrinsic::StringLength);
        assert_eq!(
            d.as_callee(TY),
            CallForm::Callee(TypedCallee::Builtin {
                intrinsic: Intrinsic::StringLength,
            })
        );
        assert_eq!(
            d.as_value(),
            ValueForm::Eta(EtaTarget::Builtin {
                intrinsic: Intrinsic::StringLength,
            })
        );
        assert_eq!(d.as_ctor().map(|(_, a)| a), None);
    }

    /// A constructor's frame resolution means nothing and must not leak into
    /// a `ValueRef`. `from_kind` takes no place at all, so it can't.
    ///
    /// It takes no name either, so every field of the `VariantRef` it builds
    /// comes from the declaration. The name a use site bound the constructor
    /// to used to be passed in as well, and an aliased import made that the
    /// alias.
    #[test]
    fn a_ctors_meaningless_frame_resolution_cannot_leak() {
        let d = Denotation::from_kind(ValueKind::Constructor {
            type_name: StrId(10),
            type_id: TypeId(3),
            variant_idx: 1,
            variant_name: StrId(11),
            arity: 0,
            field_labels: scarlet_types::types::ArenaSlice::EMPTY,
        })
        .expect("a constructor's kind fixes its denotation");
        assert_eq!(d.as_value(), ValueForm::Ctor(variant()));
        assert_eq!(d.as_ctor(), Some((variant(), Arity(0))));
    }

    /// A builtin's kind fixes it too, and it is not a constructor.
    #[test]
    fn a_builtins_kind_fixes_its_denotation() {
        let d = Denotation::from_kind(ValueKind::Builtin {
            intrinsic: Intrinsic::StringLength,
            param_labels: scarlet_types::types::ArenaSlice::EMPTY,
        })
        .expect("a builtin's kind fixes its denotation");
        assert_eq!(d, Denotation::builtin(Intrinsic::StringLength));
        assert_eq!(d.as_ctor(), None);
    }
}
