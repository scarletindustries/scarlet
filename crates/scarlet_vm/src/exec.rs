//! The interpreter, which runs loaded instructions.
//!
//! Frames live on a stack the VM owns, not on Rust's, so a deep recursion in
//! a Scarlet program uses memory and never overflows anything. A frame is
//! registers on that stack and nothing else (`frame.rs`). The loop holds the
//! running frame's registers as one array, where it is on the stack, and the
//! place in the code. It cannot make the stack longer, so a call that needs
//! more of it leaves the loop, which starts again at that call.
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
use crate::bigint;
use crate::code::{
    Arg, Args, BitsOp, Code, Counted, Dead, Dst, Entry, Ins, Op, Over, Reg, Room, Shared, Src,
    Variant,
};
use crate::counts::Counts;
use crate::eq;
use crate::float;
use crate::frame::{self, CLOSURE, Link, RESULT, WINDOW, Window, window};
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
    ($win:ident, $rest:ident, $dst:ident, $a:ident, $b:ident, $op:expr) => {{
        let (x, y) = ($win.look($a), $win.look($b));
        match x.both_small(y).and_then(|(x, y)| int::fast($op, x, y)) {
            Some(v) => $win.put(&mut $rest.heap, $dst, v)?,
            None => int_slow($win.lend(), $rest, $dst, $op, x, y)?,
        }
    }};
}

macro_rules! int_k {
    ($win:ident, $rest:ident, $dst:ident, $a:ident, $k:ident, $op:expr) => {{
        let x = $win.look($a);
        match x.as_small().and_then(|x| int::fast($op, x, $k)) {
            Some(v) => $win.put(&mut $rest.heap, $dst, v)?,
            None => int_slow($win.lend(), $rest, $dst, $op, x, $k.value())?,
        }
    }};
}

macro_rules! int_k_first {
    ($win:ident, $rest:ident, $dst:ident, $k:ident, $a:ident, $op:expr) => {{
        let y = $win.look($a);
        match y.as_small().and_then(|y| int::fast($op, $k, y)) {
            Some(v) => $win.put(&mut $rest.heap, $dst, v)?,
            None => int_slow($win.lend(), $rest, $dst, $op, $k.value(), y)?,
        }
    }};
}

macro_rules! int_holds {
    ($win:ident, $rest:ident, $a:ident, $b:ident, $op:expr) => {{
        let (x, y) = ($win.look($a), $win.look($b));
        match x.both_small(y) {
            Some((x, y)) => $op.holds(x.cmp(&y)),
            None => int::compare(&$rest.heap, $op, x, y)?,
        }
    }};
}

macro_rules! int_holds_k {
    ($win:ident, $rest:ident, $a:ident, $k:ident, $op:expr) => {{
        let x = $win.look($a);
        match x.as_small() {
            Some(x) => $op.holds(x.cmp(&$k)),
            None => int::compare(&$rest.heap, $op, x, $k.value())?,
        }
    }};
}

macro_rules! float {
    ($win:ident, $rest:ident, $dst:ident, $a:ident, $b:ident, $op:expr) => {{
        let (x, y) = ($win.look($a), $win.look($b));
        match float::fast($op, x, y) {
            Some(v) => $win.put(&mut $rest.heap, $dst, v)?,
            None => float_slow($win.lend(), $rest, $dst, $op, x, y)?,
        }
    }};
}

macro_rules! float_k {
    ($win:ident, $rest:ident, $dst:ident, $a:ident, $k:ident, $op:expr) => {{
        let x = $win.look($a);
        match float::fast($op, x, $k.value()) {
            Some(v) => $win.put(&mut $rest.heap, $dst, v)?,
            None => float_slow($win.lend(), $rest, $dst, $op, x, $k.value())?,
        }
    }};
}

macro_rules! float_k_first {
    ($win:ident, $rest:ident, $dst:ident, $k:ident, $a:ident, $op:expr) => {{
        let y = $win.look($a);
        match float::fast($op, $k.value(), y) {
            Some(v) => $win.put(&mut $rest.heap, $dst, v)?,
            None => float_slow($win.lend(), $rest, $dst, $op, $k.value(), y)?,
        }
    }};
}

macro_rules! float_holds {
    ($win:ident, $rest:ident, $a:ident, $b:ident, $op:expr) => {{
        let (x, y) = ($win.look($a), $win.look($b));
        match float::fast_compare($op, x, y) {
            Some(holds) => holds,
            None => float::slow_compare($op, x, y)?,
        }
    }};
}

macro_rules! float_holds_k {
    ($win:ident, $rest:ident, $a:ident, $k:ident, $op:expr) => {{
        let x = $win.look($a);
        match float::fast_compare($op, x, $k.value()) {
            Some(holds) => holds,
            None => float::slow_compare($op, x, $k.value())?,
        }
    }};
}

macro_rules! store_bool {
    ($win:ident, $rest:ident, $dst:ident, $holds:ident($a:ident, $b:ident), $op:expr) => {{
        let holds = $holds!($win, $rest, $a, $b, $op);
        $win.put(&mut $rest.heap, $dst, Value::bool(holds))?
    }};
}

macro_rules! jump {
    ($win:ident, $rest:ident, $pc:ident, $to:ident, $holds:ident($a:ident, $b:ident), $op:expr) => {
        if !$holds!($win, $rest, $a, $b, $op) {
            $pc = $to.index();
        }
    };
}

pub(crate) struct Machine<'c, 'h> {
    /// Every frame's registers.
    stack: Vec<Value>,
    /// How many registers the stack may have. It is [`MAX_STACK`], but in
    /// tests.
    stack_limit: usize,
    rest: Rest<'c, 'h>,
}

/// How many registers a stack may have, which is 32 GiB of them. Where a frame
/// starts is a number of 32 bits. A callee starts no more than a window above
/// its caller, whose window is on the stack, so that number is exact for it too.
const MAX_STACK: usize = u32::MAX as usize;

/// An instruction, and the frame it is to run in.
#[derive(Clone, Copy)]
struct At {
    pc: usize,
    base: u32,
}

/// Why the loop came back.
enum Pause {
    /// The run ended, with this value.
    Done(Value),
    /// The call in front of `after` needs a window past the stack's end, for
    /// a callee that starts `own` registers above it. It has done nothing yet,
    /// so it can run again.
    NeedStack { after: At, own: Room },
}

