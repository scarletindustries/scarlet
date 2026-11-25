use super::*;

impl ResolvedPool {
    fn tuple_elems(&self, t: RTy) -> Result<&[RTy], ForeignRTy> {
        match self.node(t)? {
            ResolvedNode::Tuple { elems } => self.children(t, elems),
            ResolvedNode::Bound(_) | ResolvedNode::Con { .. } | ResolvedNode::Fun { .. } => Ok(&[]),
        }
    }

    fn tuple_elem(&self, t: RTy, i: usize) -> Result<Option<RTy>, ForeignRTy> {
        Ok(self.tuple_elems(t)?.get(i).copied())
    }
}

fn pool() -> ResolvedPool {
    ResolvedPool::new(PrimIds {
        int: TypeId(1),
        float: TypeId(2),
        string: TypeId(3),
        array: TypeId(4),
    })
}

/// Core IR's `:{ty}` sigil must render as `:7`, not `:r7`.
#[test]
fn rty_prints_the_bare_pool_index() {
    assert_eq!(RTy(0).to_string(), "0");
    assert_eq!(RTy(42).to_string(), "42");
    assert_eq!(format!(":{}", RTy(7)), ":7");
}

#[test]
fn primitives_are_not_heap() {
    let mut p = pool();
    let int = p.mk_con(TypeId(1), Repr::Counted, &[]).unwrap();
    let float = p.mk_con(TypeId(2), Repr::Word, &[]).unwrap();
    let string = p.mk_con(TypeId(3), Repr::Counted, &[]).unwrap();
    assert_eq!(p.prim_of(int).unwrap(), Some(Prim::Int));
    assert_eq!(p.prim_of(float).unwrap(), Some(Prim::Float));
    assert_eq!(p.prim_of(string).unwrap(), Some(Prim::String));
    assert!(!p.is_heap(int).unwrap());
    assert!(!p.is_heap(float).unwrap());
    // Strings live on the heap at run time, but they aren't Perceus cells.
    assert!(!p.is_heap(string).unwrap());
}

#[test]
fn user_types_tuples_and_functions_are_heap() {
    let mut p = pool();
    let int = p.mk_con(TypeId(1), Repr::Counted, &[]).unwrap();
    let user = p.mk_con(TypeId(9), Repr::Counted, &[]).unwrap();
    let tup = p.mk_tuple(&[int, int]).unwrap();
    let fun = p.mk_fun(&[int], int).unwrap();
    assert!(p.is_heap(user).unwrap());
    assert!(p.is_heap(tup).unwrap());
    assert!(p.is_heap(fun).unwrap());
    assert_eq!(p.prim_of(user).unwrap(), None);
}

#[test]
fn a_bound_variable_is_polymorphic_not_missing() {
    let mut p = pool();
    let b = p.mk_bound(0).unwrap();
    assert!(!p.is_heap(b).unwrap());
    assert_eq!(p.prim_of(b).unwrap(), None);
    assert_eq!(p.node(b).unwrap(), ResolvedNode::Bound(0));
}

/// Every kind of type says whether its values can be cells. A named one says
/// what it was made with, whatever its arguments, and nothing else is ever a
/// word. A type from another pool has no answer.
#[test]
fn every_type_says_whether_its_values_can_be_cells() {
    let mut p = pool();
    let int = p.mk_con(TypeId(1), Repr::Counted, &[]).unwrap();
    let float = p.mk_con(TypeId(2), Repr::Word, &[]).unwrap();
    let bool_ = p.mk_con(TypeId(5), Repr::Word, &[]).unwrap();
    let any = p.mk_bound(0).unwrap();
    let tag_of_int = p.mk_con(TypeId(6), Repr::Word, &[int]).unwrap();
    let box_of_bool = p.mk_con(TypeId(7), Repr::Counted, &[bool_]).unwrap();
    let pair = p.mk_tuple(&[bool_, float]).unwrap();
    let unit = p.mk_tuple(&[]).unwrap();
    let test = p.mk_fun(&[bool_], bool_).unwrap();
    for word in [float, bool_, tag_of_int] {
        assert_eq!(p.repr(word), Ok(Repr::Word), "{word}");
    }
    for counted in [int, any, box_of_bool, pair, unit, test] {
        assert_eq!(p.repr(counted), Ok(Repr::Counted), "{counted}");
    }
    let foreign = RTy(9);
    assert_eq!(p.repr(foreign), Err(ForeignRTy(foreign)));
}

#[test]
fn children_slices_do_not_alias() {
    let mut p = pool();
    let int = p.mk_con(TypeId(1), Repr::Counted, &[]).unwrap();
    let str_ = p.mk_con(TypeId(3), Repr::Counted, &[]).unwrap();
    let a = p.mk_tuple(&[int, str_]).unwrap();
    let b = p.mk_tuple(&[str_, int]).unwrap();
    assert_eq!(p.tuple_elems(a).unwrap(), &[int, str_]);
    assert_eq!(p.tuple_elems(b).unwrap(), &[str_, int]);
    assert_eq!(p.tuple_elem(a, 1).unwrap(), Some(str_));
    assert_eq!(p.tuple_elem(a, 2).unwrap(), None);
    assert!(matches!(p.node(a).unwrap(), ResolvedNode::Tuple { elems } if elems.len == 2));
    assert!(!matches!(p.node(int).unwrap(), ResolvedNode::Tuple { .. }));
}

#[test]
fn con_args_and_fun_shape() {
    let mut p = pool();
    let int = p.mk_con(TypeId(1), Repr::Counted, &[]).unwrap();
    let nil = p.mk_con(TypeId(7), Repr::Counted, &[]).unwrap();
    let res = p.mk_con(TypeId(8), Repr::Counted, &[int, nil]).unwrap();
    assert_eq!(p.con_arg(res, 0).unwrap(), Some(int));
    assert_eq!(p.con_arg(res, 1).unwrap(), Some(nil));
    assert_eq!(p.con_arg(res, 2).unwrap(), None);
    let f = p.mk_fun(&[int, res], nil).unwrap();
    assert_eq!(p.fun_params(f).unwrap(), &[int, res]);
    assert_eq!(p.fun_ret(f).unwrap(), Some(nil));
    assert_eq!(p.fun_ret(int).unwrap(), None);
}
