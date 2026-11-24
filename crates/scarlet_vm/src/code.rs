//! The loaded program, and the loader that makes it from Core IR.
//!
//! When the program loads, each function's Core IR becomes register
//! instructions, once. Each construct maps to one instruction or a few, so
//! the listing reads like the Core IR it came from. Where the loader knows
//! something about an operand it picks an instruction for that: one that
//! holds a constant, or one that compares and jumps.
//!
//! A function goes through four passes, and each hands the next a value:
//!
//! 1. [`translate`] makes its instructions, over values that have no register
//!    yet ([`Draft`]).
//! 2. [`live`] goes over them once from the end. It finds what each
//!    instruction may take from its register, and which values are ever held
//!    at once.
//! 3. [`alloc`] gives each value a register, and leaves out what that has made
//!    do nothing ([`Fitted`]).
//! 4. [`emit`] puts every function's instructions in one array, where a jump
//!    and a call name a place in it.
//!
//! A function's registers are numbered in a byte, and one that needs more than
//! there are is taken through again with its long literals made a piece at a
//! time ([`literals`]).
//!
//! The VM is being built one feature at a time. A function that uses
//! something not built yet still loads, as one instruction that says what it
//! needs, and only a call to it stops the run. So a program runs as far as
//! the VM has come.

use scarlet_ir::core_ir::{Abi, CoreFn, FuncIdx, LoweredFn, MAX_PARAMS, Program};
use scarlet_ir::intrinsic::Intrinsic;
use scarlet_ir::tivec::TiVec;

use crate::Audit;
use crate::show::Types;
use crate::stop::{Broken, Limit, Missing, Stop};

mod alloc;
mod emit;
#[macro_use]
mod ins;
mod literals;
mod live;
mod translate;

pub(crate) use ins::{
    Arg, ArgList, Args, BinaryId, BitsOp, BlockId, Dead, Dst, EntryPc, Hit, Ins, KeepList, Kept,
    Lists, LookList, Looks, Newline, Op, Over, Pc, Reg, Room, ShareList, Shared, Shares, Src,
    Taken, TextId, Variant, WhyId,
};

use alloc::Fitted;

/// The most registers a frame has. A register is named by a byte, and two of
/// its 256 numbers are kept back for a frame's link to its caller and the
/// closure it runs as, which are not in registers yet.
pub(crate) const MAX_REGS: usize = 254;

/// A whole loaded program.
pub(crate) struct Code {
    /// Every function's instructions, one after another. The first is
    /// [`Ins::Halt`] and belongs to none.
    pub(crate) ins: Vec<Ins<Reg>>,
    pub(crate) lists: Lists<Reg>,
    pub(crate) texts: Vec<Box<[u8]>>,
    pub(crate) binaries: Vec<BinaryText>,
    pub(crate) whys: Vec<Why>,
    pub(crate) funcs: TiVec<FuncIdx, Entry>,
    /// Each module's toplevel in the order they run, with the entry file's
    /// last.
    pub(crate) toplevels: Vec<Entry>,
    pub(crate) main: Option<FuncIdx>,
    pub(crate) globals: u32,
    pub(crate) types: Types,
    /// Each function's source name, for showing a function value.
    pub(crate) names: TiVec<FuncIdx, String>,
    /// The stdlib constructors the VM builds on its own.
    pub(crate) abi: Abi,
}

impl Code {
    /// The function instruction `pc` is one of.
    pub(crate) fn function_at(&self, pc: usize) -> Option<&Entry> {
        let mut all = (&self.funcs).into_iter().chain(&self.toplevels);
        all.find(|e| (e.pc.index()..e.end.index()).contains(&pc))
    }

    /// The string `id` names.
    #[inline(always)]
    pub(crate) fn text(&self, id: TextId) -> Result<&[u8], Stop> {
        match self.texts.get(id.0 as usize) {
            Some(text) => Ok(text),
            None => Err(no_place()),
        }
    }

    /// The binary `id` names.
    #[inline(always)]
    pub(crate) fn binary(&self, id: BinaryId) -> Result<&BinaryText, Stop> {
        self.binaries.get(id.0 as usize).ok_or_else(no_place)
    }

