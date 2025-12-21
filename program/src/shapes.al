// Shape calculations

// Approximate pi as 314/100 for integer math
fn circle_area(radius Int) Int {
	314 * radius * radius / 100
}

fn circle_circumference(radius Int) Int {
	2 * 314 * radius / 100
}

fn rectangle_area(width Int, height Int) Int {
	width * height
}

fn rectangle_perimeter(width Int, height Int) Int {
	2 * width + height
}

fn square_area(side Int) Int {
	side * side
}

fn square_perimeter(side Int) Int {
	4 * side
}

struct ShapeInfo {
	name String,
	area Int,
	perimeter Int,
}

// Compute areas for different shapes
[ShapeInfo{
	name: 'Circle r=10',
	area: circle_area(10),
	perimeter: circle_circumference(10),
}, ShapeInfo{
	name: 'Rectangle 5x8',
	area: rectangle_area(5, 8),
	perimeter: rectangle_perimeter(5, 8),
}, ShapeInfo{
	name: 'Square 7',
	area: square_area(7),
	perimeter: square_perimeter(7),
}]
