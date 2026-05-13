use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

use indexmap::IndexMap;

use crate::bytecode::{
    ClosureValue, EnumValue, ErrorValue, Function, Op, Program, SocketValue, StructValue,
    TupleValue, Value, compute_enum_hash, compute_struct_hash,
};
use crate::flags::Flags;

type VmResult<T> = Result<T, String>;

#[derive(Debug, Clone)]
struct CallFrame {
    func: Function,
    func_idx: i32,
    ip: i32,
    base_slot: usize,
    captures: Vec<Value>,
}

pub struct VM {
    flags: Flags,
    program: Program,
    stack: Vec<Value>,
    frames: Vec<CallFrame>,
    next_socket_id: i32,
    tcp_listeners: HashMap<i32, TcpListener>,
    tcp_connections: HashMap<i32, TcpStream>,
}

pub fn new_vm(program: Program, fl: Flags) -> VM {
    VM {
        flags: fl,
        program,
        stack: Vec::new(),
        frames: Vec::new(),
        next_socket_id: 1,
        tcp_listeners: HashMap::new(),
        tcp_connections: HashMap::new(),
    }
}

impl VM {
    pub fn run(&mut self) -> VmResult<Value> {
        let main_func = self.program.functions[self.program.entry as usize].clone();

        self.frames.push(CallFrame {
            func: main_func.clone(),
            func_idx: self.program.entry,
            ip: 0,
            base_slot: 0,
            captures: Vec::new(),
        });

        for _ in 0..main_func.locals {
            self.stack.push(Value::None);
        }

        self.execute()
    }

