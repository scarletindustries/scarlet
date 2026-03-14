enum MySubEnum {
	D
	E
}

enum MyEnum {
	A
	B
	C(MySubEnum)
}

fn test(arg MyEnum) String {
	match arg {
		A -> 'a is the best!',
		B -> 'b is the best!',
		C(sub) -> match sub {
			D -> 'd is the best!',
			E -> 'e is the worst!',
		},
	}
}

println(test(C(D)))
println(test(A))
println(test(C(E)))
