// Algebraic data types: a recursive enum, a record, and type aliases, all
// modelling a small filesystem tree.

import al/array
import al/string

/** Where an entry lives. An alias names an existing type; it is not a new one. */
type Path = String

/** One entry in a directory. `Entry` mentions itself, so the type is a tree. */
type Entry {
	/** A leaf: a name and a size in bytes. */
	File(name String, size Int)
	/** A node: a name and everything below it. */
	Dir(name Path, entries Array(Entry))
}

/** `Listing` is `Array(Entry)` under a name that says what it is for. */
type Listing = Array(Entry)

/** Running totals for a subtree. A single-constructor type is a record. */
type Stat {
	files Int
	dirs Int
	bytes Int
}

const EMPTY = Stat(files: 0, dirs: 0, bytes: 0)

// Records are built with labelled arguments and read with `.field`.
fn merge(a Stat, b Stat) Stat {
	Stat(files: a.files + b.files, dirs: a.dirs + b.dirs, bytes: a.bytes + b.bytes)
}

fn walk(e Entry) Stat {
	match e {
		File(_, size) -> Stat(files: 1, dirs: 0, bytes: size)
		Dir(_, entries) -> {
			totals = array.fold(entries, EMPTY, fn(acc, child) merge(acc, walk(child)))
			// `..totals` spreads the record, and the labelled argument after it
			// overrides that one field — the original value is untouched.
			Stat(..totals, dirs: totals.dirs + 1)
		}
	}
}

fn render(e Entry, indent String) Nil {
	match e {
		File(name, size) -> println('${indent}${name}  ${size}b')
		Dir(name, entries) -> {
			println('${indent}${name}/')
			array.each(entries, fn(child) render(child, '${indent}  '))
		}
	}
}

fn find(e Entry, want Path) Option(Entry) {
	match e {
		File(name, _) -> if name == want {
			Some(e)
		} else {
			None
		}
		Dir(name, entries) -> if name == want {
			Some(e)
		} else {
			find_first(entries, want)
		}
	}
}

// The first hit wins, so the search stops at the first `Some`.
fn find_first(entries Listing, want Path) Option(Entry) {
	match entries {
		[] -> None
		[head, ..rest] -> match find(head, want) {
			Some(e) -> Some(e)
			None -> find_first(rest, want)
		}
	}
}

fn label(e Entry) String {
	match e {
		File(name, size) -> 'file ${name} of ${size}b'
		Dir(name, entries) -> 'dir ${name} with ${array.length(entries)} entries'
	}
}

root = Dir(
	name: 'project',
	entries: [
		File(name: 'README.md', size: 1024),
		Dir(
			name: 'src',
			entries: [File(name: 'main.al', size: 2048), File(name: 'lib.al', size: 512)],
		),
		Dir(name: 'tests', entries: [File(name: 'golden.al', size: 256)]),
	],
)

render(root, '')

// A single-constructor type also destructures in a binding position.
Stat(files, dirs, bytes) = walk(root)
println('${files} files, ${dirs} dirs, ${bytes} bytes')

// Interpolation stringifies any value, enums and records included.
println(string.inspect(walk(root)))

println(match find(root, 'lib.al') {
	Some(e) -> label(e)
	None -> 'not found'
})
println(match find(root, 'missing.al') {
	Some(e) -> label(e)
	None -> 'not found'
})
