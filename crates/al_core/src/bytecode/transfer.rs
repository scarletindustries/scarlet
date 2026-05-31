//! Deep-copy conversion between the `Rc`-backed runtime representation and an
//! owned, `Send` mirror.
//!
//! `Value` and `Program` are `Rc`-backed (non-atomic refcounts), so they can
//! never cross an OS-thread boundary — not even read-only. `scheduler.spawn`
//! therefore ships work to another thread by deep-copying it into the `Send*`
//! types here, and the receiving thread rebuilds a fresh `Rc` heap of its own
//! from that copy. Values are immutable, so the copy is observationally
//! identical to the original.
//!
//! Socket handles are the one exception to "copy": the `SendValue` carries the
//! id, but the underlying OS resource is owned by the VM's side tables and must
//! be transferred separately — `value_out` records every socket id it
//! encounters so the caller can do that.
//!
//! Binary buffers are the other exception: their bytes live behind an
//! `Arc<[u8]>` (atomic, unlike the `Rc` everything else uses), so a binary
//! crosses the boundary as a single refcount bump — `value_out` clones the
//! `Arc` and `value_in` clones it back, with no byte copy on either side.

use std::rc::Rc;
use std::sync::Arc;

use super::{
    ClosureValue, EnumValue, Function, HeapValue, Instruction, Program, SocketValue, Value,
    ValueView, enum_hash_with_payload, enum_name_prefix_hash,
};

/// Owned, `Send` mirror of [`Value`].
#[derive(Debug, Clone)]
pub enum SendValue {
    Int(i64),
    Float(f64),
    Bool(bool),
    Nil,
    Str(String),
    Array(Vec<SendValue>),
    Range(i64, i64),
    Binary {
        backing: Arc<[u8]>,
        bit_offset: u64,
        bit_len: u64,
    },
    Tuple(Vec<SendValue>),
    Closure {
        func_idx: i32,
        captures: Vec<SendValue>,
        name: String,
    },
    Enum {
        type_id: i32,
        enum_name: String,
        variant_name: String,
        field_labels: Vec<String>,
        payload: Vec<SendValue>,
    },
    Socket {
        id: i32,
        is_listener: bool,
    },
}

/// Owned, `Send` mirror of [`Function`].
#[derive(Debug, Clone)]
pub struct SendFunction {
    pub name: String,
    pub arity: i32,
    pub locals: i32,
    pub capture_count: i32,
    pub code_start: i32,
    pub code_len: i32,
}

/// Owned, `Send` mirror of [`Program`]. A spawned thread rebuilds its own
/// `Program` from this, so closure `func_idx`s stay index-compatible.
#[derive(Debug, Clone)]
pub struct SendProgram {
    pub constants: Vec<SendValue>,
    pub functions: Vec<SendFunction>,
    pub code: Vec<Instruction>,
    pub entry: i32,
}

// The whole point of these types: they must stay `Send`.
const _: () = {
    const fn assert_send<T: Send>() {}
    assert_send::<SendValue>();
    assert_send::<SendProgram>();
};

/// Deep-copy a value into its `Send` form. Every socket id encountered is
/// pushed onto `socket_ids` so the caller can transfer the underlying OS
/// resources to the receiving thread.
pub fn value_out(v: &Value, socket_ids: &mut Vec<i32>) -> SendValue {
    match v.kind() {
        ValueView::Int(i) => SendValue::Int(i),
        ValueView::Float(f) => SendValue::Float(f),
        ValueView::Bool(b) => SendValue::Bool(b),
        ValueView::Nil => SendValue::Nil,
        ValueView::Heap(h) => match h {
            HeapValue::Str(s) => SendValue::Str(s.to_string()),
            HeapValue::Array(seq) => {
                SendValue::Array(seq.iter().map(|e| value_out(e, socket_ids)).collect())
            }
            HeapValue::Range(s, e) => SendValue::Range(*s, *e),
            // Share the backing buffer across the boundary by bumping the
            // `Arc` — no byte copy. The view's `bit_offset`/`bit_len` ride along
            // so a sub-slice keeps pointing at the same bits on the far side.
            HeapValue::Binary(b) => SendValue::Binary {
                backing: Arc::clone(&b.backing),
                bit_offset: b.bit_offset,
                bit_len: b.bit_len,
            },
            HeapValue::Tuple(t) => SendValue::Tuple(
                t.elements
                    .iter()
                    .map(|e| value_out(e, socket_ids))
                    .collect(),
            ),
            HeapValue::Closure(c) => SendValue::Closure {
                func_idx: c.func_idx,
                captures: c
                    .captures
                    .iter()
                    .map(|e| value_out(e, socket_ids))
                    .collect(),
                name: c.name.to_string(),
            },
            HeapValue::Enum(e) => SendValue::Enum {
                type_id: e.type_id,
                enum_name: e.enum_name.to_string(),
                variant_name: e.variant_name.to_string(),
                field_labels: e.field_labels.iter().map(|l| l.to_string()).collect(),
                payload: e.payload.iter().map(|p| value_out(p, socket_ids)).collect(),
            },
            HeapValue::Socket(s) => {
                socket_ids.push(s.id);
                SendValue::Socket {
                    id: s.id,
                    is_listener: s.is_listener,
                }
            }
            // `kind()` folds BigInt into ValueView::Int, so this arm is never
            // reached; kept so the match stays exhaustive over HeapValue.
            HeapValue::BigInt(i) => SendValue::Int(*i),
        },
    }
}

