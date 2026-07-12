import al/array
import al/binary.{Hex, Dec}
import al/bool
import al/int
import al/float
import al/option
import al/result
import al/map
import al/string.{inspect}
import al/decimal.{Up, Down, HalfUp, HalfEven, Ceiling, Floor}
import al/time
import al/net/address
import al/http/status
import al/http/headers
import al/http/body

// array
println(inspect(array.reverse([1, 2, 3])))
println(inspect(array.find([1, 2, 3], fn(x) x > 2)))
println(array.all([1, 2], fn(x) x > 0))
println(inspect(array.concat([1], [2, 3])))

// int
println(int.to_string(int.clamp(99, 0, 10)) + '/' + int.to_string(int.abs(-5)) + '/' + int.to_string(int.min(1, 2)))
println(int.max_value)
println(int.min_value)

// float
println(float.to_string(float.min(1.5, 2.5)))
println(float.floor(1.9) + float.ceil(1.1) + float.truncate(-1.9))

// bool
println(bool.to_string(bool.negate(True)))

// option / result
println(inspect(option.map(Some(2), fn(x) x + 1)))
println(inspect(option.then(Some(2), fn(x) Some(x * 2))))
println(inspect(option.or_else(None, fn() Some(9))))
println(option.is_some(Some(1)) && option.is_none(None))
println(inspect(result.map_err(Err('e'), fn(e) e + '!')))
println(result.is_ok(Ok(1)) && result.is_err(Err(2)))

// map
m = map.from_list([('a', 1), ('b', 2)])
println(inspect(array.length(map.keys(m))))
println(inspect(map.values(m)))
println(inspect(map.to_list(m)))

// binary
b = binary.from_string('Hello')
println(binary.bit_size(b))
println(binary.byte_at(b, 0))
println(inspect(binary.index_of(b, binary.from_string('llo'), 0)))
println(inspect(binary.to_string(binary.to_ascii_lower(b))))
println(binary.eq_ignore_ascii_case(b, binary.from_string('HELLO')))
println(inspect(binary.to_string(binary.from_int_ascii(255, Hex))))
println(inspect(binary.parse_int(binary.from_string('ff'), Hex)))
println(inspect(binary.to_string(binary.append(b, binary.from_string('!')))))
println(inspect(binary.to_string(binary.slice_bytes(b, 1, 3))))
println(inspect(binary.slice(b, 0, 8)))

// address
println(inspect(address.parse('127.0.0.1')))
println(inspect(status.reason_phrase(404)))
