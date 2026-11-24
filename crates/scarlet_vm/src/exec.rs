//! The interpreter, which runs loaded instructions.
//!
//! Frames live on a stack the VM owns, not on Rust's, so a deep recursion in
//! a Scarlet program uses memory and never overflows anything. All frames'
//! registers sit in one vector. A frame is where its registers start, and
//! beside them a record of where it returns to ([`Frame`]). The loop holds
//! only the running frame's place in the registers and in the code.
//!
//! Reference counting follows one rule. Every register, global and argument
//! holds one reference to its value. So copying a value into a register adds
//! a reference, and overwriting a register, or leaving a frame, gives one up.
//! Where the loader saw that nothing reads a register again, the copy takes
//! the register's reference instead of adding one (`code::live`).
//! Perceus's `Drop` gives one up early. This alone keeps every count right,
//! whatever Perceus did or did not insert (`docs/vm-design.md`, "Memory").
//! If a count is wrong all the same, the instruction that takes or gives up
//! a reference to a cell already freed stops the run with `Broken::DeadCell`.

use scarlet_ir::core_ir::{FuncIdx, GlobalSlot};

use crate::array::{self, End, Seq};
use crate::code::{
    Arg, Args, BitsOp, Code, Counted, Dead, Dst, Entry, Hit, Ins, Kept, Op, Over, Reg, Room,
    Shared, Src, Taken, Variant,
};
use crate::counts::Counts;
use crate::eq;
use crate::float;
use crate::heap::{DeadCell, Heap, Kind};
use crate::host::Host;
use crate::int;
use crate::map;
use crate::ops::{Arith, Compare, Order};
use crate::stop::{Broken, Missing, fault};
use crate::value::{Value, View};
use crate::{Audit, Stop};

mod rest;

use rest::Rest;

// The handlers of the operator instructions. Each takes its operands out of
// the registers, does its operator in place when it can, and asks the one slow
// path of its kind when it cannot. The operator is a constant in every use, so
// nothing looks at it as a program runs.

macro_rules! int {
    ($vm:ident, $base:ident, $dst:ident, $a:ident, $b:ident, $op:expr) => {{
        let (x, y) = ($vm.look($base, $a), $vm.look($base, $b));
        match x.both_small(y).and_then(|(x, y)| int::fast($op, x, y)) {
            Some(v) => $vm.put($base, $dst, v)?,
            None => $vm.int_slow($base, $dst, $op, x, y)?,
        }
    }};
}

macro_rules! int_k {
    ($vm:ident, $base:ident, $dst:ident, $a:ident, $k:ident, $op:expr) => {{
        let x = $vm.look($base, $a);
        match x.as_small().and_then(|x| int::fast($op, x, $k)) {
            Some(v) => $vm.put($base, $dst, v)?,
            None => $vm.int_slow($base, $dst, $op, x, $k.value())?,
        }
    }};
}

macro_rules! int_k_first {
    ($vm:ident, $base:ident, $dst:ident, $k:ident, $a:ident, $op:expr) => {{
        let y = $vm.look($base, $a);
        match y.as_small().and_then(|y| int::fast($op, $k, y)) {
            Some(v) => $vm.put($base, $dst, v)?,
            None => $vm.int_slow($base, $dst, $op, $k.value(), y)?,
        }
    }};
}

macro_rules! int_holds {
    ($vm:ident, $base:ident, $a:ident, $b:ident, $op:expr) => {{
        let (x, y) = ($vm.look($base, $a), $vm.look($base, $b));
        match x.both_small(y) {
            Some((x, y)) => $op.holds(x.cmp(&y)),
            None => int::compare(&$vm.rest.heap, $op, x, y)?,
        }
    }};
}

macro_rules! int_holds_k {
    ($vm:ident, $base:ident, $a:ident, $k:ident, $op:expr) => {{
        let x = $vm.look($base, $a);
        match x.as_small() {
            Some(x) => $op.holds(x.cmp(&$k)),
            None => int::compare(&$vm.rest.heap, $op, x, $k.value())?,
        }
    }};
}

macro_rules! float {
    ($vm:ident, $base:ident, $dst:ident, $a:ident, $b:ident, $op:expr) => {{
        let (x, y) = ($vm.look($base, $a), $vm.look($base, $b));
        match float::fast($op, x, y) {
            Some(v) => $vm.put($base, $dst, v)?,
            None => $vm.float_slow($base, $dst, $op, x, y)?,
        }
    }};
}

macro_rules! float_k {
    ($vm:ident, $base:ident, $dst:ident, $a:ident, $k:ident, $op:expr) => {{
        let x = $vm.look($base, $a);
        match float::fast($op, x, $k.value()) {
            Some(v) => $vm.put($base, $dst, v)?,
            None => $vm.float_slow($base, $dst, $op, x, $k.value())?,
        }
    }};
}

macro_rules! float_k_first {
    ($vm:ident, $base:ident, $dst:ident, $k:ident, $a:ident, $op:expr) => {{
        let y = $vm.look($base, $a);
        match float::fast($op, $k.value(), y) {
            Some(v) => $vm.put($base, $dst, v)?,
            None => $vm.float_slow($base, $dst, $op, $k.value(), y)?,
        }
    }};
}

macro_rules! float_holds {
    ($vm:ident, $base:ident, $a:ident, $b:ident, $op:expr) => {{
        let (x, y) = ($vm.look($base, $a), $vm.look($base, $b));
        match float::fast_compare($op, x, y) {
            Some(holds) => holds,
            None => float::slow_compare($op, x, y)?,
        }
    }};
}

macro_rules! float_holds_k {
    ($vm:ident, $base:ident, $a:ident, $k:ident, $op:expr) => {{
        let x = $vm.look($base, $a);
        match float::fast_compare($op, x, $k.value()) {
            Some(holds) => holds,
            None => float::slow_compare($op, x, $k.value())?,
        }
    }};
}

macro_rules! store_bool {
    ($vm:ident, $base:ident, $dst:ident, $holds:ident($a:ident, $b:ident), $op:expr) => {
        $vm.put($base, $dst, Value::bool($holds!($vm, $base, $a, $b, $op)))?
    };
}

macro_rules! jump {
    ($vm:ident, $base:ident, $pc:ident, $to:ident, $holds:ident($a:ident, $b:ident), $op:expr) => {
        if !$holds!($vm, $base, $a, $b, $op) {
            $pc = $to.index();
        }
    };
}

/// One call that has not returned: where it goes on when it does, and what it
/// runs as.
struct Frame {
    /// The instruction it returns to.
    pc: u32,
    /// How far below it its caller's registers start, which is how many the
    /// caller has.
    below: Room,
    /// The caller's register the result goes to.
    dst: Reg,
    /// The closure it runs as, holding one reference. It is a closure cell, a
    /// function value, or `NIL` for a call by name. Its captures are what
    /// `GetCapture` reads.
    env: Value,
}

// The fields keep this order, the ones the loop reads first and the heap
// last, so that a change to the heap's size cannot move them. An offset past
// what a load can hold takes an instruction more at each use.
#[repr(C)]
pub(crate) struct Machine<'c, 'h> {
    /// Every frame's registers. Each past the running frame's is `Nil`.
    stack: Vec<Value>,
    /// The running frame's record, last, and those of the frames waiting on
    /// it.
    frames: Vec<Frame>,
    /// The values on their way into a frame or a cell, which are a call's
    /// arguments or a constructor's fields. It is kept between uses so that
    /// neither allocates.
    passing: Vec<Value>,
    rest: Rest<'c, 'h>,
}

/// A run that reached its program's end, holding the value of the last thing
/// it ran and that value's reference. Only [`Machine::run`] makes one, so
/// whatever takes one knows the run did not stop part way.
pub(crate) struct Ended {
    last: Value,
}

