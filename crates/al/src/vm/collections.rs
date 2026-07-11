//! The aggregate-value opcodes: arrays (persistent vectors), tuples,
//! lazy integer ranges, and enum/record field access.
//!
//! Two ideas shape every method here:
//!
//! - **Arrays are persistent trees** ([`al_core::bytecode::seq`]):
//!   concat merges border nodes, slice/drop path-copy along the cut,
//!   push is a path copy — operands always stay valid, structurally
//!   shared values. There is no in-place fast path: seq operands must
//!   stay valid for structural sharing, so a mutate-in-place would need
//!   proof no alias exists across the seq API (`seq_root`).
//! - **Ranges stay lazy until they must not.** `s..e` is two words;
//!   index/len/slice/drop on it are O(1) arithmetic, and only the ops
//!   that need real elements (concat, push) materialize it into a tree
//!   — that materialization is the op's real cost.

use al_core::bytecode::{Value, ValueView, seq};

use super::{VM, VmError, VmResult, range_len, value_type_name};

impl VM {
    pub(super) fn make_array(&mut self, operand: i32) -> VmResult<()> {
        let len = operand as usize;
        let base = self.operand_base(len)?;
        let v = Value::array_in(&mut self.heap, &self.stack[base..]);
        self.stack.truncate(base);
        self.stack.push(v);
        Ok(())
    }

    pub(super) fn make_tuple(&mut self, operand: i32) -> VmResult<()> {
        let len = operand as usize;
        let base = self.operand_base(len)?;
        let v = Value::tuple_in(&mut self.heap, &self.stack[base..]);
        self.stack.truncate(base);
        self.stack.push(v);
        Ok(())
    }

    pub(super) fn tuple_index(&mut self, operand: i32) -> VmResult<()> {
        let tuple_val = self.pop()?;
        if let Some(t) = tuple_val.as_tuple() {
            let idx = operand;
            if idx >= 0 && (idx as usize) < t.len() {
                self.stack.push(t[idx as usize].clone());
                Ok(())
            } else {
                Err(VmError::IndexOutOfBounds {
                    idx: idx as i64,
                    len: t.len() as i64,
                    what: "tuple",
                })
            }
        } else {
            Err(VmError::type_mismatch("tuple.index", "Tuple", &tuple_val))
        }
    }

    pub(super) fn make_range(&mut self) -> VmResult<()> {
        let end_val = self.pop()?;
        let start_val = self.pop()?;

        if let (Some(start), Some(end)) = (start_val.as_int(), end_val.as_int()) {
            let v = Value::range_in(&mut self.heap, start, end);
            self.stack.push(v);
            Ok(())
        } else {
            Err(VmError::internal(format!(
                "range bounds must be integers, got '{}' and '{}'",
                value_type_name(&start_val),
                value_type_name(&end_val)
            )))
        }
    }

    /// The element of an Array/Range operand at `idx`, or `None` when the
    /// index is out of bounds, negative, or the operand is not a sequence.
    /// Callers decide what a miss means (`None` vs an internal error).
    #[inline]
    fn seq_elem(&mut self, v: &Value, idx: i64) -> Option<Value> {
        if idx < 0 {
            return None;
        }
        match v.kind() {
            ValueView::Array(arr) => arr.get(idx as usize),
            ValueView::Range(start, end) => {
                let elem = range_elem(start, end, idx)?;
                Some(self.boxed_int(elem))
            }
            _ => None,
        }
    }

    /// `arr[i]` — `Some(elem)` / `None`, never an error.
    pub(super) fn seq_index(&mut self) -> VmResult<()> {
        let idx_val = self.pop()?;
        let arr_val = self.pop()?;
        let elem = idx_val
            .as_int()
            .and_then(|idx| self.seq_elem(&arr_val, idx));
        let v = match elem {
            Some(elem) => self.make_some(elem),
            None => self.make_none(),
        };
        self.stack.push(v);
        Ok(())
    }