/// What [`Machine::grow`] did to the stack.
#[derive(Debug, PartialEq)]
enum Grown {
    Longer,
    /// Nothing. It was long enough.
    HadRoom,
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
            stack_limit: MAX_STACK,
            rest: Rest {
                code,
                host,
                globals: vec![Value::NIL; code.globals as usize],
                env: None,
                ran: Vec::new(),
                passing: Vec::new(),
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
        let at = At {
            pc: entry.pc.index(),
            base: 0,
        };
        // Nothing called this frame, so it returns to the code's first
        // instruction, the `Halt`, which takes the result from `RESULT`.
        let link = Link::new(0, Room(0), Dst(RESULT));
        self.grow(0)?;
        let mut win = window(&mut self.stack, 0).ok_or_else(lost_window)?;
        win.start(link);
        win.receive(args);
        self.go_on(at)
    }

    /// Run from `at` until the frame at the bottom of the stack returns.
    fn go_on(&mut self, mut at: At) -> Result<Value, Stop> {
        loop {
            match steps(&mut self.stack, &mut self.rest, at)? {
                Pause::Done(v) => return Ok(v),
                Pause::NeedStack { after, own } => {
                    match self.grow(after.base.wrapping_add(own.below()))? {
                        Grown::Longer => {}
                        // A call that has its room and asks for more would ask
                        // for ever.
                        Grown::HadRoom => return Err(lost_window()),
                    }
                    at = At {
                        pc: after.pc.wrapping_sub(1),
                        base: after.base,
                    };
                }
            }
        }
    }

    /// Make the stack long enough for a window at `base`, and a little longer
    /// than that, so that a deep recursion asks seldom.
    #[cold]
    #[inline(never)]
    fn grow(&mut self, base: u32) -> Result<Grown, Stop> {
        let len = self.stack.len();
        let needs = (base as usize).saturating_add(WINDOW);
        if needs > self.stack_limit {
            return Err(Stop::StackFull);
        }
        if needs <= len {
            return Ok(Grown::HadRoom);
        }
        let to = needs.saturating_add(needs / 16).min(self.stack_limit);
        let more = to.saturating_sub(len);
        // The first asks for as much again as there is, and the system may
        // have less than that and still enough.
        let had = self.stack.try_reserve(more);
        had.or_else(|_| self.stack.try_reserve_exact(more))
            .map_err(|_| Stop::StackFull)?;
        self.stack.resize(to, Value::NIL);
        Ok(Grown::Longer)
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
                check(&self.stack, &self.rest, ended.last)?;
                self.let_go(ended)?;
                self.rest.let_go_of_handles();
                check(&self.stack, &self.rest, Value::NIL)?;
                match self.rest.heap.live_cells()? {
                    0 => Ok(()),
                    cells => Err(Stop::broken(Broken::CellsLeft { cells })),
                }
            }
        }
    }
}

