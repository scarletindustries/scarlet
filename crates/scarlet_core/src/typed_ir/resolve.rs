//! Name resolution, finished: the sole producer of [`ValueRef`] and
//! [`TypedCallee`].
//!
//! A top-level `fn` referenced as a **value** must load `PushGlobal slot`;
//! `PushSelf` would read the sentinel `captures` a `CallKnown` frame carries,
//! not the closure. The same name *called* is `CallSelf`. So a top-level fn's
//! self-reference is two different loads depending on position.
//!
//! [`Denotation`] makes the wrong pairing unspellable: its payload is private,
//! and the constructors that denote a top-level fn fix the value load to
//! [`ValueRef::Global`]. [`Denotation::as_value`] and
//! [`Denotation::as_callee`] project the position-dependent halves.
//!
//! [`Denotation::self_closure`] is the *nested lambda* case, where the frame is
//! a real closure frame and `PushSelf` is correct.

use crate::core_ir::{FuncIdx, VariantRef};
use crate::types::ValueKind;
use scarlet_types::intrinsic::Intrinsic;

use super::{Arity, CaptureIdx, GlobalSlot, RTy, TypedCallee, TypedExpr, ValueRef};

/// How a statically-known function is *called*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FnTarget {
    /// `CallKnown func_idx`.
    Known(FuncIdx),
    /// `CallSelf` — the function currently being elaborated.
    SelfRec,
}

/// What a resolved name denotes. Opaque on purpose: see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Denotation(Den);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Den {
    /// An ordinary runtime value. Calling it is a dynamic call.
    Value(ValueRef),
    /// A statically-known function. `place` is how it loads as a value and
    /// `target` is how it is called; the two are never interchangeable, so the
    /// constructors always choose them together.
    Fn { place: ValueRef, target: FnTarget },
    /// A data constructor.
    Ctor { variant: VariantRef, arity: Arity },
    /// A `@vm` builtin: the call *is* the intrinsic.
    Builtin { intrinsic: Intrinsic },
}

/// A name in value position. Not every name is a load: a constructor is a
/// construction, and a non-nullary constructor or builtin needs an eta wrapper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueForm {
    /// `TypedExpr::Var { place }`.
    Ref(ValueRef),
    /// A nullary constructor: `TypedExpr::Ctor { variant, args: [] }`.
    Ctor(VariantRef),
    /// Needs `fn(a0..aN-1) { <apply>(a0..aN-1) }`: a zero-capture
    /// `TypedExpr::Closure` over a `TypedFn` the elaborator synthesises.
    Eta(EtaTarget),
}

/// What an eta wrapper's body applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EtaTarget {
    /// Arity comes from the declaration, not from the (instantiated) type.
    Ctor { variant: VariantRef, arity: Arity },
    /// Arity comes from the instantiated function type via `FnRTy`.
    Builtin { intrinsic: Intrinsic },
}

/// A name in callee position.
#[derive(Debug, Clone, PartialEq)]
pub enum CallForm {
    Callee(TypedCallee),
    /// A constructor call: `TypedExpr::Ctor { variant, args }`, not a call at
    /// all. Payload-free on purpose, so every path goes through the
    /// elaborator's `ctor_of` for label reordering and the arity check.
    Ctor,
}

impl Denotation {
    /// A module-scope value that is not a statically-known function.
    pub(crate) fn global(slot: GlobalSlot) -> Self {
        Denotation(Den::Value(ValueRef::Global(slot)))
    }

    pub(crate) fn capture(idx: CaptureIdx) -> Self {
        Denotation(Den::Value(ValueRef::Capture(idx)))
    }

    /// A top-level `fn` other than the one being elaborated. Called by index,
    /// loaded from its entry-frame slot.
    pub(crate) fn known_fn(slot: GlobalSlot, func_idx: FuncIdx) -> Self {
        Denotation(Den::Fn {
            place: ValueRef::Global(slot),
            target: FnTarget::Known(func_idx),
        })
    }

