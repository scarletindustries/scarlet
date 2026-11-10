//! Finished types. A [`Type`] holds no inference state, so it means the same
//! thing in every module, and an interface, a typed tree or an error message
//! can carry one without the engine that made it.

use std::fmt;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use crate::name::{Name, TypeName};

/// A constraint on a type variable, as in Elm: the variable can only become
/// one of a fixed set of primitive types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Constraint {
    /// `Int`, `Float` or `String`: what `+` takes.
    Addable,
    /// `Int` or `Float`: what the other arithmetic operators take.
    Numeric,
}

impl Constraint {
    fn name(self) -> &'static str {
        match self {
            Constraint::Addable => "addable",
            Constraint::Numeric => "numeric",
        }
    }

    /// Whether a primitive meets this constraint.
    pub(crate) fn allows(self, prim: Prim) -> bool {
        match (self, prim) {
            (Constraint::Addable, Prim::Int | Prim::Float | Prim::String) => true,
            (Constraint::Numeric, Prim::Int | Prim::Float) => true,
            (Constraint::Numeric, Prim::String) => false,
        }
    }

    /// The constraint that allows exactly the types both allow. Every pair
    /// shares at least `Int`, so this never fails.
    pub(crate) fn intersect(self, other: Constraint) -> Constraint {
        match (self, other) {
            (Constraint::Addable, Constraint::Addable) => Constraint::Addable,
            (Constraint::Numeric, _) | (_, Constraint::Numeric) => Constraint::Numeric,
        }
    }

    /// Whether every type `self` allows is one `required` allows too.
    pub(crate) fn implies(self, required: Constraint) -> bool {
        [Prim::Int, Prim::Float, Prim::String]
            .into_iter()
            .all(|p| !self.allows(p) || required.allows(p))
    }
}

pub use scarlet_ir::rty::Prim;

/// A type variable that inference left open. Within one module check, `id`
/// is unique. In an interface, a scheme's variables are numbered from 0.
///
/// Two variables are the same variable when their ids are equal. The name and
/// constraint go along for display and instantiation. A variable has a name
/// only when the source gave it one, like `a` in `fn id(x a) a`; any other
/// is lettered when a type holding it is shown.
#[derive(Debug, Clone)]
pub struct TypeVar {
    pub id: u32,
    pub name: Option<Name>,
    pub constraint: Option<Constraint>,
}

impl PartialEq for TypeVar {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for TypeVar {}

impl Hash for TypeVar {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Type {
    /// `Name(args...)`. Primitives are named types too: `Int` is
    /// `scarlet.Int`. An alias never appears here, because annotations expand
    /// it.
    Named {
        name: TypeName,
        args: Rc<[Type]>,
    },
    /// `fn(params...) ret`.
    Fn {
        params: Rc<[Type]>,
        ret: Rc<Type>,
    },
    /// `(elems...)`.
    Tuple(Rc<[Type]>),
    Var(TypeVar),
}

impl Type {
    pub fn named(name: TypeName, args: Vec<Type>) -> Type {
        Type::Named {
            name,
            args: args.into(),
        }
    }

    pub fn function(params: Vec<Type>, ret: Type) -> Type {
        Type::Fn {
            params: params.into(),
            ret: Rc::new(ret),
        }
    }

    pub fn tuple(elems: Vec<Type>) -> Type {
        Type::Tuple(elems.into())
    }

    /// The named type this is, with its arguments.
    pub fn as_named(&self) -> Option<(&TypeName, &[Type])> {
        match self {
            Type::Named { name, args } => Some((name, args)),
            Type::Fn { .. } | Type::Tuple(_) | Type::Var(_) => None,
        }
    }

    /// Replace each variable `f` gives a type for. Shared subtrees that hold
    /// no such variable are kept as they are.
    pub(crate) fn substitute(&self, f: &impl Fn(&TypeVar) -> Option<Type>) -> Type {
        match self {
            Type::Var(v) => f(v).unwrap_or_else(|| self.clone()),
            Type::Named { name, args } => Type::Named {
                name: name.clone(),
                args: args.iter().map(|a| a.substitute(f)).collect(),
            },
            Type::Fn { params, ret } => Type::Fn {
                params: params.iter().map(|p| p.substitute(f)).collect(),
                ret: Rc::new(ret.substitute(f)),
            },
            Type::Tuple(elems) => Type::Tuple(elems.iter().map(|e| e.substitute(f)).collect()),
        }
    }

    /// Every variable in the type, each once, in the order they first appear.
    pub fn vars(&self) -> Vec<TypeVar> {
        let mut out: Vec<TypeVar> = Vec::new();
        self.walk_vars(&mut |v| {
            if !out.contains(v) {
                out.push(v.clone());
            }
        });
        out
    }

