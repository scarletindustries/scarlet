module vm

import compiler.bytecode
import net
import os

struct CallFrame {
mut:
	func      bytecode.Function
	func_idx  int
	ip        int
	base_slot int
	captures  []bytecode.Value
}

pub struct VMOptions {
pub:
	io_enabled bool
}

pub struct VM {
	options VMOptions
mut:
	program         bytecode.Program
	stack           []bytecode.Value
	frames          []CallFrame
	next_socket_id  int
	tcp_listeners   map[int]&net.TcpListener
	tcp_connections map[int]&net.TcpConn
}

pub fn new_vm(program bytecode.Program, options VMOptions) VM {
	return VM{
		options:         options
		program:         program
		stack:           []
		frames:          []
		next_socket_id:  1
		tcp_listeners:   map[int]&net.TcpListener{}
		tcp_connections: map[int]&net.TcpConn{}
	}
}

pub fn (mut vm VM) run() !bytecode.Value {
	main_func := vm.program.functions[vm.program.entry]

	vm.frames << CallFrame{
		func:      main_func
		func_idx:  vm.program.entry
		ip:        0
		base_slot: 0
		captures:  []
	}

	for _ in 0 .. main_func.locals {
		vm.stack << bytecode.NoneValue{}
	}

	return vm.execute()!
}

