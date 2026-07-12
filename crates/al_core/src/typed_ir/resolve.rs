//! Name resolution, finished: the sole producer of [`ValueRef`] and
//! [`TypedCallee`].
//!
//! `lower` used to resolve names itself, mapping a `(Ty, ValueKind, VarLoad)`
//! triple onto a `PushLocal`/`PushGlobal`/`PushCapture`/`PushSelf` atom at the
//! point of use. That put one invariant in prose and nowhere else:
//!
//! > A top-level `fn` referenced as a **value** must load `PushGlobal slot`.
//! > `PushSelf` would read the sentinel `captures` a `CallKnown` frame
//! > carries, not the closure.
//!
//! The same name *called* is `CallSelf`. So a top-level fn's self-reference is
//! two different loads depending on position, and the old `VarLoad::SelfGlobal`
//! kept them apart only by every consumer remembering to match it alongside
//! `VarLoad::Self_` in callee position and alongside `VarLoad::Global` in value
//! position.
//!
//! [`Denotation`] makes that unspellable instead. Its payload is private; the
//! only way to denote a top-level fn is [`Denotation::self_toplevel_fn`] or
//! [`Denotation::known_fn`], both of which *fix* the value load to
//! [`ValueRef::Global`]. [`Denotation::as_value`] and
//! [`Denotation::as_callee`] then project the position-dependent halves. A
//! caller cannot pair `SelfClosure` with a top-level fn because it cannot build
//! the pair. `VarLoad` is gone: the module walk builds a `Denotation` straight
//! from its frame, so there is no untyped intermediate for a consumer to
//! mis-project.
//!
//! [`Denotation::self_closure`] is the *nested lambda* case — a `fn(){}` value
//! recursing into itself. There `PushSelf` is correct, because the frame is a
//! closure frame with real captures.

use crate::bytecode::Op;
use crate::core_ir::{FuncIdx, VariantRef};
use crate::types::{StrId, ValueKind};

use super::{
    Arity, BindingId, CaptureIdx, FrameSlot, GlobalSlot, RTy, TypedCallee, TypedExpr, ValueRef,
};

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
    /// A statically-known function. `place` is how it loads as a value,
    /// `target` is how it is called; the two are never interchangeable and are
    /// chosen together by the smart constructors.
    Fn { place: ValueRef, target: FnTarget },
    /// A data constructor.
    Ctor { variant: VariantRef, arity: Arity },
    /// A `@vm` builtin: the call *is* the opcode.
    Builtin { op: Op },
}

/// A name in value position. Not every name is a load: a constructor is a
/// construction, and a non-nullary constructor or a builtin is a request for an
/// eta wrapper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueForm {
    /// `TypedExpr::Var { place }`.
    Ref(ValueRef),
    /// A nullary constructor: `TypedExpr::Ctor { variant, args: [] }`.
    Ctor(VariantRef),
    /// Needs `fn(a0..aN-1) { <apply>(a0..aN-1) }` — a zero-capture
    /// `TypedExpr::Closure` over a `TypedFn` the elaborator synthesises.
    Eta(EtaTarget),
}

/// What an eta wrapper's body applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EtaTarget {
    /// Arity comes from the declaration, not from the (instantiated) type.
    Ctor { variant: VariantRef, arity: Arity },
    /// Arity is the wrapped type's `ResolvedPool::arity`.
    Builtin { op: Op },
}

/// A name in callee position.
#[derive(Debug, Clone, PartialEq)]
pub enum CallForm {
    Callee(TypedCallee),
    /// A constructor call: `TypedExpr::Ctor { variant, args }`, not a call at
    /// all. Deliberately payload-free — the elaborator's `ctor_of` re-derives
    /// the variant through [`Denotation::as_ctor`] and owns label reordering
    /// and the arity check, so no call path can bypass it.
    Ctor,
}

impl Denotation {
    /// A binding the elaborator itself minted in the current function.
    pub fn local(id: BindingId) -> Self {
        Denotation(Den::Value(ValueRef::Local(id)))
    }

    /// A raw frame slot the module walk assigned — see [`ValueRef::Slot`].
    pub fn slot(slot: FrameSlot) -> Self {
        Denotation(Den::Value(ValueRef::Slot(slot)))
    }

    /// A module-scope value that is not a statically-known function.
    pub fn global(slot: GlobalSlot) -> Self {
        Denotation(Den::Value(ValueRef::Global(slot)))
    }

    pub fn capture(idx: CaptureIdx) -> Self {
        Denotation(Den::Value(ValueRef::Capture(idx)))
    }

    /// A top-level `fn` other than the one being elaborated. Called by index;
    /// loaded — necessarily — from its entry-frame slot.
    pub fn known_fn(slot: GlobalSlot, func_idx: FuncIdx) -> Self {
        Denotation(Den::Fn {
            place: ValueRef::Global(slot),
            target: FnTarget::Known(func_idx),
        })
    }

    /// The top-level `fn` currently being elaborated. `CallSelf` when called;
    /// `PushGlobal slot` — never `PushSelf` — when loaded as a value, because a
    /// `CallKnown` frame's `captures` is a sentinel.
    pub fn self_toplevel_fn(slot: GlobalSlot) -> Self {
        Denotation(Den::Fn {
            place: ValueRef::Global(slot),
            target: FnTarget::SelfRec,
        })
    }

    /// A nested lambda referring to itself. Its frame is a real closure frame,
    /// so `PushSelf` is the correct value load.
    pub fn self_closure() -> Self {
        Denotation(Den::Fn {
            place: ValueRef::SelfClosure,
            target: FnTarget::SelfRec,
        })
    }

    pub fn ctor(variant: VariantRef, arity: Arity) -> Self {
        Denotation(Den::Ctor { variant, arity })
    }

