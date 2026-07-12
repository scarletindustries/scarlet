import al/string.{inspect}

/** doc */
pub opaque type Token {
	Token(v String)
}

pub type Handle


// or-pattern binding same names
fn g(o Option(Int)) Int {
	match o {
		Some(1) | Some(2) -> 100
		Some(n) -> n
		None -> 0
	}
}
println(g(Some(2)))
println(g(Some(7)))

// negative literal pattern, string pattern, Bool ctor pattern
fn h(n Int) String {
	match n {
		-1 -> 'neg one'
		0..10 -> 'small'
		else -> 'other'
	}
}
println(h(-1))
println(h(3))

fn bl(b Bool) String {
	match b {
		True -> 'yes'
		False -> 'no'
	}
}
println(bl(True))

// array pattern with spread binding
fn head_tail(xs Array(Int)) String {
	match xs {
		[] -> 'empty'
		[x] -> 'one ${x}'
		[x, y, ..rest] -> 'two+ ${x},${y} rest=${inspect(rest)}'
	}
}
println(head_tail([1, 2, 3]))

// string escapes
println('tab\there\nnewline done \'quoted\' \\ \$notinterp')
println(inspect(Token("t")))
