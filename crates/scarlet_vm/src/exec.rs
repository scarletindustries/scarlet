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

use scarlet_ir::core_ir::{GlobalSlot, MAX_PARAMS};

use crate::array::{self, End, Seq};
use crate::bigint;
use crate::code::{
    Arg, AssignList, BitsOp, Code, Counted, Dead, Dst, Entry, Hands, Ins, Makes, MaybeDst, Op, Reg,
    Room, Sets, Shared, Src, Taken,
};
use crate::counts::Counts;
use crate::eq;
use crate::float;
use crate::frame::{self, CLOSURE, FIRST, Link, RESULT, WINDOW, Window, window};
use crate::heap::{Cell, Ctor, DeadCell, Heap, Kind, Layout, NotCopied, Opened, STRIDE, Shape};
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
                heap: Heap::new(code.ctors.clone()),
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
        // and the audits and the `Halt` are in no block.
        let ops = Op::ALL
            .iter()
            .zip(ran)
            .filter(|(op, _)| !matches!(op, Op::Count | Op::Audit | Op::AuditMade | Op::Halt));
        let heap = &self.rest.heap;
        Ok(Counts {
            ops: ops.map(|(op, n)| (op.name(), n)).collect(),
            cells_made: heap.made(),
            cells_live: heap.live_cells()?,
            heap_words: heap.words_carved(),
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
            Ins::AuditMade { src, may } => check_made(&rest.heap, win.look(src), may)?,
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
            Ins::Move2 { ref dsts, ref srcs } => move_all(win.lend(), rest, dsts, srcs)?,
            Ins::Move3 { ref dsts, ref srcs } => move_all(win.lend(), rest, dsts, srcs)?,
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
            Ins::Ctor1 {
                dst,
                ctor,
                ref fields,
            } => new_cell(win.lend(), rest, dst, Shape::ctor(ctor), fields)?,
            Ins::Ctor2 {
                dst,
                ctor,
                ref fields,
            } => new_cell(win.lend(), rest, dst, Shape::ctor(ctor), fields)?,
            Ins::Ctor3 {
                dst,
                ctor,
                ref fields,
            } => new_cell(win.lend(), rest, dst, Shape::ctor(ctor), fields)?,
            Ins::Ctor4 {
                dst,
                ctor,
                ref fields,
            } => new_cell(win.lend(), rest, dst, Shape::ctor(ctor), fields)?,
            Ins::Ctor5 {
                dst,
                ctor,
                ref fields,
            } => new_cell(win.lend(), rest, dst, Shape::ctor(ctor), fields)?,
            Ins::Ctor6 {
                dst,
                ctor,
                ref fields,
            } => new_cell(win.lend(), rest, dst, Shape::ctor(ctor), fields)?,
            Ins::Ctor7 {
                dst,
                ctor,
                ref fields,
            } => new_cell(win.lend(), rest, dst, Shape::ctor(ctor), fields)?,
            Ins::Ctor8 {
                dst,
                ctor,
                ref fields,
            } => new_cell(win.lend(), rest, dst, Shape::ctor(ctor), fields)?,
            Ins::Ctor {
                dst,
                ctor,
                ref fields,
            } => new_cell(win.lend(), rest, dst, Shape::ctor(ctor), fields)?,
            Ins::CtorWide { dst, ctor, fields } => {
                new_cell_apart(
                    win.lend(),
                    rest,
                    dst,
                    Shape::ctor(ctor),
                    fields.args(&code.lists),
                )?;
            }
            Ins::Update1 { dst, ref sets } => update(win.lend(), rest, dst, sets)?,
            Ins::Update2 { dst, ref sets } => update_out(win.lend(), rest, dst, sets)?,
            Ins::Update3 { dst, ref sets } => update_out(win.lend(), rest, dst, sets)?,
            Ins::UpdateWide {
                dst,
                base,
                layout,
                sets,
            } => update_apart(win.lend(), rest, dst, base, layout, sets)?,
            Ins::Open1 {
                ref dsts,
                src,
                ctor,
            } => open(win.lend(), rest, dsts, src, ctor)?,
            Ins::Open2 {
                ref dsts,
                src,
                ctor,
            } => open(win.lend(), rest, dsts, src, ctor)?,
            Ins::Open3 {
                ref dsts,
                src,
                ctor,
            } => open(win.lend(), rest, dsts, src, ctor)?,
            Ins::Open4 {
                ref dsts,
                src,
                ctor,
            } => open(win.lend(), rest, dsts, src, ctor)?,
            Ins::Open5 {
                ref dsts,
                src,
                ctor,
            } => open(win.lend(), rest, dsts, src, ctor)?,
            Ins::Open6 {
                ref dsts,
                src,
                ctor,
            } => open(win.lend(), rest, dsts, src, ctor)?,
            Ins::Open7 {
                ref dsts,
                src,
                ctor,
            } => open(win.lend(), rest, dsts, src, ctor)?,
            Ins::Open8 {
                ref dsts,
                src,
                ctor,
            } => open(win.lend(), rest, dsts, src, ctor)?,
            Ins::OpenWide { dsts, src, ctor } => {
                open_apart(win.lend(), rest, dsts.dsts(&code.lists), src, ctor)?;
            }
            Ins::Drop { reg } => win.drop(&mut rest.heap, reg)?,
            Ins::DropWide { regs } => {
                win.drop_each(&mut rest.heap, regs.regs(&code.lists))?;
            }
            Ins::Call0 { dst, own, ref to } => {
                let up = room!(own);
                open!(up, own, dst);
                pc = to.index();
            }
            Ins::Call1 {
                dst,
                own,
                ref to,
                a,
            } => {
                let up = room!(own);
                let a = win.take(a);
                open!(up, own, dst);
                win.receive(&[a]);
                pc = to.index();
            }
            Ins::Call2 {
                dst,
                own,
                ref to,
                a,
                b,
            } => {
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
                ref to,
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
            Ins::Call1Share {
                dst,
                own,
                ref to,
                a,
            } => {
                let up = room!(own);
                let a = win.pass(&mut rest.heap, a)?;
                open!(up, own, dst);
                win.receive(&[a]);
                pc = to.index();
            }
            Ins::Call2Share {
                dst,
                own,
                ref to,
                a,
                b,
            } => {
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
                ref to,
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
                ref to,
                ref args,
            } => {
                let up = room!(own);
                let mut values = UNSET;
                let values = pass_few(win.lend(), rest, args.iter(), &mut values)?;
                open!(up, own, dst);
                win.receive(values);
                pc = to.index();
            }
            Ins::CallWide {
                dst,
                own,
                ref to,
                args,
            } => {
                let up = room!(own);
                pass_all(stack, rest, base, up, args.args(&code.lists))?;
                open!(up, own, dst);
                pc = to.index();
            }
            Ins::CallSelf {
                dst,
                own,
                ref to,
                ref args,
            } => {
                let up = room!(own);
                let closure = rest.share(win.closure())?;
                let mut values = UNSET;
                let values = pass_few(win.lend(), rest, args.iter(), &mut values)?;
                open!(up, own, dst);
                win.run_as(closure);
                win.receive(values);
                pc = to.index();
            }
            Ins::CallSelfWide {
                dst,
                own,
                ref to,
                args,
            } => {
                let up = room!(own);
                let closure = rest.share(win.closure())?;
                pass_all(stack, rest, base, up, args.args(&code.lists))?;
                open!(up, own, dst);
                win.run_as(closure);
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
                let values = pass_few(win.lend(), rest, args.iter(), &mut values)?;
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
                pass_all(stack, rest, base, up, args)?;
                open!(up, own, dst);
                win.run_as(closure);
                pc = entry.pc.index();
            }
            // A tail call's callee takes over this frame, so a loop written as
            // tail recursion runs in constant space. A function known when the
            // program loaded is entered by a jump, once its arguments are in
            // its parameters.
            Ins::Enter { to } => pc = to.index(),
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
                let values = pass_few(win.lend(), rest, args.iter(), &mut values)?;
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
            } => new_cell_apart(win.lend(), rest, dst, Shape::closure(func), captures.regs())?,
            Ins::ClosureWide {
                dst,
                func,
                captures,
            } => new_cell_apart(
                win.lend(),
                rest,
                dst,
                Shape::closure(func),
                captures.regs(&code.lists),
            )?,
            Ins::GetCapture { dst, index } => {
                let closure = win.closure().as_cell();
                match closure.and_then(|c| rest.heap.near_capture(c, index)) {
                    Some(v) if v.as_cell().is_none_or(|c| rest.heap.count_up(c)) => {
                        win.put(&mut rest.heap, dst, v)?;
                    }
                    Some(_) | None => far_capture(win.lend(), rest, dst, index, pc)?,
                }
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
            // A test reads where it jumps to only when it jumps. Read before
            // the test, it would be one more load for a test that goes on.
            Ins::JumpUnlessTag { src, tag, ref to } => {
                if win.look(src).tag() != tag {
                    pc = to.index();
                }
            }
            Ins::Switch { src, table } => {
                pc = match table.at(win.look(src).tag().variant(), &code.lists) {
                    Some(to) => to.index(),
                    None => return Err(no_case()),
                }
            }
            Ins::SwitchInt { src, table } => {
                let to = match win.look(src).small_int() {
                    Some(n) => table.at(n, &code.lists),
                    None => table.other(&code.lists),
                };
                pc = match to {
                    Some(to) => to.index(),
                    None => return Err(no_case()),
                }
            }
            Ins::SwitchIntFar { src, places, to } => {
                let found = rest.far_case(places, win.look(src));
                pc = found.unwrap_or(to).index();
            }
            Ins::JumpUnlessInt { src, n, ref to } => {
                if !rest.is_int(win.look(src), n) {
                    pc = to.index();
                }
            }
            Ins::JumpUnlessFloat { src, x, ref to } => {
                if !matches!(win.look(src).view(), View::Float(f) if f == x) {
                    pc = to.index();
                }
            }
            Ins::JumpUnlessStr { src, text, ref to } => {
                if !rest.is_text(win.look(src), text)? {
                    pc = to.index();
                }
            }
            Ins::Field { dst, src, index } => {
                let cell = win.look(src).as_cell();
                match cell.and_then(|c| rest.heap.near_field(c, usize::from(index))) {
                    Some(v) if v.as_cell().is_none_or(|c| rest.heap.count_up(c)) => {
                        win.put(&mut rest.heap, dst, v)?;
                    }
                    Some(_) | None => far_field(win.lend(), rest, dst, src, index)?,
                }
            }
            Ins::Tuple2 { dst, ref elements } => {
                new_cell(win.lend(), rest, dst, Shape::TUPLE, elements)?
            }
            Ins::Tuple3 { dst, ref elements } => {
                new_cell(win.lend(), rest, dst, Shape::TUPLE, elements)?
            }
            Ins::Tuple { dst, ref elements } => {
                new_cell_apart(win.lend(), rest, dst, Shape::TUPLE, elements.regs())?
            }
            Ins::TupleWide { dst, elements } => {
                new_cell_apart(
                    win.lend(),
                    rest,
                    dst,
                    Shape::TUPLE,
                    elements.regs(&code.lists),
                )?;
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
                    None => code.abi.none,
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
            Ins::JumpIfFalse { cond, ref to } => match win.look(cond).view() {
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
            Ins::JumpUnlessIntEq { a, b, ref to } => {
                jump!(win, rest, pc, to, int_holds(a, b), Compare::Eq);
            }
            Ins::JumpUnlessIntNe { a, b, ref to } => {
                jump!(win, rest, pc, to, int_holds(a, b), Compare::Ne);
            }
            Ins::JumpUnlessIntLt { a, b, ref to } => {
                jump!(win, rest, pc, to, int_holds(a, b), Compare::LT);
            }
            Ins::JumpUnlessIntLe { a, b, ref to } => {
                jump!(win, rest, pc, to, int_holds(a, b), Compare::LE);
            }
            Ins::JumpUnlessIntEqK { a, k, ref to } => {
                jump!(win, rest, pc, to, int_holds_k(a, k), Compare::Eq);
            }
            Ins::JumpUnlessIntNeK { a, k, ref to } => {
                jump!(win, rest, pc, to, int_holds_k(a, k), Compare::Ne);
            }
            Ins::JumpUnlessIntLtK { a, k, ref to } => {
                jump!(win, rest, pc, to, int_holds_k(a, k), Compare::LT);
            }
            Ins::JumpUnlessIntLeK { a, k, ref to } => {
                jump!(win, rest, pc, to, int_holds_k(a, k), Compare::LE);
            }
            Ins::JumpUnlessIntGtK { a, k, ref to } => {
                jump!(win, rest, pc, to, int_holds_k(a, k), Compare::GT);
            }
            Ins::JumpUnlessIntGeK { a, k, ref to } => {
                jump!(win, rest, pc, to, int_holds_k(a, k), Compare::GE);
            }
            Ins::JumpUnlessFloatLt { a, b, ref to } => {
                jump!(win, rest, pc, to, float_holds(a, b), Order::Lt);
            }
            Ins::JumpUnlessFloatLe { a, b, ref to } => {
                jump!(win, rest, pc, to, float_holds(a, b), Order::Le);
            }
            Ins::JumpUnlessFloatLtK { a, k, ref to } => {
                jump!(win, rest, pc, to, float_holds_k(a, k), Order::Lt);
            }
            Ins::JumpUnlessFloatLeK { a, k, ref to } => {
                jump!(win, rest, pc, to, float_holds_k(a, k), Order::Le);
            }
            Ins::JumpUnlessFloatGtK { a, k, ref to } => {
                jump!(win, rest, pc, to, float_holds_k(a, k), Order::Gt);
            }
            Ins::JumpUnlessFloatGeK { a, k, ref to } => {
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

/// Give each of `dsts` what its one of `srcs` held.
#[inline(always)]
fn move_all<const N: usize>(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    dsts: &[Dst<Reg>; N],
    srcs: &[Taken<Reg>; N],
) -> Result<(), DeadCell> {
    let values = srcs.map(|src| win.take(src));
    for (dst, v) in dsts.iter().zip(values) {
        win.put(&mut rest.heap, *dst, v)?;
    }
    Ok(())
}

/// Put what `args` pass at the start of `values`, and give that much of it.
#[inline(always)]
fn pass_few<'v, const N: usize>(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    args: impl ExactSizeIterator<Item = Arg<Reg>>,
    values: &'v mut [Value; N],
) -> Result<&'v [Value], DeadCell> {
    let len = args.len();
    for (slot, a) in values.iter_mut().zip(args) {
        *slot = win.pass(&mut rest.heap, a)?;
    }
    Ok(values.get(..len).unwrap_or_default())
}

/// Put what `args` pass from the frame at `base` in the parameters of the one
/// at `up`, which is starting and is above it.
#[inline(never)]
fn pass_all(
    stack: &mut [Value],
    rest: &mut Rest<'_, '_>,
    base: u32,
    up: u32,
    args: &[Arg<Reg>],
) -> Result<(), Stop> {
    for (a, param) in args.iter().zip(FIRST.0..) {
        let mut from = window(stack, base).ok_or_else(lost_window)?;
        let v = from.pass(&mut rest.heap, *a)?;
        let mut to = window(stack, up).ok_or_else(lost_window)?;
        to.receive_one(Reg(param), v);
    }
    Ok(())
}

/// [`Window::rewrite`], with what `args` pass.
#[inline(never)]
fn rewrite_wide(
    win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    own: Room,
    args: &[Arg<Reg>],
) -> Result<(), Stop> {
    // Room that is not used costs a store for each value all the same.
    if args.len() <= 12 {
        rewrite_up_to::<12>(win, rest, own, args)
    } else {
        rewrite_up_to::<MAX_PARAMS>(win, rest, own, args)
    }
}

/// [`rewrite_wide`], of `N` arguments at most. No call has more than
/// [`MAX_PARAMS`].
fn rewrite_up_to<const N: usize>(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    own: Room,
    args: &[Arg<Reg>],
) -> Result<(), Stop> {
    if args.len() > N {
        return Err(Stop::broken(Broken::NoPlace));
    }
    let mut values = [Value::UNSET; N];
    let values = pass_few(win.lend(), rest, args.iter().copied(), &mut values)?;
    Ok(win.rewrite(&mut rest.heap, own, values)?)
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

/// A new cell of `shape` in `dst`, given what `given` hands on.
///
/// The operands are gone over twice. First each value that is shared gets its
/// reference. Then the values go from the registers to the cell, once there is
/// one, so a cell that is refused has taken nothing.
#[inline(always)]
fn new_cell<const N: usize, G: Hands + ?Sized>(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    dst: Dst<Reg>,
    shape: Shape<N>,
    given: &G,
) -> Result<(), Stop> {
    for a in given.hands() {
        win.add(&mut rest.heap, a)?;
    }
    // The two ways do not join, so taking a freed cell is one straight line.
    let values = given.hands().map(|a| win.hand(a));
    match rest.heap.in_freed(shape, values) {
        Some(cell) => Ok(win.put(&mut rest.heap, dst, Value::cell(cell))?),
        None => new_cell_at_end(win, rest, dst, shape, given),
    }
}

/// [`new_cell`], for an instruction that is a call out of the loop.
#[inline(never)]
fn new_cell_apart<const N: usize, G: Hands + ?Sized>(
    win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    dst: Dst<Reg>,
    shape: Shape<N>,
    given: &G,
) -> Result<(), Stop> {
    new_cell(win, rest, dst, shape, given)
}

/// The rest of [`new_cell`] when no freed cell has the size.
#[inline(never)]
fn new_cell_at_end<const N: usize, G: Hands + ?Sized>(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    dst: Dst<Reg>,
    shape: Shape<N>,
    given: &G,
) -> Result<(), Stop> {
    // Each value is read through the window itself and not through a reference
    // to it, which is a load fewer.
    let mut from = win.lend();
    let values = given.hands().map(move |a| from.hand(a));
    match rest.heap.at_end(shape, values) {
        Ok(cell) => Ok(win.put(&mut rest.heap, dst, Value::cell(cell))?),
        Err(refused) => {
            for a in given.hands() {
                win.take_back(&mut rest.heap, a)?;
            }
            Err(fault(refused))
        }
    }
}

/// The constructor that `sets` updates, with its values, in `dst`.
///
/// A cell that nothing else holds is written there, when every operand is
/// taken. The old values are given up once all of it is done, so a stop at one
/// that is freed leaves nothing half done. The rest is [`update_held`]'s.
#[inline(always)]
fn update<const N: usize>(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    dst: Dst<Reg>,
    sets: &Sets<Reg, N>,
) -> Result<(), Stop> {
    let Some((base, values)) = sets.taken() else {
        return update_held(win, rest, dst, sets);
    };
    let heap = &mut rest.heap;
    let v = win.get(base.0);
    let new = values.map(|r| win.get(r.0));
    let alone = |cell| heap.set_alone(cell, sets.layout, sets.at, new);
    let Some(old) = v.as_cell().and_then(alone) else {
        return update_held(win, rest, dst, sets);
    };
    for r in values {
        win.take(r);
    }
    let mut dead = false;
    for old in old {
        if let Some(old) = old.as_cell() {
            dead |= heap.release_apart(old).is_err();
        }
    }
    // It is nearly always the same register, which then holds its own word.
    if dst.0 != base.0 {
        win.take(base);
        win.put(heap, dst, v)?;
    }
    unless(dead)
}

/// [`update`], out of the loop.
#[inline(never)]
fn update_out<const N: usize>(
    win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    dst: Dst<Reg>,
    sets: &Sets<Reg, N>,
) -> Result<(), Stop> {
    update(win, rest, dst, sets)
}

/// [`update`] of a cell that something else holds, or that this does not take,
/// and with a value that this does not take.
///
/// The values get their references first. So a cell that is to hold itself has
/// two holders, and is copied. A copy that is refused has taken nothing.
#[inline(never)]
fn update_held<const N: usize>(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    dst: Dst<Reg>,
    sets: &Sets<Reg, N>,
) -> Result<(), Stop> {
    let heap = &mut rest.heap;
    add_each(&win, heap, sets.each().map(|(_, a)| a))?;
    let base = sets.base();
    let v = win.get(base.reg);
    let new = sets.values().map(|r| win.get(r));
    if base.moved && sets.taken().is_none() {
        let alone = |cell| heap.set_alone(cell, sets.layout, sets.at, new);
        if let Some(old) = v.as_cell().and_then(alone) {
            return update_alone(win, rest, dst, sets, v, old);
        }
    }
    let copied = match v.as_cell() {
        Some(cell) => heap.copy_set(cell, sets.layout, sets.at, new),
        None => Err(NotCopied::Other),
    };
    let copy = match copied {
        Ok(copy) => copy,
        Err(why) => {
            let why = not_copied(heap, v, why);
            take_back_each(&win, heap, sets.each().map(|(_, a)| a))?;
            return Err(why);
        }
    };
    for (_, a) in sets.each() {
        win.hand(a);
    }
    let mut dead = false;
    if base.moved {
        dead = win.drop(heap, Dead(base.reg)).is_err();
    }
    win.put(heap, dst, Value::cell(copy))?;
    unless(dead)
}

/// The end of an update that wrote to `v`, whose cell nothing else holds, and had
/// `old` there.
#[inline(never)]
fn update_alone<const N: usize>(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    dst: Dst<Reg>,
    sets: &Sets<Reg, N>,
    v: Value,
    old: [Value; N],
) -> Result<(), Stop> {
    for (_, a) in sets.each() {
        win.hand(a);
    }
    win.empty(sets.base().reg);
    let mut dead = false;
    for old in old.into_iter().chain([win.swap(dst, v)]) {
        if let Some(old) = old.as_cell() {
            dead |= rest.heap.release(old).is_err();
        }
    }
    unless(dead)
}

/// [`Window::add`] each of `args`, or at a freed cell none of them.
#[inline(always)]
fn add_each(
    win: &Window<'_>,
    heap: &mut Heap,
    args: impl Iterator<Item = Arg<Reg>> + Clone,
) -> Result<(), DeadCell> {
    for (n, a) in args.clone().enumerate() {
        if let Err(dead) = win.add(heap, a) {
            take_back_each(win, heap, args.take(n))?;
            return Err(dead);
        }
    }
    Ok(())
}

/// [`Window::take_back`] each of `args`.
#[cold]
#[inline(never)]
fn take_back_each(
    win: &Window<'_>,
    heap: &mut Heap,
    args: impl Iterator<Item = Arg<Reg>>,
) -> Result<(), DeadCell> {
    for a in args {
        win.take_back(heap, a)?;
    }
    Ok(())
}

/// [`update`] of more fields, or of a long constructor.
#[inline(never)]
fn update_apart(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    dst: Dst<Reg>,
    base: Arg<Reg>,
    layout: Layout,
    sets: AssignList,
) -> Result<(), Stop> {
    let sets = sets.sets(&rest.code.lists);
    let heap = &mut rest.heap;
    add_each(&win, heap, sets.iter().map(|set| set.value))?;
    let v = win.get(base.reg);
    let last = sets.last().map_or(0, |set| usize::from(set.field));
    let cell = v.as_cell().filter(|_| base.moved);
    let cell = cell.filter(|c| heap.has_field(*c, layout, last));
    let mut dead = false;
    let made = match cell.filter(|c| heap.unique(*c) == Ok(true)) {
        Some(cell) => {
            for set in sets {
                let v = win.hand(set.value);
                let old = heap.set_field(cell, usize::from(set.field), v);
                if let Some(old) = old.and_then(Value::as_cell) {
                    dead |= heap.release(old).is_err();
                }
            }
            win.empty(base.reg);
            cell
        }
        None => {
            let each = sets.iter().map(|s| (s.field, win.get(s.value.reg)));
            let each: smallvec::SmallVec<[(u16, Value); STRIDE]> = each.collect();
            let copied = match v.as_cell() {
                Some(cell) => heap.copy_set_wide(cell, layout, &each),
                None => Err(NotCopied::Other),
            };
            let copy = match copied {
                Ok(copy) => copy,
                Err(why) => {
                    let why = not_copied(heap, v, why);
                    take_back_each(&win, heap, sets.iter().map(|set| set.value))?;
                    return Err(why);
                }
            };
            for set in sets {
                win.hand(set.value);
            }
            if base.moved {
                dead = win.drop(heap, Dead(base.reg)).is_err();
            }
            copy
        }
    };
    win.put(heap, dst, Value::cell(made))?;
    unless(dead)
}

/// The stop for an update of `v` that made no copy.
#[cold]
#[inline(never)]
fn not_copied(heap: &Heap, v: Value, why: NotCopied) -> Stop {
    match why {
        NotCopied::Refused(why) => fault(why),
        NotCopied::Other => match v.as_cell().map(|cell| heap.unique(cell)) {
            Some(Err(dead)) => dead.into(),
            Some(Ok(_)) | None => Stop::broken(Broken::UpdatedOther(v.view())),
        },
    }
}

/// Take the `ctor` in `src` apart. Each field goes to its register in `dsts`.
///
/// First the fields have their references: the cell's own when nothing else held
/// it, and then it is freed, and one more for each that goes to a register when
/// something did. Then `src` is emptied, and only then is a register written,
/// and what it held given up. So one of the registers may be `src`, and one may
/// hold what else held the cell.
///
/// What stops this is a freed cell. When it is a field that another reference was
/// to be added to, nothing has changed, and when it is something to give up,
/// everything else is done. A field that only moves is not looked at.
///
/// A cell that `Heap::open` does not take goes to [`open_apart`], which keeps
/// another order.
#[inline(always)]
fn open<const N: usize>(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    dsts: &[MaybeDst<Reg>; N],
    src: Taken<Reg>,
    ctor: Ctor,
) -> Result<(), Stop> {
    let heap = &mut rest.heap;
    let cell = win.get(src.0).as_cell();
    match cell.and_then(|cell| heap.open::<N>(cell, ctor)) {
        Some(Opened::Alone(fields)) => {
            win.take(src);
            let mut dead = false;
            for (dst, v) in dsts.iter().zip(fields) {
                let old = match dst.dst() {
                    Some(dst) => win.swap(dst, v),
                    None => v,
                };
                if let Some(old) = old.as_cell() {
                    dead |= heap.release_apart(old).is_err();
                }
            }
            unless(dead)
        }
        // Inside the loop a wider one costs what is around it more than the
        // call it saves.
        Some(Opened::Held(cell, fields)) if N <= 2 => open_held(win, rest, dsts, src, cell, fields),
        Some(Opened::Held(cell, fields)) => open_held_apart(win, rest, dsts, src, cell, fields),
        None => open_apart(win, rest, dsts, src, ctor),
    }
}

/// The rest of [`open`] for `cell`, which something else holds and which has
/// lost a holder.
#[inline(always)]
fn open_held<const N: usize>(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    dsts: &[MaybeDst<Reg>; N],
    src: Taken<Reg>,
    cell: Cell,
    fields: [Value; N],
) -> Result<(), Stop> {
    let heap = &mut rest.heap;
    for (n, (dst, v)) in dsts.iter().zip(fields).enumerate() {
        if dst.dst().is_some() && heap.own(v).is_err() {
            return Err(unshare(heap, dsts, cell, n));
        }
    }
    win.take(src);
    let mut dead = false;
    for (dst, v) in dsts.iter().zip(fields) {
        if let Some(dst) = dst.dst()
            && let Some(old) = win.swap(dst, v).as_cell()
        {
            dead |= heap.release_apart(old).is_err();
        }
    }
    unless(dead)
}

/// [`open_held`], out of the loop.
#[inline(never)]
fn open_held_apart<const N: usize>(
    win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    dsts: &[MaybeDst<Reg>; N],
    src: Taken<Reg>,
    cell: Cell,
    fields: [Value; N],
) -> Result<(), Stop> {
    open_held(win, rest, dsts, src, cell, fields)
}

/// Give `cell` its holder back, and take back the references added to its first
/// `n` fields, because the one after them is a freed cell.
#[cold]
#[inline(never)]
fn unshare(heap: &mut Heap, dsts: &[MaybeDst<Reg>], cell: Cell, n: usize) -> Stop {
    let mut undone = heap.share(cell);
    for (i, dst) in dsts.iter().enumerate().take(n) {
        if dst.dst().is_some()
            && let Some(field) = heap.field(cell, i).and_then(Value::as_cell)
        {
            undone = undone.and(heap.release(field));
        }
    }
    undone.err().unwrap_or(DeadCell).into()
}

/// [`open`] of any cell: one whose fields are in a list, a long one, one that is
/// never counted, and one that cannot be taken apart.
///
/// It goes field by field: each has its reference, then its register is written,
/// and then what that held is given up. The cell is given up last, so until then
/// its fields are there, whatever held it besides. At a freed field that another
/// reference was to be added to, the fields before it are in their registers and
/// `src` has the cell back.
#[cold]
#[inline(never)]
fn open_apart(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    dsts: &[MaybeDst<Reg>],
    src: Taken<Reg>,
    ctor: Ctor,
) -> Result<(), Stop> {
    let v = win.get(src.0);
    let heap = &mut rest.heap;
    let cell = v.as_cell().ok_or_else(|| opened_other(v))?;
    let alone = heap.unique(cell)?;
    if !heap.is_ctor(cell, ctor, dsts.len()) {
        return Err(opened_other(v));
    }
    win.take(src);
    let mut dead = false;
    for (run, dsts) in dsts.chunks(STRIDE).enumerate() {
        let fields = heap.run_of_values(cell, run);
        for (dst, v) in dsts.iter().zip(fields) {
            let old = match dst.dst() {
                Some(dst) => {
                    if !alone && heap.own(v).is_err() {
                        win.set(heap, src.0, Value::cell(cell))?;
                        return Err(DeadCell.into());
                    }
                    win.swap(dst, v)
                }
                None if alone => v,
                None => Value::NIL,
            };
            if let Some(old) = old.as_cell() {
                dead |= heap.release_apart(old).is_err();
            }
        }
    }
    if alone {
        heap.discard(cell)?;
    } else {
        heap.release(cell)?;
    }
    unless(dead)
}

/// The stop for a freed cell, when `dead` says there was one.
#[inline(always)]
fn unless(dead: bool) -> Result<(), Stop> {
    if dead { Err(DeadCell.into()) } else { Ok(()) }
}

/// The stop for taking `v` apart as a constructor it is not.
#[cold]
#[inline(never)]
fn opened_other(v: Value) -> Stop {
    Stop::broken(Broken::OpenedOther(v.view()))
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
fn array(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    dst: Dst<Reg>,
    elements: &[Reg],
) -> Result<(), Stop> {
    let mut values = Vec::with_capacity(elements.len());
    for r in elements {
        values.push(win.share(&mut rest.heap, Shared(*r))?);
    }
    let cell = array::from_values(&mut rest.heap, &values).map_err(fault)?;
    Ok(win.put(&mut rest.heap, dst, Value::cell(cell))?)
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
            let left = array::range(&mut rest.heap, start.wrapping_add_unsigned(n), end);
            if src.moved && left.is_ok() {
                let range = win.empty(src.reg);
                rest.release(range)?;
            }
            left
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

/// The stop for a switch that has nowhere to go for its value.
#[cold]
#[inline(never)]
fn no_case() -> Stop {
    Stop::broken(Broken::NoArm)
}

/// Field `index` of the value in `src` into `dst`, for a cell too long for the
/// arena and for a field that is a cell and is not counted. Any other that comes
/// here has no such field.
#[inline(never)]
fn far_field(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    dst: Dst<Reg>,
    src: Src<Reg>,
    index: u16,
) -> Result<(), Stop> {
    let cell = win.look(src).as_cell();
    let field = cell.and_then(|c| rest.heap.field(c, usize::from(index)));
    let v = field.ok_or_else(|| Stop::broken(Broken::NoField { index }))?;
    let v = rest.share(v)?;
    Ok(win.put(&mut rest.heap, dst, v)?)
}

/// [`far_field`], of a capture of the closure the frame runs as, for the
/// instruction before `pc`.
#[inline(never)]
fn far_capture(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    dst: Dst<Reg>,
    index: u32,
    pc: usize,
) -> Result<(), Stop> {
    let cell = win.closure().as_cell();
    let capture = cell.and_then(|c| rest.heap.capture(c, index));
    let v = capture.ok_or_else(|| no_capture(rest.code, pc, index))?;
    let v = rest.share(v)?;
    Ok(win.put(&mut rest.heap, dst, v)?)
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
    let roots = stack
        .iter()
        .chain(&rest.globals)
        .copied()
        .chain(std::iter::once(last))
        .chain(rest.env.map(Value::cell));
    Ok(rest.heap.audit(roots)?)
}

/// Check that `v` is what `may` says.
#[cold]
#[inline(never)]
fn check_made(heap: &Heap, v: Value, may: Makes) -> Result<(), Stop> {
    let cell = v.as_cell();
    let fits = match may {
        Makes::NoCell => cell.is_none(),
        Makes::Int => cell.is_none_or(|c| heap.is(c, Kind::BigInt)),
        Makes::Any => true,
    };
    if fits {
        Ok(())
    } else {
        Err(Stop::broken(Broken::UnexpectedCell(v.view())))
    }
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