fn (mut vm VM) execute() !bytecode.Value {
	for vm.frames.len > 0 {
		mut frame := &vm.frames[vm.frames.len - 1]

		addr := frame.func.code_start + frame.ip
		if addr >= vm.program.code.len {
			break
		}

		instr := vm.program.code[addr]
		frame.ip += 1

		match instr.op {
			.push_const {
				vm.stack << vm.program.constants[instr.operand]
			}
			.push_local {
				slot := frame.base_slot + instr.operand
				vm.stack << vm.stack[slot]
			}
			.store_local {
				slot := frame.base_slot + instr.operand
				vm.stack[slot] = vm.pop()!
			}
			.push_none {
				vm.stack << bytecode.NoneValue{}
			}
			.push_true {
				vm.stack << true
			}
			.push_false {
				vm.stack << false
			}
			.pop {
				vm.pop()!
			}
			.dup {
				vm.stack << vm.peek()!
			}
			.add {
				b := vm.pop()!
				a := vm.pop()!
				vm.stack << vm.binary_op(a, b, .add)!
			}
			.sub {
				b := vm.pop()!
				a := vm.pop()!
				vm.stack << vm.binary_op(a, b, .sub)!
			}
			.mul {
				b := vm.pop()!
				a := vm.pop()!
				vm.stack << vm.binary_op(a, b, .mul)!
			}
			.div {
				b := vm.pop()!
				a := vm.pop()!
				vm.stack << vm.binary_op(a, b, .div)!
			}
			.mod {
				b := vm.pop()!
				a := vm.pop()!
				vm.stack << vm.binary_op(a, b, .mod)!
			}
			.neg {
				a := vm.pop()!
				match a {
					int {
						neg := -a
						vm.stack << neg
					}
					f64 {
						neg := -a
						vm.stack << neg
					}
					else {
						return error('Cannot negate non-number')
					}
				}
			}
			.eq {
				b := vm.pop()!
				a := vm.pop()!
				vm.stack << vm.values_equal(a, b)
			}
			.neq {
				b := vm.pop()!
				a := vm.pop()!
				vm.stack << !vm.values_equal(a, b)
			}
			.lt {
				b := vm.pop()!
				a := vm.pop()!
				vm.stack << vm.compare(a, b, .lt)!
			}
			.gt {
				b := vm.pop()!
				a := vm.pop()!
				vm.stack << vm.compare(a, b, .gt)!
			}
			.lte {
				b := vm.pop()!
				a := vm.pop()!
				vm.stack << vm.compare(a, b, .lte)!
			}
			.gte {
				b := vm.pop()!
				a := vm.pop()!
				vm.stack << vm.compare(a, b, .gte)!
			}
			.not {
				a := vm.pop()!
				vm.stack << !vm.is_truthy(a)
			}
			.jump {
				vm.frames[vm.frames.len - 1].ip = instr.operand - frame.func.code_start
			}
			.jump_if_false {
				cond := vm.pop()!
				if !vm.is_truthy(cond) {
					vm.frames[vm.frames.len - 1].ip = instr.operand - frame.func.code_start
				}
			}
			.jump_if_true {
				cond := vm.pop()!
				if vm.is_truthy(cond) {
					vm.frames[vm.frames.len - 1].ip = instr.operand - frame.func.code_start
				}
			}
			.call {
				arity := instr.operand
				callee := vm.pop()!

				if callee is bytecode.ClosureValue {
					func := vm.program.functions[callee.func_idx]

					if arity != func.arity {
						return error('Expected ${func.arity} arguments, got ${arity}')
					}

					new_base := vm.stack.len - arity

					for _ in arity .. func.locals {
						vm.stack << bytecode.NoneValue{}
					}

					vm.frames << CallFrame{
						func:      func
						func_idx:  callee.func_idx
						ip:        0
						base_slot: new_base
						captures:  callee.captures
					}
				} else {
					return error('Cannot call non-function')
				}
			}
			.tail_call {
				arity := instr.operand
				callee := vm.pop()!

				if callee is bytecode.ClosureValue {
					func := vm.program.functions[callee.func_idx]

					if arity != func.arity {
						return error('Expected ${func.arity} arguments, got ${arity}')
					}

					// Collect arguments from stack
					mut args := []bytecode.Value{cap: arity}
					for _ in 0 .. arity {
						args << vm.pop()!
					}

					// Get current frame
					mut current_frame := &vm.frames[vm.frames.len - 1]
					base := current_frame.base_slot

					// Clear current frame's stack slots
					for vm.stack.len > base {
						vm.stack.pop()
					}

					// Push arguments back (in reverse since we popped them)
					for i := arity - 1; i >= 0; i-- {
						vm.stack << args[i]
					}

					// Fill remaining locals with none
					for _ in arity .. func.locals {
						vm.stack << bytecode.NoneValue{}
					}

					// Reuse the frame with new function
					current_frame.func = func
					current_frame.func_idx = callee.func_idx
					current_frame.ip = 0
					current_frame.captures = callee.captures
				} else {
					return error('Cannot tail_call non-function')
				}
			}
			.ret {
				ret_val := vm.pop()!
				old_frame := vm.frames.pop()

				for vm.stack.len > old_frame.base_slot {
					vm.stack.pop()
				}

				vm.stack << ret_val

				if vm.frames.len == 0 {
					break
				}
			}
			.make_array {
				len := instr.operand
				// unsafe: we immediately write to every index, no uninitialized reads
				mut arr := unsafe { []bytecode.Value{len: len} }
				for i := len - 1; i >= 0; i-- {
					arr[i] = vm.pop()!
				}
				vm.stack << bytecode.Value(arr)
			}
			.make_range {
				end_val := vm.pop()!
				start_val := vm.pop()!

				if start_val is int && end_val is int {
					mut arr := []bytecode.Value{}
					for i in start_val .. end_val {
						arr << bytecode.Value(i)
					}
					vm.stack << bytecode.Value(arr)
				} else {
					return error('Range bounds must be integers')
				}
			}
			.index {
				idx_val := vm.pop()!
				arr_val := vm.pop()!

				if arr_val is []bytecode.Value {
					if idx_val is int {
						if idx_val >= 0 && idx_val < arr_val.len {
							vm.stack << arr_val[idx_val]
						} else {
							return error('Index out of bounds: ${idx_val}')
						}
					} else {
						return error('Array index must be integer')
					}
				} else {
					return error('Cannot index non-array')
				}
			}
			.make_struct {
				field_count := instr.operand

				type_name_val := vm.pop()!
				type_name := if type_name_val is string {
					type_name_val
				} else {
					return error('Struct type name must be string')
				}

				mut fields := map[string]bytecode.Value{}
				for _ in 0 .. field_count {
					val := vm.pop()!
					name_val := vm.pop()!
					name := if name_val is string {
						name_val
					} else {
						return error('Field name must be string')
					}
					fields[name] = val
				}
				vm.stack << bytecode.StructValue{
					type_name: type_name
					fields:    fields
				}
			}
			.get_field {
				field_name_idx := instr.operand
				field_name := vm.program.constants[field_name_idx]
				if field_name !is string {
					return error('Field name must be string')
				}
				struct_val := vm.pop()!
				if struct_val is bytecode.StructValue {
					if val := struct_val.fields[field_name as string] {
						vm.stack << val
					} else {
						return error('Unknown field: ${field_name}')
					}
				} else {
					return error('Cannot access field on non-struct')
				}
			}
			.make_closure {
				func_idx := instr.operand
				func := vm.program.functions[func_idx]

				// unsafe: we immediately write to every index, no uninitialized reads
				mut captures := unsafe { []bytecode.Value{len: func.capture_count} }
				for i := func.capture_count - 1; i >= 0; i-- {
					captures[i] = vm.pop()!
				}

				vm.stack << bytecode.ClosureValue{
					func_idx: func_idx
					captures: captures
				}
			}
			.push_capture {
				capture_idx := instr.operand
				if vm.frames.len > 0 {
					current_frame := vm.frames[vm.frames.len - 1]
					if capture_idx < current_frame.captures.len {
						vm.stack << current_frame.captures[capture_idx]
					} else {
						return error('Capture index out of bounds: ${capture_idx}')
					}
				}
			}
			.push_self {
				// Push the currently-executing closure onto the stack
				if vm.frames.len > 0 {
					current_frame := vm.frames[vm.frames.len - 1]
					vm.stack << bytecode.ClosureValue{
						func_idx: current_frame.func_idx
						captures: current_frame.captures
					}
				}
			}
			.print {
				val := vm.pop()!
				println(inspect(val))
			}
			.stack_depth {
				vm.stack << vm.frames.len
			}
			.make_enum {
				variant_name_val := vm.pop()!
				enum_name_val := vm.pop()!

				enum_name := if enum_name_val is string {
					enum_name_val
				} else {
					return error('Enum name must be string')
				}

				variant_name := if variant_name_val is string {
					variant_name_val
				} else {
					return error('Variant name must be string')
				}

				vm.stack << bytecode.EnumValue{
					enum_name:    enum_name
					variant_name: variant_name
					payload:      none
				}
			}
			.make_enum_payload {
				payload := vm.pop()!
				variant_name_val := vm.pop()!
				enum_name_val := vm.pop()!

				enum_name := if enum_name_val is string {
					enum_name_val
				} else {
					return error('Enum name must be string')
				}
				variant_name := if variant_name_val is string {
					variant_name_val
				} else {
					return error('Variant name must be string')
				}

				vm.stack << bytecode.EnumValue{
					enum_name:    enum_name
					variant_name: variant_name
					payload:      payload
				}
			}
			.match_enum {
				// Match variant only, ignore payload
				variant_name := vm.pop()!
				enum_name := vm.pop()!
				val := vm.pop()!

				if variant_name !is string || enum_name !is string {
					return error('Enum/variant names must be strings')
				}

				if val is bytecode.EnumValue {
					vm.stack << (val.enum_name == (enum_name as string)
						&& val.variant_name == (variant_name as string))
				} else {
					vm.stack << false
				}
			}
			.unwrap_enum {
				enum_val := vm.pop()!
				if enum_val is bytecode.EnumValue {
					if p := enum_val.payload {
						vm.stack << p
					} else {
						vm.stack << bytecode.NoneValue{}
					}
				} else {
					return error('Cannot unwrap non-enum value')
				}
			}
			.make_error {
				payload := vm.pop()!
				vm.stack << bytecode.ErrorValue{
					payload: payload
				}
			}
			.is_error {
				val := vm.pop()!
				vm.stack << (val is bytecode.ErrorValue)
			}
			.is_error_or_none {
				val := vm.pop()!
				vm.stack << (val is bytecode.ErrorValue || val is bytecode.NoneValue)
			}
			.unwrap_error {
				val := vm.pop()!
				if val is bytecode.ErrorValue {
					vm.stack << val.payload
				} else {
					return error('Expected error value')
				}
			}
			.to_string {
				val := vm.pop()!
				vm.stack << inspect(val)
			}
			.str_concat {
				b := vm.pop()!
				a := vm.pop()!
				if a is string && b is string {
					vm.stack << (a + b)
				} else {
					return error('str_concat requires two strings')
				}
			}
			.halt {
				break
			}
			.file_read {
				if !vm.options.io_enabled {
					return error('I/O operations require --experimental-shitty-io flag')
				}
				path_val := vm.pop()!
				if path_val is string {
					content := os.read_file(path_val) or {
						vm.stack << bytecode.ErrorValue{payload: 'Failed to read file: ${err}'}
						continue
					}
					vm.stack << content
				} else {
					return error('read_file requires string path')
				}
			}
			.file_write {
				if !vm.options.io_enabled {
					return error('I/O operations require --experimental-shitty-io flag')
				}
				content := vm.pop()!
				path_val := vm.pop()!
				if path_val is string && content is string {
					os.write_file(path_val, content) or {
						vm.stack << bytecode.ErrorValue{payload: 'Failed to write file: ${err}'}
						continue
					}
					vm.stack << bytecode.NoneValue{}
				} else {
					return error('write_file requires string path and content')
				}
			}
			.tcp_listen {
				if !vm.options.io_enabled {
					return error('I/O operations require --experimental-shitty-io flag')
				}
				port_val := vm.pop()!
				if port_val is int {
					listener := net.listen_tcp(.ip, '0.0.0.0:${port_val}') or {
						vm.stack << bytecode.ErrorValue{payload: 'Failed to listen: ${err}'}
						continue
					}
					socket_id := vm.next_socket_id
					vm.next_socket_id += 1
					vm.tcp_listeners[socket_id] = listener
					vm.stack << bytecode.SocketValue{
						id:          socket_id
						is_listener: true
					}
				} else {
					return error('tcp_listen requires int port')
				}
			}
			.tcp_accept {
				if !vm.options.io_enabled {
					return error('I/O operations require --experimental-shitty-io flag')
				}
				socket_val := vm.pop()!
				if socket_val is bytecode.SocketValue {
					if !socket_val.is_listener {
						return error('tcp_accept requires a listener socket')
					}
					if mut listener := vm.tcp_listeners[socket_val.id] {
						conn := listener.accept() or {
							vm.stack << bytecode.ErrorValue{payload: 'Failed to accept: ${err}'}
							continue
						}
						conn_id := vm.next_socket_id
						vm.next_socket_id += 1
						vm.tcp_connections[conn_id] = conn
						vm.stack << bytecode.SocketValue{
							id:          conn_id
							is_listener: false
						}
					} else {
						return error('Invalid listener socket')
					}
				} else {
					return error('tcp_accept requires socket')
				}
			}
			.tcp_read {
				if !vm.options.io_enabled {
					return error('I/O operations require --experimental-shitty-io flag')
				}
				socket_val := vm.pop()!
				if socket_val is bytecode.SocketValue {
					if socket_val.is_listener {
						return error('tcp_read requires a connection socket, not a listener')
					}
					if mut conn := vm.tcp_connections[socket_val.id] {
						mut buf := []u8{len: 4096}
						bytes_read := conn.read(mut buf) or {
							vm.stack << bytecode.ErrorValue{payload: 'Failed to read: ${err}'}
							continue
						}
						if bytes_read == 0 {
							vm.stack << bytecode.NoneValue{}
						} else {
							vm.stack << buf[..bytes_read].bytestr()
						}
					} else {
						return error('Invalid connection socket')
					}
				} else {
					return error('tcp_read requires socket')
				}
			}
			.tcp_write {
				if !vm.options.io_enabled {
					return error('I/O operations require --experimental-shitty-io flag')
				}
				data := vm.pop()!
				socket_val := vm.pop()!
				if socket_val is bytecode.SocketValue && data is string {
					if socket_val.is_listener {
						return error('tcp_write requires a connection socket, not a listener')
					}
					if mut conn := vm.tcp_connections[socket_val.id] {
						bytes_written := conn.write(data.bytes()) or {
							vm.stack << bytecode.ErrorValue{payload: 'Failed to write: ${err}'}
							continue
						}
						vm.stack << bytes_written
					} else {
						return error('Invalid connection socket')
					}
				} else {
					return error('tcp_write requires socket and string data')
				}
			}
			.tcp_close {
				if !vm.options.io_enabled {
					return error('I/O operations require --experimental-shitty-io flag')
				}
				socket_val := vm.pop()!
				if socket_val is bytecode.SocketValue {
					if socket_val.is_listener {
						if mut listener := vm.tcp_listeners[socket_val.id] {
							listener.close() or {}
							vm.tcp_listeners.delete(socket_val.id)
						}
					} else {
						if mut conn := vm.tcp_connections[socket_val.id] {
							conn.close() or {}
							vm.tcp_connections.delete(socket_val.id)
						}
					}
					vm.stack << bytecode.NoneValue{}
				} else {
					return error('tcp_close requires socket')
				}
			}
		}
	}

	if vm.stack.len > 0 {
		return vm.stack[vm.stack.len - 1]
	}
	return bytecode.NoneValue{}
}

