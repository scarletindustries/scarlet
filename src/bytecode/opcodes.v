module bytecode

pub enum Op {
	// Stack manipulation
	push_const  // push constant from pool: push_const <idx>
	push_local  // push local variable: push_local <idx>
	store_local // pop and store in local: store_local <idx>
	push_none   // push none value
	push_true   // push true
	push_false  // push false
	pop         // discard top of stack
	dup         // duplicate top of stack
	swap        // swap top two stack elements

	// Arithmetic
	add
	sub
	mul
	div
	mod
	neg // negate (unary minus)

	// Comparison
	eq
	neq
	lt
	gt
	lte
	gte

	// Logic
	not

	// Control flow
	jump          // unconditional jump: jump <addr>
	jump_if_false // conditional jump: jump_if_false <addr>
	jump_if_true  // conditional jump: jump_if_true <addr>
	call          // call function: call <arity>
	tail_call     // tail call (reuses stack frame): tail_call <arity>
	ret           // return top of stack

	// Data structures
	make_array   // pop N items, push array: make_array <len>
	make_range   // pop end, pop start, push array [start..end)
	index        // pop index, pop array, push element
	array_len    // pop array, push int (length)
	array_slice  // pop end, pop start, pop array, push slice [start..end)
	array_concat // pop arr2, pop arr1, push concatenated array
	make_struct  // create struct: pop type_name, pop N (field_name, value) pairs: make_struct <field_count>
	get_field    // get struct field: get_field <field_name_idx> (field name in constant pool)

	// Enums
	make_enum         // create enum: pop variant_name, pop enum_name, push EnumValue (no payload)
	make_enum_payload // create enum with payload: pop payload, pop variant_name, pop enum_name, push EnumValue
	match_enum        // match variant only (ignore payload): pop variant_name, pop enum_name, pop value, push bool
	unwrap_enum       // get payload from enum: pop enum, push payload (or none)

	// Closures
	make_closure // create closure: pop N captures (N = func.capture_count), push ClosureValue; operand = func_idx
	push_capture // push captured variable from current closure: push_capture <idx>
	push_self    // push the currently-executing closure (for self-recursive calls)

	// Error handling
	make_error   // pop value, push ErrorValue
	is_error     // pop value, push bool (true if ErrorValue)
	is_none      // pop value, push bool (true if NoneValue)
	unwrap_error // pop ErrorValue, push payload value

	// String operations
	to_string  // pop value, push string representation
	str_concat // pop two strings, push concatenated result
	str_split  // pop string, pop separator, push array of strings

	// Misc
	print       // print top of stack (temporary, for debugging)
	stack_depth // push current stack frame count (for debugging TCO)
	halt        // stop execution

	// I/O operations (experimental)
	file_read  // pop path, push file contents as string
	file_write // pop content, pop path, write to file, push none
	tcp_listen // pop port, create listener, push SocketValue
	tcp_accept // pop listener, accept connection, push SocketValue (blocks)
	tcp_read   // pop socket, read data, push string (blocks)
	tcp_write  // pop data, pop socket, write data, push bytes written
	tcp_close  // pop socket, close it, push none
}

pub struct Instruction {
pub:
	op      Op
	operand int // optional operand (index, address, count, etc.)
}

pub struct Function {
pub:
	name          string
	arity         int // number of parameters
	locals        int // number of local variables (including params)
	capture_count int // number of captured variables from enclosing scope
	code_start    int // starting address in bytecode
	code_len      int // length of function bytecode
}

pub type Value = int
	| f64
	| bool
	| string
	| NoneValue
	| []Value
	| StructValue
	| ClosureValue
	| EnumValue
	| ErrorValue
	| SocketValue

pub struct NoneValue {}

pub struct ErrorValue {
pub:
	payload Value
}

pub struct SocketValue {
pub:
	id          int
	is_listener bool
}

pub struct EnumValue {
pub:
	type_id      int @[required]
	enum_name    string  // e.g., "MyEnum" (for display only)
	variant_name string  // e.g., "C"
	payload      []Value // payload values (empty if no payload)
	hash         u64
}

pub struct StructValue {
pub mut:
	type_id   int @[required]
	type_name string // for display only
	fields    map[string]Value
	hash      u64
}

pub struct ClosureValue {
pub:
	func_idx int
	captures []Value
	name     string
}

pub struct Program {
pub mut:
	constants []Value       // constant pool
	functions []Function    // function table
	code      []Instruction // all bytecode
	entry     int           // entry point (index into functions)
}

pub fn op(o Op) Instruction {
	return Instruction{
		op:      o
		operand: 0
	}
}

pub fn op_arg(o Op, operand int) Instruction {
	return Instruction{
		op:      o
		operand: operand
	}
}

// FNV-1a hash for combining values
fn fnv1a_combine(h u64, val u64) u64 {
	return (h ^ val) * 0x100000001b3
}

pub fn hash_value(v Value) u64 {
	mut h := hash_basis
	match v {
		int {
			h = fnv1a_combine(h, u64(v))
		}
		f64 {
			// reinterpret f64 bits as u64
			unsafe {
				bits := *(&u64(&v))
				h = fnv1a_combine(h, bits)
			}
		}
		bool {
			h = fnv1a_combine(h, if v { u64(1) } else { u64(0) })
		}
		string {
			for c in v {
				h = fnv1a_combine(h, u64(c))
			}
		}
		NoneValue {
			h = fnv1a_combine(h, 0)
		}
		EnumValue {
			h = v.hash
		}
		StructValue {
			h = v.hash
		}
		[]Value {
			for item in v {
				h = fnv1a_combine(h, hash_value(item))
			}
		}
		else {
			h = fnv1a_combine(h, 0)
		}
	}
	return h
}

// FNV offset basis
const hash_basis = u64(0xcbf29ce484222325)

pub fn compute_struct_hash(type_name string, fields map[string]Value) u64 {
	mut h := hash_basis
	// hash the type name first (nominal typing)
	for c in type_name {
		h = fnv1a_combine(h, u64(c))
	}
	// hash fields in sorted order for consistency
	mut keys := fields.keys()
	keys.sort()
	for key in keys {
		for c in key {
			h = fnv1a_combine(h, u64(c))
		}
		h = fnv1a_combine(h, hash_value(fields[key] or { NoneValue{} }))
	}
	return h
}

pub fn compute_enum_hash(enum_name string, variant_name string, payload []Value) u64 {
	mut h := hash_basis
	for c in enum_name {
		h = fnv1a_combine(h, u64(c))
	}
	for c in variant_name {
		h = fnv1a_combine(h, u64(c))
	}
	for p in payload {
		h = fnv1a_combine(h, hash_value(p))
	}
	return h
}