    /// `arr[idx] or default` fused: no `Option` box is built. The default is
    /// already on the stack (`lower` only fuses a pure atom), and is released
    /// by its `Value` drop when the index hits.
    pub(super) fn seq_index_or(&mut self, operand: i32) -> VmResult<()> {
        // `operand >= 0` is a `ConstId`; `-1` means `lower` pushed the default
        // because it was not a constant (`a[i] or False`, `a[i] or x`).
        let dflt = if operand < 0 {
            self.pop()?
        } else {
            self.program.constants[operand as usize].clone()
        };
        let idx_val = self.pop()?;
        let arr_val = self.pop()?;
        let elem = idx_val
            .as_int()
            .and_then(|idx| self.seq_elem(&arr_val, idx));
        self.stack.push(elem.unwrap_or(dflt));
        Ok(())
    }

    /// Compiler-proven in-bounds element fetch (pattern destructuring).
    pub(super) fn elem_at(&mut self, operand: i32) -> VmResult<()> {
        let arr_val = self.pop()?;
        let idx = operand as i64;
        match self.seq_elem(&arr_val, idx) {
            Some(v) => {
                self.stack.push(v);
                Ok(())
            }
            None => Err(elem_at_miss(&arr_val, idx)),
        }
    }

    pub(super) fn seq_len(&mut self) -> VmResult<()> {
        let arr_val = self.pop()?;
        match arr_val.kind() {
            ValueView::Array(a) => {
                let n = a.len() as i64;
                self.push_int(n);
            }
            // A range's length can exceed the small-int payload
            // (`len(i64::MIN..i64::MAX)`): `push_int` boxes it.
            ValueView::Range(s, e) => self.push_int(range_len(s, e)),
            ValueView::Tuple(t) => {
                let n = t.len() as i64;
                self.push_int(n);
            }
            _ => {
                return Err(VmError::type_mismatch("array.len", "Array", &arr_val));
            }
        }
        Ok(())
    }

    pub(super) fn seq_slice(&mut self) -> VmResult<()> {
        let end_val = self.pop()?;
        let start_val = self.pop()?;
        let arr_val = self.pop()?;

        let (Some(start), Some(end)) = (start_val.as_int(), end_val.as_int()) else {
            return Err(VmError::internal("slice indices must be integers"));
        };
        match arr_val.kind() {
            ValueView::Array(arr) => {
                check_slice_bounds(start, end, arr.len() as i64)?;
                // take + skip: two persistent splits.
                let prefix = seq::take(&mut self.heap, &arr_val, end as usize);
                let sliced = seq::skip(&mut self.heap, &prefix, start as usize);
                self.stack.push(sliced);
                Ok(())
            }
            ValueView::Range(rs, re) => {
                check_slice_bounds(start, end, range_len(rs, re))?;
                let v = Value::range_in(&mut self.heap, rs + start, rs + end);
                self.stack.push(v);
                Ok(())
            }
            _ => Err(VmError::type_mismatch("array.slice", "Array", &arr_val)),
        }
    }

    pub(super) fn seq_concat(&mut self) -> VmResult<()> {
        let arr2_val = self.pop()?;
        let arr1_val = self.pop()?;
        // Persistent concat: both operands stay structurally
        // shared (border-node merges only); Range/non-array
        // routing + error messages live in `seq_root`.
        let a = self.seq_root(arr1_val)?;
        let b = self.seq_root(arr2_val)?;
        let merged = seq::concat(&mut self.heap, &a, &b);
        self.stack.push(merged);
        Ok(())
    }

    pub(super) fn seq_prepend(&mut self, operand: i32) -> VmResult<()> {
        let k = operand as usize;
        let seq_val = self.pop()?;
        let mut root = self.seq_root(seq_val)?;
        // Stack below `seq` holds e0..e_{k-1} (e0 pushed first), so
        // popping yields e_{k-1}..e0; push_front each so the final
        // order is [e0, e1, .., e_{k-1}, ..seq].
        for _ in 0..k {
            let e = self.pop()?;
            root = seq::push_front(&mut self.heap, &root, e);
        }
        self.stack.push(root);
        Ok(())
    }