/// Run from `at`, until the run ends or a call needs more stack.
///
/// The stack cannot grow while this runs, so its length and where it is are
/// the same throughout, and `win` is the running frame's part of it.
#[inline(never)]
fn steps(stack: &mut [Value], rest: &mut Rest<'_, '_>, at: At) -> Result<Pause, Stop> {
    let code = rest.code;
    let ins = code.ins.as_slice();
    let len = stack.len();
    let At { mut pc, mut base } = at;
    let mut win = window(stack, base).ok_or_else(lost_window)?;

    // Where the callee's frame starts, for a call from a frame of `$own`
    // registers. Every call asks this first, before it has shared or written
    // anything, so one with no room can run again from its start.
    macro_rules! room {
        ($own:expr) => {{
            let up = base.wrapping_add($own.below());
            if !fits(len, up) {
                return Ok(Pause::NeedStack {
                    after: At { pc, base },
                    own: $own,
                });
            }
            up
        }};
    }

    // Make the frame at `$up` the running one, called from a frame of `$own`
    // registers and with its result for `$dst`.
    macro_rules! open {
        ($up:expr, $own:expr, $dst:expr) => {{
            base = $up;
            win = window(stack, base).ok_or_else(lost_window)?;
            win.start(Link::new(pc, $own, $dst));
        }};
    }

    loop {
        let Some(i) = ins.get(pc) else {
            return Err(ran_past_end(code, pc));
        };
        // `pc` names an instruction, so one past it is exact.
        pc = pc.wrapping_add(1);
        // Matched in place: a copy of the instruction would have every
        // field of it read before the jump that finds its handler.
        match *i {
            Ins::Halt { src } => return Ok(Pause::Done(win.take(src))),
            Ins::Count { block } => {
                if let Some(n) = rest.ran.get_mut(block.0 as usize) {
                    *n = n.wrapping_add(1);
                }
            }
            Ins::Audit => {
                check(stack, rest, Value::NIL)?;
                win = window(stack, base).ok_or_else(lost_window)?;
            }
            Ins::Const { dst, value } => win.put(&mut rest.heap, dst, value)?,
            Ins::Str { dst, text } => {
                let v = rest.text_const(text)?;
                win.put(&mut rest.heap, dst, v)?;
            }
            Ins::BinaryConst { dst, bits } => {
                let v = rest.binary_const(bits)?;
                win.put(&mut rest.heap, dst, v)?;
            }
            Ins::Bits { dst, op, args } => bits(win.lend(), rest, dst, op, args.regs())?,
            Ins::BitsWide { dst, op, args } => {
                bits(win.lend(), rest, dst, op, args.regs(&code.lists))?;
            }
            Ins::BigInt { dst, n, id } => {
                let v = rest.big_const(id, n)?;
                win.put(&mut rest.heap, dst, v)?;
            }
            // Only a cell has a reference to hand over or to add, so
            // any other value is copied and its register left alone.
            Ins::Move { dst, src } => {
                let v = win.get(src.reg);
                if let Some(cell) = v.as_cell() {
                    if src.moved {
                        win.empty(src.reg);
                    } else {
                        rest.heap.share(cell)?;
                    }
                }
                win.put(&mut rest.heap, dst, v)?;
            }
            Ins::GetGlobal { dst, slot } => {
                let v = global(slot)
                    .and_then(|i| rest.globals.get(i))
                    .copied()
                    .unwrap_or(Value::NIL);
                let v = rest.share(v)?;
                win.put(&mut rest.heap, dst, v)?;
            }
            Ins::SetGlobal { slot, src } => {
                let v = win.share(&mut rest.heap, src)?;
                if let Some(g) = global(slot).and_then(|i| rest.globals.get_mut(i)) {
                    let old = std::mem::replace(g, v);
                    rest.release(old)?;
                }
            }
            Ins::IntAdd { dst, a, b } => int!(win, rest, dst, a, b, Arith::Add),
            Ins::IntSub { dst, a, b } => int!(win, rest, dst, a, b, Arith::Sub),
            Ins::IntMul { dst, a, b } => int!(win, rest, dst, a, b, Arith::Mul),
            Ins::IntDiv { dst, a, b } => int!(win, rest, dst, a, b, Arith::Div),
            Ins::IntRem { dst, a, b } => int!(win, rest, dst, a, b, Arith::Rem),
            Ins::IntAddK { dst, a, k } => int_k!(win, rest, dst, a, k, Arith::Add),
            Ins::IntSubK { dst, a, k } => int_k!(win, rest, dst, a, k, Arith::Sub),
            Ins::IntMulK { dst, a, k } => int_k!(win, rest, dst, a, k, Arith::Mul),
            Ins::IntDivK { dst, a, k } => int_k!(win, rest, dst, a, k, Arith::Div),
            Ins::IntRemK { dst, a, k } => int_k!(win, rest, dst, a, k, Arith::Rem),
            Ins::IntRSubK { dst, k, a } => int_k_first!(win, rest, dst, k, a, Arith::Sub),
            Ins::IntEq { dst, a, b } => {
                store_bool!(win, rest, dst, int_holds(a, b), Compare::Eq);
            }
            Ins::IntNe { dst, a, b } => {
                store_bool!(win, rest, dst, int_holds(a, b), Compare::Ne);
            }
            Ins::IntLt { dst, a, b } => {
                store_bool!(win, rest, dst, int_holds(a, b), Compare::LT);
            }
            Ins::IntLe { dst, a, b } => {
                store_bool!(win, rest, dst, int_holds(a, b), Compare::LE);
            }
            Ins::IntEqK { dst, a, k } => {
                store_bool!(win, rest, dst, int_holds_k(a, k), Compare::Eq);
            }
            Ins::IntNeK { dst, a, k } => {
                store_bool!(win, rest, dst, int_holds_k(a, k), Compare::Ne);
            }
            Ins::IntLtK { dst, a, k } => {
                store_bool!(win, rest, dst, int_holds_k(a, k), Compare::LT);
            }
            Ins::IntLeK { dst, a, k } => {
                store_bool!(win, rest, dst, int_holds_k(a, k), Compare::LE);
            }
            Ins::IntGtK { dst, a, k } => {
                store_bool!(win, rest, dst, int_holds_k(a, k), Compare::GT);
            }
            Ins::IntGeK { dst, a, k } => {
                store_bool!(win, rest, dst, int_holds_k(a, k), Compare::GE);
            }
            Ins::IntNeg { dst, a } => {
                let x = win.look(a);
                match x.as_small().and_then(int::fast_neg) {
                    Some(v) => win.put(&mut rest.heap, dst, v)?,
                    None => int_neg_slow(win.lend(), rest, dst, x)?,
                }
            }
            Ins::FloatAdd { dst, a, b } => float!(win, rest, dst, a, b, Arith::Add),
            Ins::FloatSub { dst, a, b } => float!(win, rest, dst, a, b, Arith::Sub),
            Ins::FloatMul { dst, a, b } => float!(win, rest, dst, a, b, Arith::Mul),
            Ins::FloatDiv { dst, a, b } => float!(win, rest, dst, a, b, Arith::Div),
            Ins::FloatRem { dst, a, b } => float!(win, rest, dst, a, b, Arith::Rem),
            Ins::FloatAddK { dst, a, k } => float_k!(win, rest, dst, a, k, Arith::Add),
            Ins::FloatSubK { dst, a, k } => float_k!(win, rest, dst, a, k, Arith::Sub),
            Ins::FloatMulK { dst, a, k } => float_k!(win, rest, dst, a, k, Arith::Mul),
            Ins::FloatDivK { dst, a, k } => float_k!(win, rest, dst, a, k, Arith::Div),
            Ins::FloatRSubK { dst, k, a } => {
                float_k_first!(win, rest, dst, k, a, Arith::Sub);
            }
            Ins::FloatRDivK { dst, k, a } => {
                float_k_first!(win, rest, dst, k, a, Arith::Div);
            }
            Ins::FloatLt { dst, a, b } => {
                store_bool!(win, rest, dst, float_holds(a, b), Order::Lt);
            }
            Ins::FloatLe { dst, a, b } => {
                store_bool!(win, rest, dst, float_holds(a, b), Order::Le);
            }
            Ins::FloatLtK { dst, a, k } => {
                store_bool!(win, rest, dst, float_holds_k(a, k), Order::Lt);
            }
            Ins::FloatLeK { dst, a, k } => {
                store_bool!(win, rest, dst, float_holds_k(a, k), Order::Le);
            }
            Ins::FloatGtK { dst, a, k } => {
                store_bool!(win, rest, dst, float_holds_k(a, k), Order::Gt);
            }
            Ins::FloatGeK { dst, a, k } => {
                store_bool!(win, rest, dst, float_holds_k(a, k), Order::Ge);
            }
            Ins::FloatNeg { dst, a } => {
                let x = win.look(a);
                match float::fast_neg(x) {
                    Some(v) => win.put(&mut rest.heap, dst, v)?,
                    None => float_neg_slow(win.lend(), rest, dst, x)?,
                }
            }
            Ins::FloatFromInt { dst, a } => {
                let x = win.look(a);
                match x.small_int() {
                    Some(n) => win.put(&mut rest.heap, dst, Value::float(n as f64))?,
                    None => float_from_int_slow(win.lend(), rest, dst, x)?,
                }
            }
            Ins::Num { dst, op, a, b } => {
                let (a, b) = (win.look(a), win.look(b));
                let v = rest.num(op, a, b)?;
                win.put(&mut rest.heap, dst, v)?;
            }
            Ins::Neg { dst, a } => {
                let a = win.look(a);
                let v = rest.neg(a)?;
                win.put(&mut rest.heap, dst, v)?;
            }
            Ins::Print { dst, a, newline } => {
                rest.print(win.look(a), newline.0)?;
                win.put(&mut rest.heap, dst, Value::NIL)?;
            }
            Ins::ToString { dst, a } => {
                let v = win.get(a.0);
                if rest.is_string(v) {
                    let v = rest.share(v)?;
                    win.put(&mut rest.heap, dst, v)?;
                } else {
                    make_text(win.lend(), rest, dst, v)?;
                }
            }
            Ins::Concat { dst, parts } => concat(win.lend(), rest, dst, parts.regs())?,
            Ins::ConcatWide { dst, parts } => {
                concat(win.lend(), rest, dst, parts.regs(&code.lists))?;
            }
            Ins::Ctor {
                dst,
                variant,
                ref fields,
            } => ctor(win.lend(), rest, dst, variant, fields)?,
            Ins::CtorWide {
                dst,
                variant,
                fields,
            } => ctor_wide(win.lend(), rest, dst, variant, fields.args(&code.lists))?,
            Ins::CtorOver {
                dst,
                variant,
                ref fields,
                reuse,
            } => over_few(win.lend(), rest, dst, variant, fields, reuse)?,
            Ins::CtorOverWide {
                dst,
                variant,
                fields,
                reuse,
            } => over_wide(
                win.lend(),
                rest,
                dst,
                variant,
                fields.args(&code.lists),
                reuse,
            )?,
            Ins::Drop { reg } => win.drop(&mut rest.heap, reg)?,
            Ins::DropReuse { reg } => win.keep(&mut rest.heap, reg)?,
            Ins::Call0 { dst, own, to } => {
                let up = room!(own);
                open!(up, own, dst);
                pc = to.index();
            }
            Ins::Call1 { dst, own, to, a } => {
                let up = room!(own);
                let a = win.take(a);
                open!(up, own, dst);
                win.receive(&[a]);
                pc = to.index();
            }
            Ins::Call2 { dst, own, to, a, b } => {
                let up = room!(own);
                let a = win.take(a);
                let b = win.take(b);
                open!(up, own, dst);
                win.receive(&[a, b]);
                pc = to.index();
            }
            Ins::Call3 {
                dst,
                own,
                to,
                a,
                b,
                c,
            } => {
                let up = room!(own);
                let a = win.take(a);
                let b = win.take(b);
                let c = win.take(c);
                open!(up, own, dst);
                win.receive(&[a, b, c]);
                pc = to.index();
            }
            Ins::Call1Share { dst, own, to, a } => {
                let up = room!(own);
                let a = win.pass(&mut rest.heap, a)?;
                open!(up, own, dst);
                win.receive(&[a]);
                pc = to.index();
            }
            Ins::Call2Share { dst, own, to, a, b } => {
                let up = room!(own);
                let a = win.pass(&mut rest.heap, a)?;
                let b = win.pass(&mut rest.heap, b)?;
                open!(up, own, dst);
                win.receive(&[a, b]);
                pc = to.index();
            }
            Ins::Call3Share {
                dst,
                own,
                to,
                a,
                b,
                c,
            } => {
                let up = room!(own);
                let a = win.pass(&mut rest.heap, a)?;
                let b = win.pass(&mut rest.heap, b)?;
                let c = win.pass(&mut rest.heap, c)?;
                open!(up, own, dst);
                win.receive(&[a, b, c]);
                pc = to.index();
            }
            Ins::Call {
                dst,
                own,
                to,
                ref args,
            } => {
                let up = room!(own);
                let mut values = UNSET;
                let values = pass_few(win.lend(), rest, args, &mut values)?;
                open!(up, own, dst);
                win.receive(values);
                pc = to.index();
            }
            Ins::CallWide { dst, own, to, args } => {
                let up = room!(own);
                pass_all(win.lend(), rest, args.args(&code.lists))?;
                open!(up, own, dst);
                receive_all(win.lend(), rest);
                pc = to.index();
            }
            Ins::CallSelf {
                dst,
                own,
                to,
                ref args,
            } => {
                let up = room!(own);
                let closure = rest.share(win.closure())?;
                let mut values = UNSET;
                let values = pass_few(win.lend(), rest, args, &mut values)?;
                open!(up, own, dst);
                win.run_as(closure);
                win.receive(values);
                pc = to.index();
            }
            Ins::CallSelfWide { dst, own, to, args } => {
                let up = room!(own);
                let closure = rest.share(win.closure())?;
                pass_all(win.lend(), rest, args.args(&code.lists))?;
                open!(up, own, dst);
                win.run_as(closure);
                receive_all(win.lend(), rest);
                pc = to.index();
            }
            Ins::CallValue {
                dst,
                callee,
                own,
                ref args,
            } => {
                let up = room!(own);
                let (entry, closure) = rest.callee(win.get(callee.0), args.len())?;
                let mut values = UNSET;
                let values = pass_few(win.lend(), rest, args, &mut values)?;
                open!(up, own, dst);
                win.run_as(closure);
                win.receive(values);
                pc = entry.pc.index();
            }
            Ins::CallValueWide {
                dst,
                callee,
                own,
                args,
            } => {
                let up = room!(own);
                let args = args.args(&code.lists);
                let (entry, closure) = rest.callee(win.get(callee.0), args.len())?;
                pass_all(win.lend(), rest, args)?;
                open!(up, own, dst);
                win.run_as(closure);
                receive_all(win.lend(), rest);
                pc = entry.pc.index();
            }
            // A tail call's callee takes over this frame, so a loop
            // written as tail recursion runs in constant space.
            Ins::TailCall { to, ref args, own } => {
                let mut values = UNSET;
                let values = pass_few(win.lend(), rest, args, &mut values)?;
                win.rewrite(&mut rest.heap, own, values)?;
                win.drop(&mut rest.heap, Dead(CLOSURE))?;
                pc = to.index();
            }
            Ins::TailCallWide { to, args, own } => {
                rewrite_wide(win.lend(), rest, own, args.args(&code.lists))?;
                win.drop(&mut rest.heap, Dead(CLOSURE))?;
                pc = to.index();
            }
            Ins::TailCallSelf { to, ref args, own } => {
                let mut values = UNSET;
                let values = pass_few(win.lend(), rest, args, &mut values)?;
                win.rewrite(&mut rest.heap, own, values)?;
                pc = to.index();
            }
            Ins::TailCallSelfWide { to, args, own } => {
                rewrite_wide(win.lend(), rest, own, args.args(&code.lists))?;
                pc = to.index();
            }
            Ins::TailCallSelfKeep {
                to,
                args,
                keep,
                own,
            } => {
                let (args, keep) = (args.args(&code.lists), keep.regs(&code.lists));
                rewrite_keeping(win.lend(), rest, own, args, keep)?;
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
                let (entry, closure) = rest.callee(win.get(callee.0), args.len())?;
                let mut values = UNSET;
                let values = pass_few(win.lend(), rest, args, &mut values)?;
                win.rewrite(&mut rest.heap, own, values)?;
                win.set(&mut rest.heap, CLOSURE, closure)?;
                pc = entry.pc.index();
            }
            Ins::TailCallValueWide { callee, args, own } => {
                let args = args.args(&code.lists);
                let (entry, closure) = rest.callee(win.get(callee.0), args.len())?;
                rewrite_wide(win.lend(), rest, own, args)?;
                win.set(&mut rest.heap, CLOSURE, closure)?;
                pc = entry.pc.index();
            }
            Ins::Closure {
                dst,
                func,
                ref captures,
            } => closure(win.lend(), rest, dst, func, captures.regs())?,
            Ins::ClosureWide {
                dst,
                func,
                captures,
            } => closure(win.lend(), rest, dst, func, captures.regs(&code.lists))?,
            Ins::GetCapture { dst, index } => {
                let heap = &rest.heap;
                let capture = match win.closure().as_cell() {
                    Some(cell) if heap.kind(cell) == Some(Kind::Closure) => {
                        heap.capture(cell, index as usize)
                    }
                    _ => None,
                };
                let Some(v) = capture else {
                    return Err(no_capture(code, pc, index));
                };
                let v = rest.share(v)?;
                win.put(&mut rest.heap, dst, v)?;
            }
            // A frame that runs as a plain function has no closure cell.
            Ins::GetSelf { dst, this } => {
                let v = match win.closure().as_cell() {
                    Some(_) => rest.share(win.closure())?,
                    None => Value::func(this),
                };
                win.put(&mut rest.heap, dst, v)?;
            }
            Ins::Jump { to } => pc = to.index(),
            Ins::JumpUnlessVariant { src, variant, to } => {
                if !rest.is_variant(win.look(src), variant.get()) {
                    pc = to.index();
                }
            }
            Ins::JumpUnlessInt { src, n, to } => {
                if !rest.is_int(win.look(src), n) {
                    pc = to.index();
                }
            }
            Ins::JumpUnlessFloat { src, x, to } => {
                if !matches!(win.look(src).view(), View::Float(f) if f == x) {
                    pc = to.index();
                }
            }
            Ins::JumpUnlessStr { src, text, to } => {
                if !rest.is_text(win.look(src), text)? {
                    pc = to.index();
                }
            }
            Ins::JumpUnlessBinary { src, bits, to } => {
                if !rest.is_binary(win.look(src), bits)? {
                    pc = to.index();
                }
            }
            Ins::Field { dst, src, index } => {
                let heap = &rest.heap;
                let field = match win.look(src).as_cell() {
                    Some(cell) if heap.kind(cell) == Some(Kind::Ctor) => {
                        heap.field(cell, usize::from(index))
                    }
                    _ => None,
                };
                let Some(field) = field else {
                    return Err(Stop::broken(Broken::NoField { index }));
                };
                let field = rest.share(field)?;
                win.put(&mut rest.heap, dst, field)?;
            }
            Ins::Tuple { dst, elements } => tuple(win.lend(), rest, dst, elements.regs())?,
            Ins::TupleWide { dst, elements } => {
                tuple(win.lend(), rest, dst, elements.regs(&code.lists))?;
            }
            Ins::Element { dst, src, index } => {
                let heap = &rest.heap;
                let element = match win.look(src).as_cell() {
                    Some(cell) if heap.kind(cell) == Some(Kind::Tuple) => {
                        heap.element(cell, usize::from(index))
                    }
                    _ => None,
                };
                let Some(element) = element else {
                    return Err(Stop::broken(Broken::NoElement { index }));
                };
                let element = rest.share(element)?;
                win.put(&mut rest.heap, dst, element)?;
            }
            Ins::Array { dst, elements } => array(win.lend(), rest, dst, elements.regs())?,
            Ins::ArrayWide { dst, elements } => {
                array(win.lend(), rest, dst, elements.regs(&code.lists))?;
            }
            Ins::Builtin {
                dst,
                intrinsic,
                ref args,
            } => {
                let v = rest.builtin(intrinsic, win.regs(), args.regs())?;
                win.put(&mut rest.heap, dst, v)?;
            }
            Ins::StackDepth { dst } => {
                let v = stack_depth(stack, rest, base)?;
                win = window(stack, base).ok_or_else(lost_window)?;
                win.put(&mut rest.heap, dst, v)?;
            }
            Ins::Equal { dst, a, b } => {
                let (a, b) = (win.look(a), win.look(b));
                let same = eq::equal(&rest.heap, a, b);
                win.put(&mut rest.heap, dst, Value::bool(same))?;
            }
            Ins::NotEqual { dst, a, b } => {
                let (a, b) = (win.look(a), win.look(b));
                let same = eq::equal(&rest.heap, a, b);
                win.put(&mut rest.heap, dst, Value::bool(!same))?;
            }
            Ins::Not { dst, a } => {
                let b = match win.look(a).view() {
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
                win.put(&mut rest.heap, dst, Value::bool(!b))?;
            }
            Ins::Range { dst, start, end } => {
                let v = rest.range(win.look(start), win.look(end))?;
                win.put(&mut rest.heap, dst, v)?;
            }
            Ins::ArrayLen { dst, src } => {
                let v = rest.array_len(win.look(src))?;
                win.put(&mut rest.heap, dst, v)?;
            }
            Ins::ArrayElem { dst, src, index } => {
                let s = rest.seq(win.look(src))?;
                let Some(v) = rest.element(s, usize::from(index))? else {
                    return Err(Stop::broken(Broken::NoArrayElement { index }));
                };
                win.put(&mut rest.heap, dst, v)?;
            }
            Ins::ArrayDrop { dst, src, n } => array_drop(win.lend(), rest, dst, src, n)?,
            Ins::ArrayPrepend {
                dst,
                rest: xs,
                front,
            } => {
                array_push(win.lend(), rest, dst, xs, front.regs(), End::Front)?;
            }
            Ins::ArrayPrependWide {
                dst,
                rest: xs,
                front,
            } => {
                let front = front.regs(&code.lists);
                array_push(win.lend(), rest, dst, xs, front, End::Front)?;
            }
            Ins::ArrayAppend {
                dst,
                rest: xs,
                back,
            } => {
                array_push(win.lend(), rest, dst, xs, back.regs(), End::Back)?;
            }
            Ins::ArrayAppendWide {
                dst,
                rest: xs,
                back,
            } => {
                let back = back.regs(&code.lists);
                array_push(win.lend(), rest, dst, xs, back, End::Back)?;
            }
            Ins::ArrayConcat { dst, a, b } => {
                let v = rest.array_concat(win.look(a), win.look(b))?;
                win.put(&mut rest.heap, dst, v)?;
            }
            Ins::ArraySlice {
                dst,
                src,
                start,
                end,
            } => {
                let of = win.look(src);
                let v = rest.array_slice(of, win.look(start), win.look(end))?;
                win.put(&mut rest.heap, dst, v)?;
            }
            Ins::ArraySet {
                dst,
                src,
                index,
                value,
                miss,
            } => match array_set(win.lend(), rest, src, index, value)? {
                Some(array) => win.hit(&mut rest.heap, dst, array)?,
                None => pc = miss.index(),
            },
            Ins::ArrayIndex { dst, src, index } => {
                let found = rest.index(win.look(src), win.look(index))?;
                let v = match found {
                    Some(v) => rest.some(v)?,
                    None => Value::nullary(code.abi.none),
                };
                win.put(&mut rest.heap, dst, v)?;
            }
            Ins::ArrayIndexOr {
                dst,
                src,
                index,
                default,
            } => {
                let found = rest.index(win.look(src), win.look(index))?;
                let v = match found {
                    Some(v) => v,
                    None => win.share(&mut rest.heap, default)?,
                };
                win.put(&mut rest.heap, dst, v)?;
            }
            Ins::Bad { why } => return Err(code.why(why)),
            Ins::JumpIfFalse { cond, to } => match win.look(cond).view() {
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
                let m = rest.map(win.get(map.0))?;
                let (k, v) = (win.look(key), win.look(value));
                // The register's reference passes to the set.
                win.take(map);
                let cell = map::set_owned(&mut rest.heap, m, k, v).map_err(fault)?;
                win.put(&mut rest.heap, dst, Value::cell(cell))?;
            }
            Ins::MapFind { dst, map, key, to } => {
                let m = rest.map(win.look(map))?;
                match map::get(&rest.heap, m, win.look(key)) {
                    Some(v) => {
                        let v = rest.share(v)?;
                        win.hit(&mut rest.heap, dst, v)?;
                    }
                    None => pc = to.index(),
                }
            }
            Ins::MapDeleteOwned { dst, map, key } => {
                let m = rest.map(win.get(map.0))?;
                let k = win.look(key);
                // The register's reference passes to the delete.
                win.take(map);
                let cell = map::delete_owned(&mut rest.heap, m, k).map_err(fault)?;
                win.put(&mut rest.heap, dst, Value::cell(cell))?;
            }
            Ins::JumpUnlessIntEq { a, b, to } => {
                jump!(win, rest, pc, to, int_holds(a, b), Compare::Eq);
            }
            Ins::JumpUnlessIntNe { a, b, to } => {
                jump!(win, rest, pc, to, int_holds(a, b), Compare::Ne);
            }
            Ins::JumpUnlessIntLt { a, b, to } => {
                jump!(win, rest, pc, to, int_holds(a, b), Compare::LT);
            }
            Ins::JumpUnlessIntLe { a, b, to } => {
                jump!(win, rest, pc, to, int_holds(a, b), Compare::LE);
            }
            Ins::JumpUnlessIntEqK { a, k, to } => {
                jump!(win, rest, pc, to, int_holds_k(a, k), Compare::Eq);
            }
            Ins::JumpUnlessIntNeK { a, k, to } => {
                jump!(win, rest, pc, to, int_holds_k(a, k), Compare::Ne);
            }
            Ins::JumpUnlessIntLtK { a, k, to } => {
                jump!(win, rest, pc, to, int_holds_k(a, k), Compare::LT);
            }
            Ins::JumpUnlessIntLeK { a, k, to } => {
                jump!(win, rest, pc, to, int_holds_k(a, k), Compare::LE);
            }
            Ins::JumpUnlessIntGtK { a, k, to } => {
                jump!(win, rest, pc, to, int_holds_k(a, k), Compare::GT);
            }
            Ins::JumpUnlessIntGeK { a, k, to } => {
                jump!(win, rest, pc, to, int_holds_k(a, k), Compare::GE);
            }
            Ins::JumpUnlessFloatLt { a, b, to } => {
                jump!(win, rest, pc, to, float_holds(a, b), Order::Lt);
            }
            Ins::JumpUnlessFloatLe { a, b, to } => {
                jump!(win, rest, pc, to, float_holds(a, b), Order::Le);
            }
            Ins::JumpUnlessFloatLtK { a, k, to } => {
                jump!(win, rest, pc, to, float_holds_k(a, k), Order::Lt);
            }
            Ins::JumpUnlessFloatLeK { a, k, to } => {
                jump!(win, rest, pc, to, float_holds_k(a, k), Order::Le);
            }
            Ins::JumpUnlessFloatGtK { a, k, to } => {
                jump!(win, rest, pc, to, float_holds_k(a, k), Order::Gt);
            }
            Ins::JumpUnlessFloatGeK { a, k, to } => {
                jump!(win, rest, pc, to, float_holds_k(a, k), Order::Ge);
            }
            // The frame is ending, so the result's register gives its
            // reference up instead of sharing it.
            Ins::RetClosure { src, last } => {
                let v = win.take(src);
                win.leave(&mut rest.heap, last)?;
                win.drop(&mut rest.heap, Dead(CLOSURE))?;
                let link = win.link();
                base = base.wrapping_sub(link.below());
                win = window(stack, base).ok_or_else(lost_window)?;
                win.put(&mut rest.heap, link.dst(), v)?;
                pc = link.pc();
            }
            Ins::Ret { src, last } => {
                let v = win.take(src);
                win.leave(&mut rest.heap, last)?;
                let link = win.link();
                base = base.wrapping_sub(link.below());
                win = window(stack, base).ok_or_else(lost_window)?;
                win.put(&mut rest.heap, link.dst(), v)?;
                pc = link.pc();
            }
        }
    }
}

/// Put what `args` pass at the start of `values`, and give that much of it.
#[inline(always)]
fn pass_few<'v, const N: usize>(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    args: &Args<Reg, N>,
    values: &'v mut [Value; N],
) -> Result<&'v [Value], DeadCell> {
    for (slot, a) in values.iter_mut().zip(args.iter()) {
        *slot = win.pass(&mut rest.heap, a)?;
    }
    Ok(values.get(..args.len()).unwrap_or_default())
}