    fn execute(&mut self) -> VmResult<Value> {
        while !self.frames.is_empty() {
            // Read frame scalars up-front; re-index for writes to satisfy the borrow checker.
            let (code_start, ip, base_slot) = {
                let f = self.frames.last().unwrap();
                (f.func.code_start, f.ip, f.base_slot)
            };

            let addr = code_start + ip;
            if addr as usize >= self.program.code.len() {
                break;
            }

            let instr = self.program.code[addr as usize];
            self.frames.last_mut().unwrap().ip += 1;

            match instr.op {
                Op::PushConst => {
                    self.stack
                        .push(self.program.constants[instr.operand as usize].clone());
                }
                Op::PushLocal => {
                    let slot = base_slot + instr.operand as usize;
                    self.stack.push(self.stack[slot].clone());
                }
                Op::StoreLocal => {
                    let slot = base_slot + instr.operand as usize;
                    let v = self.pop()?;
                    self.stack[slot] = v;
                }
                Op::PushNone => {
                    self.stack.push(Value::None);
                }
                Op::PushTrue => {
                    self.stack.push(Value::Bool(true));
                }
                Op::PushFalse => {
                    self.stack.push(Value::Bool(false));
                }
                Op::Pop => {
                    self.pop()?;
                }
                Op::Dup => {
                    let v = self.peek()?.clone();
                    self.stack.push(v);
                }
                Op::Swap => {
                    if self.stack.len() < 2 {
                        return Err(
                            "Stack underflow on swap. This is likely a compiler bug.".to_string()
                        );
                    }
                    let top = self.stack.len() - 1;
                    self.stack.swap(top, top - 1);
                }
                Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Mod => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    self.stack.push(binary_op(a, b, instr.op)?);
                }
                Op::Neg => {
                    let a = self.pop()?;
                    match a {
                        Value::Int(i) => self.stack.push(Value::Int(-i)),
                        Value::Float(f) => self.stack.push(Value::Float(-f)),
                        _ => {
                            return Err(format!(
                                "Cannot negate non-numeric value '{}'",
                                value_type_name(&a)
                            ));
                        }
                    }
                }
                Op::Eq => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    self.stack.push(Value::Bool(values_equal(&a, &b)));
                }
                Op::Neq => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    self.stack.push(Value::Bool(!values_equal(&a, &b)));
                }
                Op::Lt | Op::Gt | Op::Lte | Op::Gte => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    self.stack.push(Value::Bool(compare(a, b, instr.op)?));
                }
                Op::Not => {
                    let a = self.pop()?;
                    self.stack.push(Value::Bool(!is_truthy(&a)));
                }
                Op::Jump => {
                    self.frames.last_mut().unwrap().ip = instr.operand - code_start;
                }
                Op::JumpIfFalse => {
                    let cond = self.pop()?;
                    if !is_truthy(&cond) {
                        self.frames.last_mut().unwrap().ip = instr.operand - code_start;
                    }
                }
                Op::JumpIfTrue => {
                    let cond = self.pop()?;
                    if is_truthy(&cond) {
                        self.frames.last_mut().unwrap().ip = instr.operand - code_start;
                    }
                }
                Op::Call => {
                    let arity = instr.operand;
                    let callee = self.pop()?;

                    if let Value::Closure(cl) = callee {
                        let func = self.program.functions[cl.func_idx as usize].clone();

                        if arity != func.arity {
                            return Err(format!(
                                "Expected {} arguments, got {}",
                                func.arity, arity
                            ));
                        }

                        let new_base = self.stack.len() - arity as usize;

                        for _ in arity..func.locals {
                            self.stack.push(Value::None);
                        }

                        self.frames.push(CallFrame {
                            func,
                            func_idx: cl.func_idx,
                            ip: 0,
                            base_slot: new_base,
                            captures: cl.captures,
                        });
                    } else {
                        return Err("Cannot call non-function".to_string());
                    }
                }
                Op::TailCall => {
                    let arity = instr.operand;
                    let callee = self.pop()?;

                    if let Value::Closure(cl) = callee {
                        let func = self.program.functions[cl.func_idx as usize].clone();

                        if arity != func.arity {
                            return Err(format!(
                                "Expected {} arguments, got {}",
                                func.arity, arity
                            ));
                        }

                        let mut args: Vec<Value> = Vec::with_capacity(arity as usize);
                        for _ in 0..arity {
                            args.push(self.pop()?);
                        }

                        let base = self.frames.last().unwrap().base_slot;

                        self.stack.truncate(base);

                        for i in (0..arity as usize).rev() {
                            self.stack.push(args[i].clone());
                        }

                        for _ in arity..func.locals {
                            self.stack.push(Value::None);
                        }

                        let current_frame = self.frames.last_mut().unwrap();
                        current_frame.func = func;
                        current_frame.func_idx = cl.func_idx;
                        current_frame.ip = 0;
                        current_frame.captures = cl.captures;
                    } else {
                        return Err("Cannot tail_call non-function".to_string());
                    }
                }
                Op::Ret => {
                    let ret_val = self.pop()?;
                    let old_frame = self.frames.pop().unwrap();

                    self.stack.truncate(old_frame.base_slot);

                    self.stack.push(ret_val);

                    if self.frames.is_empty() {
                        break;
                    }
                }
                Op::MakeArray => {
                    let len = instr.operand as usize;
                    let mut arr = vec![Value::None; len];
                    for i in (0..len).rev() {
                        arr[i] = self.pop()?;
                    }
                    self.stack.push(Value::Array(arr));
                }
                Op::MakeTuple => {
                    let len = instr.operand as usize;
                    let mut arr = vec![Value::None; len];
                    for i in (0..len).rev() {
                        arr[i] = self.pop()?;
                    }
                    self.stack.push(Value::Tuple(TupleValue { elements: arr }));
                }
                Op::TupleIndex => {
                    let tuple_val = self.pop()?;
                    if let Value::Tuple(t) = tuple_val {
                        let idx = instr.operand;
                        if idx >= 0 && (idx as usize) < t.elements.len() {
                            self.stack.push(t.elements[idx as usize].clone());
                        } else {
                            return Err(format!(
                                "Tuple index {} out of bounds (len: {})",
                                idx,
                                t.elements.len()
                            ));
                        }
                    } else {
                        return Err(format!(
                            "Expected tuple, got '{}'",
                            value_type_name(&tuple_val)
                        ));
                    }
                }
                Op::MakeRange => {
                    let end_val = self.pop()?;
                    let start_val = self.pop()?;

                    if let (Value::Int(start), Value::Int(end)) = (&start_val, &end_val) {
                        let arr: Vec<Value> = (*start..*end).map(Value::Int).collect();
                        self.stack.push(Value::Array(arr));
                    } else {
                        return Err(format!(
                            "Range bounds must be integers, got '{}' and '{}'",
                            value_type_name(&start_val),
                            value_type_name(&end_val)
                        ));
                    }
                }
                Op::Index => {
                    let idx_val = self.pop()?;
                    let arr_val = self.pop()?;

                    if let Value::Array(arr) = arr_val {
                        if let Value::Int(idx) = idx_val {
                            if idx >= 0 && (idx as usize) < arr.len() {
                                self.stack.push(arr[idx as usize].clone());
                            } else {
                                self.stack.push(Value::None);
                            }
                        } else {
                            self.stack.push(Value::None);
                        }
                    } else {
                        self.stack.push(Value::None);
                    }
                }
                Op::ArrayLen => {
                    let arr_val = self.pop()?;
                    match arr_val {
                        Value::Array(a) => self.stack.push(Value::Int(a.len() as i64)),
                        Value::Tuple(t) => self.stack.push(Value::Int(t.elements.len() as i64)),
                        _ => {
                            return Err(format!(
                                "Cannot get length of non-array type '{}'",
                                value_type_name(&arr_val)
                            ));
                        }
                    }
                }
                Op::ArraySlice => {
                    let end_val = self.pop()?;
                    let start_val = self.pop()?;
                    let arr_val = self.pop()?;

                    if let Value::Array(arr) = &arr_val {
                        if let (Value::Int(start), Value::Int(end)) = (&start_val, &end_val) {
                            let start = *start;
                            let end = *end;
                            if start >= 0 && (end as usize) <= arr.len() && start <= end {
                                let sliced = arr[start as usize..end as usize].to_vec();
                                self.stack.push(Value::Array(sliced));
                            } else {
                                return Err(format!(
                                    "Slice indices out of bounds: [{}..{}] (array length is {})",
                                    start,
                                    end,
                                    arr.len()
                                ));
                            }
                        } else {
                            return Err("Slice indices must be integers".to_string());
                        }
                    } else {
                        return Err(format!(
                            "Cannot slice non-array type '{}'",
                            value_type_name(&arr_val)
                        ));
                    }
                }
                Op::ArrayConcat => {
                    let arr2_val = self.pop()?;
                    let arr1_val = self.pop()?;

                    if let (Value::Array(a1), Value::Array(a2)) = (arr1_val, arr2_val) {
                        let mut result = Vec::with_capacity(a1.len() + a2.len());
                        result.extend(a1);
                        result.extend(a2);
                        self.stack.push(Value::Array(result));
                    } else {
                        return Err("Cannot concatenate non-array types".to_string());
                    }
                }
                Op::MakeStruct => {
                    let field_count = instr.operand;

                    let type_name_val = self.pop()?;
                    let type_name = if let Value::Str(s) = type_name_val {
                        s
                    } else {
                        return Err("Struct type name must be string".to_string());
                    };

                    let type_id_val = self.pop()?;
                    let type_id = if let Value::Int(i) = type_id_val {
                        i as i32
                    } else {
                        return Err("Struct type id must be int".to_string());
                    };

                    let mut fields: IndexMap<String, Value> = IndexMap::new();
                    for _ in 0..field_count {
                        let val = self.pop()?;
                        let name_val = self.pop()?;
                        let name = if let Value::Str(s) = name_val {
                            s
                        } else {
                            return Err("Field name must be string".to_string());
                        };
                        fields.insert(name, val);
                    }
                    let hash = compute_struct_hash(&type_name, &fields);
                    self.stack.push(Value::Struct(StructValue {
                        type_id,
                        type_name,
                        fields,
                        hash,
                    }));
                }
                Op::GetField => {
                    let field_name_idx = instr.operand;
                    let field_name_val = &self.program.constants[field_name_idx as usize];
                    let field_name = if let Value::Str(s) = field_name_val {
                        s.clone()
                    } else {
                        return Err("Field name must be string".to_string());
                    };
                    let struct_val = self.pop()?;
                    if let Value::Struct(sv) = struct_val {
                        if let Some(val) = sv.fields.get(&field_name) {
                            self.stack.push(val.clone());
                        } else {
                            return Err(format!("Unknown field: {}", field_name));
                        }
                    } else {
                        return Err("Cannot access field on non-struct".to_string());
                    }
                }
                Op::MakeClosure => {
                    let func_idx = instr.operand;
                    let func = self.program.functions[func_idx as usize].clone();

                    let cc = func.capture_count as usize;
                    let mut captures = vec![Value::None; cc];
                    for i in (0..cc).rev() {
                        captures[i] = self.pop()?;
                    }

                    self.stack.push(Value::Closure(ClosureValue {
                        func_idx,
                        captures,
                        name: func.name,
                    }));
                }
                Op::PushCapture => {
                    let capture_idx = instr.operand as usize;
                    if !self.frames.is_empty() {
                        let current_frame = self.frames.last().unwrap();
                        if capture_idx < current_frame.captures.len() {
                            self.stack.push(current_frame.captures[capture_idx].clone());
                        } else {
                            return Err(format!("Capture index out of bounds: {}", capture_idx));
                        }
                    }
                }
                Op::PushSelf => {
                    if !self.frames.is_empty() {
                        let current_frame = self.frames.last().unwrap();
                        let cl = ClosureValue {
                            func_idx: current_frame.func_idx,
                            captures: current_frame.captures.clone(),
                            name: current_frame.func.name.clone(),
                        };
                        self.stack.push(Value::Closure(cl));
                    }
                }
                Op::Print => {
                    let val = self.pop()?;
                    println!("{}", inspect(&val));
                }
                Op::StackDepth => {
                    self.stack.push(Value::Int(self.frames.len() as i64));
                }
                Op::MakeEnum => {
                    let variant_name_val = self.pop()?;
                    let enum_name_val = self.pop()?;
                    let type_id_val = self.pop()?;

                    let type_id = if let Value::Int(i) = type_id_val {
                        i as i32
                    } else {
                        return Err("Enum type id must be int".to_string());
                    };

                    let enum_name = if let Value::Str(s) = enum_name_val {
                        s
                    } else {
                        return Err("Enum name must be string".to_string());
                    };

                    let variant_name = if let Value::Str(s) = variant_name_val {
                        s
                    } else {
                        return Err("Variant name must be string".to_string());
                    };

                    let hash = compute_enum_hash(&enum_name, &variant_name, &[]);
                    self.stack.push(Value::Enum(EnumValue {
                        type_id,
                        enum_name,
                        variant_name,
                        payload: Vec::new(),
                        hash,
                    }));
                }
                Op::MakeEnumPayload => {
                    let payload_count = instr.operand as usize;

                    let mut payloads: Vec<Value> = Vec::with_capacity(payload_count);
                    for _ in 0..payload_count {
                        payloads.push(self.pop()?);
                    }
                    payloads.reverse();

                    let variant_name_val = self.pop()?;
                    let enum_name_val = self.pop()?;
                    let type_id_val = self.pop()?;

                    let type_id = if let Value::Int(i) = type_id_val {
                        i as i32
                    } else {
                        return Err("Enum type id must be int".to_string());
                    };

                    let enum_name = if let Value::Str(s) = enum_name_val {
                        s
                    } else {
                        return Err("Enum name must be string".to_string());
                    };
                    let variant_name = if let Value::Str(s) = variant_name_val {
                        s
                    } else {
                        return Err("Variant name must be string".to_string());
                    };

                    let hash = compute_enum_hash(&enum_name, &variant_name, &payloads);
                    self.stack.push(Value::Enum(EnumValue {
                        type_id,
                        enum_name,
                        variant_name,
                        payload: payloads,
                        hash,
                    }));
                }
                Op::MatchEnum => {
                    let variant_name = self.pop()?;
                    let enum_name = self.pop()?;
                    let type_id_val = self.pop()?;
                    let val = self.pop()?;

                    if !matches!(variant_name, Value::Str(_)) || !matches!(enum_name, Value::Str(_))
                    {
                        return Err("Enum/variant names must be strings".to_string());
                    }
                    let type_id = if let Value::Int(i) = type_id_val {
                        i as i32
                    } else {
                        return Err("Enum type id must be int".to_string());
                    };
                    let variant_str = if let Value::Str(s) = variant_name {
                        s
                    } else {
                        unreachable!()
                    };

                    if let Value::Enum(ev) = val {
                        self.stack.push(Value::Bool(
                            ev.type_id == type_id && ev.variant_name == variant_str,
                        ));
                    } else {
                        self.stack.push(Value::Bool(false));
                    }
                }
                Op::UnwrapEnum => {
                    let enum_val = self.pop()?;
                    if let Value::Enum(ev) = enum_val {
                        if !ev.payload.is_empty() {
                            for p in ev.payload {
                                self.stack.push(p);
                            }
                        } else {
                            self.stack.push(Value::None);
                        }
                    } else {
                        return Err("Cannot unwrap non-enum value".to_string());
                    }
                }
                Op::MakeError => {
                    let payload = self.pop()?;
                    self.stack
                        .push(Value::Error(Box::new(ErrorValue { payload })));
                }
                Op::IsError => {
                    let val = self.pop()?;
                    self.stack.push(Value::Bool(matches!(val, Value::Error(_))));
                }
                Op::IsNone => {
                    let val = self.pop()?;
                    self.stack.push(Value::Bool(matches!(val, Value::None)));
                }
                Op::UnwrapError => {
                    let val = self.pop()?;
                    if let Value::Error(e) = val {
                        self.stack.push(e.payload);
                    } else {
                        return Err("Expected error value".to_string());
                    }
                }
                Op::IsFailure => {
                    let val = self.pop()?;
                    self.stack
                        .push(Value::Bool(matches!(val, Value::Error(_) | Value::None)));
                }
                Op::UnwrapFailure => {
                    let val = self.pop()?;
                    if let Value::Error(e) = val {
                        self.stack.push(e.payload);
                    } else {
                        self.stack.push(Value::None);
                    }
                }
                Op::ToString => {
                    let val = self.pop()?;
                    self.stack.push(Value::Str(inspect(&val)));
                }
                Op::StrConcat => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    if let (Value::Str(sa), Value::Str(sb)) = (a, b) {
                        self.stack.push(Value::Str(sa + &sb));
                    } else {
                        return Err("str_concat requires two strings".to_string());
                    }
                }
                Op::Halt => {
                    break;
                }
                Op::FileRead => {
                    if !self.flags.io_enabled {
                        return Err(
                            "I/O operations require --experimental-shitty-io flag".to_string()
                        );
                    }
                    let path_val = self.pop()?;
                    if let Value::Str(path) = path_val {
                        match std::fs::read_to_string(&path) {
                            Ok(content) => self.stack.push(Value::Str(content)),
                            Err(err) => {
                                self.stack.push(Value::Error(Box::new(ErrorValue {
                                    payload: Value::Str(format!("Failed to read file: {}", err)),
                                })));
                                continue;
                            }
                        }
                    } else {
                        return Err("read_file requires string path".to_string());
                    }
                }
                Op::FileWrite => {
                    if !self.flags.io_enabled {
                        return Err(
                            "I/O operations require --experimental-shitty-io flag".to_string()
                        );
                    }
                    let content = self.pop()?;
                    let path_val = self.pop()?;
                    if let (Value::Str(path), Value::Str(content)) = (path_val, content) {
                        match std::fs::write(&path, content) {
                            Ok(_) => self.stack.push(Value::None),
                            Err(err) => {
                                self.stack.push(Value::Error(Box::new(ErrorValue {
                                    payload: Value::Str(format!("Failed to write file: {}", err)),
                                })));
                                continue;
                            }
                        }
                    } else {
                        return Err("write_file requires string path and content".to_string());
                    }
                }
                Op::TcpListen => {
                    if !self.flags.io_enabled {
                        return Err(
                            "I/O operations require --experimental-shitty-io flag".to_string()
                        );
                    }
                    let port_val = self.pop()?;
                    if let Value::Int(port) = port_val {
                        match TcpListener::bind(format!("0.0.0.0:{}", port)) {
                            Ok(listener) => {
                                let socket_id = self.next_socket_id;
                                self.next_socket_id += 1;
                                self.tcp_listeners.insert(socket_id, listener);
                                self.stack.push(Value::Socket(SocketValue {
                                    id: socket_id,
                                    is_listener: true,
                                }));
                            }
                            Err(err) => {
                                self.stack.push(Value::Error(Box::new(ErrorValue {
                                    payload: Value::Str(format!("Failed to listen: {}", err)),
                                })));
                                continue;
                            }
                        }
                    } else {
                        return Err("tcp_listen requires int port".to_string());
                    }
                }
                Op::TcpAccept => {
                    if !self.flags.io_enabled {
                        return Err(
                            "I/O operations require --experimental-shitty-io flag".to_string()
                        );
                    }
                    let socket_val = self.pop()?;
                    if let Value::Socket(sv) = socket_val {
                        if !sv.is_listener {
                            return Err("tcp_accept requires a listener socket".to_string());
                        }
                        if let Some(listener) = self.tcp_listeners.get(&sv.id) {
                            match listener.accept() {
                                Ok((conn, _)) => {
                                    let conn_id = self.next_socket_id;
                                    self.next_socket_id += 1;
                                    self.tcp_connections.insert(conn_id, conn);
                                    self.stack.push(Value::Socket(SocketValue {
                                        id: conn_id,
                                        is_listener: false,
                                    }));
                                }
                                Err(err) => {
                                    self.stack.push(Value::Error(Box::new(ErrorValue {
                                        payload: Value::Str(format!("Failed to accept: {}", err)),
                                    })));
                                    continue;
                                }
                            }
                        } else {
                            return Err("Invalid listener socket".to_string());
                        }
                    } else {
                        return Err("tcp_accept requires socket".to_string());
                    }
                }
                Op::TcpRead => {
                    if !self.flags.io_enabled {
                        return Err(
                            "I/O operations require --experimental-shitty-io flag".to_string()
                        );
                    }
                    let socket_val = self.pop()?;
                    if let Value::Socket(sv) = socket_val {
                        if sv.is_listener {
                            return Err(
                                "tcp_read requires a connection socket, not a listener".to_string()
                            );
                        }
                        if let Some(conn) = self.tcp_connections.get_mut(&sv.id) {
                            let mut buf = [0u8; 4096];
                            match conn.read(&mut buf) {
                                Ok(bytes_read) => {
                                    if bytes_read == 0 {
                                        self.stack.push(Value::None);
                                    } else {
                                        let s =
                                            String::from_utf8_lossy(&buf[..bytes_read]).to_string();
                                        self.stack.push(Value::Str(s));
                                    }
                                }
                                Err(err) => {
                                    self.stack.push(Value::Error(Box::new(ErrorValue {
                                        payload: Value::Str(format!("Failed to read: {}", err)),
                                    })));
                                    continue;
                                }
                            }
                        } else {
                            return Err("Invalid connection socket".to_string());
                        }
                    } else {
                        return Err("tcp_read requires socket".to_string());
                    }
                }
                Op::TcpWrite => {
                    if !self.flags.io_enabled {
                        return Err(
                            "I/O operations require --experimental-shitty-io flag".to_string()
                        );
                    }
                    let data = self.pop()?;
                    let socket_val = self.pop()?;
                    if let (Value::Socket(sv), Value::Str(data)) = (socket_val, data) {
                        if sv.is_listener {
                            return Err("tcp_write requires a connection socket, not a listener"
                                .to_string());
                        }
                        if let Some(conn) = self.tcp_connections.get_mut(&sv.id) {
                            match conn.write(data.as_bytes()) {
                                Ok(bytes_written) => {
                                    self.stack.push(Value::Int(bytes_written as i64));
                                }
                                Err(err) => {
                                    self.stack.push(Value::Error(Box::new(ErrorValue {
                                        payload: Value::Str(format!("Failed to write: {}", err)),
                                    })));
                                    continue;
                                }
                            }
                        } else {
                            return Err("Invalid connection socket".to_string());
                        }
                    } else {
                        return Err("tcp_write requires socket and string data".to_string());
                    }
                }
                Op::TcpClose => {
                    if !self.flags.io_enabled {
                        return Err(
                            "I/O operations require --experimental-shitty-io flag".to_string()
                        );
                    }
                    let socket_val = self.pop()?;
                    if let Value::Socket(sv) = socket_val {
                        if sv.is_listener {
                            self.tcp_listeners.remove(&sv.id);
                        } else {
                            self.tcp_connections.remove(&sv.id);
                        }
                        self.stack.push(Value::None);
                    } else {
                        return Err("tcp_close requires socket".to_string());
                    }
                }
                Op::StrSplit => {
                    if !self.flags.std_lib_enabled {
                        return Err(
                            "std lib operations require --experimental-std-lib flag".to_string()
                        );
                    }
                    let delimiter = self.pop()?;
                    let string_val = self.pop()?;

                    if let (Value::Str(s), Value::Str(delim)) = (string_val, delimiter) {
                        let parts: Vec<Value> = if delim.is_empty() {
                            s.chars().map(|c| Value::Str(c.to_string())).collect()
                        } else {
                            s.split(delim.as_str())
                                .map(|p| Value::Str(p.to_string()))
                                .collect()
                        };
                        self.stack.push(Value::Array(parts));
                    } else {
                        return Err("str_split requires string and delimiter".to_string());
                    }
                }
            }
        }

        Ok(self.stack.last().cloned().unwrap_or(Value::None))
    }

    fn pop(&mut self) -> VmResult<Value> {
        self.stack
            .pop()
            .ok_or_else(|| "Stack underflow. This is likely a compiler bug.".to_string())
    }

    fn peek(&self) -> VmResult<&Value> {
        self.stack
            .last()
            .ok_or_else(|| "Stack underflow. This is likely a compiler bug.".to_string())
    }
}

