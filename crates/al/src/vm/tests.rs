//! Tests for the VM front door: opcode goldens driven through whole
//! programs (`run_fn`), `inspect` layout pins, `values_equal` semantics,
//! and the scheduler's timer-wake and donation-guard edges. The fixture
//! helpers shared with sibling modules' tests (`halt_test_vm`,
//! `nil_value`) live in `mod.rs`.

use std::time::Duration;

use super::*;
use al_core::bytecode::values_equal;
use al_core::bytecode::{Instruction, SocketValue, op, op_ab, op_arg};
use al_core::frozen::FrozenArea;

// A single-function Program ("main", arity 0, no captures) over the given
// constants and code, with `locals` preallocated local slots. Constants
// are built through the program's own frozen builder, exactly as the
// compiler builds them.
fn single_fn_program(
    constants: impl FnOnce(&mut FrozenBuilder) -> Vec<Value>,
    code: Vec<Instruction>,
    locals: i32,
) -> Program {
    let frozen = Arc::new(FrozenArea::new());
    let mut fb = frozen.builder();
    let constants = constants(&mut fb);
    Program {
        constants,
        functions: vec![Function {
            name: "main".into(),
            arity: 0,
            locals,
            capture_count: 0,
            code_start: 0,
            code_len: code.len() as i32,
        }],
        code,
        entry: 0,
        frozen,
    }
}

// Build a single-function `Program` with `locals` preallocated local slots,
// run it to completion, and return the value left on top of the stack.
// Mirrors the entry-frame setup in `VM::run` (base_slot 0, locals zeroed), so
// `local[i]` lives at stack slot `i`. The VM is returned alongside the
// value because the value may point into main's arena, which the VM owns
// (see `VM::run`): callers must keep the VM bound while consuming the value.
fn run_fn(
    constants: impl FnOnce(&mut FrozenBuilder) -> Vec<Value>,
    code: Vec<Instruction>,
    locals: i32,
) -> (Value, VM) {
    let mut vm = new_vm(single_fn_program(constants, code, locals)).expect("vm must construct");
    let value = vm.run().expect("vm must run without panicking or erroring");
    // `value` first: it lives in `vm`'s heap, so it must drop (decref) before
    // the VM's heap is destroyed (tuple fields drop in declaration order).
    (value, vm)
}

// An array of small ints, e.g. `ints(&mut h, &[1, 2, 3])` == `[1, 2, 3]`.
fn ints(h: &mut ProcHeap, xs: &[i64]) -> Value {
    let elems: Vec<Value> = xs.iter().copied().map(Value::small_int).collect();
    Value::array_in(h, &elems)
}

// Only closures read the function table when rendering, so every fixture
// except `inspect_closure_renders_name` (which calls `super::inspect` with
// a real program) renders against an empty one. Shadows `vm::inspect`.
fn inspect(v: &Value) -> String {
    super::inspect(v, &Program::default())
}

#[test]
fn test_push_add_halt() {
    let (result, _vm) = run_fn(
        |_f| vec![Value::small_int(1), Value::small_int(2)],
        vec![
            op_arg(Op::PushConst, 0),
            op_arg(Op::PushConst, 1),
            op(Op::Add),
            op(Op::Halt),
        ],
        0,
    );
    assert_eq!(
        result.as_int(),
        Some(3),
        "expected Int(3), got {:?}",
        result
    );
}

#[test]
fn test_inspect_basic() {
    let mut h = ProcHeap::new();
    assert_eq!(inspect(&Value::small_int(42)), "42");
    assert_eq!(inspect(&Value::bool(true)), "True");
    let nil = nil_value(&mut h, al_core::TypeId(1));
    assert_eq!(inspect(&nil), "Nil");
    assert_eq!(inspect(&ints(&mut h, &[1, 2])), "[1, 2]");
    let range = Value::range_in(&mut h, 1, 4);
    assert_eq!(inspect(&range), "[1, 2, 3]");
}

#[test]
fn test_values_equal() {
    let mut h = ProcHeap::new();
    assert!(values_equal(&Value::small_int(5), &Value::small_int(5)));
    assert!(!values_equal(&Value::small_int(5), &Value::small_int(6)));
    let (na, nb) = (
        nil_value(&mut h, al_core::TypeId(1)),
        nil_value(&mut h, al_core::TypeId(1)),
    );
    assert!(values_equal(&na, &nb));
    let five = Value::str_in(&mut h, "5");
    assert!(!values_equal(&Value::small_int(5), &five));
    let range = Value::range_in(&mut h, 0, 3);
    let arr = ints(&mut h, &[0, 1, 2]);
    assert!(values_equal(&range, &arr));
}

