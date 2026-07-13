// Conway's Game of Life: a glider crossing an 8x8 grid.
//
// The grid is an Array(Array(Bool)) rebuilt from scratch each generation —
// nothing is mutated. Reads outside the grid fall back to a dead cell with
// `or`, which is what lets the rules ignore the edges entirely.

import al/array

const width = 8
const height = 8
const generations = 4

// Build a grid by asking `cell` for every coordinate. `0..height` is an
// Array(Int), so the whole grid is two nested maps.
fn make_grid(cell fn(Int, Int) Bool) Array(Array(Bool)) {
	array.map(0..height, fn(y) array.map(0..width, fn(x) cell(x, y)))
}

// Indexing yields an Option, and `or` supplies the value for a miss — so a
// coordinate off any edge simply reads as dead.
fn cell_at(grid Array(Array(Bool)), x Int, y Int) Bool {
	row = grid[y] or []
	row[x] or False
}

const neighborhood = [(-1, -1), (0, -1), (1, -1), (-1, 0), (1, 0), (-1, 1), (0, 1), (1, 1)]

fn live_neighbors(grid Array(Array(Bool)), x Int, y Int) Int {
	array.fold(neighborhood, 0, fn(count, offset) if cell_at(grid, x + offset.0, y + offset.1) {
		count + 1
	} else {
		count
	})
}

// The rules, as one match on the pair: a live cell survives with 2 or 3
// neighbours, a dead cell with exactly 3 is born, everything else dies.
fn next_cell(alive Bool, neighbors Int) Bool {
	match (alive, neighbors) {
		(True, 2 | 3) -> True
		(False, 3) -> True
		else -> False
	}
}

fn step(grid Array(Array(Bool))) Array(Array(Bool)) {
	make_grid(fn(x, y) next_cell(cell_at(grid, x, y), live_neighbors(grid, x, y)))
}

fn render(grid Array(Array(Bool))) String {
	array.fold(grid, '', fn(text, row) text + render_row(row) + '\n')
}

fn render_row(row Array(Bool)) String {
	array.fold(row, '', fn(line, alive) line + if alive { '#' } else { '.' })
}

fn run(grid Array(Array(Bool)), gen Int) Nil {
	println('generation ${gen}:')
	println(render(grid))
	if gen < generations {
		run(step(grid), gen + 1)
	} else {
		Nil
	}
}

// A glider: the smallest pattern that travels. After four generations the
// same shape reappears, one cell down and one cell to the right.
const glider = [(1, 0), (2, 1), (0, 2), (1, 2), (2, 2)]

run(make_grid(fn(x, y) array.contains(glider, (x, y))), 0)
