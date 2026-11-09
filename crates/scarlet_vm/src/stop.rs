//! Why a run stops before its program does, as values. The words are written
//! only when a stop is shown.

use std::fmt;

use scarlet_ir::core_ir::{FuncIdx, PrimOp};
use scarlet_ir::intrinsic::Intrinsic;

use crate::value::View;

/// Why a run ended before the program did.
#[derive(Debug, PartialEq)]
pub enum Stop {
    /// The program reached something the VM does not run yet.
    NotBuiltYet(NotBuilt),
    /// The program's output was closed, like a pipe into `head` that has
    /// read enough. Nothing is wrong with the program, so it stops quietly.
    OutputClosed,
    /// The program's heap grew past what the VM can address. This is a limit
    /// of the machine, like running out of memory, not a bug in the program.
    HeapFull,
    /// The program broke a promise the compiler makes about every program,
    /// like a `match` having an arm for every value. Only a compiler bug
    /// causes this, so the VM stops and says what broke instead of guessing.
    BadProgram(BadProgram),
}

impl Stop {
    pub(crate) fn missing(what: Missing) -> Stop {
        Stop::NotBuiltYet(NotBuilt(what))
    }

    pub(crate) fn broken(why: Broken) -> Stop {
        Stop::BadProgram(BadProgram(why))
    }
}

/// What the VM does not run yet. Its `Display` names it.
#[derive(Debug, Clone, PartialEq)]
pub struct NotBuilt(Missing);

/// Which promise a program broke. Its `Display` says what happened.
#[derive(Debug, Clone, PartialEq)]
pub struct BadProgram(Broken);

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Missing {
    Intrinsic(Intrinsic),
    Arity { op: PrimOp, want: Arity },
    PrintlnArity,
    TooManyFields,
    NoConst(usize),
    BigConst(i64),
    StringConst,
    BinaryConst,
    NoFunc(FuncIdx),
    Join(View),
    Branch(View),
    BigRangeEnd,
    IntOperation(View),
    Printing,
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
    NoArm,
    UndeclaredCase,
    NegativeCapture,
    ToplevelSelf,
    ToplevelCallsSelf,
    RanPastEnd {
        func: String,
    },
    NoCapture {
        func: String,
        index: u32,
    },
    NoField {
        index: u16,
    },
    NoElement {
        index: u16,
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
    TreePiece,
    RangeCell,
}

impl fmt::Display for NotBuilt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Missing::Intrinsic(i) => write!(f, "the built-in {i:?}"),
            Missing::Arity { op, want } => write!(f, "{op:?} with {want}"),
            Missing::PrintlnArity => write!(f, "println with {}", Arity::One),
            Missing::TooManyFields => f.write_str("a constructor with more than 65535 fields"),
            Missing::NoConst(i) => write!(f, "constant c{i}, which the program does not have"),
            Missing::BigConst(n) => write!(f, "the constant {n} as a small Int"),
            Missing::StringConst => f.write_str("String"),
            Missing::BinaryConst => f.write_str("a Binary as a plain value"),
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
            Broken::NoArm => f.write_str("a match found no arm for its value"),
            Broken::UndeclaredCase => f.write_str("a jump to a case that no match declares"),
            Broken::NegativeCapture => f.write_str("a capture with a negative index"),
            Broken::ToplevelSelf => f.write_str("a toplevel reads itself as a closure"),
            Broken::ToplevelCallsSelf => f.write_str("a toplevel calls itself"),
            Broken::RanPastEnd { func } => write!(f, "{func} ran past its last instruction"),
            Broken::NoCapture { func, index } => write!(
                f,
                "{func} read capture {index}, which the closure it runs as does not have"
            ),
            Broken::NoField { index } => {
                write!(f, "field {index} read from a value with no such field")
            }
            Broken::NoElement { index } => {
                write!(f, "element {index} read from a value with no such element")
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
            Broken::TreePiece => {
                f.write_str("a piece of an array's or a map's tree held as a value")
            }
            Broken::RangeCell => f.write_str("a range cell that is not a range"),
        }
    }
}