// Regression: the Range arms of the safe-indexing opcodes computed
// `start + idx` (and lengths as `end - start`) with unchecked i64
// arithmetic. A non-zero-start range indexed near i64::MAX overflowed:
// debug panicked ("attempt to add with overflow"), release wrapped and
// returned a wrong, in-bounds-looking value instead of None. Indexing is a
// total Option op, so an overflowing index must read as out-of-bounds, and
// range length must saturate rather than wrap.
#[test]
fn range_index_and_len_do_not_overflow() {
    // `(5..10)[idx]` via Op::Index.
    let index_at = |idx: i64| -> (Value, VM) {
        run_fn(
            move |f| vec![f.int(5), f.int(10), f.int(idx)],
            vec![
                op_arg(Op::PushConst, 0),
                op_arg(Op::PushConst, 1),
                op(Op::MakeRange),
                op_arg(Op::PushConst, 2),
                op(Op::Index),
                op(Op::Halt),
            ],
            0,
        )
    };
    // In-bounds indexing is unchanged. Each result points into its VM's
    // arena, so the VM stays bound while the value is read.
    let (some, _vm) = index_at(2);
    let some = some.as_enum().expect("index returns an Option enum");
    assert_eq!(some.payload()[0].as_int(), Some(7));
    // 5 + i64::MAX overflows i64; the result must be None, not a wrapped
    // value that spuriously passes the `< end` bound.
    let (none, _vm) = index_at(i64::MAX);
    assert!(none.as_enum().expect("Option enum").payload().is_empty());

    // `len(i64::MIN..i64::MAX)` via Op::ArrayLen: `end - start` overflows,
    // so the length must saturate to i64::MAX instead of panicking/wrapping.
    let (len, _vm) = run_fn(
        |f| vec![f.int(i64::MIN), f.int(i64::MAX)],
        vec![
            op_arg(Op::PushConst, 0),
            op_arg(Op::PushConst, 1),
            op(Op::MakeRange),
            op(Op::ArrayLen),
            op(Op::Halt),
        ],
        0,
    );
    assert_eq!(len.as_int(), Some(i64::MAX));

    // values_equal's Range length arithmetic must not overflow on extreme
    // bounds either.
    let mut h = ProcHeap::new();
    let ra = Value::range_in(&mut h, i64::MIN, i64::MAX);
    let rb = Value::range_in(&mut h, i64::MIN, i64::MAX);
    assert!(values_equal(&ra, &rb));
}

// `Option::Some(payload)` with the payload-folded hash the VM computes.
fn build_some(h: &mut ProcHeap, payload: Value) -> Value {
    ev(h, 7, "Option", "Some", &["value"], vec![payload])
}

// Regression: wrapping a range in a constructor (`Some(0..n)`) eagerly folds
// the payload hash via `enum_hash_with_payload`. Hashing the range must be
// O(1), and must still match the hash of the array it materialises to, so
// the `ae.hash == be.hash` fast-reject in `values_equal` never drops an
// equal pair. Pre-fix this hashed every element and hung on a large range.
#[test]
fn enum_with_range_payload_equals_materialized_and_builds_fast() {
    let mut h = ProcHeap::new();
    let range = Value::range_in(&mut h, 0, 4);
    let from_range = build_some(&mut h, range);
    let arr = ints(&mut h, &[0, 1, 2, 3]);
    let from_array = build_some(&mut h, arr);
    assert!(
        values_equal(&from_range, &from_array),
        "Some(0..4) must equal Some([0, 1, 2, 3])"
    );

    // Building this hung before the fix: the range payload was hashed
    // element by element across ~9.2e18 values. Reaching the asserts proves
    // construction is bounded.
    let huge_range = Value::range_in(&mut h, 0, i64::MAX);
    let huge = build_some(&mut h, huge_range);
    assert!(values_equal(&huge, &huge));
    assert!(!values_equal(&huge, &from_range));
}

