module vm

import compiler.bytecode
import sync

struct CallFrame {
mut:
	func      bytecode.Function
	ip        int
	base_slot int
	captures  []bytecode.Value
}

struct Process {
mut:
	pid     int
	stack   []bytecode.Value
	frames  []CallFrame
	mailbox []bytecode.Value
	status  ProcessStatus
}

enum ProcessStatus {
	running
	waiting
	completed
}

pub struct VM {
mut:
	program       bytecode.Program
	processes     map[int]&Process
	current_pid   int
	next_pid      int
	runnable      []int
	globals       map[string]bytecode.Value
	mailbox_mutex &sync.Mutex = sync.new_mutex()
}

pub fn new_vm(program bytecode.Program) VM {
	return VM{
		program:     program
		processes:   {}
		current_pid: 0
		next_pid:    1
		runnable:    []
		globals:     {}
	}
}

pub fn (mut vm VM) run() !bytecode.Value {
	main_func := vm.program.functions[vm.program.entry]

	mut main_process := &Process{
		pid:     0
		stack:   []
		frames:  []
		mailbox: []
		status:  .running
	}

	main_process.frames << CallFrame{
		func:      main_func
		ip:        0
		base_slot: 0
		captures:  []
	}

	for _ in 0 .. main_func.locals {
		main_process.stack << bytecode.NoneValue{}
	}

	vm.processes[0] = main_process
	vm.runnable << 0

	return vm.execute()!
}

fn (mut vm VM) current_process() ?&Process {
	return vm.processes[vm.current_pid] or { return none }
}

fn (mut vm VM) execute() !bytecode.Value {
	for {
		if vm.runnable.len == 0 {
			if proc := vm.processes[0] {
				if proc.stack.len > 0 {
					return proc.stack[proc.stack.len - 1]
				}
			}
			return bytecode.NoneValue{}
		}

		vm.current_pid = vm.runnable[0]
		vm.runnable = vm.runnable[1..]

		completed := vm.execute_one()!

		if completed {
			if vm.current_pid == 0 {
				if proc := vm.processes[0] {
					if proc.stack.len > 0 {
						return proc.stack[proc.stack.len - 1]
					}
				}
				return bytecode.NoneValue{}
			} else {
				vm.processes.delete(vm.current_pid)
			}
		} else {
			if proc := vm.processes[vm.current_pid] {
				if proc.status == .running {
					vm.runnable << vm.current_pid
				}
			}
		}
	}

	return bytecode.NoneValue{}
}