impl<'c, 'h> Machine<'c, 'h> {
    pub(crate) fn new(code: &'c Code, host: &'h mut dyn Host) -> Self {
        Machine {
            stack: Vec::new(),
            frames: Vec::new(),
            passing: Vec::new(),
            rest: Rest {
                code,
                host,
                globals: vec![Value::NIL; code.globals as usize],
                env: None,
                ran: Vec::new(),
                heap: Heap::default(),
            },
        }
    }

    /// A machine for a counted load, which counts each run of each block.
    pub(crate) fn counting(counted: &'c Counted, host: &'h mut dyn Host) -> Self {
        let mut machine = Machine::new(&counted.code, host);
        machine.rest.ran = vec![0; counted.blocks.len()];
        machine
    }

    /// Run every module's toplevel in order, then `main` if the program has
    /// one. The result holds the value of the last thing run, which is
    /// `main`'s, or the entry toplevel's for a script.
    pub(crate) fn run(&mut self) -> Result<Ended, Stop> {
        let code = self.rest.code;
        let mut last = Value::NIL;
        for top in &code.toplevels {
            // Given up first, so that while a call runs every reference the
            // run holds is one an audit is told of.
            self.rest.release(last)?;
            last = self.call(top, &[])?;
        }
        if let Some(main) = code.main {
            self.rest.release(last)?;
            let main = code.funcs.get(main).ok_or(Missing::NoFunc(main));
            last = self.call(main.map_err(Stop::missing)?, &[])?;
        }
        self.rest.let_go_of_handles();
        Ok(Ended { last })
    }

    /// Call `entry` with `args` and run until it returns. The arguments'
    /// references pass to the callee, and the result's passes to the caller.
    fn call(&mut self, entry: &Entry, args: &[Value]) -> Result<Value, Stop> {
        // It returns to the code's first instruction, the `Halt`, which takes
        // the result from the first register. So there is always one.
        self.open(entry.needs.len().max(1));
        for (slot, arg) in self.stack.iter_mut().zip(args) {
            *slot = *arg;
        }
        self.open_frame(Frame {
            pc: 0,
            below: Room(0),
            dst: Reg(0),
            env: Value::NIL,
        });
        self.steps(entry.pc.index())
    }