// Regression (signed-zero congruence): `+0.0` and `-0.0` are `values_equal`
// (`0.0 == -0.0` in IEEE 754), so congruence (`a == b => f(a) == f(b)`)
// demands `Some(0.0) == Some(-0.0)`. The enum-equality arm gates on the
// cached payload hash (`ae.hash == be.hash` as a necessary precondition);
// hashing a signed zero by its raw bits fast-rejected the equal pair. `-0.0`
// is reachable from ordinary code via unary `-` (`Op::NegFloat`).
#[test]
fn enum_with_signed_zero_payload_is_congruent() {
    // Premise: the scalar payloads compare equal.
    assert!(values_equal(&Value::float(0.0), &Value::float(-0.0)));

    let mut h = ProcHeap::new();
    let pos = build_some(&mut h, Value::float(0.0));
    let neg = build_some(&mut h, Value::float(-0.0));
    assert!(
        values_equal(&pos, &neg),
        "Some(0.0) must equal Some(-0.0): a == b => f(a) == f(b)"
    );
    assert!(values_equal(&neg, &pos), "equality must be symmetric");
    // A genuinely distinct payload must still compare unequal.
    let one = build_some(&mut h, Value::float(1.0));
    assert!(!values_equal(&pos, &one));
}

// The peephole pass fuses `PushLocal a; PushConst b; AddInt` into a single
// `AddIntLC` whose result is `local[a] + constants[b]`. Loop goldens exercise
// it only when fusion fires; nothing pins the fused op's actual result or its
// overflow edge. AddIntLC uses `wrapping_add`, so overflow is total — it wraps
// rather than panicking.
#[test]
fn superinstr_add_int_lc_adds_local_and_const_with_wrapping() {
    // local[0] = constants[0]; AddIntLC pushes local[0] + constants[1].
    let add = |local: i64, c: i64| -> Option<i64> {
        let (v, _vm) = run_fn(
            move |f| vec![f.int(local), f.int(c)],
            vec![
                op_arg(Op::PushConst, 0),
                op_arg(Op::StoreLocal, 0),
                op_ab(Op::AddIntLC, 0, 1, 0),
                op(Op::Halt),
            ],
            1,
        );
        v.as_int()
    };
    assert_eq!(add(40, 2), Some(42));
    assert_eq!(add(-5, 8), Some(3));
    assert_eq!(add(7, -7), Some(0));
    // wrapping_add edge: i64::MAX + 1 wraps to i64::MIN, never panics.
    assert_eq!(add(i64::MAX, 1), Some(i64::MIN));
}

// `PushLocal a; PushConst b; SubInt` fuses to `SubIntLC` = `local[a] -
// constants[b]`. SubIntLC uses `wrapping_sub`; the underflow edge must wrap.
#[test]
fn superinstr_sub_int_lc_subtracts_const_from_local_with_wrapping() {
    // local[0] = constants[0]; SubIntLC pushes local[0] - constants[1].
    let sub = |local: i64, c: i64| -> Option<i64> {
        let (v, _vm) = run_fn(
            move |f| vec![f.int(local), f.int(c)],
            vec![
                op_arg(Op::PushConst, 0),
                op_arg(Op::StoreLocal, 0),
                op_ab(Op::SubIntLC, 0, 1, 0),
                op(Op::Halt),
            ],
            1,
        );
        v.as_int()
    };
    assert_eq!(sub(50, 8), Some(42));
    assert_eq!(sub(3, 10), Some(-7));
    // wrapping_sub edge: i64::MIN - 1 wraps to i64::MAX, never panics.
    assert_eq!(sub(i64::MIN, 1), Some(i64::MAX));
}

// Run a fused `local[0] <jump_op> constants[1]` branch and report whether it
// was taken. The taken target (ip 5) and the fall-through (ip 3) push
// distinct markers, so the return value reveals which path executed.
fn jump_lc_taken(jump_op: Op, local: i64, c: i64) -> bool {
    let (r, _vm) = run_fn(
        move |f| {
            vec![
                f.int(local),        // 0: local value
                f.int(c),            // 1: compared constant
                Value::small_int(0), // 2: fall-through marker (not taken)
                Value::small_int(1), // 3: jump-target marker (taken)
            ]
        },
        vec![
            op_arg(Op::PushConst, 0),
            op_arg(Op::StoreLocal, 0),
            op_ab(jump_op, 0, 1, 5),
            op_arg(Op::PushConst, 2),
            op(Op::Halt),
            op_arg(Op::PushConst, 3),
            op(Op::Halt),
        ],
        1,
    );
    r.as_int() == Some(1)
}

