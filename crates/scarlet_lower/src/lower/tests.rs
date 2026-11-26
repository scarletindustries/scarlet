use super::*;
use crate::testkit::vref;
use crate::typed_ir::{Gives, TypedPat};
use crate::{ProbeOp, VariantRef};
use scarlet_ir::TypeId;

const ANY: RTy = RTy(0);
const OPTION: TypeId = TypeId(1);

fn ctors() -> PreludeCtors {
    PreludeCtors {
        some: vref(OPTION.0, 0),
        none: vref(OPTION.0, 1),
        ok: vref(2, 0),
        err: vref(2, 1),
        true_: vref(3, 0),
        false_: vref(3, 1),
        nil: vref(4, 0),
    }
}

/// A function of one parameter, `BindingId(0)`, that is `body`.
fn lowered(body: TypedExpr) -> Result<String, InternalLinkError> {
    let shapes = Shapes::new([(OPTION, vec![1, 0]), (TypeId(2), vec![1, 1])]);
    let around = Around {
        temps: TempTys {
            int: ANY,
            bool: ANY,
            string: ANY,
            binary: ANY,
            int_pair: ANY,
        },
        ctors: ctors(),
        shapes: &shapes,
        consts: &[],
        allowance: Allowance::USUAL,
    };
    let f = TypedFn {
        params: vec![named(0)],
        ret: ANY,
        body,
        binds: 10,
    };
    match lower_fn(around, &f) {
        Ok(lowered) => Ok(lowered.core.body.to_string()),
        Err(LinkError::Internal(why)) => Err(why),
        Err(why) => panic!("{why}"),
    }
}

fn named(id: usize) -> TypedBind {
    TypedBind {
        id: BindingId(id),
        named: true,
        ty: ANY,
        global: None,
    }
}

fn var(id: usize) -> TypedExpr {
    TypedExpr::Var {
        ty: ANY,
        place: ValueRef::Local(BindingId(id)),
    }
}

fn probe(args: Vec<TypedExpr>, gives: Gives) -> TypedExpr {
    TypedExpr::Probe {
        ty: ANY,
        op: ProbeOp::ArraySet,
        args,
        found: ANY,
        gives,
    }
}

fn given(gives: Gives) -> TypedExpr {
    probe(vec![var(0), var(0), var(0)], gives)
}

const ALL: [Gives; 3] = [Gives::Option, Gives::Result { nil: ANY }, Gives::OrFirst];

fn block(bind: TypedBind, init: TypedExpr, body: TypedExpr) -> TypedExpr {
    TypedExpr::Let {
        ty: ANY,
        bind,
        init: Box::new(init),
        body: Box::new(body),
    }
}

fn seq(effect: TypedExpr, body: TypedExpr) -> TypedExpr {
    TypedExpr::Seq {
        ty: ANY,
        effect: Box::new(effect),
        body: Box::new(body),
    }
}

fn match_on(scrut: TypedExpr, arms: Vec<(TypedPat, Option<TypedExpr>, TypedExpr)>) -> TypedExpr {
    let arm = |(pat, guard, body)| TypedArm { pat, guard, body };
    TypedExpr::Match {
        ty: ANY,
        scrut: Box::new(scrut),
        arms: arms.into_iter().map(arm).collect(),
    }
}

fn wild() -> TypedPat {
    TypedPat::Wild { ty: ANY }
}

fn ctor(variant: VariantRef, fields: Vec<TypedPat>) -> TypedPat {
    TypedPat::Ctor {
        ty: ANY,
        variant,
        fields,
    }
}

/// A probe that gives back its first operand where there is nothing, and has none.
#[test]
fn a_probe_of_nothing_cannot_give_it_back() {
    let why = InternalLinkError::ProbeOperands(ProbeOp::ArraySet);
    assert_eq!(lowered(probe(Vec::new(), Gives::OrFirst)), Err(why.clone()));
    assert!(why.to_string().ends_with("ArraySet of nothing"), "{why}");
}

