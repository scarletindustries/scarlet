struct Person {
	name String
	age Int
}

fn greet(p Person) String {
	'Hello, ${p.name}! You are ${p.age} years old.'
}

person = Person{ name: 'Alice', age: 30 }
println(greet(person))
