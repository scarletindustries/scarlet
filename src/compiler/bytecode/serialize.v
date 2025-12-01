module bytecode

import encoding.binary

const version = 1

const alb_magic = [u8(0xA1), 0xB0, 0x00, version] // "ALB" + version byte

pub fn (p Program) serialize() []u8 {
	mut buf := []u8{}

	buf << alb_magic

	buf << u8(version)

	write_u32(mut buf, u32(p.constants.len))
	for c in p.constants {
		serialize_value(mut buf, c)
	}

	write_u32(mut buf, u32(p.functions.len))
	for f in p.functions {
		serialize_function(mut buf, f)
	}

	write_u32(mut buf, u32(p.code.len))
	for instr in p.code {
		buf << u8(instr.op)
		write_i32(mut buf, instr.operand)
	}

	write_i32(mut buf, p.entry)

	return buf
}

struct Reader {
	data []u8
mut:
	pos int
}

fn (mut r Reader) read_u8() u8 {
	v := r.data[r.pos]
	r.pos += 1
	return v
}

fn (mut r Reader) read_u32() u32 {
	v := binary.little_endian_u32(r.data[r.pos..r.pos + 4])
	r.pos += 4
	return v
}

fn (mut r Reader) read_i32() int {
	return int(r.read_u32())
}

fn (mut r Reader) read_i64() i64 {
	v := binary.little_endian_u64(r.data[r.pos..r.pos + 8])
	r.pos += 8
	return i64(v)
}

fn (mut r Reader) read_f64() f64 {
	bits := binary.little_endian_u64(r.data[r.pos..r.pos + 8])
	r.pos += 8
	return unsafe { *(&f64(&bits)) }
}

fn (mut r Reader) read_string() string {
	len := r.read_u32()
	s := r.data[r.pos..r.pos + int(len)].bytestr()
	r.pos += int(len)
	return s
}

pub fn deserialize(data []u8) !Program {
	if data.len < 5 {
		return error('Invalid .alb file: too short')
	}

	if data[0] != alb_magic[0] || data[1] != alb_magic[1] || data[2] != alb_magic[2]
		|| data[3] != alb_magic[3] {
		return error('Invalid .alb file: bad magic bytes')
	}

	if data[4] != version {
		return error('Unsupported .alb version: ${data[4]}')
	}

	mut r := Reader{
		data: data
		pos:  5
	}

	const_len := r.read_u32()
	mut constants := []Value{cap: int(const_len)}
	for _ in 0 .. const_len {
		constants << r.read_value()!
	}

	func_len := r.read_u32()
	mut functions := []Function{cap: int(func_len)}
	for _ in 0 .. func_len {
		functions << r.read_function()
	}

	code_len := r.read_u32()
	mut code := []Instruction{cap: int(code_len)}
	for _ in 0 .. code_len {
		op := unsafe { Op(r.read_u8()) }
		operand := r.read_i32()
		code << Instruction{
			op:      op
			operand: operand
		}
	}

	entry := r.read_i32()

	return Program{
		constants: constants
		functions: functions
		code:      code
		entry:     entry
	}
}

fn serialize_value(mut buf []u8, v Value) {
	match v {
		int {
			buf << u8(0)
			write_i64(mut buf, i64(v))
		}
		f64 {
			buf << u8(1)
			write_f64(mut buf, v)
		}
		bool {
			buf << u8(2)
			buf << if v { u8(1) } else { u8(0) }
		}
		string {
			buf << u8(3)
			write_string(mut buf, v)
		}
		NoneValue {
			buf << u8(4)
		}
		[]Value {
			buf << u8(5)
			write_u32(mut buf, u32(v.len))
			for elem in v {
				serialize_value(mut buf, elem)
			}
		}
		StructValue {
			buf << u8(6)
			write_string(mut buf, v.type_name)
			write_u32(mut buf, u32(v.fields.len))
			for name, val in v.fields {
				write_string(mut buf, name)
				serialize_value(mut buf, val)
			}
		}
		ClosureValue {
			buf << u8(7)
			write_i32(mut buf, v.func_idx)
		}
		PID {
			buf << u8(8)
			write_i32(mut buf, v.id)
		}
		EnumValue {
			buf << u8(9)
			write_string(mut buf, v.enum_name)
			write_string(mut buf, v.variant_name)
			if p := v.payload {
				buf << u8(1)
				serialize_value(mut buf, p)
			} else {
				buf << u8(0)
			}
		}
		ErrorValue {
			buf << u8(10)
			serialize_value(mut buf, v.payload)
		}
	}
}

fn (mut r Reader) read_value() !Value {
	tag := r.read_u8()

	match tag {
		0 {
			return int(r.read_i64())
		}
		1 {
			return r.read_f64()
		}
		2 {
			return r.read_u8() == 1
		}
		3 {
			return r.read_string()
		}
		4 {
			return NoneValue{}
		}
		5 {
			len := r.read_u32()
			mut arr := []Value{cap: int(len)}
			for _ in 0 .. len {
				arr << r.read_value()!
			}
			return arr
		}
		6 {
			type_name := r.read_string()
			field_count := r.read_u32()
			mut fields := map[string]Value{}
			for _ in 0 .. field_count {
				name := r.read_string()
				val := r.read_value()!
				fields[name] = val
			}
			return StructValue{
				type_name: type_name
				fields:    fields
			}
		}
		7 {
			return ClosureValue{
				func_idx: r.read_i32()
			}
		}
		8 {
			return PID{
				id: r.read_i32()
			}
		}
		9 {
			enum_name := r.read_string()
			variant_name := r.read_string()
			has_payload := r.read_u8() == 1
			mut payload := ?Value(none)
			if has_payload {
				payload = r.read_value()!
			}
			return EnumValue{
				enum_name:    enum_name
				variant_name: variant_name
				payload:      payload
			}
		}
		10 { // ErrorValue
			return ErrorValue{
				payload: r.read_value()!
			}
		}
		else {
			return error('Unknown value type tag: ${tag}')
		}
	}
}

fn serialize_function(mut buf []u8, f Function) {
	write_string(mut buf, f.name)
	write_i32(mut buf, f.arity)
	write_i32(mut buf, f.locals)
	write_i32(mut buf, f.code_start)
	write_i32(mut buf, f.code_len)
}

fn (mut r Reader) read_function() Function {
	name := r.read_string()
	arity := r.read_i32()
	locals := r.read_i32()
	code_start := r.read_i32()
	code_len := r.read_i32()
	return Function{
		name:       name
		arity:      arity
		locals:     locals
		code_start: code_start
		code_len:   code_len
	}
}

fn write_u32(mut buf []u8, v u32) {
	mut bytes := []u8{len: 4}
	binary.little_endian_put_u32(mut bytes, v)
	buf << bytes
}

fn write_i32(mut buf []u8, v int) {
	write_u32(mut buf, u32(v))
}

fn write_i64(mut buf []u8, v i64) {
	mut bytes := []u8{len: 8}
	binary.little_endian_put_u64(mut bytes, u64(v))
	buf << bytes
}

fn write_f64(mut buf []u8, v f64) {
	mut bytes := []u8{len: 8}
	binary.little_endian_put_u64(mut bytes, unsafe { *(&u64(&v)) })
	buf << bytes
}

fn write_string(mut buf []u8, s string) {
	write_u32(mut buf, u32(s.len))
	buf << s.bytes()
}