fn binary_op(a: Value, b: Value, op: Op) -> VmResult<Value> {
    match (&a, &b) {
        (Value::Int(ai), Value::Int(bi)) => {
            let (ai, bi) = (*ai, *bi);
            Ok(match op {
                Op::Add => Value::Int(ai + bi),
                Op::Sub => Value::Int(ai - bi),
                Op::Mul => Value::Int(ai * bi),
                Op::Div => Value::Int(ai / bi),
                Op::Mod => Value::Int(ai % bi),
                _ => return Err("Unknown binary op. This is likely a compiler bug.".to_string()),
            })
        }
        (Value::Float(af), Value::Float(bf)) => {
            let (af, bf) = (*af, *bf);
            Ok(match op {
                Op::Add => Value::Float(af + bf),
                Op::Sub => Value::Float(af - bf),
                Op::Mul => Value::Float(af * bf),
                Op::Div => Value::Float(af / bf),
                _ => return Err("Unknown binary op. This is likely a compiler bug.".to_string()),
            })
        }
        (Value::Int(ai), Value::Float(bf)) => {
            let af = *ai as f64;
            let bf = *bf;
            Ok(match op {
                Op::Add => Value::Float(af + bf),
                Op::Sub => Value::Float(af - bf),
                Op::Mul => Value::Float(af * bf),
                Op::Div => Value::Float(af / bf),
                _ => return Err("Unknown binary op. This is likely a compiler bug.".to_string()),
            })
        }
        (Value::Float(af), Value::Int(bi)) => {
            let af = *af;
            let bf = *bi as f64;
            Ok(match op {
                Op::Add => Value::Float(af + bf),
                Op::Sub => Value::Float(af - bf),
                Op::Mul => Value::Float(af * bf),
                Op::Div => Value::Float(af / bf),
                _ => return Err("Unknown binary op. This is likely a compiler bug.".to_string()),
            })
        }
        (Value::Str(sa), Value::Str(sb)) if op == Op::Add => Ok(Value::Str(sa.clone() + sb)),
        _ => Err(format!(
            "Cannot perform arithmetic on '{}' and '{}'. This is likely a compiler bug.",
            value_type_name(&a),
            value_type_name(&b)
        )),
    }
}

fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Float(x), Value::Float(y)) => x == y,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Str(x), Value::Str(y)) => x == y,
        (Value::None, Value::None) => true,
        (Value::Enum(ae), Value::Enum(be)) => {
            if ae.hash != be.hash {
                return false;
            }
            if ae.type_id != be.type_id {
                return false;
            }
            if ae.variant_name != be.variant_name {
                return false;
            }
            if ae.payload.is_empty() && be.payload.is_empty() {
                return true;
            }
            if ae.payload.len() != be.payload.len() {
                return false;
            }
            for (av, bv) in ae.payload.iter().zip(be.payload.iter()) {
                if !values_equal(av, bv) {
                    return false;
                }
            }
            true
        }
        (Value::Struct(asv), Value::Struct(bsv)) => {
            if asv.hash != bsv.hash {
                return false;
            }
            if asv.type_id != bsv.type_id {
                return false;
            }
            if asv.fields.is_empty() && bsv.fields.is_empty() {
                return true;
            }
            if asv.fields.len() != bsv.fields.len() {
                return false;
            }
            for (key, av) in asv.fields.iter() {
                match bsv.fields.get(key) {
                    Some(bv) => {
                        if !values_equal(av, bv) {
                            return false;
                        }
                    }
                    None => return false,
                }
            }
            true
        }
        (Value::Array(aa), Value::Array(ba)) => {
            if aa.len() != ba.len() {
                return false;
            }
            if aa.is_empty() {
                return true;
            }
            for (av, bv) in aa.iter().zip(ba.iter()) {
                if !values_equal(av, bv) {
                    return false;
                }
            }
            true
        }
        (Value::Tuple(at), Value::Tuple(bt)) => {
            if at.elements.len() != bt.elements.len() {
                return false;
            }
            for (av, bv) in at.elements.iter().zip(bt.elements.iter()) {
                if !values_equal(av, bv) {
                    return false;
                }
            }
            true
        }
        (Value::Closure(_), _) => false,
        (Value::Error(ae), Value::Error(be)) => values_equal(&ae.payload, &be.payload),
        (Value::Socket(asv), Value::Socket(bsv)) => {
            asv.id == bsv.id && asv.is_listener == bsv.is_listener
        }
        _ => false,
    }
}