    #[cold]
    #[inline(never)]
    pub(crate) fn why(&self, why: WhyId) -> Stop {
        match self.whys.get(why.0 as usize) {
            Some(Why::Broken(why)) => Stop::broken(why.clone()),
            Some(Why::Missing(what)) => Stop::missing(what.clone()),
            None => no_place(),
        }
    }
}

/// The stop for an instruction that names something the program does not have.
#[cold]
#[inline(never)]
fn no_place() -> Stop {
    Stop::broken(Broken::NoPlace)
}

/// A binary the program spells: the first `len` bits of `bytes`.
#[derive(Debug)]
pub(crate) struct BinaryText {
    pub(crate) bytes: Box<[u8]>,
    pub(crate) len: u64,
}

/// Why an [`Ins::Bad`] stops the run.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Why {
    Broken(Broken),
    Missing(Missing),
}

/// Where a loaded function starts, and what a call to it has to know.
#[derive(Debug)]
pub(crate) struct Entry {
    pub(crate) name: String,
    pub(crate) pc: EntryPc,
    /// One past its last instruction.
    pub(crate) end: Pc,
    /// How many arguments it takes. They arrive in its first registers.
    pub(crate) params: usize,
    /// Registers a frame for this function needs.
    pub(crate) needs: Room,
}

/// A program loaded so that a run of it can say how often each instruction
/// ran ([`load_counted`]).
pub(crate) struct Counted {
    /// The program, with an [`Ins::Count`] at the head of every basic block.
    pub(crate) code: Code,
    /// The kinds of the instructions in each block, by the number the
    /// block's `Count` holds.
    pub(crate) blocks: Vec<Box<[Op]>>,
}

/// Load `program` for a run that checks its heap when `audit` says. One that
/// is to check before every instruction gets an [`Ins::Audit`] in front of
/// each, and any other gets none.
pub(crate) fn load(program: &Program, audit: Audit) -> Result<Code, Stop> {
    load_as(program, emit::Extra { count: None, audit })
}

/// [`load`], with an [`Ins::Count`] at the head of every basic block. The
/// counts leave the audits out.
pub(crate) fn load_counted(program: &Program, audit: Audit) -> Result<Counted, Stop> {
    let mut blocks = Vec::new();
    let extra = emit::Extra {
        count: Some(&mut blocks),
        audit,
    };
    let code = load_as(program, extra)?;
    Ok(Counted { code, blocks })
}

fn load_as(program: &Program, extra: emit::Extra<'_>) -> Result<Code, Stop> {
    let full = |_| Stop::broken(Broken::TooManyFunctions);
    let mut tables = Tables::default();
    let mut funcs = TiVec::new();
    let mut names = TiVec::new();
    for f in &program.fns {
        let this = funcs.next_idx().map_err(full)?;
        let fitted = fit(program, f, Some(this), &mut tables)?;
        funcs.push(fitted).map_err(full)?;
        names.push(f.name.clone()).map_err(full)?;
    }
    let mut toplevels = Vec::new();
    for f in program.inits.iter().chain([&program.toplevel]) {
        toplevels.push(fit(program, f, None, &mut tables)?);
    }
    let placed = emit::emit(&funcs, &toplevels, extra)?;
    Ok(Code {
        ins: placed.ins,
        lists: placed.lists,
        texts: tables.texts,
        binaries: tables.binaries,
        whys: tables.whys,
        funcs: placed.funcs,
        toplevels: placed.toplevels,
        main: program.main,
        globals: program.globals,
        types: program.types.clone(),
        names,
        abi: program.abi,
    })
}

/// What the program's instructions name by a number, because an instruction
/// has no room for it.
#[derive(Default)]
struct Tables {
    texts: Vec<Box<[u8]>>,
    binaries: Vec<BinaryText>,
    whys: Vec<Why>,
}

impl Tables {
    fn text(&mut self, text: &[u8]) -> Result<TextId, Unfit> {
        add(&mut self.texts, text.into()).map(TextId)
    }

