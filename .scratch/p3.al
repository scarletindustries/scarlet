import al/result
import al/io
import al/io.{NotFound}
import al/process
import al/array
import al/binary
import al/string.{inspect}

path = '/tmp/al_io_demo.txt'
Nil = result.unwrap(io.write_text(path, 'hello from al\n'), Nil)
println(inspect(io.read_text(path)))
match io.read_text('/nonexistent/nope.txt') {
	Ok(_) -> println('??')
	Err(NotFound(p)) -> println('not found: ${p}')
	Err(e) -> println('other: ${inspect(e)}')
}
println(array.length(process.argv()))
println(inspect(process.argv()))
println(inspect(process.get_env('AL_DEMO_UNSET_XYZ')))