/// Put what `args` pass in `passing`.
fn pass_all(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    args: &[Arg<Reg>],
) -> Result<(), DeadCell> {
    rest.passing.clear();
    for a in args {
        let v = win.pass(&mut rest.heap, *a)?;
        rest.passing.push(v);
    }
    Ok(())
}

/// Hand what [`pass_all`] gathered to the frame that is starting.
fn receive_all(mut win: Window<'_>, rest: &mut Rest<'_, '_>) {
    win.receive(&rest.passing);
    rest.passing.clear();
}

/// [`Window::rewrite`], with what `args` pass.
#[cold]
#[inline(never)]
fn rewrite_wide(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    own: Room,
    args: &[Arg<Reg>],
) -> Result<(), DeadCell> {
    pass_all(win.lend(), rest, args)?;
    win.rewrite(&mut rest.heap, own, &rest.passing)?;
    rest.passing.clear();
    Ok(())
}

/// [`rewrite_wide`], except that the registers in `keep` stay, so a loop
/// overwrites the cell its last turn gave up. Each is the only reference to a
/// cell holding nothing, and the register still owns it. So a cell the loop
/// never takes is freed when the frame really ends.
#[inline(never)]
fn rewrite_keeping(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    own: Room,
    args: &[Arg<Reg>],
    keep: &[Reg],
) -> Result<(), DeadCell> {
    pass_all(win.lend(), rest, args)?;
    win.rewrite_keeping(&mut rest.heap, own, &rest.passing, keep)?;
    rest.passing.clear();
    Ok(())
}

