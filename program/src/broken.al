fn broken() {
	Upper = struct MyStruct {
	}

	lower = struct MyStruct {
	}

	println(MyStruct{  })
	println(Upper{  })
	println(lower{  })
}

broken()