fn (mut vm VM) pop() !bytecode.Value {
	if vm.stack.len == 0 {
		return error('Stack underflow')
	}
	return vm.stack.pop()
}

fn (vm VM) peek() !bytecode.Value {
	if vm.stack.len == 0 {
		return error('Stack underflow')
	}
	return vm.stack[vm.stack.len - 1]
}

fn (vm VM) binary_op(a bytecode.Value, b bytecode.Value, op bytecode.Op) !bytecode.Value {
	if a is int && b is int {
		return match op {
			.add { a + b }
			.sub { a - b }
			.mul { a * b }
			.div { a / b }
			.mod { a % b }
			else { error('Unknown binary op') }
		}
	}

	if a is f64 && b is f64 {
		return match op {
			.add { a + b }
			.sub { a - b }
			.mul { a * b }
			.div { a / b }
			else { error('Unknown binary op for floats') }
		}
	}

	if a is int && b is f64 {
		af := f64(a)
		return match op {
			.add { af + b }
			.sub { af - b }
			.mul { af * b }
			.div { af / b }
			else { error('Unknown binary op') }
		}
	}

	if a is f64 && b is int {
		bf := f64(b)
		return match op {
			.add { a + bf }
			.sub { a - bf }
			.mul { a * bf }
			.div { a / bf }
			else { error('Unknown binary op') }
		}
	}

	if a is string && b is string && op == .add {
		return a + b
	}

	return error('Cannot perform arithmetic on these types')
}

