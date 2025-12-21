// Triangle classifier

fn is_valid_triangle(a Int, b Int, c Int) Bool {
	match true {
		a + b <= c | b + c <= a | a + c <= b -> false,
		else -> true,
	}
}

fn classify_triangle(a Int, b Int, c Int) String {
	match true {
		!is_valid_triangle(a, b, c) -> 'Invalid',
		a == b && b == c -> 'Equilateral',
		a == b || b == c || a == c -> 'Isosceles',
		else -> 'Scalene',
	}
}

fn perimeter(a Int, b Int, c Int) Int {
	a + b + c
}

struct TriangleInfo {
	sides String,
	triangle_type String,
	perimeter Int,
	valid Bool,
}

fn analyze_triangle(a Int, b Int, c Int) TriangleInfo {
	TriangleInfo{
		sides: '${a}, ${b}, ${c}',
		triangle_type: classify_triangle(a, b, c),
		perimeter: perimeter(a, b, c),
		valid: is_valid_triangle(a, b, c),
	}
}

[analyze_triangle(3, 3, 3), analyze_triangle(3, 3, 5), analyze_triangle(3, 4, 5), analyze_triangle(1, 2, 10)]