fn (mut vm VM) execute_one() !bool {
	mut proc := vm.processes[vm.current_pid] or { return true }

	if proc.frames.len == 0 {
		return true
	}

	mut frame := &proc.frames[proc.frames.len - 1]

	addr := frame.func.code_start + frame.ip
	if addr >= vm.program.code.len {
		return true
	}

	instr := vm.program.code[addr]
	frame.ip += 1

	match instr.op {
		.push_const {
			vm.push_current(vm.program.constants[instr.operand])
		}
		.push_local {
			slot := frame.base_slot + instr.operand
			vm.push_current(proc.stack[slot])
		}
		.store_local {
			slot := frame.base_slot + instr.operand
			val := vm.pop_current()!
			if mut p := vm.processes[vm.current_pid] {
				p.stack[slot] = val
			}
		}
		.push_none {
			vm.push_current(bytecode.NoneValue{})
		}
		.push_true {
			vm.push_current(true)
		}
		.push_false {
			vm.push_current(false)
		}
		.pop {
			vm.pop_current()!
		}
		.dup {
			val := vm.peek_current()!
			vm.push_current(val)
		}
		.add {
			b := vm.pop_current()!
			a := vm.pop_current()!
			vm.push_current(vm.binary_op(a, b, .add)!)
		}
		.sub {
			b := vm.pop_current()!
			a := vm.pop_current()!
			vm.push_current(vm.binary_op(a, b, .sub)!)
		}
		.mul {
			b := vm.pop_current()!
			a := vm.pop_current()!
			vm.push_current(vm.binary_op(a, b, .mul)!)
		}
		.div {
			b := vm.pop_current()!
			a := vm.pop_current()!
			vm.push_current(vm.binary_op(a, b, .div)!)
		}
		.mod {
			b := vm.pop_current()!
			a := vm.pop_current()!
			vm.push_current(vm.binary_op(a, b, .mod)!)
		}
		.neg {
			a := vm.pop_current()!
			match a {
				int {
					neg := -a
					vm.push_current(neg)
				}
				f64 {
					neg := -a
					vm.push_current(neg)
				}
				else {
					return error('Cannot negate non-number')
				}
			}
		}
		.eq {
			b := vm.pop_current()!
			a := vm.pop_current()!
			vm.push_current(vm.values_equal(a, b))
		}
		.neq {
			b := vm.pop_current()!
			a := vm.pop_current()!
			vm.push_current(!vm.values_equal(a, b))
		}
		.lt {
			b := vm.pop_current()!
			a := vm.pop_current()!
			vm.push_current(vm.compare(a, b, .lt)!)
		}
		.gt {
			b := vm.pop_current()!
			a := vm.pop_current()!
			vm.push_current(vm.compare(a, b, .gt)!)
		}
		.lte {
			b := vm.pop_current()!
			a := vm.pop_current()!
			vm.push_current(vm.compare(a, b, .lte)!)
		}
		.gte {
			b := vm.pop_current()!
			a := vm.pop_current()!
			vm.push_current(vm.compare(a, b, .gte)!)
		}
		.not {
			a := vm.pop_current()!
			vm.push_current(!vm.is_truthy(a))
		}
		.and {
			b := vm.pop_current()!
			a := vm.pop_current()!
			vm.push_current(vm.is_truthy(a) && vm.is_truthy(b))
		}
		.or {
			b := vm.pop_current()!
			a := vm.pop_current()!
			vm.push_current(vm.is_truthy(a) || vm.is_truthy(b))
		}
		.jump {
			if mut p := vm.processes[vm.current_pid] {
				p.frames[proc.frames.len - 1].ip = instr.operand - frame.func.code_start
			}
		}
		.jump_if_false {
			cond := vm.pop_current()!
			if !vm.is_truthy(cond) {
				if mut p := vm.processes[vm.current_pid] {
					p.frames[proc.frames.len - 1].ip = instr.operand - frame.func.code_start
				}
			}
		}
		.jump_if_true {
			cond := vm.pop_current()!
			if vm.is_truthy(cond) {
				if mut p := vm.processes[vm.current_pid] {
					p.frames[proc.frames.len - 1].ip = instr.operand - frame.func.code_start
				}
			}
		}
		.call {
			arity := instr.operand
			callee := vm.pop_current()!

			if callee is bytecode.ClosureValue {
				func := vm.program.functions[callee.func_idx]

				if arity != func.arity {
					return error('Expected ${func.arity} arguments, got ${arity}')
				}

				if mut p := vm.processes[vm.current_pid] {
					new_base := p.stack.len - arity

					for _ in arity .. func.locals {
						vm.push_current(bytecode.NoneValue{})
					}

					p.frames << CallFrame{
						func:      func
						ip:        0
						base_slot: new_base
						captures:  callee.captures
					}
				}
			} else {
				return error('Cannot call non-function')
			}
		}
		.ret {
			ret_val := vm.pop_current()!

			if mut p := vm.processes[vm.current_pid] {
				old_frame := p.frames.pop()

				for p.stack.len > old_frame.base_slot {
					p.stack.pop()
				}

				vm.push_current(ret_val)

				if p.frames.len == 0 {
					return true
				}
			}
		}
		.make_array {
			len := instr.operand
			mut arr := []bytecode.Value{cap: len}
			for _ in 0 .. len {
				arr.prepend(vm.pop_current()!)
			}
			vm.push_current(bytecode.Value(arr))
		}
		.make_range {
			end_val := vm.pop_current()!
			start_val := vm.pop_current()!

			if start_val is int && end_val is int {
				mut arr := []bytecode.Value{}
				for i in start_val .. end_val {
					arr << bytecode.Value(i)
				}
				vm.push_current(bytecode.Value(arr))
			} else {
				return error('Range bounds must be integers')
			}
		}
		.index {
			idx_val := vm.pop_current()!
			arr_val := vm.pop_current()!

			if arr_val is []bytecode.Value {
				if idx_val is int {
					if idx_val >= 0 && idx_val < arr_val.len {
						vm.push_current(arr_val[idx_val])
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

			type_name_val := vm.pop_current()!
			type_name := if type_name_val is string {
				type_name_val
			} else {
				return error('Struct type name must be string')
			}

			mut fields := map[string]bytecode.Value{}
			for _ in 0 .. field_count {
				val := vm.pop_current()!
				name_val := vm.pop_current()!
				name := if name_val is string {
					name_val
				} else {
					return error('Field name must be string')
				}
				fields[name] = val
			}
			vm.push_current(bytecode.StructValue{
				type_name: type_name
				fields:    fields
			})
		}
		.get_field {
			field_name_idx := instr.operand
			field_name := vm.program.constants[field_name_idx]
			if field_name !is string {
				return error('Field name must be string')
			}
			struct_val := vm.pop_current()!
			if struct_val is bytecode.StructValue {
				if val := struct_val.fields[field_name as string] {
					vm.push_current(val)
				} else {
					return error('Unknown field: ${field_name}')
				}
			} else {
				return error('Cannot access field on non-struct')
			}
		}
		.set_field {
			field_name_idx := instr.operand
			field_name := vm.program.constants[field_name_idx]
			if field_name !is string {
				return error('Field name must be string')
			}
			val := vm.pop_current()!
			mut struct_val := vm.pop_current()!
			if mut struct_val is bytecode.StructValue {
				struct_val.fields[field_name as string] = val
				vm.push_current(struct_val)
			} else {
				return error('Cannot set field on non-struct')
			}
		}
		.make_closure {
			func_idx := instr.operand
			func := vm.program.functions[func_idx]

			// Pop captured values from stack (in reverse order)
			mut captures := []bytecode.Value{cap: func.capture_count}
			for _ in 0 .. func.capture_count {
				captures.prepend(vm.pop_current()!)
			}

			vm.push_current(bytecode.ClosureValue{
				func_idx: func_idx
				captures: captures
			})
		}
		.push_capture {
			capture_idx := instr.operand
			if p := vm.processes[vm.current_pid] {
				if p.frames.len > 0 {
					current_frame := p.frames[p.frames.len - 1]
					if capture_idx < current_frame.captures.len {
						vm.push_current(current_frame.captures[capture_idx])
					} else {
						return error('Capture index out of bounds: ${capture_idx}')
					}
				}
			}
		}
		.print {
			val := vm.pop_current()!
			println(inspect(val))
		}
		.make_enum {
			variant_name_val := vm.pop_current()!
			enum_name_val := vm.pop_current()!

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

			vm.push_current(bytecode.EnumValue{
				enum_name:    enum_name
				variant_name: variant_name
				payload:      none
			})
		}
		.make_enum_payload {
			payload := vm.pop_current()!
			variant_name_val := vm.pop_current()!
			enum_name_val := vm.pop_current()!

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

			vm.push_current(bytecode.EnumValue{
				enum_name:    enum_name
				variant_name: variant_name
				payload:      payload
			})
		}
		.match_enum {
			variant_name_idx := instr.operand
			variant_name := vm.program.constants[variant_name_idx]
			if variant_name !is string {
				return error('Variant name must be string')
			}

			enum_val := vm.pop_current()!
			if enum_val is bytecode.EnumValue {
				vm.push_current(enum_val.variant_name == (variant_name as string))
			} else {
				return error('Cannot match non-enum value')
			}
		}
		.unwrap_enum {
			enum_val := vm.pop_current()!
			if enum_val is bytecode.EnumValue {
				if p := enum_val.payload {
					vm.push_current(p)
				} else {
					vm.push_current(bytecode.NoneValue{})
				}
			} else {
				return error('Cannot unwrap non-enum value')
			}
		}
		.make_error {
			payload := vm.pop_current()!
			vm.push_current(bytecode.ErrorValue{
				payload: payload
			})
		}
		.is_error {
			val := vm.pop_current()!
			vm.push_current(val is bytecode.ErrorValue)
		}
		.is_none {
			val := vm.pop_current()!
			vm.push_current(val is bytecode.NoneValue)
		}
		.is_error_or_none {
			val := vm.pop_current()!
			vm.push_current(val is bytecode.ErrorValue || val is bytecode.NoneValue)
		}
		.unwrap {
			val := vm.pop_current()!
			if val is bytecode.ErrorValue {
				return error('Unwrap failed: ${inspect(val.payload)}')
			}
			vm.push_current(val)
		}
		.unwrap_error {
			val := vm.pop_current()!
			if val is bytecode.ErrorValue {
				vm.push_current(val.payload)
			} else {
				return error('Expected error value')
			}
		}
		.to_string {
			val := vm.pop_current()!
			vm.push_current(inspect(val))
		}
		.str_concat {
			b := vm.pop_current()!
			a := vm.pop_current()!
			if a is string && b is string {
				vm.push_current(a + b)
			} else {
				return error('str_concat requires two strings')
			}
		}
		.spawn {
			callee := vm.pop_current()!
			if callee is bytecode.ClosureValue {
				func := vm.program.functions[callee.func_idx]
				new_pid := vm.next_pid
				vm.next_pid += 1

				mut new_proc := &Process{
					pid:     new_pid
					stack:   []
					frames:  []
					mailbox: []
					status:  .running
				}

				for _ in 0 .. func.locals {
					new_proc.stack << bytecode.NoneValue{}
				}

				new_proc.frames << CallFrame{
					func:      func
					ip:        0
					base_slot: 0
					captures:  callee.captures
				}

				vm.processes[new_pid] = new_proc
				vm.runnable << new_pid
				vm.push_current(bytecode.PID{ id: new_pid })
			} else {
				return error('spawn expects a function')
			}
		}
		.send {
			msg := vm.pop_current()!
			pid_val := vm.pop_current()!

			if pid_val is bytecode.PID {
				if mut target := vm.processes[pid_val.id] {
					vm.mailbox_mutex.@lock()
					target.mailbox << msg
					if target.status == .waiting {
						target.status = .running
						vm.runnable << pid_val.id
					}
					vm.mailbox_mutex.unlock()
				}
			} else {
				return error('send expects a PID as first argument')
			}
			vm.push_current(bytecode.NoneValue{})
		}
		.receive {
			if mut current_proc := vm.processes[vm.current_pid] {
				vm.mailbox_mutex.@lock()
				if current_proc.mailbox.len > 0 {
					msg := current_proc.mailbox[0]
					current_proc.mailbox = current_proc.mailbox[1..]
					vm.mailbox_mutex.unlock()
					vm.push_current(msg)
				} else {
					vm.mailbox_mutex.unlock()
					current_proc.status = .waiting
					current_proc.frames[current_proc.frames.len - 1].ip -= 1
				}
			}
		}
		.self {
			vm.push_current(bytecode.PID{ id: vm.current_pid })
		}
		.halt {
			return true
		}
	}

	return false
}

fn (mut vm VM) pop_current() !bytecode.Value {
	if mut proc := vm.processes[vm.current_pid] {
		if proc.stack.len == 0 {
			return error('Stack underflow')
		}
		return proc.stack.pop()
	}
	return error('Process not found')
}

fn (vm VM) peek_current() !bytecode.Value {
	if proc := vm.processes[vm.current_pid] {
		if proc.stack.len == 0 {
			return error('Stack underflow')
		}
		return proc.stack[proc.stack.len - 1]
	}
	return error('Process not found')
}

fn (mut vm VM) push_current(val bytecode.Value) {
	if mut proc := vm.processes[vm.current_pid] {
		proc.stack << val
	}
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
				return a.enum_name == b.enum_name && a.variant_name == b.variant_name
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
			return '<fn ${v.func_idx}>'
		}
		bytecode.PID {
			return '<pid ${v.id}>'
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
	}
}
