use super::*;

fn engine() -> Engine {
    Engine::new(Known::new())
}

#[test]
fn a_variable_unifies_with_a_concrete_type() {
    let mut e = engine();
    let v = e.fresh();
    let i = e.int();
    assert!(e.unify(v, i).is_ok());
    assert_eq!(e.zonk(v).unwrap().to_string(), "Int");
}

#[test]
fn a_numeric_variable_refuses_a_string() {
    let mut e = engine();
    let v = e.fresh_constrained(Constraint::Numeric);
    let s = e.string();
    assert!(e.unify(v, s).is_err());
}

#[test]
fn x_of_x_is_recursive() {
    let mut e = engine();
    let v = e.fresh();
    let f = e.fun(vec![v], v);
    assert!(matches!(e.unify(v, f), Err(UnifyError::Recursive)));
}

#[test]
fn a_failed_unify_changes_nothing() {
    let mut e = engine();
    let a = e.fresh();
    let i = e.int();
    let s = e.string();
    let left = e.fun(vec![a, i], i);
    let right = e.fun(vec![s, s], i);
    assert!(e.unify(left, right).is_err());
    assert!(
        matches!(e.shape(a), Ok(Shape::Var(_))),
        "`a` stayed joined to String"
    );
}

/// No named types, so the tests only meet functions and tuples.
struct NoTypes;

impl Variances for NoTypes {
    fn variance(&self, _: &TypeName, _: usize) -> Variance {
        Variance::Invariant
    }
}

#[test]
fn a_computed_binding_still_generalises_what_only_comes_out() {
    let mut e = engine();
    let level = e.enter();
    let a = e.fresh();
    let f = e.fun(vec![], a);
    let s = e.generalize(level, f, Binding::Computed, &NoTypes).unwrap();
    assert_eq!(s.vars.len(), 1);
}

#[test]
fn a_computed_binding_pins_what_goes_in() {
    let mut e = engine();
    let level = e.enter();
    let inner = e.fresh();
    let f = e.fun(vec![inner], inner);
    let s = e.generalize(level, f, Binding::Computed, &NoTypes).unwrap();
    assert!(s.vars.is_empty());
    // The variable came up to the outer level, so generalising there
    // can't quantify it either.
    let outer_level = e.enter();
    let s2 = e
        .generalize(outer_level, f, Binding::Value, &NoTypes)
        .unwrap();
    assert!(s2.vars.is_empty());
}

#[test]
fn generalising_quantifies_only_deeper_variables() {
    let mut e = engine();
    let outer = e.fresh();
    let level = e.enter();
    let inner = e.fresh();
    let f = e.fun(vec![inner], outer);
    let s = e.generalize(level, f, Binding::Value, &NoTypes).unwrap();
    assert_eq!(s.vars.len(), 1);
    let a = e.instantiate(&s).unwrap();
    let b = e.instantiate(&s).unwrap();
    let i = e.int();
    let st = e.string();
    let Ok((pa, _)) = e.callable(a, 1) else {
        panic!("an instance of a function scheme is callable")
    };
    let Ok((pb, _)) = e.callable(b, 1) else {
        panic!("an instance of a function scheme is callable")
    };
    assert!(e.unify(pa[0], i).is_ok());
    assert!(e.unify(pb[0], st).is_ok());
}

#[test]
fn a_rigid_variable_refuses_a_concrete_type() {
    let mut e = engine();
    let a = e.rigid("a".into());
    let i = e.int();
    assert!(e.unify(a, i).is_err());
}