    /// The top-level `fn` currently being elaborated. `CallSelf` when called,
    /// `PushGlobal slot` when loaded — never `PushSelf`, because a `CallKnown`
    /// frame's `captures` is a sentinel.
    pub(crate) fn self_toplevel_fn(slot: GlobalSlot) -> Self {
        Denotation(Den::Fn {
            place: ValueRef::Global(slot),
            target: FnTarget::SelfRec,
        })
    }

    /// A nested lambda referring to itself. Its frame is a real closure frame,
    /// so `PushSelf` is the right value load.
    pub(crate) fn self_closure() -> Self {
        Denotation(Den::Fn {
            place: ValueRef::SelfClosure,
            target: FnTarget::SelfRec,
        })
    }

    fn ctor(variant: VariantRef, arity: Arity) -> Self {
        Denotation(Den::Ctor { variant, arity })
    }

    fn builtin(intrinsic: Intrinsic) -> Self {
        Denotation(Den::Builtin { intrinsic })
    }

    /// The denotation a name's [`ValueKind`] fixes on its own: a data
    /// constructor or a `@vm` builtin.
    ///
    /// `None` for [`ValueKind::Local`] / [`ValueKind::ModuleFn`], which denote
    /// a *place* only the binding frame knows. Taking no place parameter is
    /// what stops a constructor's frame resolution leaking into a [`ValueRef`].
    ///
    /// Takes no name either, for the same reason: every field of the
    /// [`VariantRef`] is the declaration's, so the name a use site spelled the
    /// constructor with cannot reach codegen. It used to be the one field read
    /// off the binding, which is how an aliased import put `G` where `Green`
    /// belonged.
    pub(crate) fn from_kind(kind: ValueKind) -> Option<Self> {
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
    use crate::type_def::TypeId;
    use crate::types::{ArenaSlice, StrId};

    const TY: RTy = RTy(7);

    fn variant() -> VariantRef {
        VariantRef {
            type_id: TypeId(3),
            variant_idx: 1,
        }
    }

    /// A top-level fn is `PushGlobal slot` as a value and `CallSelf` as a
    /// callee. Nothing can pair them the other way: `Den` is private and both
    /// halves come from one constructor.
    #[test]
    fn a_toplevel_fn_loads_its_global_slot_and_calls_itself() {
        let d = Denotation::self_toplevel_fn(GlobalSlot(4));
        assert_eq!(
            d.as_value(),
            ValueForm::Ref(ValueRef::Global(GlobalSlot(4)))
        );
        assert_eq!(d.as_callee(TY), CallForm::Callee(TypedCallee::SelfRec));
    }

    /// A place-valued name gets no denotation from its `ValueKind` alone: only
    /// the frame that bound it knows where it lives.
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

    /// A nested lambda's frame is a real closure frame, so `PushSelf` is right.
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

    /// A constructor's frame resolution is meaningless and must not leak into
    /// a `ValueRef`. `from_kind` takes no place at all, so it cannot.
    ///
    /// It takes no name either, so the `VariantRef` it builds is the
    /// declaration's in every field. The name a use site bound the constructor
    /// under used to be passed in alongside, and an aliased import made that
    /// the alias.
    #[test]
    fn a_ctors_meaningless_frame_resolution_cannot_leak() {
        let d = Denotation::from_kind(ValueKind::Constructor {
            type_name: StrId(10),
            type_id: TypeId(3),
            variant_idx: 1,
            variant_name: StrId(11),
            arity: 0,
            field_labels: crate::types::ArenaSlice::EMPTY,
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
            param_labels: crate::types::ArenaSlice::EMPTY,
        })
        .expect("a builtin's kind fixes its denotation");
        assert_eq!(d, Denotation::builtin(Intrinsic::StringLength));
        assert_eq!(d.as_ctor(), None);
    }
}