/// How many frames there are, the one at `base` the last. Only a built-in for
/// debugging asks, so the walk is kept out of the loop's way.
#[cold]
#[inline(never)]
fn stack_depth(stack: &[Value], rest: &mut Rest<'_, '_>, base: u32) -> Result<Value, Stop> {
    let frames = frame::depth(stack, base).ok_or_else(lost_window)?;
    bigint::int(&mut rest.heap, frames).map_err(fault)
}

/// Whether a stack of `len` registers has a window at `base`.
#[inline(always)]
fn fits(len: usize, base: u32) -> bool {
    (base as usize).wrapping_add(WINDOW) <= len
}

/// The stop for a frame that is not on the stack.
#[cold]
#[inline(never)]
fn lost_window() -> Stop {
    Stop::broken(Broken::NoWindow)
}

// What an instruction does that is more than a few lines, out of line so
// that the loop keeps what it holds in the machine's registers.

/// A new cell for `variant`.
#[inline(always)]
fn ctor(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    dst: Dst<Reg>,
    variant: Variant,
    fields: &Args<Reg, 6>,
) -> Result<(), Stop> {
    let mut values = UNSET;
    let values = pass_few(win.lend(), rest, fields, &mut values)?;
    let cell = rest.heap.ctor(variant.get(), values).map_err(fault)?;
    Ok(win.put(&mut rest.heap, dst, Value::cell(cell))?)
}

