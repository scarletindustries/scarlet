//! The stack of registers, and the frame on it that is running.
//!
//! Every frame's registers are on one stack. A frame starts where its
//! caller's ends, and while it runs the interpreter holds the 256 registers
//! from its start as one array ([`Window`]). A register is named by a byte, so
//! every register an instruction can name is in the window, and the types say
//! so: reaching one tests nothing.
//!
//! | register | holds                                                       |
//! |----------|-------------------------------------------------------------|
//! | 0        | where the frame returns to ([`Link`])                       |
//! | 1        | the closure it runs as, when it reads that or its captures  |
//! | 2 and up | its parameters in order, then everything else               |
//!
//! Each register holds one reference to its value. A frame that ends gives up
//! what its registers hold and leaves no cell behind, so the registers above
//! the running frame's never hold one, and neither does register 1 of a frame
//! that keeps no closure.

use crate::code::{Arg, Dead, Dst, Hit, Last, Reg, Room, Shared, Src, Taken};
use crate::heap::{DeadCell, Heap};
use crate::value::Value;
use crate::word::{low_byte, low_u16, low_u32};

/// How many registers an instruction can name.
pub(crate) const WINDOW: usize = 256;

/// The register that holds a frame's [`Link`].
pub(crate) const LINK: Reg = Reg(0);

/// The register that holds the closure a frame runs as.
pub(crate) const CLOSURE: Reg = Reg(1);

/// The register of a frame's first parameter.
pub(crate) const FIRST: Reg = Reg(2);

/// The most values a frame holds at once.
pub(crate) const MAX_VALUES: usize = WINDOW - FIRST.0 as usize;

/// Where the result of a frame that nothing called goes.
pub(crate) const RESULT: Reg = FIRST;

/// Where a frame returns to, in one word: the instruction it goes on at, how
/// far below it its caller starts, and the caller's register for the result.
///
/// The word is a Float to anything that asks what it holds, so nothing that
/// gives up a frame's registers mistakes it for a cell.
#[derive(Clone, Copy)]
pub(crate) struct Link(u64);

impl Link {
    const BELOW: u32 = 32;
    const DST: u32 = 48;

    /// The link of a frame that returns to instruction `pc` of a frame that
    /// has `own` registers. The loader gives every instruction a number of 32
    /// bits (`code::emit`), so the three have a part of the word each.
    #[inline(always)]
    pub(crate) fn new(pc: usize, own: Room, dst: Dst<Reg>) -> Link {
        let below = u64::from(own.0) << Link::BELOW;
        let dst = u64::from(dst.0.0) << Link::DST;
        Link(pc as u64 | below | dst)
    }

    /// The instruction the frame returns to.
    #[inline(always)]
    pub(crate) fn pc(self) -> usize {
        low_u32(self.0) as usize
    }

    /// How far below the frame its caller starts, which is 0 for a frame
    /// nothing called.
    #[inline(always)]
    pub(crate) fn below(self) -> u32 {
        u32::from(low_u16(self.0 >> Link::BELOW))
    }

    /// The caller's register for the result.
    #[inline(always)]
    pub(crate) fn dst(self) -> Dst<Reg> {
        Dst(Reg(low_byte(self.0 >> Link::DST)))
    }
}

/// The registers of the running frame, and those after them up to as many as
/// an instruction can name.
pub(crate) struct Window<'s>(&'s mut [Value; WINDOW]);

/// The window at `base`, when the stack reaches that far.
#[inline(always)]
pub(crate) fn window(stack: &mut [Value], base: u32) -> Option<Window<'_>> {
    let start = base as usize;
    let end = start.wrapping_add(WINDOW);
    stack.get_mut(start..end)?.first_chunk_mut().map(Window)
}

/// How many frames there are, the one at `base` the last. It is `None` when a
/// link on the way leads to no frame. Nothing called the frame at the bottom,
/// and only that one.
pub(crate) fn depth(stack: &[Value], mut base: u32) -> Option<usize> {
    let mut frames = 1usize;
    while base > 0 {
        let below = Link(stack.get(base as usize)?.bits()).below();
        base = base.checked_sub(below).filter(|caller| *caller < base)?;
        frames = frames.saturating_add(1);
    }
    let bottom = Link(stack.first()?.bits());
    (bottom.below() == 0).then_some(frames)
}