    /// Run from instruction `pc`, in the frame at the stack's start.
    fn steps(&mut self, start: usize) -> Result<Value, Stop> {
        let code = self.rest.code;
        let ins = code.ins.as_slice();
        let mut pc = start;
        let mut base = 0usize;
        loop {
            let Some(i) = ins.get(pc) else {
                return Err(self.ran_past_end(pc));
            };
            // `pc` names an instruction, so one past it is exact.
            pc = pc.wrapping_add(1);
            // Matched in place: a copy of the instruction would have every
            // field of it read before the jump that finds its handler.
            match *i {
                Ins::Halt { src } => return Ok(self.take(base, src)),
                Ins::Count { block } => {
                    if let Some(n) = self.rest.ran.get_mut(block.0 as usize) {
                        *n = n.wrapping_add(1);
                    }
                }
                Ins::Audit => self.audit(Value::NIL)?,
                Ins::Const { dst, value } => self.put(base, dst, value)?,
                Ins::Str { dst, text } => {
                    let v = self.rest.text_const(text)?;
                    self.put(base, dst, v)?;
                }
                Ins::BinaryConst { dst, bits } => {
                    let v = self.rest.binary_const(bits)?;
                    self.put(base, dst, v)?;
                }
                Ins::Bits { dst, op, args } => self.bits(base, dst, op, args.regs())?,
                Ins::BitsWide { dst, op, args } => {
                    self.bits(base, dst, op, args.regs(&code.lists))?;
                }
                Ins::BigInt { dst, n, id } => {
                    let v = self.rest.big_const(id, n)?;
                    self.put(base, dst, v)?;
                }
                // Only a cell has a reference to hand over or to add, so
                // any other value is copied and its register left alone.
                Ins::Move { dst, src } => {
                    let v = self.get(base, src.reg);
                    if let Some(cell) = v.as_cell() {
                        if src.moved {
                            self.empty(base, src.reg);
                        } else {
                            self.rest.heap.share(cell)?;
                        }
                    }
                    self.put(base, dst, v)?;
                }
                Ins::GetGlobal { dst, slot } => {
                    let v = global(slot)
                        .and_then(|i| self.rest.globals.get(i))
                        .copied()
                        .unwrap_or(Value::NIL);
                    let v = self.rest.share(v)?;
                    self.put(base, dst, v)?;
                }
                Ins::SetGlobal { slot, src } => {
                    let v = self.share(base, src)?;
                    if let Some(g) = global(slot).and_then(|i| self.rest.globals.get_mut(i)) {
                        let old = std::mem::replace(g, v);
                        self.rest.release(old)?;
                    }
                }
                Ins::IntAdd { dst, a, b } => int!(self, base, dst, a, b, Arith::Add),
                Ins::IntSub { dst, a, b } => int!(self, base, dst, a, b, Arith::Sub),
                Ins::IntMul { dst, a, b } => int!(self, base, dst, a, b, Arith::Mul),
                Ins::IntDiv { dst, a, b } => int!(self, base, dst, a, b, Arith::Div),
                Ins::IntRem { dst, a, b } => int!(self, base, dst, a, b, Arith::Rem),
                Ins::IntAddK { dst, a, k } => int_k!(self, base, dst, a, k, Arith::Add),
                Ins::IntSubK { dst, a, k } => int_k!(self, base, dst, a, k, Arith::Sub),
                Ins::IntMulK { dst, a, k } => int_k!(self, base, dst, a, k, Arith::Mul),
                Ins::IntDivK { dst, a, k } => int_k!(self, base, dst, a, k, Arith::Div),
                Ins::IntRemK { dst, a, k } => int_k!(self, base, dst, a, k, Arith::Rem),
                Ins::IntRSubK { dst, k, a } => int_k_first!(self, base, dst, k, a, Arith::Sub),
                Ins::IntEq { dst, a, b } => {
                    store_bool!(self, base, dst, int_holds(a, b), Compare::Eq);
                }
                Ins::IntNe { dst, a, b } => {
                    store_bool!(self, base, dst, int_holds(a, b), Compare::Ne);
                }
                Ins::IntLt { dst, a, b } => {
                    store_bool!(self, base, dst, int_holds(a, b), Compare::LT);
                }
                Ins::IntLe { dst, a, b } => {
                    store_bool!(self, base, dst, int_holds(a, b), Compare::LE);
                }
                Ins::IntEqK { dst, a, k } => {
                    store_bool!(self, base, dst, int_holds_k(a, k), Compare::Eq);
                }
                Ins::IntNeK { dst, a, k } => {
                    store_bool!(self, base, dst, int_holds_k(a, k), Compare::Ne);
                }
                Ins::IntLtK { dst, a, k } => {
                    store_bool!(self, base, dst, int_holds_k(a, k), Compare::LT);
                }
                Ins::IntLeK { dst, a, k } => {
                    store_bool!(self, base, dst, int_holds_k(a, k), Compare::LE);
                }
                Ins::IntGtK { dst, a, k } => {
                    store_bool!(self, base, dst, int_holds_k(a, k), Compare::GT);
                }
                Ins::IntGeK { dst, a, k } => {
                    store_bool!(self, base, dst, int_holds_k(a, k), Compare::GE);
                }
                Ins::IntNeg { dst, a } => {
                    let x = self.look(base, a);
                    match x.as_small().and_then(int::fast_neg) {
                        Some(v) => self.put(base, dst, v)?,
                        None => self.int_neg_slow(base, dst, x)?,
                    }
                }
                Ins::FloatAdd { dst, a, b } => float!(self, base, dst, a, b, Arith::Add),
                Ins::FloatSub { dst, a, b } => float!(self, base, dst, a, b, Arith::Sub),
                Ins::FloatMul { dst, a, b } => float!(self, base, dst, a, b, Arith::Mul),
                Ins::FloatDiv { dst, a, b } => float!(self, base, dst, a, b, Arith::Div),
                Ins::FloatRem { dst, a, b } => float!(self, base, dst, a, b, Arith::Rem),
                Ins::FloatAddK { dst, a, k } => float_k!(self, base, dst, a, k, Arith::Add),
                Ins::FloatSubK { dst, a, k } => float_k!(self, base, dst, a, k, Arith::Sub),
                Ins::FloatMulK { dst, a, k } => float_k!(self, base, dst, a, k, Arith::Mul),
                Ins::FloatDivK { dst, a, k } => float_k!(self, base, dst, a, k, Arith::Div),
                Ins::FloatRSubK { dst, k, a } => {
                    float_k_first!(self, base, dst, k, a, Arith::Sub);
                }
                Ins::FloatRDivK { dst, k, a } => {
                    float_k_first!(self, base, dst, k, a, Arith::Div);
                }
                Ins::FloatLt { dst, a, b } => {
                    store_bool!(self, base, dst, float_holds(a, b), Order::Lt);
                }
                Ins::FloatLe { dst, a, b } => {
                    store_bool!(self, base, dst, float_holds(a, b), Order::Le);
                }
                Ins::FloatLtK { dst, a, k } => {
                    store_bool!(self, base, dst, float_holds_k(a, k), Order::Lt);
                }
                Ins::FloatLeK { dst, a, k } => {
                    store_bool!(self, base, dst, float_holds_k(a, k), Order::Le);
                }
                Ins::FloatGtK { dst, a, k } => {
                    store_bool!(self, base, dst, float_holds_k(a, k), Order::Gt);
                }
                Ins::FloatGeK { dst, a, k } => {
                    store_bool!(self, base, dst, float_holds_k(a, k), Order::Ge);
                }
                Ins::FloatNeg { dst, a } => {
                    let x = self.look(base, a);
                    match float::fast_neg(x) {
                        Some(v) => self.put(base, dst, v)?,
                        None => self.float_neg_slow(base, dst, x)?,
                    }
                }
                Ins::FloatFromInt { dst, a } => {
                    let x = self.look(base, a);
                    match x.small_int() {
                        Some(n) => self.put(base, dst, Value::float(n as f64))?,
                        None => self.float_from_int_slow(base, dst, x)?,
                    }
                }
                Ins::Num { dst, op, a, b } => {
                    let (a, b) = (self.look(base, a), self.look(base, b));
                    let v = self.rest.num(op, a, b)?;
                    self.put(base, dst, v)?;
                }
                Ins::Neg { dst, a } => {
                    let a = self.look(base, a);
                    let v = self.rest.neg(a)?;
                    self.put(base, dst, v)?;
                }
                Ins::Print { dst, a, newline } => {
                    self.rest.print(self.look(base, a), newline.0)?;
                    self.put(base, dst, Value::NIL)?;
                }
                Ins::ToString { dst, a } => {
                    let v = self.get(base, a.0);
                    if self.rest.is_string(v) {
                        let v = self.rest.share(v)?;
                        self.put(base, dst, v)?;
                    } else {
                        self.make_text(base, dst, v)?;
                    }
                }
                Ins::Concat { dst, parts } => self.concat(base, dst, parts.regs())?,
                Ins::ConcatWide { dst, parts } => {
                    self.concat(base, dst, parts.regs(&code.lists))?;
                }
                Ins::Ctor {
                    dst,
                    variant,
                    ref fields,
                } => self.ctor(base, dst, variant, fields)?,
                Ins::CtorWide {
                    dst,
                    variant,
                    fields,
                } => self.ctor_wide(base, dst, variant, fields.args(&code.lists))?,
                Ins::CtorOver {
                    dst,
                    variant,
                    ref fields,
                    reuse,
                } => self.over_few(base, dst, variant, fields, reuse)?,
                Ins::CtorOverWide {
                    dst,
                    variant,
                    fields,
                    reuse,
                } => self.over_wide(base, dst, variant, fields.args(&code.lists), reuse)?,
                Ins::Drop { reg } => self.drop(base, reg)?,
                Ins::DropReuse { reg } => self.keep(base, reg)?,
                Ins::Call {
                    dst,
                    to,
                    ref args,
                    needs,
                    own,
                } => {
                    let up = base.wrapping_add(own.len());
                    self.enter(base, args.iter(), up, needs)?;
                    self.open_frame(Frame {
                        pc: low(pc),
                        below: own,
                        dst: dst.0,
                        env: Value::NIL,
                    });
                    base = up;
                    pc = to.index();
                }
                Ins::CallWide {
                    dst,
                    to,
                    args,
                    needs,
                    own,
                } => {
                    let up = base.wrapping_add(own.len());
                    self.enter_wide(base, args.args(&code.lists), up, needs)?;
                    self.open_frame(Frame {
                        pc: low(pc),
                        below: own,
                        dst: dst.0,
                        env: Value::NIL,
                    });
                    base = up;
                    pc = to.index();
                }
                Ins::CallSelf {
                    dst,
                    to,
                    ref args,
                    own,
                } => {
                    let env = self.env();
                    let env = self.rest.share(env)?;
                    let up = base.wrapping_add(own.len());
                    self.enter(base, args.iter(), up, own)?;
                    self.open_frame(Frame {
                        pc: low(pc),
                        below: own,
                        dst: dst.0,
                        env,
                    });
                    base = up;
                    pc = to.index();
                }
                Ins::CallSelfWide { dst, to, args, own } => {
                    let env = self.env();
                    let env = self.rest.share(env)?;
                    let up = base.wrapping_add(own.len());
                    self.enter_wide(base, args.args(&code.lists), up, own)?;
                    self.open_frame(Frame {
                        pc: low(pc),
                        below: own,
                        dst: dst.0,
                        env,
                    });
                    base = up;
                    pc = to.index();
                }
                Ins::CallValue {
                    dst,
                    callee,
                    ref args,
                    own,
                } => {
                    let (f, argc) = (self.get(base, callee.0), args.len());
                    let (entry, env) = self.rest.callee(f, argc)?;
                    let up = base.wrapping_add(own.len());
                    self.enter(base, args.iter(), up, entry.needs)?;
                    self.open_frame(Frame {
                        pc: low(pc),
                        below: own,
                        dst: dst.0,
                        env,
                    });
                    base = up;
                    pc = entry.pc.index();
                }
                Ins::CallValueWide {
                    dst,
                    callee,
                    args,
                    own,
                } => {
                    let args = args.args(&code.lists);
                    let (entry, env) = self.rest.callee(self.get(base, callee.0), args.len())?;
                    let up = base.wrapping_add(own.len());
                    self.enter_wide(base, args, up, entry.needs)?;
                    self.open_frame(Frame {
                        pc: low(pc),
                        below: own,
                        dst: dst.0,
                        env,
                    });
                    base = up;
                    pc = entry.pc.index();
                }
                // A tail call's callee takes over this frame, so a loop
                // written as tail recursion runs in constant space.
                Ins::TailCall {
                    to,
                    ref args,
                    needs,
                    own,
                } => {
                    let mut values = UNSET;
                    let values = self.pass_few(base, args, &mut values)?;
                    self.replace(base, own, needs, values)?;
                    self.run_as(Value::NIL)?;
                    pc = to.index();
                }
                Ins::TailCallWide {
                    to,
                    args,
                    needs,
                    own,
                } => {
                    self.replace_wide(base, args.args(&code.lists), own, needs)?;
                    self.run_as(Value::NIL)?;
                    pc = to.index();
                }
                Ins::TailCallSelf { to, ref args, own } => {
                    let mut values = UNSET;
                    let values = self.pass_few(base, args, &mut values)?;
                    self.rewrite(base, own, values)?;
                    pc = to.index();
                }
                Ins::TailCallSelfWide { to, args, own } => {
                    self.replace_wide(base, args.args(&code.lists), own, own)?;
                    pc = to.index();
                }
                Ins::TailCallSelfKeep {
                    to,
                    args,
                    keep,
                    own,
                } => {
                    let (args, keep) = (args.args(&code.lists), keep.regs(&code.lists));
                    self.again(base, args, own, keep)?;
                    pc = to.index();
                }
                // The callee's closure gets its own reference before this
                // frame's registers are released, one of which may be the
                // only other holder.
                Ins::TailCallValue {
                    callee,
                    ref args,
                    own,
                } => {
                    let (f, argc) = (self.get(base, callee.0), args.len());
                    let (entry, env) = self.rest.callee(f, argc)?;
                    let mut values = UNSET;
                    let values = self.pass_few(base, args, &mut values)?;
                    self.replace(base, own, entry.needs, values)?;
                    self.run_as(env)?;
                    pc = entry.pc.index();
                }
                Ins::TailCallValueWide { callee, args, own } => {
                    let args = args.args(&code.lists);
                    let (entry, env) = self.rest.callee(self.get(base, callee.0), args.len())?;
                    self.replace_wide(base, args, own, entry.needs)?;
                    self.run_as(env)?;
                    pc = entry.pc.index();
                }
                Ins::Closure {
                    dst,
                    func,
                    ref captures,
                } => self.closure(base, dst, func, captures.regs())?,
                Ins::ClosureWide {
                    dst,
                    func,
                    captures,
                } => self.closure(base, dst, func, captures.regs(&code.lists))?,
                Ins::GetCapture { dst, index } => {
                    let heap = &self.rest.heap;
                    let capture = match self.env().as_cell() {
                        Some(cell) if heap.kind(cell) == Some(Kind::Closure) => {
                            heap.capture(cell, index as usize)
                        }
                        _ => None,
                    };
                    let Some(v) = capture else {
                        return Err(self.no_capture(pc, index));
                    };
                    let v = self.rest.share(v)?;
                    self.put(base, dst, v)?;
                }
                Ins::GetSelf { dst, this } => {
                    let env = self.env();
                    let v = match env.view() {
                        View::Nil => Value::func(this),
                        View::Float(_)
                        | View::Int(_)
                        | View::Bool(_)
                        | View::Func(_)
                        | View::Cell(_)
                        | View::Nullary(_) => self.rest.share(env)?,
                    };
                    self.put(base, dst, v)?;
                }
                Ins::Jump { to } => pc = to.index(),
                Ins::JumpUnlessVariant { src, variant, to } => {
                    if !self.rest.is_variant(self.look(base, src), variant.get()) {
                        pc = to.index();
                    }
                }
                Ins::JumpUnlessInt { src, n, to } => {
                    if !self.rest.is_int(self.look(base, src), n) {
                        pc = to.index();
                    }
                }
                Ins::JumpUnlessFloat { src, x, to } => {
                    if !matches!(self.look(base, src).view(), View::Float(f) if f == x) {
                        pc = to.index();
                    }
                }
                Ins::JumpUnlessStr { src, text, to } => {
                    if !self.rest.is_text(self.look(base, src), text)? {
                        pc = to.index();
                    }
                }
                Ins::JumpUnlessBinary { src, bits, to } => {
                    if !self.rest.is_binary(self.look(base, src), bits)? {
                        pc = to.index();
                    }
                }
                Ins::Field { dst, src, index } => {
                    let heap = &self.rest.heap;
                    let field = match self.look(base, src).as_cell() {
                        Some(cell) if heap.kind(cell) == Some(Kind::Ctor) => {
                            heap.field(cell, usize::from(index))
                        }
                        _ => None,
                    };
                    let Some(field) = field else {
                        return Err(Stop::broken(Broken::NoField { index }));
                    };
                    let field = self.rest.share(field)?;
                    self.put(base, dst, field)?;
                }
                Ins::Tuple { dst, elements } => self.tuple(base, dst, elements.regs())?,
                Ins::TupleWide { dst, elements } => {
                    self.tuple(base, dst, elements.regs(&code.lists))?;
                }
                Ins::Element { dst, src, index } => {
                    let heap = &self.rest.heap;
                    let element = match self.look(base, src).as_cell() {
                        Some(cell) if heap.kind(cell) == Some(Kind::Tuple) => {
                            heap.element(cell, usize::from(index))
                        }
                        _ => None,
                    };
                    let Some(element) = element else {
                        return Err(Stop::broken(Broken::NoElement { index }));
                    };
                    let element = self.rest.share(element)?;
                    self.put(base, dst, element)?;
                }
                Ins::Array { dst, elements } => self.array(base, dst, elements.regs())?,
                Ins::ArrayWide { dst, elements } => {
                    self.array(base, dst, elements.regs(&code.lists))?;
                }
                Ins::Builtin {
                    dst,
                    intrinsic,
                    ref args,
                } => {
                    let (frame, depth) = (frame(&self.stack, base), self.frames.len());
                    let v = self.rest.builtin(intrinsic, frame, args.regs(), depth)?;
                    self.put(base, dst, v)?;
                }
                Ins::Equal { dst, a, b } => {
                    let (a, b) = (self.look(base, a), self.look(base, b));
                    let same = eq::equal(&self.rest.heap, a, b);
                    self.put(base, dst, Value::bool(same))?;
                }
                Ins::NotEqual { dst, a, b } => {
                    let (a, b) = (self.look(base, a), self.look(base, b));
                    let same = eq::equal(&self.rest.heap, a, b);
                    self.put(base, dst, Value::bool(!same))?;
                }
                Ins::Not { dst, a } => {
                    let b = match self.look(base, a).view() {
                        View::Bool(b) => b,
                        v @ (View::Float(_)
                        | View::Int(_)
                        | View::Nil
                        | View::Func(_)
                        | View::Cell(_)
                        | View::Nullary(_)) => {
                            return Err(Stop::broken(Broken::NotOnNonBool(v)));
                        }
                    };
                    self.put(base, dst, Value::bool(!b))?;
                }
                Ins::Range { dst, start, end } => {
                    let v = self
                        .rest
                        .range(self.look(base, start), self.look(base, end))?;
                    self.put(base, dst, v)?;
                }
                Ins::ArrayLen { dst, src } => {
                    let v = self.rest.array_len(self.look(base, src))?;
                    self.put(base, dst, v)?;
                }
                Ins::ArrayElem { dst, src, index } => {
                    let s = self.rest.seq(self.look(base, src))?;
                    let Some(v) = self.rest.element(s, usize::from(index))? else {
                        return Err(Stop::broken(Broken::NoArrayElement { index }));
                    };
                    self.put(base, dst, v)?;
                }
                Ins::ArrayDrop { dst, src, n } => self.array_drop(base, dst, src, n)?,
                Ins::ArrayPrepend { dst, rest, front } => {
                    self.array_push(base, dst, rest, front.regs(), End::Front)?;
                }
                Ins::ArrayPrependWide { dst, rest, front } => {
                    let front = front.regs(&code.lists);
                    self.array_push(base, dst, rest, front, End::Front)?;
                }
                Ins::ArrayAppend { dst, rest, back } => {
                    self.array_push(base, dst, rest, back.regs(), End::Back)?;
                }
                Ins::ArrayAppendWide { dst, rest, back } => {
                    let back = back.regs(&code.lists);
                    self.array_push(base, dst, rest, back, End::Back)?;
                }
                Ins::ArrayConcat { dst, a, b } => {
                    let v = self
                        .rest
                        .array_concat(self.look(base, a), self.look(base, b))?;
                    self.put(base, dst, v)?;
                }
                Ins::ArraySlice {
                    dst,
                    src,
                    start,
                    end,
                } => {
                    let of = self.look(base, src);
                    let v =
                        self.rest
                            .array_slice(of, self.look(base, start), self.look(base, end))?;
                    self.put(base, dst, v)?;
                }
                Ins::ArraySet {
                    dst,
                    src,
                    index,
                    value,
                    miss,
                } => match self.array_set(base, src, index, value)? {
                    Some(array) => self.hit(base, dst, array)?,
                    None => pc = miss.index(),
                },
                Ins::ArrayIndex { dst, src, index } => {
                    let found = self
                        .rest
                        .index(self.look(base, src), self.look(base, index))?;
                    let v = match found {
                        Some(v) => self.rest.some(v)?,
                        None => Value::nullary(code.abi.none),
                    };
                    self.put(base, dst, v)?;
                }
                Ins::ArrayIndexOr {
                    dst,
                    src,
                    index,
                    default,
                } => {
                    let found = self
                        .rest
                        .index(self.look(base, src), self.look(base, index))?;
                    let v = match found {
                        Some(v) => v,
                        None => self.share(base, default)?,
                    };
                    self.put(base, dst, v)?;
                }
                Ins::Bad { why } => return Err(code.why(why)),
                Ins::JumpIfFalse { cond, to } => match self.look(base, cond).view() {
                    View::Bool(true) => {}
                    View::Bool(false) => pc = to.index(),
                    v @ (View::Float(_)
                    | View::Int(_)
                    | View::Nil
                    | View::Func(_)
                    | View::Cell(_)
                    | View::Nullary(_)) => {
                        return Err(Stop::missing(Missing::Branch(v)));
                    }
                },
                Ins::MapSetOwned {
                    dst,
                    map,
                    key,
                    value,
                } => {
                    let m = self.rest.map(self.get(base, map.0))?;
                    let (k, v) = (self.look(base, key), self.look(base, value));
                    // The register's reference passes to the set.
                    self.take(base, map);
                    let cell = map::set_owned(&mut self.rest.heap, m, k, v).map_err(fault)?;
                    self.put(base, dst, Value::cell(cell))?;
                }
                Ins::MapFind { dst, map, key, to } => {
                    let m = self.rest.map(self.look(base, map))?;
                    match map::get(&self.rest.heap, m, self.look(base, key)) {
                        Some(v) => {
                            let v = self.rest.share(v)?;
                            self.hit(base, dst, v)?;
                        }
                        None => pc = to.index(),
                    }
                }
                Ins::MapDeleteOwned { dst, map, key } => {
                    let m = self.rest.map(self.get(base, map.0))?;
                    let k = self.look(base, key);
                    // The register's reference passes to the delete.
                    self.take(base, map);
                    let cell = map::delete_owned(&mut self.rest.heap, m, k).map_err(fault)?;
                    self.put(base, dst, Value::cell(cell))?;
                }
                Ins::JumpUnlessIntEq { a, b, to } => {
                    jump!(self, base, pc, to, int_holds(a, b), Compare::Eq);
                }
                Ins::JumpUnlessIntNe { a, b, to } => {
                    jump!(self, base, pc, to, int_holds(a, b), Compare::Ne);
                }
                Ins::JumpUnlessIntLt { a, b, to } => {
                    jump!(self, base, pc, to, int_holds(a, b), Compare::LT);
                }
                Ins::JumpUnlessIntLe { a, b, to } => {
                    jump!(self, base, pc, to, int_holds(a, b), Compare::LE);
                }
                Ins::JumpUnlessIntEqK { a, k, to } => {
                    jump!(self, base, pc, to, int_holds_k(a, k), Compare::Eq);
                }
                Ins::JumpUnlessIntNeK { a, k, to } => {
                    jump!(self, base, pc, to, int_holds_k(a, k), Compare::Ne);
                }
                Ins::JumpUnlessIntLtK { a, k, to } => {
                    jump!(self, base, pc, to, int_holds_k(a, k), Compare::LT);
                }
                Ins::JumpUnlessIntLeK { a, k, to } => {
                    jump!(self, base, pc, to, int_holds_k(a, k), Compare::LE);
                }
                Ins::JumpUnlessIntGtK { a, k, to } => {
                    jump!(self, base, pc, to, int_holds_k(a, k), Compare::GT);
                }
                Ins::JumpUnlessIntGeK { a, k, to } => {
                    jump!(self, base, pc, to, int_holds_k(a, k), Compare::GE);
                }
                Ins::JumpUnlessFloatLt { a, b, to } => {
                    jump!(self, base, pc, to, float_holds(a, b), Order::Lt);
                }
                Ins::JumpUnlessFloatLe { a, b, to } => {
                    jump!(self, base, pc, to, float_holds(a, b), Order::Le);
                }
                Ins::JumpUnlessFloatLtK { a, k, to } => {
                    jump!(self, base, pc, to, float_holds_k(a, k), Order::Lt);
                }
                Ins::JumpUnlessFloatLeK { a, k, to } => {
                    jump!(self, base, pc, to, float_holds_k(a, k), Order::Le);
                }
                Ins::JumpUnlessFloatGtK { a, k, to } => {
                    jump!(self, base, pc, to, float_holds_k(a, k), Order::Gt);
                }
                Ins::JumpUnlessFloatGeK { a, k, to } => {
                    jump!(self, base, pc, to, float_holds_k(a, k), Order::Ge);
                }
                // The frame is ending, so the result's register gives its
                // reference up instead of sharing it.
                Ins::Ret { src, own } => {
                    let v = self.take(base, src);
                    self.leave(base, own)?;
                    let Some(frame) = self.frames.pop() else {
                        return Err(Stop::broken(Broken::NoCaller));
                    };
                    self.rest.release(frame.env)?;
                    base = base.wrapping_sub(frame.below.len());
                    pc = frame.pc as usize;
                    self.set(base, frame.dst, v)?;
                }
            }
        }
    }