// `PushLocal a; PushConst b; LtInt; JumpIfFalse t` fuses to `JumpGeIntLC`: a
// zero-stack-traffic branch taken iff `local[a] >= constants[b]`, jumping to
// the absolute target in `operand` (the VM computes `operand - code_start`).
// Pins both the `>=` semantics (incl. the equal boundary) and the retarget.
#[test]
fn superinstr_jump_ge_int_lc_branches_and_retargets() {
    let taken = |local: i64, c: i64| jump_lc_taken(Op::JumpGeIntLC, local, c);
    assert!(taken(10, 5), "10 >= 5 takes the branch");
    assert!(taken(5, 5), "5 >= 5: the equal boundary is taken");
    assert!(!taken(4, 5), "4 >= 5 is false: falls through");
    assert!(taken(i64::MAX, i64::MIN), "MAX >= MIN");
    assert!(!taken(i64::MIN, i64::MAX), "MIN >= MAX is false");
}

// `PushLocal a; PushConst b; EqInt; JumpIfFalse t` fuses to `JumpNeIntLC`:
// branch taken iff `local[a] != constants[b]`.
#[test]
fn superinstr_jump_ne_int_lc_branches_and_retargets() {
    let taken = |local: i64, c: i64| jump_lc_taken(Op::JumpNeIntLC, local, c);
    assert!(taken(10, 5), "10 != 5 takes the branch");
    assert!(
        !taken(5, 5),
        "5 != 5 is false: the equal case falls through"
    );
    assert!(taken(-1, 1), "-1 != 1 takes the branch");
}

// Build an enum Value with the same payload-folded hash the VM computes, so
// it is usable with `values_equal` as well as `inspect`.
fn ev(
    h: &mut ProcHeap,
    type_id: i32,
    en: &str,
    vn: &str,
    labels: &[&str],
    payload: Vec<Value>,
) -> Value {
    Value::enum_with_names_in(h, al_core::TypeId(type_id), 0, en, vn, labels, &payload)
}

// A function value renders as `<fn#name>`, the name resolved through
// `program.functions[func_idx]` — it is not stored in the closure.
#[test]
fn inspect_closure_renders_name() {
    let mut program = single_fn_program(|_| Vec::new(), Vec::new(), 0);
    program.functions[0].name = "myfn".into();
    let mut h = ProcHeap::new();
    let c = Value::closure_in(&mut h, 0, &[]);
    assert_eq!(super::inspect(&c, &program), "<fn#myfn>");
}

// A socket value renders with its kind and id.
#[test]
fn inspect_socket_renders_kind_and_id() {
    let conn = Value::socket(SocketValue {
        id: 7,
        is_listener: false,
    });
    let listener = Value::socket(SocketValue {
        id: 2,
        is_listener: true,
    });
    assert_eq!(inspect(&conn), "<socket#7>");
    assert_eq!(inspect(&listener), "<listener#2>");
}

// An array of all-simple leaves whose single-line form would exceed 80
// columns wraps six values per line (not one-per-line, and not flat). Pins
// the exact 6-per-line layout in `inspect_impl`.
#[test]
fn inspect_long_simple_array_wraps_six_per_line() {
    let mut h = ProcHeap::new();
    let elems: Vec<Value> = (1..=30).map(Value::small_int).collect();
    let arr = Value::array_in(&mut h, &elems);
    let s = inspect(&arr);
    let lines: Vec<&str> = s.lines().collect();
    assert_eq!(lines[0], "[");
    assert_eq!(lines[1], "  1, 2, 3, 4, 5, 6, ");
    assert_eq!(lines[2], "  7, 8, 9, 10, 11, 12, ");
    assert_eq!(lines[5], "  25, 26, 27, 28, 29, 30");
    assert_eq!(*lines.last().unwrap(), "]");
    // "[" + ceil(30/6)=5 content rows + "]".
    assert_eq!(lines.len(), 7);
}

// A short all-simple array stays on one line (the <= 80 fast path).
#[test]
fn inspect_short_array_stays_flat() {
    let mut h = ProcHeap::new();
    assert_eq!(inspect(&ints(&mut h, &[1, 2, 3])), "[1, 2, 3]");
}