fn (vm VM) values_equal(a bytecode.Value, b bytecode.Value) bool {
	match a {
		int {
			if b is int {
				return a == b
			}
		}
		f64 {
			if b is f64 {
				return a == b
			}
		}
		bool {
			if b is bool {
				return a == b
			}
		}
		string {
			if b is string {
				return a == b
			}
		}
		bytecode.NoneValue {
			if b is bytecode.NoneValue {
				return true
			}
		}
		bytecode.EnumValue {
			if b is bytecode.EnumValue {
				if a.enum_name != b.enum_name || a.variant_name != b.variant_name {
					return false
				}
				// Compare payloads
				a_payload := a.payload
				b_payload := b.payload
				if a_payload == none && b_payload == none {
					return true
				}
				if a_payload == none || b_payload == none {
					return false
				}
				return vm.values_equal(a_payload or { return false }, b_payload or { return false })
			}
		}
		else {}
	}
	return false
}

fn (vm VM) compare(a bytecode.Value, b bytecode.Value, op bytecode.Op) !bool {
	if a is int && b is int {
		return match op {
			.lt { a < b }
			.gt { a > b }
			.lte { a <= b }
			.gte { a >= b }
			else { false }
		}
	}
	if a is f64 && b is f64 {
		return match op {
			.lt { a < b }
			.gt { a > b }
			.lte { a <= b }
			.gte { a >= b }
			else { false }
		}
	}
	return error('Cannot compare these types')
}