/// Rebuild a value from its `Send` form on the current thread (fresh `Rc`s).
pub fn value_in(sv: &SendValue) -> Value {
    match sv {
        SendValue::Int(i) => Value::int(*i),
        SendValue::Float(f) => Value::float(*f),
        SendValue::Bool(b) => Value::bool(*b),
        SendValue::Nil => Value::nil(),
        SendValue::Str(s) => Value::str(s.as_str()),
        SendValue::Array(items) => Value::array(items.iter().map(value_in).collect()),
        SendValue::Range(s, e) => Value::range(*s, *e),
        SendValue::Binary {
            backing,
            bit_offset,
            bit_len,
        } => {
            // Clone the shared backing back with no byte copy, preserving the
            // sub-view offset so a transferred slice stays the same bits.
            Value::binary_view(Arc::clone(backing), *bit_offset, *bit_len)
        }
        SendValue::Tuple(items) => Value::tuple(items.iter().map(value_in).collect()),
        SendValue::Closure {
            func_idx,
            captures,
            name,
        } => Value::closure(ClosureValue {
            func_idx: *func_idx,
            captures: captures.iter().map(value_in).collect::<Vec<_>>().into(),
            name: Rc::from(name.as_str()),
        }),
        SendValue::Enum {
            type_id,
            enum_name,
            variant_name,
            field_labels,
            payload,
        } => {
            let payload: Vec<Value> = payload.iter().map(value_in).collect();
            // Hashes are not transferred; recompute exactly the way the VM
            // computes them at construction so equality keeps working.
            let hash =
                enum_hash_with_payload(enum_name_prefix_hash(enum_name, variant_name), &payload);
            Value::enum_(EnumValue {
                type_id: *type_id,
                enum_name: Rc::from(enum_name.as_str()),
                variant_name: Rc::from(variant_name.as_str()),
                field_labels: field_labels.iter().map(|l| Rc::from(l.as_str())).collect(),
                payload,
                hash,
            })
        }
        SendValue::Socket { id, is_listener } => Value::socket(SocketValue {
            id: *id,
            is_listener: *is_listener,
        }),
    }
}

/// Deep-copy a program into its `Send` form.
pub fn program_out(p: &Program) -> SendProgram {
    // Constants can't reference host resources, but `value_out` requires the
    // sink either way.
    let mut ignored = Vec::new();
    SendProgram {
        constants: p
            .constants
            .iter()
            .map(|c| value_out(c, &mut ignored))
            .collect(),
        functions: p
            .functions
            .iter()
            .map(|f| SendFunction {
                name: f.name.to_string(),
                arity: f.arity,
                locals: f.locals,
                capture_count: f.capture_count,
                code_start: f.code_start,
                code_len: f.code_len,
            })
            .collect(),
        code: p.code.clone(),
        entry: p.entry,
    }
}

