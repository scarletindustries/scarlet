//! The `Float` opcodes: arithmetic, ordering, and the Int conversions.
//!
//! These are the typed forms, emitted only where unification proved both
//! operands concrete, so `as_float_typed` reads the word directly rather than
//! testing the tag.
//!
//! Every float-producing op goes through [`Value::float`], which folds a
//! non-finite result to `0.0`. That is not cosmetic: a real NaN's bits collide
//! with the NaN-box tag space, so an unnormalized result would be read back as
//! a heap pointer. Div is total the same way the Int ops are — `x / 0.0` is
//! `0.0`, not an infinity.
//!
//! The Int conversions saturate like Rust's `as` cast (NaN to 0), which is
//! what the interpreter has always done.
//!
//! Every op here is a pure stack transformation: nothing parks or touches
//! `ip`.

use crate::bytecode::Value;

use super::inspect::f64_str;
use super::{VM, VmResult};

/// The two operands of a binary op: `lhs` was pushed first, so it is the
/// deeper of the two.
struct Operands {
    lhs: f64,
    rhs: f64,
}

impl VM {
    /// Pop two proven-Float operands.
    #[inline]
    fn pop_float_pair(&mut self) -> VmResult<Operands> {
        let rhs = self.pop()?.as_float_typed();
        let lhs = self.pop()?.as_float_typed();
        Ok(Operands { lhs, rhs })
    }

    pub(super) fn add_float(&mut self) -> VmResult<()> {
        let Operands { lhs: a, rhs: b } = self.pop_float_pair()?;
        self.stack.push(Value::float(a + b));
        Ok(())
    }

    pub(super) fn sub_float(&mut self) -> VmResult<()> {
        let Operands { lhs: a, rhs: b } = self.pop_float_pair()?;
        self.stack.push(Value::float(a - b));
        Ok(())
    }

    pub(super) fn mul_float(&mut self) -> VmResult<()> {
        let Operands { lhs: a, rhs: b } = self.pop_float_pair()?;
        self.stack.push(Value::float(a * b));
        Ok(())
    }

    /// Total like `Op::DivInt`: a zero divisor yields `0.0` rather than an
    /// infinity the value encoding cannot hold.
    pub(super) fn div_float(&mut self) -> VmResult<()> {
        let Operands { lhs: a, rhs: b } = self.pop_float_pair()?;
        let r = if b == 0.0 { 0.0 } else { a / b };
        self.stack.push(Value::float(r));
        Ok(())
    }

    pub(super) fn neg_float(&mut self) -> VmResult<()> {
        let a = self.pop()?.as_float_typed();
        self.stack.push(Value::float(-a));
        Ok(())
    }

    pub(super) fn lt_float(&mut self) -> VmResult<()> {
        let Operands { lhs: a, rhs: b } = self.pop_float_pair()?;
        self.stack.push(Value::bool(a < b));
        Ok(())
    }

    pub(super) fn gt_float(&mut self) -> VmResult<()> {
        let Operands { lhs: a, rhs: b } = self.pop_float_pair()?;
        self.stack.push(Value::bool(a > b));
        Ok(())
    }

    pub(super) fn lte_float(&mut self) -> VmResult<()> {
        let Operands { lhs: a, rhs: b } = self.pop_float_pair()?;
        self.stack.push(Value::bool(a <= b));
        Ok(())
    }

    pub(super) fn gte_float(&mut self) -> VmResult<()> {
        let Operands { lhs: a, rhs: b } = self.pop_float_pair()?;
        self.stack.push(Value::bool(a >= b));
        Ok(())
    }

    pub(super) fn float_floor(&mut self) -> VmResult<()> {
        let f = self.pop_float("float.floor")?;
        self.push_int(f.floor() as i64);
        Ok(())
    }

    pub(super) fn float_ceil(&mut self) -> VmResult<()> {
        let f = self.pop_float("float.ceil")?;
        self.push_int(f.ceil() as i64);
        Ok(())
    }

    pub(super) fn float_round(&mut self) -> VmResult<()> {
        let f = self.pop_float("float.round")?;
        self.push_int(f.round() as i64);
        Ok(())
    }

    pub(super) fn float_truncate(&mut self) -> VmResult<()> {
        let f = self.pop_float("float.truncate")?;
        self.push_int(f.trunc() as i64);
        Ok(())
    }

    pub(super) fn float_from_int(&mut self) -> VmResult<()> {
        let n = self.pop_int("float.from_int")?;
        self.stack.push(Value::float(n as f64));
        Ok(())
    }

    pub(super) fn float_to_string(&mut self) -> VmResult<()> {
        let f = self.pop_float("float.to_string")?;
        let s = f64_str(f);
        let v = Value::str_in(&mut self.heap, &s);
        self.stack.push(v);
        Ok(())
    }
}