// An array whose children are themselves containers (not "simple") expands
// one element per line via `block`.
#[test]
fn inspect_nested_array_expands_per_line() {
    let mut h = ProcHeap::new();
    let (a, b) = (ints(&mut h, &[1, 2]), ints(&mut h, &[3, 4]));
    let arr = Value::array_in(&mut h, &[a, b]);
    assert_eq!(inspect(&arr), "[\n  [1, 2],\n  [3, 4]\n]");
}

// A record (constructor named after its type, with field labels) prints
// `T{ a: .., b: .. }` inline when its fields are simple, and as a multiline
// `T {` block when a field is itself a record.
#[test]
fn inspect_record_inline_and_multiline() {
    let mut h = ProcHeap::new();
    let point = |h: &mut ProcHeap, x: i64, y: i64| {
        ev(
            h,
            1,
            "Point",
            "Point",
            &["x", "y"],
            vec![Value::small_int(x), Value::small_int(y)],
        )
    };
    let p12 = point(&mut h, 1, 2);
    assert_eq!(inspect(&p12), "Point{ x: 1, y: 2 }");

    let p34 = point(&mut h, 3, 4);
    let line = ev(&mut h, 2, "Line", "Line", &["a", "b"], vec![p12, p34]);
    assert_eq!(
        inspect(&line),
        "Line {\n  a: Point{ x: 1, y: 2 },\n  b: Point{ x: 3, y: 4 }\n}"
    );
}

// A positional sum-type variant prints `Variant(..)`, expanding to a
// multiline block when a payload element is itself a non-simple value.
#[test]
fn inspect_variant_inline_and_multiline() {
    let mut h = ProcHeap::new();
    let leaf = |h: &mut ProcHeap, v: i64| ev(h, 3, "Tree", "Leaf", &[], vec![Value::small_int(v)]);
    let l1 = leaf(&mut h, 1);
    assert_eq!(inspect(&l1), "Leaf(1)");

    let l2 = leaf(&mut h, 2);
    let node = ev(&mut h, 3, "Tree", "Node", &[], vec![l1, l2]);
    assert_eq!(inspect(&node), "Node(\n  Leaf(1),\n  Leaf(2)\n)");
}

// Tuples: empty renders `()`; a tuple containing a non-simple element
// expands one element per line.
#[test]
fn inspect_tuple_empty_and_multiline() {
    let mut h = ProcHeap::new();
    let empty = Value::tuple_in(&mut h, &[]);
    assert_eq!(inspect(&empty), "()");
    let arr = ints(&mut h, &[2, 3, 4]);
    let t = Value::tuple_in(&mut h, &[Value::small_int(1), arr]);
    assert_eq!(inspect(&t), "(\n  1,\n  [2, 3, 4]\n)");
}

// `values_equal` treats a Range and an Array as equal iff they enumerate the
// same elements: a length difference and an element difference must both
// compare unequal, in either operand order.
#[test]
fn values_equal_range_vs_array() {
    let mut h = ProcHeap::new();
    let range = Value::range_in(&mut h, 0, 3);
    // 0..3 == [0, 1, 2]
    let a = ints(&mut h, &[0, 1, 2]);
    assert!(values_equal(&range, &a));
    assert!(values_equal(&a, &range));
    // length mismatch.
    let b = ints(&mut h, &[0, 1, 2, 3]);
    assert!(!values_equal(&range, &b));
    assert!(!values_equal(&b, &range));
    // same length, differing element.
    let c = ints(&mut h, &[0, 9, 2]);
    assert!(!values_equal(&range, &c));
    assert!(!values_equal(&c, &range));
}

// `values_equal` on Binary compares the bytes; equal-length differing bytes
// are unequal.
#[test]
fn values_equal_binary() {
    let mut h = ProcHeap::new();
    let mut bin = |bytes: &[u8]| Value::binary_in(&mut h, bytes.to_vec());
    let (a, b) = (bin(&[1, 2, 3]), bin(&[1, 2, 3]));
    let c = bin(&[1, 2, 4]);
    let d = bin(&[1, 2]);
    assert!(values_equal(&a, &b));
    assert!(!values_equal(&a, &c));
    assert!(!values_equal(&d, &a));
}