/// A pattern may ask for the one field of what a probe has come out as, and for no
/// other part of it.
#[test]
fn a_part_that_a_probes_value_has_not_is_refused() {
    let Ok(()) = lowered(match_on(
        given(Gives::Option),
        vec![
            (
                ctor(ctors().some, vec![TypedPat::Bind(named(1))]),
                None,
                var(1),
            ),
            (wild(), None, var(0)),
        ],
    ))
    .map(drop) else {
        panic!("its field is there");
    };
    let odd = [
        ctor(ctors().some, vec![wild(), TypedPat::Bind(named(1))]),
        ctor(ctors().none, vec![TypedPat::Bind(named(1))]),
    ];
    for pat in odd {
        let shown = format!("{pat:?}");
        let arms = vec![(pat, None, var(1)), (wild(), None, var(0))];
        let got = lowered(match_on(given(Gives::Option), arms));
        assert!(
            matches!(got, Err(InternalLinkError::NoPlace(_))),
            "{shown}: {got:?}"
        );
    }
}

/// A value that nothing reads is not made, and is bound to nothing: not where a match
/// reads no part of it, nor behind a name that nothing reads. What is asked is asked.
#[test]
fn a_probes_value_that_nothing_reads_is_not_made() {
    let only_asked = |what: &str, e: TypedExpr| {
        let core = lowered(e).expect("it is lowered");
        assert_eq!(core.matches("probe ArraySet").count(), 1, "{what}: {core}");
        assert_eq!(core.matches("jump j0()").count(), 2, "{what}: {core}");
        for part in ["ctor", "nil", "switch"] {
            assert!(!core.contains(part), "{what}: {core}");
        }
    };
    for gives in ALL {
        let on = || given(gives);
        only_asked("a statement", seq(on(), var(0)));
        only_asked("a name", block(named(1), on(), var(0)));
        let name = TypedBind {
            named: false,
            ..named(1)
        };
        only_asked("a temporary", block(name, on(), var(0)));
        // A block that ends in it, in front of something else.
        let inner = block(named(2), var(0), on());
        only_asked("a block", seq(inner, var(0)));
        // The same, where it is not on the outermost spine.
        let nested = |e: TypedExpr| TypedExpr::Tuple {
            ty: ANY,
            elems: vec![e],
        };
        only_asked("a nested name", nested(block(named(1), on(), var(0))));
        only_asked("a nested statement", nested(seq(on(), var(0))));
    }
    for gives in [Gives::Option, Gives::Result { nil: ANY }] {
        let on = || given(gives);
        let [hit, miss] = match gives {
            Gives::Option => [ctors().some, ctors().none],
            Gives::Result { .. } | Gives::OrFirst => [ctors().ok, ctors().err],
        };
        only_asked("a wildcard", match_on(on(), vec![(wild(), None, var(0))]));
        let name = TypedPat::Bind(named(1));
        only_asked("a name in it", match_on(on(), vec![(name, None, var(0))]));
        let of = |v: VariantRef| ctor(v, vec![wild(); usize::from(v != ctors().none)]);
        let both = TypedPat::Or {
            ty: ANY,
            alts: vec![of(hit), of(miss)],
        };
        only_asked("both", match_on(on(), vec![(both, None, var(0))]));
        let guarded = vec![(wild(), Some(var(0)), var(0)), (wild(), None, var(0))];
        only_asked("a guard", match_on(on(), guarded));
    }
}

/// A value that something reads is made, however it is named: one that a module holds
/// is read by others.
#[test]
fn a_probes_value_that_something_may_read_is_made() {
    for (gives, ctors) in ALL.into_iter().zip([2, 2, 0]) {
        let on = || given(gives);
        let global = TypedBind {
            global: Some(crate::GlobalSlot(0)),
            ..named(1)
        };
        let kept = [
            block(global, on(), var(0)),
            block(named(1), on(), var(1)),
            match_on(on(), vec![(TypedPat::Bind(named(1)), None, var(1))]),
            match_on(
                on(),
                vec![
                    (wild(), Some(var(0)), var(0)),
                    (TypedPat::Bind(named(1)), None, var(1)),
                ],
            ),
        ];
        for e in kept {
            let core = lowered(e).expect("it is lowered");
            assert_eq!(core.matches("ctor").count(), ctors, "{core}");
            assert!(core.contains("join j0(%"), "{core}");
        }
    }
}