    // How a handler reaches a register. Each takes the operand as its
    // instruction declares it, so what a handler does to a register is what
    // `Ins::operands` says.

    #[inline(always)]
    fn look(&self, base: usize, r: Src<Reg>) -> Value {
        self.get(base, r.0)
    }

    /// The value in `r`, with one more reference.
    #[inline(always)]
    fn share(&mut self, base: usize, r: Shared<Reg>) -> Result<Value, DeadCell> {
        let v = self.get(base, r.0);
        self.rest.share(v)
    }

    /// The value in `r`, moved out. The register is emptied and gives up
    /// nothing, so its one reference passes to the caller.
    #[inline(always)]
    fn take(&mut self, base: usize, r: Taken<Reg>) -> Value {
        self.empty(base, r.0)
    }

    /// The value an argument passes, with the register's reference when the
    /// argument takes it and with one more otherwise.
    #[inline(always)]
    fn pass(&mut self, base: usize, a: Arg<Reg>) -> Result<Value, DeadCell> {
        if a.moved {
            Ok(self.empty(base, a.reg))
        } else {
            let v = self.get(base, a.reg);
            self.rest.share(v)
        }
    }

    /// Put `v` in `r`, whose old value is released. `v`'s reference passes to
    /// the register.
    #[inline(always)]
    fn put(&mut self, base: usize, r: Dst<Reg>, v: Value) -> Result<(), DeadCell> {
        self.set(base, r.0, v)
    }

