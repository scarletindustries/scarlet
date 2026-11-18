//! Which way values flow through a type's parameters.
//!
//! A parameter is covariant when its values only come out of the type, like
//! the element of an `Array` or the result of a function. It's contravariant
//! when they only go in, like a function's argument, and invariant when they
//! do both, like a `Subject`'s message. The relaxed value restriction (Garrigue,
//! "Relaxing the Value Restriction", 2004) reads this: a binding a call made
//! may still be generic over a variable that only ever comes out of it.

use crate::name::TypeName;
use crate::ty::{Type, TypeVar};

/// How a parameter (or one variable, at one place in a type) is used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Variance {
    /// Not used at all.
    Unused,
    /// Its values only come out.
    Covariant,
    /// Its values only go in.
    Contravariant,
    /// Its values go both ways.
    Invariant,
}

impl Variance {
    /// Both uses at once: the least variance that allows each.
    pub fn join(self, other: Variance) -> Variance {
        match (self, other) {
            (Variance::Unused, v) | (v, Variance::Unused) => v,
            (Variance::Covariant, Variance::Covariant) => Variance::Covariant,
            (Variance::Contravariant, Variance::Contravariant) => Variance::Contravariant,
            (Variance::Invariant, _)
            | (_, Variance::Invariant)
            | (Variance::Covariant, Variance::Contravariant)
            | (Variance::Contravariant, Variance::Covariant) => Variance::Invariant,
        }
    }

    /// A use with variance `inner`, inside a position with variance `self`.
    /// Going in through something that only takes values in turns it around.
    pub(crate) fn then(self, inner: Variance) -> Variance {
        match (self, inner) {
            (Variance::Unused, _) | (_, Variance::Unused) => Variance::Unused,
            (Variance::Covariant, v) => v,
            (Variance::Contravariant, Variance::Covariant) => Variance::Contravariant,
            (Variance::Contravariant, Variance::Contravariant) => Variance::Covariant,
            (Variance::Contravariant, Variance::Invariant) | (Variance::Invariant, _) => {
                Variance::Invariant
            }
        }
    }
}

/// The variance of each parameter of the named types a check can reach.
pub trait Variances {
    /// The variance of `ty`'s parameter `param`. A type it doesn't know is
    /// taken as invariant, which never lets anything generalise wrongly.
    fn variance(&self, ty: &TypeName, param: usize) -> Variance;
}

/// How `var` is used in `ty`, with `ty` itself in a covariant position.
pub fn of_var(ty: &Type, var: &TypeVar, types: &impl Variances) -> Variance {
    at(ty, var, Variance::Covariant, types)
}

fn at(ty: &Type, var: &TypeVar, here: Variance, types: &impl Variances) -> Variance {
    match ty {
        Type::Var(v) => {
            if v == var {
                here
            } else {
                Variance::Unused
            }
        }
        Type::Named { name, args } => {
            args.iter()
                .enumerate()
                .fold(Variance::Unused, |acc, (i, a)| {
                    let inner = here.then(types.variance(name, i));
                    acc.join(at(a, var, inner, types))
                })
        }
        Type::Fn { params, ret } => params.iter().fold(at(ret, var, here, types), |acc, p| {
            acc.join(at(p, var, here.then(Variance::Contravariant), types))
        }),
        Type::Tuple(elems) => elems
            .iter()
            .fold(Variance::Unused, |acc, e| acc.join(at(e, var, here, types))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn going_in_twice_comes_back_out() {
        let contra = Variance::Contravariant;
        assert_eq!(contra.then(contra), Variance::Covariant);
    }

    #[test]
    fn out_and_in_together_lock_a_parameter() {
        assert_eq!(
            Variance::Covariant.join(Variance::Contravariant),
            Variance::Invariant
        );
        assert_eq!(
            Variance::Unused.join(Variance::Covariant),
            Variance::Covariant
        );
    }

    #[test]
    fn nothing_flows_through_an_unused_position() {
        assert_eq!(Variance::Invariant.then(Variance::Unused), Variance::Unused);
        assert_eq!(Variance::Unused.then(Variance::Invariant), Variance::Unused);
    }
}