// `values_equal` on Socket compares (id, is_listener).
#[test]
fn values_equal_socket() {
    let sock = |id, is_listener| Value::socket(SocketValue { id, is_listener });
    assert!(values_equal(&sock(5, false), &sock(5, false)));
    assert!(!values_equal(&sock(5, false), &sock(6, false)));
    assert!(!values_equal(&sock(5, false), &sock(5, true)));
}

// Slicing a Range past its length is a runtime error (the indices are not
// bounds-checked at compile time), not a silent wrap or empty result. The
// VM aborts with a message naming the range length.
#[test]
fn array_slice_range_out_of_bounds_errors() {
    // (0..5)[2..99]
    let program = single_fn_program(
        |_f| {
            vec![
                Value::small_int(0),
                Value::small_int(5),
                Value::small_int(2),
                Value::small_int(99),
            ]
        },
        vec![
            op_arg(Op::PushConst, 0),
            op_arg(Op::PushConst, 1),
            op(Op::MakeRange),
            op_arg(Op::PushConst, 2),
            op_arg(Op::PushConst, 3),
            op(Op::ArraySlice),
            op(Op::Halt),
        ],
        0,
    );
    let err = new_vm(program)
        .expect("vm must construct")
        .run()
        .expect_err("slicing a range out of bounds must error");
    assert!(
        matches!(err, super::VmError::SliceOutOfBounds { len: 5, .. }),
        "unexpected error: {err}"
    );
}

// --- Scheduler: timer wake and donation guard ---

// A placeholder suspended process. wake_due_timers only moves it between the
// parked store and the run queue; it is never resumed.
fn parked_process() -> Process {
    Process {
        heap: ProcHeap,
        stack: Vec::new(),
        frames: Vec::new(),
        is_main: false,
        pid: 0,
    }
}

// Park `wait` exactly as scheduler_loop's park arm does. Returns the wait id.
fn park_wait(vm: &mut VM, wait: Wait) -> u64 {
    vm.park(wait, parked_process())
}

// A deadline-only Wait is a pure timer: it must not
// wake before its instant, and must wake once the instant has passed, while
// a not-yet-due timer stays parked.
#[test]
fn deadline_only_wait_wakes_after_its_duration() {
    let mut vm = halt_test_vm();

    // A far-future pure timer must not wake.
    let far = park_wait(
        &mut vm,
        Wait::until(Instant::now() + Duration::from_secs(3600)),
    );
    assert!(!vm.wake_due_timers(), "a future deadline must not wake");
    assert_eq!(vm.run_queue.len(), 0);

    // A short timer wakes only once its duration has elapsed.
    park_wait(
        &mut vm,
        Wait::until(Instant::now() + Duration::from_millis(50)),
    );
    std::thread::sleep(Duration::from_millis(150));
    assert!(vm.wake_due_timers(), "an elapsed deadline must wake");
    assert_eq!(
        vm.run_queue.len(),
        1,
        "only the due timer moves to the run queue"
    );

    // The far-future timer is untouched, still parked.
    assert!(
        vm.parked.contains_key(&far),
        "the future timer stays parked"
    );
    assert_eq!(vm.parked.len(), 1);
}

// A Wait carrying both a readable interest and a deadline (the read_within
// shape) lives in `parked` and leaves a deadline entry in `timer_heap`. If
// the fd fires first, drain_io_events removes the parked entry but the heap
// entry remains — now stale. When that stale deadline later comes due,
// wake_due_timers must drop it: no spurious wake, no panic on the absent id.
#[test]
fn fd_wake_leaves_stale_timer_entry_that_wake_due_timers_discards() {
    let mut vm = halt_test_vm();
    let id = park_wait(
        &mut vm,
        Wait::read_with_deadline(7, Instant::now() + Duration::from_millis(50)),
    );
    assert_eq!(vm.timer_heap.len(), 1);

    // Simulate the fd firing first: the parked entry is removed and its
    // process queued (as drain_io_events would), but the timer heap still
    // holds the now-stale deadline id.
    let (_wait, p) = vm.park_remove(id).expect("parked entry present");
    vm.run_queue.push_back(p);
    assert!(vm.parked.is_empty());
    assert_eq!(vm.timer_heap.len(), 1, "the deadline entry is now stale");

    // Once the stale deadline comes due it must be discarded silently.
    std::thread::sleep(Duration::from_millis(150));
    let before = vm.run_queue.len();
    assert!(
        !vm.wake_due_timers(),
        "a stale timer entry must not report a wake"
    );
    assert_eq!(
        vm.run_queue.len(),
        before,
        "no process is spuriously re-queued"
    );
    assert!(
        vm.timer_heap.is_empty(),
        "the stale entry is drained from the heap"
    );
}

