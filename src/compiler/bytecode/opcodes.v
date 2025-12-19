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
	and
	or

	// Control flow
	jump          // unconditional jump: jump <addr>
	jump_if_false // conditional jump: jump_if_false <addr>
	jump_if_true  // conditional jump: jump_if_true <addr>
	call          // call function: call <arity>
	ret           // return top of stack

	// Data structures
	make_array  // pop N items, push array: make_array <len>
	make_range  // pop end, pop start, push array [start..end)
	index       // pop index, pop array, push element
	make_struct // create struct: pop type_name, pop N (field_name, value) pairs: make_struct <field_count>
	get_field   // get struct field: get_field <field_name_idx> (field name in constant pool)

	// Enums
	make_enum         // create enum: pop variant_name, pop enum_name, push EnumValue (no payload)
	make_enum_payload // create enum with payload: pop payload, pop variant_name, pop enum_name, push EnumValue
	match_enum        // match variant only (ignore payload): pop variant_name, pop enum_name, pop value, push bool
	unwrap_enum       // get payload from enum: pop enum, push payload (or none)

	// Closures
	make_closure // create closure: pop N captures (N = func.capture_count), push ClosureValue; operand = func_idx
	push_capture // push captured variable from current closure: push_capture <idx>

	// Error handling
	make_error       // pop value, push ErrorValue
	is_error         // pop value, push bool (true if ErrorValue)
	is_error_or_none // pop value, push bool (true if ErrorValue or NoneValue)
	unwrap_error     // pop ErrorValue, push payload value

	// String operations
	to_string  // pop value, push string representation
	str_concat // pop two strings, push concatenated result

	// Misc
	print // print top of stack (temporary, for debugging)
	halt  // stop execution
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

pub struct NoneValue {}

pub struct ErrorValue {
pub:
	payload Value
}

pub struct EnumValue {
pub:
	enum_name    string // e.g., "MyEnum"
	variant_name string // e.g., "C"
	payload      ?Value // optional payload
}

pub struct StructValue {
pub mut:
	type_name string
	fields    map[string]Value
}

pub struct ClosureValue {
pub:
	func_idx int
	captures []Value
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