fn (vm VM) is_truthy(v bytecode.Value) bool {
	match v {
		bool { return v }
		bytecode.NoneValue { return false }
		int { return v != 0 }
		string { return v.len > 0 }
		else { return true }
	}
}

pub fn inspect(v bytecode.Value) string {
	match v {
		int {
			return v.str()
		}
		f64 {
			return v.str()
		}
		bool {
			return if v { 'true' } else { 'false' }
		}
		string {
			return v
		}
		bytecode.NoneValue {
			return 'none'
		}
		[]bytecode.Value {
			mut s := '['
			for i, elem in v {
				if i > 0 {
					s += ', '
				}
				s += inspect(elem)
			}
			s += ']'
			return s
		}
		bytecode.StructValue {
			mut s := '${v.type_name}{ '
			mut first := true
			for name, val in v.fields {
				if !first {
					s += ', '
				}
				s += '${name}: ${inspect(val)}'
				first = false
			}
			s += ' }'
			return s
		}
		bytecode.ClosureValue {
			return '<fn#${v.func_idx}>'
		}
		bytecode.EnumValue {
			if p := v.payload {
				return '${v.enum_name}.${v.variant_name}(${inspect(p)})'
			} else {
				return '${v.enum_name}.${v.variant_name}'
			}
		}
		bytecode.ErrorValue {
			return 'error(${inspect(v.payload)})'
		}
		bytecode.SocketValue {
			if v.is_listener {
				return '<listener#${v.id}>'
			}
			return '<socket#${v.id}>'
		}
	}
}
