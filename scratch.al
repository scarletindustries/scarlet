import al/io

io.read_text('./does-not-exist.txt') or err -> {
	match err {
		io.NotFound(path) -> println('couldnt find ${path}')
		else -> println(err)
	}

	'<empty>'
}
