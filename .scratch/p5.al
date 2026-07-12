import al/binary
import al/http/headers.{Header}
import al/http/body
import al/http/h1.{Http11}
import al/string.{inspect}
import al/result
import al/array

fn b(s String) Binary {
	binary.from_string(s)
}

hs = headers.set([], <<'Content-Type'>>, <<'text/plain'>>)
hs2 = headers.append(hs, b('X-Tag'), b('one'))
println(inspect(headers.get(hs2, b('content-type'))))
println(headers.valid(hs2))
println(inspect(array.length(headers.render(hs2))))
println(inspect(headers.delete(hs2, b('X-Tag'))))
println(inspect(headers.field(b('Bad Name'), b('v'))))
println(inspect(binary.to_string(h1.serialize_head(200, hs2))))
println(h1.should_close(Http11, hs2))

bd = body.from_binary(b('payload'))
println(inspect(result.map(body.collect(bd, 100), fn(x) binary.to_string(x))))
println(inspect(result.map(body.collect(body.empty(), 100), fn(x) binary.byte_size(x))))
