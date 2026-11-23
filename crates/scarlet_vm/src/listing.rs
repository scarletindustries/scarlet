//! The loaded instructions as text, for `scarlet dis --instrs`.
//!
//! One line per instruction: its number, which a jump names, then the
//! register it writes, its kind, and its operands. A register is `r3`. An
//! operand written `move r3` gives up the register's reference instead of
//! adding one. `-> 7` is where the instruction may continue instead of at
//! the next one. Each kind is spelt as a counted run spells it
//! (`counts.rs`), so a count can be read against a listing.

use std::fmt::{self, Write as _};

use scarlet_ir::core_ir::{Program, VariantRef};

use crate::Stop;
use crate::code::{self, Arg, Body, Func, Instr, Reg};
use crate::stop::NotBuilt;
use crate::value::{Value, View};

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
    let code = code::load(program)?;
    Ok(Listing {
        fns: (&code.fns).into_iter().map(function).collect(),
        toplevel: code.toplevels.last().map(function).unwrap_or_default(),
    })
}

fn function(f: &Func) -> String {
    let mut out = String::new();
    // Writing to a `String` cannot fail.
    let _ = match f {
        Func::Ready(body) => write_body(&mut out, body),
        Func::NotBuiltYet(what) => {
            let what = NotBuilt::from(what.clone());
            writeln!(out, "; not loaded: the VM does not run {what} yet")
        }
    };
    out
}

fn write_body(out: &mut String, body: &Body) -> fmt::Result {
    let registers = if body.regs == 1 {
        "register"
    } else {
        "registers"
    };
    write!(out, "; loaded: {} {registers}", body.regs)?;
    if !body.params.is_empty() {
        write!(out, ", parameters in {}", List(&body.params))?;
    }
    if !body.carried.is_empty() {
        write!(out, ", cells kept for reuse in {}", List(&body.carried))?;
    }
    out.push('\n');
    for (pc, instr) in body.instrs.iter().enumerate() {
        writeln!(out, "{pc:>5}  {instr}")?;
    }
    Ok(())
}

impl fmt::Display for Reg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "r{}", self.0)
    }
}

impl fmt::Display for Arg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.moved {
            f.write_str("move ")?;
        }
        self.reg.fmt(f)
    }
}

/// Operands, separated by commas.
struct List<'a, T>(&'a [T]);

impl<T: fmt::Display> fmt::Display for List<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, item) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            item.fmt(f)?;
        }
        Ok(())
    }
}

/// A register a `moved` flag says the instruction takes over.
fn taken(reg: Reg, moved: bool) -> Arg {
    Arg { reg, moved }
}

/// A constructor as a Core IR listing writes it: its type, then its variant.
struct Variant(VariantRef);

impl fmt::Display for Variant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.0.type_id.0, self.0.variant_idx)
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
            View::Nullary(v) => Variant(v).fmt(f),
            // The loader makes no constant that is a cell.
            View::Cell(_) => f.write_str("a cell"),
        }
    }
}

/// Bytes as the text they spell, quoted.
struct Text<'a>(&'a [u8]);

impl fmt::Display for Text<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", String::from_utf8_lossy(self.0))
    }
}

/// A call's callee and arguments: `fn#3(r1, move r2)`.
struct Called<'a, C>(C, &'a [Arg]);

impl<C: fmt::Display> fmt::Display for Called<'_, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}({})", self.0, List(self.1))
    }
}

