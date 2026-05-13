use indexmap::IndexMap;

mod builtins;
pub mod compiler;
mod prelude;
pub use compiler::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    // Stack manipulation
    PushConst,
    PushLocal,
    StoreLocal,
    PushNone,
    PushTrue,
    PushFalse,
    Pop,
    Dup,
    Swap,

    // Arithmetic
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Neg,

    // Comparison
    Eq,
    Neq,
    Lt,
    Gt,
    Lte,
    Gte,

    // Logic
    Not,

    // Control flow
    Jump,
    JumpIfFalse,
    JumpIfTrue,
    Call,
    TailCall,
    Ret,

    // Data structures
    MakeArray,
    MakeTuple,
    TupleIndex,
    MakeRange,
    Index,
    ArrayLen,
    ArraySlice,
    ArrayConcat,
    MakeStruct,
    GetField,

    // Enums
    MakeEnum,
    MakeEnumPayload,
    MatchEnum,
    UnwrapEnum,

    // Closures
    MakeClosure,
    PushCapture,
    PushSelf,

    // Error handling
    MakeError,
    IsError,
    IsNone,
    UnwrapError,
    IsFailure,
    UnwrapFailure,

    // String operations
    ToString,
    StrConcat,
    StrSplit,

    // Misc
    Print,
    StackDepth,
    Halt,

    // I/O operations (experimental)
    FileRead,
    FileWrite,
    TcpListen,
    TcpAccept,
    TcpRead,
    TcpWrite,
    TcpClose,
}

#[derive(Debug, Clone, Copy)]
pub struct Instruction {
    pub op: Op,
    pub operand: i32,
}

#[derive(Debug, Clone)]
pub struct Function {
    pub name: String,
    pub arity: i32,
    pub locals: i32,
    pub capture_count: i32,
    pub code_start: i32,
    pub code_len: i32,
}

#[derive(Debug, Clone)]
pub enum Value {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
    None,
    Array(Vec<Value>),
    Tuple(TupleValue),
    Struct(StructValue),
    Closure(ClosureValue),
    Enum(EnumValue),
    Error(Box<ErrorValue>),
    Socket(SocketValue),
}

#[derive(Debug, Clone)]
pub struct TupleValue {
    pub elements: Vec<Value>,
}

#[derive(Debug, Clone)]
pub struct ErrorValue {
    pub payload: Value,
}

#[derive(Debug, Clone, Copy)]
pub struct SocketValue {
    pub id: i32,
    pub is_listener: bool,
}

#[derive(Debug, Clone)]
pub struct EnumValue {
    pub type_id: i32,
    pub enum_name: String,
    pub variant_name: String,
    pub payload: Vec<Value>,
    pub hash: u64,
}

#[derive(Debug, Clone)]
pub struct StructValue {
    pub type_id: i32,
    pub type_name: String,
    pub fields: IndexMap<String, Value>,
    pub hash: u64,
}

#[derive(Debug, Clone)]
pub struct ClosureValue {
    pub func_idx: i32,
    pub captures: Vec<Value>,
    pub name: String,
}

#[derive(Debug, Clone, Default)]
pub struct Program {
    pub constants: Vec<Value>,
    pub functions: Vec<Function>,
    pub code: Vec<Instruction>,
    pub entry: i32,
}

pub fn op(o: Op) -> Instruction {
    Instruction { op: o, operand: 0 }
}

pub fn op_arg(o: Op, operand: i32) -> Instruction {
    Instruction { op: o, operand }
}

const HASH_BASIS: u64 = 0xcbf29ce484222325;

#[inline]
fn fnv1a_combine(h: u64, val: u64) -> u64 {
    (h ^ val).wrapping_mul(0x100000001b3)
}

pub fn hash_value(v: &Value) -> u64 {
    let mut h = HASH_BASIS;
    match v {
        Value::Int(i) => {
            h = fnv1a_combine(h, *i as u64);
        }
        Value::Float(f) => {
            h = fnv1a_combine(h, f.to_bits());
        }
        Value::Bool(b) => {
            h = fnv1a_combine(h, if *b { 1 } else { 0 });
        }
        Value::Str(s) => {
            for c in s.bytes() {
                h = fnv1a_combine(h, c as u64);
            }
        }
        Value::None => {
            h = fnv1a_combine(h, 0);
        }
        Value::Enum(e) => {
            h = e.hash;
        }
        Value::Struct(s) => {
            h = s.hash;
        }
        Value::Array(a) => {
            for item in a {
                h = fnv1a_combine(h, hash_value(item));
            }
        }
        _ => {
            h = fnv1a_combine(h, 0);
        }
    }
    h
}

pub fn compute_struct_hash(type_name: &str, fields: &IndexMap<String, Value>) -> u64 {
    let mut h = HASH_BASIS;
    for c in type_name.bytes() {
        h = fnv1a_combine(h, c as u64);
    }
    let mut keys: Vec<&String> = fields.keys().collect();
    keys.sort();
    for key in keys {
        for c in key.bytes() {
            h = fnv1a_combine(h, c as u64);
        }
        h = fnv1a_combine(h, hash_value(&fields[key]));
    }
    h
}

pub fn compute_enum_hash(enum_name: &str, variant_name: &str, payload: &[Value]) -> u64 {
    let mut h = HASH_BASIS;
    for c in enum_name.bytes() {
        h = fnv1a_combine(h, c as u64);
    }
    for c in variant_name.bytes() {
        h = fnv1a_combine(h, c as u64);
    }
    for p in payload {
        h = fnv1a_combine(h, hash_value(p));
    }
    h
}