#[inline(never)]
fn ctor_wide(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    dst: Dst<Reg>,
    variant: Variant,
    fields: &[Arg<Reg>],
) -> Result<(), Stop> {
    pass_all(win.lend(), rest, fields)?;
    let cell = rest.heap.ctor(variant.get(), &rest.passing);
    rest.passing.clear();
    Ok(win.put(&mut rest.heap, dst, Value::cell(cell.map_err(fault)?))?)
}

/// A new constructor holding `values`, over the cell in `reuse` when one
/// fits there.
#[inline(always)]
fn over(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    dst: Dst<Reg>,
    variant: Variant,
    values: &[Value],
    reuse: Over<Reg>,
) -> Result<(), Stop> {
    // Move out the cell a `DropReuse` above hollowed and left in `reuse`,
    // so the register no longer holds it. Anything else there is a value
    // this frame is done with, so release it as the register would have.
    let parked = win.empty(reuse.0);
    let heap = &mut rest.heap;
    let fits = parked.as_cell();
    let cell = match fits.filter(|c| heap.fits_ctor(*c, values.len())) {
        Some(cell) => {
            heap.ctor_in(cell, variant.get(), values);
            cell
        }
        None => {
            rest.release(parked)?;
            let made = rest.heap.ctor(variant.get(), values);
            made.map_err(fault)?
        }
    };
    Ok(win.put(&mut rest.heap, dst, Value::cell(cell))?)
}

