// Generic binary search tree: a recursive sum type with insert, contains,
// size, depth, and an in-order traversal that reads values back out sorted.

import al/int
import al/array
import al/string

type Tree(t) {
	Leaf
	Node(left Tree(t), value t, right Tree(t))
}

// Smaller values go left, larger go right, duplicates are dropped.
fn insert(tree Tree(Int), x Int) Tree(Int) {
	match tree {
		Leaf -> Node(left: Leaf, value: x, right: Leaf)
		Node(left, value, right) -> {
			if x < value {
				Node(left: insert(left, x), value: value, right: right)
			} else if x > value {
				Node(left: left, value: value, right: insert(right, x))
			} else {
				tree
			}
		}
	}
}

fn contains(tree Tree(Int), x Int) Bool {
	match tree {
		Leaf -> False
		Node(left, value, right) -> {
			if x == value {
				True
			} else if x < value {
				contains(left, x)
			} else {
				contains(right, x)
			}
		}
	}
}

// The shape helpers work for any element type, not just Int.
fn size(tree Tree(t)) Int {
	match tree {
		Leaf -> 0
		Node(left, _, right) -> 1 + size(left) + size(right)
	}
}

fn depth(tree Tree(t)) Int {
	match tree {
		Leaf -> 0
		Node(left, _, right) -> 1 + int.max(depth(left), depth(right))
	}
}

// In-order traversal: everything left of a node comes before it,
// everything right comes after. For a search tree that is sorted order.
fn to_array(tree Tree(t)) Array(t) {
	match tree {
		Leaf -> []
		Node(left, value, right) -> [..to_array(left), value, ..to_array(right)]
	}
}

fn from_array(xs Array(Int)) Tree(Int) {
	array.fold(xs, Leaf, insert)
}

values = [5, 3, 8, 1, 9, 2, 7]
tree = from_array(values)

println('input:  ${string.inspect(values)}')
println('sorted: ${string.inspect(to_array(tree))}')
println('size:   ${size(tree)}')
println('depth:  ${depth(tree)}')
println('contains 7: ${contains(tree, 7)}')
println('contains 4: ${contains(tree, 4)}')