    /// [`Self::put`], for an instruction that did not jump.
    #[inline(always)]
    fn hit(&mut self, base: usize, r: Hit<Reg>, v: Value) -> Result<(), DeadCell> {
        self.set(base, r.0, v)
    }

    #[inline(always)]
    fn drop(&mut self, base: usize, r: Dead<Reg>) -> Result<(), DeadCell> {
        self.set(base, r.0, Value::NIL)
    }

    /// Hollow the cell in `r` and leave it there with its one reference. If
    /// no constructor takes it, it is freed with the frame, like any other
    /// register. One that something else holds is given up.
    #[inline(always)]
    fn keep(&mut self, base: usize, r: Kept<Reg>) -> Result<(), DeadCell> {
        match self.get(base, r.0).as_cell() {
            Some(cell) if self.rest.heap.hollow(cell)? => Ok(()),
            _ => self.set(base, r.0, Value::NIL),
        }
    }

    #[inline(always)]
    fn get(&self, base: usize, r: Reg) -> Value {
        self.stack.get(at(base, r)).copied().unwrap_or(Value::NIL)
    }

    #[inline(always)]
    fn set(&mut self, base: usize, r: Reg, v: Value) -> Result<(), DeadCell> {
        match self.stack.get_mut(at(base, r)) {
            Some(slot) => {
                let old = std::mem::replace(slot, v);
                self.rest.release(old)
            }
            None => Ok(()),
        }
    }