    pub(super) fn seq_drop(&mut self) -> VmResult<()> {
        let n_val = self.pop()?;
        let seq_val = self.pop()?;
        let Some(n) = n_val.as_int() else {
            return Err(VmError::type_mismatch("array.drop", "Int", &n_val));
        };
        let n = n.max(0);
        // Dropping n from a lazy `s..e` Range is the slice [n, len)
        // = `s+n .. e`; keep it O(1) instead of materializing via
        // `seq_root`. Array/non-array routing matches the producer
        // side (identical error text).
        if let Some((s, e)) = seq_val.as_range() {
            let len = range_len(s, e);
            let n = n.min(len);
            let v = Value::range_in(&mut self.heap, s + n, e);
            self.stack.push(v);
            return Ok(());
        }
        let root = self.seq_root(seq_val)?;
        let n = (n as usize).min(seq::len(&root));
        // One front split (path copies along the cut), not n pops.
        let v = seq::skip(&mut self.heap, &root, n);
        self.stack.push(v);
        Ok(())
    }

    pub(super) fn seq_append(&mut self, operand: i32) -> VmResult<()> {
        let m = operand as usize;
        // The sequence word sits just below the m pushed elements; read the
        // elements in place and truncate once at the end.
        let base = self.operand_base(m + 1)?;
        let seq_val = self.stack[base].clone();
        let mut root = self.seq_root(seq_val)?;
        for i in base + 1..base + 1 + m {
            root = seq::push_back(&mut self.heap, &root, self.stack[i].clone());
        }
        self.stack.truncate(base);
        self.stack.push(root);
        Ok(())
    }

    pub(super) fn get_field(&mut self, operand: i32) -> VmResult<()> {
        let val = self.pop()?;
        let idx = operand;
        if let Some(ev) = val.as_enum() {
            let payload = ev.payload();
            if idx >= 0 && (idx as usize) < payload.len() {
                self.stack.push(payload[idx as usize].clone());
                Ok(())
            } else {
                Err(VmError::internal(format!(
                    "field index {idx} out of bounds on {}.{} (len {})",
                    ev.enum_name(),
                    ev.variant_name(),
                    payload.len()
                )))
            }
        } else {
            Err(VmError::type_mismatch("field access", "record", &val))
        }
    }

    /// The sequence root of an Array operand; a Range is materialized into
    /// a fresh tree (its elements never existed in memory before, so this
    /// is the operation's real cost — budgeted by the caller). Non-sequences
    /// keep the producer's error message.
    ///
    /// An Array operand comes back as-is (persistent trees share, never
    /// clone); every subsequent write the caller performs is a persistent
    /// path-copy update that never mutates shared structure. There is no
    /// in-place fast path: seq operands must stay valid for structural
    /// sharing, so a mutate-in-place would need proof no alias exists
    /// across the seq API.
    fn seq_root(&mut self, v: Value) -> VmResult<Value> {
        match v.kind() {
            ValueView::Array(_) => Ok(v),
            ValueView::Range(s, e) => Ok(seq::from_int_range(&mut self.heap, s, e)),
            _ => Err(VmError::internal(format!(
                "expected sequence, got '{}'",
                value_type_name(&v)
            ))),
        }
    }
}

#[inline]
fn range_elem(start: i64, end: i64, idx: i64) -> Option<i64> {
    start.checked_add(idx).filter(|e| *e < end)
}

#[inline]
fn check_slice_bounds(start: i64, end: i64, len: i64) -> VmResult<()> {
    if start >= 0 && end <= len && start <= end {
        Ok(())
    } else {
        Err(VmError::SliceOutOfBounds {
            lo: start,
            hi: end,
            len,
        })
    }
}

/// Why `seq_elem` missed — only recomputes the length once the fetch has
/// already failed, so the in-bounds path never pays for it.
#[cold]
#[inline(never)]
fn elem_at_miss(v: &Value, idx: i64) -> VmError {
    match v.kind() {
        ValueView::Array(arr) => VmError::internal(format!(
            "elem_at: array index {idx} out of bounds (len {})",
            arr.len()
        )),
        ValueView::Range(start, end) => VmError::internal(format!(
            "elem_at: range index {idx} out of bounds (len {})",
            range_len(start, end)
        )),
        _ => VmError::type_mismatch("elem_at", "Array", v),
    }
}
