// Conway's Game of Life: a glider crossing an 8x8 grid.
// The grid is an Array(Array(Bool)); reads outside it fall back to dead
// cells with `or`, so the edges need no special casing.

import al/array

const width = 8
const height = 8

// Build a grid by asking `cell` for every (x, y) coordinate.
fn make_row(cell fn(Int, Int) Bool, y Int, x Int) Array(Bool) {
	if x == width {
		[]
	} else {
		[cell(x, y), ..make_row(cell, y, x + 1)]
	}
}

fn make_rows(cell fn(Int, Int) Bool, y Int) Array(Array(Bool)) {
	if y == height {
		[]
	} else {
		[make_row(cell, y, 0), ..make_rows(cell, y + 1)]
	}
}

fn make_grid(cell fn(Int, Int) Bool) Array(Array(Bool)) {
	make_rows(cell, 0)
}

// Read one cell. Anything outside the grid counts as dead.
fn cell_at(grid Array(Array(Bool)), x Int, y Int) Bool {
	row = grid[y] or []
	row[x] or False
}

// Count the live cells among the eight neighbors of (x, y).
fn live_neighbors(grid Array(Array(Bool)), x Int, y Int) Int {
	around = [
		cell_at(grid, x - 1, y - 1),
		cell_at(grid, x, y - 1),
		cell_at(grid, x + 1, y - 1),
		cell_at(grid, x - 1, y),
		cell_at(grid, x + 1, y),
		cell_at(grid, x - 1, y + 1),
		cell_at(grid, x, y + 1),
		cell_at(grid, x + 1, y + 1),
	]
	array.length(array.filter(around, fn(alive) alive))
}

// A live cell survives with 2 or 3 neighbors. A dead cell with exactly
// 3 neighbors comes alive. Every other cell ends up dead.
fn next_cell(alive Bool, neighbors Int) Bool {
	match (alive, neighbors) {
		(True, 2 | 3) -> True
		(False, 3) -> True
		else -> False
	}
}

// Advance the whole grid one generation.
fn step(grid Array(Array(Bool))) Array(Array(Bool)) {
	make_grid(fn(x, y) next_cell(cell_at(grid, x, y), live_neighbors(grid, x, y)))
}

// Render a grid as lines of # (alive) and . (dead).
fn render_row(row Array(Bool)) String {
	array.fold(row, '', fn(line, alive) line + if alive { '#' } else { '.' })
}

fn render(grid Array(Array(Bool))) String {
	array.fold(grid, '', fn(text, row) text + render_row(row) + '\n')
}

// Print generations gen through last, stepping in between.
fn run(grid Array(Array(Bool)), gen Int, last Int) Nil {
	println('Generation ${gen}:')
	println(render(grid))
	if gen < last {
		run(step(grid), gen + 1, last)
	} else {
		Nil
	}
}

// A glider: the smallest pattern that travels. Every 4 generations the
// same shape reappears one cell down and one cell to the right.
glider = [(1, 0), (2, 1), (0, 2), (1, 2), (2, 2)]
run(make_grid(fn(x, y) array.contains(glider, (x, y))), 0, 4)
