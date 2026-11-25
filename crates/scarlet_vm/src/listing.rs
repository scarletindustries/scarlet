//! The loaded instructions as text, for `scarlet dis --instrs`.
//!
//! One line per instruction: its number in its function, which a jump names,
//! then the registers it writes, its kind, and the rest of it in the order its
//! fields are declared. A register is `r3`. A frame's first two are its own
//! (`frame.rs`), so its parameters start at `r2`. An operand written `move r3`
//! gives up the register's reference instead of adding one. `_` is a field
//! taken out of a cell that goes to no register. A constructor is its type and
//! its variant, `10.0`, and a test of one is the variant alone, because the test
//! does not look at the type: `.1` has no fields and `.1(..)` has some. `-> 7`
//! is where the instruction may continue instead of at the next one. Each kind
//! is spelt as a counted run spells it (`counts.rs`), so a count can be read
//! against a listing.

use std::fmt::{self, Write as _};

use scarlet_ir::core_ir::{ConstId, FuncIdx, GlobalSlot, Program};
use scarlet_ir::intrinsic::Intrinsic;

use crate::code::{self, Code, Entry, Ins, Reg};
use crate::code::{
    Arg, ArgList, Args, BinaryId, BitsOp, BlockId, Dead, DeadList, Dst, DstList, EntryPc, Hit,
    Last, LookList, Looks, MaybeDst, Newline, Pc, Room, ShareList, Shared, Shares, Src, Taken,
    TextId, Why, WhyId,
};
use crate::frame::FIRST;
use crate::heap::Ctor;
use crate::ops::NumOp;
use crate::stop::NotBuilt;
use crate::value::{Float, SmallInt, Tag, Value, View};
use crate::{Audit, Stop};

/// Every function of a program as the VM loaded it.
pub struct Listing {
    fns: Vec<String>,
    toplevel: String,
}

impl Listing {
    /// The instructions of function `index`, which a Core IR listing calls
    /// `fn#index`, or `None` when the program has no such function.
    pub fn function(&self, index: usize) -> Option<&str> {
        self.fns.get(index).map(String::as_str)
    }

    /// The instructions of the entry file's toplevel.
    pub fn toplevel(&self) -> &str {
        &self.toplevel
    }
}

/// Load `program` as a run would, and list what was loaded.
pub fn listing(program: &Program) -> Result<Listing, Stop> {
    let code = code::load(program, Audit::Never)?;
    let function = |entry| function(&code, entry);
    Ok(Listing {
        fns: (&code.funcs).into_iter().map(function).collect(),
        toplevel: code.toplevels.last().map(function).unwrap_or_default(),
    })
}

fn function(code: &Code, entry: &Entry) -> String {
    let body = code.ins.get(entry.pc.index()..entry.end.index());
    let body = body.unwrap_or_default();
    // A function that did not load is one instruction, which says why.
    if let [Ins::Bad { why }] = body
        && let Some(Why::Missing(what)) = code.whys.get(why.0 as usize)
    {
        let what = NotBuilt::from(what.clone());
        return format!("; not loaded: the VM does not run {what} yet\n");
    }
    let mut out = String::new();
    // Writing to a `String` cannot fail.
    let _ = write_body(&mut out, code, entry, body);
    out
}

fn write_body(out: &mut String, code: &Code, entry: &Entry, body: &[Ins<Reg>]) -> fmt::Result {
    let (regs, params) = (entry.own.len(), entry.params);
    write!(out, "; loaded: {regs} registers")?;
    if let Some(last) = params.checked_sub(1) {
        let s = if last == 0 { "" } else { "s" };
        write!(out, ", parameter{s} in {FIRST}")?;
        if last > 0 {
            write!(out, " to r{}", last.saturating_add(usize::from(FIRST.0)))?;
        }
    }
    out.push('\n');
    for (i, ins) in body.iter().enumerate() {
        let mut line = Line {
            code,
            entry,
            dsts: Vec::new(),
            parts: Vec::new(),
            to: None,
        };
        ins.show(&mut line);
        write!(out, "{i:>5}  ")?;
        if !line.dsts.is_empty() {
            write!(out, "{} = ", line.dsts.join(", "))?;
        }
        out.push_str(ins.op().name());
        if !line.parts.is_empty() {
            write!(out, "({})", line.parts.join(", "))?;
        }
        if let Some(to) = line.to {
            write!(out, " -> {to}")?;
        }
        out.push('\n');
    }
    Ok(())
}