    #[inline(always)]
    fn empty(&mut self, base: usize, r: Reg) -> Value {
        match self.stack.get_mut(at(base, r)) {
            Some(slot) => std::mem::replace(slot, Value::NIL),
            None => Value::NIL,
        }
    }

    /// Keep `frame` above the others.
    #[inline(always)]
    fn open_frame(&mut self, frame: Frame) {
        if self.frames.len() == self.frames.capacity() {
            self.more_frames();
        }
        self.frames.push(frame);
    }

    #[cold]
    #[inline(never)]
    fn more_frames(&mut self) {
        self.frames.reserve(1);
    }

    /// The closure the running frame runs as.
    #[inline(always)]
    fn env(&self) -> Value {
        self.frames.last().map_or(Value::NIL, |f| f.env)
    }

    /// Run as `env` from here on, and give up what the frame ran as.
    #[inline(always)]
    fn run_as(&mut self, env: Value) -> Result<(), DeadCell> {
        match self.frames.last_mut() {
            Some(frame) => {
                let old = std::mem::replace(&mut frame.env, env);
                self.rest.release(old)
            }
            None => self.rest.release(env),
        }
    }

    /// Give a callee that needs `needs` registers a fresh frame at `up`, above
    /// the caller's frame at `base`, and pass it `args`. The frames do not
    /// overlap, so each argument goes straight into its parameter. They are
    /// read in order, because a register a later argument moves may be one an
    /// earlier one still reads.
    #[inline(always)]
    fn enter(
        &mut self,
        base: usize,
        args: impl Iterator<Item = Arg<Reg>>,
        up: usize,
        needs: Room,
    ) -> Result<(), DeadCell> {
        self.open(up.wrapping_add(needs.len()));
        let mut param = up;
        for a in args {
            let v = self.pass(base, a)?;
            // Every register above the caller's is `Nil`, so there is nothing
            // here to release.
            if let Some(slot) = self.stack.get_mut(param) {
                *slot = v;
            }
            param = param.wrapping_add(1);
        }
        Ok(())
    }

    #[cold]
    #[inline(never)]
    fn enter_wide(
        &mut self,
        base: usize,
        args: &[Arg<Reg>],
        up: usize,
        needs: Room,
    ) -> Result<(), DeadCell> {
        self.enter(base, args.iter().copied(), up, needs)
    }

