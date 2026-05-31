fn identity(x a) a {
	x
}
fn identity2(x) {
	x
}

fn first(arr Array(a)) Option(a) {
	match arr {
		[] -> None
		[head, ..] -> Some(head)
	}
}

['${identity(5)}', identity('hello'), '${first([1, 2, 3]) or 0}', first(['a', 'b']) or 'none']