/// One instruction's line, as its fields add themselves to it.
pub(crate) struct Line<'a> {
    code: &'a Code,
    entry: &'a Entry,
    dsts: Vec<String>,
    parts: Vec<String>,
    /// The number, in its function, of the instruction it may jump to.
    to: Option<usize>,
}

impl Line<'_> {
    fn part(&mut self, part: impl fmt::Display) {
        self.parts.push(part.to_string());
    }

    /// `dst` as one more of the registers written, or as none of them.
    fn writes(&mut self, dst: Option<Reg>) {
        let shown = dst.map_or_else(|| "_".to_string(), |r| r.to_string());
        self.dsts.push(shown);
    }
}

/// A field of an instruction, which adds itself to a listing's line.
pub(crate) trait Show {
    fn show(self, line: &mut Line<'_>);
}

impl fmt::Display for Reg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "r{}", self.0)
    }
}

impl fmt::Display for Arg<Reg> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.moved {
            f.write_str("move ")?;
        }
        self.reg.fmt(f)
    }
}

/// A value an instruction holds as a constant.
struct Literal(Value);

impl fmt::Display for Literal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0.view() {
            View::Float(x) => write!(f, "{x:?}"),
            View::Int(n) => write!(f, "{n}"),
            View::Nil => f.write_str("nil"),
            View::Bool(b) => write!(f, "{b}"),
            View::Func(func) => write!(f, "{func}"),
            View::Nullary(v) => write!(f, "{}.{}", v.type_id.0, v.variant_idx),
            // The loader makes no constant that is a cell.
            View::Cell(_) => f.write_str("a cell"),
        }
    }
}

/// Bytes as the text they spell, quoted.
fn text(code: &Code, id: TextId) -> String {
    let bytes: &[u8] = code.texts.get(id.0 as usize).map_or(&[], |t| t);
    format!("{:?}", String::from_utf8_lossy(bytes))
}

impl Show for Dst<Reg> {
    fn show(self, line: &mut Line<'_>) {
        line.writes(Some(self.0));
    }
}

impl Show for MaybeDst<Reg> {
    fn show(self, line: &mut Line<'_>) {
        line.writes(self.dst().map(|dst| dst.0));
    }
}

impl<const N: usize> Show for [MaybeDst<Reg>; N] {
    fn show(self, line: &mut Line<'_>) {
        self.into_iter().for_each(|dst| dst.show(line));
    }
}

impl Show for DstList {
    fn show(self, line: &mut Line<'_>) {
        let code = line.code;
        self.dsts(&code.lists).iter().for_each(|d| d.show(line));
    }
}

impl Show for Hit<Reg> {
    fn show(self, line: &mut Line<'_>) {
        line.writes(Some(self.0));
    }
}

impl Show for Taken<Reg> {
    fn show(self, line: &mut Line<'_>) {
        line.part(Arg {
            reg: self.0,
            moved: true,
        });
    }
}

/// The fields that are one register an instruction does not write.
macro_rules! registers {
    ($($ty:ty),*) => {$(
        impl Show for $ty {
            fn show(self, line: &mut Line<'_>) {
                line.part(self.0);
            }
        }
    )*};
}

registers!(Src<Reg>, Shared<Reg>, Dead<Reg>);

impl Show for Arg<Reg> {
    fn show(self, line: &mut Line<'_>) {
        line.part(self);
    }
}

impl<const N: usize> Show for Args<Reg, N> {
    fn show(self, line: &mut Line<'_>) {
        self.iter().for_each(|a| line.part(a));
    }
}

impl<const N: usize> Show for [Dst<Reg>; N] {
    fn show(self, line: &mut Line<'_>) {
        self.into_iter().for_each(|dst| dst.show(line));
    }
}

impl<const N: usize> Show for [Taken<Reg>; N] {
    fn show(self, line: &mut Line<'_>) {
        self.into_iter().for_each(|r| r.show(line));
    }
}

impl<const N: usize> Show for [Shared<Reg>; N] {
    fn show(self, line: &mut Line<'_>) {
        self.into_iter().for_each(|r| r.show(line));
    }
}

impl Show for ArgList {
    fn show(self, line: &mut Line<'_>) {
        let code = line.code;
        self.args(&code.lists).iter().for_each(|a| line.part(a));
    }
}

impl<const N: usize> Show for Looks<Reg, N> {
    fn show(self, line: &mut Line<'_>) {
        self.regs().iter().for_each(|r| line.part(r));
    }
}

impl<const N: usize> Show for Shares<Reg, N> {
    fn show(self, line: &mut Line<'_>) {
        self.regs().iter().for_each(|r| line.part(r));
    }
}