impl fmt::Display for Instr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = self.op().name();
        match self {
            Instr::Count { block } => write!(f, "{name} block {block}"),
            Instr::Const { dst, value } => write!(f, "{dst} = {name} {}", Literal(*value)),
            Instr::Str { dst, text } => write!(f, "{dst} = {name} {}", Text(text)),
            Instr::BinaryConst { dst, len, .. } => write!(f, "{dst} = {name} of {len} bits"),
            Instr::Bits { dst, op, args } => write!(f, "{dst} = {name} {op:?}({})", List(args)),
            Instr::BigInt { dst, n } => write!(f, "{dst} = {name} {n}"),
            Instr::Move { dst, src, moved } => {
                write!(f, "{dst} = {name} {}", taken(*src, *moved))
            }
            Instr::GetGlobal { dst, slot } => write!(f, "{dst} = {name} @g{}", slot.0),
            Instr::SetGlobal { slot, src } => write!(f, "{name} @g{} = {src}", slot.0),
            Instr::Int { dst, op, a, b } => write!(f, "{dst} = {name} {op:?}({a}, {b})"),
            Instr::IntImm { dst, op, a, b } => write!(f, "{dst} = {name} {op:?}({a}, {b})"),
            Instr::Float { dst, op, a, b } | Instr::Num { dst, op, a, b } => {
                write!(f, "{dst} = {name} {op:?}({a}, {b})")
            }
            Instr::IntNeg { dst, a }
            | Instr::FloatNeg { dst, a }
            | Instr::Neg { dst, a }
            | Instr::ToString { dst, a }
            | Instr::Not { dst, a } => write!(f, "{dst} = {name} {a}"),
            Instr::Print { dst, arg, newline } => {
                let end = if *newline { " and a newline" } else { "" };
                write!(f, "{dst} = {name} {arg}{end}")
            }
            Instr::Concat { dst, parts } => write!(f, "{dst} = {name}({})", List(parts)),
            Instr::Ctor {
                dst,
                variant,
                fields,
                reuse,
            } => {
                write!(f, "{dst} = {name} {}", Called(Variant(*variant), fields))?;
                match reuse {
                    Some(cell) => write!(f, " over the cell in {cell}"),
                    None => Ok(()),
                }
            }
            Instr::Drop { reg } | Instr::DropReuse { reg } => write!(f, "{name} {reg}"),
            Instr::Call { dst, func, args } => write!(f, "{dst} = {name} {}", Called(func, args)),
            Instr::TailCall { func, args } => write!(f, "{name} {}", Called(func, args)),
            Instr::CallSelf { dst, args } => write!(f, "{dst} = {name}({})", List(args)),
            Instr::TailCallSelf { args } => write!(f, "{name}({})", List(args)),
            Instr::CallValue { dst, callee, args } => {
                write!(f, "{dst} = {name} {}", Called(callee, args))
            }
            Instr::TailCallValue { callee, args } => write!(f, "{name} {}", Called(callee, args)),
            Instr::Closure {
                dst,
                func,
                captures,
            } => write!(f, "{dst} = {name} {func}({})", List(captures)),
            Instr::GetCapture { dst, index } => write!(f, "{dst} = {name} {index}"),
            Instr::GetSelf { dst, this } => write!(f, "{dst} = {name} {this}"),
            Instr::Ret { src } => write!(f, "{name} {src}"),
            Instr::Jump { to } => write!(f, "{name} -> {to}"),
            Instr::JumpIfFalse { cond, to } => write!(f, "{name} {cond} -> {to}"),
            Instr::JumpUnlessCmp { op, a, b, to } => write!(f, "{name} {op:?}({a}, {b}) -> {to}"),
            Instr::JumpUnlessCmpImm { op, a, b, to } => {
                write!(f, "{name} {op:?}({a}, {b}) -> {to}")
            }
            Instr::MapSetOwned {
                dst,
                map,
                key,
                value,
            } => write!(f, "{dst} = {name}(move {map}, {key}, {value})"),
            Instr::MapFind { dst, map, key, to } => {
                write!(f, "{dst} = {name}({map}, {key}) -> {to}")
            }
            Instr::MapDeleteOwned { dst, map, key } => {
                write!(f, "{dst} = {name}(move {map}, {key})")
            }
            Instr::JumpUnlessVariant { src, variant, to } => {
                write!(f, "{name} {src} is {} -> {to}", Variant(*variant))
            }
            Instr::JumpUnlessInt { src, n, to } => write!(f, "{name} {src} is {n} -> {to}"),
            Instr::JumpUnlessFloat { src, x, to } => write!(f, "{name} {src} is {x:?} -> {to}"),
            Instr::JumpUnlessStr { src, text, to } => {
                write!(f, "{name} {src} is {} -> {to}", Text(text))
            }
            Instr::JumpUnlessBinary { src, len, to, .. } => {
                write!(f, "{name} {src} is a constant of {len} bits -> {to}")
            }
            Instr::Tuple { dst, elements } | Instr::Array { dst, elements } => {
                write!(f, "{dst} = {name}({})", List(elements))
            }
            Instr::Element { dst, src, index }
            | Instr::ArrayElem { dst, src, index }
            | Instr::Field { dst, src, index } => write!(f, "{dst} = {name} {index} of {src}"),
            Instr::Builtin {
                dst,
                intrinsic,
                args,
            } => write!(f, "{dst} = {name} {intrinsic:?}({})", List(args)),
            Instr::Equal { dst, a, b }
            | Instr::NotEqual { dst, a, b }
            | Instr::ArrayConcat { dst, a, b }
            | Instr::Range {
                dst,
                start: a,
                end: b,
            }
            | Instr::ArrayIndex {
                dst,
                src: a,
                index: b,
            } => write!(f, "{dst} = {name}({a}, {b})"),
            Instr::ArrayLen { dst, src } => write!(f, "{dst} = {name} {src}"),
            Instr::ArrayDrop { dst, src, n, moved } => {
                write!(f, "{dst} = {name}({}, {n})", taken(*src, *moved))
            }
            Instr::ArrayPrepend {
                dst,
                front,
                rest,
                moved,
            } => write!(
                f,
                "{dst} = {name}({}, ..{})",
                List(front),
                taken(*rest, *moved)
            ),
            Instr::ArrayAppend {
                dst,
                rest,
                back,
                moved,
            } => write!(
                f,
                "{dst} = {name}(..{}, {})",
                taken(*rest, *moved),
                List(back)
            ),
            Instr::ArraySlice {
                dst,
                src,
                start,
                end,
            } => write!(f, "{dst} = {name}({src}, {start}, {end})"),
            Instr::ArrayIndexOr {
                dst,
                src,
                index,
                default,
            } => write!(f, "{dst} = {name}({src}, {index}, {default})"),
            Instr::Bad { why } => write!(f, "{name} {why:?}"),
        }
    }
}