impl Window<'_> {
    /// The same window, for something to use and give back.
    #[inline(always)]
    pub(crate) fn lend(&mut self) -> Window<'_> {
        Window(self.0)
    }

    pub(crate) fn regs(&self) -> &[Value; WINDOW] {
        self.0
    }

    // How a handler reaches a register. Each takes the operand as its
    // instruction declares it, so what a handler does to a register is what
    // `Ins::operands` says.

    #[inline(always)]
    pub(crate) fn look(&self, r: Src<Reg>) -> Value {
        self.get(r.0)
    }

    /// The value in `r`, with one more reference.
    #[inline(always)]
    pub(crate) fn share(&self, heap: &mut Heap, r: Shared<Reg>) -> Result<Value, DeadCell> {
        heap.own(self.get(r.0))
    }

    /// The value in `r`, moved out. The register is emptied and gives up
    /// nothing, so its one reference passes to the caller.
    #[inline(always)]
    pub(crate) fn take(&mut self, r: Taken<Reg>) -> Value {
        self.empty(r.0)
    }

    /// The value an argument passes, with the register's reference when the
    /// argument takes it and with one more otherwise.
    #[inline(always)]
    pub(crate) fn pass(&mut self, heap: &mut Heap, a: Arg<Reg>) -> Result<Value, DeadCell> {
        if a.moved {
            Ok(self.empty(a.reg))
        } else {
            heap.own(self.get(a.reg))
        }
    }

    // What handing `a` on does, for a handler that has something to ask for in
    // between: the reference first, and the value after.

    /// Add the reference that handing `a` on needs, unless the register's own
    /// goes.
    #[inline(always)]
    pub(crate) fn add(&self, heap: &mut Heap, a: Arg<Reg>) -> Result<(), DeadCell> {
        if !a.moved {
            heap.own(self.get(a.reg))?;
        }
        Ok(())
    }

    /// The value `a` hands on, after [`Self::add`].
    #[inline(always)]
    pub(crate) fn hand(&mut self, a: Arg<Reg>) -> Value {
        if a.moved {
            self.empty(a.reg)
        } else {
            self.get(a.reg)
        }
    }

    /// Give up what [`Self::add`] added, when `a` is not handed on after all.
    pub(crate) fn take_back(&self, heap: &mut Heap, a: Arg<Reg>) -> Result<(), DeadCell> {
        if a.moved {
            return Ok(());
        }
        give_up(heap, self.get(a.reg))
    }

    /// Put `v` in `r`, whose old value is released. `v`'s reference passes to
    /// the register.
    #[inline(always)]
    pub(crate) fn put(&mut self, heap: &mut Heap, r: Dst<Reg>, v: Value) -> Result<(), DeadCell> {
        self.set(heap, r.0, v)
    }

    /// [`Self::put`], for an instruction that did not jump.
    #[inline(always)]
    pub(crate) fn hit(&mut self, heap: &mut Heap, r: Hit<Reg>, v: Value) -> Result<(), DeadCell> {
        self.set(heap, r.0, v)
    }

    #[inline(always)]
    pub(crate) fn drop(&mut self, heap: &mut Heap, r: Dead<Reg>) -> Result<(), DeadCell> {
        self.set(heap, r.0, Value::NIL)
    }

    // A register's number is less than the window's length, so none of these
    // ever finds its register missing, and the compiler leaves the test out.

    #[inline(always)]
    pub(crate) fn get(&self, r: Reg) -> Value {
        self.0.get(usize::from(r.0)).copied().unwrap_or(Value::NIL)
    }

    #[inline(always)]
    pub(crate) fn set(&mut self, heap: &mut Heap, r: Reg, v: Value) -> Result<(), DeadCell> {
        match self.0.get_mut(usize::from(r.0)) {
            Some(slot) => give_up(heap, std::mem::replace(slot, v)),
            None => Ok(()),
        }
    }

    /// Put `v` in `r`, and give what was there.
    #[inline(always)]
    pub(crate) fn swap(&mut self, r: Dst<Reg>, v: Value) -> Value {
        match self.0.get_mut(usize::from(r.0.0)) {
            Some(slot) => std::mem::replace(slot, v),
            None => v,
        }
    }

    #[inline(always)]
    pub(crate) fn empty(&mut self, r: Reg) -> Value {
        match self.0.get_mut(usize::from(r.0)) {
            Some(slot) => std::mem::replace(slot, Value::NIL),
            None => Value::NIL,
        }
    }

    // What a call and a return do to a whole frame.

    /// Where the frame returns to.
    #[inline(always)]
    pub(crate) fn link(&self) -> Link {
        Link(self.get(LINK).bits())
    }

    /// Start a frame here that returns where `link` says. Register 0 of a
    /// frame that is not running holds no cell, so nothing is given up.
    #[inline(always)]
    pub(crate) fn start(&mut self, link: Link) {
        if let Some(slot) = self.0.first_mut() {
            *slot = Value::from_bits(link.0);
        }
    }

    /// Put `args` in the parameters of a frame that is starting. None of them
    /// holds a cell yet.
    #[inline(always)]
    pub(crate) fn receive(&mut self, args: &[Value]) {
        if let Some(params) = self.0.get_mut(usize::from(FIRST.0)..) {
            for (slot, arg) in params.iter_mut().zip(args) {
                *slot = *arg;
            }
        }
    }

    /// The closure the frame runs as, when it keeps one.
    #[inline(always)]
    pub(crate) fn closure(&self) -> Value {
        self.get(CLOSURE)
    }

    /// Run the frame that is starting here as `closure`.
    #[inline(always)]
    pub(crate) fn run_as(&mut self, closure: Value) {
        if let Some(slot) = self.0.get_mut(usize::from(CLOSURE.0)) {
            *slot = closure;
        }
    }

    /// Give up what the registers from the first parameter to `last` hold, from
    /// the top down.
    #[inline(always)]
    pub(crate) fn leave(&mut self, heap: &mut Heap, last: Last) -> Result<(), DeadCell> {
        let Last(Reg(mut r)) = last;
        loop {
            if let Some(slot) = self.0.get_mut(usize::from(r)) {
                empty(heap, slot)?;
            }
            if r <= FIRST.0 {
                return Ok(());
            }
            r = r.wrapping_sub(1);
        }
    }

    /// Give up what the frame of `own` registers holds, from the top down, and
    /// leave `values` in its parameters. The closure stays.
    #[inline(always)]
    pub(crate) fn rewrite(
        &mut self,
        heap: &mut Heap,
        own: Room,
        values: &[Value],
    ) -> Result<(), DeadCell> {
        let from = usize::from(FIRST.0);
        let params = from.wrapping_add(values.len());
        if let Some(above) = self.0.get_mut(params..own.len()) {
            for slot in above.iter_mut().rev() {
                empty(heap, slot)?;
            }
        }
        if let Some(params) = self.0.get_mut(from..params) {
            for (slot, v) in params.iter_mut().zip(values).rev() {
                give_up(heap, std::mem::replace(slot, *v))?;
            }
        }
        Ok(())
    }
}

/// Give up the reference `v` is.
#[inline(always)]
fn give_up(heap: &mut Heap, v: Value) -> Result<(), DeadCell> {
    match v.as_cell() {
        Some(cell) => heap.release(cell),
        None => Ok(()),
    }
}

/// Give up what `slot` holds. Only a cell is taken out of it: what else a
/// register held may stay there once its frame has ended.
#[inline(always)]
fn empty(heap: &mut Heap, slot: &mut Value) -> Result<(), DeadCell> {
    match slot.as_cell() {
        Some(cell) => {
            *slot = Value::NIL;
            heap.release(cell)
        }
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests;