impl Show for DeadList {
    fn show(self, line: &mut Line<'_>) {
        let code = line.code;
        self.regs(&code.lists).iter().for_each(|r| line.part(r));
    }
}

impl Show for LookList {
    fn show(self, line: &mut Line<'_>) {
        let code = line.code;
        self.regs(&code.lists).iter().for_each(|r| line.part(r));
    }
}

impl Show for ShareList {
    fn show(self, line: &mut Line<'_>) {
        let code = line.code;
        self.regs(&code.lists).iter().for_each(|r| line.part(r));
    }
}

impl Show for Pc {
    fn show(self, line: &mut Line<'_>) {
        line.to = self.index().checked_sub(line.entry.pc.index());
    }
}

impl Show for EntryPc {
    fn show(self, line: &mut Line<'_>) {
        let mut funcs = (0..).map(FuncIdx).zip(&line.code.funcs);
        match funcs.find(|(_, e)| e.pc.index() == self.index()) {
            Some((func, _)) => line.part(func),
            None => line.part(format_args!("@{}", self.index())),
        }
    }
}

/// How many registers a frame has is in the listing once, at its top.
impl Show for Room {
    fn show(self, _: &mut Line<'_>) {}
}

impl Show for Value {
    fn show(self, line: &mut Line<'_>) {
        line.part(Literal(self));
    }
}

impl Show for Float {
    fn show(self, line: &mut Line<'_>) {
        line.part(Literal(self.value()));
    }
}

impl Show for SmallInt {
    fn show(self, line: &mut Line<'_>) {
        line.part(self.get());
    }
}

impl Show for f64 {
    fn show(self, line: &mut Line<'_>) {
        line.part(format_args!("{self:?}"));
    }
}

impl Show for Ctor {
    fn show(self, line: &mut Line<'_>) {
        match line.code.ctors.name(self) {
            Some(v) => line.part(format_args!("{}.{}", v.type_id.0, v.variant_idx)),
            None => line.part(format_args!("?.{}", self.variant())),
        }
    }
}

/// A variant of whatever type the value has, `.1` with no fields and `.1(..)`
/// with some.
impl Show for Tag {
    fn show(self, line: &mut Line<'_>) {
        match self.parts() {
            (variant, true) => line.part(format_args!(".{variant}(..)")),
            (variant, false) => line.part(format_args!(".{variant}")),
        }
    }
}

impl Show for TextId {
    fn show(self, line: &mut Line<'_>) {
        line.part(text(line.code, self));
    }
}

impl Show for BinaryId {
    fn show(self, line: &mut Line<'_>) {
        let len = line.code.binaries.get(self.0 as usize).map(|b| b.len);
        line.part(format_args!("{} bits", len.unwrap_or(0)));
    }
}

impl Show for WhyId {
    fn show(self, line: &mut Line<'_>) {
        match line.code.whys.get(self.0 as usize) {
            Some(Why::Broken(why)) => line.part(format_args!("{why:?}")),
            Some(Why::Missing(what)) => line.part(format_args!("{what:?}")),
            None => {}
        }
    }
}

impl Show for GlobalSlot {
    fn show(self, line: &mut Line<'_>) {
        line.part(format_args!("@g{}", self.0));
    }
}

impl Show for NumOp {
    fn show(self, line: &mut Line<'_>) {
        match self {
            NumOp::Arith(op) => line.part(format_args!("{op:?}")),
            NumOp::Order(op) => line.part(format_args!("{op:?}")),
        }
    }
}

impl Show for Last {
    fn show(self, _: &mut Line<'_>) {}
}

/// A constant is shown by what it is, beside this.
impl Show for ConstId {
    fn show(self, _: &mut Line<'_>) {}
}

impl Show for BlockId {
    fn show(self, line: &mut Line<'_>) {
        line.part(format_args!("block {}", self.0));
    }
}

impl Show for Newline {
    fn show(self, line: &mut Line<'_>) {
        if self.0 {
            line.part("newline");
        }
    }
}

/// The fields whose `Display` is what the listing shows.
macro_rules! displayed {
    ($($ty:ty),*) => {$(
        impl Show for $ty {
            fn show(self, line: &mut Line<'_>) {
                line.part(self);
            }
        }
    )*};
}

displayed!(u16, u32, i64, FuncIdx);

/// The fields whose `Debug` is.
macro_rules! debugged {
    ($($ty:ty),*) => {$(
        impl Show for $ty {
            fn show(self, line: &mut Line<'_>) {
                line.part(format_args!("{self:?}"));
            }
        }
    )*};
}

debugged!(BitsOp, Intrinsic);
