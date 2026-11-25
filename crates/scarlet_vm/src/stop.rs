//! Why a run stops before its program ends. Each reason is a value, and its
//! words are only written when a stop is shown.

use std::fmt;

use scarlet_ir::core_ir::{FuncIdx, MAX_PARAMS, PrimOp, VariantRef};
use scarlet_ir::intrinsic::Intrinsic;

use crate::frame::MAX_VALUES;
use crate::heap::{AuditFailure, AuditFault, DeadCell, Fault};
use crate::value::View;

/// Why a run ended before the program did.
#[derive(Debug, PartialEq)]
pub enum Stop {
    /// The program reached something the VM does not run yet.
    NotBuiltYet(NotBuilt),
    /// The program's output was closed, like a pipe into `head` that has
    /// read enough. Nothing is wrong with the program, so it stops quietly.
    OutputClosed,
    /// The program's heap is at its limit, or the system has no more memory
    /// to give it, or one value is longer than a value can be. This is a
    /// limit of the machine, not a bug in the program.
    HeapFull,
    /// The program calls so deep that its stack is at its limit, or the system
    /// has no more memory to give it. Like `HeapFull`, this is a limit of the
    /// machine.
    StackFull,
    /// A run that was asked to check its heap could not get the memory to do
    /// it with. Like `HeapFull`, this is a limit of the machine. The
    /// program's own heap had room.
    NoRoomToAudit,
    /// The program has more instructions, or more of something they name,
    /// than the VM can number. Like `HeapFull`, this is a limit of the
    /// machine.
    ProgramTooLong,
    /// The program has more constructors with fields than a cell's header can
    /// number, among the types it uses. Like `HeapFull`, this is a limit of
    /// the machine.
    TooManyConstructors,
    /// The program broke a promise the compiler makes about every program,
    /// like a `match` having an arm for every value, or the VM found a
    /// reference count wrong. Only a bug in the compiler or the VM causes
    /// this, so the VM stops and says what broke instead of guessing.
    BadProgram(BadProgram),
}

impl Stop {
    pub(crate) fn missing(what: Missing) -> Stop {
        Stop::NotBuiltYet(what.into())
    }

    pub(crate) fn broken(why: Broken) -> Stop {
        Stop::BadProgram(BadProgram(why))
    }
}

impl From<DeadCell> for Stop {
    // No correct program takes this path. Saying so keeps the optimizer
    // from weighing each check for it as a likely way out of the loop.
    #[cold]
    #[inline(never)]
    fn from(_: DeadCell) -> Stop {
        Stop::broken(Broken::DeadCell)
    }
}

impl From<AuditFault> for Stop {
    fn from(fault: AuditFault) -> Stop {
        Stop::broken(Broken::Audit(fault))
    }
}

impl From<AuditFailure> for Stop {
    fn from(failure: AuditFailure) -> Stop {
        match failure {
            AuditFailure::Found(fault) => fault.into(),
            AuditFailure::NoRoom => Stop::NoRoomToAudit,
        }
    }
}

/// How a run stops when the heap cannot do what it was asked.
pub(crate) fn fault(fault: Fault) -> Stop {
    match fault {
        Fault::Full => Stop::HeapFull,
        Fault::DeadCell => Stop::broken(Broken::DeadCell),
    }
}

/// What the VM does not run yet. Its `Display` names it.
#[derive(Debug, Clone, PartialEq)]
pub struct NotBuilt(Missing);

impl From<Missing> for NotBuilt {
    fn from(what: Missing) -> NotBuilt {
        NotBuilt(what)
    }
}

/// Which promise a program broke. Its `Display` says what happened.
#[derive(Debug, Clone, PartialEq)]
pub struct BadProgram(Broken);

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Missing {
    Intrinsic(Intrinsic),
    Arity {
        op: PrimOp,
        want: Arity,
    },
    PrintArity(Intrinsic),
    TooManyFields,
    /// A function past one of the VM's limits.
    TooMany {
        func: String,
        of: Limit,
    },
    NoConst(usize),
    NoFunc(FuncIdx),
    Join(View),
    Branch(View),
    BigRangeEnd,
    IntOperation(View),
    Printing,
}