// The pre-side-effect donation guard: a victim referencing a connection id
// that is mid-connect (`pending_connects`) or armed by a parked sibling's
// `Wait` must not be donated; quiescent connections and listeners (which
// are shared program-wide, never moved) are always donatable.
#[test]
fn donation_fd_guard_blocks_entangled_connections() {
    let mut vm = halt_test_vm();
    let sock = |id, is_listener| Value::socket(SocketValue { id, is_listener });
    let victim = |v: Value| Process {
        heap: ProcHeap,
        stack: vec![v],
        frames: Vec::new(),
        is_main: false,
        pid: 0,
    };

    // A quiescent connection is donatable.
    assert!(vm.can_donate_fds(&victim(sock(7, false))));

    // Listener ids never need guarding, even while armed by a parked
    // accept — the listener socket is shared program-wide, so a moved
    // process resolves the same fd wherever it lands and the parked accept
    // still wakes on the donor (armed fd, no waiter: dropped by design).
    park_wait(&mut vm, Wait::readable(3));
    assert!(vm.can_donate_fds(&victim(sock(3, true))));

    // A parked sibling armed fd 7; moving it would break the sleeper.
    park_wait(&mut vm, Wait::readable(7));
    assert!(!vm.can_donate_fds(&victim(sock(7, false))));

    // The armed id is found through nested containers too.
    let mut h = ProcHeap::new();
    let arr = Value::array_in(&mut h, &[sock(7, false)]);
    assert!(!vm.can_donate_fds(&victim(arr)));

    // ... and through a frame's closure captures, not just the stack.
    let closure = Value::closure_in(&mut h, 0, &[sock(7, false)]);
    let framed = Process {
        heap: h,
        stack: Vec::new(),
        frames: vec![CallFrame {
            func_idx: 0,
            code_start: 0,
            ip: 0,
            base_slot: 0,
            captures: closure,
        }],
        is_main: false,
        pid: 0,
    };
    assert!(!vm.can_donate_fds(&framed));

    // An in-flight non-blocking connect pins its id to this scheduler.
    let pending = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None)
        .expect("socket creation");
    vm.pending_connects.insert(9, pending);
    assert!(!vm.can_donate_fds(&victim(sock(9, false))));

    // Unrelated connection ids remain donatable throughout.
    assert!(vm.can_donate_fds(&victim(sock(8, false))));
}

// Donation targeting must skip workers that never spawned. A worker
// `ensure_workers` failed to start publishes a load of 0 forever, so a
// load-only scan would always prefer it — and a migrant donated there
// would sit in an inbox no thread drains. The marker for "spawned" is a
// filled poller slot.
#[test]
fn donation_skips_never_spawned_workers() {
    use std::sync::atomic::Ordering;

    let program = single_fn_program(|_f| vec![], vec![op(Op::Halt)], 0);
    let (rt, _poll) =
        sched::Runtime::new(Arc::new(program), Vec::new(), 3).expect("runtime must construct");
    // Worker 1 spawned (slot filled, busier); worker 2's spawn failed
    // (slot empty, permanent load 0) — the state ensure_workers leaves
    // behind on partial failure.
    let poll = mio::Poll::new().expect("poller must construct");
    let waker = mio::Waker::new(poll.registry(), poll::WAKER_TOKEN).expect("waker must construct");
    let _ = rt.slots[1].waker.set(Arc::new(waker));
    rt.slots[1].run_len.store(3, Ordering::Relaxed);
    rt.slots[2].run_len.store(0, Ordering::Relaxed);

    assert_eq!(
        rt.pick_underloaded_peer(0, 9),
        Some(1),
        "the busier-but-live worker is chosen over the dead one"
    );
    assert_eq!(
        rt.pick_underloaded_peer(0, 4),
        None,
        "with the only live peer too loaded to narrow the gap, nothing is picked"
    );
}
