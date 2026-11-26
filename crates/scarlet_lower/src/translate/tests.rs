use super::*;
use scarlet_ir::TypeId;
use scarlet_ir::rty::PrimIds;

/// The value of a probe has a type that says what it is made of. One that does not is
/// an error that names the probe and the type.
#[test]
fn a_probe_whose_type_holds_nothing_is_refused() {
    let mut pool = ResolvedPool::new(PrimIds::default());
    let mut con = |id, args: &[RTy]| {
        let ty = pool.mk_con(TypeId(id), Repr::Counted, args);
        ty.expect("a type")
    };
    let plain = con(40, &[]);
    let one = con(41, &[plain]);
    let two = con(42, &[one, plain]);
    let refused = |e: R<TypedExpr>| match e {
        Err(LinkError::Internal(why)) => why,
        other => panic!("{other:?}"),
    };
    for (intrinsic, op) in [
        (Intrinsic::MapGet, ProbeOp::MapGet),
        (Intrinsic::ArraySet, ProbeOp::ArraySet),
    ] {
        let why = refused(built_in(&pool, intrinsic, Vec::new(), plain));
        assert_eq!(why, InternalLinkError::ProbeType(op, plain));
    }
    let slice = |ty| as_result(&pool, ProbeOp::ArraySlice, Vec::new(), ty);
    for ty in [plain, one] {
        let why = InternalLinkError::ProbeType(ProbeOp::ArraySlice, ty);
        assert_eq!(refused(slice(ty)), why);
    }
    let index = as_option(&pool, ProbeOp::ArrayGet, Vec::new(), plain);
    let why = refused(index);
    let text = format!("ArrayGet as a value of type :{}, which holds none", plain.0);
    assert!(why.to_string().contains(&text), "{why}");
    // What each is made of, where it is there.
    let made = |e: R<TypedExpr>| match e {
        Ok(TypedExpr::Probe { found, gives, .. }) => (found, gives),
        other => panic!("{other:?}"),
    };
    let index = as_option(&pool, ProbeOp::ArrayGet, Vec::new(), one);
    assert_eq!(made(index), (plain, Gives::Option));
    assert_eq!(made(slice(two)), (one, Gives::Result { nil: plain }));
    let put = built_in(&pool, Intrinsic::ArrayPut, Vec::new(), one);
    assert_eq!(made(put), (one, Gives::OrFirst));
}
