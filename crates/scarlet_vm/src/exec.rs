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

use crate::array::{self, End, Flat, Found, Seq, Stored, Wrote, flat};
use crate::bigint;
use crate::code::{
    Arg, AssignList, BitsOp, Code, Counted, Dead, Dst, Entry, Hands, Hit, Hits, Ins, Makes,
    MaybeDst, MaybeHit, Near, Op, Reg, Room, Sets, Split, SplitWide, Src, Taken,
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
use crate::word::to_usize;
use crate::{Audit, Stop};

mod rest;

use rest::{Rest, Span, empty_cell, not_an_array};

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
            Ins::Array { dst, ref elements } => array_apart(win.lend(), rest, dst, elements)?,
            Ins::ArrayWide { dst, elements } => {
                array_apart(win.lend(), rest, dst, elements.args(&code.lists))?;
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
                    | View::EmptyArray
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
                let of = win.look(src);
                let len = array::seq(&rest.heap, of).map(|s| s.len(&rest.heap));
                let small = len.and_then(|n| i64::try_from(n).ok());
                let len = match small.and_then(Value::int) {
                    Some(len) => len,
                    None => rest.array_len(of)?,
                };
                win.put(&mut rest.heap, dst, len)?;
            }
            Ins::ArrayPush {
                dst,
                src,
                item,
                end,
            } => array_push(win.lend(), rest, dst, src, item, end)?,
            Ins::ArrayExtend {
                dst,
                src,
                items,
                end,
            } => {
                let items = items.args(&code.lists);
                array_extend(win.lend(), rest, dst, src, items, end)?;
            }
            Ins::ArrayUncons {
                head,
                tail,
                src,
                ref miss,
            } => {
                let found = match src.moved {
                    true => array_uncons(win.lend(), rest, head, tail, src)?,
                    false => uncons_apart(win.lend(), rest, head, tail, src)?,
                };
                if !found {
                    pc = miss.index();
                }
            }
            Ins::ArraySplit {
                ref elements,
                tail,
                src,
                split,
                ref miss,
            } => {
                if !array_split(win.lend(), rest, elements, tail, src, split)? {
                    pc = miss.index();
                }
            }
            Ins::JumpUnlessLength {
                src,
                length,
                ref to,
            } => {
                let of = win.look(src);
                let s = array::seq(&rest.heap, of).ok_or_else(|| not_an_array(of))?;
                if !array::fits(s.len(&rest.heap), length) {
                    pc = to.index();
                }
            }
            Ins::ArraySplitWide {
                elements,
                tail,
                src,
                ref miss,
            } => {
                let wide = elements.of(&code.lists).ok_or_else(no_split)?;
                if !split_wide(win.lend(), rest, wide, tail, src)? {
                    pc = miss.index();
                }
            }
            Ins::ArrayConcat { dst, a, b } => {
                array_concat(win.lend(), rest, dst, a, b)?;
            }
            Ins::ArraySlice {
                dst,
                src,
                start,
                end,
                miss,
            } => {
                if !array_slice(win.lend(), rest, dst, src, [start, end])? {
                    pc = miss.index();
                }
            }
            Ins::ArraySet {
                dst,
                src,
                index,
                value,
                miss,
            } => {
                if !array_set(win.lend(), rest, dst, src, index, value)? {
                    pc = miss.index();
                }
            }
            Ins::ArrayGet {
                dst,
                src,
                index,
                miss,
            } => {
                let (of, index) = (win.look(src), win.look(index));
                match dst.hit() {
                    Some(to) => match flat::near(&rest.heap, of, index) {
                        array::Near::Found(v)
                            if v.as_cell().is_none_or(|c| rest.heap.count_up(c)) =>
                        {
                            win.hit(&mut rest.heap, to, v)?;
                        }
                        array::Near::Past => pc = miss.index(),
                        array::Near::Found(_) | array::Near::Other => {
                            if !array_get(win.lend(), rest, to, of, index)? {
                                pc = miss.index();
                            }
                        }
                    },
                    None => {
                        if !rest.has_index(of, index)? {
                            pc = miss.index();
                        }
                    }
                }
            }
            Ins::Bad { why } => return Err(code.why(why)),
            Ins::JumpIfFalse { cond, ref to } => match win.look(cond).view() {
                View::Bool(true) => {}
                View::Bool(false) => pc = to.index(),
                v @ (View::Float(_)
                | View::Int(_)
                | View::Nil
                | View::EmptyArray
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
            Ins::MapGet {
                dst,
                map,
                key,
                miss,
            } => {
                let m = rest.map(win.look(map))?;
                match (map::get(&rest.heap, m, win.look(key)), dst.hit()) {
                    (Some(v), Some(to)) => {
                        let v = rest.share(v)?;
                        win.hit(&mut rest.heap, to, v)?;
                    }
                    (Some(_), None) => {}
                    (None, _) => pc = miss.index(),
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

/// A new array of what `given` hands on in `dst`. As in [`new_cell`], the operands are gone
/// over twice, and an array that is refused has taken nothing.
#[inline(always)]
fn array<G: Hands + ?Sized>(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    dst: Dst<Reg>,
    given: &G,
) -> Result<(), Stop> {
    let len = given.hands().len();
    if len == 0 || len > flat::MOST {
        return long_array(win, rest, dst, given);
    }
    for a in given.hands() {
        win.add(&mut rest.heap, a)?;
    }
    let values = given.hands().map(|a| win.hand(a));
    match flat::in_freed(&mut rest.heap, values) {
        Some(made) => Ok(win.put(&mut rest.heap, dst, made)?),
        None => array_at_end(win, rest, dst, given),
    }
}

/// [`array`], for an instruction that is a call out of the loop.
#[inline(never)]
fn array_apart<G: Hands + ?Sized>(
    win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    dst: Dst<Reg>,
    given: &G,
) -> Result<(), Stop> {
    array(win, rest, dst, given)
}

/// The rest of [`array`] when no freed cell has the size.
#[inline(never)]
fn array_at_end<G: Hands + ?Sized>(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    dst: Dst<Reg>,
    given: &G,
) -> Result<(), Stop> {
    let mut from = win.lend();
    let values = given.hands().map(move |a| from.hand(a));
    match flat::at_end(&mut rest.heap, values) {
        Ok(made) => Ok(win.put(&mut rest.heap, dst, made)?),
        Err(refused) => {
            for a in given.hands() {
                win.take_back(&mut rest.heap, a)?;
            }
            Err(fault(refused))
        }
    }
}

/// [`array`] of more elements than one cell has, or of none.
#[inline(never)]
fn long_array<G: Hands + ?Sized>(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    dst: Dst<Reg>,
    given: &G,
) -> Result<(), Stop> {
    let mut values = Vec::with_capacity(given.hands().len());
    for a in given.hands() {
        values.push(win.pass(&mut rest.heap, a)?);
    }
    let made = array::from_values(&mut rest.heap, &values).map_err(fault)?;
    Ok(win.put(&mut rest.heap, dst, made)?)
}

/// The array in `src`, which is `s`, as one that stores its elements, with one
/// reference. The reference is the register's when `src` gives it up, so
/// an array that nothing else holds has a count of 1. A range stores none, so the array of
/// its elements is built.
#[inline(always)]
fn to_change(
    win: &mut Window<'_>,
    rest: &mut Rest<'_, '_>,
    src: Arg<Reg>,
    s: Seq,
) -> Result<Stored, Stop> {
    match s {
        // There is no reference to take.
        Seq::Empty => Ok(Stored::Empty),
        Seq::Flat(flat) => {
            take_or_share(win, rest, src, flat.cell())?;
            Ok(Stored::Flat(flat))
        }
        Seq::Tree(cell) => {
            take_or_share(win, rest, src, cell)?;
            Ok(Stored::Tree(cell))
        }
        Seq::Range { start, end } => {
            let a = rest.built(start, end)?;
            let_go(win, rest, src)?;
            Ok(a)
        }
    }
}

/// Give up what the register of `src` holds, when `src` does.
#[inline(always)]
fn let_go(win: &mut Window<'_>, rest: &mut Rest<'_, '_>, src: Arg<Reg>) -> Result<(), DeadCell> {
    match src.moved {
        true => {
            let held = win.empty(src.reg);
            rest.release(held)
        }
        false => Ok(()),
    }
}

/// A reference to `cell`, which the register of `src` holds. It is the register's own when
/// `src` gives it up.
#[inline(always)]
fn take_or_share(
    win: &mut Window<'_>,
    rest: &mut Rest<'_, '_>,
    src: Arg<Reg>,
    cell: Cell,
) -> Result<(), DeadCell> {
    if src.moved {
        win.empty(src.reg);
        Ok(())
    } else {
        rest.heap.share(cell)
    }
}

/// The array in `src` with the value in `item` at `end` of it.
#[inline(never)]
fn array_push(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    dst: Dst<Reg>,
    src: Arg<Reg>,
    item: Arg<Reg>,
    end: End,
) -> Result<(), Stop> {
    let v = win.get(src.reg);
    if src.moved
        && let Some(cell) = v.as_cell()
    {
        win.add(&mut rest.heap, item)?;
        let x = win.get(item.reg);
        let pushed = match flat::push_alone(&mut rest.heap, cell, x, end) {
            true => Ok(true),
            false => array::push_alone(&mut rest.heap, cell, x, end),
        };
        if let Ok(true) = pushed {
            win.hand(item);
            // It is nearly always the same register, which then holds its own word.
            if dst.0 != src.reg {
                win.empty(src.reg);
                win.put(&mut rest.heap, dst, v)?;
            }
            return Ok(());
        }
        win.take_back(&mut rest.heap, item)?;
        pushed.map_err(fault)?;
    }
    push_apart(win, rest, dst, src, item, end)
}

/// [`array_push`] of any array.
#[inline(never)]
fn push_apart(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    dst: Dst<Reg>,
    src: Arg<Reg>,
    item: Arg<Reg>,
    end: End,
) -> Result<(), Stop> {
    let s = rest.seq(win.get(src.reg))?;
    let x = win.pass(&mut rest.heap, item)?;
    let a = match to_change(&mut win, rest, src, s) {
        Ok(a) => a,
        Err(stop) => return Err(rest.give_back([x], stop)),
    };
    let pushed = array::push(&mut rest.heap, a, x, end).map_err(fault)?;
    Ok(win.put(&mut rest.heap, dst, pushed)?)
}

/// The array in `src` with the values in `items` at `end` of it.
#[inline(never)]
fn array_extend(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    dst: Dst<Reg>,
    src: Arg<Reg>,
    items: &[Arg<Reg>],
    end: End,
) -> Result<(), Stop> {
    let s = rest.seq(win.get(src.reg))?;
    let mut values: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::with_capacity(items.len());
    for a in items {
        values.push(win.pass(&mut rest.heap, *a)?);
    }
    let a = match to_change(&mut win, rest, src, s) {
        Ok(a) => a,
        Err(stop) => return Err(rest.give_back(values, stop)),
    };
    let longer = array::extend(&mut rest.heap, a, &values, end).map_err(fault)?;
    Ok(win.put(&mut rest.heap, dst, longer)?)
}

/// The array in `a` and then the one in `b`.
#[inline(never)]
fn array_concat(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    dst: Dst<Reg>,
    a: Arg<Reg>,
    b: Arg<Reg>,
) -> Result<(), Stop> {
    let (l, r) = (rest.seq(win.get(a.reg))?, rest.seq(win.get(b.reg))?);
    let l = to_change(&mut win, rest, a, l)?;
    let r = match to_change(&mut win, rest, b, r) {
        Ok(r) => r,
        Err(stop) => return Err(rest.give_back([l.value()], stop)),
    };
    // A range that is all of the join is an array of its elements by now. Kept as a range it
    // would be built again by each push on it and each write to it.
    let joined = array::concat(&mut rest.heap, l, r).map_err(fault)?;
    Ok(win.put(&mut rest.heap, dst, joined)?)
}

/// The first element of the array in `src`, which this takes, into `head` and the array of
/// the others into `tail`. Whether it has one.
#[inline(never)]
fn array_uncons(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    head: MaybeHit<Reg>,
    tail: Hit<Reg>,
    src: Arg<Reg>,
) -> Result<bool, Stop> {
    let v = win.get(src.reg);
    // An array that is one cell, which this takes and nothing else holds, is asked first,
    // then a tree that only leaves one more of its head behind, and then a range.
    let (first, left) = match v.as_cell() {
        Some(cell) => match flat::take_first(&mut rest.heap, cell) {
            Some(first) if first.only => {
                rest.heap.discard(cell)?;
                (first.value, Value::EMPTY_ARRAY)
            }
            Some(first) => (first.value, v),
            None => match array::leave_first(&mut rest.heap, cell)? {
                Some(first) => (first, v),
                None => match array::next_of_range(&mut rest.heap, cell)? {
                    Some(first) => (first, v),
                    None => return uncons_apart(win, rest, head, tail, src),
                },
            },
        },
        None => return uncons_apart(win, rest, head, tail, src),
    };
    win.empty(src.reg);
    match head.hit() {
        Some(head) => win.hit(&mut rest.heap, head, first)?,
        None => rest.release(first)?,
    }
    win.hit(&mut rest.heap, tail, left)?;
    Ok(true)
}

/// [`array_uncons`] of any array, taken or not.
#[inline(never)]
fn uncons_apart(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    head: MaybeHit<Reg>,
    tail: Hit<Reg>,
    src: Arg<Reg>,
) -> Result<bool, Stop> {
    let head = head.hit();
    let v = win.get(src.reg);
    let s = rest.seq(v)?;
    let wants = head.is_some();
    let found = match s {
        Seq::Empty => return Ok(false),
        Seq::Range { start, end } => rest.uncons_range(start, end, wants)?,
        Seq::Flat(flat) => flat::uncons(&mut rest.heap, flat, wants).map_err(fault)?,
        Seq::Tree(cell) if src.moved => {
            let found = array::uncons_owned(&mut rest.heap, cell, wants);
            // It has taken what the register held.
            if !matches!(found, Ok(None)) {
                win.empty(src.reg);
            }
            found.map_err(fault)?
        }
        Seq::Tree(cell) => array::uncons(&mut rest.heap, cell, wants).map_err(fault)?,
    };
    let (first, left) = found.ok_or_else(empty_cell)?;
    let_go(&mut win, rest, src)?;
    if let Some(head) = head {
        win.hit(&mut rest.heap, head, first)?;
    }
    win.hit(&mut rest.heap, tail, left)?;
    Ok(true)
}

/// Some elements of the array in `src` into `elements`, and what it has behind those
/// that `split` counts into `tail`. Whether it has the length that `split` says.
#[inline(never)]
fn array_split(
    win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    elements: &Hits<Reg, 4>,
    tail: MaybeHit<Reg>,
    src: Arg<Reg>,
    split: Near,
) -> Result<bool, Stop> {
    split_in::<4>(win, rest, elements.regs(), tail, src, split.split())
}

/// [`array_split`] of any number of elements.
#[inline(never)]
fn split_wide(
    win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    wide: &SplitWide<Reg>,
    tail: MaybeHit<Reg>,
    src: Arg<Reg>,
) -> Result<bool, Stop> {
    split_in::<16>(win, rest, &wide.elements, tail, src, wide.split)
}

/// [`array_split`] with room for `N` elements, and for more where there are more.
#[inline(always)]
fn split_in<const N: usize>(
    win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    elements: &[MaybeHit<Reg>],
    tail: MaybeHit<Reg>,
    src: Arg<Reg>,
    split: Split,
) -> Result<bool, Stop> {
    let v = win.get(src.reg);
    let Some(cell) = v.as_cell() else {
        return split_empty(win, rest, (elements, tail), v, split);
    };
    if let Some(flat) = Flat::of(&rest.heap, cell) {
        return split_flat::<N>(win, rest, elements, tail, src, (split, flat));
    }
    // All zeros, which costs the least to write.
    let mut room = [Value::from_bits(0); N];
    let mut more = Vec::new();
    let found = match room.get_mut(..elements.len()) {
        Some(room) => room,
        None => {
            more.resize(elements.len(), Value::NIL);
            more.as_mut_slice()
        }
    };
    match array::split(&rest.heap, cell, split, found) {
        Found::Miss => return Ok(false),
        Found::All => {}
        Found::Apart(r) => rest.run(&r, split.from as usize, found)?,
        Found::Other => return split_range(win, rest, (elements, tail), src, split, found),
    }
    let found = &*found;
    for (hit, v) in elements.iter().zip(found) {
        if hit.hit().is_some() {
            rest.heap.own(*v)?;
        }
    }
    let array = Stored::Tree(cell);
    put_found(
        win,
        rest,
        (elements, tail),
        src,
        split,
        (array, cell, found),
    )
}

/// [`split_in`] of what is no cell, which is the empty array and nothing else.
#[inline(never)]
fn split_empty(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    (elements, tail): (&[MaybeHit<Reg>], MaybeHit<Reg>),
    v: Value,
    split: Split,
) -> Result<bool, Stop> {
    rest.seq(v)?;
    if !array::fits(0, split.length) {
        return Ok(false);
    }
    if !elements.is_empty() {
        let index = split.from as usize;
        return Err(Stop::broken(Broken::NoArrayElement { index }));
    }
    if let Some(tail) = tail.hit() {
        win.hit(&mut rest.heap, tail, v)?;
    }
    Ok(true)
}

/// [`split_in`] of a range. Its elements are made, in `found`.
#[inline(always)]
fn split_range(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    (elements, tail): (&[MaybeHit<Reg>], MaybeHit<Reg>),
    src: Arg<Reg>,
    split: Split,
    found: &mut [Value],
) -> Result<bool, Stop> {
    let v = win.get(src.reg);
    let range = v.as_cell().and_then(|c| array::range_at(&rest.heap, c));
    let (start, end) = range.ok_or_else(|| not_an_array(v))?;
    let len = array::range_len(start, end);
    if !array::fits(len, split.length) {
        return Ok(false);
    }
    let first = start.wrapping_add(i64::from(split.from));
    rest.run_of_range(first, elements, found)?;
    let left = match tail.hit() {
        Some(tail) => {
            let from = u64::from(split.length.len);
            match array::slice_of_range(&mut rest.heap, start, from, len) {
                Ok(left) => Some((tail, left)),
                Err(why) => return Err(give_back_wanted(rest, elements, found, fault(why))),
            }
        }
        None => None,
    };
    let_go(&mut win, rest, src)?;
    put_split(win, rest, elements, found, left)?;
    Ok(true)
}

/// [`split_in`] of a short array. One that this takes and nothing else holds hands its
/// elements over.
#[inline(always)]
fn split_flat<const N: usize>(
    win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    elements: &[MaybeHit<Reg>],
    tail: MaybeHit<Reg>,
    src: Arg<Reg>,
    (split, flat): (Split, Flat),
) -> Result<bool, Stop> {
    if !array::fits(flat.len() as u64, split.length) {
        return Ok(false);
    }
    let cell = flat.cell();
    let mut room = [Value::from_bits(0); N];
    let mut more = Vec::new();
    let found = match room.get_mut(..elements.len()) {
        Some(room) => room,
        None => {
            more.resize(elements.len(), Value::NIL);
            more.as_mut_slice()
        }
    };
    let from = split.from as usize;
    let wanted = || elements.iter().map(|hit| hit.hit().is_some());
    let taken = src.moved && flat::take_run(&mut rest.heap, cell, from, wanted(), found);
    if !taken {
        flat.read_run(&rest.heap, from, found);
        for (v, wanted) in found.iter().zip(wanted()) {
            if wanted {
                rest.heap.own(*v)?;
            }
        }
    }
    let found = &*found;
    let array = Stored::Flat(flat);
    put_found(
        win,
        rest,
        (elements, tail),
        src,
        split,
        (array, cell, found),
    )
}

/// Put what a split has `found`, with a reference to each, and what is left behind it of the
/// `array` that `cell` is, in their registers.
#[inline(always)]
fn put_found(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    (elements, tail): (&[MaybeHit<Reg>], MaybeHit<Reg>),
    src: Arg<Reg>,
    split: Split,
    (array, cell, found): (Stored, Cell, &[Value]),
) -> Result<bool, Stop> {
    let left = match tail.hit() {
        Some(tail) => {
            take_or_share(&mut win, rest, src, cell)?;
            match array::skip(&mut rest.heap, array, split.length.len as usize) {
                Ok(left) => Some((tail, left)),
                Err(why) => return Err(give_back_wanted(rest, elements, found, fault(why))),
            }
        }
        None => {
            if src.moved {
                win.empty(src.reg);
            }
            None
        }
    };
    put_split(win, rest, elements, found, left)?;
    if left.is_none() && src.moved {
        rest.heap.release(cell)?;
    }
    Ok(true)
}

/// Put what a split found, and what is left behind it, in their registers.
#[inline(always)]
fn put_split(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    elements: &[MaybeHit<Reg>],
    found: &[Value],
    left: Option<(Hit<Reg>, Value)>,
) -> Result<(), DeadCell> {
    for (hit, v) in elements.iter().zip(found) {
        if let Some(to) = hit.hit() {
            win.hit(&mut rest.heap, to, *v)?;
        }
    }
    match left {
        Some((tail, left)) => win.hit(&mut rest.heap, tail, left),
        None => Ok(()),
    }
}

/// `stop`, where what a split found is given up.
#[cold]
#[inline(never)]
fn give_back_wanted(
    rest: &mut Rest<'_, '_>,
    elements: &[MaybeHit<Reg>],
    found: &[Value],
    stop: Stop,
) -> Stop {
    let wanted = elements.iter().zip(found);
    let wanted = wanted.filter(|(hit, _)| hit.hit().is_some());
    rest.give_back(wanted.map(|(_, v)| *v), stop)
}

/// The stop for an instruction whose list is not there.
#[cold]
#[inline(never)]
fn no_split() -> Stop {
    Stop::broken(Broken::NoPlace)
}

/// The elements of the array in `src` from the first of `ends` up to the second,
/// into `dst`. Whether the array has them.
#[inline(never)]
fn array_slice(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    dst: MaybeHit<Reg>,
    src: Arg<Reg>,
    ends: [Src<Reg>; 2],
) -> Result<bool, Stop> {
    let [start, end] = ends.map(|r| win.look(r));
    let Some(span) = rest.span(win.get(src.reg), start, end)? else {
        return Ok(false);
    };
    let Some(to) = dst.hit() else {
        let_go(&mut win, rest, src)?;
        return Ok(true);
    };
    let Span {
        of,
        from,
        to: until,
    } = span;
    let cut = match of {
        Seq::Flat(_) | Seq::Tree(_) if from < until => {
            let array = to_change(&mut win, rest, src, of)?;
            array::slice(&mut rest.heap, array, from, until).map_err(fault)?
        }
        Seq::Empty | Seq::Flat(_) | Seq::Tree(_) => {
            let_go(&mut win, rest, src)?;
            Value::EMPTY_ARRAY
        }
        Seq::Range { start, .. } => {
            let ends = (from as u64, until as u64);
            let cut = array::slice_of_range(&mut rest.heap, start, ends.0, ends.1);
            let cut = cut.map_err(fault)?;
            let_go(&mut win, rest, src)?;
            cut
        }
    };
    win.hit(&mut rest.heap, to, cut)?;
    Ok(true)
}

/// Element `index` of the array `of` into `to`, for any array. Whether it has one.
#[inline(never)]
fn array_get(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    to: Hit<Reg>,
    of: Value,
    index: Value,
) -> Result<bool, Stop> {
    // A tree is asked next, as a short array is asked first.
    let found = match (of.as_cell(), index.small_int()) {
        (Some(array), Some(i)) => array::near(&rest.heap, array, i),
        (Some(_) | None, _) => array::Near::Other,
    };
    match found {
        array::Near::Found(v) => {
            let v = rest.share(v)?;
            win.hit(&mut rest.heap, to, v)?;
            Ok(true)
        }
        array::Near::Past => Ok(false),
        array::Near::Other => array_get_apart(win, rest, to, of, index),
    }
}

/// [`array_get`] of what is no tree, or where the index is no small Int.
#[inline(never)]
fn array_get_apart(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    to: Hit<Reg>,
    of: Value,
    index: Value,
) -> Result<bool, Stop> {
    match rest.index(of, index)? {
        Some(v) => {
            win.hit(&mut rest.heap, to, v)?;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// The array in `src` with the value in `value` at the index in `index`, into `dst`. Whether
/// the array has the index. Where it has not, this has read the registers and changed
/// nothing. A write to an array that `src` gives up and nothing else holds is made in
/// place.
#[inline(never)]
fn array_set(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    dst: Hit<Reg>,
    src: Arg<Reg>,
    index: Src<Reg>,
    value: Arg<Reg>,
) -> Result<bool, Stop> {
    let v = win.get(src.reg);
    if src.moved
        && let (Some(cell), Some(i)) = (v.as_cell(), win.look(index).small_int())
    {
        win.add(&mut rest.heap, value)?;
        let x = win.get(value.reg);
        // One below 0 is past the end of a short array this way.
        let found = flat::set_alone(&mut rest.heap, cell, to_usize(i.cast_unsigned()), x);
        if let array::Near::Found(old) = found {
            win.hand(value);
            if let Some(old) = old.as_cell() {
                rest.heap.release(old)?;
            }
            // It is nearly always the same register, which then holds its own word.
            if dst.0 != src.reg {
                win.empty(src.reg);
                win.hit(&mut rest.heap, dst, v)?;
            }
            return Ok(true);
        }
        if let array::Near::Past = found {
            win.take_back(&mut rest.heap, value)?;
            return Ok(false);
        }
        let wrote = array::set_alone(&mut rest.heap, cell, i, x);
        if let Ok(Wrote::Done) = wrote {
            win.hand(value);
            if dst.0 != src.reg {
                win.empty(src.reg);
                win.hit(&mut rest.heap, dst, v)?;
            }
            return Ok(true);
        }
        win.take_back(&mut rest.heap, value)?;
        if let Wrote::Past = wrote.map_err(fault)? {
            return Ok(false);
        }
    }
    set_apart(win, rest, dst, src, index, value)
}

/// [`array_set`] of any array.
#[inline(never)]
fn set_apart(
    mut win: Window<'_>,
    rest: &mut Rest<'_, '_>,
    dst: Hit<Reg>,
    src: Arg<Reg>,
    index: Src<Reg>,
    value: Arg<Reg>,
) -> Result<bool, Stop> {
    let s = rest.seq(win.get(src.reg))?;
    let i = rest.position_in(win.look(index))?;
    let Some(i) = i.filter(|i| (*i as u64) < s.len(&rest.heap)) else {
        return Ok(false);
    };
    let x = win.pass(&mut rest.heap, value)?;
    let array = match to_change(&mut win, rest, src, s) {
        Ok(array) => array,
        Err(stop) => return Err(rest.give_back([x], stop)),
    };
    let written = array::set(&mut rest.heap, array, i, x).map_err(fault)?;
    win.hit(&mut rest.heap, dst, written)?;
    Ok(true)
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