    fn walk_vars(&self, f: &mut impl FnMut(&TypeVar)) {
        match self {
            Type::Var(v) => f(v),
            Type::Named { args, .. } => args.iter().for_each(|a| a.walk_vars(f)),
            Type::Fn { params, ret } => {
                params.iter().for_each(|p| p.walk_vars(f));
                ret.walk_vars(f);
            }
            Type::Tuple(elems) => elems.iter().for_each(|e| e.walk_vars(f)),
        }
    }
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names = VarNames::of(&[self]);
        names.write(self, f)
    }
}

/// Show `types` side by side, as in "expected 'a', got 'fn(a) b'": a variable
/// that appears in more than one of them gets the same letter in each.
pub(crate) fn show_together(types: &[&Type]) -> Vec<String> {
    let names = VarNames::of(types);
    types
        .iter()
        .map(|t| {
            let mut out = String::new();
            let _ = fmt::write(&mut out, format_args!("{}", Shown(t, &names)));
            out
        })
        .collect()
}

/// What each variable in some types is called when they are shown: its own
/// name when it has one, otherwise the next letter no named variable uses,
/// in the order the variables first appear.
struct VarNames(Vec<(u32, Name)>);

impl VarNames {
    fn of(types: &[&Type]) -> Self {
        let mut vars: Vec<TypeVar> = Vec::new();
        for t in types {
            for v in t.vars() {
                if !vars.contains(&v) {
                    vars.push(v);
                }
            }
        }
        let taken: Vec<&Name> = vars.iter().filter_map(|v| v.name.as_ref()).collect();
        let mut next = 0u32;
        let mut out = Vec::with_capacity(vars.len());
        for v in &vars {
            let name = match &v.name {
                Some(n) => n.clone(),
                None => loop {
                    let candidate: Name = letters(next).into();
                    next += 1;
                    if !taken.contains(&&candidate) {
                        break candidate;
                    }
                },
            };
            out.push((v.id, name));
        }
        VarNames(out)
    }

    fn get(&self, v: &TypeVar) -> &str {
        self.0
            .iter()
            .find(|(id, _)| *id == v.id)
            .map_or("_", |(_, n)| n)
    }

    fn write(&self, t: &Type, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let list = |f: &mut fmt::Formatter<'_>, items: &[Type]| -> fmt::Result {
            for (i, x) in items.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                self.write(x, f)?;
            }
            Ok(())
        };
        match t {
            Type::Named { name, args } => {
                f.write_str(name.name())?;
                if !args.is_empty() {
                    f.write_str("(")?;
                    list(f, args)?;
                    f.write_str(")")?;
                }
                Ok(())
            }
            Type::Fn { params, ret } => {
                f.write_str("fn(")?;
                list(f, params)?;
                f.write_str(") ")?;
                self.write(ret, f)
            }
            Type::Tuple(elems) => {
                f.write_str("(")?;
                list(f, elems)?;
                f.write_str(")")
            }
            Type::Var(v) => match v.constraint {
                Some(c) => f.write_str(c.name()),
                None => f.write_str(self.get(v)),
            },
        }
    }
}

struct Shown<'a>(&'a Type, &'a VarNames);

impl fmt::Display for Shown<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.1.write(self.0, f)
    }
}

/// The base-26 letters for the `n`th variable: `a` to `z`, then `aa`.
fn letters(mut n: u32) -> String {
    let mut out = Vec::new();
    loop {
        out.push(b'a' + (n % 26) as u8);
        if n < 26 {
            break;
        }
        n = n / 26 - 1;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

/// A possibly generic type: each use of a value with this scheme may pick
/// its own type for each of `vars`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Scheme {
    pub vars: Vec<TypeVar>,
    pub ty: Type,
}

impl Scheme {
    /// The same scheme with its variables numbered from 0 in the order they
    /// are listed, so two equal schemes from different checks compare equal.
    pub fn canonical(&self) -> Scheme {
        let renumbered: Vec<TypeVar> = self
            .vars
            .iter()
            .enumerate()
            .map(|(i, v)| TypeVar {
                id: i as u32,
                name: v.name.clone(),
                constraint: v.constraint,
            })
            .collect();
        let ty = self.ty.substitute(&|v| {
            self.vars
                .iter()
                .position(|q| q == v)
                .map(|i| Type::Var(renumbered[i].clone()))
        });
        Scheme {
            vars: renumbered,
            ty,
        }
    }
}

impl fmt::Display for Scheme {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.ty.fmt(f)
    }
}
