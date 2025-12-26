struct Box(D) {
	/**
     * A generic struct holding data of any type.
     */
	data D
}

enum Option(T) {
	/**
     * A generic enum representing an optional value.
     */
	Some(T)
	None
}

enum Option2(T) {
	/**
	 * Another generic enum representing an optional value.
	 */
	Some2(T)
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