    pub fn builtin(op: Op) -> Self {
        Denotation(Den::Builtin { op })
    }

    /// The denotation a name's [`ValueKind`] fixes on its own, with no help
    /// from the frame: a data constructor or a `@vm` builtin. `name` is the
    /// interned source name, used only to complete a constructor's
    /// [`VariantRef`].
    ///
    /// `None` for [`ValueKind::Local`] / [`ValueKind::ModuleFn`] — those denote
    /// a *place*, and only the frame that bound them knows which. This is why a
    /// constructor's frame resolution can no longer leak into a [`ValueRef`]:
    /// there is no parameter here to leak from.
    pub fn from_kind(kind: ValueKind, name: StrId) -> Option<Self> {
        match kind {
            ValueKind::Constructor {
                type_id,
                variant_idx,
                type_name,
                arity,
                ..
            } => Some(Denotation::ctor(
                VariantRef {
                    type_id,
                    variant_idx,
                    type_name,
                    variant_name: name,
                },
                Arity(arity),
            )),
            ValueKind::Builtin { op } => Some(Denotation::builtin(op)),
            ValueKind::Local | ValueKind::ModuleFn => None,
        }
    }

    /// The name in value position.
    pub fn as_value(&self) -> ValueForm {
        match self.0 {
            Den::Value(place) | Den::Fn { place, .. } => ValueForm::Ref(place),
            Den::Ctor {
                variant,
                arity: Arity(0),
            } => ValueForm::Ctor(variant),
            Den::Ctor { variant, arity } => ValueForm::Eta(EtaTarget::Ctor { variant, arity }),
            Den::Builtin { op } => ValueForm::Eta(EtaTarget::Builtin { op }),
        }
    }

    /// The name in callee position. `fn_ty` is the callee's own resolved type,
    /// needed only to type the `TypedExpr::Var` a dynamic call evaluates.
    pub fn as_callee(&self, fn_ty: RTy) -> CallForm {
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
            Den::Builtin { op } => CallForm::Callee(TypedCallee::Builtin(op)),
        }
    }

    /// The variant and declared arity this name constructs, for a pattern's
    /// use site. `None` for anything that is not a constructor.
    pub fn as_ctor(&self) -> Option<(VariantRef, u16)> {
        match self.0 {
            Den::Ctor { variant, arity } => Some((variant, arity.0)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::type_def::TypeId;

    const TY: RTy = RTy(7);

    fn variant() -> VariantRef {
        VariantRef {
            type_id: TypeId(3),
            variant_idx: 1,
            type_name: 10,
            variant_name: 11,
        }
    }

    /// The invariant the old `VarLoad::SelfGlobal` documented in prose: a
    /// top-level fn is `PushGlobal slot` as a value and `CallSelf` as a callee.
    /// Neither the
    /// caller nor a future arm can pair the other way — `Den` is private and
    /// both halves come from one constructor.
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
    /// the frame that bound it knows where it lives. This is what makes the
    /// `SelfGlobal` pairing above the *only* way to spell a top-level fn.
    #[test]
    fn the_kind_bridge_defers_places_to_the_frame() {
        assert_eq!(Denotation::from_kind(ValueKind::ModuleFn, 0), None);
        assert_eq!(Denotation::from_kind(ValueKind::Local, 0), None);
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

    /// A module-scope *value* (a `const`, an import re-exported as a value) has
    /// no `func_idx`, so calling it is a dynamic call through its load.
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

    /// A frame slot in resolved-name position is never a `BindingId`: the
    /// elaborator's own binds are found before the walk is consulted, so what
    /// is left is a slot the walk minted (a selective `import mod.{x}`).
    #[test]
    fn a_walk_frame_slot_is_the_escape_hatch() {
        let d = Denotation::slot(FrameSlot(6));
        assert_eq!(d.as_value(), ValueForm::Ref(ValueRef::Slot(FrameSlot(6))));
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
        assert_eq!(d.as_ctor().map(|(_, a)| a), Some(2));
    }

    #[test]
    fn a_builtin_is_an_opcode_when_called_and_a_wrapper_when_loaded() {
        let d = Denotation::builtin(Op::Add);
        assert_eq!(
            d.as_callee(TY),
            CallForm::Callee(TypedCallee::Builtin(Op::Add))
        );
        assert_eq!(
            d.as_value(),
            ValueForm::Eta(EtaTarget::Builtin { op: Op::Add })
        );
        assert_eq!(d.as_ctor().map(|(_, a)| a), None);
    }

    /// A constructor's frame resolution is meaningless (`resolve_name` finds no
    /// place for it) and must not leak into a `ValueRef`. `from_kind` takes no
    /// place at all, so it cannot.
    #[test]
    fn a_ctors_meaningless_frame_resolution_cannot_leak() {
        let d = Denotation::from_kind(
            ValueKind::Constructor {
                type_name: 10,
                type_id: TypeId(3),
                variant_idx: 1,
                arity: 0,
                field_labels: crate::types::ArenaSlice::EMPTY,
            },
            11,
        )
        .expect("a constructor's kind fixes its denotation");
        assert_eq!(d.as_value(), ValueForm::Ctor(variant()));
        assert_eq!(d.as_ctor(), Some((variant(), 0)));
    }

    /// A builtin's kind fixes it too, and it is not a constructor.
    #[test]
    fn a_builtins_kind_fixes_its_denotation() {
        let d = Denotation::from_kind(ValueKind::Builtin { op: Op::Add }, 0)
            .expect("a builtin's kind fixes its denotation");
        assert_eq!(d, Denotation::builtin(Op::Add));
        assert_eq!(d.as_ctor(), None);
    }
}