    fn binary(&mut self, bytes: &[u8], len: u64) -> Result<BinaryId, Unfit> {
        let bytes = bytes.into();
        add(&mut self.binaries, BinaryText { bytes, len }).map(BinaryId)
    }

    fn why(&mut self, why: Why) -> Result<WhyId, Unfit> {
        add(&mut self.whys, why).map(WhyId)
    }

    /// How much is in each table.
    fn lens(&self) -> [usize; 3] {
        [self.texts.len(), self.binaries.len(), self.whys.len()]
    }

    /// Take out everything added since [`Self::lens`] gave `lens`.
    fn cut(&mut self, [texts, binaries, whys]: [usize; 3]) {
        self.texts.truncate(texts);
        self.binaries.truncate(binaries);
        self.whys.truncate(whys);
    }
}

/// Put `item` at the end of `list`, and give its number there.
fn add<T>(list: &mut Vec<T>, item: T) -> Result<u32, Unfit> {
    let number = u32::try_from(list.len()).map_err(|_| Unfit::TooLong)?;
    list.push(item);
    Ok(number)
}

/// Why a function did not load.
#[derive(Debug, PartialEq)]
enum Unfit {
    /// It needs something the VM does not run yet. The function still loads,
    /// and stops with this if it is ever called.
    Missing(Missing),
    /// It is not a function the compiler should have made. It stops with this
    /// if it is ever called.
    Broken(Broken),
    /// It is past a limit of the VM's. It stops with this if it is ever called.
    TooMany(Limit),
    /// The program has more of something than an instruction can name.
    TooLong,
}

impl From<Missing> for Unfit {
    fn from(m: Missing) -> Self {
        Unfit::Missing(m)
    }
}

/// Take `f` as far as registers. `this` is the function's own index, which a
/// call to `self` means. A toplevel has none, and cannot call itself.
fn fit(
    program: &Program,
    f: &LoweredFn,
    this: Option<FuncIdx>,
    tables: &mut Tables,
) -> Result<Fitted, Stop> {
    let name = format!("{}.{}", f.module, f.name);
    let before = tables.lens();
    let mut fitted = fit_core(program, &f.core, this, tables);
    // A long literal is built a piece at a time only here, so that a function
    // with room for its literals makes the cells it always did.
    if fitted.as_ref().err() == Some(&Unfit::TooMany(Limit::Values))
        && let Some(core) = literals::in_pieces(&f.core)
    {
        // Nothing names what the first try put in the tables.
        tables.cut(before);
        fitted = fit_core(program, &core, this, tables);
    }
    let why = match fitted {
        Ok(body) => return Ok(Fitted::new(name, f.core.params.len(), body)),
        Err(Unfit::Missing(what)) => Why::Missing(what),
        Err(Unfit::Broken(why)) => Why::Broken(why),
        Err(Unfit::TooMany(of)) => Why::Missing(Missing::TooMany {
            func: name.clone(),
            of,
        }),
        Err(Unfit::TooLong) => return Err(Stop::ProgramTooLong),
    };
    let why = tables.why(why).map_err(|_| Stop::ProgramTooLong)?;
    Ok(Fitted::bad(name, f.core.params.len(), why))
}

fn fit_core(
    program: &Program,
    core: &CoreFn,
    this: Option<FuncIdx>,
    tables: &mut Tables,
) -> Result<alloc::Body, Unfit> {
    if core.params.len() > MAX_PARAMS {
        return Err(Unfit::TooMany(Limit::Parameters));
    }
    let mut draft = translate::translate(program, core, this, tables)?;
    let held = live::settle(&mut draft)?;
    alloc::allocate(draft, &held)
}