fn compare(a: Value, b: Value, op: Op) -> VmResult<bool> {
    match (&a, &b) {
        (Value::Int(ai), Value::Int(bi)) => Ok(match op {
            Op::Lt => ai < bi,
            Op::Gt => ai > bi,
            Op::Lte => ai <= bi,
            Op::Gte => ai >= bi,
            _ => return Err("Unknown compare op. This is likely a compiler bug.".to_string()),
        }),
        (Value::Float(af), Value::Float(bf)) => Ok(match op {
            Op::Lt => af < bf,
            Op::Gt => af > bf,
            Op::Lte => af <= bf,
            Op::Gte => af >= bf,
            _ => return Err("Unknown compare op. This is likely a compiler bug.".to_string()),
        }),
        _ => Err(format!(
            "Cannot compare '{}' with '{}'. This is likely a compiler bug.",
            value_type_name(&a),
            value_type_name(&b)
        )),
    }
}

fn is_truthy(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        _ => false,
    }
}

fn value_type_name(v: &Value) -> String {
    match v {
        Value::Int(_) => "Int".to_string(),
        Value::Float(_) => "Float".to_string(),
        Value::Bool(_) => "Bool".to_string(),
        Value::Str(_) => "String".to_string(),
        Value::None => "None".to_string(),
        Value::Array(_) => "Array".to_string(),
        Value::Tuple(_) => "Tuple".to_string(),
        Value::Struct(s) => s.type_name.clone(),
        Value::Closure(_) => "Function".to_string(),
        Value::Enum(e) => e.enum_name.clone(),
        Value::Error(_) => "Error".to_string(),
        Value::Socket(_) => "Socket".to_string(),
    }
}