/// Rebuild a program from its `Send` form on the current thread.
pub fn program_in(sp: &SendProgram) -> Program {
    Program {
        constants: sp.constants.iter().map(value_in).collect(),
        functions: sp
            .functions
            .iter()
            .map(|f| Function {
                name: Rc::from(f.name.as_str()),
                arity: f.arity,
                locals: f.locals,
                capture_count: f.capture_count,
                code_start: f.code_start,
                code_len: f.code_len,
            })
            .collect(),
        code: sp.code.clone(),
        entry: sp.entry,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(v: &Value) -> Value {
        let mut ids = Vec::new();
        value_in(&value_out(v, &mut ids))
    }

    #[test]
    fn roundtrip_scalars() {
        assert_eq!(roundtrip(&Value::int(42)).as_int(), Some(42));
        assert_eq!(roundtrip(&Value::int(i64::MAX)).as_int(), Some(i64::MAX));
        assert_eq!(roundtrip(&Value::float(1.5)).as_float(), Some(1.5));
        assert_eq!(roundtrip(&Value::bool(true)).as_bool(), Some(true));
        assert_eq!(
            roundtrip(&Value::str("hello"))
                .as_str()
                .map(|s| s.to_string()),
            Some("hello".to_string())
        );
    }

    #[test]
    fn roundtrip_array_is_deep() {
        let original = Value::array(vec![Value::int(1), Value::str("two"), Value::int(3)]);
        let copy = roundtrip(&original);
        let (a, b) = (original.as_array().unwrap(), copy.as_array().unwrap());
        assert_eq!(a.len(), b.len());
        // Deep copy: no shared Rc between original and copy.
        assert!(!Rc::ptr_eq(a, b));
        assert_eq!(
            b.get(1).and_then(|v| v.as_str()).map(|s| s.to_string()),
            Some("two".to_string())
        );
    }

    fn binary_parts(v: &Value) -> (Arc<[u8]>, u64) {
        match v.as_heap() {
            Some(HeapValue::Binary(b)) => (Arc::clone(&b.backing), b.bit_len),
            _ => panic!("expected a binary value"),
        }
    }

    #[test]
    fn roundtrip_binary_shares_backing_no_copy() {
        let original = Value::binary(vec![1, 2, 3, 4, 5]);
        let copy = roundtrip(&original);
        let (orig_backing, orig_bits) = binary_parts(&original);
        let (copy_backing, copy_bits) = binary_parts(&copy);
        // Logical value is preserved.
        assert_eq!(&*copy_backing, &*orig_backing);
        assert_eq!(copy_bits, orig_bits);
        // And the bytes were NOT deep-copied: the round-tripped value shares
        // the very same `Arc` allocation as the original.
        assert!(Arc::ptr_eq(&orig_backing, &copy_backing));
    }

    #[test]
    fn roundtrip_binary_preserves_bit_len() {
        // A bit-string that is not byte-aligned (5 significant bits).
        let original = Value::binary_bits(vec![0b1010_1000], 5);
        let copy = roundtrip(&original);
        let (orig_backing, orig_bits) = binary_parts(&original);
        let (copy_backing, copy_bits) = binary_parts(&copy);
        assert_eq!(&*copy_backing, &*orig_backing);
        assert_eq!(orig_bits, 5);
        assert_eq!(copy_bits, 5);
        assert!(Arc::ptr_eq(&orig_backing, &copy_backing));
    }

    #[test]
    fn roundtrip_binary_subview_preserves_offset_no_copy() {
        // An O(1) sub-view (the middle two bytes of the backing) carries a
        // non-zero bit_offset. The boundary must ship the offset+len alongside
        // the shared backing so the slice still points at the same bits.
        let backing: Arc<[u8]> = Arc::from(vec![0xDE, 0xAD, 0xBE, 0xEF]);
        let original = Value::binary_view(Arc::clone(&backing), 8, 16);
        let copy = roundtrip(&original);
        match (original.as_heap(), copy.as_heap()) {
            (Some(HeapValue::Binary(o)), Some(HeapValue::Binary(c))) => {
                assert_eq!(c.bit_offset, 8);
                assert_eq!(c.bit_len, 16);
                // Same shared allocation on both sides — no byte copy.
                assert!(Arc::ptr_eq(&o.backing, &c.backing));
                assert!(Arc::ptr_eq(&backing, &c.backing));
                // Logical window survives the trip (0xAD, 0xBE).
                assert_eq!(c.byte_window(), &[0xAD, 0xBE]);
                // And the view compares logically equal to the original.
                assert_eq!(c, o);
            }
            _ => panic!("expected two binary values"),
        }
    }

    #[test]
    fn roundtrip_closure_preserves_shape() {
        let original = Value::closure(ClosureValue {
            func_idx: 7,
            captures: vec![Value::int(1), Value::str("cap")].into(),
            name: Rc::from("worker"),
        });
        let copy = roundtrip(&original);
        let cl = copy.as_closure().unwrap();
        assert_eq!(cl.func_idx, 7);
        assert_eq!(cl.captures.len(), 2);
        assert_eq!(&*cl.name, "worker");
    }

    #[test]
    fn roundtrip_enum_recomputes_equal_hash() {
        let payload = vec![Value::int(5)];
        let hash = enum_hash_with_payload(enum_name_prefix_hash("Option", "Some"), &payload);
        let original = Value::enum_(EnumValue {
            type_id: 3,
            enum_name: "Option".into(),
            variant_name: "Some".into(),
            field_labels: Rc::from(Vec::<Rc<str>>::new()),
            payload,
            hash,
        });
        let copy = roundtrip(&original);
        let e = copy.as_enum().unwrap();
        assert_eq!(e.hash, hash, "recomputed hash must match the original");
        assert_eq!(e.type_id, 3);
        assert_eq!(&*e.variant_name, "Some");
    }

    #[test]
    fn socket_ids_are_collected() {
        let sock = Value::socket(SocketValue {
            id: 9,
            is_listener: false,
        });
        let captured = Value::closure(ClosureValue {
            func_idx: 0,
            captures: vec![sock].into(),
            name: Rc::from("handler"),
        });
        let mut ids = Vec::new();
        value_out(&captured, &mut ids);
        assert_eq!(ids, vec![9]);
    }

    #[test]
    fn program_roundtrip() {
        use super::super::{Op, op_arg};
        let p = Program {
            constants: vec![Value::str("const"), Value::int(1)],
            functions: vec![Function {
                name: Rc::from("__main__"),
                arity: 0,
                locals: 2,
                capture_count: 0,
                code_start: 0,
                code_len: 1,
            }],
            code: vec![op_arg(Op::PushConst, 0)],
            entry: 0,
        };
        let rebuilt = program_in(&program_out(&p));
        assert_eq!(rebuilt.constants.len(), 2);
        assert_eq!(rebuilt.functions.len(), 1);
        assert_eq!(&*rebuilt.functions[0].name, "__main__");
        assert_eq!(rebuilt.code.len(), 1);
        assert_eq!(rebuilt.entry, 0);
    }
}