    /// Put what `args` pass at the start of `values`, and give that much of it.
    #[inline(always)]
    fn pass_few<'v, const N: usize>(
        &mut self,
        base: usize,
        args: &Args<Reg, N>,
        values: &'v mut [Value; N],
    ) -> Result<&'v [Value], DeadCell> {
        for (slot, a) in values.iter_mut().zip(args.iter()) {
            *slot = self.pass(base, a)?;
        }
        Ok(values.get(..args.len()).unwrap_or_default())
    }

    /// Put what `args` pass in `passing`, where the frame a tail call replaces
    /// does not reach.
    fn pass_all(&mut self, base: usize, args: &[Arg<Reg>]) -> Result<(), DeadCell> {
        self.passing.clear();
        for a in args {
            let v = self.pass(base, *a)?;
            self.passing.push(v);
        }
        Ok(())
    }

    /// End the frame of `own` registers at `base` and start one of `needs`
    /// there, with `values` in its first.
    #[inline(always)]
    fn replace(
        &mut self,
        base: usize,
        own: Room,
        needs: Room,
        values: &[Value],
    ) -> Result<(), DeadCell> {
        self.open(base.wrapping_add(needs.len()));
        self.rewrite(base, own.max(needs), values)
    }

    /// Release the `n` registers from `base` on, from the top down, leaving
    /// `values` in the first of them and `Nil` in the rest.
    // One loop for both. The compiler makes a call to `memcpy` of a loop that
    // only puts the values in, which costs more than the values.
    #[inline(always)]
    fn rewrite(&mut self, base: usize, n: Room, values: &[Value]) -> Result<(), DeadCell> {
        let mut k = n.len();
        if let Some(frame) = self.stack.get_mut(base..base.wrapping_add(k)) {
            for slot in frame.iter_mut().rev() {
                k = k.wrapping_sub(1);
                let v = values.get(k).copied().unwrap_or(Value::NIL);
                let old = std::mem::replace(slot, v);
                if let Some(cell) = old.as_cell() {
                    self.rest.heap.release(cell)?;
                }
            }
        }
        Ok(())
    }

    #[cold]
    #[inline(never)]
    fn replace_wide(
        &mut self,
        base: usize,
        args: &[Arg<Reg>],
        own: Room,
        needs: Room,
    ) -> Result<(), DeadCell> {
        self.pass_all(base, args)?;
        let values = std::mem::take(&mut self.passing);
        self.replace(base, own, needs, &values)?;
        self.passing = values;
        self.passing.clear();
        Ok(())
    }

    /// [`Self::replace`] of a frame by one of the same function, except that
    /// the registers in `keep` stay, so a loop overwrites the cell its last
    /// turn gave up. Each is the only reference to a cell holding nothing, and
    /// the register still owns it. So a cell the loop never takes is freed
    /// when the frame really ends.
    #[inline(never)]
    fn again(
        &mut self,
        base: usize,
        args: &[Arg<Reg>],
        own: Room,
        keep: &[Reg],
    ) -> Result<(), DeadCell> {
        self.pass_all(base, args)?;
        for r in (0..own.0).map(Reg) {
            if !keep.contains(&r) {
                self.set(base, r, Value::NIL)?;
            }
        }
        let passing = std::mem::take(&mut self.passing);
        for (r, v) in (0..).map(Reg).zip(&passing) {
            self.set(base, r, *v)?;
        }
        self.passing = passing;
        self.passing.clear();
        Ok(())
    }

    /// Release the `own` registers from `base` on, leaving each `Nil`. They
    /// are released from the top down, the order frames end in.
    #[inline(always)]
    fn leave(&mut self, base: usize, own: Room) -> Result<(), DeadCell> {
        if let Some(live) = self.stack.get_mut(base..base.wrapping_add(own.len())) {
            for slot in live.iter_mut().rev() {
                let old = std::mem::replace(slot, Value::NIL);
                if let Some(cell) = old.as_cell() {
                    self.rest.heap.release(cell)?;
                }
            }
        }
        Ok(())
    }

    /// Make the registers up to `end` there to use. The stack only grows the
    /// first time a frame reaches this far, and never shrinks.
    #[inline(always)]
    fn open(&mut self, end: usize) {
        if self.stack.len() < end {
            self.grow(end);
        }
    }

    #[cold]
    #[inline(never)]
    fn grow(&mut self, end: usize) {
        self.stack.resize(end, Value::NIL);
    }

    // What an instruction does that is more than a few lines, out of line so
    // that the loop keeps what it holds in the machine's registers.

    /// A new cell for `variant`.
    #[inline(always)]
    fn ctor(
        &mut self,
        base: usize,
        dst: Dst<Reg>,
        variant: Variant,
        fields: &Args<Reg, 6>,
    ) -> Result<(), Stop> {
        let mut values = UNSET;
        let values = self.pass_few(base, fields, &mut values)?;
        let cell = self.rest.heap.ctor(variant.get(), values).map_err(fault)?;
        Ok(self.put(base, dst, Value::cell(cell))?)
    }

    #[inline(never)]
    fn ctor_wide(
        &mut self,
        base: usize,
        dst: Dst<Reg>,
        variant: Variant,
        fields: &[Arg<Reg>],
    ) -> Result<(), Stop> {
        self.pass_all(base, fields)?;
        let cell = self.rest.heap.ctor(variant.get(), &self.passing);
        self.passing.clear();
        Ok(self.put(base, dst, Value::cell(cell.map_err(fault)?))?)
    }

    /// A new constructor holding `values`, over the cell in `reuse` when one
    /// fits there.
    #[inline(always)]
    fn over(
        &mut self,
        base: usize,
        dst: Dst<Reg>,
        variant: Variant,
        values: &[Value],
        reuse: Over<Reg>,
    ) -> Result<(), Stop> {
        // Move out the cell a `DropReuse` above hollowed and left in `reuse`,
        // so the register no longer holds it. Anything else there is a value
        // this frame is done with, so release it as the register would have.
        let parked = self.empty(base, reuse.0);
        let heap = &mut self.rest.heap;
        let fits = parked.as_cell();
        let cell = match fits.filter(|c| heap.fits_ctor(*c, values.len())) {
            Some(cell) => {
                heap.ctor_in(cell, variant.get(), values);
                cell
            }
            None => {
                self.rest.release(parked)?;
                let made = self.rest.heap.ctor(variant.get(), values);
                made.map_err(fault)?
            }
        };
        Ok(self.put(base, dst, Value::cell(cell))?)
    }

    #[inline(always)]
    fn over_few(
        &mut self,
        base: usize,
        dst: Dst<Reg>,
        variant: Variant,
        fields: &Args<Reg, 5>,
        reuse: Over<Reg>,
    ) -> Result<(), Stop> {
        let mut values = [Value::UNSET; 5];
        let values = self.pass_few(base, fields, &mut values)?;
        self.over(base, dst, variant, values, reuse)
    }

    #[inline(never)]
    fn over_wide(
        &mut self,
        base: usize,
        dst: Dst<Reg>,
        variant: Variant,
        fields: &[Arg<Reg>],
        reuse: Over<Reg>,
    ) -> Result<(), Stop> {
        self.pass_all(base, fields)?;
        let values = std::mem::take(&mut self.passing);
        self.over(base, dst, variant, &values, reuse)?;
        self.passing = values;
        self.passing.clear();
        Ok(())
    }

    #[inline(never)]
    fn bits(&mut self, base: usize, dst: Dst<Reg>, op: BitsOp, regs: &[Reg]) -> Result<(), Stop> {
        let v = self.rest.bits_op(op, frame(&self.stack, base), regs)?;
        Ok(self.put(base, dst, v)?)
    }

    /// A new string of what `${v}` shows, for a `v` that is none.
    #[inline(never)]
    fn make_text(&mut self, base: usize, dst: Dst<Reg>, v: Value) -> Result<(), Stop> {
        let v = self.rest.text_shown(v)?;
        Ok(self.put(base, dst, v)?)
    }

    #[inline(never)]
    fn concat(&mut self, base: usize, dst: Dst<Reg>, parts: &[Reg]) -> Result<(), Stop> {
        let v = self.rest.concat(frame(&self.stack, base), parts)?;
        Ok(self.put(base, dst, v)?)
    }

    #[inline(never)]
    fn closure(
        &mut self,
        base: usize,
        dst: Dst<Reg>,
        func: FuncIdx,
        captures: &[Reg],
    ) -> Result<(), Stop> {
        let values = self.shared(base, captures)?;
        let cell = self.rest.heap.closure(func, &values).map_err(fault)?;
        Ok(self.put(base, dst, Value::cell(cell))?)
    }

    #[inline(never)]
    fn tuple(&mut self, base: usize, dst: Dst<Reg>, elements: &[Reg]) -> Result<(), Stop> {
        let values = self.shared(base, elements)?;
        let cell = self.rest.heap.tuple(&values).map_err(fault)?;
        Ok(self.put(base, dst, Value::cell(cell))?)
    }

    #[inline(never)]
    fn array(&mut self, base: usize, dst: Dst<Reg>, elements: &[Reg]) -> Result<(), Stop> {
        let values = self.shared(base, elements)?;
        let cell = array::from_values(&mut self.rest.heap, &values).map_err(fault)?;
        Ok(self.put(base, dst, Value::cell(cell))?)
    }

    /// The values in `regs`, each with one more reference, for passing on.
    fn shared(&mut self, base: usize, regs: &[Reg]) -> Result<Vec<Value>, DeadCell> {
        let mut values = Vec::with_capacity(regs.len());
        for r in regs {
            values.push(self.share(base, Shared(*r))?);
        }
        Ok(values)
    }

    #[inline(never)]
    fn array_drop(
        &mut self,
        base: usize,
        dst: Dst<Reg>,
        src: Arg<Reg>,
        n: Src<Reg>,
    ) -> Result<(), Stop> {
        let s = self.rest.seq(self.get(base, src.reg))?;
        let n = match self.look(base, n).view() {
            View::Int(n) if n >= 0 => crate::word::to_usize(n.unsigned_abs()),
            v @ (View::Int(_)
            | View::Float(_)
            | View::Nil
            | View::Bool(_)
            | View::Func(_)
            | View::Cell(_)
            | View::Nullary(_)) => {
                return Err(Stop::broken(Broken::DropCount(v)));
            }
        };
        let cell = match s {
            // The register's reference passes to the skip.
            Seq::Tree(a) if src.moved => {
                self.empty(base, src.reg);
                array::skip_owned(&mut self.rest.heap, a, n)
            }
            Seq::Tree(a) => array::skip(&mut self.rest.heap, a, n),
            Seq::Range { start, end } => {
                let n = (n as u64).min(array::range_len(start, end));
                // `n` is at most `end - start`, so `start + n` is
                // within `start..=end` and the add is exact.
                array::range(&mut self.rest.heap, start.wrapping_add_unsigned(n), end)
            }
        };
        Ok(self.put(base, dst, Value::cell(cell.map_err(fault)?))?)
    }

    /// The array in `rest` with the values in `items` at `end` of it.
    #[inline(never)]
    fn array_push(
        &mut self,
        base: usize,
        dst: Dst<Reg>,
        rest: Arg<Reg>,
        items: &[Reg],
        end: End,
    ) -> Result<(), Stop> {
        let s = self.rest.seq(self.get(base, rest.reg))?;
        let a = self.rest.tree(s)?;
        let mut values: smallvec::SmallVec<[Value; 4]> =
            smallvec::SmallVec::with_capacity(items.len());
        for r in items {
            values.push(self.share(base, Shared(*r))?);
        }
        if rest.moved {
            let held = self.empty(base, rest.reg);
            self.rest.release(held)?;
        }
        let cell = self.rest.push_all(a, &values, end)?;
        Ok(self.put(base, dst, Value::cell(cell))?)
    }

    /// The array in `src` with the value in `value` at the index in `index`,
    /// holding its own reference, or `None` when the array has no such
    /// index. A `None` has read the registers and changed nothing. Otherwise
    /// the write holds one reference to the array, which is the register's
    /// when `src` gives it up, and so finds an array nothing else holds with
    /// a count of 1.
    #[inline(never)]
    fn array_set(
        &mut self,
        base: usize,
        src: Arg<Reg>,
        index: Src<Reg>,
        value: Arg<Reg>,
    ) -> Result<Option<Value>, Stop> {
        let s = self.rest.seq(self.get(base, src.reg))?;
        let at = self.rest.position(self.look(base, index))?;
        let Some(i) = at.filter(|i| (*i as u64) < s.len(&self.rest.heap)) else {
            return Ok(None);
        };
        let x = self.pass(base, value)?;
        let array = match s {
            Seq::Tree(a) => {
                if src.moved {
                    self.empty(base, src.reg);
                } else {
                    self.rest.heap.share(a)?;
                }
                a
            }
            // A range stores no elements to write over, so it is built as a
            // tree first, which nothing else holds.
            Seq::Range { .. } => {
                let a = self.rest.tree(s)?;
                if src.moved {
                    let range = self.empty(base, src.reg);
                    self.rest.release(range)?;
                }
                a
            }
        };
        let cell = array::set(&mut self.rest.heap, array, i, x).map_err(fault)?;
        Ok(Some(Value::cell(cell)))
    }

    // What an operator instruction does when it cannot answer in place. Each
    // of these is out of line and cold, and writes its answer itself, so that
    // the instruction's fast path is a test and a store that nothing else
    // joins.

    /// `x op y` into `dst`, for an Int instruction with an operand that is no
    /// small Int or an answer that is none.
    #[cold]
    #[inline(never)]
    fn int_slow(
        &mut self,
        base: usize,
        dst: Dst<Reg>,
        op: Arith,
        x: Value,
        y: Value,
    ) -> Result<(), Stop> {
        let v = int::op(&mut self.rest.heap, op, x, y)?;
        Ok(self.put(base, dst, v)?)
    }

    /// `-x` into `dst`, for an `x` that is no small Int or is the lowest one.
    #[cold]
    #[inline(never)]
    fn int_neg_slow(&mut self, base: usize, dst: Dst<Reg>, x: Value) -> Result<(), Stop> {
        let v = int::neg(&mut self.rest.heap, x)?;
        Ok(self.put(base, dst, v)?)
    }

    /// `x op y` into `dst`, for a Float instruction whose plain answer is no
    /// Float: an operand is none, or the answer is none or too large.
    #[cold]
    #[inline(never)]
    fn float_slow(
        &mut self,
        base: usize,
        dst: Dst<Reg>,
        op: Arith,
        x: Value,
        y: Value,
    ) -> Result<(), Stop> {
        let v = float::op(op, float::read(x)?, float::read(y)?);
        Ok(self.put(base, dst, v)?)
    }

    /// `-x` into `dst`. Minus a Float is always one, so `x` is not.
    #[cold]
    #[inline(never)]
    fn float_neg_slow(&mut self, base: usize, dst: Dst<Reg>, x: Value) -> Result<(), Stop> {
        let v = Value::float(-float::read(x)?);
        Ok(self.put(base, dst, v)?)
    }

    /// The Float nearest `x`, which is no small Int, into `dst`.
    #[cold]
    #[inline(never)]
    fn float_from_int_slow(&mut self, base: usize, dst: Dst<Reg>, x: Value) -> Result<(), Stop> {
        let v = int::to_float(&self.rest.heap, x)?;
        Ok(self.put(base, dst, v)?)
    }

    /// The stop for a run that went past the code's end at `pc`.
    #[cold]
    #[inline(never)]
    fn ran_past_end(&self, pc: usize) -> Stop {
        Stop::broken(Broken::RanPastEnd {
            func: self.name_before(pc),
        })
    }

    /// The stop for a read of capture `index`, by the instruction before `pc`.
    #[cold]
    #[inline(never)]
    fn no_capture(&self, pc: usize, index: u32) -> Stop {
        Stop::broken(Broken::NoCapture {
            func: self.name_before(pc),
            index,
        })
    }

    /// The name of the function that the instruction before `pc` is in.
    fn name_before(&self, pc: usize) -> String {
        let entry = self.rest.code.function_at(pc.saturating_sub(1));
        entry.map(|e| e.name.clone()).unwrap_or_default()
    }

    /// A finished run's last value as `println` shows it, or `None` for
    /// `Nil`. Gives up the value's reference.
    pub(crate) fn shown(&mut self, ended: Ended) -> Result<Option<String>, Stop> {
        let v = ended.last;
        if v.bits() == Value::NIL.bits() {
            return Ok(None);
        }
        let mut text = Vec::new();
        let shown = self.rest.show(v, &mut text);
        self.rest.release(v)?;
        shown?;
        Ok(Some(String::from_utf8_lossy(&text).into_owned()))
    }

    /// Give up everything a finished run still holds: its result, every
    /// global, `os.env`'s map and the constants made. Nothing can run after
    /// this.
    fn let_go(&mut self, ended: Ended) -> Result<(), DeadCell> {
        let rest = &mut self.rest;
        rest.release(ended.last)?;
        for v in std::mem::take(&mut rest.globals) {
            rest.release(v)?;
        }
        if let Some(env) = rest.env.take() {
            rest.heap.release(env)?;
        }
        rest.heap.let_go_of_constants()
    }

    /// Check the heap against every reference the run holds between two
    /// instructions: the registers, the globals, `os.env`'s map and the
    /// closure each frame runs as. `last` is a finished run's result.
    #[cold]
    #[inline(never)]
    fn audit(&self, last: Value) -> Result<(), Stop> {
        let held = self.stack.iter().chain(&self.passing);
        let roots = held
            .chain(&self.rest.globals)
            .copied()
            .chain(self.frames.iter().map(|f| f.env))
            .chain(std::iter::once(last))
            .chain(self.rest.env.map(Value::cell));
        Ok(self.rest.heap.audit(roots)?)
    }

    /// What a finished run of `counted` counted. This is the machine
    /// [`Self::counting`] made for it. The run reached its end, so every
    /// block that was entered ran to its last instruction, and each
    /// instruction in it ran as often as the block did.
    pub(crate) fn counts(&self, counted: &Counted, _: &Ended) -> Result<Counts, Stop> {
        let mut ran = [0u64; Op::ALL.len()];
        for (block, times) in counted.blocks.iter().zip(&self.rest.ran) {
            for op in block {
                if let Some(n) = ran.get_mut(*op as usize) {
                    *n = n.saturating_add(*times);
                }
            }
        }
        // These are no part of the program. A `Count` is the counting itself,
        // and an `Audit` and the `Halt` are in no block.
        let ops = Op::ALL
            .iter()
            .zip(ran)
            .filter(|(op, _)| !matches!(op, Op::Count | Op::Audit | Op::Halt));
        let heap = &self.rest.heap;
        Ok(Counts {
            ops: ops.map(|(op, n)| (op.name(), n)).collect(),
            cells_made: heap.made(),
            cells_reused: heap.reused(),
            cells_live: heap.live_cells()?,
        })
    }

    /// End a finished run as `audit` asks. An audited run checks its heap,
    /// lets go of everything it still holds, and checks again. A cell still
    /// live after that has a count that is right and nothing outside the heap
    /// that leads to it.
    pub(crate) fn finish(&mut self, ended: Ended, audit: Audit) -> Result<(), Stop> {
        match audit {
            Audit::Never => Ok(()),
            Audit::AtEnd | Audit::EachInstruction => {
                self.audit(ended.last)?;
                self.let_go(ended)?;
                self.rest.let_go_of_handles();
                self.audit(Value::NIL)?;
                match self.rest.heap.live_cells()? {
                    0 => Ok(()),
                    cells => Err(Stop::broken(Broken::CellsLeft { cells })),
                }
            }
        }
    }
}

/// Room for what an instruction passes, before any of it is there. It is all
/// zero bits, which is cheaper to write than `Nil`s.
const UNSET: [Value; 6] = [Value::UNSET; 6];

/// The registers of the frame at `base`, and whatever is above them.
#[inline(always)]
fn frame(stack: &[Value], base: usize) -> &[Value] {
    stack.get(base..).unwrap_or_default()
}

/// Where a global is, when it has a place.
fn global(slot: GlobalSlot) -> Option<usize> {
    usize::try_from(slot.0).ok()
}

/// Where register `r` of the frame at `base` sits on the stack. `base` is no
/// more than the stack's length, so the sum is exact.
#[inline(always)]
fn at(base: usize, r: Reg) -> usize {
    base.wrapping_add(usize::from(r.0))
}

/// `pc` as a frame keeps it. The loader gives every instruction a number of 32
/// bits (`code::emit`), so this is exact.
#[inline(always)]
fn low(pc: usize) -> u32 {
    crate::word::low_u32(pc as u64)
}

#[cfg(test)]
mod tests;
