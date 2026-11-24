//! The operators on numbers, as the loader, the instructions and the two
//! kinds of arithmetic all name them.

use std::cmp::Ordering;

/// An operator that gives a number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Arith {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
}

impl Arith {
    /// Whether it gives the same answer with its operands swapped, for two
    /// Ints and for two Floats, the sign of a zero included.
    pub(crate) fn commutes(self) -> bool {
        match self {
            Arith::Add | Arith::Mul => true,
            Arith::Sub | Arith::Div | Arith::Rem => false,
        }
    }
}

/// An operator that asks which of two numbers is the larger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Order {
    Lt,
    Le,
    Gt,
    Ge,
}

impl Order {
    /// Whether it holds of two numbers that stand in `order`.
    #[inline(always)]
    pub(crate) fn holds(self, order: Ordering) -> bool {
        match self {
            Order::Lt => order.is_lt(),
            Order::Le => order.is_le(),
            Order::Gt => order.is_gt(),
            Order::Ge => order.is_ge(),
        }
    }

    /// The operator that gives the same answer with its operands swapped.
    pub(crate) fn swapped(self) -> Order {
        match self {
            Order::Lt => Order::Gt,
            Order::Le => Order::Ge,
            Order::Gt => Order::Lt,
            Order::Ge => Order::Le,
        }
    }
}

/// An operator on two numbers of one kind, for an instruction that finds out
/// which kind when it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NumOp {
    Arith(Arith),
    Order(Order),
}

/// An operator on two Ints that gives a Bool. `==` on Floats is the one that
/// works on any two values (`eq.rs`), because the two zeros are equal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Compare {
    Eq,
    Ne,
    Order(Order),
}

impl Compare {
    pub(crate) const LT: Compare = Compare::Order(Order::Lt);
    pub(crate) const LE: Compare = Compare::Order(Order::Le);
    pub(crate) const GT: Compare = Compare::Order(Order::Gt);
    pub(crate) const GE: Compare = Compare::Order(Order::Ge);

    /// Whether it holds of two Ints that stand in `order`.
    #[inline(always)]
    pub(crate) fn holds(self, order: Ordering) -> bool {
        match self {
            Compare::Eq => order.is_eq(),
            Compare::Ne => order.is_ne(),
            Compare::Order(op) => op.holds(order),
        }
    }

    /// The operator that gives the same answer with its operands swapped.
    pub(crate) fn swapped(self) -> Compare {
        match self {
            Compare::Eq => Compare::Eq,
            Compare::Ne => Compare::Ne,
            Compare::Order(op) => Compare::Order(op.swapped()),
        }
    }
}