pub fn inspect(v: &Value) -> String {
    inspect_pretty(v, 0)
}

fn is_simple_value(v: &Value) -> bool {
    match v {
        Value::Int(_) | Value::Float(_) | Value::Bool(_) | Value::None | Value::Closure(_) => true,
        Value::Str(s) => s.len() < 20,
        Value::Enum(e) => e.payload.is_empty(),
        _ => false,
    }
}

fn f64_str(f: f64) -> String {
    // Match V's f64.str() — always include a decimal point for finite values.
    let s = format!("{}", f);
    if f.is_finite() && !s.contains('.') && !s.contains('e') && !s.contains('E') {
        format!("{}.0", s)
    } else {
        s
    }
}

fn inspect_inline(v: &Value) -> String {
    match v {
        Value::Int(i) => i.to_string(),
        Value::Float(f) => f64_str(*f),
        Value::Bool(b) => {
            if *b {
                "true".to_string()
            } else {
                "false".to_string()
            }
        }
        Value::Str(s) => s.clone(),
        Value::None => "none".to_string(),
        Value::Closure(c) => format!("<fn#{}>", c.name),
        Value::Enum(e) => {
            if !e.payload.is_empty() {
                let parts: Vec<String> = e.payload.iter().map(inspect_inline).collect();
                format!("{}.{}({})", e.enum_name, e.variant_name, parts.join(", "))
            } else {
                format!("{}.{}", e.enum_name, e.variant_name)
            }
        }
        Value::Error(e) => format!("error({})", inspect_inline(&e.payload)),
        Value::Socket(s) => {
            if s.is_listener {
                format!("<listener#{}>", s.id)
            } else {
                format!("<socket#{}>", s.id)
            }
        }
        Value::Array(arr) => {
            let mut s = String::from("[");
            for (i, elem) in arr.iter().enumerate() {
                if i > 0 {
                    s.push_str(", ");
                }
                s.push_str(&inspect_inline(elem));
            }
            s.push(']');
            s
        }
        Value::Tuple(t) => {
            let mut s = String::from("(");
            for (i, elem) in t.elements.iter().enumerate() {
                if i > 0 {
                    s.push_str(", ");
                }
                s.push_str(&inspect_inline(elem));
            }
            s.push(')');
            s
        }
        Value::Struct(sv) => {
            let mut s = format!("{}{{ ", sv.type_name);
            let mut first = true;
            for (name, val) in sv.fields.iter() {
                if !first {
                    s.push_str(", ");
                }
                s.push_str(&format!("{}: {}", name, inspect_inline(val)));
                first = false;
            }
            s.push_str(" }");
            s
        }
    }
}