/// What a function may only have so much of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Limit {
    /// It holds more than [`MAX_VALUES`] values at once.
    Values,
    /// It has more than [`MAX_PARAMS`] parameters.
    Parameters,
    /// It makes a call with more than [`MAX_PARAMS`] arguments.
    Arguments,
}

impl fmt::Display for Limit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Limit::Values => write!(f, "holds more than {MAX_VALUES} values at once"),
            Limit::Parameters => write!(f, "has more than {MAX_PARAMS} parameters"),
            Limit::Arguments => write!(f, "makes a call with more than {MAX_PARAMS} arguments"),
        }
    }
}

/// How many arguments an operation takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Arity {
    One,
    Two,
    Three,
    AtLeastOne,
}

impl Arity {
    pub(crate) fn fits(self, n: usize) -> bool {
        match self {
            Arity::One => n == 1,
            Arity::Two => n == 2,
            Arity::Three => n == 3,
            Arity::AtLeastOne => n >= 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Broken {
    /// The program has more functions than a 32-bit index can name.
    TooManyFunctions,
    NoArm,
    /// A jump to a join that nothing around it declares.
    UndeclaredJoin,
    /// A jump with other than an argument for each of its join's parameters.
    JumpArity {
        got: usize,
        want: usize,
    },
    /// A function reads a value that nothing on some path to the read wrote.
    ReadBeforeWritten,
    /// A function jumps to an instruction before the jump.
    JumpsBack,
    /// A function goes on past its last instruction, or jumps there.
    RunsOff,
    /// An instruction names a register, a label or a function that was given
    /// no place.
    NoPlace,
    /// A value that has no type, or whose type is not one of the program's.
    NoType,
    /// A frame the stack does not have, like the one a wrong link returns to.
    NoWindow,
    NegativeCapture,
    ToplevelCapture,
    ToplevelSelf,
    ToplevelCallsSelf,
    RanPastEnd {
        func: String,
    },
    NoCapture {
        func: String,
        index: u32,
    },
    /// A function that reads captures is called with no closure to have them.
    CalledByName {
        func: String,
    },
    NoField {
        index: u16,
    },
    /// A value is taken apart as a constructor it is not.
    OpenedOther(View),
    /// A value is updated as a constructor it is not.
    UpdatedOther(View),
    /// The program names a constructor it does not list.
    NoCtor(VariantRef),
    /// The program updates a constructor at no field, at one that it does not
    /// list, or at fields out of order.
    UpdateFields(VariantRef),
    /// The program builds or matches a constructor with another number of
    /// fields than it lists, or the VM builds one so.
    CtorFields {
        ctor: VariantRef,
        got: usize,
        want: usize,
    },
    NoArrayElement {
        index: u16,
    },
    NotOnNonBool(View),
    DropCount(View),
    StringLength(View),
    NotUtf8,
    NotAFloat {
        text: String,
    },
    ReadPastEnd {
        at: u64,
        len: u64,
        bits: u64,
    },
    CodePointTooBig,
    NotBinary(View),
    NotString(View),
    NotRadix(View),
    NoIoTypes,
    NoJsonTypes,
    NotADoc(View),
    NotJson(View),
    NoHttpTypes,
    NotHeaders(View),
    NoAddressTypes,
    NoNetTypes,
    NoTlsTypes,
    NotAHandle(View),
    NotASocketAddress(View),
    NotAMap(View),
    NotAnArray(View),
    RangeEnd(View),
    Position(View),
    NotAFunction(View),
    CallArity {
        func: String,
        got: usize,
        want: usize,
    },
    FloatOperation(View),
    BinaryCell,
    /// A constructor's cell that is none of the program's constructors.
    CtorCell,
    /// A closure's cell that names no function.
    ClosureCell,
    TreePiece,
    RangeCell,
    /// A reference was taken to, given up on, or asked whether it was the
    /// only one to a cell that had already been freed. Some count went down
    /// once too often.
    DeadCell,
    /// A run that was asked to check each value found a cell where the value's
    /// type, or the instruction that wrote it, says there is no such cell.
    UnexpectedCell(View),
    /// A run that was asked to check its heap found something wrong with it.
    Audit(AuditFault),
    /// A run that was asked to check its heap ended, gave up everything it
    /// held, and this many cells were still not freed. Their counts are
    /// right, so they hold each other.
    CellsLeft {
        cells: usize,
    },
}

impl fmt::Display for NotBuilt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Missing::Intrinsic(i) => write!(f, "the built-in {i:?}"),
            Missing::Arity { op, want } => write!(f, "{op:?} with {want}"),
            Missing::PrintArity(i) => write!(f, "{i:?} with {}", Arity::One),
            Missing::TooManyFields => f.write_str("a constructor with more than 65535 fields"),
            Missing::NoConst(i) => write!(f, "constant c{i}, which the program does not have"),
            Missing::TooMany { func, of } => write!(f, "{func}, which {of},"),
            Missing::NoFunc(func) => write!(f, "a call to {func}, which the program does not have"),
            Missing::Join(v) => write!(f, "joining {v:?}"),
            Missing::Branch(v) => write!(f, "a branch on {v:?}"),
            Missing::BigRangeEnd => f.write_str("a range with an end past 64 bits"),
            Missing::IntOperation(v) => write!(f, "an Int operation on {v:?}"),
            Missing::Printing => f.write_str("printing this value"),
        }
    }
}