#[inline(always)]
fn over_few(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    dst: Dst<Reg>,
    variant: Variant,
    fields: &Args<Reg, 5>,
    reuse: Over<Reg>,
) -> Result<(), Stop> {
    let mut values = [Value::UNSET; 5];
    let values = pass_few(win.lend(), rest, fields, &mut values)?;
    over(win.lend(), rest, dst, variant, values, reuse)
}

#[inline(never)]
fn over_wide(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    dst: Dst<Reg>,
    variant: Variant,
    fields: &[Arg<Reg>],
    reuse: Over<Reg>,
) -> Result<(), Stop> {
    pass_all(win.lend(), rest, fields)?;
    let values = std::mem::take(&mut rest.passing);
    over(win.lend(), rest, dst, variant, &values, reuse)?;
    rest.passing = values;
    rest.passing.clear();
    Ok(())
}

#[inline(never)]
fn bits(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    dst: Dst<Reg>,
    op: BitsOp,
    regs: &[Reg],
) -> Result<(), Stop> {
    let v = rest.bits_op(op, win.regs(), regs)?;
    Ok(win.put(&mut rest.heap, dst, v)?)
}

/// A new string of what `${v}` shows, for a `v` that is none.
#[inline(never)]
fn make_text(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    dst: Dst<Reg>,
    v: Value,
) -> Result<(), Stop> {
    let v = rest.text_shown(v)?;
    Ok(win.put(&mut rest.heap, dst, v)?)
}

#[inline(never)]
fn concat(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    dst: Dst<Reg>,
    parts: &[Reg],
) -> Result<(), Stop> {
    let v = rest.concat(win.regs(), parts)?;
    Ok(win.put(&mut rest.heap, dst, v)?)
}

#[inline(never)]
fn closure(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    dst: Dst<Reg>,
    func: FuncIdx,
    captures: &[Reg],
) -> Result<(), Stop> {
    let values = shared(win.lend(), rest, captures)?;
    let cell = rest.heap.closure(func, &values).map_err(fault)?;
    Ok(win.put(&mut rest.heap, dst, Value::cell(cell))?)
}

#[inline(never)]
fn tuple(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    dst: Dst<Reg>,
    elements: &[Reg],
) -> Result<(), Stop> {
    let values = shared(win.lend(), rest, elements)?;
    let cell = rest.heap.tuple(&values).map_err(fault)?;
    Ok(win.put(&mut rest.heap, dst, Value::cell(cell))?)
}

#[inline(never)]
fn array(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    dst: Dst<Reg>,
    elements: &[Reg],
) -> Result<(), Stop> {
    let values = shared(win.lend(), rest, elements)?;
    let cell = array::from_values(&mut rest.heap, &values).map_err(fault)?;
    Ok(win.put(&mut rest.heap, dst, Value::cell(cell))?)
}

/// The values in `regs`, each with one more reference, for passing on.
fn shared(win: Window<'_>, rest: &mut Rest<'_, '_>, regs: &[Reg]) -> Result<Vec<Value>, DeadCell> {
    let mut values = Vec::with_capacity(regs.len());
    for r in regs {
        values.push(win.share(&mut rest.heap, Shared(*r))?);
    }
    Ok(values)
}

