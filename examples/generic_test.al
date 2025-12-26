fn identity(x a) a { x }

fn first(arr []a) ?a {
	match arr {
		[] -> none,
		[first, ..] -> first,
	}
}

['${identity(5)}', identity('hello'), '${first([1, 2, 3]) or 0}', first(['a', 'b']) or 'none']