impl fmt::Display for Arity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Arity::One => "other than one argument",
            Arity::Two => "other than two arguments",
            Arity::Three => "other than three arguments",
            Arity::AtLeastOne => "no arguments",
        })
    }
}

impl fmt::Display for BadProgram {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Broken::TooManyFunctions => {
                f.write_str("the program has more functions than an index can name")
            }
            Broken::NoArm => f.write_str("a match found no arm for its value"),
            Broken::UndeclaredJoin => f.write_str("a jump to a join that nothing declares"),
            Broken::JumpArity { got, want } => {
                write!(f, "a jump with {got} arguments, not {want}")
            }
            Broken::ReadBeforeWritten => f.write_str("a value read before anything wrote it"),
            Broken::JumpsBack => f.write_str("a jump to an instruction before it"),
            Broken::RunsOff => f.write_str("a function that goes on past its last instruction"),
            Broken::NoPlace => f.write_str("an instruction that names something with no place"),
            Broken::NoType => {
                f.write_str("a value with no type, or with one that the program does not have")
            }
            Broken::NoWindow => f.write_str("a frame that the stack does not have"),
            Broken::NegativeCapture => f.write_str("a capture with a negative index"),
            Broken::ToplevelCapture => f.write_str("a toplevel reads a capture"),
            Broken::ToplevelSelf => f.write_str("a toplevel reads itself as a closure"),
            Broken::ToplevelCallsSelf => f.write_str("a toplevel calls itself"),
            Broken::RanPastEnd { func } => write!(f, "{func} ran past its last instruction"),
            Broken::NoCapture { func, index } => write!(
                f,
                "{func} read capture {index}, which the closure it runs as does not have"
            ),
            Broken::CalledByName { func } => {
                write!(f, "{func}, which reads captures, is called with no closure")
            }
            Broken::NoField { index } => {
                write!(f, "field {index} read from a value with no such field")
            }
            Broken::OpenedOther(v) => {
                write!(f, "{v:?} taken apart as a constructor it is not")
            }
            Broken::UpdatedOther(v) => {
                write!(f, "{v:?} updated as a constructor it is not")
            }
            Broken::UpdateFields(v) => write!(
                f,
                "{} updated at fields that it does not have, or that are not in order",
                name(*v)
            ),
            Broken::NoCtor(v) => write!(f, "{}, which the program does not list", name(*v)),
            Broken::CtorFields { ctor, got, want } => {
                write!(f, "{} with {got} fields, not {want}", name(*ctor))
            }
            Broken::NoArrayElement { index } => {
                write!(f, "element {index} read from an array with no such element")
            }
            Broken::NotOnNonBool(v) => write!(f, "`!` on {v:?}, not a Bool"),
            Broken::DropCount(v) => write!(f, "an array's first {v:?} elements dropped"),
            Broken::StringLength(v) => write!(f, "`string.length` of {v:?}"),
            Broken::NotUtf8 => f.write_str("a string that is not UTF-8"),
            Broken::NotAFloat { text } => write!(f, "`{text}` is not a float"),
            Broken::ReadPastEnd { at, len, bits } => {
                write!(
                    f,
                    "a pattern read bits {at}..{at}+{len} of a binary {bits} long"
                )
            }
            Broken::CodePointTooBig => f.write_str("a code point past a small Int"),
            Broken::NotBinary(v) => write!(f, "a binary operation on {v:?}"),
            Broken::NotString(v) => write!(f, "a string operation on {v:?}"),
            Broken::NotRadix(v) => write!(f, "{v:?} as a `scarlet/binary.Radix`"),
            Broken::NoIoTypes => {
                f.write_str("a file built-in, in a program with no `scarlet/io.IoError`")
            }
            Broken::NoJsonTypes => {
                f.write_str("a JSON built-in, in a program with no `scarlet/json` types")
            }
            Broken::NotADoc(v) => write!(f, "a JSON read of {v:?}, which is not a `Doc`"),
            Broken::NotJson(v) => write!(f, "{v:?} as a `Json` to encode"),
            Broken::NoHttpTypes => {
                f.write_str("an HTTP built-in, in a program with no `scarlet/http/h1` types")
            }
            Broken::NotHeaders(v) => write!(f, "{v:?} as `Headers`"),
            Broken::NoAddressTypes => {
                f.write_str("a network built-in, in a program with no `scarlet/net/address` types")
            }
            Broken::NoNetTypes => {
                f.write_str("a network built-in, in a program with no `scarlet/net/socket` types")
            }
            Broken::NoTlsTypes => {
                f.write_str("a TLS built-in, in a program with no `scarlet/net/tls` types")
            }
            Broken::NotAHandle(v) => write!(f, "{v:?} as a socket or a listener"),
            Broken::NotASocketAddress(v) => write!(f, "{v:?} as a `SocketAddress`"),
            Broken::NotAMap(v) => write!(f, "a map operation on {v:?}, which is not a map"),
            Broken::NotAnArray(v) => {
                write!(f, "an array operation on {v:?}, which is not an array")
            }
            Broken::RangeEnd(v) => write!(f, "a range whose end is {v:?}, not an Int"),
            Broken::Position(v) => write!(f, "an array position that is {v:?}, not an Int"),
            Broken::NotAFunction(v) => write!(f, "a call to {v:?}, which is not a function"),
            Broken::CallArity { func, got, want } => {
                write!(f, "{func} called with {got} arguments, not {want}")
            }
            Broken::FloatOperation(v) => write!(f, "a Float operation on {v:?}"),
            Broken::BinaryCell => f.write_str("a binary cell that is not a binary"),
            Broken::CtorCell => f.write_str("a constructor cell of no constructor"),
            Broken::ClosureCell => f.write_str("a closure cell with no function"),
            Broken::TreePiece => {
                f.write_str("a piece of an array's or a map's tree held as a value")
            }
            Broken::RangeCell => f.write_str("a range cell that is not a range"),
            Broken::DeadCell => f.write_str("a reference to a cell that was already freed"),
            Broken::UnexpectedCell(v) => write!(
                f,
                "{v:?} where the value's type, or the instruction that wrote it, says there is no such cell"
            ),
            Broken::Audit(fault) => write!(f, "{fault}"),
            Broken::CellsLeft { cells } => write!(
                f,
                "{cells} cells were still held after the run gave up everything it had"
            ),
        }
    }
}

/// A constructor by its number, for a program that may have no name for it.
fn name(v: VariantRef) -> String {
    format!("constructor {}.{}", v.type_id.0, v.variant_idx)
}

#[cfg(test)]
mod tests;