#[inline(never)]
fn array_drop(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    dst: Dst<Reg>,
    src: Arg<Reg>,
    n: Src<Reg>,
) -> Result<(), Stop> {
    let s = rest.seq(win.get(src.reg))?;
    let n = match win.look(n).view() {
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
            win.empty(src.reg);
            array::skip_owned(&mut rest.heap, a, n)
        }
        Seq::Tree(a) => array::skip(&mut rest.heap, a, n),
        Seq::Range { start, end } => {
            let n = (n as u64).min(array::range_len(start, end));
            // `n` is at most `end - start`, so `start + n` is
            // within `start..=end` and the add is exact.
            array::range(&mut rest.heap, start.wrapping_add_unsigned(n), end)
        }
    };
    Ok(win.put(&mut rest.heap, dst, Value::cell(cell.map_err(fault)?))?)
}

/// The array in `xs` with the values in `items` at `end` of it.
#[inline(never)]
fn array_push(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    dst: Dst<Reg>,
    xs: Arg<Reg>,
    items: &[Reg],
    end: End,
) -> Result<(), Stop> {
    let s = rest.seq(win.get(xs.reg))?;
    let a = rest.tree(s)?;
    let mut values: smallvec::SmallVec<[Value; 4]> = smallvec::SmallVec::with_capacity(items.len());
    for r in items {
        values.push(win.share(&mut rest.heap, Shared(*r))?);
    }
    if xs.moved {
        let held = win.empty(xs.reg);
        rest.release(held)?;
    }
    let cell = rest.push_all(a, &values, end)?;
    Ok(win.put(&mut rest.heap, dst, Value::cell(cell))?)
}

/// The array in `src` with the value in `value` at the index in `index`,
/// holding its own reference, or `None` when the array has no such
/// index. A `None` has read the registers and changed nothing. Otherwise
/// the write holds one reference to the array, which is the register's
/// when `src` gives it up, and so finds an array nothing else holds with
/// a count of 1.
#[inline(never)]
fn array_set(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    src: Arg<Reg>,
    index: Src<Reg>,
    value: Arg<Reg>,
) -> Result<Option<Value>, Stop> {
    let s = rest.seq(win.get(src.reg))?;
    let at = rest.position(win.look(index))?;
    let Some(i) = at.filter(|i| (*i as u64) < s.len(&rest.heap)) else {
        return Ok(None);
    };
    let x = win.pass(&mut rest.heap, value)?;
    let array = match s {
        Seq::Tree(a) => {
            if src.moved {
                win.empty(src.reg);
            } else {
                rest.heap.share(a)?;
            }
            a
        }
        // A range stores no elements to write over, so it is built as a
        // tree first, which nothing else holds.
        Seq::Range { .. } => {
            let a = rest.tree(s)?;
            if src.moved {
                let range = win.empty(src.reg);
                rest.release(range)?;
            }
            a
        }
    };
    let cell = array::set(&mut rest.heap, array, i, x).map_err(fault)?;
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
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    dst: Dst<Reg>,
    op: Arith,
    x: Value,
    y: Value,
) -> Result<(), Stop> {
    let v = int::op(&mut rest.heap, op, x, y)?;
    Ok(win.put(&mut rest.heap, dst, v)?)
}

/// `-x` into `dst`, for an `x` that is no small Int or is the lowest one.
#[cold]
#[inline(never)]
fn int_neg_slow(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    dst: Dst<Reg>,
    x: Value,
) -> Result<(), Stop> {
    let v = int::neg(&mut rest.heap, x)?;
    Ok(win.put(&mut rest.heap, dst, v)?)
}

/// `x op y` into `dst`, for a Float instruction whose plain answer is no
/// Float: an operand is none, or the answer is none or too large.
#[cold]
#[inline(never)]
fn float_slow(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    dst: Dst<Reg>,
    op: Arith,
    x: Value,
    y: Value,
) -> Result<(), Stop> {
    let v = float::op(op, float::read(x)?, float::read(y)?);
    Ok(win.put(&mut rest.heap, dst, v)?)
}

/// `-x` into `dst`. Minus a Float is always one, so `x` is not.
#[cold]
#[inline(never)]
fn float_neg_slow(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    dst: Dst<Reg>,
    x: Value,
) -> Result<(), Stop> {
    let v = Value::float(-float::read(x)?);
    Ok(win.put(&mut rest.heap, dst, v)?)
}

/// The Float nearest `x`, which is no small Int, into `dst`.
#[cold]
#[inline(never)]
fn float_from_int_slow(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    dst: Dst<Reg>,
    x: Value,
) -> Result<(), Stop> {
    let v = int::to_float(&rest.heap, x)?;
    Ok(win.put(&mut rest.heap, dst, v)?)
}

/// The stop for a run that went past the code's end at `pc`.
#[cold]
#[inline(never)]
fn ran_past_end(code: &Code, pc: usize) -> Stop {
    Stop::broken(Broken::RanPastEnd {
        func: name_before(code, pc),
    })
}

/// The stop for a read of capture `index`, by the instruction before `pc`.
#[cold]
#[inline(never)]
fn no_capture(code: &Code, pc: usize, index: u32) -> Stop {
    Stop::broken(Broken::NoCapture {
        func: name_before(code, pc),
        index,
    })
}

/// The name of the function that the instruction before `pc` is in.
fn name_before(code: &Code, pc: usize) -> String {
    let entry = code.function_at(pc.saturating_sub(1));
    entry.map(|e| e.name.clone()).unwrap_or_default()
}

/// Check the heap against every reference the run holds between two
/// instructions: the registers, the globals and `os.env`'s map. `last` is a
/// finished run's result.
#[cold]
#[inline(never)]
fn check(stack: &[Value], rest: &Rest<'_, '_>, last: Value) -> Result<(), Stop> {
    let held = stack.iter().chain(&rest.passing);
    let roots = held
        .chain(&rest.globals)
        .copied()
        .chain(std::iter::once(last))
        .chain(rest.env.map(Value::cell));
    Ok(rest.heap.audit(roots)?)
}

/// Room for what an instruction passes, before any of it is there. It is all
/// zero bits, which is cheaper to write than `Nil`s.
const UNSET: [Value; 6] = [Value::UNSET; 6];

/// Where a global is, when it has a place.
fn global(slot: GlobalSlot) -> Option<usize> {
    usize::try_from(slot.0).ok()
}

#[cfg(test)]
mod tests;
