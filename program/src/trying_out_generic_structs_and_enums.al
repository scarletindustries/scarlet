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

user Option = Some('Alice')

result = match user {
	Some(name) -> 'Hello, ' + name,
	None -> 'No user found',
}

box = Box{ data: 42 }
box.data
