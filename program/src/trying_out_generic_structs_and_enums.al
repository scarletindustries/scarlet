struct Box(D) {
	/**
     * A generic struct holding data of any type.
     */
	data D
}

/**
 * A generic enum representing an optional value.
 */
enum Option(T) {
	Some(T)
	None
}

user Option = Some('hi')

result = match user {
	Some(name) -> 'Hello, ${name}',
	None -> 'No user found',
}

println(result)

box = Box{ data: 42 }
box.data