/// Whether the VM runs built-in `i` when called with `argc` arguments. A
/// function calling any other still loads, and stops only when it runs.
fn built(i: Intrinsic, argc: usize) -> bool {
    let arity = match i {
        Intrinsic::MapNew
        | Intrinsic::InternalStackDepth
        | Intrinsic::InternalCellsMade
        | Intrinsic::InternalCellsReused
        | Intrinsic::TimeMonotonic
        | Intrinsic::TimeEpochMs
        | Intrinsic::OsArgv
        | Intrinsic::OsEnv
        | Intrinsic::OsTerminalSize => 0,
        Intrinsic::CryptoRandomBytes
        | Intrinsic::ProcessSleep
        | Intrinsic::IoReadFile
        | Intrinsic::CryptoSha1
        | Intrinsic::CryptoSha256
        | Intrinsic::CryptoSha512 => 1,
        Intrinsic::JsonParseBinary
        | Intrinsic::JsonKind
        | Intrinsic::JsonLen
        | Intrinsic::JsonEntries
        | Intrinsic::JsonElements
        | Intrinsic::JsonString
        | Intrinsic::JsonInt
        | Intrinsic::JsonIntText
        | Intrinsic::JsonFloat
        | Intrinsic::JsonBool
        | Intrinsic::JsonEncode => 1,
        Intrinsic::JsonField | Intrinsic::JsonIndex => 2,
        Intrinsic::HttpFraming | Intrinsic::HttpHeadersValid => 1,
        Intrinsic::HttpParseHead
        | Intrinsic::HttpParseResponseHead
        | Intrinsic::HttpHeaderGet
        | Intrinsic::HttpHeaderHas => 2,
        Intrinsic::HttpChunkDecode | Intrinsic::HttpSerializeHead => 3,
        Intrinsic::IoWriteFile | Intrinsic::CryptoHmacSha256 | Intrinsic::CryptoConstEq => 2,
        Intrinsic::CryptoP256Verify | Intrinsic::CryptoEd25519Verify => 3,
        Intrinsic::MapKeys | Intrinsic::MapValues | Intrinsic::MapSize | Intrinsic::MapToList => 1,
        Intrinsic::MapGet | Intrinsic::MapHas | Intrinsic::MapDelete => 2,
        Intrinsic::MapSet => 3,
        Intrinsic::StringInspect
        | Intrinsic::StringLength
        | Intrinsic::ArrayLength
        | Intrinsic::IntToString
        | Intrinsic::FloatFloor
        | Intrinsic::FloatCeil
        | Intrinsic::FloatRound
        | Intrinsic::FloatTruncate
        | Intrinsic::FloatToString
        | Intrinsic::BinaryFromString
        | Intrinsic::BinaryToString
        | Intrinsic::BinaryBitSize
        | Intrinsic::BinaryByteSize => 1,
        Intrinsic::BinaryToAsciiLower
        | Intrinsic::StringTrim
        | Intrinsic::StringToGraphemes
        | Intrinsic::IntFromString
        | Intrinsic::IntBitwiseNot => 1,
        Intrinsic::StringSplit
        | Intrinsic::FloatFromDecimal
        | Intrinsic::StringContains
        | Intrinsic::IntBitwiseAnd
        | Intrinsic::IntBitwiseOr
        | Intrinsic::IntBitwiseXor
        | Intrinsic::IntBitwiseShiftLeft
        | Intrinsic::IntBitwiseShiftRight => 2,
        Intrinsic::BinaryAppend
        | Intrinsic::BinaryByteAt
        | Intrinsic::BinaryParseInt
        | Intrinsic::BinaryEqIgnoreAsciiCase
        | Intrinsic::BinaryFromIntAscii => 2,
        Intrinsic::BinarySliceBits | Intrinsic::BinaryIndexOf => 3,
        Intrinsic::AddressParse
        | Intrinsic::NetResolve
        | Intrinsic::NetListen
        | Intrinsic::NetAccept
        | Intrinsic::NetConnect
        | Intrinsic::NetLocalAddr
        | Intrinsic::NetClose
        | Intrinsic::SocketClose
        | Intrinsic::TlsClose => 1,
        Intrinsic::NetResolveUntil
        | Intrinsic::NetConnectUntil
        | Intrinsic::SocketRead
        | Intrinsic::SocketWrite
        | Intrinsic::SocketWriteParts
        | Intrinsic::TlsHandshake
        | Intrinsic::TlsRead
        | Intrinsic::TlsWrite => 2,
        Intrinsic::SocketReadUntil | Intrinsic::TlsHandshakeUntil | Intrinsic::TlsReadUntil => 3,
        _ => return false,
    };
    argc == arity
}

#[cfg(test)]
mod tests;
