struct Box(d) {
	/**
     * A generic struct holding data of any type.
     */
	data d
}

enum Option(t) {
	/**
     * A generic enum representing an optional value.
     */
	Some(t)
	None
}

enum Option2(t) {
	/**
	 * Another generic enum representing an optional value.
	 */
	Some2(t)
	None2
}

user Option = Some('hi')

result = match user {
	Some(name) -> 'Hello, ${name}',
	None -> 'No user found',
}

println(result)

box = Box{ data: 42 }
box.data
