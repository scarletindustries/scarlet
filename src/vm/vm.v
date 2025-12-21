module vm

import bytecode
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

@[params]
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
			.swap {
				if vm.stack.len < 2 {
					return error('Stack underflow on swap. This is likely a compiler bug.')
				}
				top := vm.stack.len - 1
				vm.stack[top], vm.stack[top - 1] = vm.stack[top - 1], vm.stack[top]
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
						return error("Cannot negate non-numeric value '${value_type_name(a)}'")
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
					return error("Range bounds must be integers, got '${value_type_name(start_val)}' and '${value_type_name(end_val)}'")
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
							// Out of bounds returns none (array indexing is optional)
							vm.stack << bytecode.Value(bytecode.NoneValue{})
						}
					} else {
						// Invalid index type returns none (type checker should catch this)
						vm.stack << bytecode.Value(bytecode.NoneValue{})
					}
				} else {
					// Non-array returns none (type checker should catch this)
					vm.stack << bytecode.Value(bytecode.NoneValue{})
				}
			}
			.array_len {
				arr_val := vm.pop()!
				if arr_val is []bytecode.Value {
					vm.stack << arr_val.len
				} else {
					return error("Cannot get length of non-array type '${value_type_name(arr_val)}'")
				}
			}
			.array_slice {
				end_val := vm.pop()!
				start_val := vm.pop()!
				arr_val := vm.pop()!

				if arr_val is []bytecode.Value {
					if start_val is int && end_val is int {
						if start_val >= 0 && end_val <= arr_val.len && start_val <= end_val {
							sliced := arr_val[start_val..end_val]
							vm.stack << bytecode.Value(sliced)
						} else {
							return error('Slice indices out of bounds: [${start_val}..${end_val}] (array length is ${arr_val.len})')
						}
					} else {
						return error('Slice indices must be integers')
					}
				} else {
					return error("Cannot slice non-array type '${value_type_name(arr_val)}'")
				}
			}
			.array_concat {
				arr2_val := vm.pop()!
				arr1_val := vm.pop()!

				if arr1_val is []bytecode.Value && arr2_val is []bytecode.Value {
					mut result := arr1_val.clone()
					result << arr2_val
					vm.stack << bytecode.Value(result)
				} else {
					return error('Cannot concatenate non-array types')
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

				type_id_val := vm.pop()!
				type_id := if type_id_val is int {
					type_id_val
				} else {
					return error('Struct type id must be int')
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
					type_id:   type_id
					type_name: type_name
					fields:    fields
					hash:      bytecode.compute_struct_hash(type_name, fields)
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
					name:     func.name
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
						name:     current_frame.func.name
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
				type_id_val := vm.pop()!

				type_id := if type_id_val is int {
					type_id_val
				} else {
					return error('Enum type id must be int')
				}

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
					type_id:      type_id
					enum_name:    enum_name
					variant_name: variant_name
					payload:      []
					hash:         bytecode.compute_enum_hash(enum_name, variant_name,
						[])
				}
			}
			.make_enum_payload {
				payload_count := instr.operand

				mut payloads := []bytecode.Value{}
				for _ in 0 .. payload_count {
					payloads << vm.pop()!
				}

				// Reverse since we popped in reverse order
				payloads.reverse_in_place()

				variant_name_val := vm.pop()!
				enum_name_val := vm.pop()!
				type_id_val := vm.pop()!

				type_id := if type_id_val is int {
					type_id_val
				} else {
					return error('Enum type id must be int')
				}

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
					type_id:      type_id
					enum_name:    enum_name
					variant_name: variant_name
					payload:      payloads
					hash:         bytecode.compute_enum_hash(enum_name, variant_name,
						payloads)
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
					if enum_val.payload.len > 0 {
						for p in enum_val.payload {
							vm.stack << p
						}
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
			.is_none {
				val := vm.pop()!
				vm.stack << (val is bytecode.NoneValue)
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
						vm.stack << bytecode.ErrorValue{
							payload: 'Failed to read file: ${err}'
						}
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
						vm.stack << bytecode.ErrorValue{
							payload: 'Failed to write file: ${err}'
						}
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
						vm.stack << bytecode.ErrorValue{
							payload: 'Failed to listen: ${err}'
						}
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
							vm.stack << bytecode.ErrorValue{
								payload: 'Failed to accept: ${err}'
							}
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
							vm.stack << bytecode.ErrorValue{
								payload: 'Failed to read: ${err}'
							}
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
							vm.stack << bytecode.ErrorValue{
								payload: 'Failed to write: ${err}'
							}
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
		return error('Stack underflow. This is likely a compiler bug.')
	}
	return vm.stack.pop()
}

fn (vm VM) peek() !bytecode.Value {
	if vm.stack.len == 0 {
		return error('Stack underflow. This is likely a compiler bug.')
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
			else { return error('Unknown binary op. This is likely a compiler bug.') }
		}
	}

	if a is f64 && b is f64 {
		return match op {
			.add { a + b }
			.sub { a - b }
			.mul { a * b }
			.div { a / b }
			else { return error('Unknown binary op. This is likely a compiler bug.') }
		}
	}

	if a is int && b is f64 {
		af := f64(a)
		return match op {
			.add { af + b }
			.sub { af - b }
			.mul { af * b }
			.div { af / b }
			else { return error('Unknown binary op. This is likely a compiler bug.') }
		}
	}

	if a is f64 && b is int {
		bf := f64(b)
		return match op {
			.add { a + bf }
			.sub { a - bf }
			.mul { a * bf }
			.div { a / bf }
			else { return error('Unknown binary op. This is likely a compiler bug.') }
		}
	}

	if a is string && b is string && op == .add {
		return a + b
	}

	return error("Cannot perform arithmetic on '${value_type_name(a)}' and '${value_type_name(b)}'. This is likely a compiler bug.")
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
				// fast path: different hashes means definitely not equal
				if a.hash != b.hash {
					return false
				}
				// nominal check: must be same enum type
				if a.type_id != b.type_id {
					return false
				}
				if a.variant_name != b.variant_name {
					return false
				}
				// fast path: both empty payloads
				if a.payload.len == 0 && b.payload.len == 0 {
					return true
				}
				if a.payload.len != b.payload.len {
					return false
				}
				for i, a_val in a.payload {
					if !vm.values_equal(a_val, b.payload[i]) {
						return false
					}
				}
				return true
			}
		}
		bytecode.StructValue {
			if b is bytecode.StructValue {
				// fast path: different hashes means definitely not equal
				if a.hash != b.hash {
					return false
				}
				// nominal check: must be same struct type
				if a.type_id != b.type_id {
					return false
				}
				// fast path: both empty structs
				if a.fields.len == 0 && b.fields.len == 0 {
					return true
				}
				// structural check: compare all fields (handles hash collisions)
				if a.fields.len != b.fields.len {
					return false
				}
				for key, a_val in a.fields {
					if b_val := b.fields[key] {
						if !vm.values_equal(a_val, b_val) {
							return false
						}
					} else {
						return false
					}
				}
				return true
			}
		}
		[]bytecode.Value {
			if b is []bytecode.Value {
				// fast path: different lengths means definitely not equal
				if a.len != b.len {
					return false
				}
				// fast path: both empty arrays
				if a.len == 0 {
					return true
				}
				for i, a_val in a {
					if !vm.values_equal(a_val, b[i]) {
						return false
					}
				}
				return true
			}
		}
		bytecode.ClosureValue {
			false
		}
		bytecode.ErrorValue {
			if b is bytecode.ErrorValue {
				return vm.values_equal(a.payload, b.payload)
			}
		}
		bytecode.SocketValue {
			if b is bytecode.SocketValue {
				return a.id == b.id && a.is_listener == b.is_listener
			}
		}
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
			else { return error('Unknown compare op. This is likely a compiler bug.') }
		}
	}

	if a is f64 && b is f64 {
		return match op {
			.lt { a < b }
			.gt { a > b }
			.lte { a <= b }
			.gte { a >= b }
			else { return error('Unknown compare op. This is likely a compiler bug.') }
		}
	}

	return error("Cannot compare '${value_type_name(a)}' with '${value_type_name(b)}'. This is likely a compiler bug.")
}

fn (vm VM) is_truthy(v bytecode.Value) bool {
	return match v {
		bool {
			v
		}
		bytecode.NoneValue, int, f64, string, []bytecode.Value, bytecode.StructValue,
		bytecode.ClosureValue, bytecode.EnumValue, bytecode.ErrorValue, bytecode.SocketValue {
			false
		}
	}
}

fn value_type_name(v bytecode.Value) string {
	return match v {
		int { 'Int' }
		f64 { 'Float' }
		bool { 'Bool' }
		string { 'String' }
		bytecode.NoneValue { 'None' }
		[]bytecode.Value { 'Array' }
		bytecode.StructValue { v.type_name }
		bytecode.ClosureValue { 'Function' }
		bytecode.EnumValue { v.enum_name }
		bytecode.ErrorValue { 'Error' }
		bytecode.SocketValue { 'Socket' }
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
			return '<fn#${v.name}>'
		}
		bytecode.EnumValue {
			if v.payload.len > 0 {
				parts := v.payload.map(inspect(it))
				return '${v.enum_name}.${v.variant_name}(${parts.join(', ')})'
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
