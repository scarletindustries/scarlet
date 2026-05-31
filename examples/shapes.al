// Geometry on a Shape sum type. Circle, Rect, and Triangle all live in one
// Array(Shape), and area and perimeter pick a formula by pattern matching.

import al/float
import al/list
import al/string

type Shape {
	Circle(r Float)
	Rect(w Float h Float)
	Triangle(b Float h Float)
}

const pi = 3.141592653589793

// Newton's method square root: improve the guess until it stops moving.
// The stdlib has no sqrt, so the example carries its own.
fn sqrt_iter(x Float, guess Float) Float {
	next = { guess + x / guess } / 2.0
	if float.abs(next - guess) < 0.000001 {
		next
	} else {
		sqrt_iter(x, next)
	}
}

fn sqrt(x Float) Float {
	sqrt_iter(x, x / 2.0 + 1.0)
}

fn area(s Shape) Float {
	match s {
		Circle(r) -> pi * r * r
		Rect(w, h) -> w * h
		Triangle(b, h) -> b * h / 2.0
	}
}

// Triangles here are right triangles, so the third side is the hypotenuse.
fn perimeter(s Shape) Float {
	match s {
		Circle(r) -> 2.0 * pi * r
		Rect(w, h) -> 2.0 * w + 2.0 * h
		Triangle(b, h) -> b + h + sqrt(b * b + h * h)
	}
}

// Round to two decimals for display.
fn round2(x Float) Float {
	float.from_int(float.round(x * 100.0)) / 100.0
}

fn describe(s Shape) String {
	'${string.inspect(s)}: area ${round2(area(s))}, perimeter ${round2(perimeter(s))}'
}

// One printed line per shape, via recursion over the array.
fn print_shapes(shapes Array(Shape)) Nil {
	match shapes {
		[] -> Nil
		[s, ..rest] -> {
			println(describe(s))
			print_shapes(rest)
		}
	}
}

shapes = [Circle(1.5), Rect(4.0, 2.5), Triangle(3.0, 4.0)]
print_shapes(shapes)

total = list.fold(shapes, 0.0, fn(sum, s) sum + area(s))
println('total area: ${round2(total)}')