fn inspect_pretty(v: &Value, indent: usize) -> String {
    let tab = "  ";
    let pad = tab.repeat(indent);
    let pad_inner = tab.repeat(indent + 1);

    match v {
        Value::Int(i) => i.to_string(),
        Value::Float(f) => f64_str(*f),
        Value::Bool(b) => {
            if *b {
                "true".to_string()
            } else {
                "false".to_string()
            }
        }
        Value::Str(s) => s.clone(),
        Value::None => "none".to_string(),
        Value::Closure(c) => format!("<fn#{}>", c.name),
        Value::Socket(s) => {
            if s.is_listener {
                format!("<listener#{}>", s.id)
            } else {
                format!("<socket#{}>", s.id)
            }
        }
        Value::Error(e) => format!("error({})", inspect_pretty(&e.payload, indent)),
        Value::Enum(e) => {
            if !e.payload.is_empty() {
                let all_simple = e.payload.iter().all(is_simple_value);
                if all_simple {
                    let parts: Vec<String> = e.payload.iter().map(inspect_inline).collect();
                    return format!("{}.{}({})", e.enum_name, e.variant_name, parts.join(", "));
                }
                let mut s = format!("{}.{}(\n", e.enum_name, e.variant_name);
                for (i, p) in e.payload.iter().enumerate() {
                    s.push_str(&format!("{}{}", pad_inner, inspect_pretty(p, indent + 1)));
                    if i < e.payload.len() - 1 {
                        s.push(',');
                    }
                    s.push('\n');
                }
                s.push_str(&format!("{})", pad));
                s
            } else {
                format!("{}.{}", e.enum_name, e.variant_name)
            }
        }
        Value::Array(arr) => {
            if arr.is_empty() {
                return "[]".to_string();
            }
            let all_simple = arr.iter().all(is_simple_value);
            if all_simple {
                let inline = inspect_inline(v);
                if inline.len() <= 80 {
                    return inline;
                }
                let mut s = String::from("[\n");
                let mut line = pad_inner.clone();
                let items_per_line = 6;
                for (i, elem) in arr.iter().enumerate() {
                    let item = inspect_inline(elem);
                    line.push_str(&item);
                    let is_last = i == arr.len() - 1;
                    if !is_last {
                        line.push_str(", ");
                    }
                    if (i + 1) % items_per_line == 0 && !is_last {
                        s.push_str(&line);
                        s.push('\n');
                        line = pad_inner.clone();
                    }
                }
                if line != pad_inner {
                    s.push_str(&line);
                    s.push('\n');
                }
                s.push_str(&format!("{}]", pad));
                return s;
            }
            let mut s = String::from("[\n");
            for (i, elem) in arr.iter().enumerate() {
                s.push_str(&format!(
                    "{}{}",
                    pad_inner,
                    inspect_pretty(elem, indent + 1)
                ));
                if i < arr.len() - 1 {
                    s.push(',');
                }
                s.push('\n');
            }
            s.push_str(&format!("{}]", pad));
            s
        }
        Value::Tuple(t) => {
            if t.elements.is_empty() {
                return "()".to_string();
            }
            let all_simple = t.elements.iter().all(is_simple_value);
            if all_simple {
                let inline = inspect_inline(v);
                if inline.len() <= 80 {
                    return inline;
                }
            }
            let mut s = String::from("(\n");
            for (i, elem) in t.elements.iter().enumerate() {
                s.push_str(&format!(
                    "{}{}",
                    pad_inner,
                    inspect_pretty(elem, indent + 1)
                ));
                if i < t.elements.len() - 1 {
                    s.push(',');
                }
                s.push('\n');
            }
            s.push_str(&format!("{})", pad));
            s
        }
        Value::Struct(sv) => {
            if sv.fields.is_empty() {
                return format!("{}{{}}", sv.type_name);
            }
            let all_simple = sv.fields.values().all(is_simple_value);
            if all_simple && sv.fields.len() <= 3 {
                let inline = inspect_inline(v);
                if inline.len() <= 60 {
                    return inline;
                }
            }
            let mut s = format!("{} {{\n", sv.type_name);
            let n = sv.fields.len();
            for (i, (name, val)) in sv.fields.iter().enumerate() {
                s.push_str(&format!(
                    "{}{}: {}",
                    pad_inner,
                    name,
                    inspect_pretty(val, indent + 1)
                ));
                if i < n - 1 {
                    s.push(',');
                }
                s.push('\n');
            }
            s.push_str(&format!("{}}}", pad));
            s
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::{Instruction, op, op_arg};

    #[test]
    fn test_push_add_halt() {
        let program = Program {
            constants: vec![Value::Int(1), Value::Int(2)],
            functions: vec![Function {
                name: "main".to_string(),
                arity: 0,
                locals: 0,
                capture_count: 0,
                code_start: 0,
                code_len: 4,
            }],
            code: vec![
                op_arg(Op::PushConst, 0),
                op_arg(Op::PushConst, 1),
                op(Op::Add),
                op(Op::Halt),
            ],
            entry: 0,
        };

        let mut vm = new_vm(program, Flags::default());
        let result = vm.run().expect("vm should run cleanly");
        match result {
            Value::Int(3) => {}
            other => panic!("expected Int(3), got {:?}", other),
        }
    }

    #[test]
    fn test_inspect_basic() {
        assert_eq!(inspect(&Value::Int(42)), "42");
        assert_eq!(inspect(&Value::Bool(true)), "true");
        assert_eq!(inspect(&Value::None), "none");
        assert_eq!(
            inspect(&Value::Array(vec![Value::Int(1), Value::Int(2)])),
            "[1, 2]"
        );
    }

    #[test]
    fn test_values_equal() {
        assert!(values_equal(&Value::Int(5), &Value::Int(5)));
        assert!(!values_equal(&Value::Int(5), &Value::Int(6)));
        assert!(values_equal(&Value::None, &Value::None));
        assert!(!values_equal(&Value::Int(5), &Value::Str("5".to_string())));
    }

    #[allow(dead_code)]
    fn _instruction_is_copy(_: Instruction) {}
}
