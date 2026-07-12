import al/array as arr
import al/string.{inspect}

type Point {
	x Int
	y Int
}

type Shape {
	Circle(r Int)
	Rect(w Int, h Int)
}

type Pair(a, b) = (a, b)

fn area(s Shape) Int {
	match s {
		Circle(r) -> 3 * r * r
		Rect(w: w, h: h) -> w * h
	}
}

p0 = Point(x: 1, y: 2)
p1 = Point(..p0, y: 9)
println(inspect(p1))

Nil = println('typed discard')
Point(px, py) = p1
println(px + py)

xs = [1, 2, 3]
r = 0..5
println(inspect(r))
println(inspect(r[1..3]))
println(inspect(arr.reverse(xs)))
println(inspect(xs[9]))

fn cls(s Shape) String {
	match s {
		Circle(r) if r > 10 -> 'big circle'
		Circle(_) | Rect(..) -> 'small'
	}
}
println(cls(Circle(r: 20)))
println(cls(Rect(w: 2, h: 3)))
println(area(Rect(w: 2, h: 3)))

f = 1.5
println(f + 0.25)

p Pair(Int, String) = (1, 'x')
println(inspect(p))
Int = 3 + 4
