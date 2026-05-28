type Container(D) {
	data D
}

user = Some('hi')

result = match user {
	Some(name) -> 'Hello, ${name}'
	None -> 'No user found'
}

println(result)

box = Container(data: 42)
box.data
